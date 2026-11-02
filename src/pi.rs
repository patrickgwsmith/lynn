use crate::agent_cli::{self, AgentReply, AgentUser, ModelInfo};
use crate::session::AgentAccess;
use std::path::Path;
use std::process::{Command, Stdio};

/// Pi's models as `provider/id`, listed fresh so new models need no Lynn release.
pub fn models() -> Vec<String> {
    Command::new("pi")
        .arg("--list-models")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|output| parse_models(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or_default()
}

/// The provider and model columns of `pi --list-models`.
fn parse_models(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| {
            let mut columns = line.split_whitespace();
            let provider = columns.next()?;
            let id = columns.next()?;
            (provider != "provider").then(|| format!("{provider}/{id}"))
        })
        .collect()
}

pub fn run(
    task: &str,
    cwd: &Path,
    context: &str,
    model: Option<&str>,
    access: AgentAccess,
    user: AgentUser,
) -> Result<AgentReply, String> {
    let model = model.ok_or_else(choose_model_error)?;
    let task = agent_cli::request_for_access(task, access);
    let text = super::agent_cli::run(
        "pi",
        &[
            "--print",
            "--no-session",
            "--no-extensions",
            "--no-approve",
            "--model",
            model,
            "--tools",
            match access {
                AgentAccess::ReadOnly => "read,grep,find,ls",
                AgentAccess::ReadWrite => "read,grep,find,ls,edit,write,bash",
            },
            match access {
                AgentAccess::ReadOnly => "Answer the Lynn user request supplied on stdin. Use the Lynn session context and inspect project files as needed. Do not change files.",
                AgentAccess::ReadWrite => "Carry out the Lynn user request supplied on stdin. Use the Lynn session context and project files as needed. You may change files and run commands.",
            },
        ],
        &task,
        cwd,
        context,
        user,
        access,
    )?;
    Ok(AgentReply {
        text,
        model_info: Some(ModelInfo::Requested(model.to_owned())),
    })
}

/// The interactive Pi CLI with the same tools as `pi?` or `pi!`.
pub fn interactive(
    cwd: &Path,
    model: Option<&str>,
    access: AgentAccess,
) -> Result<Command, String> {
    let model = model.ok_or_else(choose_model_error)?;
    let mut command = agent_cli::workspace_command("pi", AgentUser::Current, access)?;
    command
        .args([
            "--no-session",
            "--no-extensions",
            "--model",
            model,
            "--tools",
            match access {
                AgentAccess::ReadOnly => "read,grep,find,ls",
                AgentAccess::ReadWrite => "read,grep,find,ls,edit,write,bash",
            },
        ])
        .current_dir(cwd);
    Ok(command)
}

pub fn choose_model_error() -> String {
    "choose a Pi model first with pi.<model>? task; press Tab after pi to list models".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pi_model_listing() {
        let listing = "provider      model          context  max-out  thinking  images\n\
                       kapitsa       qwen3.5-9b     262.1K   32.8K    yes       no\n\
                       openai-codex  gpt-6-sol      272K     128K     yes       yes\n";
        assert_eq!(
            parse_models(listing),
            ["kapitsa/qwen3.5-9b", "openai-codex/gpt-6-sol"]
        );
    }
}
