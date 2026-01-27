//! Agent controller - main execution loop.

use async_trait::async_trait;
use tracing::{debug, error, info, warn};

use crate::event::{Action, Event, EventPayload, Observation, ReviewResult};
use crate::state::{AgentState, State};
use crate::stream::EventStream;
use crate::Error;

/// Maximum number of iterations before forcing termination.
const MAX_ITERATIONS: u32 = 100;

/// Trait for Claude Code integration.
#[async_trait]
pub trait ClaudeBackend: Send + Sync {
    /// Send a prompt and get response events.
    async fn prompt(&mut self, messages: &[Message]) -> Result<Vec<ClaudeResponse>, Error>;
}

/// A message in the conversation.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    System,
    User,
    Assistant,
}

/// Response from Claude.
#[derive(Debug, Clone)]
pub enum ClaudeResponse {
    /// Text content.
    Text(String),
    /// Tool use request.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Final result (agent finished).
    Result {
        subtype: String,
        result: Option<String>,
    },
    /// Token usage info.
    Usage { input_tokens: u64, output_tokens: u64 },
}

/// Trait for executing actions in the environment.
#[async_trait]
pub trait ActionExecutor: Send + Sync {
    /// Execute an action and return the observation.
    async fn execute(&self, action: &Action) -> Result<Observation, Error>;
}

/// The main agent controller.
pub struct AgentController<C, E> {
    pub state: State,
    pub stream: EventStream,
    claude: C,
    executor: E,
    system_prompt: String,
}

impl<C, E> AgentController<C, E>
where
    C: ClaudeBackend,
    E: ActionExecutor,
{
    pub fn new(claude: C, executor: E, system_prompt: impl Into<String>) -> Self {
        Self {
            state: State::new(),
            stream: EventStream::new(),
            claude,
            executor,
            system_prompt: system_prompt.into(),
        }
    }

    pub fn with_state(mut self, state: State) -> Self {
        self.state = state;
        self
    }

    /// Run the agent loop until completion.
    pub async fn run(&mut self, initial_prompt: &str) -> Result<ReviewResult, Error> {
        info!("Starting agent controller");
        self.state.set_running();

        // Add initial user message
        let user_event = Event::message("user", initial_prompt);
        self.state.add_event(user_event.clone());
        self.stream.add_event(user_event).await;

        let mut iterations = 0;

        while self.state.is_running() && iterations < MAX_ITERATIONS {
            iterations += 1;
            debug!(iteration = iterations, "Agent iteration");

            // Build messages from history
            let messages = self.build_messages();

            // Call Claude
            let responses = match self.claude.prompt(&messages).await {
                Ok(r) => r,
                Err(e) => {
                    error!(error = %e, "Claude API error");
                    self.state.set_error(format!("Claude API error: {e}"));
                    break;
                }
            };

            // Process responses
            for response in responses {
                match response {
                    ClaudeResponse::Text(text) => {
                        let event = Event::message("assistant", &text);
                        self.state.add_event(event.clone());
                        self.stream.add_event(event).await;
                    }

                    ClaudeResponse::ToolUse { id: _, name, input } => {
                        debug!(tool = %name, "Tool use requested");
                        self.state.set_waiting();
                        self.state.record_tool_call();

                        // Parse action from tool call
                        let action = match self.parse_action(&name, &input) {
                            Ok(a) => a,
                            Err(e) => {
                                warn!(error = %e, tool = %name, "Failed to parse action");
                                let obs = Observation::Error {
                                    message: format!("Invalid tool call: {e}"),
                                };
                                let event = Event::observation(obs);
                                self.state.add_event(event.clone());
                                self.stream.add_event(event).await;
                                continue;
                            }
                        };

                        // Check for finish action
                        if let Action::Finish { result } = action {
                            info!("Agent finished with result");
                            self.state.set_finished(result.clone());
                            return Ok(result);
                        }

                        // Record action
                        let action_event = Event::action(action.clone());
                        self.state.add_event(action_event.clone());
                        self.stream.add_event(action_event).await;

                        // Execute action
                        let observation = match self.executor.execute(&action).await {
                            Ok(obs) => obs,
                            Err(e) => {
                                error!(error = %e, "Action execution error");
                                Observation::Error {
                                    message: format!("Execution error: {e}"),
                                }
                            }
                        };

                        // Record observation
                        let obs_event = Event::observation(observation);
                        self.state.add_event(obs_event.clone());
                        self.stream.add_event(obs_event).await;

                        self.state.agent_state = AgentState::Running;
                    }

                    ClaudeResponse::Result { subtype, result } => {
                        info!(subtype = %subtype, "Claude returned result");
                        // If we get a result without explicit Finish action,
                        // try to parse it as a review result
                        if let Some(result_str) = result {
                            if let Ok(review_result) =
                                serde_json::from_str::<ReviewResult>(&result_str)
                            {
                                self.state.set_finished(review_result.clone());
                                return Ok(review_result);
                            }
                        }
                        // Otherwise, keep running - Claude might send more
                    }

                    ClaudeResponse::Usage {
                        input_tokens,
                        output_tokens,
                    } => {
                        self.state.record_api_call(input_tokens + output_tokens);
                    }
                }
            }
        }

        if iterations >= MAX_ITERATIONS {
            let err = format!("Max iterations ({MAX_ITERATIONS}) exceeded");
            error!("{}", err);
            self.state.set_error(&err);
            return Err(Error::MaxIterations);
        }

        // If we got here without a result, return error
        Err(Error::NoResult)
    }

    fn build_messages(&self) -> Vec<Message> {
        let mut messages = vec![Message {
            role: MessageRole::System,
            content: self.system_prompt.clone(),
        }];

        for event in &self.state.history {
            match &event.payload {
                EventPayload::Message { role, content } => {
                    let msg_role = match role.as_str() {
                        "user" => MessageRole::User,
                        "assistant" => MessageRole::Assistant,
                        _ => continue,
                    };
                    messages.push(Message {
                        role: msg_role,
                        content: content.clone(),
                    });
                }
                EventPayload::Action(action) => {
                    // Actions are sent to Claude as tool results
                    let content = format!("Tool call: {}", serde_json::to_string(action).unwrap());
                    messages.push(Message {
                        role: MessageRole::Assistant,
                        content,
                    });
                }
                EventPayload::Observation(obs) => {
                    // Observations are tool results
                    let content = serde_json::to_string(obs).unwrap();
                    messages.push(Message {
                        role: MessageRole::User,
                        content: format!("Tool result: {content}"),
                    });
                }
            }
        }

        messages
    }

    fn parse_action(&self, name: &str, input: &serde_json::Value) -> Result<Action, Error> {
        match name {
            "read_file" => {
                let path = input
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| Error::InvalidToolInput("missing path".into()))?;
                Ok(Action::ReadFile { path: path.into() })
            }
            "run_command" => {
                let cmd = input
                    .get("cmd")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| Error::InvalidToolInput("missing cmd".into()))?;
                Ok(Action::RunCommand { cmd: cmd.into() })
            }
            "post_comment" => {
                let body = input
                    .get("body")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| Error::InvalidToolInput("missing body".into()))?;
                Ok(Action::PostComment { body: body.into() })
            }
            "approve" => Ok(Action::Approve),
            "request_changes" => {
                let reason = input
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| Error::InvalidToolInput("missing reason".into()))?;
                Ok(Action::RequestChanges {
                    reason: reason.into(),
                })
            }
            "finish" => {
                let result: ReviewResult = serde_json::from_value(input.clone())
                    .map_err(|e| Error::InvalidToolInput(format!("invalid result: {e}")))?;
                Ok(Action::Finish { result })
            }
            _ => Err(Error::UnknownTool(name.into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockClaude {
        responses: Vec<Vec<ClaudeResponse>>,
        call_count: usize,
    }

    #[async_trait]
    impl ClaudeBackend for MockClaude {
        async fn prompt(&mut self, _messages: &[Message]) -> Result<Vec<ClaudeResponse>, Error> {
            if self.call_count < self.responses.len() {
                let resp = self.responses[self.call_count].clone();
                self.call_count += 1;
                Ok(resp)
            } else {
                Ok(vec![])
            }
        }
    }

    struct MockExecutor;

    #[async_trait]
    impl ActionExecutor for MockExecutor {
        async fn execute(&self, action: &Action) -> Result<Observation, Error> {
            match action {
                Action::ReadFile { path } => Ok(Observation::FileContent {
                    path: path.clone(),
                    content: "file content".into(),
                }),
                Action::Approve => Ok(Observation::Approved),
                _ => Ok(Observation::Error {
                    message: "not implemented".into(),
                }),
            }
        }
    }

    #[tokio::test]
    async fn test_parse_action() {
        let claude = MockClaude {
            responses: vec![],
            call_count: 0,
        };
        let executor = MockExecutor;
        let controller = AgentController::new(claude, executor, "test");

        let input = serde_json::json!({"path": "src/main.rs"});
        let action = controller.parse_action("read_file", &input).unwrap();
        assert!(matches!(action, Action::ReadFile { path } if path == "src/main.rs"));
    }
}
