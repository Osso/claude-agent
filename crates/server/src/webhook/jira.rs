//! Jira webhook handler.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use tracing::{debug, error, info, warn};

use crate::jira::{self, JiraProjectMapping, JiraWebhookEvent};
use crate::payload::JiraTicketPayload;

use super::{branch_exists_on_platform, queued, skipped, AppError, AppState, WebhookResponse};

/// Jira webhook handler.
pub(super) async fn jira_webhook_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<WebhookResponse>), AppError> {
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

    if let Ok(s) = std::str::from_utf8(&body) {
        debug!(body = %s, "Raw Jira webhook body");
    }

    let event: JiraWebhookEvent = serde_json::from_slice(&body).map_err(|e| {
        if let Ok(s) = std::str::from_utf8(&body) {
            error!(error = %e, body = %s, "Failed to parse Jira webhook body");
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
        return Ok(super::ignored("No @claude-agent mention found"));
    }
    queue_jira_webhook_fix(&state, &event).await
}

/// Find mapping, check branch, build payload, and queue.
async fn queue_jira_webhook_fix(
    state: &AppState,
    event: &JiraWebhookEvent,
) -> Result<(StatusCode, Json<WebhookResponse>), AppError> {
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
            warn!(project = %project_key, "No project mapping for Jira project");
            AppError::BadRequest(format!(
                "No project mapping for Jira project: {}",
                project_key
            ))
        })?;

    let branch_name = format!("jira-fix/{}", event.issue.key.to_lowercase());
    if branch_exists_on_platform(state, &mapping.vcs_platform, &mapping.vcs_project, &branch_name)
        .await?
    {
        info!(branch = %branch_name, issue = %event.issue.key, "Fix branch already exists, skipping");
        return Ok(skipped(format!("Branch {} already exists", branch_name)));
    }

    let payload = build_jira_webhook_payload(event, mapping);
    let job_id = state.queue.push(payload).await.map_err(AppError::Redis)?;
    info!(job_id = %job_id, issue = %event.issue.key, "Queued Jira fix job");
    Ok(queued(job_id))
}

fn build_jira_webhook_payload(
    event: &JiraWebhookEvent,
    mapping: &JiraProjectMapping,
) -> JiraTicketPayload {
    let description = event
        .issue
        .fields
        .description
        .as_ref()
        .map(jira::extract_text_from_adf);

    JiraTicketPayload {
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
        priority: event
            .issue
            .fields
            .priority
            .as_ref()
            .map(|p| p.name.clone()),
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
    }
}
