//! Claude Code process management.
//!
//! Spawns and communicates with Claude Code CLI in stream-json mode.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use async_trait::async_trait;
use tracing::{debug, error, info};

use claude_agent_core::{ClaudeBackend, ClaudeResponse, Error, Message, MessageRole};

use crate::output::{ClaudeInput, ClaudeOutput, ContentBlock};

/// A running Claude Code process.
pub struct ClaudeProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl ClaudeProcess {
    /// Spawn a new Claude Code process.
    pub fn spawn(working_dir: &Path) -> Result<Self, Error> {
        info!(cwd = %working_dir.display(), "Spawning Claude Code process");

        let mut child = Command::new("claude")
            .arg("--print")
            .args(["--input-format", "stream-json"])
            .args(["--output-format", "stream-json"])
            .arg("--verbose")
            .args(["--dangerously-skip-permissions"]) // Running in isolated container
            .current_dir(working_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()) // Let stderr pass through for debugging
            .spawn()
            .map_err(|e| Error::Io(e))?;

        let stdin = child.stdin.take().ok_or_else(|| {
            Error::ClaudeApi("Failed to capture stdin".into())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            Error::ClaudeApi("Failed to capture stdout".into())
        })?;

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    /// Send a user message and collect all responses until result.
    pub fn send(&mut self, content: &str) -> Result<Vec<ClaudeOutput>, Error> {
        info!(content_len = content.len(), "Sending message to Claude");

        // Send input
        let input = ClaudeInput::user(content.into());
        let json = serde_json::to_string(&input)?;
        writeln!(self.stdin, "{json}")?;
        self.stdin.flush()?;
        info!("Message sent, waiting for Claude response");

        // Collect output until result
        let mut outputs = Vec::new();
        let mut line = String::new();

        loop {
            line.clear();
            let bytes_read = self.stdout.read_line(&mut line)?;

            if bytes_read == 0 {
                error!("Claude process closed stdout unexpectedly");
                break;
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            info!(line_len = trimmed.len(), "Received line from Claude");

            match serde_json::from_str::<ClaudeOutput>(trimmed) {
                Ok(output) => {
                    // Log progress for visibility
                    match &output {
                        ClaudeOutput::System { subtype, .. } => {
                            info!(subtype = %subtype, "Claude session started");
                        }
                        ClaudeOutput::Assistant { subtype, message } => {
                            if let (Some(subtype), Some(msg)) = (subtype, message) {
                                match subtype {
                                    crate::output::AssistantSubtype::ToolUse => {
                                        for block in &msg.content {
                                            if let crate::output::ContentBlock::ToolUse { name, .. } = block {
                                                info!(tool = %name, "Claude using tool");
                                            }
                                        }
                                    }
                                    crate::output::AssistantSubtype::Text => {
                                        // Log text preview (first 100 chars)
                                        if let Some(text) = output.text() {
                                            let preview: String = text.chars().take(100).collect();
                                            debug!(preview = %preview, "Claude text output");
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        ClaudeOutput::Result { is_error, total_cost_usd, usage, .. } => {
                            let tokens = usage.as_ref().map(|u| u.input_tokens + u.output_tokens).unwrap_or(0);
                            info!(
                                is_error = %is_error,
                                cost_usd = ?total_cost_usd,
                                total_tokens = tokens,
                                "Claude completed"
                            );
                        }
                        _ => {}
                    }

                    let is_result = output.is_result();
                    outputs.push(output);

                    if is_result {
                        debug!(count = outputs.len(), "Received result, done collecting");
                        break;
                    }
                }
                Err(e) => {
                    error!(error = %e, line = trimmed, "Failed to parse Claude output");
                    // Continue trying to read more lines
                }
            }
        }

        Ok(outputs)
    }

    /// Kill the Claude process.
    pub fn kill(&mut self) -> Result<(), Error> {
        info!("Killing Claude process");
        self.child.kill().map_err(Error::Io)
    }

    /// Wait for the process to exit.
    pub fn wait(&mut self) -> Result<std::process::ExitStatus, Error> {
        self.child.wait().map_err(Error::Io)
    }
}

impl Drop for ClaudeProcess {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

#[async_trait]
impl ClaudeBackend for ClaudeProcess {
    async fn prompt(&mut self, messages: &[Message]) -> Result<Vec<ClaudeResponse>, Error> {
        // Build prompt from messages
        let prompt = build_prompt(messages);

        // Send and collect outputs
        let outputs = self.send(&prompt)?;

        // Convert to ClaudeResponse
        let responses = outputs
            .into_iter()
            .filter_map(|output| convert_output(output))
            .collect();

        Ok(responses)
    }
}

fn build_prompt(messages: &[Message]) -> String {
    let mut prompt = String::new();

    for msg in messages {
        match msg.role {
            MessageRole::System => {
                // System messages are typically set via --system-prompt,
                // but we can include them in the prompt for now
                if !prompt.is_empty() {
                    prompt.push_str("\n\n");
                }
                prompt.push_str(&msg.content);
            }
            MessageRole::User => {
                if !prompt.is_empty() {
                    prompt.push_str("\n\n");
                }
                prompt.push_str(&msg.content);
            }
            MessageRole::Assistant => {
                // Assistant messages in history - include for context
                if !prompt.is_empty() {
                    prompt.push_str("\n\n");
                }
                prompt.push_str("Previous response: ");
                prompt.push_str(&msg.content);
            }
        }
    }

    prompt
}

fn convert_output(output: ClaudeOutput) -> Option<ClaudeResponse> {
    match output {
        ClaudeOutput::Assistant { message: Some(msg), .. } => {
            for block in msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        return Some(ClaudeResponse::Text(text));
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        return Some(ClaudeResponse::ToolUse { id, name, input });
                    }
                    _ => {}
                }
            }
            None
        }
        ClaudeOutput::Result { subtype, result, .. } => {
            Some(ClaudeResponse::Result { subtype, result })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_prompt() {
        let messages = vec![
            Message {
                role: MessageRole::System,
                content: "You are a reviewer.".into(),
            },
            Message {
                role: MessageRole::User,
                content: "Review this code.".into(),
            },
        ];

        let prompt = build_prompt(&messages);
        assert!(prompt.contains("You are a reviewer."));
        assert!(prompt.contains("Review this code."));
    }
}
