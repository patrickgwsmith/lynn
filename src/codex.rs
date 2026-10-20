use crate::agent_cli::{self, request_for_access, AgentReply, AgentUser, ModelInfo, Request};
use crate::session::{AgentAccess, ReasoningEffort};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

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
    let args = exec_args(model, access, effort);
    let request = request_for_access(task, access);
    agent_cli::run("codex", &args, &request, cwd, context, user, access).map(|text| AgentReply {
        text,
        model_info: Some(codex_model_info(model)),
    })
}

fn codex_model_info(model: Option<&str>) -> ModelInfo {
    match model {
        Some(model) => ModelInfo::Requested(model.to_owned()),
        None => ModelInfo::Unreported,
    }
}

fn exec_args(model: Option<&str>, access: AgentAccess, effort: ReasoningEffort) -> Vec<&str> {
    let mut args = vec!["--ask-for-approval", "never", "exec"];
    if access == AgentAccess::ReadOnly {
        args.push("--ignore-user-config");
    }
    args.extend([
        "--sandbox",
        match access {
            AgentAccess::ReadOnly => "read-only",
            AgentAccess::ReadWrite => "workspace-write",
        },
        "--ephemeral",
        "--skip-git-repo-check",
        "--color",
        "never",
    ]);
    if let Some(model) = model {
        args.extend(["--model", model]);
    }
    if let Some(level) = effort.level() {
        args.extend(["--config", effort_config(level)]);
    }
    args.push("-");
    args
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
    let mut args = exec_args(model, access, effort);
    args.insert(args.len() - 1, "--json");
    let mut command = agent_cli::workspace_command("codex", user, access)?;
    command.args(args).current_dir(cwd);
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("codex: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("codex stdin unavailable")?;
    let input = agent_cli::session_input(context, &request_for_access(task, access));
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let stderr = child.stderr.take().ok_or("codex stderr unavailable")?;
    let stderr_reader = std::thread::spawn(move || {
        let mut output = String::new();
        BufReader::new(stderr)
            .read_to_string(&mut output)
            .map(|_| output)
    });
    let stdout = child.stdout.take().ok_or("codex stdout unavailable")?;
    let mut answer = None;
    let mut turn_error = None;
    let mut stream_error = None;
    for line in BufReader::new(stdout).lines() {
        match line {
            Ok(line) => match serde_json::from_str::<serde_json::Value>(&line) {
                Ok(event) => {
                    if let Some(status) = progress_from_event(&event) {
                        progress(status);
                    }
                    if event["type"] == "item.completed" && event["item"]["type"] == "agent_message"
                    {
                        if let Some(text) = event["item"]["text"].as_str() {
                            answer = Some(text.to_owned());
                        }
                    }
                    if event["type"] == "turn.failed" {
                        turn_error = event["error"]["message"].as_str().map(str::to_owned);
                    }
                }
                Err(error) => {
                    stream_error = Some(format!("codex returned invalid JSON event: {error}"))
                }
            },
            Err(error) => {
                stream_error = Some(format!("codex stdout: {error}"));
                break;
            }
        }
    }
    let status = child.wait().map_err(|error| format!("codex: {error}"))?;
    match writer.join() {
        Ok(Ok(())) => {}
        Ok(Err(error)) if error.kind() == ErrorKind::BrokenPipe => {}
        Ok(Err(error)) => return Err(format!("codex stdin: {error}")),
        Err(_) => return Err("codex stdin writer panicked".to_owned()),
    }
    let stderr = stderr_reader
        .join()
        .map_err(|_| "codex stderr reader panicked")?
        .map_err(|error| format!("codex stderr: {error}"))?;
    if !status.success() {
        return Err(format!("codex {status}: {}", stderr.trim()));
    }
    if let Some(error) = stream_error {
        return Err(error);
    }
    if let Some(error) = turn_error {
        return Err(error);
    }
    answer
        .filter(|text| !text.trim().is_empty())
        .map(|text| AgentReply {
            text,
            model_info: Some(codex_model_info(model)),
        })
        .ok_or_else(|| "codex returned no answer".to_owned())
}

fn progress_from_event(event: &serde_json::Value) -> Option<String> {
    let kind = event["type"].as_str()?;
    let item = &event["item"];
    let message = match (kind, item["type"].as_str()?) {
        ("item.started", "command_execution") => {
            Some(format!("Running: {}", item["command"].as_str()?))
        }
        ("item.completed", "file_change") => Some("Applying file changes".to_owned()),
        ("item.started", "mcp_tool_call") => Some(format!("Tool: {}", item["tool"].as_str()?)),
        ("item.started", "web_search") => Some(format!("Searching: {}", item["query"].as_str()?)),
        ("item.completed", "agent_message") => {
            Some(format!("Replying: {}", item["text"].as_str()?))
        }
        _ => None,
    }?;
    let first_line = message.lines().next().unwrap_or("");
    Some(
        first_line
            .chars()
            .filter(|character| !character.is_control())
            .take(100)
            .collect(),
    )
}

fn effort_config(level: &str) -> &'static str {
    match level {
        "xhigh" => "model_reasoning_effort=\"xhigh\"",
        "max" => "model_reasoning_effort=\"max\"",
        _ => unreachable!(),
    }
}

/// The interactive Codex CLI in the same sandbox as `codex?` or `codex!`.
pub fn interactive(
    cwd: &Path,
    model: Option<&str>,
    access: AgentAccess,
    effort: ReasoningEffort,
) -> Result<Command, String> {
    let mut command = agent_cli::workspace_command("codex", AgentUser::Current, access)?;
    let sandbox = match access {
        AgentAccess::ReadOnly => "read-only",
        AgentAccess::ReadWrite => "workspace-write",
    };
    command.args(["--sandbox", sandbox, "--ask-for-approval", "never"]);
    if let Some(model) = model {
        command.args(["--model", model]);
    }
    if let Some(level) = effort.level() {
        command.args(["--config", effort_config(level)]);
    }
    command.current_dir(cwd);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::{effort_config, exec_args, progress_from_event, request_for_access};
    use crate::session::{AgentAccess, ReasoningEffort};

    #[test]
    fn live_exec_keeps_write_sandbox_and_model() {
        let args = exec_args(
            Some("gpt-6-luna"),
            AgentAccess::ReadWrite,
            ReasoningEffort::Default,
        );
        assert!(args
            .windows(2)
            .any(|part| part == ["--sandbox", "workspace-write"]));
        assert!(args
            .windows(2)
            .any(|part| part == ["--model", "gpt-6-luna"]));
        assert_eq!(args.last(), Some(&"-"));
    }

    #[test]
    fn json_events_report_activity_without_using_command_output_as_answer() {
        let command = serde_json::json!({"type":"item.started","item":{
            "type":"command_execution","command":"git status","aggregated_output":"secret"
        }});
        assert_eq!(
            progress_from_event(&command).as_deref(),
            Some("Running: git status")
        );
        assert_eq!(
            progress_from_event(&serde_json::json!({
                "type":"item.completed","item":{"type":"agent_message","text":"Project is clean"}
            }))
            .as_deref(),
            Some("Replying: Project is clean")
        );
    }

    #[test]
    fn read_only_request_asks_for_one_line() {
        let request = request_for_access("Summarize this", AgentAccess::ReadOnly);
        assert!(request.starts_with("Summarize this\n\n"));
        assert!(request.contains("one concise plain-text line"));
        assert_eq!(
            request_for_access("Make a change", AgentAccess::ReadWrite),
            "Make a change"
        );
    }

    #[test]
    fn reasoning_effort_uses_codex_config_key() {
        assert_eq!(effort_config("xhigh"), "model_reasoning_effort=\"xhigh\"");
        assert_eq!(effort_config("max"), "model_reasoning_effort=\"max\"");
    }
}
