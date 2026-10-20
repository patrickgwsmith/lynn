use crate::agent_cli::{self, AgentReply, AgentUser, ModelInfo, Request};
use crate::session::{AgentAccess, ReasoningEffort};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PROMPT: &str = "Answer the Lynn user request supplied on stdin. Use the Lynn session context and inspect project files as needed.";

pub fn run(request: Request<'_>) -> Result<AgentReply, String> {
    let Request {
        task,
        cwd,
        context,
        model,
        access,
        user,
        effort,
    } = request;
    let mut command = request_command(cwd, model, access, user, effort)?;
    command.args(["--output-format", "json"]);
    let task = agent_cli::request_for_access(task, access);
    let output =
        agent_cli::run_with_input(command, "claude", agent_cli::session_input(context, &task))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("claude {}: {}", output.status, stderr.trim()));
    }
    parse_result(&output.stdout)
}

fn request_command(
    cwd: &Path,
    model: Option<&str>,
    access: AgentAccess,
    user: AgentUser,
    effort: ReasoningEffort,
) -> Result<Command, String> {
    let mut command = agent_cli::workspace_command("claude", user, access)?;
    let tools = match access {
        AgentAccess::ReadOnly => "Read,Glob,Grep",
        AgentAccess::ReadWrite => "Read,Glob,Grep,Edit,Write,Bash",
    };
    command.args([
        "-p",
        "--restricted",
        "--strict-mcp-config",
        "--tools",
        tools,
        "--allowedTools",
        tools,
        "--disallowedTools",
        "mcp__*",
        "--permission-mode",
        "dontAsk",
        "--permission-prompts",
        "none",
        "--no-session-persistence",
    ]);
    let instruction = match access {
        AgentAccess::ReadOnly => agent_cli::READ_ONLY_REPLY_INSTRUCTION.to_owned(),
        AgentAccess::ReadWrite => "Carry out the user's task and report what changed.".to_owned(),
    };
    command.args([
        "--append-system-prompt",
        &format!(
            "{instruction} Do not mention unavailable connectors or integrations unless the user asks about them or they prevent the task."
        ),
    ]);
    if let Some(model) = model {
        command.args(["--model", model]);
    }
    apply_effort(&mut command, effort);
    command.arg(PROMPT).current_dir(cwd);
    Ok(command)
}

pub fn run_live(
    request: Request<'_>,
    mut progress: impl FnMut(String),
) -> Result<AgentReply, String> {
    let Request {
        task,
        cwd,
        context,
        model,
        access,
        user,
        effort,
    } = request;
    let mut command = request_command(cwd, model, access, user, effort)?;
    command.args([
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
    ]);
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("claude: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("claude stdin unavailable")?;
    let task = agent_cli::request_for_access(task, access);
    let input = agent_cli::session_input(context, &task);
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let stderr = child.stderr.take().ok_or("claude stderr unavailable")?;
    let stderr_reader = std::thread::spawn(move || {
        let mut output = String::new();
        BufReader::new(stderr)
            .read_to_string(&mut output)
            .map(|_| output)
    });
    let stdout = child.stdout.take().ok_or("claude stdout unavailable")?;
    let mut answer = None;
    let mut stream_error = None;
    let mut reply_fragment = String::new();
    let mut last_reply_update = Instant::now() - Duration::from_secs(1);
    for line in BufReader::new(stdout).lines() {
        match line {
            Ok(line) => match serde_json::from_str::<serde_json::Value>(&line) {
                Ok(event) => {
                    if let Some(status) = progress_from_event(&event) {
                        progress(status);
                    }
                    if let Some(delta) = event["event"]["delta"]["text"].as_str().filter(|_| {
                        event["type"] == "stream_event"
                            && event["event"]["type"] == "content_block_delta"
                    }) {
                        reply_fragment.push_str(delta);
                        if reply_fragment.len() > 1024 {
                            reply_fragment = reply_fragment
                                .chars()
                                .rev()
                                .take(160)
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .collect();
                        }
                        if last_reply_update.elapsed() >= Duration::from_millis(250) {
                            let tail: String = reply_fragment
                                .chars()
                                .rev()
                                .take(80)
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .collect();
                            let tail = tail
                                .lines()
                                .last()
                                .unwrap_or("")
                                .chars()
                                .filter(|character| !character.is_control())
                                .collect::<String>();
                            if !tail.is_empty() {
                                progress(format!("Replying: {tail}"));
                            }
                            last_reply_update = Instant::now();
                        }
                    }
                    if event["type"] == "result" {
                        answer = Some(parse_result(line.as_bytes()));
                    }
                }
                Err(error) => {
                    stream_error = Some(format!("claude returned invalid stream JSON: {error}"))
                }
            },
            Err(error) => {
                stream_error = Some(format!("claude stdout: {error}"));
                break;
            }
        }
    }
    let status = child.wait().map_err(|error| format!("claude: {error}"))?;
    match writer.join() {
        Ok(Ok(())) => {}
        Ok(Err(error)) if error.kind() == ErrorKind::BrokenPipe => {}
        Ok(Err(error)) => return Err(format!("claude stdin: {error}")),
        Err(_) => return Err("claude stdin writer panicked".to_owned()),
    }
    let stderr = stderr_reader
        .join()
        .map_err(|_| "claude stderr reader panicked")?
        .map_err(|error| format!("claude stderr: {error}"))?;
    if !status.success() {
        return Err(format!("claude {status}: {}", stderr.trim()));
    }
    if let Some(error) = stream_error {
        return Err(error);
    }
    answer.ok_or_else(|| "claude returned no result".to_owned())?
}

fn progress_from_event(event: &serde_json::Value) -> Option<String> {
    if event["type"] != "assistant" {
        return None;
    }
    let content = event["message"]["content"].as_array()?;
    content.iter().rev().find_map(|block| {
        if block["type"] != "tool_use" {
            return None;
        }
        let name = block["name"].as_str()?;
        let target = match name {
            "Read" | "Edit" | "Write" => block["input"]["file_path"].as_str(),
            "Glob" => block["input"]["pattern"].as_str(),
            "Grep" => block["input"]["pattern"].as_str(),
            "Bash" => block["input"]["command"].as_str(),
            _ => None,
        };
        let target = target.unwrap_or("").lines().next().unwrap_or("");
        let target: String = target
            .chars()
            .filter(|character| !character.is_control())
            .take(80)
            .collect();
        Some(if target.is_empty() {
            format!("{name}…")
        } else {
            format!("{name}: {target}")
        })
    })
}

fn apply_effort(command: &mut Command, effort: ReasoningEffort) {
    if let Some(level) = effort.level() {
        command.args(["--effort", level]);
        command.env("CLAUDE_CODE_EFFORT_LEVEL", level);
    }
}

/// The interactive Claude Code CLI with the same tools as `claude?` or `claude!`.
pub fn interactive(
    cwd: &Path,
    model: Option<&str>,
    access: AgentAccess,
    effort: ReasoningEffort,
) -> Result<Command, String> {
    let mut command = agent_cli::workspace_command("claude", AgentUser::Current, access)?;
    let tools = match access {
        AgentAccess::ReadOnly => "Read,Glob,Grep",
        AgentAccess::ReadWrite => "Read,Glob,Grep,Edit,Write,Bash",
    };
    command.args([
        "--restricted",
        "--strict-mcp-config",
        "--tools",
        tools,
        "--allowedTools",
        tools,
        "--disallowedTools",
        "mcp__*",
    ]);
    if let Some(model) = model {
        command.args(["--model", model]);
    }
    apply_effort(&mut command, effort);
    command.current_dir(cwd);
    Ok(command)
}

fn parse_result(output: &[u8]) -> Result<AgentReply, String> {
    let value: serde_json::Value = serde_json::from_slice(output)
        .map_err(|error| format!("claude returned invalid JSON: {error}"))?;
    let result = value["result"]
        .as_str()
        .ok_or("claude returned no result")?;
    if value["is_error"].as_bool() == Some(true) {
        return Err(result.to_owned());
    }
    let used_models = value["modelUsage"].as_object().map(|usage| {
        usage
            .keys()
            .map(|name| {
                name.chars()
                    .filter(|character| !character.is_control())
                    .take(120)
                    .collect::<String>()
            })
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>()
    });
    let model_info = match used_models {
        Some(models) if !models.is_empty() => ModelInfo::Reported(models),
        _ => ModelInfo::Unreported,
    };
    Ok(AgentReply {
        text: result.to_owned(),
        model_info: Some(model_info),
    })
}

#[cfg(test)]
mod tests {
    use super::{apply_effort, ModelInfo};
    use crate::session::ReasoningEffort;
    use std::process::Command;

    #[test]
    fn effort_flag_is_per_request() {
        let mut command = Command::new("claude");
        apply_effort(&mut command, ReasoningEffort::ExtraHigh);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["--effort", "xhigh"]
        );
        assert!(command.get_envs().any(|(key, value)| {
            key == "CLAUDE_CODE_EFFORT_LEVEL" && value == Some(std::ffi::OsStr::new("xhigh"))
        }));
        let mut command = Command::new("claude");
        apply_effort(&mut command, ReasoningEffort::Maximum);
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["--effort", "max"]);
        let mut command = Command::new("claude");
        apply_effort(&mut command, ReasoningEffort::Default);
        assert_eq!(command.get_args().count(), 0);
    }
    use super::parse_result;

    #[test]
    fn result_errors_are_not_shown_as_answers() {
        let reply = parse_result(br#"{"type":"result","is_error":false,"result":"found it","modelUsage":{"claude-opus-5-5":{},"claude-fable-5-1":{}}}"#).unwrap();
        assert_eq!(reply.text, "found it");
        assert_eq!(
            reply.model_info,
            Some(ModelInfo::Reported(vec![
                "claude-fable-5-1".into(),
                "claude-opus-5-5".into()
            ]))
        );
        assert_eq!(
            parse_result(br#"{"type":"result","is_error":true,"result":"denied"}"#).unwrap_err(),
            "denied"
        );
    }

    #[test]
    fn stream_progress_reports_tools_and_keeps_result_separate() {
        let event: serde_json::Value = serde_json::json!({
            "type": "assistant", "message": {"content": [
                {"type": "text", "text": "I will inspect the file"},
                {"type": "tool_use", "name": "Read", "input": {"file_path": "src/main.rs"}}
            ]}
        });
        assert_eq!(
            super::progress_from_event(&event).as_deref(),
            Some("Read: src/main.rs")
        );
        assert_eq!(
            super::progress_from_event(&serde_json::json!({
                "type": "result", "result": "done"
            })),
            None
        );
    }
}
