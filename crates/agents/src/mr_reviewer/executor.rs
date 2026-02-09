//! Action executor and GitLab client for MR review agent.

use std::process::Command;

use async_trait::async_trait;
use tracing::{debug, info, warn};

use claude_agent_core::{Action, ActionExecutor, Error, Observation};

use super::MrReviewAgent;

#[async_trait]
impl ActionExecutor for MrReviewAgent {
    async fn execute(&self, action: &Action) -> Result<Observation, Error> {
        match action {
            Action::ReadFile { path } => execute_read_file(&self.repo_path, path),
            Action::RunCommand { cmd } => execute_command(&self.repo_path, cmd),
            Action::PostComment { body } => self.execute_post_comment(body).await,
            Action::Approve => self.execute_approve().await,
            Action::RequestChanges { reason } => self.execute_request_changes(reason).await,
            Action::Finish { .. } => Ok(Observation::Error {
                message: "Finish should be handled by controller".into(),
            }),
        }
    }
}

fn execute_read_file(repo_path: &std::path::Path, path: &str) -> Result<Observation, Error> {
    let full_path = repo_path.join(path);
    debug!(path = %full_path.display(), "Reading file");

    match std::fs::read_to_string(&full_path) {
        Ok(content) => Ok(Observation::FileContent {
            path: path.into(),
            content,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(Observation::FileNotFound { path: path.into() })
        }
        Err(e) => Ok(Observation::Error {
            message: format!("Failed to read file: {e}"),
        }),
    }
}

fn execute_command(repo_path: &std::path::Path, cmd: &str) -> Result<Observation, Error> {
    info!(cmd = %cmd, "Running command");

    if !is_safe_command(cmd) {
        warn!(cmd = %cmd, "Blocked unsafe command");
        return Ok(Observation::Error {
            message: "Command not allowed for security reasons".into(),
        });
    }

    let output = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(repo_path)
        .output()
        .map_err(Error::Io)?;

    Ok(Observation::CommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(-1),
    })
}

impl MrReviewAgent {
    async fn execute_post_comment(&self, body: &str) -> Result<Observation, Error> {
        if let Some(client) = &self.gitlab_client {
            match client.post_mr_note(&self.context.mr_id, body).await {
                Ok(note_id) => Ok(Observation::CommentPosted {
                    comment_id: note_id,
                }),
                Err(e) => Ok(Observation::Error {
                    message: format!("Failed to post comment: {e}"),
                }),
            }
        } else {
            info!(body_len = body.len(), "Would post comment (no GitLab client)");
            Ok(Observation::CommentPosted {
                comment_id: "mock".into(),
            })
        }
    }

    async fn execute_approve(&self) -> Result<Observation, Error> {
        if let Some(client) = &self.gitlab_client {
            match client.approve_mr(&self.context.mr_id).await {
                Ok(()) => Ok(Observation::Approved),
                Err(e) => Ok(Observation::Error {
                    message: format!("Failed to approve: {e}"),
                }),
            }
        } else {
            info!("Would approve MR (no GitLab client)");
            Ok(Observation::Approved)
        }
    }

    async fn execute_request_changes(&self, reason: &str) -> Result<Observation, Error> {
        if let Some(client) = &self.gitlab_client {
            match client.post_mr_note(&self.context.mr_id, reason).await {
                Ok(_) => Ok(Observation::ChangesRequested),
                Err(e) => Ok(Observation::Error {
                    message: format!("Failed to request changes: {e}"),
                }),
            }
        } else {
            info!(reason = %reason, "Would request changes (no GitLab client)");
            Ok(Observation::ChangesRequested)
        }
    }
}

/// Check if a command is safe to run.
pub(crate) fn is_safe_command(cmd: &str) -> bool {
    let allowed_prefixes = [
        "cargo ",
        "cargo clippy",
        "cargo test",
        "cargo check",
        "cargo fmt",
        "npm ",
        "yarn ",
        "pnpm ",
        "phpstan ",
        "mago lint",
        "eslint ",
        "prettier ",
        "black ",
        "ruff ",
        "mypy ",
        "pytest ",
        "go test",
        "go vet",
        "golangci-lint",
        "cat ",
        "head ",
        "tail ",
        "wc ",
        "grep ",
        "rg ",
        "ls ",
        "find ",
        "php -l",
        "php --syntax-check",
        "mago lint",
        "jq ",
        "github pr ",
        "gitlab mr ",
        "gitlab ci ",
        "sentry ",
        "jira ",
        // Git write commands (for lint-fix jobs)
        "git add ",
        "git commit ",
        "git push ",
    ];

    let cmd_lower = cmd.to_lowercase();
    allowed_prefixes.iter().any(|p| cmd_lower.starts_with(p))
}

/// GitLab API client for MR operations.
pub struct GitLabClient {
    client: reqwest::Client,
    base_url: String,
    project_id: String,
    token: String,
}

impl GitLabClient {
    pub fn new(
        base_url: impl Into<String>,
        project_id: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        let project_id: String = project_id.into();
        let encoded_project = project_id.replace('/', "%2F");
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            project_id: encoded_project,
            token: token.into(),
        }
    }

    /// Post a note (comment) on a merge request.
    pub async fn post_mr_note(&self, mr_iid: &str, body: &str) -> Result<String, Error> {
        let url = format!(
            "{}/api/v4/projects/{}/merge_requests/{}/notes",
            self.base_url, self.project_id, mr_iid
        );

        let resp = self
            .client
            .post(&url)
            .header("PRIVATE-TOKEN", &self.token)
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await
            .map_err(|e| Error::ClaudeApi(format!("HTTP error: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::ClaudeApi(format!(
                "GitLab API error: {} - {}",
                status, text
            )));
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::ClaudeApi(format!("JSON error: {e}")))?;

        let note_id = json["id"]
            .as_i64()
            .map(|id| id.to_string())
            .unwrap_or_else(|| "unknown".into());

        Ok(note_id)
    }

    /// Approve a merge request.
    pub async fn approve_mr(&self, mr_iid: &str) -> Result<(), Error> {
        let url = format!(
            "{}/api/v4/projects/{}/merge_requests/{}/approve",
            self.base_url, self.project_id, mr_iid
        );

        let resp = self
            .client
            .post(&url)
            .header("PRIVATE-TOKEN", &self.token)
            .send()
            .await
            .map_err(|e| Error::ClaudeApi(format!("HTTP error: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::ClaudeApi(format!(
                "GitLab API error: {} - {}",
                status, text
            )));
        }

        Ok(())
    }
}
