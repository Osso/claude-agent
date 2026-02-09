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
    init_logging();

    const VERSION: &str = "2026.02.06.3";
    info!(version = VERSION, "Claude Agent Server starting");

    let state = build_app_state().await?;
    let queue = state.queue.clone();
    let jira_token_manager = state.jira_token_manager.clone();
    let listen_addr = env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8443".into());

    let app = router(state).layer(TraceLayer::new_for_http());

    let scheduler = Arc::new(
        Scheduler::new(queue, jira_token_manager)
            .await
            .context("Failed to create scheduler")?,
    );

    let scheduler_clone = scheduler.clone();
    let scheduler_handle = tokio::spawn(async move {
        scheduler_clone.run().await;
    });

    let addr: SocketAddr = listen_addr.parse().context("Invalid LISTEN_ADDR")?;
    let listener = TcpListener::bind(addr).await?;
    info!(addr = %addr, "Listening for webhooks");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(scheduler.clone()))
        .await
        .context("Server error")?;

    scheduler_handle.await?;
    info!("Server shutdown complete");
    Ok(())
}

fn init_logging() {
    let filter = EnvFilter::try_from_env("LOG_LEVEL")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

async fn build_app_state() -> Result<AppState> {
    let redis_url = env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let webhook_secret = env::var("WEBHOOK_SECRET").context("WEBHOOK_SECRET not set")?;
    let gitlab_token = env::var("GITLAB_TOKEN").context("GITLAB_TOKEN not set")?;

    let queue = Queue::new(&redis_url)
        .await
        .context("Failed to connect to Redis")?;
    info!(redis = %redis_url, "Connected to Redis");

    let jira_token_manager = init_jira_token_manager().await;

    let allowed_authors = parse_allowed_authors();
    if !allowed_authors.is_empty() {
        info!(authors = ?allowed_authors, "Author allowlist configured");
    }

    Ok(AppState {
        queue,
        webhook_secret,
        api_key: env::var("API_KEY").ok(),
        gitlab_token,
        github_token: env::var("GITHUB_TOKEN").ok(),
        sentry_webhook_secret: env::var("SENTRY_WEBHOOK_SECRET").ok(),
        sentry_auth_token: env::var("SENTRY_AUTH_TOKEN").ok(),
        claude_token: env::var("CLAUDE_CODE_OAUTH_TOKEN").ok(),
        sentry_organization: env::var("SENTRY_ORGANIZATION").ok(),
        sentry_project_mappings: parse_sentry_mappings(),
        jira_token_manager,
        jira_webhook_secret: env::var("JIRA_WEBHOOK_SECRET").ok(),
        jira_project_mappings: parse_jira_mappings(),
        allowed_authors,
    })
}

async fn init_jira_token_manager() -> Option<Arc<JiraTokenManager>> {
    let client_id = env::var("JIRA_CLIENT_ID").ok()?;
    if client_id.is_empty() {
        return None;
    }
    let client_secret = match env::var("JIRA_CLIENT_SECRET").ok() {
        Some(s) => s,
        None => return None,
    };
    let refresh_token = env::var("JIRA_REFRESH_TOKEN").ok();

    let k8s_client = match kube::Client::try_default().await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "Failed to create K8s client for Jira token manager");
            return None;
        }
    };

    match JiraTokenManager::new(k8s_client, client_id, client_secret, refresh_token).await {
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

/// Parse comma-separated ALLOWED_AUTHORS env var.
fn parse_allowed_authors() -> Vec<String> {
    env::var("ALLOWED_AUTHORS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
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

fn parse_sentry_mappings() -> Vec<SentryMapping> {
    match env::var("SENTRY_PROJECT_MAPPINGS") {
        Ok(json) => sentry::parse_project_mappings(&json).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to parse SENTRY_PROJECT_MAPPINGS");
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

fn parse_jira_mappings() -> Vec<JiraProjectMapping> {
    match env::var("JIRA_PROJECT_MAPPINGS") {
        Ok(json) => jira::parse_project_mappings(&json).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to parse JIRA_PROJECT_MAPPINGS");
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}
