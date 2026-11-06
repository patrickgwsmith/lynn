use crate::agent_cli::{self, AgentReply, AgentUser, ModelInfo, Request};
use crate::session::{AgentAccess, ReasoningEffort};
use std::path::Path;
use std::process::Command;

pub fn run(request: Request<'_>) -> Result<AgentReply, String> {
    let mut command = command(
        request.cwd,
        request.model,
        request.access,
        request.user,
        request.effort,
    )?;
    let instruction = match request.access {
        AgentAccess::ReadOnly => "Inspect project files as needed. Do not change files or run commands that change files.",
        AgentAccess::ReadWrite => "Carry out the user's task and report what changed. You may change project files and run commands.",
    };
    let task = agent_cli::request_for_access(request.task, request.access);
    let input = agent_cli::session_input(request.context, &format!("{task}\n\n{instruction}"));
    if request.access == AgentAccess::ReadOnly {
        // Headless agy denies any command not allowed here and then ends the
        // turn with no answer, so let agy? run the usual read-only commands.
        command.args(["--settings", &read_only_settings()]);
    }
    // agy's print mode takes its prompt as the flag's value, not from stdin.
    command.args([
        "--output-format",
        "json",
        "--disable-slash-commands",
        &format!("--print={input}"),
    ]);
    let output = command.output().map_err(|error| format!("agy: {error}"))?;
    let reply = parse_result(&output.stdout, request.model);
    if !output.status.success() {
        let detail = match reply {
            Err(error) if output.stdout.is_empty() => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                stderr.trim().lines().last().unwrap_or(&error).to_owned()
            }
            Err(error) => error,
            Ok(_) => "request failed".to_owned(),
        };
        return Err(format!("agy {}: {detail}", output.status));
    }
    reply
}

fn command(
    cwd: &Path,
    model: Option<&str>,
    access: AgentAccess,
    user: AgentUser,
    effort: ReasoningEffort,
) -> Result<Command, String> {
    let mut command = agent_cli::workspace_command("agy", user, access)?;
    // agy ignores plan mode when slash commands are disabled, as they are in
    // print mode, so agy? relies on the read-only sandbox to stop edits.
    // Auto mode lets agy! edit and run safe commands.
    command.args([
        "--mode",
        match access {
            AgentAccess::ReadOnly => "plan",
            AgentAccess::ReadWrite => "auto",
        },
    ]);
    if let Some(model) = model {
        command.args(["--model", model]);
    }
    if let Some(level) = effort.level() {
        command.args(["--effort", level]);
    }
    command.current_dir(cwd).env("AGY_CLI_OSC7501", "off");
    Ok(command)
}

/// The interactive Antigravity CLI in the same mode as `agy?` or `agy!`.
pub fn interactive(
    cwd: &Path,
    model: Option<&str>,
    access: AgentAccess,
    effort: ReasoningEffort,
) -> Result<Command, String> {
    let mut command = command(cwd, model, access, AgentUser::Current, effort)?;
    command.env_remove("AGY_CLI_OSC7501");
    Ok(command)
}

/// Commands `agy?` may run without approval. agy matches each rule as a
/// command prefix, and a pipeline passes when every part of it matches.
const READ_ONLY_COMMANDS: &[&str] = &[
    "ls",
    "cat",
    "head",
    "tail",
    "wc",
    "rg",
    "grep",
    "file",
    "stat",
    "du",
    "pwd",
    "which",
    "diff",
    "git status",
    "git diff",
    "git log",
    "git show",
    "git blame",
    "git grep",
    "git ls-files",
    "git rev-parse",
];

fn read_only_settings() -> String {
    let allow: Vec<String> = READ_ONLY_COMMANDS
        .iter()
        .map(|command| format!("command({command})"))
        .collect();
    serde_json::json!({ "permissions": { "allow": allow } }).to_string()
}

fn parse_result(output: &[u8], requested: Option<&str>) -> Result<AgentReply, String> {
    let value: serde_json::Value = serde_json::from_slice(output)
        .map_err(|error| format!("agy returned invalid JSON: {error}"))?;
    if value["status"].as_str() == Some("ERROR") {
        return Err(value["error"]
            .as_str()
            .filter(|error| !error.trim().is_empty())
            .unwrap_or("request failed")
            .to_owned());
    }
    let answer = value["response"]
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| empty_answer_error(&value))?;
    let model = value["model"].as_str().map(|model| {
        model
            .chars()
            .filter(|c| !c.is_control())
            .take(120)
            .collect::<String>()
    });
    Ok(AgentReply {
        text: answer.trim().to_owned(),
        model_info: Some(match model {
            Some(model) if !model.is_empty() => ModelInfo::Reported(vec![model]),
            _ => match requested {
                Some(model) => ModelInfo::Requested(model.to_owned()),
                None => ModelInfo::Unreported,
            },
        }),
    })
}

/// Headless agy denies actions it would have asked about, then ends the turn
/// reporting success with an empty response and a `denied_actions` list.
fn empty_answer_error(value: &serde_json::Value) -> String {
    let mut denied: Vec<&str> = value["denied_actions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|action| action["action"].as_str())
        .collect();
    denied.dedup();
    if denied.is_empty() {
        return "agy returned no answer".to_owned();
    }
    let actions = denied.join(", ");
    if denied == ["command"] {
        format!(
            "agy stopped: it tried to run a command outside agy?'s allow-list ({}), which headless mode denies",
            READ_ONLY_COMMANDS.join(", ")
        )
    } else {
        format!("agy stopped: headless mode denied an action it tried ({actions})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_the_response_and_any_reported_model() {
        let reply = parse_result(
            br#"{"conversation_id":"c","status":"SUCCESS","response":" Done\n","num_turns":1}"#,
            None,
        )
        .unwrap();
        assert_eq!(reply.text, "Done");
        assert_eq!(reply.model_info, Some(ModelInfo::Unreported));
        let reply = parse_result(
            br#"{"status":"SUCCESS","response":"Done","model":"gemini"}"#,
            Some("gemini-3.8-flash-high"),
        )
        .unwrap();
        assert_eq!(
            reply.model_info,
            Some(ModelInfo::Reported(vec!["gemini".into()]))
        );
        let reply = parse_result(
            br#"{"status":"SUCCESS","response":"Done"}"#,
            Some("gemini-3.8-flash-high"),
        )
        .unwrap();
        assert_eq!(
            reply.model_info,
            Some(ModelInfo::Requested("gemini-3.8-flash-high".into()))
        );
    }

    #[test]
    fn rejects_errors_and_empty_answers() {
        let error = parse_result(
            br#"{"conversation_id":"","status":"ERROR","response":"","error":"authentication failed or timed out"}"#,
            None,
        )
        .unwrap_err();
        assert_eq!(error, "authentication failed or timed out");
        assert!(parse_result(br#"{"status":"SUCCESS","response":"  "}"#, None).is_err());
        assert!(parse_result(b"not JSON", None).is_err());
        assert_eq!(
            parse_result(br#"{"status":"SUCCESS","response":""}"#, None).unwrap_err(),
            "agy returned no answer"
        );
        let error = parse_result(
            br#"{"status":"SUCCESS","response":"","denied_actions":[{"action":"command","display_name":"RunCommand"}]}"#,
            None,
        )
        .unwrap_err();
        assert!(
            error.starts_with("agy stopped: it tried to run a command"),
            "{error}"
        );
        assert!(error.contains("rg, grep"), "{error}");
        let error = parse_result(
            br#"{"status":"SUCCESS","response":"","denied_actions":[{"action":"mcp"}]}"#,
            None,
        )
        .unwrap_err();
        assert_eq!(
            error,
            "agy stopped: headless mode denied an action it tried (mcp)"
        );
    }

    #[test]
    fn read_only_settings_allow_read_only_commands() {
        let settings: serde_json::Value = serde_json::from_str(&read_only_settings()).unwrap();
        let allow = settings["permissions"]["allow"].as_array().unwrap();
        assert!(allow.contains(&"command(rg)".into()));
        assert!(allow.contains(&"command(git log)".into()));
    }

    #[test]
    fn access_selects_the_agy_mode_model_and_effort() {
        let args = |command: &Command| {
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        let cwd = Path::new(".");
        let write = command(
            cwd,
            Some("gemini-3.8-flash-high"),
            AgentAccess::ReadWrite,
            AgentUser::Current,
            ReasoningEffort::Maximum,
        )
        .unwrap();
        assert_eq!(write.get_program(), "agy");
        assert_eq!(
            args(&write),
            [
                "--mode",
                "auto",
                "--model",
                "gemini-3.8-flash-high",
                "--effort",
                "max"
            ]
        );
        let read = command(
            cwd,
            None,
            AgentAccess::ReadOnly,
            AgentUser::Current,
            ReasoningEffort::Default,
        )
        .unwrap();
        assert!(args(&read).ends_with(&["--mode".into(), "plan".into()]));
        if cfg!(target_os = "macos") {
            assert_eq!(read.get_program(), "/usr/bin/sandbox-exec");
        }
    }
}
