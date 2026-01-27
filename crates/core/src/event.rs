//! Event types for agent communication.
//!
//! Events flow between the agent and environment:
//! - Actions: Agent initiates (read file, run command, post comment)
//! - Observations: Environment responds (file content, command output)

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventId(Uuid);

impl EventId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for EventId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for EventId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Actions that the agent can initiate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// Read a file from the repository.
    ReadFile { path: String },

    /// Run a shell command.
    RunCommand { cmd: String },

    /// Post a comment on the MR.
    PostComment { body: String },

    /// Approve the MR.
    Approve,

    /// Request changes on the MR.
    RequestChanges { reason: String },

    /// Mark review as finished.
    Finish { result: ReviewResult },
}

/// Result of a code review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewResult {
    pub decision: ReviewDecision,
    pub summary: String,
    pub issues: Vec<ReviewIssue>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    Comment,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewIssue {
    pub severity: IssueSeverity,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueSeverity {
    Error,
    Warning,
    Info,
}

/// Observations from the environment in response to actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Observation {
    /// Content of a file that was read.
    FileContent { path: String, content: String },

    /// File not found.
    FileNotFound { path: String },

    /// Output from a command execution.
    CommandOutput {
        stdout: String,
        stderr: String,
        exit_code: i32,
    },

    /// Comment was posted successfully.
    CommentPosted { comment_id: String },

    /// MR was approved.
    Approved,

    /// Changes were requested on MR.
    ChangesRequested,

    /// An error occurred.
    Error { message: String },
}

/// A timestamped event in the agent's history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    pub timestamp: DateTime<Utc>,
    pub payload: EventPayload,
}

impl Event {
    pub fn new(payload: EventPayload) -> Self {
        Self {
            id: EventId::new(),
            timestamp: Utc::now(),
            payload,
        }
    }

    pub fn action(action: Action) -> Self {
        Self::new(EventPayload::Action(action))
    }

    pub fn observation(observation: Observation) -> Self {
        Self::new(EventPayload::Observation(observation))
    }

    pub fn message(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self::new(EventPayload::Message {
            role: role.into(),
            content: content.into(),
        })
    }
}

/// The payload of an event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventPayload {
    /// An action initiated by the agent.
    Action(Action),

    /// An observation from the environment.
    Observation(Observation),

    /// A message (user or assistant).
    Message { role: String, content: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_serialization() {
        let event = Event::action(Action::ReadFile {
            path: "src/main.rs".into(),
        });
        let json = serde_json::to_string(&event).unwrap();
        let parsed: Event = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed.payload,
            EventPayload::Action(Action::ReadFile { .. })
        ));
    }

    #[test]
    fn test_review_result_serialization() {
        let result = ReviewResult {
            decision: ReviewDecision::ChangesRequested,
            summary: "Found issues".into(),
            issues: vec![ReviewIssue {
                severity: IssueSeverity::Error,
                file: Some("src/lib.rs".into()),
                line: Some(42),
                message: "Unused variable".into(),
            }],
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("changes_requested"));
    }
}
