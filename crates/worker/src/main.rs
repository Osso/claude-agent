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
use tracing::{info, warn, Level};
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
    /// Webhook action: "open", "reopen", "update", etc.
    #[serde(default = "default_action")]
    action: String,
    /// Platform: "gitlab" or "github"
    #[serde(default = "default_platform")]
    platform: String,
}

fn default_action() -> String {
    "open".into()
}

fn default_platform() -> String {
    "gitlab".into()
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
    let is_github = payload.platform == "github";

    let token = if is_github {
        env::var("GITHUB_TOKEN").context("GITHUB_TOKEN not set")?
    } else {
        env::var("GITLAB_TOKEN").context("GITLAB_TOKEN not set")?
    };

    info!(
        project = %payload.project,
        mr_iid = %payload.mr_iid,
        platform = %payload.platform,
        "Processing review"
    );

    let work_dir = PathBuf::from("/work/repo");
    std::fs::create_dir_all(&work_dir)?;

    let auth_clone_url = if is_github {
        inject_github_credentials(&payload.clone_url, &token)
    } else {
        inject_git_credentials(&payload.clone_url, &token)
    };

    clone_repo(
        &auth_clone_url,
        &payload.source_branch,
        &payload.target_branch,
        &work_dir,
    )?;

    let diff = get_diff(&work_dir, &payload.target_branch)?;
    let changed_files = get_changed_files(&work_dir, &payload.target_branch)?;

    let (base_sha, head_sha, start_sha) = match get_diff_shas(&work_dir, &payload.target_branch) {
        Ok(shas) => (Some(shas.0), Some(shas.1), Some(shas.2)),
        Err(e) => {
            warn!(error = %e, "Failed to compute diff SHAs, inline comments will not work");
            (None, None, None)
        }
    };

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
        base_sha,
        head_sha,
        start_sha,
    };

    let changed_files_ref = context.changed_files.clone();
    let agent = MrReviewAgent::new(context, &work_dir);

    let prompt = if payload.action == "lint_fix" {
        let linter_output = run_linters(&work_dir, &changed_files_ref)?;
        if linter_output.trim().is_empty() {
            info!("Linters produced no output, skipping lint-fix");
            return Ok(());
        }
        info!(output_len = linter_output.len(), "Collected linter output");
        agent.build_lint_fix_prompt(&linter_output)
    } else if is_github {
        if payload.action == "update" {
            let discussions = fetch_github_review_comments(&payload, &token)?;
            agent.build_github_update_prompt(&discussions)
        } else {
            agent.build_github_prompt()
        }
    } else if payload.action == "update" {
        let discussions = fetch_unresolved_discussions(&payload, &token)?;
        info!(
            threads = discussions.len(),
            "Fetched unresolved discussion threads"
        );
        let formatted = format_discussions(&discussions);
        agent.build_update_prompt(&formatted)
    } else {
        agent.build_prompt()
    };

    info!(action = %payload.action, platform = %payload.platform, "Running Claude");
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

/// Inject GitHub access token into a git HTTPS URL.
fn inject_github_credentials(url: &str, token: &str) -> String {
    if let Some(rest) = url.strip_prefix("https://") {
        format!("https://x-access-token:{token}@{rest}")
    } else {
        url.to_string()
    }
}

/// Inject OAuth2 credentials into a git HTTPS URL (GitLab).
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
    // base_sha == start_sha for standard MRs
    Ok((start_sha.clone(), head_sha, start_sha))
}

/// Detect file types from changed files and run relevant linters.
fn run_linters(repo_dir: &PathBuf, changed_files: &[String]) -> Result<String> {
    let mut output = String::new();

    let has_ext = |ext: &str| changed_files.iter().any(|f| f.ends_with(ext));

    // PHP: phpstan + mago
    if has_ext(".php") {
        if let Ok(result) = run_linter(repo_dir, "phpstan", &["analyse", "--no-progress", "--error-format=raw"]) {
            if !result.is_empty() {
                output.push_str("### phpstan\n");
                output.push_str(&result);
                output.push('\n');
            }
        }
        if let Ok(result) = run_linter(repo_dir, "mago", &["lint"]) {
            if !result.is_empty() {
                output.push_str("### mago\n");
                output.push_str(&result);
                output.push('\n');
            }
        }
    }

    // Rust: cargo clippy
    if has_ext(".rs") {
        if let Ok(result) = run_linter(repo_dir, "cargo", &["clippy", "--workspace", "--message-format=short", "--", "-D", "warnings"]) {
            if !result.is_empty() {
                output.push_str("### cargo clippy\n");
                output.push_str(&result);
                output.push('\n');
            }
        }
    }

    // JavaScript/TypeScript: eslint
    if has_ext(".js") || has_ext(".ts") || has_ext(".jsx") || has_ext(".tsx") {
        if let Ok(result) = run_linter(repo_dir, "eslint", &["."]) {
            if !result.is_empty() {
                output.push_str("### eslint\n");
                output.push_str(&result);
                output.push('\n');
            }
        }
    }

    // Python: ruff
    if has_ext(".py") {
        if let Ok(result) = run_linter(repo_dir, "ruff", &["check", "."]) {
            if !result.is_empty() {
                output.push_str("### ruff\n");
                output.push_str(&result);
                output.push('\n');
            }
        }
    }

    // Go: golangci-lint
    if has_ext(".go") {
        if let Ok(result) = run_linter(repo_dir, "golangci-lint", &["run"]) {
            if !result.is_empty() {
                output.push_str("### golangci-lint\n");
                output.push_str(&result);
                output.push('\n');
            }
        }
    }

    Ok(output)
}

/// Run a linter command and return its combined stdout+stderr output.
/// Returns Ok("") if the linter is not found (not installed).
fn run_linter(repo_dir: &PathBuf, cmd: &str, args: &[&str]) -> Result<String> {
    let output = match Command::new(cmd)
        .args(args)
        .current_dir(repo_dir)
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!(cmd = cmd, "Linter not found, skipping");
            return Ok(String::new());
        }
        Err(e) => return Err(e.into()),
    };

    let mut result = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        result.push_str(&stderr);
    }

    // Linters exit non-zero when they find issues — that's expected
    Ok(result)
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

/// Fetch unresolved discussion threads from GitLab API.
fn fetch_unresolved_discussions(
    payload: &ReviewPayload,
    token: &str,
) -> Result<Vec<serde_json::Value>> {
    let encoded_project = urlencoding::encode(&payload.project);
    let gitlab_url = &payload.gitlab_url;
    let iid = &payload.mr_iid;
    let url = format!(
        "{gitlab_url}/api/v4/projects/{encoded_project}/merge_requests/{iid}/discussions?per_page=100"
    );

    let headers = claude_agent_server::gitlab::gitlab_auth_headers(token)?;
    let client = reqwest::blocking::Client::builder()
        .default_headers(headers)
        .build()?;

    let resp = client.get(&url).send().context("Failed to fetch discussions")?;
    if !resp.status().is_success() {
        bail!(
            "GitLab discussions API {} - {}",
            resp.status(),
            resp.text().unwrap_or_default()
        );
    }

    let discussions: Vec<serde_json::Value> = resp.json().context("Failed to parse discussions")?;

    // Filter to unresolved threads only
    let unresolved = discussions
        .into_iter()
        .filter(|d| {
            d["notes"]
                .as_array()
                .map(|notes| {
                    notes.iter().any(|n| {
                        n["resolvable"].as_bool().unwrap_or(false)
                            && !n["resolved"].as_bool().unwrap_or(true)
                    })
                })
                .unwrap_or(false)
        })
        .collect();

    Ok(unresolved)
}

/// Format discussions into text for the prompt.
fn format_discussions(discussions: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for d in discussions {
        let disc_id = d["id"].as_str().unwrap_or("?");
        let notes = match d["notes"].as_array() {
            Some(n) => n,
            None => continue,
        };
        let first = match notes.first() {
            Some(n) => n,
            None => continue,
        };

        // File position
        if let Some(pos) = first["position"].as_object() {
            let path = pos
                .get("new_path")
                .or(pos.get("old_path"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let line = pos
                .get("new_line")
                .or(pos.get("old_line"))
                .and_then(|v| v.as_u64())
                .map(|l| l.to_string())
                .unwrap_or_default();
            out.push_str(&format!("### Thread {disc_id} ({path}:{line})\n\n"));
        } else {
            out.push_str(&format!("### Thread {disc_id}\n\n"));
        }

        for note in notes {
            let author = note["author"]["username"].as_str().unwrap_or("?");
            let body = note["body"].as_str().unwrap_or("");
            out.push_str(&format!("**@{author}**: {body}\n\n"));
        }
    }
    out
}

/// Fetch review comments from GitHub API for update reviews.
fn fetch_github_review_comments(payload: &ReviewPayload, token: &str) -> Result<String> {
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
        bail!(
            "GitHub API {} - {}",
            resp.status(),
            resp.text().unwrap_or_default()
        );
    }

    let comments: Vec<serde_json::Value> = resp.json().context("Failed to parse comments")?;
    let mut out = String::new();

    for comment in &comments {
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

    Ok(out)
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
    fn test_inject_github_credentials() {
        let url = "https://github.com/owner/repo.git";
        let token = "ghs_xxx";
        let result = inject_github_credentials(url, token);
        assert_eq!(
            result,
            "https://x-access-token:ghs_xxx@github.com/owner/repo.git"
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
