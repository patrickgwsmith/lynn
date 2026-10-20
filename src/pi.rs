use crate::agent_cli::{self, AgentUser};
use crate::session::AgentAccess;
use std::path::Path;
use std::process::Command;

pub fn run(
    task: &str,
    cwd: &Path,
    context: &str,
    access: AgentAccess,
    user: AgentUser,
) -> Result<String, String> {
    let task = agent_cli::request_for_access(task, access);
    super::agent_cli::run(
        "pi",
        &[
            "--print",
            "--no-session",
            "--no-extensions",
            "--no-approve",
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
    )
}

/// The interactive Pi CLI with the same tools as `pi?` or `pi!`.
pub fn interactive(cwd: &Path, access: AgentAccess) -> Result<Command, String> {
    let mut command = agent_cli::workspace_command("pi", AgentUser::Current, access)?;
    command
        .args([
            "--no-session",
            "--no-extensions",
            "--tools",
            match access {
                AgentAccess::ReadOnly => "read,grep,find,ls",
                AgentAccess::ReadWrite => "read,grep,find,ls,edit,write,bash",
            },
        ])
        .current_dir(cwd);
    Ok(command)
}
