use std::io::{ErrorKind, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;

use crate::session::{AgentAccess, ReasoningEffort};

pub const READ_ONLY_REPLY_INSTRUCTION: &str =
    "Answer in one concise plain-text line, preferably under 40 words. Give the finding directly; omit headings, Markdown, preambles, and extra explanation. If the user explicitly requests a longer or structured answer, follow that request.";

pub fn request_for_access(task: &str, access: AgentAccess) -> String {
    match access {
        AgentAccess::ReadOnly => format!("{task}\n\n{READ_ONLY_REPLY_INSTRUCTION}"),
        AgentAccess::ReadWrite => task.to_owned(),
    }
}

/// Paths under the agent user's home directory that agent CLIs write state,
/// credentials, and caches to, and whether each is matched as a name prefix
/// (for sibling backup and lock files) rather than a directory subpath.
#[cfg(target_os = "macos")]
const READ_ONLY_HOME_WRITABLE: &[(&str, bool)] = &[
    (".claude", false),
    (".claude.json", true),
    (".codex", false),
    (".pi", false),
    (".cache", false),
    ("Library/Caches", false),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentUser {
    Current,
    Staffer,
}

/// One agent request as Claude Code and Codex receive it; `model` is the
/// backend's own model name, or `None` for the CLI default.
#[derive(Clone, Copy)]
pub struct Request<'a> {
    pub task: &'a str,
    pub cwd: &'a Path,
    pub context: &'a str,
    pub model: Option<&'a str>,
    pub access: AgentAccess,
    pub user: AgentUser,
    pub effort: ReasoningEffort,
}

/// Where the model detail shown on expansion came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelInfo {
    Reported(Vec<String>),
    Requested(String),
    Unreported,
}

/// The answer and any model detail available for expansion.
#[derive(Debug)]
pub struct AgentReply {
    pub text: String,
    pub model_info: Option<ModelInfo>,
}

impl From<&str> for AgentReply {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

impl From<String> for AgentReply {
    fn from(text: String) -> Self {
        Self {
            text,
            model_info: None,
        }
    }
}

pub fn command(program: &str, user: AgentUser) -> Command {
    match user {
        AgentUser::Current => Command::new(program),
        AgentUser::Staffer => {
            let mut command = Command::new("sudo");
            command.args(["-n", "-H", "-u", "staffer", "--", program]);
            command
        }
    }
}

pub fn workspace_command(
    program: &str,
    user: AgentUser,
    access: AgentAccess,
) -> Result<Command, String> {
    match access {
        AgentAccess::ReadOnly => read_only_command(program, user),
        AgentAccess::ReadWrite => Ok(command(program, user)),
    }
}

#[cfg(target_os = "macos")]
fn read_only_command(program: &str, user: AgentUser) -> Result<Command, String> {
    let profile = read_only_profile(&home_directory(user)?)?;
    let mut command = command("/usr/bin/sandbox-exec", user);
    command.args(["-p", &profile, program]);
    Ok(command)
}

#[cfg(not(target_os = "macos"))]
fn read_only_command(program: &str, user: AgentUser) -> Result<Command, String> {
    Ok(command(program, user))
}

/// Denies every file write except to devices, temporary directories, and the
/// agent CLIs' own state under `home`.
#[cfg(target_os = "macos")]
fn read_only_profile(home: &Path) -> Result<String, String> {
    let home = home
        .to_str()
        .ok_or("read-only agent home path is not valid UTF-8")?
        .trim_end_matches('/');
    let mut profile = String::from(
        "(version 1)(allow default)(deny file-write*)(allow file-write* \
         (subpath \"/dev\") (subpath \"/private/tmp\") (subpath \"/private/var/folders\")",
    );
    for (path, prefix) in READ_ONLY_HOME_WRITABLE {
        let filter = if *prefix { "prefix" } else { "subpath" };
        let path = sandbox_string(&format!("{home}/{path}"));
        profile.push_str(&format!(" ({filter} {path})"));
    }
    profile.push(')');
    Ok(profile)
}

#[cfg(target_os = "macos")]
fn sandbox_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(target_os = "macos")]
fn home_directory(user: AgentUser) -> Result<std::path::PathBuf, String> {
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;

    // SAFETY: getpwnam/getpwuid return null or a pointer to a static passwd
    // record, whose pw_dir is copied before any other passwd call.
    unsafe {
        let entry = match user {
            AgentUser::Current => libc::getpwuid(libc::getuid()),
            AgentUser::Staffer => libc::getpwnam(c"staffer".as_ptr()),
        };
        if entry.is_null() || (*entry).pw_dir.is_null() {
            return Err("read-only agent: cannot find the agent user's home directory".to_owned());
        }
        let home = CStr::from_ptr((*entry).pw_dir).to_bytes();
        Ok(Path::new(OsStr::from_bytes(home)).to_path_buf())
    }
}

/// Spawns `command` with piped stdio, feeds `input` on a separate thread so a
/// large input cannot deadlock against a full stdout or stderr pipe, and waits
/// for the output.
pub fn run_with_input(
    mut command: Command,
    program: &str,
    input: String,
) -> Result<Output, String> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{program}: {error}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("{program} stdin unavailable"))?;
    let writer = thread::spawn(move || stdin.write_all(input.as_bytes()));
    let output = child
        .wait_with_output()
        .map_err(|error| format!("{program}: {error}"))?;
    match writer.join() {
        Ok(Ok(())) => {}
        // The agent may exit without reading all of its input; its exit
        // status and output are more informative than the broken pipe.
        Ok(Err(error)) if error.kind() == ErrorKind::BrokenPipe => {}
        Ok(Err(error)) => return Err(format!("{program} stdin: {error}")),
        Err(_) => return Err(format!("{program} stdin writer panicked")),
    }
    Ok(output)
}

/// Runs an agent's own interactive CLI in the foreground on the terminal,
/// inside the read-only sandbox, and returns its exit status.
pub fn run_interactive(mut command: Command, program: &str) -> i32 {
    use std::os::unix::process::{CommandExt, ExitStatusExt};

    // SAFETY: the pre-exec hook only restores default signal dispositions.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_DFL);
            Ok(())
        });
    }
    match command.status() {
        Ok(status) => status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
        Err(error) => {
            eprintln!("lynn: {program}: {error}");
            if error.kind() == ErrorKind::NotFound {
                127
            } else {
                126
            }
        }
    }
}

pub fn session_input(context: &str, task: &str) -> String {
    format!("Lynn session context:\n{context}\nUser request:\n{task}\n")
}

pub fn run(
    program: &str,
    args: &[&str],
    task: &str,
    cwd: &Path,
    context: &str,
    user: AgentUser,
    access: AgentAccess,
) -> Result<String, String> {
    let mut command = workspace_command(program, user, access)?;
    command.args(args).current_dir(cwd);
    let output = run_with_input(command, program, session_input(context, task))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{program} {}: {}", output.status, stderr.trim()));
    }
    let answer = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if answer.is_empty() {
        return Err(format!("{program} returned no answer"));
    }
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::run_with_input;
    use std::process::Command;

    #[test]
    fn large_input_does_not_deadlock_against_output() {
        let input = "x".repeat(4 * 1024 * 1024);
        let output = run_with_input(Command::new("cat"), "cat", input.clone()).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), input.len());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn read_only_profile_denies_writes_outside_agent_state() {
        let profile = super::read_only_profile(std::path::Path::new("/Users/a \"b\"/")).unwrap();
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains(r#"(subpath "/Users/a \"b\"/.claude")"#));
        assert!(profile.contains(r#"(prefix "/Users/a \"b\"/.claude.json")"#));
        assert!(!profile.contains("(subpath \"/Users/a \\\"b\\\"\")"));
    }
}
