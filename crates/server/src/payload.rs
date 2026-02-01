//! Unified job payload types for the queue.

use serde::{Deserialize, Deserializer, Serialize};

use crate::gitlab::ReviewPayload;

/// Unified job payload enum supporting all job types.
///
/// Serializes with a `"type"` tag to distinguish variants.
/// Deserializes with backward compatibility for legacy payloads that don't have the tag.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum JobPayload {
    /// MR/PR review job (GitLab or GitHub)
    #[serde(rename = "review")]
    Review(ReviewPayload),

    /// Sentry issue fix job
    #[serde(rename = "sentry_fix")]
    SentryFix(SentryFixPayload),

    /// Jira ticket fix job
    #[serde(rename = "jira_ticket")]
    JiraTicket(JiraTicketPayload),
}

impl<'de> Deserialize<'de> for JobPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Deserialize to raw Value first so we can try multiple formats
        let value = serde_json::Value::deserialize(deserializer)?;

        // Try tagged format first (has "type" field)
        if value.get("type").is_some() {
            #[derive(Deserialize)]
            #[serde(tag = "type")]
            enum Tagged {
                #[serde(rename = "review")]
                Review(ReviewPayload),
                #[serde(rename = "sentry_fix")]
                SentryFix(SentryFixPayload),
                #[serde(rename = "jira_ticket")]
                JiraTicket(JiraTicketPayload),
            }

            return match serde_json::from_value::<Tagged>(value) {
                Ok(Tagged::Review(p)) => Ok(JobPayload::Review(p)),
                Ok(Tagged::SentryFix(p)) => Ok(JobPayload::SentryFix(p)),
                Ok(Tagged::JiraTicket(p)) => Ok(JobPayload::JiraTicket(p)),
                Err(e) => Err(serde::de::Error::custom(e)),
            };
        }

        // Fall back to legacy ReviewPayload format (no type tag)
        serde_json::from_value::<ReviewPayload>(value)
            .map(JobPayload::Review)
            .map_err(serde::de::Error::custom)
    }
}

impl JobPayload {
    /// Get a short description for logging.
    pub fn description(&self) -> String {
        match self {
            JobPayload::Review(p) => format!("review {}!{}", p.project, p.mr_iid),
            JobPayload::SentryFix(p) => format!("sentry-fix {}", p.short_id),
            JobPayload::JiraTicket(p) => format!("jira-fix {}", p.issue_key),
        }
    }

    /// Get project identifier.
    #[allow(dead_code)]
    pub fn project(&self) -> &str {
        match self {
            JobPayload::Review(p) => &p.project,
            JobPayload::SentryFix(p) => &p.vcs_project,
            JobPayload::JiraTicket(p) => &p.vcs_project,
        }
    }

    /// Get the issue/MR ID for job naming.
    pub fn issue_id(&self) -> &str {
        match self {
            JobPayload::Review(p) => &p.mr_iid,
            JobPayload::SentryFix(p) => &p.short_id,
            JobPayload::JiraTicket(p) => &p.issue_key,
        }
    }

    /// Get job name prefix.
    pub fn job_prefix(&self) -> &str {
        match self {
            JobPayload::Review(_) => "claude-review",
            JobPayload::SentryFix(_) => "claude-sentry",
            JobPayload::JiraTicket(_) => "claude-jira",
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

/// Payload for Jira ticket fix jobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JiraTicketPayload {
    /// Jira issue key (e.g., "GC-123")
    pub issue_key: String,
    /// Jira issue ID (numeric)
    pub issue_id: String,
    /// Issue summary/title
    pub summary: String,
    /// Issue description (plain text)
    pub description: Option<String>,
    /// Issue type (e.g., "Bug", "Task", "Story")
    pub issue_type: String,
    /// Priority (e.g., "High", "Medium", "Low")
    pub priority: Option<String>,
    /// Current status
    pub status: String,
    /// Labels on the issue
    pub labels: Vec<String>,
    /// Web URL to the Jira issue
    pub web_url: String,
    /// Jira base URL (e.g., "https://globalcomix.atlassian.net")
    pub jira_base_url: String,
    /// The comment that triggered this job (with @claude-agent mention)
    pub trigger_comment: String,
    /// Author of the trigger comment
    pub trigger_author: Option<String>,
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

impl From<JiraTicketPayload> for JobPayload {
    fn from(payload: JiraTicketPayload) -> Self {
        JobPayload::JiraTicket(payload)
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
    fn test_legacy_review_payload_deserialization() {
        // Legacy format without "type" tag
        let json = r#"{
            "gitlab_url": "https://gitlab.com",
            "project": "Globalcomix/gc",
            "mr_iid": "2604",
            "clone_url": "https://gitlab.com/Globalcomix/gc.git",
            "source_branch": "feature",
            "target_branch": "master",
            "title": "Test MR",
            "author": "test",
            "action": "open",
            "platform": "gitlab"
        }"#;

        let parsed: JobPayload = serde_json::from_str(json).unwrap();
        assert!(matches!(parsed, JobPayload::Review(_)));
        if let JobPayload::Review(p) = parsed {
            assert_eq!(p.project, "Globalcomix/gc");
            assert_eq!(p.mr_iid, "2604");
        }
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
