//! GitLab webhook handlers (merge request, pipeline, note).

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;
use tracing::{debug, error, info, warn};

use crate::gitlab::{
    fetch_mr_by_branch, fetch_review_payload, MergeRequestEvent, NoteEvent, PipelineEvent,
    ReviewPayload,
};

use super::{ignored, queued, queued_with_message, AppError, AppState, WebhookResponse};

#[derive(Deserialize)]
struct EventKind {
    object_kind: String,
}

/// GitLab webhook entry point -- dispatches by object_kind.
pub(super) async fn gitlab_webhook_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<WebhookResponse>), AppError> {
    let token = headers
        .get("X-Gitlab-Token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if token != state.webhook_secret {
        warn!("Invalid webhook token");
        return Err(AppError::Unauthorized);
    }
    if let Ok(s) = std::str::from_utf8(&body) {
        debug!(body = %s, "Raw webhook body");
    }

    let kind: EventKind = serde_json::from_slice(&body)
        .map_err(|e| AppError::BadRequest(format!("Invalid JSON: {e}")))?;

    match kind.object_kind.as_str() {
        "merge_request" => handle_merge_request_event(&state, &body).await,
        "pipeline" => handle_pipeline_event(&state, &body).await,
        "note" => handle_note_event(&state, &body).await,
        other => {
            debug!(object_kind = other, "Ignoring unsupported GitLab event type");
            Ok(ignored(format!("Unsupported event type: {other}")))
        }
    }
}

async fn handle_merge_request_event(
    state: &AppState,
    body: &[u8],
) -> Result<(StatusCode, Json<WebhookResponse>), AppError> {
    let event: MergeRequestEvent = parse_body(body, "merge request")?;
    info!(
        project = %event.project.path_with_namespace,
        mr_iid = %event.object_attributes.iid,
        action = ?event.object_attributes.action,
        "Received GitLab merge request webhook"
    );

    if !event.should_review() {
        debug!("Event does not require review");
        return Ok(ignored("Event does not require review"));
    }
    if event.has_label("skip-review") {
        debug!("MR has skip-review label");
        return Ok(super::skipped("MR has skip-review label"));
    }
    let payload = ReviewPayload::from(&event);
    let job_id = state.queue.push(payload).await.map_err(AppError::Redis)?;
    info!(job_id = %job_id, "Queued review job");
    Ok(queued(job_id))
}

async fn handle_pipeline_event(
    state: &AppState,
    body: &[u8],
) -> Result<(StatusCode, Json<WebhookResponse>), AppError> {
    let event: PipelineEvent = parse_body(body, "pipeline")?;
    info!(
        project = %event.project.path_with_namespace,
        pipeline_id = %event.object_attributes.id,
        status = %event.object_attributes.status,
        ref_name = %event.object_attributes.ref_name,
        has_mr = %event.merge_request.is_some(),
        "Received GitLab pipeline webhook"
    );

    if event.object_attributes.status != "failed" {
        return Ok(ignored(format!(
            "Pipeline status is '{}', not 'failed'",
            event.object_attributes.status
        )));
    }
    if !state.is_author_allowed(&event.user.username) {
        debug!(author = %event.user.username, "Author not in allowed list");
        return Ok(ignored(format!(
            "Author '{}' not in allowed list",
            event.user.username
        )));
    }

    let payload = match lookup_pipeline_payload(state, &event).await? {
        Some(p) => p,
        None => {
            return Ok(ignored(format!(
                "No open MR found for branch '{}'",
                event.object_attributes.ref_name
            )));
        }
    };
    let job_id = state.queue.push(payload).await.map_err(AppError::Redis)?;
    info!(job_id = %job_id, "Queued lint-fix job");
    Ok(queued_with_message(job_id, "Lint-fix job queued"))
}

/// Resolve a ReviewPayload for a failed pipeline event.
async fn lookup_pipeline_payload(
    state: &AppState,
    event: &PipelineEvent,
) -> Result<Option<ReviewPayload>, AppError> {
    if event.merge_request.is_some() {
        return Ok(Some(ReviewPayload::from(event)));
    }

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
            Ok(Some(payload))
        }
        Ok(None) => Ok(None),
        Err(e) => {
            warn!(error = %e, "Failed to look up MR by branch");
            Ok(None)
        }
    }
}

async fn handle_note_event(
    state: &AppState,
    body: &[u8],
) -> Result<(StatusCode, Json<WebhookResponse>), AppError> {
    let event: NoteEvent = parse_body(body, "note")?;
    info!(
        project = %event.project.path_with_namespace,
        noteable_type = %event.object_attributes.noteable_type,
        user = %event.user.username,
        "Received GitLab note webhook"
    );

    if let Some(resp) = validate_note_event(&event) {
        return Ok(resp);
    }

    let payload = build_note_payload(state, &event).await?;
    let mr_iid = event.merge_request.as_ref().unwrap().iid;
    let instruction = event.instruction();
    let job_id = state.queue.push(payload).await.map_err(AppError::Redis)?;
    info!(
        job_id = %job_id,
        mr_iid = %mr_iid,
        instruction = %instruction,
        "Queued comment-triggered job"
    );
    Ok(queued_with_message(job_id, "Comment-triggered job queued"))
}

/// Validate a note event, returning an early response if it should be skipped.
fn validate_note_event(event: &NoteEvent) -> Option<(StatusCode, Json<WebhookResponse>)> {
    if event.user.username.contains("_bot_") {
        debug!(user = %event.user.username, "Ignoring note from bot user");
        return Some(ignored("Note from bot user"));
    }
    if !event.is_merge_request_note() {
        debug!("Note is not on a merge request");
        return Some(ignored("Not a merge request note"));
    }
    if !event.mentions_bot() {
        debug!("Note does not mention @claude-agent");
        return Some(ignored("No @claude-agent mention"));
    }
    let mr = event.merge_request.as_ref().unwrap();
    if mr.state != "opened" && mr.state != "reopened" {
        debug!(state = %mr.state, "MR is not open");
        return Some(ignored(format!("MR state is '{}', not open", mr.state)));
    }
    None
}

/// Build a ReviewPayload from a note event by fetching full MR details.
async fn build_note_payload(
    state: &AppState,
    event: &NoteEvent,
) -> Result<ReviewPayload, AppError> {
    let gitlab_url = event
        .project
        .web_url
        .split('/')
        .take(3)
        .collect::<Vec<_>>()
        .join("/");

    let mr = event.merge_request.as_ref().unwrap();
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
    Ok(payload)
}

/// Parse webhook JSON body, logging on failure.
fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8], kind: &str) -> Result<T, AppError> {
    serde_json::from_slice(body).map_err(|e| {
        if let Ok(s) = std::str::from_utf8(body) {
            error!(error = %e, body = %s, "Failed to parse {} webhook", kind);
        }
        AppError::BadRequest(format!("Invalid JSON: {e}"))
    })
}
