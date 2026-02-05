//! Jira webhook event parsing.
//!
//! Handles Jira Cloud webhooks for comment events where @claude-agent is mentioned.

use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

/// Bot mention trigger - looks for this text in comments.
pub const BOT_MENTION: &str = "@claude-agent";

/// Jira Cloud account ID for the claude-agent bot user.
/// When mentioned via @, Jira stores the account ID in ADF mention nodes
/// instead of the display name.
pub const BOT_ACCOUNT_ID: &str = "712020:8218f147-a7bd-4843-b5d3-0b2b01212bb2";

/// Jira webhook event from Jira Cloud.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JiraWebhookEvent {
    /// Webhook event type (e.g., "comment_created", "comment_updated")
    pub webhook_event: String,
    /// Issue data
    pub issue: JiraIssue,
    /// Comment data (present for comment events)
    pub comment: Option<JiraComment>,
    /// User who triggered the event
    pub user: Option<JiraUser>,
}

/// Jira issue data.
#[derive(Debug, Clone, Deserialize)]
pub struct JiraIssue {
    /// Issue ID (numeric)
    pub id: String,
    /// Issue key (e.g., "GC-123")
    pub key: String,
    /// API URL to the issue
    #[serde(rename = "self")]
    pub self_url: String,
    /// Issue fields
    pub fields: JiraIssueFields,
}

/// Jira issue fields.
#[derive(Debug, Clone, Deserialize)]
pub struct JiraIssueFields {
    /// Issue summary/title
    pub summary: String,
    /// Issue description (Atlassian Document Format or plain text)
    pub description: Option<serde_json::Value>,
    /// Issue type
    #[serde(rename = "issuetype")]
    pub issue_type: Option<JiraIssueType>,
    /// Project
    pub project: Option<JiraProject>,
    /// Priority
    pub priority: Option<JiraPriority>,
    /// Status
    pub status: Option<JiraStatus>,
    /// Reporter
    pub reporter: Option<JiraUser>,
    /// Assignee
    pub assignee: Option<JiraUser>,
    /// Labels
    #[serde(default)]
    pub labels: Vec<String>,
}

/// Jira issue type.
#[derive(Debug, Clone, Deserialize)]
pub struct JiraIssueType {
    pub name: String,
}

/// Jira project.
#[derive(Debug, Clone, Deserialize)]
pub struct JiraProject {
    pub key: String,
    pub name: String,
}

/// Jira priority.
#[derive(Debug, Clone, Deserialize)]
pub struct JiraPriority {
    pub name: String,
}

/// Jira status.
#[derive(Debug, Clone, Deserialize)]
pub struct JiraStatus {
    pub name: String,
}

/// Jira comment.
#[derive(Debug, Clone, Deserialize)]
pub struct JiraComment {
    /// Comment ID
    pub id: String,
    /// Comment body (Atlassian Document Format in Cloud, or plain text)
    pub body: serde_json::Value,
    /// Author of the comment
    pub author: Option<JiraUser>,
    /// When the comment was created
    pub created: Option<String>,
    /// When the comment was updated
    pub updated: Option<String>,
}

/// Jira user.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JiraUser {
    /// Account ID (Jira Cloud)
    pub account_id: Option<String>,
    /// Display name
    pub display_name: Option<String>,
    /// Email address (if available)
    pub email_address: Option<String>,
}

impl JiraWebhookEvent {
    /// Check if this event should trigger the bot.
    ///
    /// Returns true if:
    /// - Event is a comment creation/update
    /// - Comment contains @claude-agent mention
    pub fn should_trigger(&self) -> bool {
        // Only handle comment events
        if !self.webhook_event.starts_with("comment_") {
            return false;
        }

        // Check if comment mentions the bot
        if let Some(ref comment) = self.comment {
            return comment.mentions_bot();
        }

        false
    }

    /// Get the Jira instance base URL from the issue self URL.
    pub fn jira_base_url(&self) -> Option<String> {
        // self_url is like "https://globalcomix.atlassian.net/rest/api/3/issue/12345"
        self.issue
            .self_url
            .split("/rest/")
            .next()
            .map(String::from)
    }

    /// Get the web URL to the issue.
    pub fn issue_web_url(&self) -> String {
        if let Some(base) = self.jira_base_url() {
            format!("{}/browse/{}", base, self.issue.key)
        } else {
            format!("https://globalcomix.atlassian.net/browse/{}", self.issue.key)
        }
    }
}

impl JiraComment {
    /// Check if comment body contains the bot mention.
    ///
    /// Checks both the display name (@claude-agent) and the Jira account ID,
    /// since Jira Cloud ADF mention nodes store the account ID rather than
    /// the display name.
    pub fn mentions_bot(&self) -> bool {
        let text = self.body_as_text();
        text.to_lowercase().contains(&BOT_MENTION.to_lowercase())
            || text.contains(BOT_ACCOUNT_ID)
    }

    /// Extract plain text from comment body.
    ///
    /// Jira Cloud uses Atlassian Document Format (ADF) which is a JSON structure.
    /// This extracts the text content from it.
    pub fn body_as_text(&self) -> String {
        extract_text_from_adf(&self.body)
    }
}

/// Extract plain text from Atlassian Document Format (ADF).
///
/// ADF is a nested JSON structure with "content" arrays and "text" fields.
pub fn extract_text_from_adf(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(obj) => {
            let mut text = String::new();

            // Handle "text" field directly
            if let Some(serde_json::Value::String(t)) = obj.get("text") {
                text.push_str(t);
            }

            // Handle mention nodes - they have "attrs" with "text" for display name
            if obj.get("type").and_then(|v| v.as_str()) == Some("mention") {
                if let Some(attrs) = obj.get("attrs") {
                    if let Some(mention_text) = attrs.get("text").and_then(|v| v.as_str()) {
                        text.push_str(mention_text);
                    }
                }
            }

            // Recurse into "content" array
            if let Some(serde_json::Value::Array(content)) = obj.get("content") {
                for item in content {
                    text.push_str(&extract_text_from_adf(item));
                }
            }

            text
        }
        serde_json::Value::Array(arr) => arr.iter().map(extract_text_from_adf).collect(),
        _ => String::new(),
    }
}

/// Verify Jira webhook signature.
///
/// Jira Cloud webhooks can be configured with a secret for HMAC-SHA256 verification.
pub fn verify_signature(secret: &str, body: &[u8], signature: &str) -> bool {
    // Jira sends signature as "sha256=<hex>"
    let expected = match signature.strip_prefix("sha256=") {
        Some(hex) => hex,
        None => return false,
    };

    let mut mac = match Hmac::<Sha256>::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };

    mac.update(body);
    let result = mac.finalize();
    let computed = hex::encode(result.into_bytes());

    // Constant-time comparison
    computed.len() == expected.len()
        && computed
            .bytes()
            .zip(expected.bytes())
            .all(|(a, b)| a == b)
}

/// Jira project mapping configuration.
///
/// Maps Jira projects to VCS repositories.
#[derive(Debug, Clone, Deserialize)]
pub struct JiraProjectMapping {
    /// Jira project key (e.g., "GC")
    pub jira_project: String,
    /// Git clone URL
    pub clone_url: String,
    /// VCS platform: "gitlab" or "github"
    pub vcs_platform: String,
    /// VCS project path (e.g., "Globalcomix/gc")
    pub vcs_project: String,
    /// Target branch for fixes (e.g., "master")
    pub target_branch: String,
}

/// Parse Jira project mappings from JSON string.
pub fn parse_project_mappings(json: &str) -> Result<Vec<JiraProjectMapping>, serde_json::Error> {
    serde_json::from_str(json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_text_from_adf_simple() {
        let adf = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "paragraph",
                    "content": [
                        {
                            "type": "text",
                            "text": "Hello "
                        },
                        {
                            "type": "mention",
                            "attrs": {
                                "id": "123",
                                "text": "@claude-agent"
                            }
                        },
                        {
                            "type": "text",
                            "text": " please fix this"
                        }
                    ]
                }
            ]
        });

        let text = extract_text_from_adf(&adf);
        assert!(text.contains("Hello"));
        assert!(text.contains("@claude-agent"));
        assert!(text.contains("please fix this"));
    }

    #[test]
    fn test_extract_text_from_plain_string() {
        let plain = serde_json::json!("@claude-agent please help");
        let text = extract_text_from_adf(&plain);
        assert_eq!(text, "@claude-agent please help");
    }

    #[test]
    fn test_comment_mentions_bot() {
        let comment = JiraComment {
            id: "123".into(),
            body: serde_json::json!({
                "type": "doc",
                "content": [{
                    "type": "paragraph",
                    "content": [{
                        "type": "mention",
                        "attrs": {"text": "@claude-agent"}
                    }]
                }]
            }),
            author: None,
            created: None,
            updated: None,
        };

        assert!(comment.mentions_bot());
    }

    #[test]
    fn test_comment_mentions_bot_by_account_id() {
        let comment = JiraComment {
            id: "123".into(),
            body: serde_json::json!({
                "type": "doc",
                "content": [{
                    "type": "paragraph",
                    "content": [{
                        "type": "mention",
                        "attrs": {
                            "id": "712020:8218f147-a7bd-4843-b5d3-0b2b01212bb2",
                            "text": "~accountid:712020:8218f147-a7bd-4843-b5d3-0b2b01212bb2"
                        }
                    }]
                }]
            }),
            author: None,
            created: None,
            updated: None,
        };

        assert!(comment.mentions_bot());
    }

    #[test]
    fn test_comment_no_mention() {
        let comment = JiraComment {
            id: "123".into(),
            body: serde_json::json!({
                "type": "doc",
                "content": [{
                    "type": "paragraph",
                    "content": [{
                        "type": "text",
                        "text": "Just a regular comment"
                    }]
                }]
            }),
            author: None,
            created: None,
            updated: None,
        };

        assert!(!comment.mentions_bot());
    }

    #[test]
    fn test_should_trigger() {
        let event = JiraWebhookEvent {
            webhook_event: "comment_created".into(),
            issue: JiraIssue {
                id: "12345".into(),
                key: "GC-100".into(),
                self_url: "https://globalcomix.atlassian.net/rest/api/3/issue/12345".into(),
                fields: JiraIssueFields {
                    summary: "Test issue".into(),
                    description: None,
                    issue_type: None,
                    project: None,
                    priority: None,
                    status: None,
                    reporter: None,
                    assignee: None,
                    labels: vec![],
                },
            },
            comment: Some(JiraComment {
                id: "456".into(),
                body: serde_json::json!("@claude-agent fix this"),
                author: None,
                created: None,
                updated: None,
            }),
            user: None,
        };

        assert!(event.should_trigger());
    }

    #[test]
    fn test_should_not_trigger_no_mention() {
        let event = JiraWebhookEvent {
            webhook_event: "comment_created".into(),
            issue: JiraIssue {
                id: "12345".into(),
                key: "GC-100".into(),
                self_url: "https://globalcomix.atlassian.net/rest/api/3/issue/12345".into(),
                fields: JiraIssueFields {
                    summary: "Test issue".into(),
                    description: None,
                    issue_type: None,
                    project: None,
                    priority: None,
                    status: None,
                    reporter: None,
                    assignee: None,
                    labels: vec![],
                },
            },
            comment: Some(JiraComment {
                id: "456".into(),
                body: serde_json::json!("Just a comment"),
                author: None,
                created: None,
                updated: None,
            }),
            user: None,
        };

        assert!(!event.should_trigger());
    }

    #[test]
    fn test_should_not_trigger_issue_event() {
        let event = JiraWebhookEvent {
            webhook_event: "issue_updated".into(),
            issue: JiraIssue {
                id: "12345".into(),
                key: "GC-100".into(),
                self_url: "https://globalcomix.atlassian.net/rest/api/3/issue/12345".into(),
                fields: JiraIssueFields {
                    summary: "Test issue".into(),
                    description: None,
                    issue_type: None,
                    project: None,
                    priority: None,
                    status: None,
                    reporter: None,
                    assignee: None,
                    labels: vec![],
                },
            },
            comment: None,
            user: None,
        };

        assert!(!event.should_trigger());
    }

    #[test]
    fn test_jira_base_url() {
        let event = JiraWebhookEvent {
            webhook_event: "comment_created".into(),
            issue: JiraIssue {
                id: "12345".into(),
                key: "GC-100".into(),
                self_url: "https://globalcomix.atlassian.net/rest/api/3/issue/12345".into(),
                fields: JiraIssueFields {
                    summary: "Test".into(),
                    description: None,
                    issue_type: None,
                    project: None,
                    priority: None,
                    status: None,
                    reporter: None,
                    assignee: None,
                    labels: vec![],
                },
            },
            comment: None,
            user: None,
        };

        assert_eq!(
            event.jira_base_url(),
            Some("https://globalcomix.atlassian.net".into())
        );
    }

    #[test]
    fn test_verify_signature() {
        let secret = "test-secret";
        let body = b"test body";

        // Compute expected signature
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let result = mac.finalize();
        let signature = format!("sha256={}", hex::encode(result.into_bytes()));

        assert!(verify_signature(secret, body, &signature));
        assert!(!verify_signature(secret, body, "sha256=invalid"));
        assert!(!verify_signature(secret, body, "invalid"));
    }

    #[test]
    fn test_parse_project_mappings() {
        let json = r#"[
            {
                "jira_project": "GC",
                "clone_url": "https://gitlab.com/Globalcomix/gc.git",
                "vcs_platform": "gitlab",
                "vcs_project": "Globalcomix/gc",
                "target_branch": "master"
            }
        ]"#;

        let mappings = parse_project_mappings(json).unwrap();
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].jira_project, "GC");
        assert_eq!(mappings[0].vcs_platform, "gitlab");
    }
}
