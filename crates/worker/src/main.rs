//! Worker binary for ephemeral K8s jobs.
//!
//! This is the entry point for review jobs spawned by the scheduler.
//! It receives review context via environment variable, clones the repo,
//! runs the Claude agent, and posts the review.

use std::env;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use base64::Engine;
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;

use claude_agent_agents::{GitLabClient, MrReviewAgent};
use claude_agent_claude::ClaudeProcess;
use claude_agent_core::{AgentController, ReviewContext, State};

/// Payload received from the scheduler.
#[derive(Debug, serde::Deserialize)]
struct ReviewPayload {
    /// GitLab base URL (e.g., "https://gitlab.com")
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

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .json()
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("Claude Agent Worker starting");

    // Get payload from environment
    let payload_b64 = env::var("REVIEW_PAYLOAD").context("REVIEW_PAYLOAD not set")?;
    let payload_bytes = base64::engine::general_purpose::STANDARD
        .decode(&payload_b64)
        .context("Failed to decode base64 payload")?;
    let payload: ReviewPayload =
        serde_json::from_slice(&payload_bytes).context("Failed to parse payload JSON")?;

    info!(
        project = %payload.project,
        mr_iid = %payload.mr_iid,
        "Processing review"
    );

    // Get tokens from environment
    let gitlab_token = env::var("GITLAB_TOKEN").context("GITLAB_TOKEN not set")?;

    // Setup work directory
    let work_dir = PathBuf::from("/work/repo");
    std::fs::create_dir_all(&work_dir)?;

    // Clone repository
    clone_repo(&payload.clone_url, &payload.source_branch, &work_dir)?;

    // Get diff
    let diff = get_diff(&work_dir, &payload.target_branch)?;
    let changed_files = get_changed_files(&work_dir, &payload.target_branch)?;

    // Build review context
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

    // Create GitLab client
    let gitlab_client = GitLabClient::new(&payload.gitlab_url, &payload.project, &gitlab_token);

    // Create agent
    let agent = MrReviewAgent::new(context.clone(), &work_dir).with_gitlab(gitlab_client);
    let initial_prompt = agent.build_prompt();

    // Spawn Claude process
    let claude = ClaudeProcess::spawn(&work_dir)?;

    // Create controller
    let state = State::with_context(context);
    let mut controller = AgentController::new(claude, agent, "").with_state(state);

    // Run the review
    match controller.run(&initial_prompt).await {
        Ok(result) => {
            info!(
                decision = ?result.decision,
                issues = result.issues.len(),
                "Review completed"
            );
            println!(
                "{}",
                serde_json::to_string_pretty(&result).unwrap_or_default()
            );
        }
        Err(e) => {
            error!(error = %e, "Review failed");
            bail!("Review failed: {e}");
        }
    }

    Ok(())
}

fn clone_repo(clone_url: &str, branch: &str, target: &PathBuf) -> Result<()> {
    info!(url = %clone_url, branch = %branch, "Cloning repository");

    let status = Command::new("git")
        .args(["clone", "--depth", "50", "--branch", branch, clone_url])
        .arg(target)
        .status()
        .context("Failed to run git clone")?;

    if !status.success() {
        bail!("git clone failed with status {}", status);
    }

    // Fetch target branch for diff comparison
    let status = Command::new("git")
        .args(["fetch", "origin", "HEAD"])
        .current_dir(target)
        .status()
        .context("Failed to fetch origin")?;

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
