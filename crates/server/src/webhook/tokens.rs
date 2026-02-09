//! Token validation endpoint.

use std::sync::Arc;

use axum::{
    extract::State,
    http::HeaderMap,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::jira_token::JiraTokenManager;

use super::{AppError, AppState};

pub(super) async fn check_tokens_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    if !state.verify_api_key(&headers) {
        warn!("Invalid API key for /api/check-tokens");
        return Err(AppError::Unauthorized);
    }

    let client = reqwest::Client::new();
    let gitlab = check_gitlab_token(&client, &state.gitlab_token).await;
    let github = match &state.github_token {
        Some(token) => check_github_token(&client, token).await,
        None => TokenStatus::not_configured(),
    };
    let sentry = match &state.sentry_auth_token {
        Some(token) => check_sentry_token(&client, token).await,
        None => TokenStatus::not_configured(),
    };
    let claude = match &state.claude_token {
        Some(token) => check_claude_token(token),
        None => TokenStatus::not_configured(),
    };
    let jira = match &state.jira_token_manager {
        Some(manager) => check_jira_token(manager).await,
        None => TokenStatus::not_configured(),
    };

    Ok(Json(serde_json::json!({
        "gitlab": gitlab,
        "github": github,
        "sentry": sentry,
        "claude": claude,
        "jira": jira,
    })))
}

#[derive(Serialize)]
struct TokenStatus {
    configured: bool,
    valid: bool,
    info: Option<String>,
    error: Option<String>,
}

impl TokenStatus {
    fn not_configured() -> Self {
        Self {
            configured: false,
            valid: false,
            info: None,
            error: None,
        }
    }

    fn valid(info: String) -> Self {
        Self {
            configured: true,
            valid: true,
            info: Some(info),
            error: None,
        }
    }

    fn invalid(error: String) -> Self {
        Self {
            configured: true,
            valid: false,
            info: None,
            error: Some(error),
        }
    }
}

async fn check_gitlab_token(client: &reqwest::Client, token: &str) -> TokenStatus {
    let resp = client
        .get("https://gitlab.com/api/v4/user")
        .header("PRIVATE-TOKEN", token)
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() => {
            #[derive(Deserialize)]
            struct User {
                username: String,
            }
            match r.json::<User>().await {
                Ok(u) => TokenStatus::valid(format!("@{}", u.username)),
                Err(e) => TokenStatus::invalid(e.to_string()),
            }
        }
        Ok(r) => TokenStatus::invalid(format!("{}", r.status())),
        Err(e) => TokenStatus::invalid(e.to_string()),
    }
}

async fn check_github_token(client: &reqwest::Client, token: &str) -> TokenStatus {
    let resp = client
        .get("https://api.github.com/user")
        .header("Authorization", format!("Bearer {}", token))
        .header("User-Agent", "claude-agent")
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() => {
            #[derive(Deserialize)]
            struct User {
                login: String,
            }
            match r.json::<User>().await {
                Ok(u) => TokenStatus::valid(format!("@{}", u.login)),
                Err(e) => TokenStatus::invalid(e.to_string()),
            }
        }
        Ok(r) => TokenStatus::invalid(format!("{}", r.status())),
        Err(e) => TokenStatus::invalid(e.to_string()),
    }
}

async fn check_sentry_token(client: &reqwest::Client, token: &str) -> TokenStatus {
    let resp = client
        .get("https://sentry.io/api/0/organizations/")
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() => {
            #[derive(Deserialize)]
            struct Org {
                slug: String,
            }
            match r.json::<Vec<Org>>().await {
                Ok(orgs) => {
                    let slugs: Vec<_> = orgs.iter().map(|o| o.slug.as_str()).collect();
                    TokenStatus::valid(format!("orgs: {}", slugs.join(", ")))
                }
                Err(e) => TokenStatus::invalid(e.to_string()),
            }
        }
        Ok(r) => TokenStatus::invalid(format!("{}", r.status())),
        Err(e) => TokenStatus::invalid(e.to_string()),
    }
}

fn check_claude_token(token: &str) -> TokenStatus {
    if token.starts_with("sk-ant-oat01-") {
        TokenStatus::valid("OAuth token (format valid)".to_string())
    } else if token.starts_with("sk-ant-api") {
        TokenStatus::valid("API key (format valid)".to_string())
    } else {
        TokenStatus::invalid("unrecognized token format".to_string())
    }
}

async fn check_jira_token(manager: &JiraTokenManager) -> TokenStatus {
    match manager.get_access_token_with_expiry().await {
        Ok((_token, expires_in_secs)) => {
            TokenStatus::valid(format!("expires in {}m", expires_in_secs / 60))
        }
        Err(e) => TokenStatus::invalid(e.to_string()),
    }
}
