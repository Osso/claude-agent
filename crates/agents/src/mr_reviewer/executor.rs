//! Action executor for MR review agent.

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
        Ok(content) => Ok(Observation::FileContent { path: path.into(), content }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(Observation::FileNotFound { path: path.into() })
        }
        Err(e) => Ok(Observation::Error { message: format!("Failed to read file: {e}") }),
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
        info!(body_len = body.len(), "Would post comment (no VCS client configured)");
        Ok(Observation::CommentPosted { comment_id: "mock".into() })
    }

    async fn execute_approve(&self) -> Result<Observation, Error> {
        info!("Would approve MR (no VCS client configured)");
        Ok(Observation::Approved)
    }

    async fn execute_request_changes(&self, reason: &str) -> Result<Observation, Error> {
        info!(reason = %reason, "Would request changes (no VCS client configured)");
        Ok(Observation::ChangesRequested)
    }
}

/// Check if a command is safe to run.
pub(crate) fn is_safe_command(cmd: &str) -> bool {
    let cmd_lower = cmd.to_lowercase();
    is_build_tool(&cmd_lower) || is_read_tool(&cmd_lower) || is_vcs_tool(&cmd_lower)
}

fn is_build_tool(cmd: &str) -> bool {
    let prefixes = [
        "cargo ", "cargo clippy", "cargo test", "cargo check", "cargo fmt",
        "npm ", "yarn ", "pnpm ",
        "phpstan ", "mago lint",
        "eslint ", "prettier ",
        "black ", "ruff ", "mypy ", "pytest ",
        "go test", "go vet", "golangci-lint",
        "php -l", "php --syntax-check",
        "jq ", "sentry ", "jira ",
    ];
    prefixes.iter().any(|p| cmd.starts_with(p))
}

fn is_read_tool(cmd: &str) -> bool {
    let prefixes = ["cat ", "head ", "tail ", "wc ", "grep ", "rg ", "ls ", "find "];
    prefixes.iter().any(|p| cmd.starts_with(p))
}

fn is_vcs_tool(cmd: &str) -> bool {
    let prefixes = [
        "git add ", "git commit ", "git push ",
        "github pr ",
    ];
    prefixes.iter().any(|p| cmd.starts_with(p))
}
