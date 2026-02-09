//! Sentry API client for fetching issue details.

#![allow(dead_code)] // Used by worker crate

use anyhow::{Context, Result};
use serde_json::Value;
use std::time::Duration;
use tracing::debug;

const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF_SECS: u64 = 1;

/// Sentry API client.
pub struct SentryClient {
    http: reqwest::Client,
    base_url: String,
    auth_token: String,
    organization: String,
}

impl SentryClient {
    pub fn new(organization: &str, auth_token: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;

        Ok(Self {
            http,
            base_url: "https://sentry.io/api/0".to_string(),
            auth_token: auth_token.to_string(),
            organization: organization.to_string(),
        })
    }

    /// Get issue details by ID (numeric or short ID like "WEB-123").
    pub async fn get_issue(&self, issue_id: &str) -> Result<Value> {
        self.get(&format!(
            "/organizations/{}/issues/{}/",
            self.organization, issue_id
        ))
        .await
    }

    /// Get the latest event for an issue.
    pub async fn get_issue_latest_event(&self, issue_id: &str) -> Result<Value> {
        self.get(&format!("/issues/{}/events/latest/", issue_id))
            .await
    }

    /// Get events for an issue.
    pub async fn get_issue_events(&self, issue_id: &str, limit: u32) -> Result<Value> {
        self.get(&format!("/issues/{}/events/?per_page={}", issue_id, limit))
            .await
    }

    /// Get a specific event by ID.
    pub async fn get_event(&self, issue_id: &str, event_id: &str) -> Result<Value> {
        self.get(&format!("/issues/{}/events/{}/", issue_id, event_id))
            .await
    }

    async fn get(&self, endpoint: &str) -> Result<Value> {
        let url = format!("{}{}", self.base_url, endpoint);
        debug!(url = %url, "Sentry API request");

        let resp = self
            .send_with_retry(|| {
                self.http
                    .get(&url)
                    .header("Authorization", format!("Bearer {}", self.auth_token))
                    .header("Content-Type", "application/json")
                    .send()
            })
            .await
            .context("Failed to send Sentry API request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Sentry API error: {} - {}", status, body);
        }

        resp.json().await.context("Failed to parse Sentry JSON response")
    }

    /// Send request with retry logic for transient failures.
    async fn send_with_retry<F, Fut>(&self, make_request: F) -> Result<reqwest::Response, reqwest::Error>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
    {
        let mut last_error = None;

        for attempt in 0..=MAX_RETRIES {
            match make_request().await {
                Ok(resp) => return Ok(resp),
                Err(err) => {
                    if attempt < MAX_RETRIES && is_retryable(&err) {
                        let delay = INITIAL_BACKOFF_SECS * 2u64.pow(attempt);
                        debug!(
                            attempt = attempt + 1,
                            max = MAX_RETRIES,
                            delay_secs = delay,
                            "Retrying Sentry API request"
                        );
                        tokio::time::sleep(Duration::from_secs(delay)).await;
                        last_error = Some(err);
                    } else {
                        return Err(err);
                    }
                }
            }
        }

        Err(last_error.expect("should have an error after retries"))
    }
}

/// Check if an error is retryable (timeouts, connection errors).
fn is_retryable(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect() || {
        let err_string = format!("{:?}", err);
        err_string.contains("os error 110") || err_string.contains("Connection timed out")
    }
}

/// Extract a formatted stacktrace from a Sentry event.
pub fn format_stacktrace(event: &Value) -> String {
    let mut output = String::new();

    // Try to find exception info
    if let Some(entries) = event["entries"].as_array() {
        for entry in entries {
            if entry["type"].as_str() == Some("exception")
                && let Some(values) = entry["data"]["values"].as_array()
            {
                for exc in values {
                    let exc_type = exc["type"].as_str().unwrap_or("Exception");
                    let exc_value = exc["value"].as_str().unwrap_or("");
                    output.push_str(&format!("## {} : {}\n\n", exc_type, exc_value));

                    if let Some(frames) = exc["stacktrace"]["frames"].as_array() {
                        output.push_str("### Stacktrace (most recent last)\n\n");
                        for frame in frames {
                            let filename = frame["filename"].as_str().unwrap_or("?");
                            let function = frame["function"].as_str().unwrap_or("?");
                            let lineno = frame["lineNo"]
                                .as_u64()
                                .map(|n| n.to_string())
                                .unwrap_or_else(|| "?".into());

                            output.push_str(&format!(
                                "  {} in {}:{}\n",
                                function, filename, lineno
                            ));

                            // Include context lines if available
                            if let Some(context) = frame["context"].as_array() {
                                for line in context {
                                    if let (Some(num), Some(code)) =
                                        (line[0].as_u64(), line[1].as_str())
                                    {
                                        let marker =
                                            if Some(num) == frame["lineNo"].as_u64() {
                                                ">"
                                            } else {
                                                " "
                                            };
                                        output.push_str(&format!(
                                            "    {} {:4} | {}\n",
                                            marker, num, code
                                        ));
                                    }
                                }
                            }
                            output.push('\n');
                        }
                    }
                }
            }
        }
    }

    // If no exception entry, try the message
    if output.is_empty() {
        if let Some(message) = event["message"].as_str() {
            output.push_str(&format!("## Message\n\n{}\n", message));
        } else if let Some(title) = event["title"].as_str() {
            output.push_str(&format!("## Error\n\n{}\n", title));
        }
    }

    output
}

/// Extract tags from a Sentry event.
pub fn extract_tags(event: &Value) -> Vec<(String, String)> {
    let mut tags = Vec::new();

    if let Some(tag_list) = event["tags"].as_array() {
        for tag in tag_list {
            if let (Some(key), Some(value)) = (tag["key"].as_str(), tag["value"].as_str()) {
                tags.push((key.to_string(), value.to_string()));
            }
        }
    }

    tags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_stacktrace_exception() {
        let event = serde_json::json!({
            "entries": [{
                "type": "exception",
                "data": {
                    "values": [{
                        "type": "NullPointerException",
                        "value": "Cannot read property 'foo' of null",
                        "stacktrace": {
                            "frames": [{
                                "filename": "app/Services/Foo.php",
                                "function": "doSomething",
                                "lineNo": 42,
                                "context": [
                                    [40, "    $bar = $this->bar;"],
                                    [41, "    // Process bar"],
                                    [42, "    return $bar->foo;"],
                                    [43, "}"]
                                ]
                            }]
                        }
                    }]
                }
            }]
        });

        let output = format_stacktrace(&event);
        assert!(output.contains("NullPointerException"));
        assert!(output.contains("Cannot read property 'foo' of null"));
        assert!(output.contains("doSomething"));
        assert!(output.contains("Foo.php:42"));
        assert!(output.contains("> "));
    }

    #[test]
    fn test_format_stacktrace_message_only() {
        let event = serde_json::json!({
            "message": "Something went wrong"
        });

        let output = format_stacktrace(&event);
        assert!(output.contains("Something went wrong"));
    }

    #[test]
    fn test_extract_tags() {
        let event = serde_json::json!({
            "tags": [
                {"key": "environment", "value": "production"},
                {"key": "browser", "value": "Chrome 120"}
            ]
        });

        let tags = extract_tags(&event);
        assert_eq!(tags.len(), 2);
        assert!(tags.contains(&("environment".into(), "production".into())));
    }
}
