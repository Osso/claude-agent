//! Core agent loop and types for Claude Code agentic system.

pub mod controller;
pub mod event;
pub mod state;
pub mod stream;

pub use controller::{
    ActionExecutor, AgentController, ClaudeBackend, ClaudeResponse, Message, MessageRole,
};
pub use event::{Action, Event, EventId, EventPayload, Observation, ReviewDecision, ReviewResult};
pub use state::{AgentState, Metrics, ReviewContext, State};
pub use stream::EventStream;

/// Error types for the core crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Claude API error: {0}")]
    ClaudeApi(String),

    #[error("Invalid tool input: {0}")]
    InvalidToolInput(String),

    #[error("Unknown tool: {0}")]
    UnknownTool(String),

    #[error("Max iterations exceeded")]
    MaxIterations,

    #[error("Agent finished without result")]
    NoResult,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}
