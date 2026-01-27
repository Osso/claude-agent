//! Claude Code output parsing.
//!
//! Parses the stream-json output format from Claude Code CLI.

use serde::{Deserialize, Serialize};

/// Input message to send to Claude Code.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClaudeInput {
    User { content: String },
}

/// Output message from Claude Code (stream-json format).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClaudeOutput {
    /// System information at start.
    System {
        subtype: String,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
    },

    /// Assistant message content.
    Assistant {
        subtype: AssistantSubtype,
        #[serde(default)]
        message: Option<AssistantMessage>,
    },

    /// Result/completion message.
    Result {
        subtype: String,
        #[serde(default)]
        result: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        total_cost_usd: Option<f64>,
        #[serde(default)]
        usage: Option<Usage>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantSubtype {
    Init,
    Text,
    ToolUse,
    ToolResult,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssistantMessage {
    #[serde(default)]
    pub id: Option<String>,
    pub role: String,
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: Option<String>,
        #[serde(default)]
        is_error: bool,
    },
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl ClaudeOutput {
    /// Check if this is a result/completion message.
    pub fn is_result(&self) -> bool {
        matches!(self, ClaudeOutput::Result { .. })
    }

    /// Check if this is an error result.
    pub fn is_error(&self) -> bool {
        matches!(self, ClaudeOutput::Result { is_error: true, .. })
    }

    /// Extract text content if this is a text message.
    pub fn text(&self) -> Option<&str> {
        match self {
            ClaudeOutput::Assistant {
                message: Some(msg), ..
            } => {
                for block in &msg.content {
                    if let ContentBlock::Text { text } = block {
                        return Some(text);
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// Extract tool use if this is a tool use message.
    pub fn tool_use(&self) -> Option<(&str, &str, &serde_json::Value)> {
        match self {
            ClaudeOutput::Assistant {
                message: Some(msg), ..
            } => {
                for block in &msg.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        return Some((id, name, input));
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// Extract usage info.
    pub fn usage(&self) -> Option<&Usage> {
        match self {
            ClaudeOutput::Assistant {
                message: Some(msg), ..
            } => msg.usage.as_ref(),
            ClaudeOutput::Result { usage, .. } => usage.as_ref(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_system_init() {
        let json = r#"{"type":"system","subtype":"init","cwd":"/home/user","session_id":"abc123"}"#;
        let output: ClaudeOutput = serde_json::from_str(json).unwrap();
        assert!(matches!(output, ClaudeOutput::System { subtype, .. } if subtype == "init"));
    }

    #[test]
    fn test_parse_assistant_text() {
        let json = r#"{
            "type": "assistant",
            "subtype": "text",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "Hello!"}]
            }
        }"#;
        let output: ClaudeOutput = serde_json::from_str(json).unwrap();
        assert_eq!(output.text(), Some("Hello!"));
    }

    #[test]
    fn test_parse_tool_use() {
        let json = r#"{
            "type": "assistant",
            "subtype": "tool_use",
            "message": {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "tool_1",
                    "name": "read_file",
                    "input": {"path": "src/main.rs"}
                }]
            }
        }"#;
        let output: ClaudeOutput = serde_json::from_str(json).unwrap();
        let (id, name, input) = output.tool_use().unwrap();
        assert_eq!(id, "tool_1");
        assert_eq!(name, "read_file");
        assert_eq!(input["path"], "src/main.rs");
    }

    #[test]
    fn test_parse_result() {
        let json = r#"{
            "type": "result",
            "subtype": "success",
            "result": "Done",
            "is_error": false,
            "total_cost_usd": 0.05
        }"#;
        let output: ClaudeOutput = serde_json::from_str(json).unwrap();
        assert!(output.is_result());
        assert!(!output.is_error());
    }
}
