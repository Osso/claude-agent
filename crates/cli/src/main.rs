//! Claude Agent CLI
//!
//! CLI for managing the review queue and testing.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use futures_util::io::AsyncBufReadExt;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::api::{Api, ListParams, LogParams};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Deserialize;
use tracing::Level;
use tracing_subscriber::FmtSubscriber;

use claude_agent_server::FailedItem;

const NAMESPACE: &str = "claude-agent";

/// Config file structure (~/.config/claude-agent/config.toml)
#[derive(Debug, Default, Deserialize)]
struct Config {
    server_url: Option<String>,
    api_key: Option<String>,
}

impl Config {
    fn load() -> Self {
        let path = Self::config_path();
        if path.exists() {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| toml::from_str(&s).ok())
                .unwrap_or_default()
        } else {
            Self::default()
        }
    }

    fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("claude-agent")
            .join("config.toml")
    }
}

#[derive(Parser)]
#[command(name = "claude-agent")]
#[command(about = "Claude Agent CLI for PR review management")]
struct Cli {
    #[arg(long, env = "CLAUDE_AGENT_URL")]
    server_url: Option<String>,

    #[arg(long, env = "CLAUDE_AGENT_API_KEY")]
    api_key: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Trigger a review for a GitHub PR
    ReviewGithub {
        #[arg(long, short)]
        repo: String,
        #[arg(long, short)]
        pr: u64,
    },

    /// Trigger a Sentry fix job
    SentryFix {
        #[arg(long, short)]
        org: String,
        #[arg(long, short)]
        project: String,
        #[arg(long, short)]
        issue: String,
    },

    /// Trigger a Jira ticket fix job
    JiraFix {
        #[arg(long, short)]
        issue: String,
        #[arg(long, default_value = "https://globalcomix.atlassian.net")]
        jira_url: String,
    },

    /// Show queue statistics
    Stats,

    /// List failed items in the queue
    ListFailed {
        #[arg(long, default_value = "10")]
        limit: usize,
    },

    /// Retry a failed item
    Retry { id: String },

    /// Show logs from a running or completed review job
    Logs {
        job: Option<String>,
        #[arg(long, short)]
        follow: bool,
        #[arg(long, short = 'n', default_value = "100")]
        tail: i64,
    },

    /// List review jobs in Kubernetes
    Jobs {
        #[arg(long, short)]
        all: bool,
    },

    /// Check if server's configured tokens are valid
    CheckTokens,
}

#[tokio::main]
async fn main() -> Result<()> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let cli = Cli::parse();
    let config = Config::load();
    let server_url = cli.server_url.or(config.server_url);
    let api_key = cli.api_key.or(config.api_key);

    if handle_local_command(&cli.command).await? {
        return Ok(());
    }

    let server_url = server_url.context(
        "Server URL required. Set in ~/.config/claude-agent/config.toml or CLAUDE_AGENT_URL",
    )?;
    let api_key = api_key.context(
        "API key required. Set in ~/.config/claude-agent/config.toml or CLAUDE_AGENT_API_KEY",
    )?;

    handle_server_command(cli.command, &server_url, &api_key).await
}

/// Handle commands that don't need the server. Returns true if handled.
async fn handle_local_command(command: &Commands) -> Result<bool> {
    match command {
        Commands::Logs { job, follow, tail } => {
            show_logs(job.as_deref(), *follow, *tail).await?;
            Ok(true)
        }
        Commands::Jobs { all } => {
            list_jobs(*all).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Handle commands that require server URL and API key.
async fn handle_server_command(command: Commands, server_url: &str, api_key: &str) -> Result<()> {
    match command {
        Commands::Logs { .. } | Commands::Jobs { .. } => unreachable!(),

        Commands::ReviewGithub { repo, pr } => {
            let id = api_queue_github_review(server_url, api_key, &repo, pr, None).await?;
            println!("Queued review for #{} in {}", pr, repo);
            println!("Job ID: {id}");
        }

        Commands::SentryFix {
            org,
            project,
            issue,
        } => {
            handle_sentry_fix(server_url, api_key, &org, &project, &issue).await?;
        }

        Commands::JiraFix { issue, jira_url } => {
            handle_jira_fix(server_url, api_key, &issue, &jira_url).await?;
        }

        Commands::Stats => api_stats(server_url, api_key).await?,
        Commands::ListFailed { limit } => api_list_failed(server_url, api_key, limit).await?,
        Commands::Retry { id } => api_retry(server_url, api_key, &id).await?,
        Commands::CheckTokens => api_check_tokens(server_url, api_key).await?,
    }

    Ok(())
}

async fn handle_sentry_fix(
    server_url: &str,
    api_key: &str,
    org: &str,
    project: &str,
    issue: &str,
) -> Result<()> {
    match api_queue_sentry_fix(server_url, api_key, org, project, issue).await? {
        SentryFixResult::Queued(id) => {
            println!("Queued Sentry fix for {} in {}/{}", issue, org, project);
            println!("Job ID: {id}");
        }
        SentryFixResult::Skipped(msg) => println!("Skipped: {msg}"),
    }
    Ok(())
}

async fn handle_jira_fix(
    server_url: &str,
    api_key: &str,
    issue: &str,
    jira_url: &str,
) -> Result<()> {
    match api_queue_jira_fix(server_url, api_key, issue, jira_url).await? {
        JiraFixResult::Queued(id) => {
            println!("Queued Jira fix for {}", issue);
            println!("Job ID: {id}");
        }
        JiraFixResult::Skipped(msg) => println!("Skipped: {msg}"),
    }
    Ok(())
}

fn print_failed_item(item: &FailedItem) {
    use claude_agent_server::JobPayload;

    println!("  ID:       {}", item.item.id);
    println!("  Job:      {}", item.item.payload.description());

    match &item.item.payload {
        JobPayload::Review(p) => {
            println!("  Project:  {}", p.project);
            println!("  MR:       !{}", p.mr_iid);
            println!("  Title:    {}", p.title);
        }
        JobPayload::SentryFix(p) => {
            println!("  Project:  {}", p.vcs_project);
            println!("  Issue:    {}", p.short_id);
            println!("  Title:    {}", p.title);
        }
        JobPayload::JiraTicket(p) => {
            println!("  Project:  {}", p.vcs_project);
            println!("  Issue:    {}", p.issue_key);
            println!("  Summary:  {}", p.summary);
        }
    }

    println!("  Attempts: {}", item.item.attempts);
    println!("  Error:    {}", item.error);
    println!("  Failed:   {}", item.failed_at);
}

/// Print a single job line.
fn print_job_line(job: &Job, show_all: bool) {
    let name = job.metadata.name.as_deref().unwrap_or("unknown");
    let status = job.status.as_ref();

    let state = if status.and_then(|s| s.succeeded).unwrap_or(0) > 0 {
        "succeeded"
    } else if status.and_then(|s| s.failed).unwrap_or(0) > 0 {
        "failed"
    } else if status.and_then(|s| s.active).unwrap_or(0) > 0 {
        "running"
    } else {
        "pending"
    };

    if !show_all && (state == "succeeded" || state == "failed") {
        return;
    }

    let queue_id = job
        .metadata
        .labels
        .as_ref()
        .and_then(|l| l.get("queue-id"))
        .map(|s| s.as_str())
        .unwrap_or("-");

    println!("  {name}  [{state}]  queue-id={queue_id}");
}

async fn list_jobs(show_all: bool) -> Result<()> {
    let client = Client::try_default()
        .await
        .context("Failed to create Kubernetes client")?;
    let jobs: Api<Job> = Api::namespaced(client, NAMESPACE);
    let lp = ListParams::default().labels("app=claude-review");
    let job_list = jobs.list(&lp).await.context("Failed to list jobs")?;

    if job_list.items.is_empty() {
        println!("No review jobs found");
        return Ok(());
    }

    println!("Review Jobs:");
    for job in &job_list.items {
        print_job_line(job, show_all);
    }

    Ok(())
}

async fn show_logs(job_filter: Option<&str>, follow: bool, tail: i64) -> Result<()> {
    let client = Client::try_default()
        .await
        .context("Failed to create Kubernetes client")?;
    let jobs: Api<Job> = Api::namespaced(client.clone(), NAMESPACE);
    let pods: Api<Pod> = Api::namespaced(client, NAMESPACE);

    let job_name = resolve_job_name(&jobs, job_filter).await?;
    println!("Fetching logs for job: {job_name}");

    let lp = ListParams::default().labels(&format!("job-name={job_name}"));
    let pod_list = pods.list(&lp).await.context("Failed to list pods")?;

    let pod_name = pod_list
        .items
        .first()
        .and_then(|p| p.metadata.name.clone())
        .context("No pod found for job")?;

    stream_or_fetch_logs(&pods, &pod_name, follow, tail).await
}

async fn resolve_job_name(jobs: &Api<Job>, job_filter: Option<&str>) -> Result<String> {
    if let Some(filter) = job_filter {
        if filter.starts_with("claude-review-") {
            return Ok(filter.to_string());
        }
        let lp = ListParams::default().labels("app=claude-review");
        let job_list = jobs.list(&lp).await.context("Failed to list jobs")?;
        return job_list
            .items
            .into_iter()
            .find(|j| {
                j.metadata
                    .labels
                    .as_ref()
                    .and_then(|l| l.get("queue-id"))
                    .is_some_and(|id| id.starts_with(filter))
            })
            .and_then(|j| j.metadata.name)
            .context(format!("No job found matching '{filter}'"));
    }

    let lp = ListParams::default().labels("app=claude-review");
    let job_list = jobs.list(&lp).await.context("Failed to list jobs")?;

    let running = job_list
        .items
        .iter()
        .find(|j| j.status.as_ref().and_then(|s| s.active).unwrap_or(0) > 0);

    running
        .or(job_list.items.last())
        .and_then(|j| j.metadata.name.clone())
        .context("No review jobs found")
}

async fn stream_or_fetch_logs(
    pods: &Api<Pod>,
    pod_name: &str,
    follow: bool,
    tail: i64,
) -> Result<()> {
    let mut lp = LogParams {
        tail_lines: Some(tail),
        follow,
        ..Default::default()
    };

    if follow {
        let stream = pods
            .log_stream(pod_name, &lp)
            .await
            .context("Failed to get log stream")?;
        let mut lines = stream.lines();
        while let Some(line) = lines.next().await {
            println!("{}", line?);
        }
    } else {
        lp.follow = false;
        let logs = pods
            .logs(pod_name, &lp)
            .await
            .context("Failed to get logs")?;
        print!("{logs}");
    }

    Ok(())
}

// HTTP API client for server communication

#[derive(Deserialize)]
struct ApiStats {
    pending: u64,
    processing: u64,
    failed: u64,
}

fn create_api_client(api_key: &str) -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {api_key}"))?,
    );
    Ok(reqwest::Client::builder()
        .default_headers(headers)
        .build()?)
}

async fn api_stats(server_url: &str, api_key: &str) -> Result<()> {
    let client = create_api_client(api_key)?;
    let url = format!("{}/api/stats", server_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .send()
        .await
        .context("Failed to fetch stats")?;

    if !resp.status().is_success() {
        bail!("API error: {} - {}", resp.status(), resp.text().await?);
    }

    let stats: ApiStats = resp
        .json()
        .await
        .context("Failed to parse stats response")?;
    println!("Queue Statistics:");
    println!("  Pending:    {}", stats.pending);
    println!("  Processing: {}", stats.processing);
    println!("  Failed:     {}", stats.failed);

    Ok(())
}

async fn api_list_failed(server_url: &str, api_key: &str, limit: usize) -> Result<()> {
    let client = create_api_client(api_key)?;
    let url = format!("{}/api/failed", server_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .send()
        .await
        .context("Failed to fetch failed items")?;

    if !resp.status().is_success() {
        bail!("API error: {} - {}", resp.status(), resp.text().await?);
    }

    let items: Vec<FailedItem> = resp
        .json()
        .await
        .context("Failed to parse failed items response")?;

    if items.is_empty() {
        println!("No failed items");
    } else {
        println!("Failed Items:");
        for item in items.into_iter().take(limit) {
            println!();
            print_failed_item(&item);
        }
    }

    Ok(())
}

async fn api_retry(server_url: &str, api_key: &str, id: &str) -> Result<()> {
    let client = create_api_client(api_key)?;
    let url = format!("{}/api/retry/{}", server_url.trim_end_matches('/'), id);
    let resp = client
        .post(&url)
        .send()
        .await
        .context("Failed to retry item")?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        println!("Job not found in failed list: {id}");
    } else if !resp.status().is_success() {
        bail!("API error: {} - {}", resp.status(), resp.text().await?);
    } else {
        println!("Retried job: {id}");
    }

    Ok(())
}

async fn api_queue_github_review(
    server_url: &str,
    api_key: &str,
    repo: &str,
    pr: u64,
    action: Option<&str>,
) -> Result<String> {
    let client = create_api_client(api_key)?;
    let url = format!("{}/api/review/github", server_url.trim_end_matches('/'));

    let mut body = serde_json::json!({ "repo": repo, "pr": pr });
    if let Some(action) = action {
        body["action"] = serde_json::json!(action);
    }

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("Failed to queue GitHub review")?;

    if !resp.status().is_success() {
        bail!("API error: {} - {}", resp.status(), resp.text().await?);
    }

    #[derive(Deserialize)]
    struct QueueResponse {
        job_id: String,
    }

    let result: QueueResponse = resp
        .json()
        .await
        .context("Failed to parse queue response")?;
    Ok(result.job_id)
}

enum SentryFixResult {
    Queued(String),
    Skipped(String),
}

async fn api_queue_sentry_fix(
    server_url: &str,
    api_key: &str,
    org: &str,
    project: &str,
    issue_id: &str,
) -> Result<SentryFixResult> {
    let client = create_api_client(api_key)?;
    let url = format!("{}/api/sentry-fix", server_url.trim_end_matches('/'));
    let body = serde_json::json!({ "organization": org, "project": project, "issue_id": issue_id });

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("Failed to queue Sentry fix")?;

    if !resp.status().is_success() {
        bail!("API error: {} - {}", resp.status(), resp.text().await?);
    }

    let result: serde_json::Value = resp
        .json()
        .await
        .context("Failed to parse queue response")?;

    if result["status"].as_str().unwrap_or("") == "skipped" {
        Ok(SentryFixResult::Skipped(
            result["message"]
                .as_str()
                .unwrap_or("Already exists")
                .to_string(),
        ))
    } else {
        Ok(SentryFixResult::Queued(
            result["job_id"]
                .as_str()
                .context("Missing job_id")?
                .to_string(),
        ))
    }
}

enum JiraFixResult {
    Queued(String),
    Skipped(String),
}

async fn api_queue_jira_fix(
    server_url: &str,
    api_key: &str,
    issue_key: &str,
    jira_url: &str,
) -> Result<JiraFixResult> {
    let client = create_api_client(api_key)?;
    let url = format!("{}/api/jira-fix", server_url.trim_end_matches('/'));
    let body = serde_json::json!({ "issue_key": issue_key, "jira_url": jira_url });

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("Failed to queue Jira fix")?;

    if !resp.status().is_success() {
        bail!("API error: {} - {}", resp.status(), resp.text().await?);
    }

    let result: serde_json::Value = resp
        .json()
        .await
        .context("Failed to parse queue response")?;

    if result["status"].as_str().unwrap_or("") == "skipped" {
        Ok(JiraFixResult::Skipped(
            result["message"]
                .as_str()
                .unwrap_or("Already exists")
                .to_string(),
        ))
    } else {
        Ok(JiraFixResult::Queued(
            result["job_id"]
                .as_str()
                .context("Missing job_id")?
                .to_string(),
        ))
    }
}

#[derive(Deserialize)]
struct TokenStatus {
    configured: bool,
    valid: bool,
    info: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct CheckTokensResponse {
    github: TokenStatus,
    sentry: TokenStatus,
    claude: TokenStatus,
    jira: TokenStatus,
}

fn print_token_status(name: &str, status: &TokenStatus, all_valid: &mut bool) {
    print!("{name}");
    if !status.configured {
        println!("- not configured");
    } else if status.valid {
        println!("✓ valid ({})", status.info.as_deref().unwrap_or(""));
    } else {
        println!(
            "✗ invalid - {}",
            status.error.as_deref().unwrap_or("unknown")
        );
        *all_valid = false;
    }
}

async fn api_check_tokens(server_url: &str, api_key: &str) -> Result<()> {
    let client = create_api_client(api_key)?;
    let url = format!("{}/api/check-tokens", server_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .send()
        .await
        .context("Failed to check tokens")?;

    if !resp.status().is_success() {
        bail!("API error: {} - {}", resp.status(), resp.text().await?);
    }

    let result: CheckTokensResponse = resp
        .json()
        .await
        .context("Failed to parse check-tokens response")?;

    let mut all_valid = true;
    print_token_status("GitHub:  ", &result.github, &mut all_valid);
    print_token_status("Sentry:  ", &result.sentry, &mut all_valid);
    print_token_status("Claude:  ", &result.claude, &mut all_valid);
    print_token_status("Jira:    ", &result.jira, &mut all_valid);

    if !all_valid {
        bail!("One or more tokens are invalid");
    }

    Ok(())
}
