use crate::agent_cli;
use crate::session::{self, AgentAccess};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

const SCHEMA: &str = r#"{
  "title": "LynnStep",
  "type": "object",
  "additionalProperties": false,
  "x-order": ["action", "path", "text"],
  "required": ["action", "path", "text"],
  "properties": {
    "action": { "type": "string", "enum": ["list", "read", "write", "run", "final"] },
    "path": { "type": "string" },
    "text": { "type": "string" }
  }
}"#;

const FINAL_SCHEMA: &str = r#"{
  "title": "LynnFinal",
  "type": "object",
  "additionalProperties": false,
  "x-order": ["action", "path", "text"],
  "required": ["action", "path", "text"],
  "properties": {
    "action": { "type": "string", "enum": ["final"] },
    "path": { "type": "string" },
    "text": { "type": "string" }
  }
}"#;

const MAX_STEPS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StepMode {
    Tools,
    Final,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskScope {
    Workspace,
    ReferenceSummary,
    SessionSummary,
}

fn task_scope(task: &str, references: &str) -> TaskScope {
    let first_word = task
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let summarizes = matches!(
        first_word.as_str(),
        "summarize" | "summarise" | "shorten" | "rephrase"
    );
    let references_reply = task
        .split(|character: char| {
            !(character.is_ascii_alphanumeric()
                || character == '#'
                || character == '_'
                || character == '-'
                || character == '?'
                || character == '!')
        })
        .any(|token| {
            let token = token.trim_end_matches(['?', '!']);
            session::agent_number_reference(token).is_some()
                || session::agent_reference(token).is_some()
        });
    let mentions_files = task.split_whitespace().any(|word| {
        let word = word.to_ascii_lowercase();
        let file_extension = word.rsplit_once('.').is_some_and(|(_, extension)| {
            !extension.is_empty()
                && extension.len() <= 8
                && extension.bytes().all(|byte| byte.is_ascii_alphabetic())
        });
        (word.contains('/')
            && (word.starts_with('/')
                || word.starts_with("./")
                || word.starts_with("../")
                || word.starts_with("~/")
                || word.ends_with('/')
                || word.contains('.')
                || word.matches('/').count() > 1))
            || file_extension
            || matches!(
                word.as_str(),
                "file"
                    | "files"
                    | "directory"
                    | "project"
                    | "repository"
                    | "repo"
                    | "source"
                    | "code"
            )
    });
    if !summarizes || mentions_files {
        return TaskScope::Workspace;
    }
    if references_reply && !references.is_empty() {
        TaskScope::ReferenceSummary
    } else {
        TaskScope::SessionSummary
    }
}

const INSTRUCTIONS: &str = "You are Lynn's coding agent. Respond with exactly one JSON step matching the supplied schema. Lynn executes each action and sends back the result in the next prompt. You cannot inspect project files directly: choose action=list to list a directory or read to read a file, then choose final only after the results answer the user's question. For list/read use path relative to Workspace; use '.' for its root. To replace a file, choose write with path and full replacement text. To execute a command, choose run with the command in text. For final, put the answer in text. Use an empty string for unused fields. Inspect relevant files before changing them. Do not claim work succeeded without observing its result.";

struct SchemaFile(PathBuf);

impl SchemaFile {
    fn new(schema: &str) -> Result<Self, String> {
        let id = NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("lynn-fm-{}-{id}.json", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("create fm schema: {e}"))?;
        file.write_all(schema.as_bytes())
            .map_err(|e| format!("write fm schema: {e}"))?;
        Ok(Self(path))
    }
}

impl Drop for SchemaFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn run(
    task: &str,
    cwd: &Path,
    context: &str,
    references: &str,
    access: AgentAccess,
) -> Result<String, String> {
    match task_scope(task, references) {
        TaskScope::ReferenceSummary => {
            return respond_text(&summary_prompt(task, references, access))
        }
        TaskScope::SessionSummary => return respond_text(&summary_prompt(task, context, access)),
        TaskScope::Workspace => {}
    }
    let root = cwd.canonicalize().map_err(|e| format!("workspace: {e}"))?;
    let schema = SchemaFile::new(SCHEMA)?;
    let final_schema = SchemaFile::new(FINAL_SCHEMA)?;
    run_with(&root, task, context, access, |mode, prompt| {
        let schema = match mode {
            StepMode::Tools => &schema.0,
            StepMode::Final => &final_schema.0,
        };
        respond(schema, prompt)
    })
}

fn summary_prompt(task: &str, source: &str, access: AgentAccess) -> String {
    let reply_instruction = if access == AgentAccess::ReadOnly {
        agent_cli::READ_ONLY_REPLY_INSTRUCTION
    } else {
        "Keep the answer concise."
    };
    format!(
        "Answer the request from the supplied Lynn session results. Use relevant figures from earlier replies when present. If the requested facts are absent, say so; do not invent them.\n\nRequest: {task}\n\nLynn session results:\n{source}\n\n{reply_instruction}"
    )
}

fn run_with(
    root: &Path,
    task: &str,
    context: &str,
    access: AgentAccess,
    mut respond: impl FnMut(StepMode, &str) -> Result<Value, String>,
) -> Result<String, String> {
    let mut observations = format!("Workspace entries (root):\n{}\n", list(root, ".")?);
    let mut seen: HashMap<(String, String, String), String> = HashMap::new();
    let mut next_mode = StepMode::Tools;
    for step_number in 0..MAX_STEPS {
        let mode = if next_mode == StepMode::Final || step_number == MAX_STEPS - 1 {
            StepMode::Final
        } else {
            StepMode::Tools
        };
        let access_instruction = if access == AgentAccess::ReadWrite {
            "You may use list, read, write, run, and final."
        } else {
            "This is read-only. You may use only list, read, and final. Never choose write or run."
        };
        let next_step = match mode {
            StepMode::Tools => "Choose the next step. If the tool results answer the task, choose final now. Never repeat an identical tool call.",
            StepMode::Final => "No more tool calls are available. Choose final now. Answer from the observed results, and say clearly if they are insufficient.",
        };
        let reply_instruction = if access == AgentAccess::ReadOnly {
            format!(
                "For the final answer's text field: {} Keep all steps in the required JSON schema.",
                agent_cli::READ_ONLY_REPLY_INSTRUCTION
            )
        } else {
            String::new()
        };
        let prompt = format!(
            "Task: {task}\nWorkspace: {}\n{access_instruction}\nShared session:\n{context}\nPrior tool results:\n{observations}\n{next_step}\n{reply_instruction}",
            root.display()
        );
        let step = respond(mode, &prompt)?;
        if std::env::var_os("LYNN_FM_DEBUG").is_some() {
            eprintln!("fm step: {step}");
        }
        let action = step["action"].as_str().ok_or("fm omitted action")?;
        let path = step["path"].as_str().unwrap_or("");
        let text = step["text"].as_str().unwrap_or("");
        if action == "final" {
            return Ok(text.to_owned());
        }
        if mode == StepMode::Final {
            return Err(format!(
                "fm did not provide a final answer; last action: {action} {path}"
            ));
        }
        let key_path = if matches!(action, "list" | "read") {
            inside(root, path)
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|_| path.to_owned())
        } else {
            path.to_owned()
        };
        let key = (action.to_owned(), key_path, text.to_owned());
        if let Some(previous_result) = seen.get(&key) {
            observations.push_str(&format!(
                "\nRepeated {action} {path} was skipped. Earlier result: {}\n",
                limit(previous_result, 4_000)
            ));
            next_mode = StepMode::Final;
            continue;
        }
        let result = match action {
            "list" => list(root, path),
            "read" => read(root, path),
            "write" if access == AgentAccess::ReadWrite => write(root, path, text),
            "run" if access == AgentAccess::ReadWrite => run_command(root, text),
            _ => Err("tool denied for this request".into()),
        };
        let result = match result {
            Ok(output) => output,
            Err(error) => format!("ERROR: {error}"),
        };
        seen.insert(key, result.clone());
        observations.push_str(&format!("\n{action} {path}: {}\n", limit(&result, 12_000)));
        if observations.len() > 32_000 {
            observations = tail(&observations, 24_000);
        }
    }
    Err("fm did not provide a final answer".into())
}

fn respond_text(prompt: &str) -> Result<String, String> {
    let output = Command::new("/usr/bin/fm")
        .args(["respond", "--no-stream", prompt])
        .output()
        .map_err(|error| format!("start fm: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "fm: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let answer = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if answer.is_empty() {
        return Err("fm returned no answer".into());
    }
    Ok(answer)
}

fn respond(schema: &Path, prompt: &str) -> Result<Value, String> {
    let mut child = Command::new("/usr/bin/fm")
        .args([
            "respond",
            "--no-stream",
            "--instructions",
            INSTRUCTIONS,
            "--schema",
        ])
        .arg(schema)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("start fm: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .map_err(|e| format!("prompt fm: {e}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("wait for fm: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "fm: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("parse fm response: {e}"))
}

fn inside(root: &Path, path: &str) -> Result<PathBuf, String> {
    let path = if path.is_empty() {
        Path::new(".")
    } else {
        Path::new(path)
    };
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        root.join(path)
    };
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if canonical.starts_with(root) {
        Ok(canonical)
    } else {
        Err("path is outside the workspace".into())
    }
}

fn list(root: &Path, path: &str) -> Result<String, String> {
    let path = inside(root, path)?;
    let mut names = Vec::new();
    for entry in fs::read_dir(&path).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let mut name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            name.push('/');
        }
        names.push(name);
    }
    names.sort();
    Ok(limit(&names.join("\n"), 12_000))
}

fn read(root: &Path, path: &str) -> Result<String, String> {
    let path = inside(root, path)?;
    let meta = fs::metadata(&path).map_err(|e| e.to_string())?;
    if meta.len() > 64_000 {
        return Err("file exceeds 64 KB read limit".into());
    }
    fs::read_to_string(path).map_err(|e| e.to_string())
}

fn write(root: &Path, path: &str, text: &str) -> Result<String, String> {
    let candidate = root.join(path);
    let parent = candidate.parent().ok_or("write needs a file path")?;
    let parent = inside(root, &parent.to_string_lossy())?;
    let name = candidate.file_name().ok_or("write needs a file name")?;
    let target = parent.join(name);
    if target.exists() {
        inside(root, &target.to_string_lossy())?;
    }
    fs::write(&target, text).map_err(|e| format!("{}: {e}", target.display()))?;
    Ok(format!(
        "wrote {} bytes to {}",
        text.len(),
        target.display()
    ))
}

fn run_command(root: &Path, command: &str) -> Result<String, String> {
    let output = Command::new("/bin/sh")
        .args(["-c", command])
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "exit: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        limit(&String::from_utf8_lossy(&output.stdout), 8_000),
        limit(&String::from_utf8_lossy(&output.stderr), 4_000)
    ))
}

fn limit(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let boundary = text.floor_char_boundary(max);
    format!("{}\n[truncated]", &text[..boundary])
}

pub fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::{
        list, read, run_with, summary_prompt, task_scope, AgentAccess, StepMode, TaskScope,
        MAX_STEPS,
    };
    use serde_json::json;
    use std::fs;

    #[test]
    fn lists_and_reads_workspace_files() {
        let root = std::env::temp_dir().join(format!("lynn-fm-read-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("note.txt"), "hello").unwrap();
        let root = root.canonicalize().unwrap();
        assert!(list(&root, ".")
            .unwrap()
            .lines()
            .any(|name| name == "note.txt"));
        assert_eq!(read(&root, "note.txt").unwrap(), "hello");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repeated_tool_call_moves_to_final_answer() {
        let root = std::env::current_dir().unwrap();
        let mut modes = Vec::new();
        let answer = run_with(
            &root,
            "list files",
            "",
            AgentAccess::ReadOnly,
            |mode, prompt| {
                modes.push(mode);
                match modes.len() {
                    1 | 2 => Ok(json!({"action":"list", "path":".", "text":""})),
                    3 => {
                        assert!(prompt.contains("Repeated list . was skipped"));
                        Ok(json!({"action":"final", "path":"", "text":"done"}))
                    }
                    _ => panic!("unexpected fm step"),
                }
            },
        )
        .unwrap();
        assert_eq!(answer, "done");
        assert_eq!(modes, [StepMode::Tools, StepMode::Tools, StepMode::Final]);
    }

    #[test]
    fn last_step_requires_a_final_answer() {
        let root = std::env::current_dir().unwrap();
        let mut steps = 0;
        let answer = run_with(
            &root,
            "inspect files",
            "",
            AgentAccess::ReadOnly,
            |mode, _| {
                steps += 1;
                if steps == MAX_STEPS {
                    assert_eq!(mode, StepMode::Final);
                    Ok(json!({"action":"final", "path":"", "text":"insufficient evidence"}))
                } else {
                    assert_eq!(mode, StepMode::Tools);
                    Ok(json!({"action":"read", "path":format!("missing-{steps}"), "text":""}))
                }
            },
        )
        .unwrap();
        assert_eq!(answer, "insufficient evidence");
        assert_eq!(steps, MAX_STEPS);
    }

    #[test]
    fn summarizing_a_reply_uses_direct_prompt() {
        let references = "Referenced reply luna?#3:\nA detailed answer about the project.\n";
        assert_eq!(
            task_scope("Summarize #3 in 5 words", references),
            TaskScope::ReferenceSummary
        );
        assert_eq!(
            task_scope("Summarize luna?#3?", references),
            TaskScope::ReferenceSummary
        );
        assert_eq!(
            task_scope("Summarize #3 and inspect src/main.rs", references),
            TaskScope::Workspace
        );
        assert!(
            summary_prompt("Summarize #3 in 5 words", references, AgentAccess::ReadOnly)
                .contains(references)
        );
    }

    #[test]
    fn summarizing_session_results_does_not_start_workspace_inspection() {
        assert_eq!(
            task_scope("summarize the ops/s", ""),
            TaskScope::SessionSummary
        );
        assert_eq!(
            task_scope("summarize the ops/s in results.txt", ""),
            TaskScope::Workspace
        );
        let context = "response (luna?#3): Compiled: 120 ops/s; interpreted: 20 ops/s.\n";
        let prompt = summary_prompt("summarize the ops/s", context, AgentAccess::ReadOnly);
        assert!(prompt.contains(context));
        assert!(prompt.contains("Request: summarize the ops/s"));
    }
}
