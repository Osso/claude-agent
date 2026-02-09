//! GitHub webhook handler.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use tracing::{debug, error, info, warn};

use crate::github::{verify_signature, PullRequestEvent};
use crate::gitlab::ReviewPayload;

use super::{ignored, queued, AppError, AppState, WebhookResponse};

/// GitHub webhook handler for pull_request events.
pub(super) async fn github_webhook_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<WebhookResponse>), AppError> {
    let signature = headers
        .get("X-Hub-Signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !verify_signature(&state.webhook_secret, &body, signature) {
        warn!("Invalid GitHub webhook signature");
        return Err(AppError::Unauthorized);
    }

    let event_type = headers
        .get("X-GitHub-Event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if event_type != "pull_request" {
        return Ok(ignored(format!("Unsupported event type: {event_type}")));
    }

    if let Ok(s) = std::str::from_utf8(&body) {
        debug!(body = %s, "Raw GitHub webhook body");
    }

    let event: PullRequestEvent = serde_json::from_slice(&body).map_err(|e| {
        if let Ok(s) = std::str::from_utf8(&body) {
            error!(error = %e, body = %s, "Failed to parse GitHub webhook body");
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
        return Ok(ignored("Event does not require review"));
    }
    let payload = ReviewPayload::from(&event);
    let job_id = state.queue.push(payload).await.map_err(AppError::Redis)?;
    info!(job_id = %job_id, "Queued GitHub review job");
    Ok(queued(job_id))
}

/// Fetch PR details from GitHub API and build a ReviewPayload.
pub(crate) async fn fetch_github_pr_payload(
    repo: &str,
    pr: u64,
    token: &str,
) -> anyhow::Result<ReviewPayload> {
    let client = reqwest::Client::new();
    let url = format!("https://api.github.com/repos/{}/pulls/{}", repo, pr);
    let resp: serde_json::Value = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .header("User-Agent", "claude-agent")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    Ok(ReviewPayload {
        project: repo.to_string(),
        mr_iid: pr.to_string(),
        title: resp["title"].as_str().unwrap_or("").to_string(),
        description: resp["body"].as_str().map(|s| s.to_string()),
        source_branch: resp["head"]["ref"].as_str().unwrap_or("").to_string(),
        target_branch: resp["base"]["ref"].as_str().unwrap_or("").to_string(),
        author: resp["user"]["login"].as_str().unwrap_or("").to_string(),
        clone_url: resp["head"]["repo"]["clone_url"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        action: "open".to_string(),
        gitlab_url: String::new(),
        platform: "github".to_string(),
        trigger_comment: None,
    })
}
