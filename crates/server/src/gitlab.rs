//! GitLab webhook event parsing.

#![allow(dead_code)] // Deserialization structs have unused fields

use serde::{Deserialize, Serialize};

/// GitLab Merge Request webhook event.
#[derive(Debug, Clone, Deserialize)]
pub struct MergeRequestEvent {
    pub object_kind: String,
    pub event_type: Option<String>,
    pub user: User,
    pub project: Project,
    pub object_attributes: MergeRequestAttributes,
    pub labels: Option<Vec<Label>>,
    pub changes: Option<Changes>,
    pub repository: Option<Repository>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub username: String,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub path_with_namespace: String,
    pub web_url: String,
    pub git_http_url: Option<String>,
    pub git_ssh_url: Option<String>,
    pub default_branch: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MergeRequestAttributes {
    pub id: i64,
    pub iid: i64,
    pub title: String,
    pub description: Option<String>,
    pub source_branch: String,
    pub target_branch: String,
    pub state: String,
    pub action: Option<String>,
    pub draft: Option<bool>,
    pub work_in_progress: Option<bool>,
    pub url: String,
    pub author_id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    pub id: i64,
    pub title: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Changes {
    pub title: Option<Change>,
    pub description: Option<Change>,
    pub labels: Option<LabelsChange>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Change {
    pub previous: Option<String>,
    pub current: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LabelsChange {
    pub previous: Option<Vec<Label>>,
    pub current: Option<Vec<Label>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Repository {
    pub name: String,
    pub url: String,
    pub homepage: Option<String>,
}

impl MergeRequestEvent {
    /// Check if this event should trigger a review.
    pub fn should_review(&self) -> bool {
        // Only review merge requests
        if self.object_kind != "merge_request" {
            return false;
        }

        let attrs = &self.object_attributes;

        // Only review open MRs
        if attrs.state != "opened" && attrs.state != "reopened" {
            return false;
        }

        // Skip drafts/WIP
        if attrs.draft.unwrap_or(false) || attrs.work_in_progress.unwrap_or(false) {
            return false;
        }

        // Review on: open, update, reopen
        match attrs.action.as_deref() {
            Some("open") | Some("update") | Some("reopen") => true,
            _ => false,
        }
    }

    /// Check if MR has a specific label.
    pub fn has_label(&self, label_name: &str) -> bool {
        self.labels
            .as_ref()
            .map(|labels| labels.iter().any(|l| l.title == label_name))
            .unwrap_or(false)
    }

    /// Get the clone URL for the repository.
    pub fn clone_url(&self) -> Option<&str> {
        self.project
            .git_http_url
            .as_deref()
            .or(self.project.git_ssh_url.as_deref())
    }
}

/// Payload to queue for processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewPayload {
    pub gitlab_url: String,
    pub project: String,
    pub mr_iid: String,
    pub clone_url: String,
    pub source_branch: String,
    pub target_branch: String,
    pub title: String,
    pub description: Option<String>,
    pub author: String,
}

impl From<&MergeRequestEvent> for ReviewPayload {
    fn from(event: &MergeRequestEvent) -> Self {
        let gitlab_url = event
            .project
            .web_url
            .split('/')
            .take(3)
            .collect::<Vec<_>>()
            .join("/");

        Self {
            gitlab_url,
            project: event.project.path_with_namespace.clone(),
            mr_iid: event.object_attributes.iid.to_string(),
            clone_url: event.clone_url().unwrap_or_default().to_string(),
            source_branch: event.object_attributes.source_branch.clone(),
            target_branch: event.object_attributes.target_branch.clone(),
            title: event.object_attributes.title.clone(),
            description: event.object_attributes.description.clone(),
            author: event.user.username.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(action: &str, state: &str, draft: bool) -> MergeRequestEvent {
        MergeRequestEvent {
            object_kind: "merge_request".into(),
            event_type: Some("merge_request".into()),
            user: User {
                id: 1,
                name: "Test".into(),
                username: "test".into(),
                email: None,
            },
            project: Project {
                id: 1,
                name: "test".into(),
                path_with_namespace: "group/test".into(),
                web_url: "https://gitlab.com/group/test".into(),
                git_http_url: Some("https://gitlab.com/group/test.git".into()),
                git_ssh_url: None,
                default_branch: Some("main".into()),
            },
            object_attributes: MergeRequestAttributes {
                id: 1,
                iid: 123,
                title: "Test MR".into(),
                description: None,
                source_branch: "feature".into(),
                target_branch: "main".into(),
                state: state.into(),
                action: Some(action.into()),
                draft: Some(draft),
                work_in_progress: None,
                url: "https://gitlab.com/group/test/-/merge_requests/123".into(),
                author_id: 1,
            },
            labels: None,
            changes: None,
            repository: None,
        }
    }

    #[test]
    fn test_should_review_open() {
        let event = make_event("open", "opened", false);
        assert!(event.should_review());
    }

    #[test]
    fn test_should_not_review_draft() {
        let event = make_event("open", "opened", true);
        assert!(!event.should_review());
    }

    #[test]
    fn test_should_not_review_merged() {
        let event = make_event("merge", "merged", false);
        assert!(!event.should_review());
    }

    #[test]
    fn test_payload_from_event() {
        let event = make_event("open", "opened", false);
        let payload = ReviewPayload::from(&event);
        assert_eq!(payload.project, "group/test");
        assert_eq!(payload.mr_iid, "123");
        assert_eq!(payload.gitlab_url, "https://gitlab.com");
    }
}
