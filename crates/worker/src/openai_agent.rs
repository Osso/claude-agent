use std::env;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use tracing::info;

const DEFAULT_OPENAI_MODEL: &str = "gpt-5.4-mini";
const DEFAULT_OPENAI_API_BASE: &str = "https://api.openai.com/v1";
const OPENAI_AGENT_MAX_STEPS: usize = 80;
const COMMAND_OUTPUT_LIMIT: usize = 60_000;

/// Run GPT through the OpenAI Responses API with local shell-command tool calls.
pub(crate) fn run_gpt_agent(work_dir: &Path, prompt: &str) -> Result<()> {
    let config = OpenAiAgentConfig::from_env()?;
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("building OpenAI HTTP client")?;

    let mut request = build_openai_request(&config.model, prompt);
    let mut previous_response_id: Option<String> = None;

    for step in 1..=OPENAI_AGENT_MAX_STEPS {
        if let Some(response_id) = previous_response_id.as_deref() {
            request["previous_response_id"] = serde_json::Value::String(response_id.to_string());
        }

        let response = call_openai(&client, &config, request)?;
        let parsed = parse_openai_response(response)?;
        previous_response_id = parsed.response_id;

        if parsed.function_calls.is_empty() {
            log_final_response(step, parsed.output_text);
            return Ok(());
        }

        let tool_outputs = parsed
            .function_calls
            .iter()
            .map(|call| execute_openai_tool(work_dir, call))
            .collect::<Vec<_>>();
        request = build_openai_tool_output_request(&config.model, tool_outputs);
    }

    bail!("GPT agent exceeded max steps ({OPENAI_AGENT_MAX_STEPS})");
}

struct OpenAiAgentConfig {
    api_key: String,
    model: String,
    api_base: String,
}

impl OpenAiAgentConfig {
    fn from_env() -> Result<Self> {
        Ok(Self {
            api_key: env::var("OPENAI_API_KEY").context("OPENAI_API_KEY not set")?,
            model: env::var("OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_OPENAI_MODEL.to_string()),
            api_base: env::var("OPENAI_API_BASE")
                .unwrap_or_else(|_| DEFAULT_OPENAI_API_BASE.to_string()),
        })
    }
}

#[derive(Debug)]
struct OpenAiResponseAction {
    response_id: Option<String>,
    output_text: Option<String>,
    function_calls: Vec<OpenAiFunctionCall>,
}

#[derive(Debug)]
struct OpenAiFunctionCall {
    call_id: String,
    name: String,
    arguments: serde_json::Value,
}

fn call_openai(
    client: &reqwest::blocking::Client,
    config: &OpenAiAgentConfig,
    request: serde_json::Value,
) -> Result<serde_json::Value> {
    let url = format!("{}/responses", config.api_base.trim_end_matches('/'));
    let mut last_error = None;

    for attempt in 1..=3 {
        match send_openai_request(client, config, &url, &request) {
            Ok(value) => return Ok(value),
            Err(error) => {
                last_error = Some(error);
                if attempt < 3 {
                    std::thread::sleep(std::time::Duration::from_secs(attempt));
                }
            }
        }
    }

    Err(last_error.expect("OpenAI request attempted at least once"))
}

fn send_openai_request(
    client: &reqwest::blocking::Client,
    config: &OpenAiAgentConfig,
    url: &str,
    request: &serde_json::Value,
) -> Result<serde_json::Value> {
    let response = client
        .post(url)
        .bearer_auth(&config.api_key)
        .json(request)
        .send()
        .context("sending OpenAI response request")?;
    let status = response.status();
    let value: serde_json::Value = response
        .json()
        .with_context(|| format!("reading OpenAI response body after HTTP {status}"))?;

    if !status.is_success() {
        bail!("OpenAI response request failed with HTTP {status}: {value}");
    }

    Ok(value)
}

fn build_openai_request(model: &str, prompt: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "instructions": "You are an autonomous coding agent running in an isolated worker container. Use run_command for repository inspection, edits, tests, commits, pushes, and PR creation. Prefer small verifiable steps. Stop only after the requested review or fix has been completed, or after explaining the blocker.",
        "input": prompt,
        "tools": [run_command_tool_schema()],
    })
}

fn build_openai_tool_output_request(
    model: &str,
    outputs: Vec<serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "input": outputs,
        "tools": [run_command_tool_schema()],
    })
}

fn run_command_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "name": "run_command",
        "description": "Run a shell command in the cloned repository and return stdout, stderr, and exit code.",
        "parameters": {
            "type": "object",
            "properties": {
                "cmd": {
                    "type": "string",
                    "description": "Shell command to run from the repository root."
                }
            },
            "required": ["cmd"],
            "additionalProperties": false
        },
        "strict": true
    })
}

fn parse_openai_response(value: serde_json::Value) -> Result<OpenAiResponseAction> {
    let response_id = value
        .get("id")
        .and_then(|id| id.as_str())
        .map(str::to_string);
    let output_text = extract_openai_output_text(&value);
    let mut function_calls = Vec::new();

    if let Some(outputs) = value.get("output").and_then(|output| output.as_array()) {
        for output in outputs {
            parse_function_call(output, &mut function_calls)?;
        }
    }

    Ok(OpenAiResponseAction {
        response_id,
        output_text,
        function_calls,
    })
}

fn parse_function_call(
    output: &serde_json::Value,
    calls: &mut Vec<OpenAiFunctionCall>,
) -> Result<()> {
    if output.get("type").and_then(|t| t.as_str()) != Some("function_call") {
        return Ok(());
    }

    let call_id = required_json_string(output, "call_id")?.to_string();
    let name = required_json_string(output, "name")?.to_string();
    let raw_arguments = required_json_string(output, "arguments")?;
    let arguments = serde_json::from_str(raw_arguments).with_context(|| {
        format!("parsing OpenAI function arguments for {name}: {raw_arguments}")
    })?;

    calls.push(OpenAiFunctionCall {
        call_id,
        name,
        arguments,
    });
    Ok(())
}

fn extract_openai_output_text(value: &serde_json::Value) -> Option<String> {
    if let Some(text) = value.get("output_text").and_then(|v| v.as_str()) {
        let text = text.trim();
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }

    let text = value
        .get("output")
        .and_then(|v| v.as_array())?
        .iter()
        .filter_map(|output| output.get("content").and_then(|v| v.as_array()))
        .flat_map(|content| content.iter())
        .filter_map(|part| part.get("text").and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    if text.is_empty() { None } else { Some(text) }
}

fn required_json_string<'a>(value: &'a serde_json::Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .with_context(|| format!("OpenAI response missing string field {key}"))
}

fn execute_openai_tool(work_dir: &Path, call: &OpenAiFunctionCall) -> serde_json::Value {
    let output = match call.name.as_str() {
        "run_command" => {
            let Some(cmd) = call.arguments.get("cmd").and_then(|cmd| cmd.as_str()) else {
                return openai_tool_output(&call.call_id, "missing string argument: cmd");
            };
            run_shell_command(work_dir, cmd)
        }
        other => format!("unknown tool: {other}"),
    };

    openai_tool_output(&call.call_id, &output)
}

fn run_shell_command(work_dir: &Path, cmd: &str) -> String {
    info!(cmd = %cmd, "Running GPT-requested command");

    match Command::new("sh")
        .arg("-lc")
        .arg(cmd)
        .current_dir(work_dir)
        .output()
    {
        Ok(output) => format_command_output(&output),
        Err(error) => format!("failed to run command: {error}"),
    }
}

fn format_command_output(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    limit_tool_output(&format!(
        "exit_code: {}\nstdout:\n{}\nstderr:\n{}",
        output.status.code().unwrap_or(-1),
        stdout,
        stderr
    ))
}

fn openai_tool_output(call_id: &str, output: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": output,
    })
}

fn limit_tool_output(output: &str) -> String {
    if output.len() <= COMMAND_OUTPUT_LIMIT {
        return output.to_string();
    }

    let keep = COMMAND_OUTPUT_LIMIT / 2;
    let head = output.chars().take(keep).collect::<String>();
    let tail = output
        .chars()
        .rev()
        .take(keep)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!(
        "{}\n\n[output truncated: {} bytes omitted]\n\n{}",
        head,
        output.len() - COMMAND_OUTPUT_LIMIT,
        tail
    )
}

fn log_final_response(step: usize, text: Option<String>) {
    if let Some(text) = text {
        info!(step, output_len = text.len(), "GPT agent finished");
        println!("{text}");
    } else {
        info!(step, "GPT agent finished without text output");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_openai_request_uses_responses_api_tools() {
        let request = build_openai_request("gpt-test", "fix the bug");

        assert_eq!(request["model"], "gpt-test");
        assert_eq!(request["input"], "fix the bug");
        assert!(
            request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "run_command")
        );
    }

    #[test]
    fn test_extract_openai_function_call() {
        let response = serde_json::json!({
            "id": "resp_123",
            "output": [{
                "type": "function_call",
                "call_id": "call_123",
                "name": "run_command",
                "arguments": "{\"cmd\":\"cargo test\"}"
            }]
        });

        let action = parse_openai_response(response).unwrap();

        assert_eq!(action.response_id, Some("resp_123".to_string()));
        assert_eq!(action.function_calls.len(), 1);
        assert_eq!(action.function_calls[0].name, "run_command");
        assert_eq!(action.function_calls[0].arguments["cmd"], "cargo test");
    }
}
