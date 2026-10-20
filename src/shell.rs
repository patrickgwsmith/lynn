use crate::session;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellExecution {
    CurrentUser,
    Staffer,
}

impl ShellExecution {
    pub fn suffix(self) -> char {
        match self {
            Self::CurrentUser => '!',
            Self::Staffer => '?',
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Reference(String),
    Pipe,
    In,
    Out,
    Append,
    ErrOut,
    ErrAppend,
}

#[derive(Default, Debug)]
struct Stage {
    args: Vec<String>,
    reference_args: BTreeSet<usize>,
    stdin: Option<String>,
    stdin_reference: bool,
    stdout: Option<(String, bool)>,
    stderr: Option<(String, bool)>,
}

fn word_token(word: String, literal: bool) -> Token {
    if !literal
        && (session::agent_number_reference(&word).is_some()
            || session::watch_revision_reference(&word).is_some()
            || session::agent_reference(&word).is_some())
    {
        Token::Reference(word)
    } else {
        Token::Word(word)
    }
}

fn expand(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, last_status: i32) -> String {
    if chars.peek() == Some(&'?') {
        chars.next();
        return last_status.to_string();
    }
    let mut name = String::new();
    if chars.peek() == Some(&'{') {
        chars.next();
        while let Some(&c) = chars.peek() {
            chars.next();
            if c == '}' {
                break;
            }
            name.push(c);
        }
    } else {
        while let Some(&c) = chars.peek() {
            if !c.is_ascii_alphanumeric() && c != '_' {
                break;
            }
            name.push(c);
            chars.next();
        }
    }
    if name.is_empty() {
        "$".into()
    } else {
        std::env::var(&name).unwrap_or_default()
    }
}

fn lex(input: &str, last_status: i32) -> Result<Vec<Token>, String> {
    let mut chars = input.chars().peekable();
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut literal = false;
    let mut quote = None;
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '$' && q == '"' {
                word.push_str(&expand(&mut chars, last_status));
            } else if c == '\\' && q == '"' {
                if let Some(next) = chars.next() {
                    if !matches!(next, '$' | '"' | '\\' | '\n') {
                        word.push('\\');
                    }
                    if next != '\n' {
                        word.push(next);
                    }
                } else {
                    return Err("trailing escape".into());
                }
            } else {
                word.push(c);
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                started = true;
                literal = true;
            }
            '\\' => {
                word.push(chars.next().ok_or("trailing escape")?);
                started = true;
                literal = true;
            }
            '$' => {
                word.push_str(&expand(&mut chars, last_status));
                started = true;
                literal = true;
            }
            '~' if !started && matches!(chars.peek(), None | Some(' ' | '\t' | '/')) => {
                word.push_str(&std::env::var("HOME").unwrap_or_else(|_| "~".into()));
                started = true;
            }
            ' ' | '\t' => {
                if started {
                    tokens.push(word_token(std::mem::take(&mut word), literal));
                    started = false;
                    literal = false;
                }
            }
            '|' | '<' | '>' => {
                if started {
                    tokens.push(word_token(std::mem::take(&mut word), literal));
                    started = false;
                    literal = false;
                }
                tokens.push(match c {
                    '|' => Token::Pipe,
                    '<' => Token::In,
                    _ if chars.peek() == Some(&'>') => {
                        chars.next();
                        Token::Append
                    }
                    _ => Token::Out,
                });
            }
            '2' if !started && chars.peek() == Some(&'>') => {
                chars.next();
                let append = chars.peek() == Some(&'>');
                if append {
                    chars.next();
                }
                tokens.push(if append {
                    Token::ErrAppend
                } else {
                    Token::ErrOut
                });
            }
            _ => {
                word.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Err("unclosed quote".into());
    }
    if started {
        tokens.push(word_token(word, literal));
    }
    Ok(tokens)
}

fn parse(input: &str, last_status: i32) -> Result<Vec<Stage>, String> {
    let tokens = lex(input, last_status)?;
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    let mut stages = vec![Stage::default()];
    let mut iter = tokens.into_iter();
    while let Some(token) = iter.next() {
        let stage = stages.last_mut().unwrap();
        match token {
            Token::Word(s) => stage.args.push(s),
            Token::Reference(s) => {
                stage.reference_args.insert(stage.args.len());
                stage.args.push(s);
            }
            Token::Pipe => {
                if stage.args.is_empty() {
                    return Err("empty pipeline stage".into());
                }
                stages.push(Stage::default());
            }
            op => {
                let (path, reference) = match iter.next() {
                    Some(Token::Word(s)) => (s, false),
                    Some(Token::Reference(s)) => (s, true),
                    _ => return Err("redirection needs a filename".into()),
                };
                if reference && op != Token::In {
                    return Err("session references cannot be output destinations; quote the filename to use it literally".into());
                }
                match op {
                    Token::In => {
                        stage.stdin = Some(path);
                        stage.stdin_reference = reference;
                    }
                    Token::Out => stage.stdout = Some((path, false)),
                    Token::Append => stage.stdout = Some((path, true)),
                    Token::ErrOut => stage.stderr = Some((path, false)),
                    Token::ErrAppend => stage.stderr = Some((path, true)),
                    _ => unreachable!(),
                }
            }
        }
    }
    if stages.last().unwrap().args.is_empty() {
        return Err("empty pipeline stage".into());
    }
    for stage in &stages {
        let program = stage
            .args
            .iter()
            .position(|arg| !arg.split_once('=').is_some_and(|(key, _)| valid_name(key)));
        if program.is_some_and(|index| stage.reference_args.contains(&index)) {
            return Err("a session reference cannot be a command; use cat #n".into());
        }
        if program.is_some_and(|index| {
            matches!(
                stage.args[index].as_str(),
                "cd" | "exit" | "export" | "unset"
            )
        }) && (!stage.reference_args.is_empty() || stage.stdin_reference)
        {
            return Err("session reference inputs require an external command".into());
        }
    }
    Ok(stages)
}

/// Unquoted references in arguments or input redirections, in command order.
pub fn reference_arguments(input: &str, last_status: i32) -> Result<Vec<String>, String> {
    let mut references = Vec::new();
    for stage in parse(input, last_status)? {
        references.extend(
            stage
                .reference_args
                .iter()
                .map(|&index| stage.args[index].clone()),
        );
        if stage.stdin_reference {
            references.push(stage.stdin.unwrap());
        }
    }
    Ok(references)
}

struct ReferenceInput {
    reader: OwnedFd,
    writer: File,
    text: String,
}

impl ReferenceInput {
    fn new(text: String) -> io::Result<Self> {
        let mut fds = [-1; 2];
        #[cfg(target_os = "linux")]
        let result = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        #[cfg(not(target_os = "linux"))]
        let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        let mut ends = unsafe { [OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])] };
        for end in &mut ends {
            // Keep these private to the parent until the intended child's pre_exec.
            if unsafe { libc::fcntl(end.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
                return Err(io::Error::last_os_error());
            }
            // Stdio setup must not overwrite a reference if stdin/stdout was closed.
            if end.as_raw_fd() < 3 {
                let fd = unsafe { libc::fcntl(end.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
                if fd == -1 {
                    return Err(io::Error::last_os_error());
                }
                *end = unsafe { OwnedFd::from_raw_fd(fd) };
            }
        }
        let [reader, writer] = ends;
        Ok(Self {
            reader,
            writer: File::from(writer),
            text,
        })
    }

    fn path(&self) -> String {
        format!("/dev/fd/{}", self.reader.as_raw_fd())
    }

    fn feed(self) -> JoinHandle<io::Result<()>> {
        let Self {
            reader,
            writer,
            text,
        } = self;
        // Retaining a parent reader would prevent EPIPE when the child stops reading.
        drop(reader);
        feed_reference(writer, text)
    }
}

fn feed_reference(mut writer: File, text: String) -> JoinHandle<io::Result<()>> {
    thread::spawn(move || match writer.write_all(text.as_bytes()) {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    })
}

#[derive(Default)]
struct Pipeline {
    children: Vec<Child>,
    writers: Vec<JoinHandle<io::Result<()>>>,
}

impl Pipeline {
    fn finish(&mut self) -> Result<(), String> {
        let mut first_error = None;
        for writer in self.writers.drain(..) {
            let result = writer
                .join()
                .map_err(|_| "reference input writer panicked".to_owned())
                .and_then(|result| result.map_err(|error| format!("reference input: {error}")));
            if let Err(error) = result {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        for child in &mut self.children {
            // A waited child is already reaped; kill() then does nothing.
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = self.finish();
    }
}

fn output_file(path: &str, append: bool) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(path)
}

pub enum Outcome {
    Status(i32),
    Exit(i32),
}

pub fn reader_shell_command(program: &str, cwd: &Path, noninteractive: bool) -> Command {
    let mut command = Command::new("sudo");
    if noninteractive {
        command.arg("-n");
    }
    command.args(["-H", "-u", "staffer", "--"]);
    if let Some(root) = owned_git_root(cwd) {
        command.arg("/usr/bin/env");
        command.args(["GIT_CONFIG_COUNT=1", "GIT_CONFIG_KEY_0=safe.directory"]);
        let mut value = OsString::from("GIT_CONFIG_VALUE_0=");
        value.push(root.as_os_str());
        command.arg(value);
    }
    command.arg(format!("/bin/{program}"));
    command
}

fn owned_git_root(cwd: &Path) -> Option<PathBuf> {
    let owner = unsafe { libc::geteuid() };
    for directory in cwd.ancestors() {
        let git_path = directory.join(".git");
        let git_metadata = match fs::metadata(&git_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        let root = fs::canonicalize(directory).ok()?;
        if fs::metadata(&root).ok()?.uid() != owner || git_metadata.uid() != owner {
            return None;
        }
        if git_metadata.is_file() {
            let git_file = fs::read_to_string(&git_path).ok()?;
            let path = git_file.lines().next()?.strip_prefix("gitdir: ")?.trim();
            let git_dir = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                directory.join(path)
            };
            if fs::metadata(git_dir).ok()?.uid() != owner {
                return None;
            }
        } else if !git_metadata.is_dir() {
            return None;
        }
        return Some(root);
    }
    None
}

pub fn execute_host_shell(
    program: &str,
    script: &str,
    execution: ShellExecution,
) -> Result<i32, String> {
    if script.trim().is_empty() {
        return Err(format!(
            "{program}{}: expected a command",
            execution.suffix()
        ));
    }
    let mut child = match execution {
        ShellExecution::Staffer => {
            let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
            reader_shell_command(program, &cwd, unsafe { libc::isatty(0) } != 1)
        }
        ShellExecution::CurrentUser => Command::new(program),
    };
    child.arg("-c").arg(script);
    unsafe {
        child.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGQUIT, libc::SIG_DFL);
            Ok(())
        });
    }
    let status = match child.status() {
        Ok(status) => status,
        Err(error) => {
            eprintln!("lynn: {program}: {error}");
            return Ok(if error.kind() == io::ErrorKind::NotFound {
                127
            } else {
                126
            });
        }
    };
    use std::os::unix::process::ExitStatusExt;
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
}

pub fn execute_substitution_shell(script: &str) -> Result<i32, String> {
    let shell = std::env::var("SHELL").ok();
    let program = shell.as_deref().filter(|shell| {
        matches!(
            Path::new(shell).file_name().and_then(|name| name.to_str()),
            Some("bash" | "zsh")
        )
    });
    let fallback = if cfg!(target_os = "macos") {
        "zsh"
    } else {
        "bash"
    };
    execute_host_shell(
        program.unwrap_or(fallback),
        script,
        ShellExecution::CurrentUser,
    )
}

pub fn execute(input: &str, last_status: i32) -> Result<Outcome, String> {
    execute_with_references(input, last_status, &BTreeMap::new())
}

pub fn execute_with_references(
    input: &str,
    last_status: i32,
    references: &BTreeMap<String, String>,
) -> Result<Outcome, String> {
    let mut stages = parse(input, last_status)?;
    if stages.is_empty() {
        return Ok(Outcome::Status(0));
    }
    // Validate the whole selection before opening/truncating any output file.
    for stage in &stages {
        for &index in &stage.reference_args {
            if !references.contains_key(&stage.args[index]) {
                return Err(format!("no result supplied for {}", stage.args[index]));
            }
        }
        if stage.stdin_reference && !references.contains_key(stage.stdin.as_ref().unwrap()) {
            return Err(format!(
                "no result supplied for {}",
                stage.stdin.as_ref().unwrap()
            ));
        }
    }
    if stages.len() == 1 {
        let args = &stages[0].args;
        match args[0].as_str() {
            "cd" => {
                if args.len() > 2 {
                    return Err("cd: too many arguments".into());
                }
                let target = if args.len() == 1 {
                    std::env::var("HOME").map_err(|_| "cd: HOME is unset")?
                } else if args[1] == "-" {
                    std::env::var("OLDPWD").map_err(|_| "cd: OLDPWD is unset")?
                } else {
                    args[1].clone()
                };
                let old = std::env::current_dir().map_err(|e| e.to_string())?;
                std::env::set_current_dir(&target).map_err(|e| format!("cd: {target}: {e}"))?;
                std::env::set_var("OLDPWD", old);
                std::env::set_var("PWD", std::env::current_dir().unwrap());
                return Ok(Outcome::Status(0));
            }
            "exit" => {
                let status = if args.len() > 1 {
                    args[1]
                        .parse()
                        .map_err(|_| "exit: expected numeric status")?
                } else {
                    last_status
                };
                return Ok(Outcome::Exit(status));
            }
            "export" => {
                for arg in &args[1..] {
                    let (key, value) = arg.split_once('=').ok_or("export: expected NAME=value")?;
                    if !valid_name(key) {
                        return Err(format!("export: invalid name: {key}"));
                    }
                    std::env::set_var(key, value);
                }
                return Ok(Outcome::Status(0));
            }
            "unset" => {
                for key in &args[1..] {
                    std::env::remove_var(key);
                }
                return Ok(Outcome::Status(0));
            }
            _ => {}
        }
        if args.len() == 1 {
            if let Some((key, value)) = args[0].split_once('=') {
                if valid_name(key) {
                    std::env::set_var(key, value);
                    return Ok(Outcome::Status(0));
                }
            }
        }
    }
    let count = stages.len();
    let mut pipeline = Pipeline::default();
    let mut previous = None;
    for (index, stage) in stages.iter_mut().enumerate() {
        let mut inputs = Vec::new();
        for &arg_index in &stage.reference_args {
            let reference = &stage.args[arg_index];
            let input = ReferenceInput::new(references[reference].clone())
                .map_err(|error| format!("{reference}: {error}"))?;
            stage.args[arg_index] = input.path();
            inputs.push(input);
        }
        let mut envs = Vec::new();
        while let Some((key, value)) = stage.args.first().and_then(|s| s.split_once('=')) {
            if !valid_name(key) {
                break;
            }
            envs.push((key.to_owned(), value.to_owned()));
            stage.args.remove(0);
        }
        if stage.args.is_empty() {
            return Err("missing command after environment assignment".into());
        }
        let mut command = Command::new(&stage.args[0]);
        command.args(&stage.args[1..]).envs(envs);
        let mut stdin_writer = None;
        if let Some(path) = &stage.stdin {
            if stage.stdin_reference {
                let input = ReferenceInput::new(references[path].clone())
                    .map_err(|error| format!("{path}: {error}"))?;
                let ReferenceInput {
                    reader,
                    writer,
                    text,
                } = input;
                command.stdin(Stdio::from(reader));
                // Stdio owns the reader; no /dev/fd argument needs to inherit it.
                stdin_writer = Some((writer, text));
            } else {
                command.stdin(File::open(path).map_err(|e| format!("{path}: {e}"))?);
            }
        } else if let Some(stdout) = previous.take() {
            command.stdin(Stdio::from(stdout));
        }
        if let Some((path, append)) = &stage.stdout {
            command.stdout(output_file(path, *append).map_err(|e| format!("{path}: {e}"))?);
        } else if index + 1 < count {
            command.stdout(Stdio::piped());
        }
        if let Some((path, append)) = &stage.stderr {
            command.stderr(output_file(path, *append).map_err(|e| format!("{path}: {e}"))?);
        }
        let reference_fds: Vec<_> = inputs
            .iter()
            .map(|input| input.reader.as_raw_fd())
            .collect();
        unsafe {
            command.pre_exec(move || {
                for &fd in &reference_fds {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                }
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGQUIT, libc::SIG_DFL);
                Ok(())
            });
        }
        match command.spawn() {
            Ok(mut child) => {
                previous = child.stdout.take();
                pipeline.children.push(child);
                // Drop Command's copy of stdin before joining any writers.
                drop(command);
                if let Some((writer, text)) = stdin_writer {
                    pipeline.writers.push(feed_reference(writer, text));
                }
                pipeline
                    .writers
                    .extend(inputs.into_iter().map(ReferenceInput::feed));
            }
            Err(e) => {
                drop(command);
                drop(previous);
                eprintln!("lynn: {}: {e}", stage.args[0]);
                return Ok(Outcome::Status(if e.kind() == io::ErrorKind::NotFound {
                    127
                } else {
                    126
                }));
            }
        }
    }
    let mut status = 0;
    for (index, child) in pipeline.children.iter_mut().enumerate() {
        let result = child.wait().map_err(|e| e.to_string())?;
        if index + 1 == count {
            use std::os::unix::process::ExitStatusExt;
            status = result
                .code()
                .unwrap_or_else(|| 128 + result.signal().unwrap_or(0));
        }
    }
    pipeline.finish()?;
    Ok(Outcome::Status(status))
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("lynn-reference-{}-{unique}", std::process::id()));
            fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn file(&self, name: &str) -> String {
            format!("'{}'", self.0.join(name).display())
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn assert_status(result: Result<Outcome, String>, expected: i32) {
        assert!(matches!(result.unwrap(), Outcome::Status(status) if status == expected));
    }

    #[test]
    fn reference_arguments_preserve_literal_filenames_and_input_order() {
        assert_eq!(
            reference_arguments("cat #7 '#12' \"#13\" \\#14 ./#15 opus?#16 #3.2 < #8", 0).unwrap(),
            ["#7", "opus?#16", "#3.2", "#8"]
        );
        assert!(reference_arguments("cat '#'7 > '#12'", 0)
            .unwrap()
            .is_empty());
        assert!(parse("cat #7 > #12", 0).is_err());
        assert!(parse("#7 | cat", 0).is_err());
        assert!(parse("cd #7", 0).is_err());
    }

    #[test]
    fn cat_concatenates_files_and_large_reference_streams_including_duplicates() {
        let root = TestDirectory::new();
        fs::write(root.0.join("requirements.md"), "Requirements\n").unwrap();
        let large = format!("\nReferenced reply opus?#7:\n{}\n", "a\n".repeat(1_000_000));
        let small = "\nReferenced reply luna?#12:\nSecond answer\n".to_owned();
        let references =
            BTreeMap::from([("#7".into(), large.clone()), ("#12".into(), small.clone())]);
        assert_status(
            execute_with_references(
                &format!(
                    "cat {} #7 #12 > {}",
                    root.file("requirements.md"),
                    root.file("brief.md")
                ),
                0,
                &references,
            ),
            0,
        );
        assert_status(
            execute_with_references(
                &format!("cat #12 #12 >> {}", root.file("brief.md")),
                0,
                &references,
            ),
            0,
        );
        assert_eq!(
            fs::read_to_string(root.0.join("brief.md")).unwrap(),
            format!("Requirements\n{large}{small}{small}{small}")
        );
    }

    #[test]
    fn reference_streams_work_in_pipeline_arguments_and_stdin() {
        let root = TestDirectory::new();
        let references = BTreeMap::from([
            ("#7".into(), "first\n".into()),
            ("#12".into(), "second\n".into()),
        ]);
        assert_status(
            execute_with_references(
                &format!(
                    "printf prefix | cat - #7 #12 | wc -c > {}",
                    root.file("count")
                ),
                0,
                &references,
            ),
            0,
        );
        assert_eq!(
            fs::read_to_string(root.0.join("count")).unwrap().trim(),
            "19"
        );
        assert_status(
            execute_with_references(
                &format!("cat < #7 > {}", root.file("input")),
                0,
                &references,
            ),
            0,
        );
        assert_eq!(fs::read_to_string(root.0.join("input")).unwrap(), "first\n");
        fs::write(root.0.join("#7"), "literal\n").unwrap();
        assert_status(
            execute_with_references(
                &format!("cat {} > {}", root.file("#7"), root.file("literal")),
                0,
                &references,
            ),
            0,
        );
        assert_eq!(
            fs::read_to_string(root.0.join("literal")).unwrap(),
            "literal\n"
        );
    }

    #[test]
    fn reference_writers_finish_when_commands_stop_reading_or_fail() {
        let root = TestDirectory::new();
        let references = BTreeMap::from([("#7".into(), "x".repeat(2_000_000))]);
        assert_status(
            execute_with_references(
                &format!("head -c 1 #7 > {}", root.file("first")),
                0,
                &references,
            ),
            0,
        );
        assert_eq!(fs::read(root.0.join("first")).unwrap(), b"x");
        for _ in 0..8 {
            assert_status(execute_with_references("true #7", 0, &references), 0);
            assert_status(execute_with_references("true < #7", 0, &references), 0);
        }
        assert_status(
            execute_with_references("lynn_missing_reference_command #7", 0, &references),
            127,
        );
        assert_status(
            execute_with_references("lynn_missing_reference_command < #7", 0, &references),
            127,
        );
        assert!(execute_with_references(
            &format!("cat #7 | cat > {}", root.file("missing/output")),
            0,
            &references,
        )
        .is_err());
    }

    #[test]
    fn unresolved_references_do_not_truncate_output_files() {
        let root = TestDirectory::new();
        fs::write(root.0.join("brief.md"), "Keep this brief\n").unwrap();
        let references = BTreeMap::from([("#7".into(), "answer".into())]);
        assert!(execute_with_references(
            &format!("cat #7 | cat #99 > {}", root.file("brief.md")),
            0,
            &references,
        )
        .is_err());
        assert_eq!(
            fs::read_to_string(root.0.join("brief.md")).unwrap(),
            "Keep this brief\n"
        );
    }

    #[test]
    fn reader_shell_trusts_only_the_current_users_repository() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("lynn-reader-{}-{unique}", std::process::id()));
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();

        let plain = reader_shell_command("bash", &nested, true);
        let plain_args: Vec<_> = plain
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(!plain_args.iter().any(|arg| arg.starts_with("GIT_CONFIG_")));

        fs::create_dir(root.join(".git")).unwrap();
        let trusted = reader_shell_command("zsh", &nested, false);
        let trusted_args: Vec<_> = trusted
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(trusted_args.contains(&"GIT_CONFIG_COUNT=1".to_owned()));
        assert!(trusted_args.contains(&"GIT_CONFIG_KEY_0=safe.directory".to_owned()));
        assert!(trusted_args.contains(&format!(
            "GIT_CONFIG_VALUE_0={}",
            root.canonicalize().unwrap().display()
        )));
        assert_eq!(trusted_args.last().unwrap(), "/bin/zsh");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn quotes_and_operators() {
        assert_eq!(
            lex("echo 'a b' \"c d\"|cat 2>>err", 0).unwrap(),
            vec![
                Token::Word("echo".into()),
                Token::Word("a b".into()),
                Token::Word("c d".into()),
                Token::Pipe,
                Token::Word("cat".into()),
                Token::ErrAppend,
                Token::Word("err".into())
            ]
        );
    }
    #[test]
    fn empty_quotes_and_bad_syntax() {
        assert_eq!(lex("echo ''", 0).unwrap()[1], Token::Word(String::new()));
        assert!(parse("echo |", 0).is_err());
        assert!(parse("cat >", 0).is_err());
        assert!(parse("echo 'unfinished", 0).is_err());
    }
}
