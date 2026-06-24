//! Agent controller - main execution loop.

use async_trait::async_trait;
use tracing::{debug, error, info, warn};

use crate::Error;
use crate::event::{Action, Event, EventPayload, Observation, ReviewResult};
use crate::state::{AgentState, State};
use crate::stream::EventStream;

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
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
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

        let user_event = Event::message("user", initial_prompt);
        self.state.add_event(user_event.clone());
        self.stream.add_event(user_event).await;

        let mut iterations = 0;

        while self.state.is_running() && iterations < MAX_ITERATIONS {
            iterations += 1;
            debug!(iteration = iterations, "Agent iteration");

            let messages = self.build_messages();
            let responses = match self.claude.prompt(&messages).await {
                Ok(r) => r,
                Err(e) => {
                    error!(error = %e, "Claude API error");
                    self.state.set_error(format!("Claude API error: {e}"));
                    break;
                }
            };

            if let Some(result) = self.process_responses(responses).await? {
                return Ok(result);
            }
        }

        if iterations >= MAX_ITERATIONS {
            let err = format!("Max iterations ({MAX_ITERATIONS}) exceeded");
            error!("{}", err);
            self.state.set_error(&err);
            return Err(Error::MaxIterations);
        }

        Err(Error::NoResult)
    }

    /// Process a batch of Claude responses, returning a result if the agent finished.
    async fn process_responses(
        &mut self,
        responses: Vec<ClaudeResponse>,
    ) -> Result<Option<ReviewResult>, Error> {
        for response in responses {
            match response {
                ClaudeResponse::Text(text) => {
                    let event = Event::message("assistant", &text);
                    self.state.add_event(event.clone());
                    self.stream.add_event(event).await;
                }
                ClaudeResponse::ToolUse { id: _, name, input } => {
                    if let Some(result) = self.handle_tool_use(&name, &input).await? {
                        return Ok(Some(result));
                    }
                }
                ClaudeResponse::Result { subtype, result } => {
                    info!(subtype = %subtype, "Claude returned result");
                    if let Some(result_str) = result
                        && let Ok(review_result) = serde_json::from_str::<ReviewResult>(&result_str)
                    {
                        self.state.set_finished(review_result.clone());
                        return Ok(Some(review_result));
                    }
                }
                ClaudeResponse::Usage {
                    input_tokens,
                    output_tokens,
                } => {
                    self.state.record_api_call(input_tokens + output_tokens);
                }
            }
        }
        Ok(None)
    }

    /// Handle a tool use request, returning a result if the agent finished.
    async fn handle_tool_use(
        &mut self,
        name: &str,
        input: &serde_json::Value,
    ) -> Result<Option<ReviewResult>, Error> {
        debug!(tool = %name, "Tool use requested");
        self.state.set_waiting();
        self.state.record_tool_call();

        let action = match self.parse_action(name, input) {
            Ok(action) => action,
            Err(error) => return self.handle_invalid_action(name, error).await,
        };

        if let Action::Finish { result } = action {
            info!("Agent finished with result");
            self.state.set_finished(result.clone());
            return Ok(Some(result));
        }

        let action_event = Event::action(action.clone());
        self.state.add_event(action_event.clone());
        self.stream.add_event(action_event).await;

        let observation = match self.executor.execute(&action).await {
            Ok(obs) => obs,
            Err(e) => {
                error!(error = %e, "Action execution error");
                Observation::Error {
                    message: format!("Execution error: {e}"),
                }
            }
        };

        let obs_event = Event::observation(observation);
        self.state.add_event(obs_event.clone());
        self.stream.add_event(obs_event).await;
        self.state.agent_state = AgentState::Running;
        Ok(None)
    }

    async fn handle_invalid_action(
        &mut self,
        name: &str,
        error: Error,
    ) -> Result<Option<ReviewResult>, Error> {
        warn!(error = %error, tool = %name, "Failed to parse action");
        let obs = Observation::Error {
            message: format!("Invalid tool call: {error}"),
        };
        let event = Event::observation(obs);
        self.state.add_event(event.clone());
        self.stream.add_event(event).await;
        Ok(None)
    }

    fn build_messages(&self) -> Vec<Message> {
        let mut messages = vec![Message {
            role: MessageRole::System,
            content: self.system_prompt.clone(),
        }];

        for event in &self.state.history {
            if let Some(message) = event_to_message(event) {
                messages.push(message);
            }
        }

        messages
    }

    fn parse_action(&self, name: &str, input: &serde_json::Value) -> Result<Action, Error> {
        match name {
            "read_file" => {
                let path = required_string(input, "path")?;
                Ok(Action::ReadFile { path: path.into() })
            }
            "run_command" => {
                let cmd = required_string(input, "cmd")?;
                Ok(Action::RunCommand { cmd: cmd.into() })
            }
            "post_comment" => {
                let body = required_string(input, "body")?;
                Ok(Action::PostComment { body: body.into() })
            }
            "approve" => Ok(Action::Approve),
            "request_changes" => {
                let reason = required_string(input, "reason")?;
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

fn event_to_message(event: &Event) -> Option<Message> {
    match &event.payload {
        EventPayload::Message { role, content } => {
            let role = parse_message_role(role)?;
            Some(Message {
                role,
                content: content.clone(),
            })
        }
        EventPayload::Action(action) => Some(Message {
            role: MessageRole::Assistant,
            content: format!("Tool call: {}", serde_json::to_string(action).unwrap()),
        }),
        EventPayload::Observation(obs) => {
            let content = serde_json::to_string(obs).unwrap();
            Some(Message {
                role: MessageRole::User,
                content: format!("Tool result: {content}"),
            })
        }
    }
}

fn parse_message_role(role: &str) -> Option<MessageRole> {
    match role {
        "user" => Some(MessageRole::User),
        "assistant" => Some(MessageRole::Assistant),
        _ => None,
    }
}

fn required_string<'a>(input: &'a serde_json::Value, key: &str) -> Result<&'a str, Error> {
    input
        .get(key)
        .and_then(|value| value.as_str())
        .ok_or_else(|| Error::InvalidToolInput(format!("missing {key}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::ReviewDecision;

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

    struct FailingExecutor;

    #[async_trait]
    impl ActionExecutor for FailingExecutor {
        async fn execute(&self, _action: &Action) -> Result<Observation, Error> {
            Err(Error::ClaudeApi("executor down".into()))
        }
    }

    fn approved_result() -> ReviewResult {
        ReviewResult {
            decision: ReviewDecision::Approved,
            summary: "ready".into(),
            issues: vec![],
        }
    }

    fn result_response(result: ReviewResult) -> ClaudeResponse {
        ClaudeResponse::Result {
            subtype: "success".into(),
            result: Some(serde_json::to_string(&result).unwrap()),
        }
    }

    fn finish_tool(result: ReviewResult) -> ClaudeResponse {
        ClaudeResponse::ToolUse {
            id: "finish-1".into(),
            name: "finish".into(),
            input: serde_json::to_value(result).unwrap(),
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

    #[tokio::test]
    async fn run_records_text_usage_and_result_response() {
        let result = approved_result();
        let claude = MockClaude {
            responses: vec![vec![
                ClaudeResponse::Text("Looks good".into()),
                ClaudeResponse::Usage {
                    input_tokens: 11,
                    output_tokens: 7,
                },
                result_response(result.clone()),
            ]],
            call_count: 0,
        };
        let mut controller = AgentController::new(claude, MockExecutor, "system");

        let actual = controller.run("review this").await.unwrap();

        assert_eq!(actual.summary, result.summary);
        assert!(controller.state.is_finished());
        assert_eq!(controller.state.metrics.api_calls, 1);
        assert_eq!(controller.state.metrics.total_tokens, 18);
        assert_eq!(controller.state.history.len(), 2);
    }

    #[tokio::test]
    async fn run_executes_tool_observation_before_finish() {
        let result = approved_result();
        let claude = MockClaude {
            responses: vec![
                vec![ClaudeResponse::ToolUse {
                    id: "read-1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "src/lib.rs"}),
                }],
                vec![finish_tool(result.clone())],
            ],
            call_count: 0,
        };
        let mut controller = AgentController::new(claude, MockExecutor, "system");

        let actual = controller.run("inspect").await.unwrap();

        assert_eq!(actual.summary, result.summary);
        assert_eq!(controller.state.metrics.tool_calls, 2);
        assert!(controller.state.history.iter().any(|event| {
            matches!(
                &event.payload,
                EventPayload::Observation(Observation::FileContent { path, content })
                    if path == "src/lib.rs" && content == "file content"
            )
        }));
    }

    #[tokio::test]
    async fn invalid_tool_input_records_error_and_continues() {
        let result = approved_result();
        let claude = MockClaude {
            responses: vec![
                vec![ClaudeResponse::ToolUse {
                    id: "bad-1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({}),
                }],
                vec![finish_tool(result)],
            ],
            call_count: 0,
        };
        let mut controller = AgentController::new(claude, MockExecutor, "system");

        controller.run("inspect").await.unwrap();

        assert!(controller.state.history.iter().any(|event| {
            matches!(
                &event.payload,
                EventPayload::Observation(Observation::Error { message })
                    if message.contains("missing path")
            )
        }));
    }

    #[tokio::test]
    async fn executor_error_becomes_observation_and_loop_continues() {
        let result = approved_result();
        let claude = MockClaude {
            responses: vec![
                vec![ClaudeResponse::ToolUse {
                    id: "cmd-1".into(),
                    name: "run_command".into(),
                    input: serde_json::json!({"cmd": "cargo test"}),
                }],
                vec![finish_tool(result)],
            ],
            call_count: 0,
        };
        let mut controller = AgentController::new(claude, FailingExecutor, "system");

        controller.run("inspect").await.unwrap();

        assert!(controller.state.history.iter().any(|event| {
            matches!(
                &event.payload,
                EventPayload::Observation(Observation::Error { message })
                    if message.contains("executor down")
            )
        }));
    }

    #[test]
    fn build_messages_converts_history_events() {
        let mut state = State::new();
        state.add_event(Event::message("user", "please review"));
        state.add_event(Event::message("assistant", "reading"));
        state.add_event(Event::action(Action::Approve));
        state.add_event(Event::observation(Observation::Approved));
        state.add_event(Event::message("system", "ignored"));
        let controller = AgentController::new(
            MockClaude {
                responses: vec![],
                call_count: 0,
            },
            MockExecutor,
            "system prompt",
        )
        .with_state(state);

        let messages = controller.build_messages();

        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0].role, MessageRole::System);
        assert_eq!(messages[1].role, MessageRole::User);
        assert_eq!(messages[2].role, MessageRole::Assistant);
        assert_eq!(messages[3].role, MessageRole::Assistant);
        assert_eq!(messages[4].role, MessageRole::User);
    }

    #[test]
    fn parse_action_validates_all_tool_shapes() {
        let controller = AgentController::new(
            MockClaude {
                responses: vec![],
                call_count: 0,
            },
            MockExecutor,
            "test",
        );

        let run = controller
            .parse_action("run_command", &serde_json::json!({"cmd": "cargo test"}))
            .unwrap();
        assert!(matches!(run, Action::RunCommand { cmd } if cmd == "cargo test"));

        let comment = controller
            .parse_action("post_comment", &serde_json::json!({"body": "note"}))
            .unwrap();
        assert!(matches!(comment, Action::PostComment { body } if body == "note"));

        let changes = controller
            .parse_action(
                "request_changes",
                &serde_json::json!({"reason": "needs tests"}),
            )
            .unwrap();
        assert!(matches!(changes, Action::RequestChanges { reason } if reason == "needs tests"));

        assert!(matches!(
            controller
                .parse_action("approve", &serde_json::json!({}))
                .unwrap(),
            Action::Approve
        ));
        assert!(matches!(
            controller.parse_action("unknown", &serde_json::json!({})),
            Err(Error::UnknownTool(tool)) if tool == "unknown"
        ));
    }

    #[test]
    fn parse_finish_rejects_invalid_result_shape() {
        let controller = AgentController::new(
            MockClaude {
                responses: vec![],
                call_count: 0,
            },
            MockExecutor,
            "test",
        );

        assert!(matches!(
            controller.parse_action("finish", &serde_json::json!({"summary": "missing decision"})),
            Err(Error::InvalidToolInput(message)) if message.contains("invalid result")
        ));
    }
}
