//! Webhook HTTP handler.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use crate::github::{verify_signature, PullRequestEvent};
use crate::gitlab::{fetch_review_payload, MergeRequestEvent, PipelineEvent, ReviewPayload};
use crate::queue::Queue;

/// Application state shared across handlers.
#[derive(Clone)]
pub struct AppState {
    pub queue: Queue,
    pub webhook_secret: String,
    /// API key for CLI access (defaults to webhook_secret if not set)
    pub api_key: Option<String>,
    /// GitLab API token for fetching MR details
    pub gitlab_token: String,
    /// GitHub API token (optional, for GitHub webhook support)
    pub github_token: Option<String>,
}

impl AppState {
    /// Verify API key from Authorization: Bearer header.
    fn verify_api_key(&self, headers: &HeaderMap) -> bool {
        let expected = self.api_key.as_ref().unwrap_or(&self.webhook_secret);

        if let Some(auth) = headers.get("Authorization").and_then(|v| v.to_str().ok()) {
            if let Some(token) = auth.strip_prefix("Bearer ") {
                return token == expected;
            }
        }

        false
    }
}

/// Build the HTTP router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health_handler))
        .route("/webhook/gitlab", post(gitlab_webhook_handler))
        .route("/webhook/github", post(github_webhook_handler))
        // API endpoints for CLI
        .route("/api/stats", get(queue_stats_handler))
        .route("/api/failed", get(list_failed_handler))
        .route("/api/retry/{id}", post(retry_handler))
        .route("/api/review", post(queue_review_handler))
        // Legacy endpoint
        .route("/queue/stats", get(queue_stats_handler))
        .with_state(Arc::new(state))
}

/// Health check endpoint.
async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

/// Queue statistics endpoint (requires API key).
async fn queue_stats_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    if !state.verify_api_key(&headers) {
        warn!("Invalid API key for /api/stats");
        return Err(AppError::Unauthorized);
    }

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

/// Minimal struct to peek at the event type before full parsing.
#[derive(Deserialize)]
struct EventKind {
    object_kind: String,
}

/// GitLab webhook handler. Dispatches based on object_kind.
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

    // Log raw body for debugging
    if let Ok(body_str) = std::str::from_utf8(&body) {
        debug!(body = %body_str, "Raw webhook body");
    }

    // Peek at object_kind to dispatch
    let kind: EventKind = serde_json::from_slice(&body).map_err(|e| {
        AppError::BadRequest(format!("Invalid JSON: {e}"))
    })?;

    match kind.object_kind.as_str() {
        "merge_request" => handle_merge_request_event(&state, &body).await,
        "pipeline" => handle_pipeline_event(&state, &body).await,
        other => {
            debug!(object_kind = other, "Ignoring unsupported GitLab event type");
            Ok((
                StatusCode::OK,
                Json(WebhookResponse {
                    status: "ignored".into(),
                    message: Some(format!("Unsupported event type: {other}")),
                    job_id: None,
                }),
            ))
        }
    }
}

async fn handle_merge_request_event(
    state: &AppState,
    body: &[u8],
) -> Result<(StatusCode, Json<WebhookResponse>), AppError> {
    let event: MergeRequestEvent = serde_json::from_slice(body).map_err(|e| {
        if let Ok(body_str) = std::str::from_utf8(body) {
            error!(error = %e, body = %body_str, "Failed to parse merge request webhook");
        }
        AppError::BadRequest(format!("Invalid JSON: {e}"))
    })?;

    info!(
        project = %event.project.path_with_namespace,
        mr_iid = %event.object_attributes.iid,
        action = ?event.object_attributes.action,
        "Received GitLab merge request webhook"
    );

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

async fn handle_pipeline_event(
    state: &AppState,
    body: &[u8],
) -> Result<(StatusCode, Json<WebhookResponse>), AppError> {
    let event: PipelineEvent = serde_json::from_slice(body).map_err(|e| {
        if let Ok(body_str) = std::str::from_utf8(body) {
            error!(error = %e, body = %body_str, "Failed to parse pipeline webhook");
        }
        AppError::BadRequest(format!("Invalid JSON: {e}"))
    })?;

    info!(
        project = %event.project.path_with_namespace,
        pipeline_id = %event.object_attributes.id,
        status = %event.object_attributes.status,
        "Received GitLab pipeline webhook"
    );

    if !event.should_lint_fix() {
        return Ok((
            StatusCode::OK,
            Json(WebhookResponse {
                status: "ignored".into(),
                message: Some("Pipeline event does not require lint fix".into()),
                job_id: None,
            }),
        ));
    }

    let payload = ReviewPayload::from(&event);
    let job_id = state.queue.push(payload).await.map_err(AppError::Redis)?;

    info!(job_id = %job_id, "Queued lint-fix job");

    Ok((
        StatusCode::ACCEPTED,
        Json(WebhookResponse {
            status: "queued".into(),
            message: Some("Lint-fix job queued".into()),
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

/// GitHub webhook handler.
async fn github_webhook_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    // Verify HMAC-SHA256 signature
    let signature = headers
        .get("X-Hub-Signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !verify_signature(&state.webhook_secret, &body, signature) {
        warn!("Invalid GitHub webhook signature");
        return Err(AppError::Unauthorized);
    }

    // Only handle pull_request events
    let event_type = headers
        .get("X-GitHub-Event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if event_type != "pull_request" {
        return Ok((
            StatusCode::OK,
            Json(WebhookResponse {
                status: "ignored".into(),
                message: Some(format!("Unsupported event type: {event_type}")),
                job_id: None,
            }),
        ));
    }

    // Log raw body for debugging
    if let Ok(body_str) = std::str::from_utf8(&body) {
        debug!(body = %body_str, "Raw GitHub webhook body");
    }

    // Parse event
    let event: PullRequestEvent = serde_json::from_slice(&body).map_err(|e| {
        if let Ok(body_str) = std::str::from_utf8(&body) {
            error!(error = %e, body = %body_str, "Failed to parse GitHub webhook body");
        } else {
            error!(error = %e, "Failed to parse GitHub webhook body (non-UTF8)");
        }
        AppError::BadRequest(format!("Invalid JSON: {e}"))
    })?;

    info!(
        repo = %event.repository.full_name,
        pr = %event.pull_request.number,
        action = %event.action,
        "Received GitHub webhook"
    );

    if !event.should_review() {
        debug!("GitHub event does not require review");
        return Ok((
            StatusCode::OK,
            Json(WebhookResponse {
                status: "ignored".into(),
                message: Some("Event does not require review".into()),
                job_id: None,
            }),
        ));
    }

    let payload = ReviewPayload::from(&event);
    let job_id = state.queue.push(payload).await.map_err(AppError::Redis)?;

    info!(job_id = %job_id, "Queued GitHub review job");

    Ok((
        StatusCode::ACCEPTED,
        Json(WebhookResponse {
            status: "queued".into(),
            message: None,
            job_id: Some(job_id),
        }),
    ))
}

/// List failed items handler (requires API key).
async fn list_failed_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    if !state.verify_api_key(&headers) {
        warn!("Invalid API key for /api/failed");
        return Err(AppError::Unauthorized);
    }

    let items = state
        .queue
        .list_failed(100)
        .await
        .map_err(AppError::Redis)?;

    Ok(Json(items))
}

/// Retry a failed item handler (requires API key).
async fn retry_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    if !state.verify_api_key(&headers) {
        warn!("Invalid API key for /api/retry");
        return Err(AppError::Unauthorized);
    }

    let success = state
        .queue
        .retry_failed(&id)
        .await
        .map_err(AppError::Redis)?;

    if success {
        info!(id = %id, "Retried failed job");
        Ok((
            StatusCode::OK,
            Json(serde_json::json!({ "status": "retried", "id": id })),
        ))
    } else {
        Ok((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "status": "not_found", "id": id })),
        ))
    }
}

/// Queue a review via API — server fetches MR details from GitLab.
#[derive(Deserialize)]
struct QueueReviewRequest {
    /// Project path (e.g., "Globalcomix/gc")
    project: String,
    /// Merge request IID
    mr_iid: u64,
    /// GitLab base URL (defaults to https://gitlab.com)
    #[serde(default = "default_gitlab_url")]
    gitlab_url: String,
}

fn default_gitlab_url() -> String {
    "https://gitlab.com".into()
}

async fn queue_review_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<QueueReviewRequest>,
) -> Result<impl IntoResponse, AppError> {
    if !state.verify_api_key(&headers) {
        warn!("Invalid API key for /api/review");
        return Err(AppError::Unauthorized);
    }

    // Fetch MR details from GitLab
    let payload = fetch_review_payload(&req.gitlab_url, &req.project, req.mr_iid, &state.gitlab_token)
        .await
        .map_err(|e| AppError::Internal(format!("Failed to fetch MR from GitLab: {e}")))?;

    let job_id = state.queue.push(payload).await.map_err(AppError::Redis)?;

    info!(
        job_id = %job_id,
        project = %req.project,
        mr_iid = %req.mr_iid,
        "Queued review via API"
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "status": "queued",
            "job_id": job_id,
        })),
    ))
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
