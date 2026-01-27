//! Webhook HTTP handler.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use tracing::{debug, error, info, warn};

use crate::gitlab::{MergeRequestEvent, ReviewPayload};
use crate::queue::Queue;

/// Application state shared across handlers.
#[derive(Clone)]
pub struct AppState {
    pub queue: Queue,
    pub webhook_secret: String,
}

/// Build the HTTP router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health_handler))
        .route("/webhook/gitlab", post(gitlab_webhook_handler))
        .route("/queue/stats", get(queue_stats_handler))
        .with_state(Arc::new(state))
}

/// Health check endpoint.
async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

/// Queue statistics endpoint.
async fn queue_stats_handler(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let pending = state.queue.len().await.map_err(AppError::Redis)?;
    let processing = state
        .queue
        .processing_count()
        .await
        .map_err(AppError::Redis)?;
    let failed = state
        .queue
        .failed_count()
        .await
        .map_err(AppError::Redis)?;

    Ok(Json(serde_json::json!({
        "pending": pending,
        "processing": processing,
        "failed": failed,
    })))
}

/// GitLab webhook handler.
async fn gitlab_webhook_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    // Verify webhook token
    let token = headers
        .get("X-Gitlab-Token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if token != state.webhook_secret {
        warn!("Invalid webhook token");
        return Err(AppError::Unauthorized);
    }

    // Parse event
    let event: MergeRequestEvent = serde_json::from_slice(&body).map_err(|e| {
        error!(error = %e, "Failed to parse webhook body");
        AppError::BadRequest(format!("Invalid JSON: {e}"))
    })?;

    info!(
        project = %event.project.path_with_namespace,
        mr_iid = %event.object_attributes.iid,
        action = ?event.object_attributes.action,
        "Received GitLab webhook"
    );

    // Check if we should review
    if !event.should_review() {
        debug!("Event does not require review");
        return Ok((
            StatusCode::OK,
            Json(WebhookResponse {
                status: "ignored".into(),
                message: Some("Event does not require review".into()),
                job_id: None,
            }),
        ));
    }

    // Check for skip label
    if event.has_label("skip-review") {
        debug!("MR has skip-review label");
        return Ok((
            StatusCode::OK,
            Json(WebhookResponse {
                status: "skipped".into(),
                message: Some("MR has skip-review label".into()),
                job_id: None,
            }),
        ));
    }

    // Queue for processing
    let payload = ReviewPayload::from(&event);
    let job_id = state.queue.push(payload).await.map_err(AppError::Redis)?;

    info!(job_id = %job_id, "Queued review job");

    Ok((
        StatusCode::ACCEPTED,
        Json(WebhookResponse {
            status: "queued".into(),
            message: None,
            job_id: Some(job_id),
        }),
    ))
}

#[derive(Serialize)]
struct WebhookResponse {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    job_id: Option<String>,
}

/// Application error type.
#[derive(Debug)]
pub enum AppError {
    Unauthorized,
    BadRequest(String),
    Redis(redis::RedisError),
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "Unauthorized".into()),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            AppError::Redis(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Redis error: {e}"),
            ),
            AppError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };

        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}
