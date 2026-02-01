//! Agent implementations for different tasks.

pub mod jira_handler;
pub mod mr_reviewer;
pub mod sentry_fixer;

pub use jira_handler::{JiraHandlerAgent, JiraTicketContext, JIRA_HANDLER_SYSTEM_PROMPT};
pub use mr_reviewer::{GitLabClient, MrReviewAgent, SYSTEM_PROMPT};
pub use sentry_fixer::{SentryFixContext, SentryFixerAgent, SENTRY_FIX_SYSTEM_PROMPT};
