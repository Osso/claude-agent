//! Agent implementations for different tasks.

pub mod jira_handler;
pub mod mr_reviewer;
pub mod sentry_fixer;

pub use jira_handler::{JIRA_HANDLER_SYSTEM_PROMPT, JiraHandlerAgent, JiraTicketContext};
pub use mr_reviewer::{MrReviewAgent, SYSTEM_PROMPT};
pub use sentry_fixer::{SENTRY_FIX_SYSTEM_PROMPT, SentryFixContext, SentryFixerAgent};
