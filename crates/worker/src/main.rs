//! Worker binary for ephemeral K8s jobs.
//!
//! This is the entry point for review jobs spawned by the scheduler.
//! It receives review context via environment variable, clones the repo,
//! and runs the Claude agent which posts its review via the gitlab CLI.

use std::env;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use base64::Engine;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

use claude_agent_agents::MrReviewAgent;
use claude_agent_core::ReviewContext;

/// Payload received from the scheduler.
#[derive(Debug, serde::Deserialize)]
struct ReviewPayload {
    /// GitLab base URL (e.g., "https://gitlab.com")
    #[allow(dead_code)]
    gitlab_url: String,
    /// Project path or ID
    project: String,
    /// Merge request IID
    mr_iid: String,
    /// Clone URL for the repository
    clone_url: String,
    /// Source branch to checkout
    source_branch: String,
    /// Target branch for comparison
    target_branch: String,
    /// MR title
    title: String,
    /// MR description
    description: Option<String>,
    /// Author username
    author: String,
}

fn main() -> Result<()> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .json()
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("Claude Agent Worker starting");

    let payload = decode_payload()?;
    let gitlab_token = env::var("GITLAB_TOKEN").context("GITLAB_TOKEN not set")?;

    info!(
        project = %payload.project,
        mr_iid = %payload.mr_iid,
        "Processing review"
    );

    let work_dir = PathBuf::from("/work/repo");
    std::fs::create_dir_all(&work_dir)?;

    let auth_clone_url = inject_git_credentials(&payload.clone_url, &gitlab_token);
    clone_repo(
        &auth_clone_url,
        &payload.source_branch,
        &payload.target_branch,
        &work_dir,
    )?;

    let diff = get_diff(&work_dir, &payload.target_branch)?;
    let changed_files = get_changed_files(&work_dir, &payload.target_branch)?;

    let context = ReviewContext {
        project: payload.project.clone(),
        mr_id: payload.mr_iid.clone(),
        source_branch: payload.source_branch.clone(),
        target_branch: payload.target_branch.clone(),
        diff,
        changed_files,
        title: payload.title.clone(),
        description: payload.description.clone(),
        author: payload.author.clone(),
    };

    let agent = MrReviewAgent::new(context, &work_dir);
    let prompt = agent.build_prompt();

    info!("Running Claude review");
    run_claude(&work_dir, &prompt)?;

    info!("Review completed");
    Ok(())
}

fn decode_payload() -> Result<ReviewPayload> {
    let payload_b64 = env::var("REVIEW_PAYLOAD").context("REVIEW_PAYLOAD not set")?;
    let payload_bytes = base64::engine::general_purpose::STANDARD
        .decode(&payload_b64)
        .context("Failed to decode base64 payload")?;
    serde_json::from_slice(&payload_bytes).context("Failed to parse payload JSON")
}

/// Run Claude Code with tools enabled. Claude will post the review itself.
fn run_claude(work_dir: &PathBuf, prompt: &str) -> Result<()> {
    let status = Command::new("claude")
        .arg("-p")
        .arg(prompt)
        .arg("--dangerously-skip-permissions")
        .current_dir(work_dir)
        .status()
        .context("Failed to run claude")?;

    if !status.success() {
        bail!("Claude exited with status {}", status);
    }

    Ok(())
}

/// Inject OAuth2 credentials into a git HTTPS URL.
fn inject_git_credentials(url: &str, token: &str) -> String {
    if let Some(rest) = url.strip_prefix("https://") {
        format!("https://oauth2:{token}@{rest}")
    } else {
        url.to_string()
    }
}

fn clone_repo(
    clone_url: &str,
    branch: &str,
    target_branch: &str,
    target: &PathBuf,
) -> Result<()> {
    info!(branch = %branch, "Cloning repository");

    let status = Command::new("git")
        .args(["clone", "--depth", "50", "--branch", branch, clone_url])
        .arg(target)
        .status()
        .context("Failed to run git clone")?;

    if !status.success() {
        bail!("git clone failed with status {}", status);
    }

    let refspec = format!("{target_branch}:refs/remotes/origin/{target_branch}");
    let status = Command::new("git")
        .args(["fetch", "origin", &refspec])
        .current_dir(target)
        .status()
        .context("Failed to fetch target branch")?;

    if !status.success() {
        bail!("git fetch failed with status {}", status);
    }

    Ok(())
}

fn get_diff(repo_dir: &PathBuf, target_branch: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["diff", &format!("origin/{target_branch}...HEAD")])
        .current_dir(repo_dir)
        .output()
        .context("Failed to run git diff")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git diff failed: {}", stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn get_changed_files(repo_dir: &PathBuf, target_branch: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args([
            "diff",
            "--name-only",
            &format!("origin/{target_branch}...HEAD"),
        ])
        .current_dir(repo_dir)
        .output()
        .context("Failed to run git diff --name-only")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git diff --name-only failed: {}", stderr);
    }

    let files = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(String::from)
        .collect();

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inject_git_credentials() {
        let url = "https://gitlab.com/group/repo.git";
        let token = "test-token";
        let result = inject_git_credentials(url, token);
        assert_eq!(
            result,
            "https://oauth2:test-token@gitlab.com/group/repo.git"
        );
    }

    #[test]
    fn test_inject_git_credentials_with_path() {
        let url = "https://gitlab.com/Globalcomix/gc.git";
        let token = "glpat-xxx";
        let result = inject_git_credentials(url, token);
        assert_eq!(
            result,
            "https://oauth2:glpat-xxx@gitlab.com/Globalcomix/gc.git"
        );
    }

    #[test]
    fn test_inject_git_credentials_non_https() {
        let url = "git@gitlab.com:group/repo.git";
        let token = "test-token";
        let result = inject_git_credentials(url, token);
        assert_eq!(result, "git@gitlab.com:group/repo.git");
    }
}
