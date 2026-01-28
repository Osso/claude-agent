//! GitHub webhook event parsing.

#![allow(dead_code)] // Deserialization structs have unused fields

use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

use crate::gitlab::ReviewPayload;

type HmacSha256 = Hmac<Sha256>;

/// GitHub pull_request webhook event.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestEvent {
    pub action: String,
    pub number: u64,
    pub pull_request: PullRequest,
    pub repository: Repository,
    pub sender: User,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PullRequest {
    pub id: u64,
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub draft: Option<bool>,
    pub user: User,
    pub head: GitRef,
    pub base: GitRef,
    pub html_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
    pub repo: Option<RefRepo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RefRepo {
    pub full_name: String,
    pub clone_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Repository {
    pub id: u64,
    pub name: String,
    pub full_name: String,
    pub clone_url: String,
    pub html_url: String,
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: u64,
    pub login: String,
}

impl PullRequestEvent {
    /// Check if this event should trigger a review.
    pub fn should_review(&self) -> bool {
        // Only review on opened, synchronize (new push), or reopened
        if !matches!(self.action.as_str(), "opened" | "synchronize" | "reopened") {
            return false;
        }

        // Skip drafts
        if self.pull_request.draft.unwrap_or(false) {
            return false;
        }

        true
    }

    /// Map GitHub action to our internal action name.
    fn review_action(&self) -> &str {
        match self.action.as_str() {
            "opened" => "open",
            "reopened" => "reopen",
            "synchronize" => "update",
            other => other,
        }
    }
}

impl From<&PullRequestEvent> for ReviewPayload {
    fn from(event: &PullRequestEvent) -> Self {
        Self {
            gitlab_url: String::new(), // Not used for GitHub
            project: event.repository.full_name.clone(),
            mr_iid: event.pull_request.number.to_string(),
            clone_url: event.repository.clone_url.clone(),
            source_branch: event.pull_request.head.ref_name.clone(),
            target_branch: event.pull_request.base.ref_name.clone(),
            title: event.pull_request.title.clone(),
            description: event.pull_request.body.clone(),
            author: event.pull_request.user.login.clone(),
            action: event.review_action().to_string(),
            platform: "github".into(),
        }
    }
}

/// Verify GitHub HMAC-SHA256 webhook signature.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(action: &str, draft: bool) -> PullRequestEvent {
        PullRequestEvent {
            action: action.into(),
            number: 42,
            pull_request: PullRequest {
                id: 1,
                number: 42,
                title: "Test PR".into(),
                body: Some("Description".into()),
                state: "open".into(),
                draft: Some(draft),
                user: User {
                    id: 1,
                    login: "testuser".into(),
                },
                head: GitRef {
                    ref_name: "feature-branch".into(),
                    sha: "abc123".into(),
                    repo: None,
                },
                base: GitRef {
                    ref_name: "main".into(),
                    sha: "def456".into(),
                    repo: None,
                },
                html_url: "https://github.com/owner/repo/pull/42".into(),
            },
            repository: Repository {
                id: 1,
                name: "repo".into(),
                full_name: "owner/repo".into(),
                clone_url: "https://github.com/owner/repo.git".into(),
                html_url: "https://github.com/owner/repo".into(),
                default_branch: Some("main".into()),
            },
            sender: User {
                id: 1,
                login: "testuser".into(),
            },
        }
    }

    #[test]
    fn test_should_review_opened() {
        assert!(make_event("opened", false).should_review());
    }

    #[test]
    fn test_should_review_synchronize() {
        assert!(make_event("synchronize", false).should_review());
    }

    #[test]
    fn test_should_review_reopened() {
        assert!(make_event("reopened", false).should_review());
    }

    #[test]
    fn test_should_not_review_closed() {
        assert!(!make_event("closed", false).should_review());
    }

    #[test]
    fn test_should_not_review_draft() {
        assert!(!make_event("opened", true).should_review());
    }

    #[test]
    fn test_payload_from_event() {
        let event = make_event("opened", false);
        let payload = ReviewPayload::from(&event);
        assert_eq!(payload.project, "owner/repo");
        assert_eq!(payload.mr_iid, "42");
        assert_eq!(payload.platform, "github");
        assert_eq!(payload.action, "open");
        assert_eq!(payload.source_branch, "feature-branch");
        assert_eq!(payload.target_branch, "main");
    }

    #[test]
    fn test_synchronize_maps_to_update() {
        let event = make_event("synchronize", false);
        let payload = ReviewPayload::from(&event);
        assert_eq!(payload.action, "update");
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
}
