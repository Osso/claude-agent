//! Claude Agent Server
//!
//! Webhook handler and job scheduler for MR reviews.

use std::env;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::signal;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod github;
mod gitlab;
mod queue;
mod scheduler;
mod webhook;

use queue::Queue;
use scheduler::Scheduler;
use webhook::{router, AppState};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging (configurable via RUST_LOG or LOG_LEVEL env var)
    let filter = EnvFilter::try_from_env("LOG_LEVEL")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    const VERSION: &str = "2026.01.28.5";
    info!(version = VERSION, "Claude Agent Server starting");

    // Get configuration from environment
    let redis_url = env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let webhook_secret = env::var("WEBHOOK_SECRET").context("WEBHOOK_SECRET not set")?;
    let api_key = env::var("API_KEY").ok(); // Optional, defaults to webhook_secret
    let gitlab_token = env::var("GITLAB_TOKEN").context("GITLAB_TOKEN not set")?;
    let github_token = env::var("GITHUB_TOKEN").ok();
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
        api_key,
        gitlab_token,
        github_token,
    };

    // Build router
    let app = router(state).layer(TraceLayer::new_for_http());

    // Start scheduler in background
    let scheduler = Arc::new(
        Scheduler::new(queue)
            .await
            .context("Failed to create scheduler")?,
    );

    let scheduler_clone = scheduler.clone();
    let scheduler_handle = tokio::spawn(async move {
        scheduler_clone.run().await;
    });

    // Start HTTP server
    let addr: SocketAddr = listen_addr.parse().context("Invalid LISTEN_ADDR")?;
    let listener = TcpListener::bind(addr).await?;
    info!(addr = %addr, "Listening for webhooks");

    // Run server with graceful shutdown
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(scheduler.clone()))
        .await
        .context("Server error")?;

    // Wait for scheduler to stop
    scheduler_handle.await?;

    info!("Server shutdown complete");
    Ok(())
}

async fn shutdown_signal(scheduler: Arc<Scheduler>) {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("Shutdown signal received, stopping...");
    scheduler.stop().await;
}
