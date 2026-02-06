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
use crate::gitlab::{fetch_mr_by_branch, fetch_review_payload, MergeRequestEvent, NoteEvent, PipelineEvent, ReviewPayload};
use crate::jira::{self, JiraProjectMapping, JiraWebhookEvent};
use crate::jira_token::JiraTokenManager;
use crate::payload::{JiraTicketPayload, SentryFixPayload};
use crate::queue::Queue;
use crate::sentry::{self, SentryProjectMapping as SentryMapping, SentryWebhookEvent};

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
        .route("/webhook/sentry", post(sentry_webhook_handler))
        .route("/webhook/jira", post(jira_webhook_handler))
        // API endpoints for CLI
        .route("/api/stats", get(queue_stats_handler))
        .route("/api/failed", get(list_failed_handler))
        .route("/api/retry/{id}", post(retry_handler))
        .route("/api/review", post(queue_review_handler))
        .route("/api/review/github", post(queue_github_review_handler))
        .route("/api/sentry-fix", post(queue_sentry_fix_handler))
        .route("/api/jira-fix", post(queue_jira_fix_handler))
        .route("/api/check-tokens", get(check_tokens_handler))
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
        "note" => handle_note_event(&state, &body).await,
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

    let has_mr = event.merge_request.is_some();
    info!(
        project = %event.project.path_with_namespace,
        pipeline_id = %event.object_attributes.id,
        status = %event.object_attributes.status,
        ref_name = %event.object_attributes.ref_name,
        has_mr = %has_mr,
        "Received GitLab pipeline webhook"
    );

    // Only process failed pipelines
    if event.object_attributes.status != "failed" {
        debug!(status = %event.object_attributes.status, "Pipeline not failed, ignoring");
        return Ok((
            StatusCode::OK,
            Json(WebhookResponse {
                status: "ignored".into(),
                message: Some(format!(
                    "Pipeline status is '{}', not 'failed'",
                    event.object_attributes.status
                )),
                job_id: None,
            }),
        ));
    }

    // Get payload - either from webhook MR data or by looking up MR by branch
    let payload = if event.merge_request.is_some() {
        // MR data in webhook (rare but handle it)
        ReviewPayload::from(&event)
    } else {
        // Look up MR by branch name
        let gitlab_url = event
            .project
            .web_url
            .split('/')
            .take(3)
            .collect::<Vec<_>>()
            .join("/");

        match fetch_mr_by_branch(
            &gitlab_url,
            &event.project.path_with_namespace,
            &event.object_attributes.ref_name,
            &state.gitlab_token,
        )
        .await
        {
            Ok(Some(mut payload)) => {
                payload.action = "lint_fix".into();
                payload.author = event.user.username.clone();
                payload
            }
            Ok(None) => {
                debug!(
                    branch = %event.object_attributes.ref_name,
                    "No open MR found for branch"
                );
                return Ok((
                    StatusCode::OK,
                    Json(WebhookResponse {
                        status: "ignored".into(),
                        message: Some(format!(
                            "No open MR found for branch '{}'",
                            event.object_attributes.ref_name
                        )),
                        job_id: None,
                    }),
                ));
            }
            Err(e) => {
                warn!(error = %e, "Failed to look up MR by branch");
                return Ok((
                    StatusCode::OK,
                    Json(WebhookResponse {
                        status: "ignored".into(),
                        message: Some(format!("Failed to look up MR: {e}")),
                        job_id: None,
                    }),
                ));
            }
        }
    };

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

async fn handle_note_event(
    state: &AppState,
    body: &[u8],
) -> Result<(StatusCode, Json<WebhookResponse>), AppError> {
    let event: NoteEvent = serde_json::from_slice(body).map_err(|e| {
        if let Ok(body_str) = std::str::from_utf8(body) {
            error!(error = %e, body = %body_str, "Failed to parse note webhook");
        }
        AppError::BadRequest(format!("Invalid JSON: {e}"))
    })?;

    info!(
        project = %event.project.path_with_namespace,
        noteable_type = %event.object_attributes.noteable_type,
        user = %event.user.username,
        "Received GitLab note webhook"
    );

    // Only handle MR comments that mention @claude-agent
    if !event.is_merge_request_note() {
        debug!("Note is not on a merge request");
        return Ok((
            StatusCode::OK,
            Json(WebhookResponse {
                status: "ignored".into(),
                message: Some("Not a merge request note".into()),
                job_id: None,
            }),
        ));
    }

    if !event.mentions_bot() {
        debug!("Note does not mention @claude-agent");
        return Ok((
            StatusCode::OK,
            Json(WebhookResponse {
                status: "ignored".into(),
                message: Some("No @claude-agent mention".into()),
                job_id: None,
            }),
        ));
    }

    let mr = event.merge_request.as_ref().unwrap();

    // Only handle open MRs
    if mr.state != "opened" && mr.state != "reopened" {
        debug!(state = %mr.state, "MR is not open");
        return Ok((
            StatusCode::OK,
            Json(WebhookResponse {
                status: "ignored".into(),
                message: Some(format!("MR state is '{}', not open", mr.state)),
                job_id: None,
            }),
        ));
    }

    // Fetch full MR details (note webhook doesn't include clone_url)
    let gitlab_url = event
        .project
        .web_url
        .split('/')
        .take(3)
        .collect::<Vec<_>>()
        .join("/");

    let mut payload = fetch_review_payload(
        &gitlab_url,
        &event.project.path_with_namespace,
        mr.iid as u64,
        &state.gitlab_token,
    )
    .await
    .map_err(|e| {
        warn!(error = %e, "Failed to fetch MR details for note event");
        AppError::Internal(format!("Failed to fetch MR details: {e}"))
    })?;

    let instruction = event.instruction();
    payload.action = "comment".into();
    payload.trigger_comment = Some(if instruction.is_empty() {
        "review this".into()
    } else {
        instruction.to_string()
    });

    let job_id = state.queue.push(payload).await.map_err(AppError::Redis)?;

    info!(
        job_id = %job_id,
        mr_iid = %mr.iid,
        instruction = %instruction,
        "Queued comment-triggered job"
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(WebhookResponse {
            status: "queued".into(),
            message: Some("Comment-triggered job queued".into()),
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

/// Sentry webhook handler.
async fn sentry_webhook_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    // Check if Sentry webhook is configured
    let sentry_secret = state
        .sentry_webhook_secret
        .as_ref()
        .ok_or_else(|| AppError::Internal("Sentry webhook not configured".into()))?;

    // Verify HMAC-SHA256 signature
    let signature = headers
        .get("Sentry-Hook-Signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !sentry::verify_signature(sentry_secret, &body, signature) {
        warn!("Invalid Sentry webhook signature");
        return Err(AppError::Unauthorized);
    }

    // Log raw body for debugging
    if let Ok(body_str) = std::str::from_utf8(&body) {
        debug!(body = %body_str, "Raw Sentry webhook body");
    }

    // Parse event
    let event: SentryWebhookEvent = serde_json::from_slice(&body).map_err(|e| {
        if let Ok(body_str) = std::str::from_utf8(&body) {
            error!(error = %e, body = %body_str, "Failed to parse Sentry webhook body");
        }
        AppError::BadRequest(format!("Invalid JSON: {e}"))
    })?;

    info!(
        action = %event.action,
        issue_id = %event.data.issue.short_id,
        project = %event.data.issue.project.slug,
        "Received Sentry webhook"
    );

    if !event.should_fix() {
        debug!("Sentry event does not require fixing");
        return Ok((
            StatusCode::OK,
            Json(WebhookResponse {
                status: "ignored".into(),
                message: Some("Event does not require fixing".into()),
                job_id: None,
            }),
        ));
    }

    // Find project mapping
    let issue = event.issue();
    let mapping = state
        .sentry_project_mappings
        .iter()
        .find(|m| m.sentry_project == issue.project.slug)
        .ok_or_else(|| {
            warn!(
                project = %issue.project.slug,
                "No project mapping for Sentry project"
            );
            AppError::BadRequest(format!(
                "No project mapping for Sentry project: {}",
                issue.project.slug
            ))
        })?;

    let organization = state
        .sentry_organization
        .as_ref()
        .ok_or_else(|| AppError::Internal("SENTRY_ORGANIZATION not configured".into()))?;

    // Check if fix branch already exists
    let branch_name = format!("sentry-fix/{}", issue.short_id.to_lowercase());
    let branch_exists = if mapping.vcs_platform == "github" {
        let token = state.github_token.as_ref().ok_or_else(|| {
            AppError::Internal("GITHUB_TOKEN not configured for GitHub repo".into())
        })?;
        crate::github::branch_exists(&mapping.vcs_project, &branch_name, token).await
    } else {
        crate::gitlab::branch_exists(
            "https://gitlab.com",
            &mapping.vcs_project,
            &branch_name,
            &state.gitlab_token,
        )
        .await
    };

    if branch_exists.unwrap_or(false) {
        info!(
            branch = %branch_name,
            issue = %issue.short_id,
            "Fix branch already exists, skipping"
        );
        return Ok((
            StatusCode::OK,
            Json(WebhookResponse {
                status: "skipped".into(),
                message: Some(format!("Branch {} already exists", branch_name)),
                job_id: None,
            }),
        ));
    }

    let payload = SentryFixPayload {
        issue_id: issue.id.clone(),
        short_id: issue.short_id.clone(),
        title: issue.title.clone(),
        culprit: issue.culprit.clone(),
        platform: issue.platform.clone(),
        issue_type: issue.issue_type.clone().unwrap_or_else(|| "error".into()),
        issue_category: issue.issue_category.clone().unwrap_or_else(|| "error".into()),
        web_url: issue.web_url.clone().unwrap_or_default(),
        project_slug: issue.project.slug.clone(),
        organization: organization.clone(),
        clone_url: mapping.clone_url.clone(),
        target_branch: mapping.target_branch.clone(),
        vcs_platform: mapping.vcs_platform.clone(),
        vcs_project: mapping.vcs_project.clone(),
    };

    let job_id = state.queue.push(payload).await.map_err(AppError::Redis)?;

    info!(job_id = %job_id, issue = %issue.short_id, "Queued Sentry fix job");

    Ok((
        StatusCode::ACCEPTED,
        Json(WebhookResponse {
            status: "queued".into(),
            message: None,
            job_id: Some(job_id),
        }),
    ))
}

/// Jira webhook handler.
async fn jira_webhook_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    // Verify HMAC signature if secret is configured
    if let Some(ref secret) = state.jira_webhook_secret {
        let signature = headers
            .get("X-Hub-Signature")
            .or_else(|| headers.get("X-Atlassian-Webhook-Signature"))
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if !jira::verify_signature(secret, &body, signature) {
            warn!("Invalid Jira webhook signature");
            return Err(AppError::Unauthorized);
        }
    }

    // Log raw body for debugging
    if let Ok(body_str) = std::str::from_utf8(&body) {
        debug!(body = %body_str, "Raw Jira webhook body");
    }

    // Parse event
    let event: JiraWebhookEvent = serde_json::from_slice(&body).map_err(|e| {
        if let Ok(body_str) = std::str::from_utf8(&body) {
            error!(error = %e, body = %body_str, "Failed to parse Jira webhook body");
        }
        AppError::BadRequest(format!("Invalid JSON: {e}"))
    })?;

    info!(
        webhook_event = %event.webhook_event,
        issue_key = %event.issue.key,
        "Received Jira webhook"
    );

    if !event.should_trigger() {
        debug!("Jira event does not trigger bot (no @claude-agent mention)");
        return Ok((
            StatusCode::OK,
            Json(WebhookResponse {
                status: "ignored".into(),
                message: Some("No @claude-agent mention found".into()),
                job_id: None,
            }),
        ));
    }

    // Find project mapping by Jira project key
    let project_key = event
        .issue
        .fields
        .project
        .as_ref()
        .map(|p| p.key.as_str())
        .unwrap_or("");

    let mapping = state
        .jira_project_mappings
        .iter()
        .find(|m| m.jira_project == project_key)
        .ok_or_else(|| {
            warn!(
                project = %project_key,
                "No project mapping for Jira project"
            );
            AppError::BadRequest(format!(
                "No project mapping for Jira project: {}",
                project_key
            ))
        })?;

    // Check if fix branch already exists
    let branch_name = format!("jira-fix/{}", event.issue.key.to_lowercase());
    let branch_exists = if mapping.vcs_platform == "github" {
        let token = state.github_token.as_ref().ok_or_else(|| {
            AppError::Internal("GITHUB_TOKEN not configured for GitHub repo".into())
        })?;
        crate::github::branch_exists(&mapping.vcs_project, &branch_name, token).await
    } else {
        crate::gitlab::branch_exists(
            "https://gitlab.com",
            &mapping.vcs_project,
            &branch_name,
            &state.gitlab_token,
        )
        .await
    };

    if branch_exists.unwrap_or(false) {
        info!(
            branch = %branch_name,
            issue = %event.issue.key,
            "Fix branch already exists, skipping"
        );
        return Ok((
            StatusCode::OK,
            Json(WebhookResponse {
                status: "skipped".into(),
                message: Some(format!("Branch {} already exists", branch_name)),
                job_id: None,
            }),
        ));
    }

    // Extract description as plain text
    let description = event
        .issue
        .fields
        .description
        .as_ref()
        .map(|d| jira::extract_text_from_adf(d));

    // Build payload
    let payload = JiraTicketPayload {
        issue_key: event.issue.key.clone(),
        issue_id: event.issue.id.clone(),
        summary: event.issue.fields.summary.clone(),
        description,
        issue_type: event
            .issue
            .fields
            .issue_type
            .as_ref()
            .map(|t| t.name.clone())
            .unwrap_or_else(|| "Unknown".into()),
        priority: event.issue.fields.priority.as_ref().map(|p| p.name.clone()),
        status: event
            .issue
            .fields
            .status
            .as_ref()
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "Unknown".into()),
        labels: event.issue.fields.labels.clone(),
        web_url: event.issue_web_url(),
        jira_base_url: event.jira_base_url().unwrap_or_default(),
        trigger_comment: event
            .comment
            .as_ref()
            .map(|c| c.body_as_text())
            .unwrap_or_default(),
        trigger_author: event
            .comment
            .as_ref()
            .and_then(|c| c.author.as_ref())
            .and_then(|a| a.display_name.clone()),
        clone_url: mapping.clone_url.clone(),
        target_branch: mapping.target_branch.clone(),
        vcs_platform: mapping.vcs_platform.clone(),
        vcs_project: mapping.vcs_project.clone(),
    };

    let job_id = state.queue.push(payload).await.map_err(AppError::Redis)?;

    info!(job_id = %job_id, issue = %event.issue.key, "Queued Jira fix job");

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
    /// Action: "open" (default) or "lint_fix"
    #[serde(default)]
    action: Option<String>,
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
    let mut payload = fetch_review_payload(&req.gitlab_url, &req.project, req.mr_iid, &state.gitlab_token)
        .await
        .map_err(|e| AppError::Internal(format!("Failed to fetch MR from GitLab: {e}")))?;

    if let Some(action) = &req.action {
        payload.action = action.clone();
    }

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

/// Queue a GitHub PR review via API.
#[derive(Deserialize)]
struct QueueGithubReviewRequest {
    /// Repository (e.g., "owner/repo")
    repo: String,
    /// Pull request number
    pr: u64,
    /// Action: "open" (default) or "lint_fix"
    #[serde(default)]
    action: Option<String>,
}

async fn queue_github_review_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<QueueGithubReviewRequest>,
) -> Result<impl IntoResponse, AppError> {
    if !state.verify_api_key(&headers) {
        warn!("Invalid API key for /api/review/github");
        return Err(AppError::Unauthorized);
    }

    let github_token = state
        .github_token
        .as_ref()
        .ok_or_else(|| AppError::Internal("GitHub token not configured".into()))?;

    let mut payload = fetch_github_pr_payload(&req.repo, req.pr, github_token)
        .await
        .map_err(|e| AppError::Internal(format!("Failed to fetch PR from GitHub: {e}")))?;

    if let Some(action) = &req.action {
        payload.action = action.clone();
    }

    let job_id = state.queue.push(payload).await.map_err(AppError::Redis)?;

    info!(
        job_id = %job_id,
        repo = %req.repo,
        pr = %req.pr,
        "Queued GitHub review via API"
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "status": "queued",
            "job_id": job_id,
        })),
    ))
}

/// Fetch PR details from GitHub API and build a ReviewPayload.
async fn fetch_github_pr_payload(repo: &str, pr: u64, token: &str) -> anyhow::Result<ReviewPayload> {
    let client = reqwest::Client::new();

    // Fetch PR
    let pr_url = format!("https://api.github.com/repos/{}/pulls/{}", repo, pr);
    let pr_resp: serde_json::Value = client
        .get(&pr_url)
        .header("Authorization", format!("Bearer {}", token))
        .header("User-Agent", "claude-agent")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let title = pr_resp["title"].as_str().unwrap_or("").to_string();
    let description = pr_resp["body"].as_str().map(|s| s.to_string());
    let source_branch = pr_resp["head"]["ref"].as_str().unwrap_or("").to_string();
    let target_branch = pr_resp["base"]["ref"].as_str().unwrap_or("").to_string();
    let author = pr_resp["user"]["login"].as_str().unwrap_or("").to_string();
    let clone_url = pr_resp["head"]["repo"]["clone_url"]
        .as_str()
        .unwrap_or("")
        .to_string();

    Ok(ReviewPayload {
        project: repo.to_string(),
        mr_iid: pr.to_string(),
        title,
        description,
        source_branch,
        target_branch,
        author,
        clone_url,
        action: "open".to_string(),
        gitlab_url: String::new(),
        platform: "github".to_string(),
        trigger_comment: None,
    })
}

/// Queue a Sentry fix via API.
#[derive(Deserialize)]
struct QueueSentryFixRequest {
    /// Sentry organization
    organization: String,
    /// Sentry project slug
    project: String,
    /// Sentry issue ID (numeric or short ID like "WEB-123")
    issue_id: String,
}

async fn queue_sentry_fix_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<QueueSentryFixRequest>,
) -> Result<impl IntoResponse, AppError> {
    if !state.verify_api_key(&headers) {
        warn!("Invalid API key for /api/sentry-fix");
        return Err(AppError::Unauthorized);
    }

    // Find project mapping
    let mapping = state
        .sentry_project_mappings
        .iter()
        .find(|m| m.sentry_project == req.project)
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "No project mapping for Sentry project: {}",
                req.project
            ))
        })?;

    // Fetch issue details from Sentry to get the short_id
    let sentry_token = state.sentry_auth_token.as_ref().ok_or_else(|| {
        AppError::Internal("SENTRY_AUTH_TOKEN not configured".into())
    })?;
    let sentry_client = crate::sentry_api::SentryClient::new(&req.organization, sentry_token)
        .map_err(|e| AppError::Internal(format!("Failed to create Sentry client: {e}")))?;

    let issue = sentry_client
        .get_issue(&req.issue_id)
        .await
        .map_err(|e| AppError::Internal(format!("Failed to fetch Sentry issue: {e}")))?;

    let short_id = issue["shortId"]
        .as_str()
        .unwrap_or(&req.issue_id)
        .to_string();

    // Check if fix branch already exists
    let branch_name = format!("sentry-fix/{}", short_id.to_lowercase());
    let branch_exists = if mapping.vcs_platform == "github" {
        let token = state.github_token.as_ref().ok_or_else(|| {
            AppError::Internal("GITHUB_TOKEN not configured for GitHub repo".into())
        })?;
        crate::github::branch_exists(&mapping.vcs_project, &branch_name, token).await
    } else {
        crate::gitlab::branch_exists(
            "https://gitlab.com",
            &mapping.vcs_project,
            &branch_name,
            &state.gitlab_token,
        )
        .await
    };

    if branch_exists.unwrap_or(false) {
        info!(
            branch = %branch_name,
            issue = %short_id,
            "Fix branch already exists, skipping"
        );
        return Ok((
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "skipped",
                "message": format!("Branch {} already exists", branch_name),
            })),
        ));
    }

    // Construct payload with real short_id
    let payload = SentryFixPayload {
        issue_id: req.issue_id.clone(),
        short_id: short_id.clone(),
        title: issue["title"].as_str().unwrap_or("").to_string(),
        culprit: issue["culprit"].as_str().unwrap_or("").to_string(),
        platform: issue["platform"].as_str().unwrap_or("").to_string(),
        issue_type: issue["issueType"]
            .as_str()
            .unwrap_or("error")
            .to_string(),
        issue_category: issue["issueCategory"]
            .as_str()
            .unwrap_or("error")
            .to_string(),
        web_url: issue["permalink"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| {
                format!(
                    "https://sentry.io/organizations/{}/issues/{}/",
                    req.organization, req.issue_id
                )
            }),
        project_slug: req.project.clone(),
        organization: req.organization.clone(),
        clone_url: mapping.clone_url.clone(),
        target_branch: mapping.target_branch.clone(),
        vcs_platform: mapping.vcs_platform.clone(),
        vcs_project: mapping.vcs_project.clone(),
    };

    let job_id = state.queue.push(payload).await.map_err(AppError::Redis)?;

    info!(
        job_id = %job_id,
        org = %req.organization,
        project = %req.project,
        issue = %short_id,
        "Queued Sentry fix via API"
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "status": "queued",
            "job_id": job_id,
        })),
    ))
}

/// Queue a Jira fix via API.
#[derive(Deserialize)]
struct QueueJiraFixRequest {
    /// Jira issue key (e.g., "GC-123")
    issue_key: String,
    /// Jira base URL (defaults to globalcomix)
    #[serde(default = "default_jira_url")]
    jira_url: String,
}

fn default_jira_url() -> String {
    "https://globalcomix.atlassian.net".into()
}

async fn queue_jira_fix_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<QueueJiraFixRequest>,
) -> Result<impl IntoResponse, AppError> {
    if !state.verify_api_key(&headers) {
        warn!("Invalid API key for /api/jira-fix");
        return Err(AppError::Unauthorized);
    }

    // Extract project key from issue key (e.g., "GC" from "GC-123")
    let project_key = req
        .issue_key
        .split('-')
        .next()
        .ok_or_else(|| AppError::BadRequest("Invalid issue key format".into()))?;

    // Find project mapping
    let mapping = state
        .jira_project_mappings
        .iter()
        .find(|m| m.jira_project == project_key)
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "No project mapping for Jira project: {}",
                project_key
            ))
        })?;

    // Check if fix branch already exists
    let branch_name = format!("jira-fix/{}", req.issue_key.to_lowercase());
    let branch_exists = if mapping.vcs_platform == "github" {
        let token = state.github_token.as_ref().ok_or_else(|| {
            AppError::Internal("GITHUB_TOKEN not configured for GitHub repo".into())
        })?;
        crate::github::branch_exists(&mapping.vcs_project, &branch_name, token).await
    } else {
        crate::gitlab::branch_exists(
            "https://gitlab.com",
            &mapping.vcs_project,
            &branch_name,
            &state.gitlab_token,
        )
        .await
    };

    if branch_exists.unwrap_or(false) {
        info!(
            branch = %branch_name,
            issue = %req.issue_key,
            "Fix branch already exists, skipping"
        );
        return Ok((
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "skipped",
                "message": format!("Branch {} already exists", branch_name),
            })),
        ));
    }

    // Fetch issue details from Jira API
    let jira_token = match &state.jira_token_manager {
        Some(manager) => manager
            .get_access_token()
            .await
            .map_err(|e| AppError::Internal(format!("Failed to get Jira token: {e}")))?,
        None => {
            return Err(AppError::Internal("Jira integration not configured".into()));
        }
    };

    let client = reqwest::Client::new();
    let issue_url = format!(
        "{}/rest/api/3/issue/{}",
        req.jira_url.trim_end_matches('/'),
        req.issue_key
    );

    let issue: serde_json::Value = client
        .get(&issue_url)
        .header("Authorization", format!("Bearer {}", jira_token))
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to fetch Jira issue: {e}")))?
        .error_for_status()
        .map_err(|e| AppError::Internal(format!("Jira API error: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to parse Jira response: {e}")))?;

    let fields = &issue["fields"];
    let description = fields
        .get("description")
        .map(|d| jira::extract_text_from_adf(d));

    let payload = JiraTicketPayload {
        issue_key: req.issue_key.clone(),
        issue_id: issue["id"].as_str().unwrap_or("").to_string(),
        summary: fields["summary"].as_str().unwrap_or("").to_string(),
        description,
        issue_type: fields["issuetype"]["name"]
            .as_str()
            .unwrap_or("Unknown")
            .to_string(),
        priority: fields["priority"]["name"].as_str().map(String::from),
        status: fields["status"]["name"]
            .as_str()
            .unwrap_or("Unknown")
            .to_string(),
        labels: fields["labels"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        web_url: format!("{}/browse/{}", req.jira_url.trim_end_matches('/'), req.issue_key),
        jira_base_url: req.jira_url.clone(),
        trigger_comment: "Triggered via API".into(),
        trigger_author: None,
        clone_url: mapping.clone_url.clone(),
        target_branch: mapping.target_branch.clone(),
        vcs_platform: mapping.vcs_platform.clone(),
        vcs_project: mapping.vcs_project.clone(),
    };

    let job_id = state.queue.push(payload).await.map_err(AppError::Redis)?;

    info!(
        job_id = %job_id,
        issue = %req.issue_key,
        "Queued Jira fix via API"
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "status": "queued",
            "job_id": job_id,
        })),
    ))
}

/// Check tokens endpoint - validates configured tokens
async fn check_tokens_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    if !state.verify_api_key(&headers) {
        warn!("Invalid API key for /api/check-tokens");
        return Err(AppError::Unauthorized);
    }

    let client = reqwest::Client::new();

    // Check GitLab token
    let gitlab = check_gitlab_token(&client, &state.gitlab_token).await;

    // Check GitHub token
    let github = match &state.github_token {
        Some(token) => check_github_token(&client, token).await,
        None => TokenStatus {
            configured: false,
            valid: false,
            info: None,
            error: None,
        },
    };

    // Check Sentry token
    let sentry = match &state.sentry_auth_token {
        Some(token) => check_sentry_token(&client, token).await,
        None => TokenStatus {
            configured: false,
            valid: false,
            info: None,
            error: None,
        },
    };

    // Check Claude token
    let claude = match &state.claude_token {
        Some(token) => check_claude_token(&client, token).await,
        None => TokenStatus {
            configured: false,
            valid: false,
            info: None,
            error: None,
        },
    };

    // Check Jira token
    let jira = match &state.jira_token_manager {
        Some(manager) => check_jira_token(manager).await,
        None => TokenStatus {
            configured: false,
            valid: false,
            info: None,
            error: None,
        },
    };

    Ok(Json(serde_json::json!({
        "gitlab": gitlab,
        "github": github,
        "sentry": sentry,
        "claude": claude,
        "jira": jira,
    })))
}

#[derive(Serialize)]
struct TokenStatus {
    configured: bool,
    valid: bool,
    info: Option<String>,
    error: Option<String>,
}

async fn check_gitlab_token(client: &reqwest::Client, token: &str) -> TokenStatus {
    let resp = client
        .get("https://gitlab.com/api/v4/user")
        .header("PRIVATE-TOKEN", token)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            #[derive(Deserialize)]
            struct User {
                username: String,
            }
            match r.json::<User>().await {
                Ok(u) => TokenStatus {
                    configured: true,
                    valid: true,
                    info: Some(format!("@{}", u.username)),
                    error: None,
                },
                Err(e) => TokenStatus {
                    configured: true,
                    valid: false,
                    info: None,
                    error: Some(e.to_string()),
                },
            }
        }
        Ok(r) => TokenStatus {
            configured: true,
            valid: false,
            info: None,
            error: Some(format!("{}", r.status())),
        },
        Err(e) => TokenStatus {
            configured: true,
            valid: false,
            info: None,
            error: Some(e.to_string()),
        },
    }
}

async fn check_github_token(client: &reqwest::Client, token: &str) -> TokenStatus {
    let resp = client
        .get("https://api.github.com/user")
        .header("Authorization", format!("Bearer {}", token))
        .header("User-Agent", "claude-agent")
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            #[derive(Deserialize)]
            struct User {
                login: String,
            }
            match r.json::<User>().await {
                Ok(u) => TokenStatus {
                    configured: true,
                    valid: true,
                    info: Some(format!("@{}", u.login)),
                    error: None,
                },
                Err(e) => TokenStatus {
                    configured: true,
                    valid: false,
                    info: None,
                    error: Some(e.to_string()),
                },
            }
        }
        Ok(r) => TokenStatus {
            configured: true,
            valid: false,
            info: None,
            error: Some(format!("{}", r.status())),
        },
        Err(e) => TokenStatus {
            configured: true,
            valid: false,
            info: None,
            error: Some(e.to_string()),
        },
    }
}

async fn check_sentry_token(client: &reqwest::Client, token: &str) -> TokenStatus {
    let resp = client
        .get("https://sentry.io/api/0/organizations/")
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            #[derive(Deserialize)]
            struct Org {
                slug: String,
            }
            match r.json::<Vec<Org>>().await {
                Ok(orgs) => {
                    let slugs: Vec<_> = orgs.iter().map(|o| o.slug.as_str()).collect();
                    TokenStatus {
                        configured: true,
                        valid: true,
                        info: Some(format!("orgs: {}", slugs.join(", "))),
                        error: None,
                    }
                }
                Err(e) => TokenStatus {
                    configured: true,
                    valid: false,
                    info: None,
                    error: Some(e.to_string()),
                },
            }
        }
        Ok(r) => TokenStatus {
            configured: true,
            valid: false,
            info: None,
            error: Some(format!("{}", r.status())),
        },
        Err(e) => TokenStatus {
            configured: true,
            valid: false,
            info: None,
            error: Some(e.to_string()),
        },
    }
}

async fn check_claude_token(_client: &reqwest::Client, token: &str) -> TokenStatus {
    // OAuth tokens from `claude setup-token` are restricted to Claude Code only
    // and cannot be validated via direct API calls. We verify the format instead.
    if token.starts_with("sk-ant-oat01-") {
        TokenStatus {
            configured: true,
            valid: true,
            info: Some("OAuth token (format valid)".to_string()),
            error: None,
        }
    } else if token.starts_with("sk-ant-api") {
        TokenStatus {
            configured: true,
            valid: true,
            info: Some("API key (format valid)".to_string()),
            error: None,
        }
    } else {
        TokenStatus {
            configured: true,
            valid: false,
            info: None,
            error: Some("unrecognized token format".to_string()),
        }
    }
}

async fn check_jira_token(manager: &JiraTokenManager) -> TokenStatus {
    // Try to get an access token - this will refresh if needed
    match manager.get_access_token_with_expiry().await {
        Ok((_token, expires_in_secs)) => {
            let mins = expires_in_secs / 60;
            TokenStatus {
                configured: true,
                valid: true,
                info: Some(format!("expires in {}m", mins)),
                error: None,
            }
        }
        Err(e) => TokenStatus {
            configured: true,
            valid: false,
            info: None,
            error: Some(e.to_string()),
        },
    }
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
