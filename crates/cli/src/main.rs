//! Claude Agent CLI
//!
//! CLI for managing the review queue and testing.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use futures_util::io::AsyncBufReadExt;
use futures_util::StreamExt;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, LogParams};
use kube::Client;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Deserialize;
use tracing::Level;
use tracing_subscriber::FmtSubscriber;

use claude_agent_server::FailedItem;

const NAMESPACE: &str = "claude-agent";

/// Config file structure (~/.config/claude-agent/config.toml)
#[derive(Debug, Default, Deserialize)]
struct Config {
    /// Server URL for HTTP API access
    server_url: Option<String>,
    /// API key for authentication
    api_key: Option<String>,
}

impl Config {
    /// Load config from ~/.config/claude-agent/config.toml
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

    /// Get config file path
    fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("claude-agent")
            .join("config.toml")
    }
}

#[derive(Parser)]
#[command(name = "claude-agent")]
#[command(about = "Claude Agent CLI for MR review management")]
struct Cli {
    /// Server URL for HTTP API
    #[arg(long, env = "CLAUDE_AGENT_URL")]
    server_url: Option<String>,

    /// API key for server authentication
    #[arg(long, env = "CLAUDE_AGENT_API_KEY")]
    api_key: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Fetch and display MR info
    Info {
        /// Project path (e.g., Globalcomix/gc)
        #[arg(long, short)]
        project: String,

        /// Merge request IID
        #[arg(long, short)]
        mr: u64,

        /// GitLab URL (defaults to gitlab.com)
        #[arg(long, default_value = "https://gitlab.com")]
        gitlab_url: String,

        /// GitLab token (defaults to GITLAB_TOKEN env var)
        #[arg(long, env = "GITLAB_TOKEN")]
        token: String,
    },

    /// Trigger a review for an MR
    Review {
        /// Project path (e.g., Globalcomix/gc)
        #[arg(long, short)]
        project: String,

        /// Merge request IID
        #[arg(long, short)]
        mr: u64,

        /// GitLab URL (defaults to gitlab.com)
        #[arg(long, default_value = "https://gitlab.com")]
        gitlab_url: String,
    },

    /// Show queue statistics
    Stats,

    /// List failed items in the queue
    ListFailed {
        /// Maximum number of items to show
        #[arg(long, default_value = "10")]
        limit: usize,
    },

    /// Retry a failed item
    Retry {
        /// Job ID to retry
        id: String,
    },

    /// Show logs from a running or completed review job
    Logs {
        /// Job ID (first 8 chars of queue ID) or full job name
        job: Option<String>,

        /// Follow log output (like tail -f)
        #[arg(long, short)]
        follow: bool,

        /// Number of lines to show from the end
        #[arg(long, short = 'n', default_value = "100")]
        tail: i64,
    },

    /// List review jobs in Kubernetes
    Jobs {
        /// Show all jobs (including completed)
        #[arg(long, short)]
        all: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let cli = Cli::parse();

    // Load config file and merge with CLI args (CLI args take precedence)
    let config = Config::load();
    let server_url = cli.server_url.or(config.server_url);
    let api_key = cli.api_key.or(config.api_key);

    // Handle commands that don't need the server
    match &cli.command {
        Commands::Info {
            project,
            mr,
            gitlab_url,
            token,
        } => {
            let mr_info = fetch_mr_info(gitlab_url, project, *mr, token).await?;
            println!("MR Info:");
            println!("  Title:         {}", mr_info.title);
            println!("  Author:        {}", mr_info.author);
            println!("  Source:        {}", mr_info.source_branch);
            println!("  Target:        {}", mr_info.target_branch);
            println!("  Clone URL:     {}", mr_info.clone_url);
            if let Some(desc) = &mr_info.description {
                println!("  Description:   {}", desc.lines().next().unwrap_or(""));
            }
            return Ok(());
        }
        Commands::Logs { job, follow, tail } => {
            show_logs(job.as_deref(), *follow, *tail).await?;
            return Ok(());
        }
        Commands::Jobs { all } => {
            list_jobs(*all).await?;
            return Ok(());
        }
        _ => {}
    }

    // All other commands require server URL and API key
    let server_url = server_url.context(
        "Server URL required. Set in ~/.config/claude-agent/config.toml or CLAUDE_AGENT_URL",
    )?;
    let api_key = api_key.context(
        "API key required. Set in ~/.config/claude-agent/config.toml or CLAUDE_AGENT_API_KEY",
    )?;

    match cli.command {
        Commands::Info { .. } | Commands::Logs { .. } | Commands::Jobs { .. } => {
            unreachable!() // Handled above
        }

        Commands::Review {
            project,
            mr,
            gitlab_url,
        } => {
            let id = api_queue_review(&server_url, &api_key, &project, mr, &gitlab_url).await?;
            println!("Queued review for !{} in {}", mr, project);
            println!("Job ID: {id}");
        }

        Commands::Stats => {
            api_stats(&server_url, &api_key).await?;
        }

        Commands::ListFailed { limit } => {
            api_list_failed(&server_url, &api_key, limit).await?;
        }

        Commands::Retry { id } => {
            api_retry(&server_url, &api_key, &id).await?;
        }
    }

    Ok(())
}

fn print_failed_item(item: &FailedItem) {
    println!("  ID:       {}", item.item.id);
    println!("  Project:  {}", item.item.payload.project);
    println!("  MR:       !{}", item.item.payload.mr_iid);
    println!("  Title:    {}", item.item.payload.title);
    println!("  Attempts: {}", item.item.attempts);
    println!("  Error:    {}", item.error);
    println!("  Failed:   {}", item.failed_at);
}

#[derive(Debug)]
struct MrInfo {
    title: String,
    description: Option<String>,
    source_branch: String,
    target_branch: String,
    author: String,
    clone_url: String,
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

#[derive(Deserialize)]
struct GitLabProject {
    http_url_to_repo: String,
}

async fn fetch_mr_info(
    gitlab_url: &str,
    project: &str,
    mr_iid: u64,
    token: &str,
) -> Result<MrInfo> {
    let headers = claude_agent_server::gitlab_auth_headers(token)?;
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()?;

    let encoded_project = urlencoding::encode(project);
    let base_url = gitlab_url.trim_end_matches('/');

    // Fetch MR details
    let mr_url = format!(
        "{}/api/v4/projects/{}/merge_requests/{}",
        base_url, encoded_project, mr_iid
    );

    let mr_resp = client
        .get(&mr_url)
        .send()
        .await
        .context("Failed to fetch MR")?;

    if !mr_resp.status().is_success() {
        bail!(
            "GitLab API error: {} - {}",
            mr_resp.status(),
            mr_resp.text().await?
        );
    }

    let mr: GitLabMr = mr_resp.json().await.context("Failed to parse MR response")?;

    // Fetch project to get clone URL
    let project_url = format!("{}/api/v4/projects/{}", base_url, encoded_project);

    let project_resp = client
        .get(&project_url)
        .send()
        .await
        .context("Failed to fetch project")?;

    if !project_resp.status().is_success() {
        bail!(
            "GitLab API error: {} - {}",
            project_resp.status(),
            project_resp.text().await?
        );
    }

    let project_info: GitLabProject = project_resp
        .json()
        .await
        .context("Failed to parse project response")?;

    Ok(MrInfo {
        title: mr.title,
        description: mr.description,
        source_branch: mr.source_branch,
        target_branch: mr.target_branch,
        author: mr.author.username,
        clone_url: project_info.http_url_to_repo,
    })
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
    for job in job_list.items {
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

        // Skip completed jobs unless --all
        if !show_all && (state == "succeeded" || state == "failed") {
            continue;
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

    Ok(())
}

async fn show_logs(job_filter: Option<&str>, follow: bool, tail: i64) -> Result<()> {
    let client = Client::try_default()
        .await
        .context("Failed to create Kubernetes client")?;
    let jobs: Api<Job> = Api::namespaced(client.clone(), NAMESPACE);
    let pods: Api<Pod> = Api::namespaced(client, NAMESPACE);

    // Find the job
    let job_name = if let Some(filter) = job_filter {
        // If it looks like a full job name, use it directly
        if filter.starts_with("claude-review-") {
            filter.to_string()
        } else {
            // Search for job by queue-id prefix
            let lp = ListParams::default().labels("app=claude-review");
            let job_list = jobs.list(&lp).await.context("Failed to list jobs")?;

            job_list
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
                .context(format!("No job found matching '{filter}'"))?
        }
    } else {
        // Get the most recent running job, or the most recent job if none running
        let lp = ListParams::default().labels("app=claude-review");
        let job_list = jobs.list(&lp).await.context("Failed to list jobs")?;

        let running = job_list.items.iter().find(|j| {
            j.status
                .as_ref()
                .and_then(|s| s.active)
                .unwrap_or(0)
                > 0
        });

        running
            .or(job_list.items.last())
            .and_then(|j| j.metadata.name.clone())
            .context("No review jobs found")?
    };

    println!("Fetching logs for job: {job_name}");

    // Find the pod for this job
    let lp = ListParams::default().labels(&format!("job-name={job_name}"));
    let pod_list = pods.list(&lp).await.context("Failed to list pods")?;

    let pod_name = pod_list
        .items
        .first()
        .and_then(|p| p.metadata.name.clone())
        .context("No pod found for job")?;

    // Get logs
    let mut lp = LogParams {
        tail_lines: Some(tail),
        follow,
        ..Default::default()
    };

    if follow {
        // Stream logs
        let stream = pods
            .log_stream(&pod_name, &lp)
            .await
            .context("Failed to get log stream")?;

        let mut lines = stream.lines();
        while let Some(line) = lines.next().await {
            println!("{}", line?);
        }
    } else {
        // Get all logs at once
        lp.follow = false;
        let logs = pods
            .logs(&pod_name, &lp)
            .await
            .context("Failed to get logs")?;
        print!("{logs}");
    }

    Ok(())
}

// HTTP API client for server communication

/// Stats response from the server API.
#[derive(Deserialize)]
struct ApiStats {
    pending: u64,
    processing: u64,
    failed: u64,
}

/// Create an HTTP client with API key authentication.
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

/// Fetch queue stats via HTTP API.
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

    let stats: ApiStats = resp.json().await.context("Failed to parse stats response")?;

    println!("Queue Statistics:");
    println!("  Pending:    {}", stats.pending);
    println!("  Processing: {}", stats.processing);
    println!("  Failed:     {}", stats.failed);

    Ok(())
}

/// Fetch failed items via HTTP API.
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

/// Retry a failed item via HTTP API.
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

/// Queue a review via HTTP API.
async fn api_queue_review(
    server_url: &str,
    api_key: &str,
    project: &str,
    mr_iid: u64,
    gitlab_url: &str,
) -> Result<String> {
    let client = create_api_client(api_key)?;
    let url = format!("{}/api/review", server_url.trim_end_matches('/'));

    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "project": project,
            "mr_iid": mr_iid,
            "gitlab_url": gitlab_url,
        }))
        .send()
        .await
        .context("Failed to queue review")?;

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
