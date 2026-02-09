//! API endpoints for CLI access (stats, review, fix).

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use tracing::{info, warn};

use crate::gitlab::fetch_review_payload;
use crate::jira;
use crate::payload::{JiraTicketPayload, SentryFixPayload};

use super::github::fetch_github_pr_payload;
use super::{branch_exists_on_platform, AppError, AppState};

// -- Queue management --

pub(super) async fn queue_stats_handler(
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
    let failed = state.queue.failed_count().await.map_err(AppError::Redis)?;
    Ok(Json(
        serde_json::json!({ "pending": pending, "processing": processing, "failed": failed }),
    ))
}

pub(super) async fn list_failed_handler(
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

pub(super) async fn retry_handler(
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

// -- Review endpoints --

#[derive(Deserialize)]
pub(super) struct QueueReviewRequest {
    project: String,
    mr_iid: u64,
    #[serde(default = "default_gitlab_url")]
    gitlab_url: String,
    #[serde(default)]
    action: Option<String>,
}

fn default_gitlab_url() -> String {
    "https://gitlab.com".into()
}

pub(super) async fn queue_review_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<QueueReviewRequest>,
) -> Result<impl IntoResponse, AppError> {
    if !state.verify_api_key(&headers) {
        warn!("Invalid API key for /api/review");
        return Err(AppError::Unauthorized);
    }

    let mut payload =
        fetch_review_payload(&req.gitlab_url, &req.project, req.mr_iid, &state.gitlab_token)
            .await
            .map_err(|e| AppError::Internal(format!("Failed to fetch MR from GitLab: {e}")))?;

    if let Some(action) = &req.action {
        payload.action = action.clone();
    }

    let job_id = state.queue.push(payload).await.map_err(AppError::Redis)?;
    info!(job_id = %job_id, project = %req.project, mr_iid = %req.mr_iid, "Queued review via API");
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "status": "queued", "job_id": job_id })),
    ))
}

#[derive(Deserialize)]
pub(super) struct QueueGithubReviewRequest {
    repo: String,
    pr: u64,
    #[serde(default)]
    action: Option<String>,
}

pub(super) async fn queue_github_review_handler(
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
    info!(job_id = %job_id, repo = %req.repo, pr = %req.pr, "Queued GitHub review via API");
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "status": "queued", "job_id": job_id })),
    ))
}

// -- Sentry fix endpoint --

#[derive(Deserialize)]
pub(super) struct QueueSentryFixRequest {
    organization: String,
    project: String,
    issue_id: String,
}

pub(super) async fn queue_sentry_fix_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<QueueSentryFixRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    if !state.verify_api_key(&headers) {
        warn!("Invalid API key for /api/sentry-fix");
        return Err(AppError::Unauthorized);
    }

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

    let (issue, short_id) = fetch_sentry_issue_details(&state, &req).await?;

    let branch_name = format!("sentry-fix/{}", short_id.to_lowercase());
    if branch_exists_on_platform(&state, &mapping.vcs_platform, &mapping.vcs_project, &branch_name)
        .await?
    {
        info!(branch = %branch_name, issue = %short_id, "Fix branch already exists, skipping");
        return Ok((
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "skipped",
                "message": format!("Branch {} already exists", branch_name),
            })),
        ));
    }

    let payload = build_sentry_api_payload(&req, &issue, &short_id, mapping);
    let job_id = state.queue.push(payload).await.map_err(AppError::Redis)?;
    info!(job_id = %job_id, org = %req.organization, project = %req.project, issue = %short_id, "Queued Sentry fix via API");
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "status": "queued", "job_id": job_id })),
    ))
}

async fn fetch_sentry_issue_details(
    state: &AppState,
    req: &QueueSentryFixRequest,
) -> Result<(serde_json::Value, String), AppError> {
    let token = state
        .sentry_auth_token
        .as_ref()
        .ok_or_else(|| AppError::Internal("SENTRY_AUTH_TOKEN not configured".into()))?;
    let client = crate::sentry_api::SentryClient::new(&req.organization, token)
        .map_err(|e| AppError::Internal(format!("Failed to create Sentry client: {e}")))?;
    let issue = client
        .get_issue(&req.issue_id)
        .await
        .map_err(|e| AppError::Internal(format!("Failed to fetch Sentry issue: {e}")))?;
    let short_id = issue["shortId"]
        .as_str()
        .unwrap_or(&req.issue_id)
        .to_string();
    Ok((issue, short_id))
}

fn build_sentry_api_payload(
    req: &QueueSentryFixRequest,
    issue: &serde_json::Value,
    short_id: &str,
    mapping: &crate::sentry::SentryProjectMapping,
) -> SentryFixPayload {
    SentryFixPayload {
        issue_id: req.issue_id.clone(),
        short_id: short_id.to_string(),
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
    }
}

// -- Jira fix endpoint --

#[derive(Deserialize)]
pub(super) struct QueueJiraFixRequest {
    issue_key: String,
    #[serde(default = "default_jira_url")]
    jira_url: String,
}

fn default_jira_url() -> String {
    "https://globalcomix.atlassian.net".into()
}

pub(super) async fn queue_jira_fix_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<QueueJiraFixRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    if !state.verify_api_key(&headers) {
        warn!("Invalid API key for /api/jira-fix");
        return Err(AppError::Unauthorized);
    }

    let project_key = req
        .issue_key
        .split('-')
        .next()
        .ok_or_else(|| AppError::BadRequest("Invalid issue key format".into()))?;
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

    let branch_name = format!("jira-fix/{}", req.issue_key.to_lowercase());
    if branch_exists_on_platform(&state, &mapping.vcs_platform, &mapping.vcs_project, &branch_name)
        .await?
    {
        info!(branch = %branch_name, issue = %req.issue_key, "Fix branch already exists, skipping");
        return Ok((
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "skipped",
                "message": format!("Branch {} already exists", branch_name),
            })),
        ));
    }

    let issue = fetch_jira_issue(&state, &req).await?;
    let payload = build_jira_api_payload(&req, &issue, mapping);
    let job_id = state.queue.push(payload).await.map_err(AppError::Redis)?;
    info!(job_id = %job_id, issue = %req.issue_key, "Queued Jira fix via API");
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "status": "queued", "job_id": job_id })),
    ))
}

async fn fetch_jira_issue(
    state: &AppState,
    req: &QueueJiraFixRequest,
) -> Result<serde_json::Value, AppError> {
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
    let url = format!(
        "{}/rest/api/3/issue/{}",
        req.jira_url.trim_end_matches('/'),
        req.issue_key
    );
    client
        .get(&url)
        .header("Authorization", format!("Bearer {}", jira_token))
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to fetch Jira issue: {e}")))?
        .error_for_status()
        .map_err(|e| AppError::Internal(format!("Jira API error: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("Failed to parse Jira response: {e}")))
}

fn build_jira_api_payload(
    req: &QueueJiraFixRequest,
    issue: &serde_json::Value,
    mapping: &crate::jira::JiraProjectMapping,
) -> JiraTicketPayload {
    let fields = &issue["fields"];
    let description = fields
        .get("description")
        .map(jira::extract_text_from_adf);

    JiraTicketPayload {
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
        web_url: format!(
            "{}/browse/{}",
            req.jira_url.trim_end_matches('/'),
            req.issue_key
        ),
        jira_base_url: req.jira_url.clone(),
        trigger_comment: "Triggered via API".into(),
        trigger_author: None,
        clone_url: mapping.clone_url.clone(),
        target_branch: mapping.target_branch.clone(),
        vcs_platform: mapping.vcs_platform.clone(),
        vcs_project: mapping.vcs_project.clone(),
    }
}
