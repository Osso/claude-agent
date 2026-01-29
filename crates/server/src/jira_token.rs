//! Jira OAuth token manager with automatic refresh and K8s secret persistence.
//!
//! Atlassian OAuth uses rotating refresh tokens - each exchange returns a new
//! refresh token that invalidates the previous one. This module handles:
//! - In-memory caching of access tokens
//! - Automatic refresh when tokens expire
//! - Persisting new refresh tokens to a K8s Secret

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::ByteString;
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::Client;
use reqwest::Client as HttpClient;
use serde::Deserialize;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

const NAMESPACE: &str = "claude-agent";
const DYNAMIC_SECRET_NAME: &str = "claude-agent-jira-tokens";
const TOKEN_URL: &str = "https://auth.atlassian.com/oauth/token";

/// Buffer before actual expiry to trigger refresh (5 minutes)
const EXPIRY_BUFFER: Duration = Duration::from_secs(300);

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("K8s API error: {0}")]
    Kubernetes(#[from] kube::Error),

    #[error("HTTP request error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("No refresh token available")]
    NoRefreshToken,

    #[error("OAuth error: {error} - {description}")]
    OAuth { error: String, description: String },

}

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64, // seconds
    #[allow(dead_code)]
    token_type: String,
}

#[derive(Debug, Deserialize)]
struct OAuthErrorResponse {
    error: String,
    error_description: Option<String>,
}

struct CachedToken {
    token: String,
    expires_at: Instant,
}

/// Manages Jira OAuth tokens with automatic refresh and K8s secret persistence.
pub struct JiraTokenManager {
    #[allow(dead_code)]
    k8s_client: Client,
    secrets_api: Api<Secret>,
    http_client: HttpClient,
    client_id: String,
    client_secret: String,
    /// Bootstrap refresh token from sealed secret (used only if dynamic secret is empty)
    bootstrap_refresh_token: Option<String>,
    /// In-memory cache of the current access token
    cached_token: Arc<RwLock<Option<CachedToken>>>,
}

impl JiraTokenManager {
    /// Create a new JiraTokenManager.
    ///
    /// # Arguments
    /// * `k8s_client` - Kubernetes client
    /// * `client_id` - OAuth client ID
    /// * `client_secret` - OAuth client secret
    /// * `bootstrap_refresh_token` - Initial refresh token from sealed secret (optional)
    pub async fn new(
        k8s_client: Client,
        client_id: String,
        client_secret: String,
        bootstrap_refresh_token: Option<String>,
    ) -> Result<Self, TokenError> {
        let secrets_api = Api::namespaced(k8s_client.clone(), NAMESPACE);
        let http_client = HttpClient::new();

        info!("Jira token manager initialized");

        Ok(Self {
            k8s_client,
            secrets_api,
            http_client,
            client_id,
            client_secret,
            bootstrap_refresh_token,
            cached_token: Arc::new(RwLock::new(None)),
        })
    }

    /// Get a valid access token, refreshing if needed.
    pub async fn get_access_token(&self) -> Result<String, TokenError> {
        let (token, _) = self.get_access_token_with_expiry().await?;
        Ok(token)
    }

    /// Get a valid access token and seconds until expiry.
    pub async fn get_access_token_with_expiry(&self) -> Result<(String, u64), TokenError> {
        // Check in-memory cache first
        {
            let cache = self.cached_token.read().await;
            if let Some(ref cached) = *cache
                && cached.expires_at > Instant::now() + EXPIRY_BUFFER
            {
                debug!("Using cached Jira access token");
                let secs_remaining = cached.expires_at.duration_since(Instant::now()).as_secs();
                return Ok((cached.token.clone(), secs_remaining));
            }
        }

        // Token expired or not cached, need to refresh
        self.refresh_and_cache_with_expiry().await
    }

    /// Force refresh tokens (call when API returns 401).
    #[allow(dead_code)]
    pub async fn force_refresh(&self) -> Result<String, TokenError> {
        info!("Force refreshing Jira tokens");
        // Clear cache to force refresh
        *self.cached_token.write().await = None;
        self.refresh_and_cache().await
    }

    /// Refresh tokens and update cache.
    async fn refresh_and_cache(&self) -> Result<String, TokenError> {
        let (token, _) = self.refresh_and_cache_with_expiry().await?;
        Ok(token)
    }

    /// Refresh tokens and update cache, returning expiry in seconds.
    async fn refresh_and_cache_with_expiry(&self) -> Result<(String, u64), TokenError> {
        let refresh_token = self.read_refresh_token().await?;
        let (access_token, new_refresh_token, expires_in) =
            self.exchange_refresh_token(&refresh_token).await?;

        // Update K8s secret with new tokens
        self.update_secret(&access_token, &new_refresh_token).await?;

        // Update in-memory cache
        let expires_at = Instant::now() + Duration::from_secs(expires_in);
        *self.cached_token.write().await = Some(CachedToken {
            token: access_token.clone(),
            expires_at,
        });

        info!(
            expires_in_secs = expires_in,
            "Jira tokens refreshed successfully"
        );
        Ok((access_token, expires_in))
    }

    /// Read refresh token from K8s Secret.
    /// Tries dynamic secret first, falls back to bootstrap token.
    async fn read_refresh_token(&self) -> Result<String, TokenError> {
        // Try dynamic secret first
        match self.secrets_api.get(DYNAMIC_SECRET_NAME).await {
            Ok(secret) => {
                if let Some(data) = secret.data
                    && let Some(token_bytes) = data.get("refresh-token")
                {
                    let token = String::from_utf8_lossy(&token_bytes.0).to_string();
                    if !token.is_empty() {
                        debug!("Using refresh token from dynamic secret");
                        return Ok(token);
                    }
                }
            }
            Err(kube::Error::Api(ref err)) if err.code == 404 => {
                debug!("Dynamic secret not found, will create on first refresh");
            }
            Err(e) => {
                warn!(error = %e, "Failed to read dynamic secret, trying bootstrap token");
            }
        }

        // Fall back to bootstrap token
        if let Some(ref token) = self.bootstrap_refresh_token
            && !token.is_empty()
        {
            info!("Using bootstrap refresh token (first-time initialization)");
            return Ok(token.clone());
        }

        Err(TokenError::NoRefreshToken)
    }

    /// Exchange refresh token for new access token.
    async fn exchange_refresh_token(
        &self,
        refresh_token: &str,
    ) -> Result<(String, String, u64), TokenError> {
        let response = self
            .http_client
            .post(TOKEN_URL)
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", &self.client_id),
                ("client_secret", &self.client_secret),
                ("refresh_token", refresh_token),
            ])
            .send()
            .await?;

        let status = response.status();
        let body = response.text().await?;

        if !status.is_success() {
            // Try to parse as OAuth error
            if let Ok(err) = serde_json::from_str::<OAuthErrorResponse>(&body) {
                error!(
                    error = %err.error,
                    description = err.error_description.as_deref().unwrap_or(""),
                    "OAuth token refresh failed"
                );
                return Err(TokenError::OAuth {
                    error: err.error,
                    description: err.error_description.unwrap_or_default(),
                });
            }
            error!(status = %status, body = %body, "OAuth token refresh failed");
            return Err(TokenError::OAuth {
                error: status.to_string(),
                description: body,
            });
        }

        let token_response: OAuthTokenResponse = serde_json::from_str(&body).map_err(|e| {
            error!(error = %e, body = %body, "Failed to parse OAuth response");
            TokenError::OAuth {
                error: "parse_error".into(),
                description: e.to_string(),
            }
        })?;

        Ok((
            token_response.access_token,
            token_response.refresh_token,
            token_response.expires_in,
        ))
    }

    /// Update both tokens in the dynamic K8s Secret.
    async fn update_secret(
        &self,
        access_token: &str,
        refresh_token: &str,
    ) -> Result<(), TokenError> {
        let mut data = BTreeMap::new();
        data.insert(
            "access-token".to_string(),
            ByteString(access_token.as_bytes().to_vec()),
        );
        data.insert(
            "refresh-token".to_string(),
            ByteString(refresh_token.as_bytes().to_vec()),
        );

        let secret = Secret {
            metadata: kube::api::ObjectMeta {
                name: Some(DYNAMIC_SECRET_NAME.into()),
                namespace: Some(NAMESPACE.into()),
                ..Default::default()
            },
            data: Some(data),
            ..Default::default()
        };

        // Try to patch first, create if not exists
        match self
            .secrets_api
            .patch(
                DYNAMIC_SECRET_NAME,
                &PatchParams::apply("claude-agent-server"),
                &Patch::Apply(&secret),
            )
            .await
        {
            Ok(_) => {
                debug!("Updated Jira tokens in dynamic secret");
                Ok(())
            }
            Err(kube::Error::Api(ref err)) if err.code == 404 => {
                // Secret doesn't exist, create it
                info!("Creating dynamic secret for Jira tokens");
                self.secrets_api
                    .create(&PostParams::default(), &secret)
                    .await?;
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Check if Jira integration is configured.
    #[allow(dead_code)]
    pub fn is_configured(&self) -> bool {
        !self.client_id.is_empty() && !self.client_secret.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expiry_buffer() {
        // Verify the buffer is reasonable (5 minutes)
        assert_eq!(EXPIRY_BUFFER, Duration::from_secs(300));
    }

    #[test]
    fn test_oauth_error_parse() {
        let json = r#"{"error": "invalid_grant", "error_description": "Token expired"}"#;
        let err: OAuthErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(err.error, "invalid_grant");
        assert_eq!(err.error_description.as_deref(), Some("Token expired"));
    }

    #[test]
    fn test_oauth_token_response_parse() {
        let json = r#"{
            "access_token": "eyJhbGc...",
            "refresh_token": "new_refresh_token",
            "expires_in": 3600,
            "token_type": "Bearer"
        }"#;
        let resp: OAuthTokenResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.access_token, "eyJhbGc...");
        assert_eq!(resp.refresh_token, "new_refresh_token");
        assert_eq!(resp.expires_in, 3600);
    }
}
