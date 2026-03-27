//! Worker binary for ephemeral K8s jobs.
//!
//! This is the entry point for review jobs spawned by the scheduler.
//! It receives job context via environment variable, clones the repo,
//! and runs the Claude agent which posts its review or fix.

use std::env;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use base64::Engine;
use tracing::{info, warn, Level};
use tracing_subscriber::FmtSubscriber;

use claude_agent_agents::{JiraHandlerAgent, JiraTicketContext, MrReviewAgent, SentryFixContext, SentryFixerAgent};
use claude_agent_core::ReviewContext;
use claude_agent_server::sentry_api::{extract_tags, format_stacktrace, SentryClient};
use claude_agent_server::{JiraTicketPayload, JobPayload, SentryFixPayload};

const VERSION: &str = "2026.02.12.1";

fn main() -> Result<()> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .json()
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!(version = VERSION, "Claude Agent Worker starting");

    let payload = decode_payload()?;

    match payload {
        JobPayload::Review(review) => run_review_job(review),
        JobPayload::SentryFix(sentry) => run_sentry_fix_job(sentry),
        JobPayload::JiraTicket(jira) => run_jira_ticket_job(jira),
    }
}

/// Inject GitHub access token into a git HTTPS URL.
fn inject_github_credentials(url: &str, token: &str) -> String {
    if let Some(rest) = url.strip_prefix("https://") {
        format!("https://x-access-token:{token}@{rest}")
    } else {
        url.to_string()
    }
}

fn decode_payload() -> Result<JobPayload> {
    let payload_b64 = env::var("REVIEW_PAYLOAD").context("REVIEW_PAYLOAD not set")?;
    let payload_bytes = base64::engine::general_purpose::STANDARD
        .decode(&payload_b64)
        .context("Failed to decode base64 payload")?;

    if let Ok(payload) = serde_json::from_slice::<JobPayload>(&payload_bytes) {
        return Ok(payload);
    }

    let legacy: claude_agent_server::ReviewPayload =
        serde_json::from_slice(&payload_bytes).context("Failed to parse payload JSON")?;
    warn!("Parsed legacy ReviewPayload format (missing 'type' tag)");
    Ok(JobPayload::Review(legacy))
}

/// Clone repo and return (diff, changed_files, optional SHAs).
fn clone_and_get_diff(
    payload: &claude_agent_server::ReviewPayload,
    token: &str,
    work_dir: &PathBuf,
) -> Result<(String, Vec<String>, Option<(String, String, String)>)> {
    let auth_clone_url = inject_github_credentials(&payload.clone_url, token);
    clone_repo(&auth_clone_url, &payload.source_branch, &payload.target_branch, work_dir)?;

    let diff = get_diff(work_dir, &payload.target_branch)?;
    let changed_files = get_changed_files(work_dir, &payload.target_branch)?;

    let shas = match get_diff_shas(work_dir, &payload.target_branch) {
        Ok(shas) => Some(shas),
        Err(e) => {
            warn!(error = %e, "Failed to compute diff SHAs, inline comments will not work");
            None
        }
    };

    Ok((diff, changed_files, shas))
}

/// Build ReviewContext from payload and computed diff data.
fn build_review_context(
    payload: &claude_agent_server::ReviewPayload,
    diff: String,
    changed_files: Vec<String>,
    shas: Option<(String, String, String)>,
) -> ReviewContext {
    let (base_sha, head_sha, start_sha) = match shas {
        Some((b, h, s)) => (Some(b), Some(h), Some(s)),
        None => (None, None, None),
    };
    ReviewContext {
        project: payload.project.clone(),
        mr_id: payload.mr_iid.clone(),
        source_branch: payload.source_branch.clone(),
        target_branch: payload.target_branch.clone(),
        diff,
        changed_files,
        title: payload.title.clone(),
        description: payload.description.clone(),
        author: payload.author.clone(),
        base_sha,
        head_sha,
        start_sha,
    }
}

/// Build the review prompt based on action type.
fn build_review_prompt(
    payload: &claude_agent_server::ReviewPayload,
    agent: &MrReviewAgent,
    token: &str,
) -> Result<String> {
    if payload.action == "comment" {
        let instruction = payload.trigger_comment.as_deref().unwrap_or("review this");
        info!(instruction = %instruction, "Building comment-triggered prompt");
        Ok(agent.build_comment_prompt(instruction, None))
    } else if payload.action == "lint_fix" {
        info!("Building lint-fix prompt");
        Ok(agent.build_lint_fix_prompt())
    } else if payload.action == "update" {
        let discussions = fetch_github_review_comments(payload, token)?;
        Ok(agent.build_github_update_prompt(&discussions))
    } else {
        Ok(agent.build_github_prompt())
    }
}

/// Run a review job (PR review or lint-fix).
fn run_review_job(payload: claude_agent_server::ReviewPayload) -> Result<()> {
    let token = env::var("GITHUB_TOKEN").context("GITHUB_TOKEN not set")?;

    info!(
        project = %payload.project,
        mr_iid = %payload.mr_iid,
        platform = %payload.platform,
        "Processing review"
    );

    let work_dir = PathBuf::from("/work/repo");
    std::fs::create_dir_all(&work_dir)?;

    let (diff, changed_files, shas) = clone_and_get_diff(&payload, &token, &work_dir)?;
    let context = build_review_context(&payload, diff, changed_files, shas);

    let agent = MrReviewAgent::new(context, &work_dir);
    let prompt = build_review_prompt(&payload, &agent, &token)?;

    info!(action = %payload.action, platform = %payload.platform, "Running Claude");
    run_claude(&work_dir, &prompt)?;

    info!("Review completed");
    Ok(())
}

/// Fetch Sentry issue details (stacktrace, tags, title, culprit, platform).
fn fetch_sentry_details(
    payload: &SentryFixPayload,
    sentry_token: &str,
) -> Result<(String, Vec<(String, String)>, String, String, String)> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let client = SentryClient::new(&payload.organization, sentry_token)?;

        let event = client.get_issue_latest_event(&payload.issue_id).await?;
        let stacktrace = format_stacktrace(&event);
        let tags = extract_tags(&event);

        let issue = client.get_issue(&payload.issue_id).await?;
        let title = issue["title"].as_str().unwrap_or(&payload.title).to_string();
        let culprit = issue["culprit"].as_str().unwrap_or(&payload.culprit).to_string();
        let platform = issue["platform"].as_str().unwrap_or(&payload.platform).to_string();

        Ok::<_, anyhow::Error>((stacktrace, tags, title, culprit, platform))
    })
}

/// Clone repo and run Claude for a Sentry fix.
fn clone_and_run_sentry_fix(payload: &SentryFixPayload, context: SentryFixContext) -> Result<()> {
    let work_dir = PathBuf::from("/work/repo");
    std::fs::create_dir_all(&work_dir)?;

    let token = env::var("GITHUB_TOKEN").context("GITHUB_TOKEN not set")?;
    let auth_clone_url = inject_github_credentials(&payload.clone_url, &token);
    clone_branch(&auth_clone_url, &payload.target_branch, &work_dir)?;

    let agent = SentryFixerAgent::new(context, &work_dir);
    let prompt = agent.build_prompt();

    info!(short_id = %payload.short_id, "Running Claude for Sentry fix");
    run_claude(&work_dir, &prompt)
}

/// Run a Sentry fix job.
fn run_sentry_fix_job(payload: SentryFixPayload) -> Result<()> {
    info!(
        short_id = %payload.short_id,
        project = %payload.project_slug,
        vcs_project = %payload.vcs_project,
        "Processing Sentry fix"
    );

    let sentry_token = env::var("SENTRY_AUTH_TOKEN").context("SENTRY_AUTH_TOKEN not set")?;
    let (stacktrace, tags, title, culprit, platform) = fetch_sentry_details(&payload, &sentry_token)?;
    info!(stacktrace_len = stacktrace.len(), tags_count = tags.len(), "Fetched Sentry issue details");

    let context = SentryFixContext {
        short_id: payload.short_id.clone(),
        title,
        culprit,
        platform,
        web_url: payload.web_url.clone(),
        stacktrace,
        tags,
        vcs_project: payload.vcs_project.clone(),
        target_branch: payload.target_branch.clone(),
        vcs_platform: payload.vcs_platform.clone(),
    };

    clone_and_run_sentry_fix(&payload, context)?;
    info!("Sentry fix completed");
    Ok(())
}

/// Build JiraTicketContext from payload.
fn build_jira_context(payload: &JiraTicketPayload) -> JiraTicketContext {
    JiraTicketContext {
        issue_key: payload.issue_key.clone(),
        summary: payload.summary.clone(),
        description: payload.description.clone(),
        issue_type: payload.issue_type.clone(),
        priority: payload.priority.clone(),
        status: payload.status.clone(),
        labels: payload.labels.clone(),
        web_url: payload.web_url.clone(),
        trigger_comment: payload.trigger_comment.clone(),
        trigger_author: payload.trigger_author.clone(),
        vcs_project: payload.vcs_project.clone(),
        target_branch: payload.target_branch.clone(),
        vcs_platform: payload.vcs_platform.clone(),
    }
}

/// Run a Jira ticket fix job.
fn run_jira_ticket_job(payload: JiraTicketPayload) -> Result<()> {
    info!(
        issue_key = %payload.issue_key,
        summary = %payload.summary,
        vcs_project = %payload.vcs_project,
        "Processing Jira ticket"
    );

    let work_dir = PathBuf::from("/work/repo");
    std::fs::create_dir_all(&work_dir)?;

    let token = env::var("GITHUB_TOKEN").context("GITHUB_TOKEN not set")?;
    let auth_clone_url = inject_github_credentials(&payload.clone_url, &token);
    clone_branch(&auth_clone_url, &payload.target_branch, &work_dir)?;

    let context = build_jira_context(&payload);
    let agent = JiraHandlerAgent::new(context, &work_dir);
    let prompt = agent.build_prompt();

    info!(issue_key = %payload.issue_key, "Running Claude for Jira ticket");
    run_claude(&work_dir, &prompt)?;

    info!("Jira ticket fix completed");
    Ok(())
}

/// Run Claude Code with tools enabled. Claude will post the review itself.
fn run_claude(work_dir: &PathBuf, prompt: &str) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new("claude")
        .arg("-p")
        .arg("--dangerously-skip-permissions")
        .current_dir(work_dir)
        .stdin(Stdio::piped())
        .spawn()
        .context("Failed to spawn claude")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .context("Failed to write prompt to stdin")?;
    }

    let status = child.wait().context("Failed to wait for claude")?;

    if !status.success() {
        bail!("Claude exited with status {}", status);
    }

    Ok(())
}

fn clone_repo(clone_url: &str, branch: &str, target_branch: &str, target: &PathBuf) -> Result<()> {
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

/// Clone a repository at a specific branch (for Sentry/Jira fix jobs).
fn clone_branch(clone_url: &str, branch: &str, target: &PathBuf) -> Result<()> {
    info!(branch = %branch, "Cloning repository");

    let status = Command::new("git")
        .args(["clone", "--depth", "50", "--branch", branch, clone_url])
        .arg(target)
        .status()
        .context("Failed to run git clone")?;

    if !status.success() {
        bail!("git clone failed with status {}", status);
    }

    Ok(())
}

fn run_git(repo_dir: &PathBuf, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_dir)
        .output()
        .with_context(|| format!("Failed to run git {}", args.first().unwrap_or(&"")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.first().unwrap_or(&""), stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn get_diff_shas(repo_dir: &PathBuf, target_branch: &str) -> Result<(String, String, String)> {
    let start_sha = run_git(
        repo_dir,
        &["merge-base", &format!("origin/{target_branch}"), "HEAD"],
    )?;
    let head_sha = run_git(repo_dir, &["rev-parse", "HEAD"])?;
    Ok((start_sha.clone(), head_sha, start_sha))
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
        .args(["diff", "--name-only", &format!("origin/{target_branch}...HEAD")])
        .current_dir(repo_dir)
        .output()
        .context("Failed to run git diff --name-only")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git diff --name-only failed: {}", stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(String::from)
        .collect())
}

/// Format GitHub review comments into text for the prompt.
fn format_github_comments(comments: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for comment in comments {
        let id = comment["id"].as_u64().unwrap_or(0);
        let path = comment["path"].as_str().unwrap_or("?");
        let line = comment["line"]
            .as_u64()
            .or_else(|| comment["original_line"].as_u64())
            .map(|l| l.to_string())
            .unwrap_or_default();
        let author = comment["user"]["login"].as_str().unwrap_or("?");
        let body = comment["body"].as_str().unwrap_or("");
        out.push_str(&format!("### Comment {id} ({path}:{line})\n\n"));
        out.push_str(&format!("**@{author}**: {body}\n\n"));
    }
    out
}

/// Fetch review comments from GitHub API for update reviews.
fn fetch_github_review_comments(
    payload: &claude_agent_server::ReviewPayload,
    token: &str,
) -> Result<String> {
    let repo = &payload.project;
    let number = &payload.mr_iid;
    let url = format!("https://api.github.com/repos/{repo}/pulls/{number}/comments?per_page=100");

    let client = reqwest::blocking::Client::builder()
        .user_agent("claude-agent-worker")
        .build()?;

    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .context("Failed to fetch GitHub review comments")?;

    if !resp.status().is_success() {
        bail!("GitHub API {} - {}", resp.status(), resp.text().unwrap_or_default());
    }

    let comments: Vec<serde_json::Value> = resp.json().context("Failed to parse comments")?;
    Ok(format_github_comments(&comments))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inject_github_credentials() {
        let url = "https://github.com/owner/repo.git";
        let token = "ghs_xxx";
        let result = inject_github_credentials(url, token);
        assert_eq!(result, "https://x-access-token:ghs_xxx@github.com/owner/repo.git");
    }

    #[test]
    fn test_inject_github_credentials_non_https() {
        let url = "git@github.com:owner/repo.git";
        let token = "ghs_xxx";
        let result = inject_github_credentials(url, token);
        assert_eq!(result, "git@github.com:owner/repo.git");
    }
}
