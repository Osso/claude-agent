//! Sentry webhook event parsing.

#![allow(dead_code)] // Deserialization structs have unused fields

use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Sentry webhook event (for issue alerts).
#[derive(Debug, Clone, Deserialize)]
pub struct SentryWebhookEvent {
    /// Action type: "created", "resolved", "assigned", "archived", "unresolved"
    pub action: String,
    /// Sentry installation info
    pub installation: Installation,
    /// Event data containing the issue
    pub data: IssueData,
    /// Actor who triggered the event
    pub actor: Actor,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Installation {
    pub uuid: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IssueData {
    pub issue: Issue,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Issue {
    /// Numeric issue ID
    pub id: String,
    /// Short ID (e.g., "WEB-123")
    #[serde(rename = "shortId")]
    pub short_id: String,
    /// Issue title (error message)
    pub title: String,
    /// File/function where error occurred
    pub culprit: String,
    /// Platform (e.g., "php", "python", "javascript")
    pub platform: String,
    /// Issue status
    pub status: String,
    /// Issue substatus
    pub substatus: Option<String>,
    /// Issue type (e.g., "error", "default")
    #[serde(rename = "type")]
    pub issue_type: Option<String>,
    /// Issue category (e.g., "error", "performance")
    #[serde(rename = "issueCategory")]
    pub issue_category: Option<String>,
    /// First seen timestamp
    #[serde(rename = "firstSeen")]
    pub first_seen: String,
    /// Last seen timestamp
    #[serde(rename = "lastSeen")]
    pub last_seen: String,
    /// Web URL to view the issue
    #[serde(rename = "webUrl")]
    pub web_url: Option<String>,
    /// Project info
    pub project: SentryProject,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SentryProject {
    /// Project ID
    pub id: String,
    /// Project slug
    pub slug: String,
    /// Project name
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Actor {
    #[serde(rename = "type")]
    pub actor_type: String,
    pub id: Option<String>,
    pub name: Option<String>,
}

impl SentryWebhookEvent {
    /// Check if this event should trigger a fix job.
    pub fn should_fix(&self) -> bool {
        // Only fix on new issues or regressions (unresolved)
        match self.action.as_str() {
            "created" | "unresolved" => {}
            _ => return false,
        }

        // Skip certain issue categories we can't fix
        if let Some(category) = &self.data.issue.issue_category {
            match category.as_str() {
                // Skip performance issues, outages, etc.
                "performance" | "cron" | "replay" | "feedback" | "uptime" => return false,
                _ => {}
            }
        }

        true
    }

    /// Get the issue.
    pub fn issue(&self) -> &Issue {
        &self.data.issue
    }
}

/// Verify Sentry HMAC-SHA256 webhook signature.
///
/// Sentry uses the format: `sha256=<hex-signature>`
pub fn verify_signature(secret: &str, body: &[u8], signature: &str) -> bool {
    let sig_hex = match signature.strip_prefix("sha256=") {
        Some(hex) => hex,
        None => return false,
    };

    let expected = match hex::decode(sig_hex) {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };

    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => return false,
    };

    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

/// Configuration mapping Sentry projects to Git repositories.
#[derive(Debug, Clone, Deserialize)]
pub struct SentryProjectMapping {
    /// Sentry project slug (e.g., "globalcomix-web")
    pub sentry_project: String,
    /// Git clone URL
    pub clone_url: String,
    /// VCS platform: "gitlab" or "github"
    pub vcs_platform: String,
    /// VCS project path (e.g., "Globalcomix/gc")
    pub vcs_project: String,
    /// Target branch to base fixes on
    pub target_branch: String,
}

/// Parse project mappings from environment variable.
///
/// Expected format: JSON array of SentryProjectMapping objects.
pub fn parse_project_mappings(json: &str) -> Result<Vec<SentryProjectMapping>, serde_json::Error> {
    serde_json::from_str(json)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(action: &str, issue_category: Option<&str>) -> SentryWebhookEvent {
        SentryWebhookEvent {
            action: action.into(),
            installation: Installation {
                uuid: "test-uuid".into(),
            },
            data: IssueData {
                issue: Issue {
                    id: "12345".into(),
                    short_id: "WEB-123".into(),
                    title: "NullPointerException".into(),
                    culprit: "app/Services/FooService.php".into(),
                    platform: "php".into(),
                    status: "unresolved".into(),
                    substatus: None,
                    issue_type: Some("error".into()),
                    issue_category: issue_category.map(String::from),
                    first_seen: "2025-01-01T00:00:00Z".into(),
                    last_seen: "2025-01-01T00:00:00Z".into(),
                    web_url: Some("https://sentry.io/issues/12345".into()),
                    project: SentryProject {
                        id: "1".into(),
                        slug: "globalcomix-web".into(),
                        name: "GlobalComix Web".into(),
                    },
                },
            },
            actor: Actor {
                actor_type: "application".into(),
                id: None,
                name: None,
            },
        }
    }

    #[test]
    fn test_should_fix_created() {
        let event = make_event("created", Some("error"));
        assert!(event.should_fix());
    }

    #[test]
    fn test_should_fix_unresolved() {
        let event = make_event("unresolved", Some("error"));
        assert!(event.should_fix());
    }

    #[test]
    fn test_should_not_fix_resolved() {
        let event = make_event("resolved", Some("error"));
        assert!(!event.should_fix());
    }

    #[test]
    fn test_should_not_fix_assigned() {
        let event = make_event("assigned", Some("error"));
        assert!(!event.should_fix());
    }

    #[test]
    fn test_should_not_fix_performance() {
        let event = make_event("created", Some("performance"));
        assert!(!event.should_fix());
    }

    #[test]
    fn test_should_not_fix_cron() {
        let event = make_event("created", Some("cron"));
        assert!(!event.should_fix());
    }

    #[test]
    fn test_verify_signature_valid() {
        let secret = "test-secret";
        let body = b"hello world";

        // Compute expected signature
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let result = mac.finalize();
        let sig = format!("sha256={}", hex::encode(result.into_bytes()));

        assert!(verify_signature(secret, body, &sig));
    }

    #[test]
    fn test_verify_signature_invalid() {
        assert!(!verify_signature("secret", b"body", "sha256=0000"));
    }

    #[test]
    fn test_verify_signature_missing_prefix() {
        assert!(!verify_signature("secret", b"body", "bad-format"));
    }

    #[test]
    fn test_parse_project_mappings() {
        let json = r#"[
            {
                "sentry_project": "globalcomix-web",
                "clone_url": "https://gitlab.com/Globalcomix/gc.git",
                "vcs_platform": "gitlab",
                "vcs_project": "Globalcomix/gc",
                "target_branch": "master"
            }
        ]"#;

        let mappings = parse_project_mappings(json).unwrap();
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].sentry_project, "globalcomix-web");
        assert_eq!(mappings[0].vcs_platform, "gitlab");
    }

    #[test]
    fn test_deserialize_webhook() {
        let json = r#"{
            "action": "created",
            "installation": {"uuid": "abc123"},
            "data": {
                "issue": {
                    "id": "12345",
                    "shortId": "WEB-123",
                    "title": "Error in foo()",
                    "culprit": "app/foo.php",
                    "platform": "php",
                    "status": "unresolved",
                    "type": "error",
                    "issueCategory": "error",
                    "firstSeen": "2025-01-01T00:00:00Z",
                    "lastSeen": "2025-01-01T00:00:00Z",
                    "webUrl": "https://sentry.io/issues/12345",
                    "project": {
                        "id": "1",
                        "slug": "web",
                        "name": "Web"
                    }
                }
            },
            "actor": {
                "type": "application"
            }
        }"#;

        let event: SentryWebhookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.action, "created");
        assert_eq!(event.data.issue.short_id, "WEB-123");
    }
}
