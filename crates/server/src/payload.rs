//! Unified job payload types for the queue.

use serde::{Deserialize, Serialize};

use crate::gitlab::ReviewPayload;

/// Unified job payload enum supporting all job types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum JobPayload {
    /// MR/PR review job (GitLab or GitHub)
    #[serde(rename = "review")]
    Review(ReviewPayload),

    /// Sentry issue fix job
    #[serde(rename = "sentry_fix")]
    SentryFix(SentryFixPayload),
}

impl JobPayload {
    /// Get a short description for logging.
    pub fn description(&self) -> String {
        match self {
            JobPayload::Review(p) => format!("review {}!{}", p.project, p.mr_iid),
            JobPayload::SentryFix(p) => format!("sentry-fix {}", p.short_id),
        }
    }

    /// Get project identifier.
    #[allow(dead_code)]
    pub fn project(&self) -> &str {
        match self {
            JobPayload::Review(p) => &p.project,
            JobPayload::SentryFix(p) => &p.vcs_project,
        }
    }

    /// Get the issue/MR ID for job naming.
    pub fn issue_id(&self) -> &str {
        match self {
            JobPayload::Review(p) => &p.mr_iid,
            JobPayload::SentryFix(p) => &p.short_id,
        }
    }

    /// Get job name prefix.
    pub fn job_prefix(&self) -> &str {
        match self {
            JobPayload::Review(_) => "claude-review",
            JobPayload::SentryFix(_) => "claude-sentry",
        }
    }
}

/// Payload for Sentry issue fix jobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentryFixPayload {
    /// Sentry issue ID (numeric)
    pub issue_id: String,
    /// Short ID (e.g., "WEB-123")
    pub short_id: String,
    /// Issue title
    pub title: String,
    /// File/function where error occurred
    pub culprit: String,
    /// Platform (e.g., "php", "python", "javascript")
    pub platform: String,
    /// Issue type (e.g., "error", "default")
    pub issue_type: String,
    /// Issue category (e.g., "error", "performance")
    pub issue_category: String,
    /// Web URL to the issue
    pub web_url: String,
    /// Sentry project slug
    pub project_slug: String,
    /// Sentry organization
    pub organization: String,
    /// Git clone URL
    pub clone_url: String,
    /// Target branch to base fix on (main/master)
    pub target_branch: String,
    /// VCS platform: "gitlab" or "github"
    pub vcs_platform: String,
    /// VCS project path (e.g., "Globalcomix/gc")
    pub vcs_project: String,
}

// Allow conversion from ReviewPayload for backwards compatibility
impl From<ReviewPayload> for JobPayload {
    fn from(payload: ReviewPayload) -> Self {
        JobPayload::Review(payload)
    }
}

impl From<SentryFixPayload> for JobPayload {
    fn from(payload: SentryFixPayload) -> Self {
        JobPayload::SentryFix(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_review_payload_serialization() {
        let payload = JobPayload::Review(ReviewPayload {
            gitlab_url: "https://gitlab.com".into(),
            project: "group/repo".into(),
            mr_iid: "123".into(),
            clone_url: "https://gitlab.com/group/repo.git".into(),
            source_branch: "feature".into(),
            target_branch: "main".into(),
            title: "Test MR".into(),
            description: None,
            author: "test".into(),
            action: "open".into(),
            platform: "gitlab".into(),
        });

        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains(r#""type":"review""#));

        let parsed: JobPayload = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, JobPayload::Review(_)));
    }

    #[test]
    fn test_sentry_fix_payload_serialization() {
        let payload = JobPayload::SentryFix(SentryFixPayload {
            issue_id: "12345".into(),
            short_id: "WEB-123".into(),
            title: "NullPointerException in foo()".into(),
            culprit: "app/Services/FooService.php".into(),
            platform: "php".into(),
            issue_type: "error".into(),
            issue_category: "error".into(),
            web_url: "https://sentry.io/issues/12345".into(),
            project_slug: "globalcomix-web".into(),
            organization: "globalcomix".into(),
            clone_url: "https://gitlab.com/Globalcomix/gc.git".into(),
            target_branch: "master".into(),
            vcs_platform: "gitlab".into(),
            vcs_project: "Globalcomix/gc".into(),
        });

        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains(r#""type":"sentry_fix""#));

        let parsed: JobPayload = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, JobPayload::SentryFix(_)));
    }

    #[test]
    fn test_job_payload_description() {
        let review = JobPayload::Review(ReviewPayload {
            gitlab_url: String::new(),
            project: "group/repo".into(),
            mr_iid: "42".into(),
            clone_url: String::new(),
            source_branch: String::new(),
            target_branch: String::new(),
            title: String::new(),
            description: None,
            author: String::new(),
            action: String::new(),
            platform: String::new(),
        });
        assert_eq!(review.description(), "review group/repo!42");

        let sentry = JobPayload::SentryFix(SentryFixPayload {
            issue_id: String::new(),
            short_id: "WEB-123".into(),
            title: String::new(),
            culprit: String::new(),
            platform: String::new(),
            issue_type: String::new(),
            issue_category: String::new(),
            web_url: String::new(),
            project_slug: String::new(),
            organization: String::new(),
            clone_url: String::new(),
            target_branch: String::new(),
            vcs_platform: String::new(),
            vcs_project: String::new(),
        });
        assert_eq!(sentry.description(), "sentry-fix WEB-123");
    }
}
