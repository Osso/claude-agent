//! Claude Agent CLI
//!
//! CLI for managing the review queue and testing.

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

use claude_agent_server::{FailedItem, Queue, ReviewPayload};

const NAMESPACE: &str = "claude-agent";

#[derive(Parser)]
#[command(name = "claude-agent")]
#[command(about = "Claude Agent CLI for MR review management")]
struct Cli {
    /// Redis URL
    #[arg(long, env = "REDIS_URL", default_value = "redis://127.0.0.1:6379")]
    redis_url: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Fetch and display MR info (no Redis required)
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

    /// Trigger a review for an MR (fetches details from GitLab)
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

        /// GitLab token (defaults to GITLAB_TOKEN env var)
        #[arg(long, env = "GITLAB_TOKEN")]
        token: String,
    },

    /// Show queue statistics
    Stats,

    /// List items in the queue
    List {
        /// Show failed items instead of pending
        #[arg(long)]
        failed: bool,

        /// Maximum number of items to show
        #[arg(long, default_value = "10")]
        limit: usize,
    },

    /// Queue a review manually
    Queue {
        /// GitLab URL (e.g., https://gitlab.com)
        #[arg(long)]
        gitlab_url: String,

        /// Project path (e.g., group/project)
        #[arg(long)]
        project: String,

        /// Merge request IID
        #[arg(long)]
        mr_iid: String,

        /// Clone URL
        #[arg(long)]
        clone_url: String,

        /// Source branch
        #[arg(long)]
        source_branch: String,

        /// Target branch
        #[arg(long, default_value = "main")]
        target_branch: String,

        /// MR title
        #[arg(long)]
        title: String,

        /// Author username
        #[arg(long)]
        author: String,
    },

    /// Retry a failed item
    Retry {
        /// Job ID to retry
        id: String,
    },

    /// Clear failed items
    ClearFailed,

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

    // Handle commands that don't need Redis first
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

    let queue = Queue::new(&cli.redis_url)
        .await
        .context("Failed to connect to Redis")?;

    match cli.command {
        Commands::Info { .. } | Commands::Logs { .. } | Commands::Jobs { .. } => {
            unreachable!() // Handled above
        }

        Commands::Review {
            project,
            mr,
            gitlab_url,
            token,
        } => {
            // Fetch MR details from GitLab
            let mr_info = fetch_mr_info(&gitlab_url, &project, mr, &token).await?;

            let payload = ReviewPayload {
                gitlab_url: gitlab_url.clone(),
                project: project.clone(),
                mr_iid: mr.to_string(),
                clone_url: mr_info.clone_url,
                source_branch: mr_info.source_branch,
                target_branch: mr_info.target_branch,
                title: mr_info.title,
                description: mr_info.description,
                author: mr_info.author,
            };

            let id = queue.push(payload).await?;
            println!("Queued review for !{} in {}", mr, project);
            println!("Job ID: {id}");
        }

        Commands::Stats => {
            let pending = queue.len().await?;
            let processing = queue.processing_count().await?;
            let failed = queue.failed_count().await?;

            println!("Queue Statistics:");
            println!("  Pending:    {pending}");
            println!("  Processing: {processing}");
            println!("  Failed:     {failed}");
        }

        Commands::List { failed, limit } => {
            if failed {
                let items = queue.list_failed(limit).await?;

                if items.is_empty() {
                    println!("No failed items");
                } else {
                    println!("Failed Items:");
                    for item in items {
                        println!();
                        print_failed_item(&item);
                    }
                }
            } else {
                // Note: We can't easily list pending items without modifying the queue
                // This would require LRANGE which doesn't remove items
                println!("Pending: {} items in queue", queue.len().await?);
                println!("(Use --failed to list failed items with details)");
            }
        }

        Commands::Queue {
            gitlab_url,
            project,
            mr_iid,
            clone_url,
            source_branch,
            target_branch,
            title,
            author,
        } => {
            let payload = ReviewPayload {
                gitlab_url,
                project,
                mr_iid,
                clone_url,
                source_branch,
                target_branch,
                title,
                description: None,
                author,
            };

            let id = queue.push(payload).await?;
            println!("Queued review job: {id}");
        }

        Commands::Retry { id } => {
            if queue.retry_failed(&id).await? {
                println!("Retried job: {id}");
            } else {
                println!("Job not found in failed list: {id}");
            }
        }

        Commands::ClearFailed => {
            let count = queue.clear_failed().await?;
            println!("Cleared {count} failed items");
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
    let mut headers = HeaderMap::new();
    // Support both PAT (PRIVATE-TOKEN) and OAuth (Bearer) tokens
    // PATs are typically shorter and start with "glpat-"
    if token.starts_with("glpat-") || token.len() < 50 {
        headers.insert("PRIVATE-TOKEN", HeaderValue::from_str(token)?);
    } else {
        headers.insert("Authorization", HeaderValue::from_str(&format!("Bearer {token}"))?);
    }

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
