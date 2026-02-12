//! Sentry webhook handler.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use tracing::{debug, error, info, warn};

use crate::payload::SentryFixPayload;
use crate::sentry::{self, SentryWebhookEvent};

use super::{branch_exists_on_platform, queued, skipped, AppError, AppState, WebhookResponse};

/// Sentry webhook handler.
pub(super) async fn sentry_webhook_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<WebhookResponse>), AppError> {
    let secret = state
        .sentry_webhook_secret
        .as_ref()
        .ok_or_else(|| AppError::Internal("Sentry webhook not configured".into()))?;
    let signature = headers
        .get("Sentry-Hook-Signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !sentry::verify_signature(secret, &body, signature) {
        warn!("Invalid Sentry webhook signature");
        return Err(AppError::Unauthorized);
    }

    if let Ok(s) = std::str::from_utf8(&body) {
        debug!(body = %s, "Raw Sentry webhook body");
    }

    let event: SentryWebhookEvent = serde_json::from_slice(&body).map_err(|e| {
        if let Ok(s) = std::str::from_utf8(&body) {
            error!(error = %e, body = %s, "Failed to parse Sentry webhook body");
        }
        AppError::BadRequest(format!("Invalid JSON: {e}"))
    })?;

    info!(
        action = %event.action,
        issue_id = %event.data.issue.short_id,
        project = %event.data.issue.project.slug,
        level = %event.data.issue.level.as_deref().unwrap_or("unknown"),
        "Received Sentry webhook"
    );

    if !event.should_fix() {
        debug!("Sentry event does not require fixing");
        return Ok(super::ignored("Event does not require fixing"));
    }
    queue_sentry_webhook_fix(&state, &event).await
}

/// Find mapping, check branch, build payload, and queue.
async fn queue_sentry_webhook_fix(
    state: &AppState,
    event: &SentryWebhookEvent,
) -> Result<(StatusCode, Json<WebhookResponse>), AppError> {
    let issue = event.issue();
    let mapping = state
        .sentry_project_mappings
        .iter()
        .find(|m| m.sentry_project == issue.project.slug)
        .ok_or_else(|| {
            warn!(project = %issue.project.slug, "No project mapping for Sentry project");
            AppError::BadRequest(format!(
                "No project mapping for Sentry project: {}",
                issue.project.slug
            ))
        })?;

    let organization = state
        .sentry_organization
        .as_ref()
        .ok_or_else(|| AppError::Internal("SENTRY_ORGANIZATION not configured".into()))?;

    let branch_name = format!("sentry-fix/{}", issue.short_id.to_lowercase());
    if branch_exists_on_platform(state, &mapping.vcs_platform, &mapping.vcs_project, &branch_name)
        .await?
    {
        info!(branch = %branch_name, issue = %issue.short_id, "Fix branch already exists, skipping");
        return Ok(skipped(format!("Branch {} already exists", branch_name)));
    }

    let payload = build_sentry_webhook_payload(issue, organization, mapping);
    let job_id = state.queue.push(payload).await.map_err(AppError::Redis)?;
    info!(job_id = %job_id, issue = %issue.short_id, "Queued Sentry fix job");
    Ok(queued(job_id))
}

fn build_sentry_webhook_payload(
    issue: &crate::sentry::Issue,
    organization: &str,
    mapping: &crate::sentry::SentryProjectMapping,
) -> SentryFixPayload {
    SentryFixPayload {
        issue_id: issue.id.clone(),
        short_id: issue.short_id.clone(),
        title: issue.title.clone(),
        culprit: issue.culprit.clone(),
        platform: issue.platform.clone(),
        issue_type: issue
            .issue_type
            .clone()
            .unwrap_or_else(|| "error".into()),
        issue_category: issue
            .issue_category
            .clone()
            .unwrap_or_else(|| "error".into()),
        web_url: issue.web_url.clone().unwrap_or_default(),
        project_slug: issue.project.slug.clone(),
        organization: organization.to_string(),
        clone_url: mapping.clone_url.clone(),
        target_branch: mapping.target_branch.clone(),
        vcs_platform: mapping.vcs_platform.clone(),
        vcs_project: mapping.vcs_project.clone(),
    }
}
