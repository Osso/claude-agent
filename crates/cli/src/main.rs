//! Claude Agent CLI
//!
//! CLI for managing the review queue and testing.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::Level;
use tracing_subscriber::FmtSubscriber;

use claude_agent_server::{FailedItem, Queue, ReviewPayload};

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

    let queue = Queue::new(&cli.redis_url)
        .await
        .context("Failed to connect to Redis")?;

    match cli.command {
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
