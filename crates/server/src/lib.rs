//! Server components for Claude Agent.

pub mod github;
pub mod gitlab;
pub mod queue;
pub mod scheduler;
pub mod webhook;

pub use gitlab::{gitlab_auth_headers, MergeRequestEvent, ReviewPayload};
pub use queue::{FailedItem, Queue, QueueItem};
pub use scheduler::Scheduler;
pub use webhook::{router, AppState};
