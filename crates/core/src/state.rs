//! Agent state management.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::event::{Event, ReviewResult};

/// Current state of the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Agent is idle, waiting for work.
    Idle,
    /// Agent is running and processing.
    Running,
    /// Agent is waiting for a tool response.
    WaitingForTool,
    /// Agent has finished successfully.
    Finished,
    /// Agent encountered an error.
    Error,
}

impl Default for AgentState {
    fn default() -> Self {
        Self::Idle
    }
}

/// Context for a merge request review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewContext {
    /// GitLab/GitHub project URL or identifier.
    pub project: String,
    /// MR/PR identifier (iid for GitLab, number for GitHub).
    pub mr_id: String,
    /// Source branch name.
    pub source_branch: String,
    /// Target branch name.
    pub target_branch: String,
    /// The diff content.
    pub diff: String,
    /// List of changed file paths.
    pub changed_files: Vec<String>,
    /// MR title.
    pub title: String,
    /// MR description.
    pub description: Option<String>,
    /// Author username.
    pub author: String,
}

/// Metrics for agent execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metrics {
    /// When the agent started.
    pub started_at: Option<DateTime<Utc>>,
    /// When the agent finished.
    pub finished_at: Option<DateTime<Utc>>,
    /// Number of Claude API calls made.
    pub api_calls: u32,
    /// Total tokens used (input + output).
    pub total_tokens: u64,
    /// Number of tool calls executed.
    pub tool_calls: u32,
    /// Number of errors encountered.
    pub errors: u32,
}

impl Metrics {
    pub fn start(&mut self) {
        self.started_at = Some(Utc::now());
    }

    pub fn finish(&mut self) {
        self.finished_at = Some(Utc::now());
    }

    pub fn duration_secs(&self) -> Option<f64> {
        match (self.started_at, self.finished_at) {
            (Some(start), Some(end)) => Some((end - start).num_milliseconds() as f64 / 1000.0),
            _ => None,
        }
    }
}

/// Complete state of an agent session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    /// Current agent state.
    pub agent_state: AgentState,
    /// Event history.
    pub history: Vec<Event>,
    /// Review context (if reviewing an MR).
    pub context: Option<ReviewContext>,
    /// Execution metrics.
    pub metrics: Metrics,
    /// Final result (if finished).
    pub result: Option<ReviewResult>,
    /// Error message (if in error state).
    pub error: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        Self {
            agent_state: AgentState::Idle,
            history: Vec::new(),
            context: None,
            metrics: Metrics::default(),
            result: None,
            error: None,
        }
    }

    pub fn with_context(context: ReviewContext) -> Self {
        Self {
            context: Some(context),
            ..Self::new()
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(
            self.agent_state,
            AgentState::Running | AgentState::WaitingForTool
        )
    }

    pub fn is_finished(&self) -> bool {
        matches!(
            self.agent_state,
            AgentState::Finished | AgentState::Error
        )
    }

    pub fn set_running(&mut self) {
        self.agent_state = AgentState::Running;
        self.metrics.start();
    }

    pub fn set_waiting(&mut self) {
        self.agent_state = AgentState::WaitingForTool;
    }

    pub fn set_finished(&mut self, result: ReviewResult) {
        self.agent_state = AgentState::Finished;
        self.result = Some(result);
        self.metrics.finish();
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        self.agent_state = AgentState::Error;
        self.error = Some(message.into());
        self.metrics.errors += 1;
        self.metrics.finish();
    }

    pub fn add_event(&mut self, event: Event) {
        self.history.push(event);
    }

    pub fn record_api_call(&mut self, tokens: u64) {
        self.metrics.api_calls += 1;
        self.metrics.total_tokens += tokens;
    }

    pub fn record_tool_call(&mut self) {
        self.metrics.tool_calls += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_transitions() {
        let mut state = State::new();
        assert_eq!(state.agent_state, AgentState::Idle);
        assert!(!state.is_running());

        state.set_running();
        assert_eq!(state.agent_state, AgentState::Running);
        assert!(state.is_running());
        assert!(state.metrics.started_at.is_some());

        state.set_waiting();
        assert_eq!(state.agent_state, AgentState::WaitingForTool);
        assert!(state.is_running());

        state.set_error("test error");
        assert_eq!(state.agent_state, AgentState::Error);
        assert!(state.is_finished());
        assert_eq!(state.error.as_deref(), Some("test error"));
    }

    #[test]
    fn test_metrics() {
        let mut metrics = Metrics::default();
        metrics.start();
        std::thread::sleep(std::time::Duration::from_millis(10));
        metrics.finish();

        let duration = metrics.duration_secs().unwrap();
        assert!(duration >= 0.01);
    }
}
