//! Webhook HTTP handler and API endpoints.

use std::sync::Arc;

use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;

use crate::jira::JiraProjectMapping;
use crate::jira_token::JiraTokenManager;
use crate::queue::Queue;
use crate::sentry::SentryProjectMapping as SentryMapping;

mod api;
mod github;
mod gitlab;
mod jira;
mod sentry;
mod tokens;

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
    /// Sentry webhook secret (optional, for Sentry webhook support)
    pub sentry_webhook_secret: Option<String>,
    /// Sentry auth token for API calls
    pub sentry_auth_token: Option<String>,
    /// Claude OAuth token
    pub claude_token: Option<String>,
    /// Sentry organization
    pub sentry_organization: Option<String>,
    /// Sentry project mappings
    pub sentry_project_mappings: Vec<SentryMapping>,
    /// Jira token manager for OAuth token refresh
    pub jira_token_manager: Option<Arc<JiraTokenManager>>,
    /// Jira webhook secret (optional, for HMAC verification)
    pub jira_webhook_secret: Option<String>,
    /// Jira project mappings
    pub jira_project_mappings: Vec<JiraProjectMapping>,
    /// Allowed MR/PR authors for automatic processing (empty = allow all)
    pub allowed_authors: Vec<String>,
}

impl AppState {
    /// Verify API key from Authorization: Bearer header.
    pub(crate) fn verify_api_key(&self, headers: &HeaderMap) -> bool {
        let expected = self.api_key.as_ref().unwrap_or(&self.webhook_secret);
        if let Some(auth) = headers.get("Authorization").and_then(|v| v.to_str().ok())
            && let Some(token) = auth.strip_prefix("Bearer ")
        {
            return token == expected;
        }
        false
    }

    /// Check if an MR/PR author is allowed for automatic processing.
    /// Returns true if the allowlist is empty (all allowed) or the author is listed.
    pub(crate) fn is_author_allowed(&self, author: &str) -> bool {
        self.allowed_authors.is_empty()
            || self
                .allowed_authors
                .iter()
                .any(|a| a.eq_ignore_ascii_case(author))
    }
}

/// Build the HTTP router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health_handler))
        .route("/webhook/gitlab", post(gitlab::gitlab_webhook_handler))
        .route("/webhook/github", post(github::github_webhook_handler))
        .route("/webhook/sentry", post(sentry::sentry_webhook_handler))
        .route("/webhook/jira", post(jira::jira_webhook_handler))
        .route("/api/stats", get(api::queue_stats_handler))
        .route("/api/failed", get(api::list_failed_handler))
        .route("/api/retry/{id}", post(api::retry_handler))
        .route("/api/review", post(api::queue_review_handler))
        .route(
            "/api/review/github",
            post(api::queue_github_review_handler),
        )
        .route("/api/sentry-fix", post(api::queue_sentry_fix_handler))
        .route("/api/jira-fix", post(api::queue_jira_fix_handler))
        .route("/api/check-tokens", get(tokens::check_tokens_handler))
        // Legacy endpoint
        .route("/queue/stats", get(api::queue_stats_handler))
        .with_state(Arc::new(state))
}

async fn health_handler() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

// -- Shared response types and helpers --

#[derive(Serialize)]
pub(crate) struct WebhookResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
}

pub(crate) fn ignored(message: impl Into<String>) -> (StatusCode, Json<WebhookResponse>) {
    (
        StatusCode::OK,
        Json(WebhookResponse {
            status: "ignored".into(),
            message: Some(message.into()),
            job_id: None,
        }),
    )
}

pub(crate) fn queued(job_id: String) -> (StatusCode, Json<WebhookResponse>) {
    (
        StatusCode::ACCEPTED,
        Json(WebhookResponse {
            status: "queued".into(),
            message: None,
            job_id: Some(job_id),
        }),
    )
}

pub(crate) fn queued_with_message(
    job_id: String,
    message: impl Into<String>,
) -> (StatusCode, Json<WebhookResponse>) {
    (
        StatusCode::ACCEPTED,
        Json(WebhookResponse {
            status: "queued".into(),
            message: Some(message.into()),
            job_id: Some(job_id),
        }),
    )
}

pub(crate) fn skipped(message: impl Into<String>) -> (StatusCode, Json<WebhookResponse>) {
    (
        StatusCode::OK,
        Json(WebhookResponse {
            status: "skipped".into(),
            message: Some(message.into()),
            job_id: None,
        }),
    )
}

/// Check if a branch exists on the VCS platform.
pub(crate) async fn branch_exists_on_platform(
    state: &AppState,
    vcs_platform: &str,
    vcs_project: &str,
    branch_name: &str,
) -> Result<bool, AppError> {
    let exists = if vcs_platform == "github" {
        let token = state.github_token.as_ref().ok_or_else(|| {
            AppError::Internal("GITHUB_TOKEN not configured for GitHub repo".into())
        })?;
        crate::github::branch_exists(vcs_project, branch_name, token)
            .await
            .unwrap_or(false)
    } else {
        crate::gitlab::branch_exists(
            "https://gitlab.com",
            vcs_project,
            branch_name,
            &state.gitlab_token,
        )
        .await
        .unwrap_or(false)
    };
    Ok(exists)
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
