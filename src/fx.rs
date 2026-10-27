use crate::agent_cli::{self, AgentReply, AgentUser, ModelInfo, Request};
use crate::session::AgentAccess;
use std::path::Path;
use std::process::Command;

pub fn run(request: Request<'_>) -> Result<AgentReply, String> {
    let mut command = command(request.cwd, request.access, request.user)?;
    command.args(["ask", "--auto", "--json", "--no-save", "--no-color"]);
    let instruction = match request.access {
        AgentAccess::ReadOnly => "Inspect project files as needed. Do not change files or run commands that change files.",
        AgentAccess::ReadWrite => "You may change project files and run commands.",
    };
    let task = agent_cli::request_for_access(request.task, request.access);
    let input = agent_cli::session_input(request.context, &format!("{task}\n\n{instruction}"));
    let output = agent_cli::run_with_input(command, "fx", input)?;
    if !output.status.success() {
        return Err(format!(
            "fx {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    parse_result(&output.stdout)
}

fn command(cwd: &Path, access: AgentAccess, user: AgentUser) -> Result<Command, String> {
    // fx has no native read-only mode. Never silently run fx? without the
    // operating-system protection that enforces its access level.
    if access == AgentAccess::ReadOnly && !cfg!(target_os = "macos") {
        return Err(
            "fx? requires the macOS read-only sandbox; use fx! for write access".to_owned(),
        );
    }
    let mut command = agent_cli::workspace_command("fx", user, access)?;
    command.current_dir(cwd).env("FX_AUTO_UPGRADE", "0");
    Ok(command)
}

pub fn interactive(cwd: &Path, access: AgentAccess) -> Result<Command, String> {
    command(cwd, access, AgentUser::Current)
}

fn parse_result(output: &[u8]) -> Result<AgentReply, String> {
    let value: serde_json::Value = serde_json::from_slice(output)
        .map_err(|error| format!("fx returned invalid JSON: {error}"))?;
    match value["exit_code"].as_i64() {
        Some(0) => {}
        Some(code) => {
            return Err(format!(
                "fx exit {code}: {}",
                value["error"].as_str().unwrap_or("request failed")
            ));
        }
        None => return Err("fx returned no exit_code".to_owned()),
    }
    let answer = value["final_output"]
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .ok_or("fx returned no final answer")?;
    let model = value["model"].as_str().map(|model| {
        model
            .chars()
            .filter(|c| !c.is_control())
            .take(120)
            .collect::<String>()
    });
    Ok(AgentReply {
        text: answer.to_owned(),
        model_info: Some(match model {
            Some(model) if !model.is_empty() => ModelInfo::Reported(vec![model]),
            _ => ModelInfo::Unreported,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_only_the_completed_answer_and_reported_model() {
        let reply = parse_result(br#"{"output":"Working...\nDone","final_output":"Done","exit_code":0,"model":"provider/model"}"#).unwrap();
        assert_eq!(reply.text, "Done");
        assert_eq!(
            reply.model_info,
            Some(ModelInfo::Reported(vec!["provider/model".into()]))
        );
    }

    #[test]
    fn rejects_failures_and_incomplete_answers() {
        assert!(
            parse_result(br#"{"exit_code":1,"error":"denied","final_output":"partial"}"#)
                .unwrap_err()
                .contains("denied")
        );
        assert!(parse_result(br#"{"exit_code":0,"output":"progress","final_output":""}"#).is_err());
        assert!(parse_result(br#"{"final_output":"missing status"}"#).is_err());
        assert!(parse_result(b"not JSON").is_err());
    }

    #[test]
    fn read_only_requires_a_sandbox_and_write_access_runs_fx() {
        let cwd = Path::new(".");
        let write = command(cwd, AgentAccess::ReadWrite, AgentUser::Current).unwrap();
        assert_eq!(write.get_program(), "fx");
        let read = command(cwd, AgentAccess::ReadOnly, AgentUser::Current);
        if cfg!(target_os = "macos") {
            let read = read.unwrap();
            assert_eq!(read.get_program(), "/usr/bin/sandbox-exec");
            assert!(read.get_args().any(|arg| arg == "fx"));
        } else {
            assert!(read.is_err());
        }
    }
}
