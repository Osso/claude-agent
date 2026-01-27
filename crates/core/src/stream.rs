//! Event stream for agent communication.
//!
//! Provides pub/sub mechanism for events between agent and environment.

use std::collections::HashMap;
use tokio::sync::mpsc;

use crate::event::{Event, EventId};

/// A stream of events with pub/sub capability.
#[derive(Debug)]
pub struct EventStream {
    events: Vec<Event>,
    subscribers: HashMap<String, mpsc::Sender<Event>>,
}

impl Default for EventStream {
    fn default() -> Self {
        Self::new()
    }
}

impl EventStream {
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            subscribers: HashMap::new(),
        }
    }

    /// Add an event to the stream and notify subscribers.
    pub async fn add_event(&mut self, event: Event) -> EventId {
        let id = event.id;
        self.events.push(event.clone());

        // Notify all subscribers, removing any that have closed
        let mut closed = Vec::new();
        for (sub_id, sender) in &self.subscribers {
            if sender.send(event.clone()).await.is_err() {
                closed.push(sub_id.clone());
            }
        }
        for sub_id in closed {
            self.subscribers.remove(&sub_id);
        }

        id
    }

    /// Add an event synchronously (for non-async contexts).
    pub fn add_event_sync(&mut self, event: Event) -> EventId {
        let id = event.id;
        self.events.push(event);
        id
    }

    /// Subscribe to new events.
    pub fn subscribe(&mut self, id: impl Into<String>) -> mpsc::Receiver<Event> {
        let (tx, rx) = mpsc::channel(100);
        self.subscribers.insert(id.into(), tx);
        rx
    }

    /// Unsubscribe from events.
    pub fn unsubscribe(&mut self, id: &str) {
        self.subscribers.remove(id);
    }

    /// Get the full event history.
    pub fn history(&self) -> &[Event] {
        &self.events
    }

    /// Get events since a specific event ID.
    pub fn since(&self, id: EventId) -> &[Event] {
        if let Some(pos) = self.events.iter().position(|e| e.id == id) {
            &self.events[pos + 1..]
        } else {
            &[]
        }
    }

    /// Get the last N events.
    pub fn last_n(&self, n: usize) -> &[Event] {
        let len = self.events.len();
        if n >= len {
            &self.events
        } else {
            &self.events[len - n..]
        }
    }

    /// Clear all events.
    pub fn clear(&mut self) {
        self.events.clear();
    }

    /// Number of events in the stream.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Check if stream is empty.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Action, EventPayload};

    #[tokio::test]
    async fn test_add_and_retrieve_events() {
        let mut stream = EventStream::new();

        let event1 = Event::action(Action::ReadFile {
            path: "file1.rs".into(),
        });
        let event2 = Event::action(Action::ReadFile {
            path: "file2.rs".into(),
        });

        stream.add_event(event1).await;
        stream.add_event(event2).await;

        assert_eq!(stream.len(), 2);
        assert_eq!(stream.history().len(), 2);
    }

    #[tokio::test]
    async fn test_subscription() {
        let mut stream = EventStream::new();
        let mut rx = stream.subscribe("test");

        let event = Event::action(Action::Approve);
        stream.add_event(event.clone()).await;

        let received = rx.try_recv().unwrap();
        assert!(matches!(
            received.payload,
            EventPayload::Action(Action::Approve)
        ));
    }

    #[tokio::test]
    async fn test_since() {
        let mut stream = EventStream::new();

        let event1 = Event::action(Action::Approve);
        let id1 = stream.add_event(event1).await;

        let event2 = Event::action(Action::ReadFile {
            path: "test.rs".into(),
        });
        stream.add_event(event2).await;

        let since = stream.since(id1);
        assert_eq!(since.len(), 1);
    }
}
