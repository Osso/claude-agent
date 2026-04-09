//! Token validation endpoint.

use std::sync::Arc;

use axum::{Json, extract::State, http::HeaderMap, response::IntoResponse};
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
    let response = match client
        .get("https://sentry.io/api/0/organizations/")
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return TokenStatus::invalid(error.to_string()),
    };
    if !response.status().is_success() {
        return TokenStatus::invalid(format!("{}", response.status()));
    }

    #[derive(Deserialize)]
    struct Org {
        slug: String,
    }
    let orgs = match response.json::<Vec<Org>>().await {
        Ok(orgs) => orgs,
        Err(error) => return TokenStatus::invalid(error.to_string()),
    };
    let slugs: Vec<_> = orgs.iter().map(|org| org.slug.as_str()).collect();
    TokenStatus::valid(format!("orgs: {}", slugs.join(", ")))
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
