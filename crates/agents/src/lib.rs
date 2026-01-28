//! Agent implementations for different tasks.

pub mod mr_reviewer;

pub use mr_reviewer::{GitLabClient, MrReviewAgent, SYSTEM_PROMPT};
