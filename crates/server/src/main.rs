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
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

mod github;
mod gitlab;
mod jira;
mod jira_token;
mod payload;
mod queue;
mod scheduler;
mod sentry;
mod sentry_api;
mod webhook;

use jira::JiraProjectMapping;
use jira_token::JiraTokenManager;
use queue::Queue;
use scheduler::Scheduler;
use sentry::SentryProjectMapping as SentryMapping;
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

    const VERSION: &str = "2026.02.05.1";
    info!(version = VERSION, "Claude Agent Server starting");

    // Get configuration from environment
    let redis_url = env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let webhook_secret = env::var("WEBHOOK_SECRET").context("WEBHOOK_SECRET not set")?;
    let api_key = env::var("API_KEY").ok(); // Optional, defaults to webhook_secret
    let gitlab_token = env::var("GITLAB_TOKEN").context("GITLAB_TOKEN not set")?;
    let github_token = env::var("GITHUB_TOKEN").ok();
    let sentry_webhook_secret = env::var("SENTRY_WEBHOOK_SECRET").ok();
    let sentry_auth_token = env::var("SENTRY_AUTH_TOKEN").ok();
    let claude_token = env::var("CLAUDE_CODE_OAUTH_TOKEN").ok();
    let sentry_organization = env::var("SENTRY_ORGANIZATION").ok();
    let sentry_project_mappings = parse_sentry_mappings();
    let listen_addr = env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8443".into());

    // Jira OAuth configuration (optional)
    let jira_client_id = env::var("JIRA_CLIENT_ID").ok();
    let jira_client_secret = env::var("JIRA_CLIENT_SECRET").ok();
    let jira_refresh_token = env::var("JIRA_REFRESH_TOKEN").ok();
    let jira_webhook_secret = env::var("JIRA_WEBHOOK_SECRET").ok();
    let jira_project_mappings = parse_jira_mappings();

    // Initialize queue
    let queue = Queue::new(&redis_url)
        .await
        .context("Failed to connect to Redis")?;

    info!(redis = %redis_url, "Connected to Redis");

    // Initialize Jira token manager (optional)
    let jira_token_manager = match (&jira_client_id, &jira_client_secret) {
        (Some(client_id), Some(client_secret)) if !client_id.is_empty() => {
            match JiraTokenManager::new(
                kube::Client::try_default()
                    .await
                    .context("Failed to create K8s client for Jira token manager")?,
                client_id.clone(),
                client_secret.clone(),
                jira_refresh_token.clone(),
            )
            .await
            {
                Ok(manager) => {
                    info!("Jira token manager initialized");
                    Some(Arc::new(manager))
                }
                Err(e) => {
                    warn!(error = %e, "Failed to initialize Jira token manager");
                    None
                }
            }
        }
        _ => {
            info!("Jira integration not configured (JIRA_CLIENT_ID/JIRA_CLIENT_SECRET not set)");
            None
        }
    };

    // Build application state
    let state = AppState {
        queue: queue.clone(),
        webhook_secret,
        api_key,
        gitlab_token,
        github_token,
        sentry_webhook_secret,
        sentry_auth_token,
        claude_token,
        sentry_organization,
        sentry_project_mappings,
        jira_token_manager: jira_token_manager.clone(),
        jira_webhook_secret,
        jira_project_mappings,
    };

    // Build router
    let app = router(state).layer(TraceLayer::new_for_http());

    // Start scheduler in background
    let scheduler = Arc::new(
        Scheduler::new(queue, jira_token_manager)
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

/// Parse Sentry project mappings from SENTRY_PROJECT_MAPPINGS env var.
fn parse_sentry_mappings() -> Vec<SentryMapping> {
    match env::var("SENTRY_PROJECT_MAPPINGS") {
        Ok(json) => sentry::parse_project_mappings(&json).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to parse SENTRY_PROJECT_MAPPINGS");
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

/// Parse Jira project mappings from JIRA_PROJECT_MAPPINGS env var.
fn parse_jira_mappings() -> Vec<JiraProjectMapping> {
    match env::var("JIRA_PROJECT_MAPPINGS") {
        Ok(json) => jira::parse_project_mappings(&json).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to parse JIRA_PROJECT_MAPPINGS");
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}
