//! GitLab webhook event parsing.

#![allow(dead_code)] // Deserialization structs have unused fields

use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

/// GitLab Merge Request webhook event.
#[derive(Debug, Clone, Deserialize)]
pub struct MergeRequestEvent {
    pub object_kind: String,
    pub event_type: Option<String>,
    pub user: User,
    pub project: Project,
    pub object_attributes: MergeRequestAttributes,
    pub labels: Option<Vec<Label>>,
    pub changes: Option<Changes>,
    pub repository: Option<Repository>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub username: String,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub path_with_namespace: String,
    pub web_url: String,
    pub git_http_url: Option<String>,
    pub git_ssh_url: Option<String>,
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MergeRequestAttributes {
    pub id: i64,
    pub iid: i64,
    pub title: String,
    pub description: Option<String>,
    pub source_branch: String,
    pub target_branch: String,
    pub state: String,
    pub action: Option<String>,
    pub draft: Option<bool>,
    pub work_in_progress: Option<bool>,
    pub url: String,
    pub author_id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    pub id: i64,
    pub title: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Changes {
    pub title: Option<Change>,
    pub description: Option<Change>,
    pub labels: Option<LabelsChange>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Change {
    pub previous: Option<String>,
    pub current: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LabelsChange {
    pub previous: Option<Vec<Label>>,
    pub current: Option<Vec<Label>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Repository {
    pub name: String,
    pub url: String,
    pub homepage: Option<String>,
}

/// GitLab Pipeline webhook event.
#[derive(Debug, Clone, Deserialize)]
pub struct PipelineEvent {
    pub object_kind: String,
    pub object_attributes: PipelineAttributes,
    pub merge_request: Option<PipelineMergeRequest>,
    pub project: Project,
    pub user: User,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PipelineAttributes {
    pub id: i64,
    pub status: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PipelineMergeRequest {
    pub iid: i64,
    pub title: String,
    pub source_branch: String,
    pub target_branch: String,
    pub url: String,
}

impl PipelineEvent {
    /// Check if this pipeline event should trigger a lint-fix job.
    pub fn should_lint_fix(&self) -> bool {
        self.object_kind == "pipeline"
            && self.object_attributes.status == "failed"
            && self.merge_request.is_some()
    }
}

impl From<&PipelineEvent> for ReviewPayload {
    fn from(event: &PipelineEvent) -> Self {
        let gitlab_url = event
            .project
            .web_url
            .split('/')
            .take(3)
            .collect::<Vec<_>>()
            .join("/");

        let mr = event.merge_request.as_ref().unwrap();

        Self {
            gitlab_url,
            project: event.project.path_with_namespace.clone(),
            mr_iid: mr.iid.to_string(),
            clone_url: event
                .project
                .git_http_url
                .clone()
                .unwrap_or_default(),
            source_branch: mr.source_branch.clone(),
            target_branch: mr.target_branch.clone(),
            title: mr.title.clone(),
            description: None,
            author: event.user.username.clone(),
            action: "lint_fix".into(),
            platform: "gitlab".into(),
        }
    }
}

/// Check if a branch exists in a GitLab project.
pub async fn branch_exists(
    gitlab_url: &str,
    project: &str,
    branch: &str,
    token: &str,
) -> Result<bool, anyhow::Error> {
    let headers = gitlab_auth_headers(token)?;
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()?;

    let encoded_project = urlencoding::encode(project);
    let encoded_branch = urlencoding::encode(branch);
    let base_url = gitlab_url.trim_end_matches('/');

    let url = format!("{base_url}/api/v4/projects/{encoded_project}/repository/branches/{encoded_branch}");
    let resp = client.get(&url).send().await?;

    Ok(resp.status().is_success())
}

/// Build auth headers for GitLab API requests.
/// Supports both PAT (PRIVATE-TOKEN) and OAuth (Bearer) tokens.
pub fn gitlab_auth_headers(token: &str) -> Result<HeaderMap, anyhow::Error> {
    let mut headers = HeaderMap::new();
    if token.starts_with("glpat-") || token.len() < 50 {
        headers.insert("PRIVATE-TOKEN", HeaderValue::from_str(token)?);
    } else {
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
    }
    Ok(headers)
}

/// Fetch MR details from GitLab API and build a ReviewPayload.
pub async fn fetch_review_payload(
    gitlab_url: &str,
    project: &str,
    mr_iid: u64,
    token: &str,
) -> Result<ReviewPayload, anyhow::Error> {
    use anyhow::{bail, Context};

    let headers = gitlab_auth_headers(token)?;
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()?;

    let encoded_project = urlencoding::encode(project);
    let base_url = gitlab_url.trim_end_matches('/');

    // Fetch MR details
    let mr_url = format!("{base_url}/api/v4/projects/{encoded_project}/merge_requests/{mr_iid}");
    let mr_resp = client.get(&mr_url).send().await.context("GitLab MR request failed")?;
    if !mr_resp.status().is_success() {
        bail!("GitLab API {} - {}", mr_resp.status(), mr_resp.text().await?);
    }

    #[derive(Deserialize)]
    struct GitLabMr {
        title: String,
        description: Option<String>,
        source_branch: String,
        target_branch: String,
        author: GitLabUser,
    }
    #[derive(Deserialize)]
    struct GitLabUser {
        username: String,
    }

    let mr: GitLabMr = mr_resp.json().await.context("Failed to parse MR")?;

    // Fetch project for clone URL
    let project_url = format!("{base_url}/api/v4/projects/{encoded_project}");
    let proj_resp = client.get(&project_url).send().await.context("GitLab project request failed")?;
    if !proj_resp.status().is_success() {
        bail!("GitLab API {} - {}", proj_resp.status(), proj_resp.text().await?);
    }

    #[derive(Deserialize)]
    struct GitLabProject {
        http_url_to_repo: String,
    }

    let proj: GitLabProject = proj_resp.json().await.context("Failed to parse project")?;

    Ok(ReviewPayload {
        gitlab_url: gitlab_url.to_string(),
        project: project.to_string(),
        mr_iid: mr_iid.to_string(),
        clone_url: proj.http_url_to_repo,
        source_branch: mr.source_branch,
        target_branch: mr.target_branch,
        title: mr.title,
        description: mr.description,
        author: mr.author.username,
        action: "open".into(),
        platform: "gitlab".into(),
    })
}

/// Fetch an open MR by source branch from GitLab API.
/// Returns None if no open MR exists for the branch.
pub async fn fetch_mr_by_branch(
    gitlab_url: &str,
    project: &str,
    source_branch: &str,
    token: &str,
) -> Result<Option<ReviewPayload>, anyhow::Error> {
    use anyhow::Context;

    let headers = gitlab_auth_headers(token)?;
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()?;

    let encoded_project = urlencoding::encode(project);
    let encoded_branch = urlencoding::encode(source_branch);
    let base_url = gitlab_url.trim_end_matches('/');

    // Fetch MRs for this source branch
    let mr_url = format!(
        "{base_url}/api/v4/projects/{encoded_project}/merge_requests?source_branch={encoded_branch}&state=opened"
    );
    let mr_resp = client
        .get(&mr_url)
        .send()
        .await
        .context("GitLab MR list request failed")?;
    if !mr_resp.status().is_success() {
        anyhow::bail!("GitLab API {} - {}", mr_resp.status(), mr_resp.text().await?);
    }

    #[derive(Deserialize)]
    struct GitLabMr {
        iid: u64,
        title: String,
        description: Option<String>,
        source_branch: String,
        target_branch: String,
        author: GitLabUser,
    }
    #[derive(Deserialize)]
    struct GitLabUser {
        username: String,
    }

    let mrs: Vec<GitLabMr> = mr_resp.json().await.context("Failed to parse MR list")?;

    // Return first (most recent) open MR if any
    let Some(mr) = mrs.into_iter().next() else {
        return Ok(None);
    };

    // Fetch project for clone URL
    let project_url = format!("{base_url}/api/v4/projects/{encoded_project}");
    let proj_resp = client
        .get(&project_url)
        .send()
        .await
        .context("GitLab project request failed")?;
    if !proj_resp.status().is_success() {
        anyhow::bail!(
            "GitLab API {} - {}",
            proj_resp.status(),
            proj_resp.text().await?
        );
    }

    #[derive(Deserialize)]
    struct GitLabProject {
        http_url_to_repo: String,
    }

    let proj: GitLabProject = proj_resp.json().await.context("Failed to parse project")?;

    Ok(Some(ReviewPayload {
        gitlab_url: gitlab_url.to_string(),
        project: project.to_string(),
        mr_iid: mr.iid.to_string(),
        clone_url: proj.http_url_to_repo,
        source_branch: mr.source_branch,
        target_branch: mr.target_branch,
        title: mr.title,
        description: mr.description,
        author: mr.author.username,
        action: "lint_fix".into(),
        platform: "gitlab".into(),
    }))
}

impl MergeRequestEvent {
    /// Check if this event should trigger a review.
    pub fn should_review(&self) -> bool {
        // Only review merge requests
        if self.object_kind != "merge_request" {
            return false;
        }

        let attrs = &self.object_attributes;

        // Only review open MRs
        if attrs.state != "opened" && attrs.state != "reopened" {
            return false;
        }

        // Skip drafts/WIP
        if attrs.draft.unwrap_or(false) || attrs.work_in_progress.unwrap_or(false) {
            return false;
        }

        // Review on: open, update, reopen
        match attrs.action.as_deref() {
            Some("open") | Some("update") | Some("reopen") => true,
            _ => false,
        }
    }

    /// Check if MR has a specific label.
    pub fn has_label(&self, label_name: &str) -> bool {
        self.labels
            .as_ref()
            .map(|labels| labels.iter().any(|l| l.title == label_name))
            .unwrap_or(false)
    }

    /// Get the clone URL for the repository.
    pub fn clone_url(&self) -> Option<&str> {
        self.project
            .git_http_url
            .as_deref()
            .or(self.project.git_ssh_url.as_deref())
    }
}

/// Payload to queue for processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewPayload {
    pub gitlab_url: String,
    pub project: String,
    pub mr_iid: String,
    pub clone_url: String,
    pub source_branch: String,
    pub target_branch: String,
    pub title: String,
    pub description: Option<String>,
    pub author: String,
    /// Webhook action: "open", "reopen", "update", etc.
    #[serde(default = "default_action")]
    pub action: String,
    /// Platform: "gitlab" or "github"
    #[serde(default = "default_platform")]
    pub platform: String,
}

fn default_action() -> String {
    "open".into()
}

fn default_platform() -> String {
    "gitlab".into()
}

impl From<&MergeRequestEvent> for ReviewPayload {
    fn from(event: &MergeRequestEvent) -> Self {
        let gitlab_url = event
            .project
            .web_url
            .split('/')
            .take(3)
            .collect::<Vec<_>>()
            .join("/");

        Self {
            gitlab_url,
            project: event.project.path_with_namespace.clone(),
            mr_iid: event.object_attributes.iid.to_string(),
            clone_url: event.clone_url().unwrap_or_default().to_string(),
            source_branch: event.object_attributes.source_branch.clone(),
            target_branch: event.object_attributes.target_branch.clone(),
            title: event.object_attributes.title.clone(),
            description: event.object_attributes.description.clone(),
            author: event.user.username.clone(),
            action: event
                .object_attributes
                .action
                .clone()
                .unwrap_or_else(|| "open".into()),
            platform: "gitlab".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(action: &str, state: &str, draft: bool) -> MergeRequestEvent {
        MergeRequestEvent {
            object_kind: "merge_request".into(),
            event_type: Some("merge_request".into()),
            user: User {
                id: 1,
                name: "Test".into(),
                username: "test".into(),
                email: None,
            },
            project: Project {
                id: 1,
                name: "test".into(),
                path_with_namespace: "group/test".into(),
                web_url: "https://gitlab.com/group/test".into(),
                git_http_url: Some("https://gitlab.com/group/test.git".into()),
                git_ssh_url: None,
                default_branch: Some("main".into()),
            },
            object_attributes: MergeRequestAttributes {
                id: 1,
                iid: 123,
                title: "Test MR".into(),
                description: None,
                source_branch: "feature".into(),
                target_branch: "main".into(),
                state: state.into(),
                action: Some(action.into()),
                draft: Some(draft),
                work_in_progress: None,
                url: "https://gitlab.com/group/test/-/merge_requests/123".into(),
                author_id: 1,
            },
            labels: None,
            changes: None,
            repository: None,
        }
    }

    #[test]
    fn test_should_review_open() {
        let event = make_event("open", "opened", false);
        assert!(event.should_review());
    }

    #[test]
    fn test_should_not_review_draft() {
        let event = make_event("open", "opened", true);
        assert!(!event.should_review());
    }

    #[test]
    fn test_should_not_review_merged() {
        let event = make_event("merge", "merged", false);
        assert!(!event.should_review());
    }

    #[test]
    fn test_payload_from_event() {
        let event = make_event("open", "opened", false);
        let payload = ReviewPayload::from(&event);
        assert_eq!(payload.project, "group/test");
        assert_eq!(payload.mr_iid, "123");
        assert_eq!(payload.gitlab_url, "https://gitlab.com");
    }
}
