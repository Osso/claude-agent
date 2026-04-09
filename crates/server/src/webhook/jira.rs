//! Jira webhook handler.

use std::sync::Arc;

use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use tracing::{debug, error, info, warn};

use crate::jira::{self, JiraProjectMapping, JiraWebhookEvent};
use crate::payload::JiraTicketPayload;

use super::{AppError, AppState, WebhookResponse, branch_exists_on_platform, queued, skipped};

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
    let project_key = project_key(event);
    let mapping = find_project_mapping(state, project_key)?;
    let branch_name = format!("jira-fix/{}", event.issue.key.to_lowercase());
    if branch_exists_for_mapping(state, mapping, &branch_name).await? {
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
    JiraTicketPayload {
        issue_key: event.issue.key.clone(),
        issue_id: event.issue.id.clone(),
        summary: event.issue.fields.summary.clone(),
        description: issue_description(event),
        issue_type: issue_type_name(event),
        priority: event.issue.fields.priority.as_ref().map(|p| p.name.clone()),
        status: issue_status_name(event),
        labels: event.issue.fields.labels.clone(),
        web_url: event.issue_web_url(),
        jira_base_url: event.jira_base_url().unwrap_or_default(),
        trigger_comment: trigger_comment_text(event),
        trigger_author: trigger_author(event),
        clone_url: mapping.clone_url.clone(),
        target_branch: mapping.target_branch.clone(),
        vcs_platform: mapping.vcs_platform.clone(),
        vcs_project: mapping.vcs_project.clone(),
    }
}

fn project_key(event: &JiraWebhookEvent) -> &str {
    event
        .issue
        .fields
        .project
        .as_ref()
        .map(|project| project.key.as_str())
        .unwrap_or("")
}

fn find_project_mapping<'a>(
    state: &'a AppState,
    project_key: &str,
) -> Result<&'a JiraProjectMapping, AppError> {
    state
        .jira_project_mappings
        .iter()
        .find(|mapping| mapping.jira_project == project_key)
        .ok_or_else(|| {
            warn!(project = %project_key, "No project mapping for Jira project");
            AppError::BadRequest(format!(
                "No project mapping for Jira project: {}",
                project_key
            ))
        })
}

async fn branch_exists_for_mapping(
    state: &AppState,
    mapping: &JiraProjectMapping,
    branch_name: &str,
) -> Result<bool, AppError> {
    branch_exists_on_platform(
        state,
        &mapping.vcs_platform,
        &mapping.vcs_project,
        branch_name,
    )
    .await
}

fn issue_description(event: &JiraWebhookEvent) -> Option<String> {
    event
        .issue
        .fields
        .description
        .as_ref()
        .map(jira::extract_text_from_adf)
}

fn issue_type_name(event: &JiraWebhookEvent) -> String {
    event
        .issue
        .fields
        .issue_type
        .as_ref()
        .map(|issue_type| issue_type.name.clone())
        .unwrap_or_else(|| "Unknown".into())
}

fn issue_status_name(event: &JiraWebhookEvent) -> String {
    event
        .issue
        .fields
        .status
        .as_ref()
        .map(|status| status.name.clone())
        .unwrap_or_else(|| "Unknown".into())
}

fn trigger_comment_text(event: &JiraWebhookEvent) -> String {
    event
        .comment
        .as_ref()
        .map(|comment| comment.body_as_text())
        .unwrap_or_default()
}

fn trigger_author(event: &JiraWebhookEvent) -> Option<String> {
    event
        .comment
        .as_ref()
        .and_then(|comment| comment.author.as_ref())
        .and_then(|author| author.display_name.clone())
}
