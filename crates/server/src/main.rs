//! Claude Agent Server
//!
//! Webhook handler and job scheduler for MR reviews.

use std::env;
use std::net::SocketAddr;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

mod gitlab;
mod queue;
mod scheduler;
mod webhook;

use queue::Queue;
use scheduler::Scheduler;
use webhook::{router, AppState};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("Claude Agent Server starting");

    // Get configuration from environment
    let redis_url = env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let webhook_secret = env::var("WEBHOOK_SECRET").context("WEBHOOK_SECRET not set")?;
    let listen_addr = env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8443".into());

    // Initialize queue
    let queue = Queue::new(&redis_url)
        .await
        .context("Failed to connect to Redis")?;

    info!(redis = %redis_url, "Connected to Redis");

    // Build application state
    let state = AppState {
        queue: queue.clone(),
        webhook_secret,
    };

    // Build router
    let app = router(state).layer(TraceLayer::new_for_http());

    // Start scheduler in background
    let scheduler = Scheduler::new(queue)
        .await
        .context("Failed to create scheduler")?;

    let scheduler_handle = tokio::spawn(async move {
        scheduler.run().await;
    });

    // Start HTTP server
    let addr: SocketAddr = listen_addr.parse().context("Invalid LISTEN_ADDR")?;
    let listener = TcpListener::bind(addr).await?;
    info!(addr = %addr, "Listening for webhooks");

    axum::serve(listener, app)
        .await
        .context("Server error")?;

    // Wait for scheduler (shouldn't happen unless shutdown)
    scheduler_handle.await?;

    Ok(())
}
