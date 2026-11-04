mod agent_cli;
mod agy;
mod claude;
mod codex;
mod feed;
mod fm;
mod fx;
mod html;
mod http;
mod inline;
mod jsc;
mod live;
mod login_path;
mod pi;
mod session;
mod shell;
mod terminal;

use agent_cli::{AgentReply, AgentUser, ModelInfo};
use indexmap::IndexMap;
use session::{
    AgentAccess, AgentModel, AgentOptions, AgentOutput, AgentTask, DiffMode, DisplayDetail, Event,
    FeedSelection, Input, LiveCommand, LiveTarget, PromptMode, ReasoningEffort, Resource, Session,
};
use shell::Outcome;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, BufRead, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SendError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use terminal::ReadResult;

const MAX_IMPLICIT_WATCH_CONTEXT_BYTES: usize = 16 * 1024;
const MAX_TOTAL_IMPLICIT_WATCH_CONTEXT_BYTES: usize = 64 * 1024;
const MAX_REFERENCED_WATCH_CONTEXT_BYTES: usize = 128 * 1024;

struct AgentUpdate {
    id: usize,
    model: String,
    access: AgentAccess,
    result: Result<AgentReply, String>,
    /// How long the agent ran, not counting time queued.
    elapsed: Duration,
}

struct QueuedAgent {
    id: usize,
    model: AgentModel,
    task: String,
    cwd: PathBuf,
    options: AgentOptions,
    /// Overrides the agent's own backend model, as `pi.<model>?` does.
    backend_model: Option<String>,
}

impl QueuedAgent {
    fn label(&self) -> AgentLabel<'_> {
        AgentLabel {
            model: self.model.name(),
            id: self.id,
            access: self.options.access,
            effort: self.options.effort,
            output: self.options.output,
        }
    }
}

#[derive(Clone, Debug)]
struct ResolvedReferences {
    text: String,
    ids: BTreeSet<usize>,
}

enum ReferenceResolution {
    Ready(ResolvedReferences),
    Pending(Vec<usize>),
}

#[derive(Clone, Copy)]
enum AgentDisplay<'a> {
    Working,
    Answer(&'a str),
    FullAnswer(&'a str),
    Error(&'a str),
}

/// Whether rendered agent lines may use terminal colors.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Styling {
    Plain,
    Terminal,
}

/// Whether a recalled agent reply is preceded by the request that produced it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PromptLine {
    Always,
    WhenExpanded,
}

/// The name of an agent result, such as `opus?#3`, `opus??#3` or `opus?!#3`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AgentLabel<'a> {
    model: &'a str,
    id: usize,
    access: AgentAccess,
    effort: ReasoningEffort,
    output: AgentOutput,
}

impl<'a> AgentLabel<'a> {
    fn new(model: &'a str, access: AgentAccess, id: usize) -> Self {
        Self {
            model,
            id,
            access,
            effort: ReasoningEffort::Default,
            output: AgentOutput::Final,
        }
    }

    /// Applies the effort and live output recorded for this result.
    fn in_session(self, session: &Session) -> Self {
        session
            .events
            .iter()
            .find_map(|event| match event {
                Event::AgentRequest {
                    id, effort, output, ..
                } if *id == self.id => Some(Self {
                    effort: *effort,
                    output: *output,
                    ..self
                }),
                _ => None,
            })
            .unwrap_or(self)
    }
}

impl std::fmt::Display for AgentLabel<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { model, id, .. } = self;
        match self.output {
            AgentOutput::Final => write!(
                formatter,
                "{model}{}#{id}",
                self.access
                    .suffix()
                    .to_string()
                    .repeat(self.effort.suffix_count())
            ),
            AgentOutput::Live => write!(formatter, "{model}?!#{id}"),
        }
    }
}

fn render_agent(label: AgentLabel<'_>, display: AgentDisplay<'_>, styling: Styling) -> String {
    render_agent_with_models(label, display, styling, None, None)
}

fn render_agent_with_models(
    label: AgentLabel<'_>,
    display: AgentDisplay<'_>,
    styling: Styling,
    model_info: Option<&ModelInfo>,
    elapsed: Option<Duration>,
) -> String {
    let color = styling == Styling::Terminal && output_color();
    let models = match model_info {
        Some(ModelInfo::Reported(models)) => Some(format!("used {}", models.join(", "))),
        Some(ModelInfo::Requested(model)) => {
            Some(format!("requested {model}; actual model unreported"))
        }
        Some(ModelInfo::Unreported) => Some("model unreported".to_owned()),
        None => None,
    };
    let parts: Vec<_> = models
        .into_iter()
        .chain(elapsed.map(format_elapsed))
        .collect();
    let detail = if parts.is_empty() {
        String::new()
    } else {
        format!(" [{}]", parts.join("; "))
    };
    match display {
        AgentDisplay::Working => format!("{label}: …"),
        AgentDisplay::Answer(text) | AgentDisplay::FullAnswer(text) if color => {
            format!(
                "\x1b[1;35m{label}{detail}:\x1b[0m {}",
                highlight_answer(text)
            )
        }
        AgentDisplay::Answer(text) | AgentDisplay::FullAnswer(text) => {
            format!("{label}{detail}: {text}")
        }
        AgentDisplay::Error(text) if color => format!("\x1b[1;31m{label}: error:\x1b[0m {text}"),
        AgentDisplay::Error(text) => format!("{label}: error: {text}"),
    }
}

/// A response time such as `0.8s`, `12.4s` or `3m 05s`.
fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs_f64();
    if seconds < 60.0 {
        format!("{seconds:.1}s")
    } else {
        let whole = elapsed.as_secs();
        format!("{}m {:02}s", whole / 60, whole % 60)
    }
}

fn output_color() -> bool {
    (unsafe { libc::isatty(1) == 1 })
        && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
        && std::env::var("TERM").ok().as_deref() != Some("dumb")
}

/// Renders a reply already in the session, labeled with its recorded effort
/// and output, optionally preceded by its request.
fn render_session_agent(
    session: &Session,
    label: AgentLabel<'_>,
    display: AgentDisplay<'_>,
    prompt: PromptLine,
) -> String {
    let label = label.in_session(session);
    let (detail, elapsed) = match session.events.iter().find_map(|event| match event {
        Event::AgentResponse {
            id,
            model_info,
            elapsed,
            ..
        } if *id == label.id => Some((model_info.as_ref(), *elapsed)),
        _ => None,
    }) {
        Some(details) if prompt == PromptLine::Always => details,
        _ => (None, None),
    };
    let result = render_agent_with_models(label, display, Styling::Terminal, detail, elapsed);
    if prompt == PromptLine::WhenExpanded && !session.expanded_response_ids.contains(&label.id) {
        return result;
    }
    let Some(input) = session.events.iter().find_map(|event| match event {
        Event::AgentRequest { id, input, .. } if *id == label.id => Some(input),
        _ => None,
    }) else {
        return result;
    };
    let prompt = format!("{label} prompt: {input}");
    format!("{}\n{result}", emphasize_prompt(&prompt, output_color()))
}

fn emphasize_prompt(prompt: &str, color: bool) -> String {
    if color {
        format!("\x1b[1m{prompt}\x1b[0m")
    } else {
        prompt.to_owned()
    }
}

fn expanded_agent_results(session: &mut Session) -> Vec<String> {
    let results: Vec<_> = session
        .events
        .iter()
        .filter_map(|event| match event {
            Event::AgentResponse {
                id, model, text, ..
            } if !session.expanded_response_ids.contains(id) => {
                let label = agent_label_by_id(session, *id)?;
                Some((
                    *id,
                    render_session_agent(
                        session,
                        AgentLabel { model, ..label },
                        AgentDisplay::FullAnswer(text),
                        PromptLine::Always,
                    ),
                ))
            }
            _ => None,
        })
        .collect();
    for (id, _) in &results {
        session.expanded_response_ids.insert(*id);
    }
    results.into_iter().map(|(_, result)| result).collect()
}

fn pending_agent_statuses(session: &Session, workers: &Workers, color: bool) -> Vec<String> {
    pending_agent_statuses_with_width(session, workers, color, terminal::columns())
}

/// Status lines for pending agents and loads. Expanded detail shows every
/// pending request's full prompt; compact detail shows the running mutating
/// request's prompt cut to one line, so you can tell what `opus!#28: …` is doing.
fn pending_agent_statuses_with_width(
    session: &Session,
    workers: &Workers,
    color: bool,
    columns: usize,
) -> Vec<String> {
    let mut statuses = Vec::new();
    for (&id, status) in &workers.pending_agents {
        let expanded = session.detail == DisplayDetail::Expanded;
        if expanded || workers.running_writer == Some(id) {
            if let Some(label) = agent_label_by_id(session, id) {
                if let Some(input) = session.events.iter().find_map(|event| match event {
                    Event::AgentRequest {
                        id: request_id,
                        input,
                        ..
                    } if *request_id == id => Some(input),
                    _ => None,
                }) {
                    let prefix = format!("{label} prompt: ");
                    let input = if expanded {
                        input.clone()
                    } else {
                        // Leave the last column free so the line never wraps.
                        let room = columns
                            .saturating_sub(1)
                            .saturating_sub(display_width(&prefix));
                        one_line_prompt(input, room)
                    };
                    statuses.push(emphasize_prompt(&format!("{prefix}{input}"), color));
                }
            }
        }
        statuses.push(status.clone());
    }
    statuses.extend(workers.pending_http.values().cloned());
    statuses
}

fn display_width(text: &str) -> usize {
    text.chars()
        .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

/// The first non-blank line of a prompt, cut to `room` columns. Ends with `…`
/// when anything was left out, including later lines.
fn one_line_prompt(input: &str, room: usize) -> String {
    let mut lines = input.lines().map(str::trim).filter(|line| !line.is_empty());
    let first = lines.next().unwrap_or("");
    let more = lines.next().is_some();
    if !more && display_width(first) <= room {
        return first.to_owned();
    }
    let budget = room.saturating_sub(1);
    let mut cut = String::new();
    let mut used = 0;
    for c in first.chars() {
        let width = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + width > budget {
            break;
        }
        used += width;
        cut.push(c);
    }
    format!("{}…", cut.trim_end())
}

fn highlight_answer(text: &str) -> String {
    let mut result = String::new();
    for line in text.split_inclusive('\n') {
        let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
        result.push_str(&line[..indent]);
        let rest = &line[indent..];
        if let Some(body) = rest.strip_prefix("- ").or_else(|| rest.strip_prefix("* ")) {
            result.push_str("\x1b[36m•\x1b[0m ");
            highlight_inline(body, &mut result);
        } else {
            highlight_inline(rest, &mut result);
        }
    }
    result
}

fn highlight_inline(mut text: &str, output: &mut String) {
    while !text.is_empty() {
        let bold = text.find("**").map(|at| (at, "**", "\x1b[1;36m"));
        let code = text.find('`').map(|at| (at, "`", "\x1b[36m"));
        let marker = match (bold, code) {
            (Some(bold), Some(code)) if bold.0 <= code.0 => Some(bold),
            (Some(_), Some(code)) => Some(code),
            (Some(bold), None) => Some(bold),
            (None, Some(code)) => Some(code),
            (None, None) => None,
        };
        let Some((at, delimiter, style)) = marker else {
            output.push_str(text);
            return;
        };
        output.push_str(&text[..at]);
        let after = &text[at + delimiter.len()..];
        if let Some(end) = after.find(delimiter) {
            output.push_str(style);
            output.push_str(&after[..end]);
            output.push_str("\x1b[0m");
            text = &after[end + delimiter.len()..];
        } else {
            output.push_str(delimiter);
            text = after;
        }
    }
}

struct HttpRequest {
    id: usize,
    url: String,
    kind: HttpRequestKind,
    /// URLs already answered by stored resources, so redirects to them are not refetched.
    stored_urls: BTreeSet<String>,
}

#[derive(Clone, Copy)]
enum HttpRequestKind {
    Load,
    Head,
}

struct HttpUpdate {
    id: usize,
    kind: HttpRequestKind,
    result: Result<HttpResult, String>,
}

enum HttpResult {
    Resource(Resource),
    /// The requested URL redirected to one an existing resource already answers.
    Stored {
        url: String,
        redirect_targets: IndexMap<String, String>,
    },
    Head {
        url: String,
        output: String,
    },
}

enum WorkerUpdate {
    Agent(AgentUpdate),
    AgentProgress { id: usize, status: String },
    Http(HttpUpdate),
}

struct LiveUpdate {
    id: usize,
    started_at: Instant,
    result: Result<String, String>,
}

struct Workers {
    worker_tx: WakeSender<WorkerUpdate>,
    worker_rx: Receiver<WorkerUpdate>,
    live_tx: WakeSender<LiveUpdate>,
    live_rx: Receiver<LiveUpdate>,
    wake_reader: UnixStream,
    next_agent_id: usize,
    active_agents: usize,
    running_writer: Option<usize>,
    write_gate: Arc<Mutex<()>>,
    pending_agents: BTreeMap<usize, String>,
    pending_http: BTreeMap<usize, String>,
    /// In-flight GET requests by URL, so agent requests can wait for them.
    loading_urls: BTreeMap<String, usize>,
    queued_agents: BTreeMap<usize, QueuedAgent>,
    /// Set while the input line starts with `#`, so queued agents wait for the note.
    holding_for_note: bool,
    interactive: bool,
    next_http_id: usize,
    active_http: usize,
    active_live: usize,
    hosts: HashMap<String, Sender<HttpRequest>>,
}

struct WakeSender<T> {
    tx: Sender<T>,
    wake_writer: Arc<UnixStream>,
}

impl<T> Clone for WakeSender<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            wake_writer: Arc::clone(&self.wake_writer),
        }
    }
}

impl<T> WakeSender<T> {
    fn send(&self, value: T) -> Result<(), SendError<T>> {
        self.tx.send(value)?;
        let _ = (&*self.wake_writer).write(&[1]);
        Ok(())
    }
}

impl Workers {
    fn new(interactive: bool) -> Self {
        let (wake_reader, wake_writer) = UnixStream::pair().expect("create wake socket");
        wake_reader
            .set_nonblocking(true)
            .expect("set wake socket nonblocking");
        wake_writer
            .set_nonblocking(true)
            .expect("set wake socket nonblocking");
        let wake_writer = Arc::new(wake_writer);
        let (worker_channel, worker_rx) = mpsc::channel();
        let (live_channel, live_rx) = mpsc::channel();
        Self {
            worker_tx: WakeSender {
                tx: worker_channel,
                wake_writer: Arc::clone(&wake_writer),
            },
            worker_rx,
            live_tx: WakeSender {
                tx: live_channel,
                wake_writer,
            },
            live_rx,
            wake_reader,
            next_agent_id: 1,
            active_agents: 0,
            running_writer: None,
            write_gate: Arc::new(Mutex::new(())),
            pending_agents: BTreeMap::new(),
            pending_http: BTreeMap::new(),
            loading_urls: BTreeMap::new(),
            queued_agents: BTreeMap::new(),
            holding_for_note: false,
            interactive,
            next_http_id: 1,
            active_http: 0,
            active_live: 0,
            hosts: HashMap::new(),
        }
    }

    fn next_result_id(&mut self) -> usize {
        let id = self.next_agent_id.max(self.next_http_id);
        self.next_agent_id = id + 1;
        self.next_http_id = id + 1;
        id
    }
}

fn main() {
    if std::env::args_os().skip(1).any(|arg| arg == "--help") {
        print_help();
        return;
    }
    login_path::load_if_started_without_shell();
    let mut session = match Session::new() {
        Ok(session) => session,
        Err(error) => {
            eprintln!("lynn: {error}");
            std::process::exit(1);
        }
    };
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
    }
    let interactive = unsafe { libc::isatty(0) == 1 };
    let output_is_terminal = unsafe { libc::isatty(1) == 1 };
    let mut terminal = terminal::Terminal::new();
    let mut workers = Workers::new(interactive);
    if interactive {
        if output_is_terminal {
            if let Err(error) = terminal::clear_screen() {
                eprintln!("lynn: terminal: {error}");
                std::process::exit(1);
            }
        }
        loop {
            if output_is_terminal {
                let home = std::env::var_os("HOME");
                let title = shortened_path(&session.cwd, home.as_deref().map(Path::new));
                if let Err(error) = terminal::set_title(&title) {
                    eprintln!("lynn: terminal title: {error}");
                    session.last_status = 1;
                    break;
                }
            }
            let color = output_color();
            let short_prompt = prompt(session.mode, &session.cwd, false, color);
            let full_prompt = prompt(session.mode, &session.cwd, true, color);
            let statuses = pending_agent_statuses(&session, &workers, color);
            let todos = session.todos.clone();
            let line = match terminal.read_line(
                &short_prompt,
                &full_prompt,
                session.detail,
                color,
                &todos,
                statuses,
                workers.wake_reader.as_raw_fd(),
                |line| {
                    set_note_hold(&session, &mut workers, line.starts_with(b"#"));
                    let updates = drain_updates(&mut workers, &mut session);
                    let statuses = pending_agent_statuses(&session, &workers, color);
                    (updates, statuses)
                },
            ) {
                Ok(ReadResult::Line(line)) => {
                    session.detail = DisplayDetail::Compact;
                    line
                }
                Ok(ReadResult::Interrupt) => {
                    session.detail = DisplayDetail::Compact;
                    session.mode = PromptMode::Shell;
                    set_note_hold(&session, &mut workers, false);
                    continue;
                }
                Ok(ReadResult::Eof) => break,
                Ok(ReadResult::Expand) => {
                    session.detail = DisplayDetail::Expanded;
                    set_note_hold(&session, &mut workers, false);
                    for result in expanded_agent_results(&mut session) {
                        println!("{result}");
                    }
                    continue;
                }
                Err(error) => {
                    eprintln!("lynn: terminal: {error}");
                    break;
                }
            };
            if let Some(status) = handle(&line, &mut session, &mut workers, &mut terminal) {
                std::process::exit(status);
            }
            // The note (if any) is now recorded, so queued agents can see it.
            set_note_hold(&session, &mut workers, false);
        }
    } else {
        for line in io::stdin().lock().lines() {
            match line {
                Ok(line) => {
                    for update in drain_updates(&mut workers, &mut session) {
                        println!("{update}");
                    }
                    if let Some(status) = handle(&line, &mut session, &mut workers, &mut terminal) {
                        std::process::exit(status);
                    }
                }
                Err(error) => {
                    eprintln!("lynn: input: {error}");
                    std::process::exit(1);
                }
            }
        }
        while workers.active_agents + workers.active_http + workers.active_live > 0 {
            match workers.worker_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(update) => {
                    if let Some(line) = format_worker_update(update, &mut session, &mut workers) {
                        println!("{line}");
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            for line in drain_updates(&mut workers, &mut session) {
                println!("{line}");
            }
        }
    }
    std::process::exit(session.last_status);
}

fn prompt(mode: PromptMode, cwd: &Path, full: bool, color: bool) -> String {
    let location = if full {
        cwd.to_string_lossy()
            .chars()
            .map(|character| {
                if character.is_control() {
                    '?'
                } else {
                    character
                }
            })
            .collect()
    } else {
        let home = std::env::var_os("HOME");
        shortened_path(cwd, home.as_deref().map(Path::new))
    };
    match (mode, color) {
        (PromptMode::Shell, false) => format!("{location}> "),
        (PromptMode::FmReadOnly, false) => format!("fm? {location}> "),
        (PromptMode::Shell, true) => format!("\x1b[1;36m{location}>\x1b[0m "),
        (PromptMode::FmReadOnly, true) => {
            format!("\x1b[1;35mfm?\x1b[0m \x1b[1;36m{location}>\x1b[0m ")
        }
    }
}

fn shortened_path(cwd: &Path, home: Option<&Path>) -> String {
    let (prefix, relative) = match home.and_then(|home| cwd.strip_prefix(home).ok()) {
        Some(relative) => ("~", relative),
        None if cwd.is_absolute() => ("/", cwd.strip_prefix("/").unwrap_or(cwd)),
        None => ("", cwd),
    };
    let components: Vec<String> = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(name) => Some(
                name.to_string_lossy()
                    .chars()
                    .map(|character| {
                        if character.is_control() {
                            '?'
                        } else {
                            character
                        }
                    })
                    .collect(),
            ),
            _ => None,
        })
        .collect();
    if components.is_empty() {
        return if prefix.is_empty() { "." } else { prefix }.to_owned();
    }
    let last = components.len() - 1;
    let parts: Vec<String> = components
        .into_iter()
        .enumerate()
        .map(|(index, component)| {
            if index == last {
                component
            } else if let Some(hidden) = component.strip_prefix('.') {
                format!(".{}", hidden.chars().next().unwrap_or('.'))
            } else {
                component.chars().next().unwrap_or('?').to_string()
            }
        })
        .collect();
    match prefix {
        "~" => format!("~/{}", parts.join("/")),
        "/" => format!("/{}", parts.join("/")),
        _ => parts.join("/"),
    }
}

fn reset_state(
    session: &mut Session,
    workers: &mut Workers,
    terminal: &mut terminal::Terminal,
) -> io::Result<()> {
    let fresh_session = Session::new()?;
    let interactive = workers.interactive;
    // Reset discards results, but a write process may still be running.
    let write_gate = Arc::clone(&workers.write_gate);
    *workers = Workers::new(interactive);
    workers.write_gate = write_gate;
    *session = fresh_session;
    *terminal = terminal::Terminal::new();
    if interactive && unsafe { libc::isatty(1) == 1 } {
        terminal::clear_screen()?;
    }
    Ok(())
}

fn handle(
    line: &str,
    session: &mut Session,
    workers: &mut Workers,
    terminal: &mut terminal::Terminal,
) -> Option<i32> {
    if line.trim() == "reset" {
        if let Err(error) = reset_state(session, workers, terminal) {
            eprintln!("lynn: reset: {error}");
            session.last_status = 1;
        }
        return None;
    }
    for update in drain_updates(workers, session) {
        println!("{update}");
    }
    if session.mode == PromptMode::FmReadOnly && line.trim() == "?" {
        print_help();
        return None;
    }
    if session.mode == PromptMode::FmReadOnly && matches!(line.trim(), "." | "exit") {
        session.mode = PromptMode::Shell;
        return None;
    }
    if session.mode == PromptMode::FmReadOnly && line.trim().is_empty() {
        return None;
    }
    let chosen_pi;
    let line = match session::pi_model_selector(line) {
        Some((model, selector)) if session.mode == PromptMode::Shell => {
            session.pi_model = Some(model.to_owned());
            chosen_pi = format!("pi{selector}");
            chosen_pi.as_str()
        }
        _ => line,
    };
    let classified = session::classify(line);
    let input = if session.mode == PromptMode::FmReadOnly
        && !matches!(
            &classified,
            Input::AgentReference { .. }
                | Input::AgentNumberReference { .. }
                | Input::LatestAgentResponse
                | Input::ShowTodos
        ) {
        session::agents(
            line.trim(),
            vec![AgentModel::Fm],
            AgentOptions::new(AgentAccess::ReadOnly),
        )
    } else {
        classified
    };
    let wants_pi = match &input {
        Input::Agents(request) => request.models.contains(&AgentModel::Pi),
        Input::InteractiveAgent { model, .. } => *model == AgentModel::Pi,
        _ => false,
    };
    if wants_pi && session.pi_model.is_none() && workers.interactive && !choose_pi_model(session) {
        return None;
    }
    match input {
        Input::Help => print_help(),
        Input::Shell(command) => {
            if command.is_empty() {
                return None;
            }
            let resolved = match resolve_inline(command, session, workers) {
                Ok(resolved) => resolved,
                Err(error) => {
                    eprintln!("lynn: {error}");
                    session.last_status = 1;
                    return None;
                }
            };
            let outcome = if inline::shell_substitution(&resolved) {
                shell::execute_substitution_shell(&resolved).map(Outcome::Status)
            } else {
                resolve_shell_references(&resolved, session, workers).and_then(|references| {
                    if references.is_empty() {
                        shell::execute(&resolved, session.last_status)
                    } else {
                        shell::execute_with_references(&resolved, session.last_status, &references)
                    }
                })
            };
            let status = match outcome {
                Ok(Outcome::Status(status)) => status,
                Ok(Outcome::Exit(status)) => return Some(status),
                Err(error) => {
                    eprintln!("lynn: {error}");
                    1
                }
            };
            session.last_status = status;
            if let Ok(cwd) = std::env::current_dir() {
                session.cwd = cwd;
            }
            session.events.push(Event::Command {
                input: command.into(),
                status,
            });
        }
        Input::InteractiveShell(program) => {
            let status = match shell::enter_interactive(program) {
                Ok(status) => status,
                Err(error) => {
                    eprintln!("lynn: {error}");
                    1
                }
            };
            session.last_status = status;
            session.events.push(Event::Command {
                input: format!("{program}!"),
                status,
            });
        }
        Input::HostShell {
            program,
            command,
            execution,
        } => {
            let status = match shell::execute_host_shell(program, command, execution) {
                Ok(status) => status,
                Err(error) => {
                    eprintln!("lynn: {error}");
                    1
                }
            };
            session.last_status = status;
            session.events.push(Event::Command {
                input: format!("{program}{} {command}", execution.suffix()),
                status,
            });
        }
        Input::Comment(comment) => session.events.push(Event::Comment(comment.into())),
        Input::LatestAgentResponse => match latest_agent_response(session) {
            Some((label, text)) => {
                println!(
                    "{}",
                    render_session_agent(
                        session,
                        label,
                        AgentDisplay::FullAnswer(text),
                        PromptLine::WhenExpanded
                    )
                );
                session.last_status = 0;
            }
            None => {
                eprintln!("lynn: no agent reply yet");
                session.last_status = 1;
            }
        },
        Input::AgentReference { model, access, id } => {
            let result = if let Some(expected_access) = access {
                if agent_label_by_id(session, id)
                    .is_none_or(|label| label.model != model || label.access != expected_access)
                {
                    Err(format!(
                        "no reply found for {}",
                        AgentLabel::new(model, expected_access, id).in_session(session)
                    ))
                } else {
                    agent_response(session, model, id, workers)
                }
            } else {
                agent_response(session, model, id, workers)
            };
            match result {
                Ok(text) => {
                    let label = agent_label_by_id(session, id).unwrap();
                    println!(
                        "{}",
                        render_session_agent(
                            session,
                            AgentLabel { model, ..label },
                            AgentDisplay::FullAnswer(text),
                            PromptLine::WhenExpanded
                        )
                    );
                    session.last_status = 0;
                }
                Err(error) => {
                    eprintln!("lynn: {error}");
                    session.last_status = 1;
                }
            }
        }
        Input::AgentNumberReference { id } if agent_label_by_id(session, id).is_none() => {
            match watch_response_by_id(session, id, None)
                .or_else(|| http_response_by_id(session, id))
            {
                Some(text) => {
                    println!("{text}");
                    session.last_status = 0;
                }
                None => {
                    if let Some(watch) = session.live_commands.iter().find(|watch| watch.id == id) {
                        eprintln!("lynn: {}", watch_reference_error(watch, None));
                    } else {
                        eprintln!("lynn: no reply found for #{id}");
                    }
                    session.last_status = 1;
                }
            }
        }
        Input::WatchRevisionReference { id, revision } => {
            match watch_response_by_id(session, id, Some(revision)) {
                Some(text) => {
                    println!("{text}");
                    session.last_status = 0;
                }
                None => {
                    let error = session
                        .live_commands
                        .iter()
                        .find(|watch| watch.id == id)
                        .map(|watch| watch_reference_error(watch, Some(revision)))
                        .unwrap_or_else(|| format!("no watch found for #{id}"));
                    eprintln!("lynn: {error}");
                    session.last_status = 1;
                }
            }
        }
        Input::AgentNumberReference { id } => match agent_response_by_id(session, id, workers) {
            Ok((label, text)) => {
                println!(
                    "{}",
                    render_session_agent(
                        session,
                        label,
                        AgentDisplay::FullAnswer(text),
                        PromptLine::WhenExpanded
                    )
                );
                session.last_status = 0;
            }
            Err(error) => {
                eprintln!("lynn: {error}");
                session.last_status = 1;
            }
        },
        Input::ShowTodos => {
            if session.todos.is_empty() {
                println!("no todos");
            } else {
                for todo in &session.todos {
                    println!("- [{}] {}", if todo.done { 'x' } else { ' ' }, todo.text);
                }
            }
            session.last_status = 0;
        }
        Input::Todo(todo) => {
            session.todos.push(session::Todo {
                text: todo.into(),
                done: false,
            });
            println!("- [ ] {todo}");
            session.last_status = 0;
        }
        Input::CompleteTodo(text) => {
            if let Some(todo) = session
                .todos
                .iter_mut()
                .find(|todo| todo.text == text && !todo.done)
            {
                todo.done = true;
                println!("- [x] {}", todo.text);
                session.last_status = 0;
            } else if session.todos.iter().any(|todo| todo.text == text) {
                println!("- [x] {text} (already complete)");
                session.last_status = 0;
            } else {
                eprintln!("lynn: no matching todo: {text}");
                session.last_status = 1;
            }
        }
        Input::LiveHostShell { program, command } => {
            start_live_command(program, command, session, workers)
        }
        Input::LiveHead(url) => start_live_head(url, session, workers),
        Input::LiveFeed(url, selection) => start_live_feed(url, selection, session, workers),
        Input::LiveDiff(mode) => show_live_diff(session, workers, mode),
        Input::EnterFm => {
            session.mode = PromptMode::FmReadOnly;
            println!("[fm read-only mode; ., exit, or Ctrl-C returns to the shell]");
        }
        Input::Agents(request) => start_agents(request, session, workers),
        Input::Yes { id, access } => reply_yes(id, access, session, workers),
        Input::Url(url) => start_http(url, HttpRequestKind::Load, session, workers),
        Input::Head(url) => start_http(url, HttpRequestKind::Head, session, workers),
        Input::UnavailableTarget { name, access } => {
            eprintln!("lynn: target '{name}{}' is not available", access.suffix());
            session.last_status = 1;
        }
        Input::InteractiveAgent {
            model,
            access,
            effort,
        } => {
            if access == AgentAccess::ReadWrite {
                wait_for_writers(session, workers);
                let write_gate = Arc::clone(&workers.write_gate);
                let _guard = write_gate.lock().unwrap_or_else(|error| error.into_inner());
                start_interactive_agent(model, access, effort, session);
            } else {
                start_interactive_agent(model, access, effort, session);
            }
        }
        Input::Reserved(input) => {
            eprintln!("lynn: reserved syntax is not available yet: {input}");
            session.last_status = 1;
        }
    }
    let _ = io::stdout().flush();
    None
}

fn run_agent(
    model: AgentModel,
    request: agent_cli::Request<'_>,
    references: &str,
    output: AgentOutput,
    progress: impl FnMut(String),
) -> Result<AgentReply, String> {
    let agent_cli::Request {
        task,
        cwd,
        context,
        model: backend_model,
        access,
        user,
        ..
    } = request;
    match (model, output) {
        (AgentModel::Fm, AgentOutput::Final) => {
            fm::run(task, cwd, context, references, access).map(Into::into)
        }
        (
            AgentModel::Claude
            | AgentModel::Fable
            | AgentModel::Opus
            | AgentModel::Sonnet
            | AgentModel::Haiku,
            AgentOutput::Final,
        ) => claude::run(request),
        (
            AgentModel::Claude
            | AgentModel::Fable
            | AgentModel::Opus
            | AgentModel::Sonnet
            | AgentModel::Haiku,
            AgentOutput::Live,
        ) => claude::run_live(request, progress),
        (
            AgentModel::Codex | AgentModel::Sol | AgentModel::Luna | AgentModel::Terra,
            AgentOutput::Final,
        ) => codex::run(request),
        (
            AgentModel::Codex | AgentModel::Sol | AgentModel::Luna | AgentModel::Terra,
            AgentOutput::Live,
        ) => codex::run_live(request, progress),
        (AgentModel::Pi, AgentOutput::Final) => {
            pi::run(task, cwd, context, backend_model, access, user)
        }
        (AgentModel::Fx, AgentOutput::Final) => fx::run(request),
        (AgentModel::Agy | AgentModel::Flash, AgentOutput::Final) => agy::run(request),
        (AgentModel::Jsc, AgentOutput::Final) => jsc::run(task, cwd, access, user).map(Into::into),
        (
            AgentModel::Fm
            | AgentModel::Pi
            | AgentModel::Fx
            | AgentModel::Agy
            | AgentModel::Flash
            | AgentModel::Jsc,
            AgentOutput::Live,
        ) => Err("live status is unavailable for this agent".to_owned()),
    }
}

fn interactive_agent(
    model: AgentModel,
    pi_model: Option<&str>,
    cwd: &Path,
    access: AgentAccess,
    effort: ReasoningEffort,
) -> Result<Command, String> {
    match model {
        AgentModel::Fm => Err("fm uses Lynn's built-in interactive mode".to_owned()),
        AgentModel::Claude
        | AgentModel::Fable
        | AgentModel::Opus
        | AgentModel::Sonnet
        | AgentModel::Haiku => claude::interactive(cwd, model.backend_model(), access, effort),
        AgentModel::Codex | AgentModel::Sol | AgentModel::Luna | AgentModel::Terra => {
            codex::interactive(cwd, model.backend_model(), access, effort)
        }
        AgentModel::Agy | AgentModel::Flash => {
            agy::interactive(cwd, model.backend_model(), access, effort)
        }
        AgentModel::Pi if effort == ReasoningEffort::Default => {
            pi::interactive(cwd, pi_model, access)
        }
        AgentModel::Fx if effort == ReasoningEffort::Default => fx::interactive(cwd, access),
        AgentModel::Jsc if effort == ReasoningEffort::Default => jsc::interactive(cwd, access),
        AgentModel::Pi | AgentModel::Fx | AgentModel::Jsc => Err(
            "extra reasoning effort is available only for Claude Code, Codex, and agy".to_owned(),
        ),
    }
}

/// Asks which Pi model to use when `pi?` runs before any `pi.<model>?`.
fn choose_pi_model(session: &mut Session) -> bool {
    let models = pi::models();
    if models.is_empty() {
        eprintln!("lynn: pi --list-models listed no models");
        session.last_status = 1;
        return false;
    }
    println!("Choose a Pi model for this session:");
    for (number, model) in models.iter().enumerate() {
        println!("  {:>2}  {model}", number + 1);
    }
    print!("Model number or name (Enter cancels): ");
    let _ = io::stdout().flush();
    let mut answer = String::new();
    let _ = io::stdin().lock().read_line(&mut answer);
    let answer = answer.trim();
    let chosen = match answer.parse::<usize>() {
        Ok(number) => models.get(number.wrapping_sub(1)).cloned(),
        Err(_) => (!answer.is_empty()).then(|| answer.to_owned()),
    };
    match chosen {
        Some(model) => {
            session.pi_model = Some(model);
            true
        }
        None => {
            eprintln!("lynn: no Pi model chosen");
            session.last_status = 1;
            false
        }
    }
}

fn start_interactive_agent(
    model: AgentModel,
    access: AgentAccess,
    effort: ReasoningEffort,
    session: &mut Session,
) {
    let selector = format!(
        "{}{}",
        model.name(),
        access.suffix().to_string().repeat(effort.suffix_count())
    );
    println!(
        "[{selector} interactive {} session; exit it to return to lynn]",
        access.description()
    );
    let _ = io::stdout().flush();
    let status = match interactive_agent(
        model,
        session.pi_model.as_deref(),
        &session.cwd,
        access,
        effort,
    ) {
        Ok(command) => agent_cli::run_interactive(command, model.name()),
        Err(error) => {
            eprintln!("lynn: {error}");
            1
        }
    };
    session.last_status = status;
    session.events.push(Event::Command {
        input: selector,
        status,
    });
}

fn resolve_inline(
    command: &str,
    session: &mut Session,
    workers: &mut Workers,
) -> Result<String, String> {
    let Some(parts) = inline::parse(command)? else {
        shell::validate_control_syntax(command)?;
        return Ok(command.to_owned());
    };
    // Check before starting inline agents, but do not interpret their prompts
    // as shell syntax. Each response will become one quoted argument.
    let syntax: String = parts
        .iter()
        .map(|part| match part {
            inline::Part::Literal(text) => *text,
            inline::Part::Agent { .. } => "inline-agent-result",
        })
        .collect();
    shell::validate_control_syntax(&syntax)?;
    let mut resolved = String::new();
    for part in parts {
        match part {
            inline::Part::Literal(text) => resolved.push_str(text),
            inline::Part::Agent { model, task } => {
                let agent_model = AgentModel::parse(model).ok_or("unknown inline agent model")?;
                let references = ready_references(task, session, workers)?;
                let id = workers.next_result_id();
                let label = AgentLabel::new(model, AgentAccess::ReadOnly, id);
                let mut context =
                    session_context_filtered(session, &references.ids, None, &session.cwd);
                context.push_str(&format!(
                    "request ({label}, read-only, current user): {task}\n"
                ));
                context.push_str(&references.text);
                session.events.push(Event::AgentRequest {
                    id,
                    input: task.to_owned(),
                    access: AgentAccess::ReadOnly,
                    user: AgentUser::Current,
                    model: Some(model.to_owned()),
                    effort: ReasoningEffort::Default,
                    output: AgentOutput::Final,
                    pi_model: (agent_model == AgentModel::Pi)
                        .then(|| session.pi_model.clone())
                        .flatten(),
                });
                println!(
                    "{}",
                    render_agent(label, AgentDisplay::Working, Styling::Plain)
                );
                let _ = io::stdout().flush();
                let started = Instant::now();
                let answer = run_agent(
                    agent_model,
                    agent_cli::Request {
                        task,
                        cwd: &session.cwd,
                        context: &context,
                        model: match agent_model {
                            AgentModel::Pi => session.pi_model.as_deref(),
                            _ => agent_model.backend_model(),
                        },
                        access: AgentAccess::ReadOnly,
                        user: AgentUser::Current,
                        effort: ReasoningEffort::Default,
                    },
                    &references.text,
                    AgentOutput::Final,
                    |_| {},
                )
                .map_err(|error| format!("{label}: {error}"))?;
                let quoted = inline::quote_argument(&answer.text)?;
                session.events.push(Event::AgentResponse {
                    id,
                    model: model.to_owned(),
                    text: answer.text.clone(),
                    model_info: answer.model_info,
                    elapsed: Some(started.elapsed()),
                });
                println!(
                    "{}",
                    render_agent(label, AgentDisplay::Answer(&answer.text), Styling::Terminal)
                );
                resolved.push_str(&quoted);
            }
        }
    }
    Ok(resolved)
}

fn start_agents(request: AgentTask<'_>, session: &mut Session, workers: &mut Workers) {
    let AgentTask {
        task,
        models,
        options,
    } = request;
    if task.is_empty() || models.is_empty() {
        eprintln!("lynn: agent unavailable or task is empty");
        session.last_status = 1;
        return;
    }
    if options.user == AgentUser::Staffer && models.contains(&AgentModel::Fm) {
        eprintln!("lynn: fm cannot run as staffer");
        session.last_status = 1;
        return;
    }
    if models.contains(&AgentModel::Pi) && session.pi_model.is_none() {
        eprintln!("lynn: {}", pi::choose_model_error());
        session.last_status = 1;
        return;
    }
    if options.effort != ReasoningEffort::Default
        && models.iter().any(|model| {
            matches!(
                model,
                AgentModel::Fm | AgentModel::Pi | AgentModel::Fx | AgentModel::Jsc
            )
        })
    {
        eprintln!("lynn: extra reasoning effort is available only for Claude Code, Codex, and agy");
        session.last_status = 1;
        return;
    }
    // Load each URL once; later requests reuse the stored or in-flight GET.
    for url in task_urls(task) {
        if stored_resource_id(session, url).is_none() && !workers.loading_urls.contains_key(url) {
            start_http(url, HttpRequestKind::Load, session, workers);
        }
    }
    let references = match resolve_references(task, session, workers) {
        Ok(references) => references,
        Err(error) => {
            eprintln!("lynn: {error}");
            session.last_status = 1;
            return;
        }
    };
    for model in models {
        let id = workers.next_result_id();
        workers.active_agents += 1;
        let backend_model = (model == AgentModel::Pi)
            .then(|| session.pi_model.clone())
            .flatten();
        session.events.push(Event::AgentRequest {
            id,
            input: task.into(),
            access: options.access,
            user: options.user,
            model: Some(model.name().to_owned()),
            effort: options.effort,
            output: options.output,
            pi_model: backend_model.clone(),
        });
        let request = QueuedAgent {
            id,
            model,
            task: task.to_owned(),
            cwd: session.cwd.clone(),
            options,
            backend_model,
        };
        let label = request.label().to_string();
        let predecessor = mutating_predecessor(&request, workers);
        let status = match (&references, predecessor) {
            (ReferenceResolution::Ready(references), None) => {
                launch_agent(request, references.clone(), session, workers);
                format!("{label}: …")
            }
            (_, Some(predecessor)) => {
                let status = queued_behind_status(&request, predecessor, session);
                workers.pending_agents.insert(id, status.clone());
                workers.queued_agents.insert(id, request);
                status
            }
            (ReferenceResolution::Pending(ids), None) => {
                let waiting_for = ids
                    .iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let status = format!("{label}: waiting for {waiting_for}");
                workers.pending_agents.insert(id, status.clone());
                workers.queued_agents.insert(id, request);
                status
            }
        };
        if !workers.interactive {
            println!("{status}");
        }
    }
    session.last_status = 0;
}

fn mutating_predecessor(request: &QueuedAgent, workers: &Workers) -> Option<usize> {
    if request.options.access == AgentAccess::ReadOnly {
        return None;
    }
    workers.running_writer.or_else(|| {
        workers
            .queued_agents
            .values()
            .find(|earlier| {
                earlier.id < request.id && earlier.options.access == AgentAccess::ReadWrite
            })
            .map(|earlier| earlier.id)
    })
}

/// The status of a mutating request queued behind another, such as
/// `opus!#7: waiting for opus!#4`.
fn queued_behind_status(request: &QueuedAgent, predecessor: usize, session: &Session) -> String {
    let predecessor = agent_label_by_id(session, predecessor)
        .map(|label| label.to_string())
        .unwrap_or_else(|| format!("#{predecessor}"));
    format!("{}: waiting for {predecessor}", request.label())
}

fn wait_for_writers(session: &mut Session, workers: &mut Workers) {
    let mut announced_wait = false;
    while workers.running_writer.is_some()
        || workers
            .queued_agents
            .values()
            .any(|request| request.options.access == AgentAccess::ReadWrite)
    {
        if !announced_wait {
            println!("[waiting for mutating requests before opening the interactive agent]");
            let _ = io::stdout().flush();
            announced_wait = true;
        }
        match workers.worker_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(update) => {
                if let Some(line) = format_worker_update(update, session, workers) {
                    println!("{line}");
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        for line in drain_updates(workers, session) {
            println!("{line}");
        }
    }
}

fn launch_agent(
    request: QueuedAgent,
    references: ResolvedReferences,
    session: &Session,
    workers: &mut Workers,
) {
    if request.options.access == AgentAccess::ReadWrite {
        debug_assert!(mutating_predecessor(&request, workers).is_none());
        workers.running_writer = Some(request.id);
    }
    let context = agent_context(session, &request, &references);
    workers
        .pending_agents
        .insert(request.id, format!("{}: …", request.label()));
    let QueuedAgent {
        id,
        model,
        task,
        cwd,
        options,
        backend_model,
    } = request;
    let references = references.text;
    let tx = workers.worker_tx.clone();
    let write_gate = Arc::clone(&workers.write_gate);
    std::thread::spawn(move || {
        // Keep writes exclusive even when reset replaces the session and its channels.
        let _guard = if options.access == AgentAccess::ReadWrite {
            Some(write_gate.lock().unwrap_or_else(|error| error.into_inner()))
        } else {
            None
        };
        let progress_tx = tx.clone();
        let started = Instant::now();
        let result = run_agent(
            model,
            agent_cli::Request {
                task: &task,
                cwd: &cwd,
                context: &context,
                model: backend_model.as_deref().or(model.backend_model()),
                access: options.access,
                user: options.user,
                effort: options.effort,
            },
            &references,
            options.output,
            |status| {
                let _ = progress_tx.send(WorkerUpdate::AgentProgress { id, status });
            },
        );
        let _ = tx.send(WorkerUpdate::Agent(AgentUpdate {
            id,
            model: model.name().to_owned(),
            access: options.access,
            result,
            elapsed: started.elapsed(),
        }));
    });
}

fn agent_context(
    session: &Session,
    request: &QueuedAgent,
    references: &ResolvedReferences,
) -> String {
    let mut context =
        session_context_filtered(session, &references.ids, Some(request.id), &request.cwd);
    let username = match request.options.user {
        AgentUser::Current => "current user",
        AgentUser::Staffer => "staffer",
    };
    context.push_str(&format!(
        "request ({}, {}, {username}): {}\n",
        request.label(),
        request.options.access.description(),
        request.task
    ));
    context.push_str(&references.text);
    context
}

fn start_live_command(
    program: &'static str,
    command: &str,
    session: &mut Session,
    workers: &mut Workers,
) {
    let command = command.trim();
    if command.is_empty() {
        eprintln!("lynn: + {program}?: expected a command");
        session.last_status = 1;
        return;
    }
    start_live(
        LiveTarget::Shell {
            program,
            command: command.to_owned(),
        },
        session,
        workers,
    );
}

fn start_live_head(url: &str, session: &mut Session, workers: &mut Workers) {
    if let Err(error) = http::origin(url) {
        eprintln!("lynn: {error}");
        session.last_status = 1;
        return;
    }
    start_live(
        LiveTarget::Head {
            url: url.to_owned(),
        },
        session,
        workers,
    );
}

fn start_live_feed(
    url: &str,
    selection: FeedSelection,
    session: &mut Session,
    workers: &mut Workers,
) {
    if let Err(error) = http::origin(url) {
        eprintln!("lynn: {error}");
        session.last_status = 1;
        return;
    }
    start_live(
        LiveTarget::Feed {
            url: url.to_owned(),
            selection,
        },
        session,
        workers,
    );
}

fn start_live(target: LiveTarget, session: &mut Session, workers: &mut Workers) {
    if let Some(existing) = session
        .live_commands
        .iter()
        .find(|live| live.cwd == session.cwd && live.target == target)
    {
        println!(
            "[watch #{}] already watching {}; enter + to refresh",
            existing.id,
            existing.label()
        );
        session.last_status = 0;
        return;
    }
    let id = workers.next_result_id();
    let cwd = session.cwd.clone();
    session.live_commands.push(LiveCommand {
        id,
        cwd: cwd.clone(),
        target: target.clone(),
        output: String::new(),
        error: None,
        last_run: None,
        last_refresh: Instant::now(),
        revisions: Vec::new(),
    });
    println!(
        "[watch #{id} loading] {}",
        session.live_commands.last().unwrap().label()
    );
    workers.active_live += 1;
    let tx = workers.live_tx.clone();
    std::thread::spawn(move || {
        let client = http::agent();
        let started_at = Instant::now();
        let _ = tx.send(LiveUpdate {
            id,
            started_at,
            result: run_live_target(&target, &cwd, &client),
        });
    });
    session.last_status = 0;
}

fn run_live_target(
    target: &LiveTarget,
    cwd: &Path,
    client: &ureq::Agent,
) -> Result<String, String> {
    match target {
        LiveTarget::Shell { program, command } => live::run(program, command, cwd),
        LiveTarget::Head { url } => http::head(client, url),
        LiveTarget::Feed { url, selection } => {
            let http::Loaded::Resource(resource) = http::load(client, url, &|_| false)? else {
                unreachable!("nothing is stored for watched feeds");
            };
            if *selection == FeedSelection::ContentType
                && !feed::is_feed_content_type(&resource.content_type)
            {
                return Err(format!("URL is not an Atom or RSS feed (Content-Type: {}); use + FEED URL to parse an XML feed explicitly", resource.content_type));
            }
            feed::render(&resource)
        }
    }
}

fn show_live_diff(session: &mut Session, workers: &mut Workers, mode: DiffMode) {
    if session.live_commands.is_empty() {
        println!(
            "[+] no live values; register one with + bash? command, + HEAD URL, or + FEED URL"
        );
        return;
    }
    let mut failed = false;
    let mut showed_lines = false;
    let client = http::agent();
    for live in &mut session.live_commands {
        let previous = live.last_run.clone();
        let result = run_live_target(&live.target, &live.cwd, &client);
        failed |= result.is_err();
        let _ = update_live_command(live, result, Instant::now());
        let current = live_current_text(live);
        if let Some(previous) = previous {
            let lines = if matches!(live.target, LiveTarget::Feed { .. }) {
                feed::diff(&previous, &current, mode)
            } else {
                diff_lines(&previous, &current, mode)
            };
            if !lines.is_empty() {
                showed_lines = true;
                println!(
                    "[watch {} {} in {}]\n{lines}",
                    watch_revision_label(live),
                    live.label(),
                    live.cwd.display()
                );
            }
        }
        live.last_run = Some(current);
    }
    if !showed_lines {
        match mode {
            DiffMode::Added => println!("[+] no added lines"),
            DiffMode::AddedAndRemoved => println!("[+-] no changed lines"),
        }
    }
    resume_queued_agents(session, workers);
    session.last_status = if failed { 1 } else { 0 };
}

fn live_current_text(live: &LiveCommand) -> String {
    match &live.error {
        Some(error) => format!("error: {error}"),
        None => live.output.clone(),
    }
}

fn diff_lines(previous: &str, current: &str, mode: DiffMode) -> String {
    let old: Vec<_> = previous.lines().collect();
    let new: Vec<_> = current.lines().collect();
    let mut common = vec![vec![0; new.len() + 1]; old.len() + 1];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            common[i][j] = if old[i] == new[j] {
                common[i + 1][j + 1] + 1
            } else {
                common[i + 1][j].max(common[i][j + 1])
            };
        }
    }
    let mut lines = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        if i < old.len() && j < new.len() && old[i] == new[j] {
            i += 1;
            j += 1;
        } else if i < old.len() && (j == new.len() || common[i + 1][j] >= common[i][j + 1]) {
            if mode == DiffMode::AddedAndRemoved {
                lines.push(format!("-{}", old[i]));
            }
            i += 1;
        } else {
            lines.push(format!("+{}", new[j]));
            j += 1;
        }
    }
    lines.join("\n")
}

fn start_http(url: &str, kind: HttpRequestKind, session: &mut Session, workers: &mut Workers) {
    let origin = match http::origin(url) {
        Ok(origin) => origin,
        Err(error) => {
            eprintln!("lynn: {error}");
            session.last_status = 1;
            return;
        }
    };
    let id = workers.next_result_id();
    let stored_urls = match kind {
        HttpRequestKind::Load => stored_resource_urls(session),
        HttpRequestKind::Head => BTreeSet::new(),
    };
    let sender = workers.hosts.entry(origin).or_insert_with(|| {
        let (sender, receiver) = mpsc::channel::<HttpRequest>();
        let updates = workers.worker_tx.clone();
        std::thread::spawn(move || {
            let client = http::agent();
            while let Ok(request) = receiver.recv() {
                let result = match request.kind {
                    HttpRequestKind::Head => {
                        http::head(&client, &request.url).map(|output| HttpResult::Head {
                            url: request.url,
                            output,
                        })
                    }
                    HttpRequestKind::Load => {
                        let stored_urls = request.stored_urls;
                        http::load(&client, &request.url, &|url| stored_urls.contains(url)).map(
                            |loaded| match loaded {
                                http::Loaded::Resource(resource) => HttpResult::Resource(resource),
                                http::Loaded::Stored {
                                    url,
                                    redirect_targets,
                                } => HttpResult::Stored {
                                    url,
                                    redirect_targets,
                                },
                            },
                        )
                    }
                };
                if updates
                    .send(WorkerUpdate::Http(HttpUpdate {
                        id: request.id,
                        kind: request.kind,
                        result,
                    }))
                    .is_err()
                {
                    break;
                }
            }
        });
        sender
    });
    if sender
        .send(HttpRequest {
            id,
            url: url.to_owned(),
            kind,
            stored_urls,
        })
        .is_err()
    {
        eprintln!("lynn: URL worker stopped");
        session.last_status = 1;
    } else {
        workers.active_http += 1;
        if let HttpRequestKind::Load = kind {
            workers.loading_urls.insert(url.to_owned(), id);
        }
        session.last_status = 0;
        let label = match kind {
            HttpRequestKind::Load => "GET",
            HttpRequestKind::Head => "HEAD",
        };
        let status = format!("{label}#{id} {url}: …");
        if workers.interactive {
            workers.pending_http.insert(id, status);
        } else {
            println!("{status}");
        }
    }
}

fn print_help() {
    let color = output_color();
    for line in [
        "Lynn runs ordinary commands directly as a shell. Enter ? for this help.",
        "Shell:",
        "  Output is shown to you, not shared with agents.",
        "  Agents see command input and exit status.",
        "  git status, cargo test, vim file   Run directly in the foreground",
        "  cargo test && cargo build   Run the next command only after success",
        "  cat #3 #7 >> brief.md  Append numbered results to a file",
        "  diff #3.1 #3.2       Compare watch revisions as streamed inputs",
        "  cd, export, unset, exit; NAME=value are built-ins",
        "  reset                Clear session, history, watches, and request IDs",
        "  bash!, zsh!         Enter an interactive shell; exit returns to Lynn",
        "  bash! command       Run a command through Bash as your user",
        "  zsh! command        Run a command through Zsh as your user",
        "  bash? command       Run Bash as staffer via sudo",
        "  zsh? command        Run Zsh as staffer via sudo",
        "Watch:",
        "  Agents see watch results; enter + or +- to refresh.",
        "  + bash? command     Run and refresh a Bash command as staffer",
        "  + zsh? command      Run and refresh a Zsh command as staffer",
        "  + HEAD https://...  Check headers now and again when you enter +",
        "  + FEED https://...  Watch an Atom or RSS feed; refresh when you enter +",
        "  + https://...       Watch an Atom/RSS URL with a feed Content-Type",
        "  +                   Show added lines from all watches",
        "  +-                  Show added and removed lines from all watches",
        "  #3, #3.2            Show the latest watch result or revision 2 of watch #3",
        "Shared context:",
        "  https://...          Load a text resource into session context (background)",
        "  HEAD https://...     Show response headers and time to receive them",
        "  # comment           Keep context for later agent requests",
        "  -                   Show the session todo list",
        "  - [ ] todo          Add a session todo",
        "  - [x] todo          Mark a matching todo complete",
        "Agent:",
        "  claude? task        Ask Claude Code to inspect the project (background)",
        "  fable? task         Ask Claude Code with Fable (background)",
        "  opus? task          Ask Claude Code with Opus (background)",
        "  Opus? task          Same as opus? task",
        "  🎵 task, 🎶 task     Aliases for opus? task",
        "  sonnet? task        Ask Claude Code with Sonnet (background)",
        "  haiku? task         Ask Claude Code with Haiku (background)",
        "  codex? task         Ask Codex to inspect the project (background)",
        "  sol? task           Ask GPT-6 Sol through Codex (background)",
        "  luna? task          Ask GPT-6 Luna through Codex (background)",
        "  ☀️ task             Alias for sol? task",
        "  🌙 task             Alias for luna? task",
        "  terra? task         Ask GPT-5.6 Terra through Codex (background)",
        "  pi? task            Ask Pi to inspect the project (background)",
        "  fx? task            Ask fx to inspect the project (macOS, background)",
        "  fx! task            Ask fx to change files (background)",
        "  agy? task           Ask Antigravity to inspect the project (background)",
        "  flash? task         Ask Gemini Flash through Antigravity (background)",
        "  🥧 task             Alias for pi? task",
        "  fm? task            Ask Apple Foundation Models to investigate (background)",
        "  🍎 task             Alias for fm? task",
        "  fm! task            Ask Apple Foundation Models to change files (background)",
        "  name! task          Ask a named agent to change files as your user",
        "  pi.model? task      Choose Pi's model (Tab after pi lists them); pi? reuses it",
        "  opus?! task         Ask Opus to change files and show live progress",
        "  luna?! task         Ask Luna to change files and show live progress",
        "  opus?? task         Ask Opus with xhigh reasoning (read-only)",
        "  sol!!! task         Ask Sol with max reasoning (write access)",
        "  ??, !! / ???, !!!   Request xhigh / max on Claude or Codex agents",
        "  opus/luna? task     Ask both agents the same task (background)",
        "  @staffer opus? task Ask a CLI agent as staffer (requires sudo access)",
        "  fm?                 Enter fm read-only mode; ., exit, or Ctrl-C returns",
        "  git commit -m luna?(Suggest a message)  Use one agent reply as an argument",
        "  echo $(date)        Run shell command substitution via Bash or Zsh",
        "  #3, codex?#3        Show a numbered result or full agent reply",
        "  #                   Show the latest completed agent reply",
        "  yes, yes #3         Reply yes to the latest or a numbered agent reply",
        "  yes!, yes! #3       Reply yes and let the agent change files",
        "  Down at empty prompt  Expand prompt, replies, and model details",
        "  claude? agree? #3   Include a numbered result; queue if an agent is still working",
        "  opus? https://...   GET the URL once, then include the full page",
    ] {
        println!("{}", terminal::styled_input(line, color));
    }
    #[cfg(target_os = "macos")]
    {
        println!();
        println!("# macOS reader account for bash? and zsh?:");
        println!("  sudo sysadminctl -addUser staffer -fullName \"Staff reader\" -GID 20 -shell /usr/bin/false");
        println!("  id staffer  # Check that gid=20(staff) and admin is absent");
        println!("  # Give staff read/search access to the current tree:");
        println!("    chgrp -R staff ./");
        println!("    chmod -R 750 ./    # owner 7, staff 5, others 0");
        println!("  # This also makes regular files executable; check any ACLs.");
        println!("  # To skip sudo password prompts, run sudo visudo and add at the end:");
        if let Some(username) = current_username() {
            println!("    {username} ALL=(staffer) NOPASSWD: ALL");
        } else {
            println!("    <your-username> ALL=(staffer) NOPASSWD: ALL");
            println!("    # Replace <your-username> with the output of whoami.");
        }
        println!("  # This allows your user to run any command as staffer without a password.");
        println!("  # staffer can still write wherever its Unix permissions allow.");
    }
}

#[cfg(target_os = "macos")]
fn current_username() -> Option<String> {
    let output = Command::new("/usr/bin/id").arg("-un").output().ok()?;
    let username = std::str::from_utf8(&output.stdout).ok()?.trim();
    if output.status.success()
        && !username.is_empty()
        && !username.starts_with('-')
        && username
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        Some(username.to_owned())
    } else {
        None
    }
}

#[cfg(test)]
fn session_context(session: &Session) -> String {
    session_context_filtered(session, &BTreeSet::new(), None, &session.cwd)
}

fn session_context_filtered(
    session: &Session,
    referenced_ids: &BTreeSet<usize>,
    current_request_id: Option<usize>,
    cwd: &Path,
) -> String {
    let mut context = format!("cwd: {}\n", cwd.display());
    let mut watch_bytes_left = MAX_TOTAL_IMPLICIT_WATCH_CONTEXT_BYTES;
    for live in &session.live_commands {
        if referenced_ids.contains(&live.id) {
            continue;
        }
        if watch_bytes_left == 0 {
            context.push_str(
                "... more watch output omitted from automatic context; refer to a watch by #n\n",
            );
            break;
        }
        let output = live_current_text(live);
        let visible_bytes = output
            .len()
            .min(MAX_IMPLICIT_WATCH_CONTEXT_BYTES)
            .min(watch_bytes_left);
        context.push_str(&format!(
            "watch {} {} in {}:\n{}\n",
            watch_revision_label(live),
            live.label(),
            live.cwd.display(),
            watch_context_excerpt(&output, visible_bytes)
        ));
        watch_bytes_left -= visible_bytes;
    }
    // Preserve the conversation: older requests and comments can contain
    // instructions that still apply. Bound bulky external sources individually.
    for event in session.events.iter().filter(|event| match event {
        Event::AgentRequest { id, .. } => Some(*id) != current_request_id,
        Event::AgentResponse { id, .. } => !referenced_ids.contains(id),
        Event::Resource { id, .. } | Event::HttpHead { id, .. } => !referenced_ids.contains(id),
        _ => true,
    }) {
        match event {
            Event::Command { input, status } => {
                context.push_str(&format!("command: {input} (exit {status})\n"))
            }
            Event::Comment(text) => context.push_str(&format!("comment: {text}\n")),
            Event::AgentRequest {
                id,
                input,
                access,
                user,
                model,
                ..
            } => context.push_str(&format!(
                "request ({}, {}{}): {input}\n",
                AgentLabel::new(model.as_deref().unwrap_or("default"), *access, *id)
                    .in_session(session),
                access.description(),
                match user {
                    AgentUser::Current => "",
                    AgentUser::Staffer => ", staffer",
                }
            )),
            Event::AgentResponse {
                id, model, text, ..
            } => {
                let label = agent_label_by_id(session, *id)
                    .map(|label| AgentLabel { model, ..label }.to_string())
                    .unwrap_or_else(|| format!("{model}#{id}"));
                context.push_str(&format!("response ({label}): {text}\n"))
            }
            Event::Resource { id, resource } => {
                // Show an HTML page's text, not its markup: inline CSS and
                // scripts can otherwise fill the whole excerpt.
                let (text, note) = if html::is_html(&resource.content_type) {
                    (
                        html::text(&resource.content).into(),
                        ", page text without markup",
                    )
                } else {
                    (std::borrow::Cow::Borrowed(resource.content.as_str()), "")
                };
                context.push_str(&format!(
                    "resource: {} ({}{note})\n",
                    resource_label(resource),
                    resource.content_type
                ));
                let mut content = text.chars();
                context.extend(content.by_ref().take(8_000));
                context.push('\n');
                if content.next().is_some() {
                    context.push_str(&format!(
                        "... more web page content omitted; refer to #{id} for the stored result\n"
                    ));
                }
            }
            Event::HttpHead { url, output, .. } => {
                context.push_str(&format!("HEAD {url}:\n{output}\n"));
            }
        }
    }
    for todo in &session.todos {
        context.push_str(&format!(
            "todo: - [{}] {}\n",
            if todo.done { 'x' } else { ' ' },
            todo.text
        ));
    }
    context
}

fn agent_response<'a>(
    session: &'a Session,
    model: &str,
    id: usize,
    workers: &Workers,
) -> Result<&'a str, String> {
    if let Some(text) = session.events.iter().find_map(|event| match event {
        Event::AgentResponse {
            id: response_id,
            model: response_model,
            text,
            ..
        } if *response_id == id && response_model == model => Some(text.as_str()),
        _ => None,
    }) {
        return Ok(text);
    }
    let reference = agent_label_by_id(session, id)
        .map(|label| AgentLabel { model, ..label }.to_string())
        .unwrap_or_else(|| format!("{model}#{id}"));
    let requested = session.events.iter().any(|event| {
        matches!(event,
            Event::AgentRequest { id: request_id, model: Some(request_model), .. }
                if *request_id == id && request_model == model
        )
    });
    if requested && workers.pending_agents.contains_key(&id) {
        Err(format!("{reference} is still working"))
    } else if requested {
        Err(format!("{reference} has no reply"))
    } else {
        Err(format!("no reply found for {reference}"))
    }
}

fn agent_response_by_id<'a>(
    session: &'a Session,
    id: usize,
    workers: &Workers,
) -> Result<(AgentLabel<'a>, &'a str), String> {
    let label =
        agent_label_by_id(session, id).ok_or_else(|| format!("no reply found for #{id}"))?;
    let text = agent_response(session, label.model, id, workers)?;
    Ok((label, text))
}

/// Sends `yes #id` to the agent that wrote that answer, with the same model and options.
/// `access` overrides the original access, as `yes!` does.
fn reply_yes(
    id: Option<usize>,
    access: Option<AgentAccess>,
    session: &mut Session,
    workers: &mut Workers,
) {
    let label = match id {
        Some(id) => agent_label_by_id(session, id),
        None => latest_agent_response(session).map(|(label, _)| label),
    };
    let Some((model, id, options)) = label.and_then(|label| {
        let options = AgentOptions {
            // Reply as the account that wrote the answer, so `yes` to
            // `@staffer opus?` stays with staffer.
            user: agent_user_by_id(session, label.id),
            effort: label.effort,
            output: label.output,
            ..AgentOptions::new(access.unwrap_or(label.access))
        };
        Some((AgentModel::parse(label.model)?, label.id, options))
    }) else {
        match id {
            Some(id) => eprintln!("lynn: no reply found for #{id}"),
            None => eprintln!("lynn: no agent reply yet"),
        }
        session.last_status = 1;
        return;
    };
    let task = format!("yes #{id}");
    // Reply with the Pi model that answered, then restore the session's latest choice.
    let replied_pi_model = session.events.iter().find_map(|event| match event {
        Event::AgentRequest {
            id: request_id,
            pi_model,
            ..
        } if *request_id == id => pi_model.clone(),
        _ => None,
    });
    let latest_pi_model = match replied_pi_model {
        Some(pi_model) => session.pi_model.replace(pi_model),
        None => session.pi_model.clone(),
    };
    start_agents(
        AgentTask {
            task: &task,
            models: vec![model],
            options,
        },
        session,
        workers,
    );
    session.pi_model = latest_pi_model;
}

fn latest_agent_response(session: &Session) -> Option<(AgentLabel<'_>, &str)> {
    session.events.iter().rev().find_map(|event| {
        if let Event::AgentResponse {
            id, model, text, ..
        } = event
        {
            agent_label_by_id(session, *id)
                .map(|label| (AgentLabel { model, ..label }, text.as_str()))
        } else {
            None
        }
    })
}

/// The label of the agent request with this id, with its recorded effort and output.
fn agent_label_by_id(session: &Session, id: usize) -> Option<AgentLabel<'_>> {
    session.events.iter().find_map(|event| match event {
        Event::AgentRequest {
            id: request_id,
            model: Some(model),
            access,
            ..
        } if *request_id == id => Some(AgentLabel::new(model, *access, id).in_session(session)),
        _ => None,
    })
}

/// The account the agent request with this id ran as.
fn agent_user_by_id(session: &Session, id: usize) -> AgentUser {
    session
        .events
        .iter()
        .find_map(|event| match event {
            Event::AgentRequest {
                id: request_id,
                user,
                ..
            } if *request_id == id => Some(*user),
            _ => None,
        })
        .unwrap_or(AgentUser::Current)
}

fn http_response_by_id(session: &Session, id: usize) -> Option<String> {
    session.events.iter().find_map(|event| match event {
        Event::HttpHead {
            id: event_id,
            url,
            output,
        } if *event_id == id => Some(format!("HEAD {url}:\n{output}")),
        Event::Resource {
            id: event_id,
            resource,
        } if *event_id == id => Some(format!(
            "{} ({}):\n{}",
            resource_label(resource),
            resource.content_type,
            resource.content
        )),
        _ => None,
    })
}

/// The http:// and https:// URLs in an agent task, without trailing punctuation.
fn task_urls(task: &str) -> impl Iterator<Item = &str> {
    task.split_whitespace().filter_map(|word| {
        let start = word.find("https://").or_else(|| word.find("http://"))?;
        let url = word[start..]
            .trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '>', '"', '\'']);
        url.contains("://").then_some(url)
    })
}

/// The latest stored GET result for this URL, including URLs that redirected to it.
fn stored_resource_id(session: &Session, url: &str) -> Option<usize> {
    session.events.iter().rev().find_map(|event| match event {
        Event::Resource { id, resource } if resource.answers(url) => Some(*id),
        _ => None,
    })
}

/// Every URL a stored GET result answers.
fn stored_resource_urls(session: &Session) -> BTreeSet<String> {
    let mut urls = BTreeSet::new();
    for event in &session.events {
        if let Event::Resource { resource, .. } = event {
            urls.insert(resource.body_url.clone());
            urls.extend(resource.redirect_targets.keys().cloned());
        }
    }
    urls
}

/// The body URL, preceded by the redirects that led to it, e.g. `A → B → C`.
fn resource_label(resource: &Resource) -> String {
    let mut label = String::new();
    for (from, to) in &resource.redirect_targets {
        if label.is_empty() {
            label.push_str(from);
        } else if !label.ends_with(from.as_str()) {
            label.push_str(&format!("; {from}"));
        }
        label.push_str(&format!(" → {to}"));
    }
    if label.is_empty() {
        label.push_str(&resource.body_url);
    }
    label
}

fn watch_response_by_id(session: &Session, id: usize, revision: Option<usize>) -> Option<String> {
    let watch = session.live_commands.iter().find(|watch| watch.id == id)?;
    let revision = revision.unwrap_or(watch.revisions.len());
    let output = watch.revisions.get(revision.checked_sub(1)?)?;
    Some(format!(
        "watch #{id}.{revision} {} in {}:\n{output}",
        watch.label(),
        watch.cwd.display()
    ))
}

fn watch_context_excerpt(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let marker = format!(
        "\n... middle of watch output omitted for agent context ({} bytes total) ...\n",
        text.len()
    );
    let available = max_bytes.saturating_sub(marker.len());
    let mut head_end = available / 2;
    let mut tail_start = text.len() - (available - head_end);
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!("{}{}{}", &text[..head_end], marker, &text[tail_start..])
}

fn watch_revision_label(watch: &LiveCommand) -> String {
    if watch.revisions.is_empty() {
        format!("#{}", watch.id)
    } else {
        format!("#{}.{}", watch.id, watch.revisions.len())
    }
}

fn watch_reference_error(watch: &LiveCommand, revision: Option<usize>) -> String {
    match (revision, watch.revisions.len(), &watch.error) {
        (None | Some(1), 0, None) => format!("watch #{} is still loading", watch.id),
        (_, 0, Some(error)) => format!("watch #{} has no result: {error}", watch.id),
        (Some(revision), _, _) => format!("watch #{} has no revision {revision}", watch.id),
        (None, _, _) => unreachable!(),
    }
}

fn ready_references(
    task: &str,
    session: &Session,
    workers: &Workers,
) -> Result<ResolvedReferences, String> {
    match resolve_references(task, session, workers)? {
        ReferenceResolution::Ready(references) => Ok(references),
        ReferenceResolution::Pending(ids) => {
            let id = ids[0];
            if let Some(label) = agent_label_by_id(session, id) {
                Err(format!("{label} is still working"))
            } else if let Some(url) = workers
                .loading_urls
                .iter()
                .find_map(|(url, loading)| (*loading == id).then_some(url))
            {
                Err(format!("GET#{id} {url} is still loading"))
            } else {
                Err(format!("watch #{id} is still loading"))
            }
        }
    }
}

fn resolve_shell_references(
    command: &str,
    session: &mut Session,
    workers: &mut Workers,
) -> Result<BTreeMap<String, String>, String> {
    let arguments = shell::reference_arguments(command, session.last_status)?;
    if arguments.is_empty() {
        return Ok(BTreeMap::new());
    }
    let selection = arguments.join(" ");
    let mut announced_wait = false;
    loop {
        for line in drain_updates(workers, session) {
            println!("{line}");
        }
        match resolve_references(&selection, session, workers)? {
            ReferenceResolution::Ready(_) => {
                return arguments
                    .into_iter()
                    .map(|reference| {
                        let text = ready_references(&reference, session, workers)?.text;
                        Ok((reference, text))
                    })
                    .collect();
            }
            ReferenceResolution::Pending(ids) => {
                if !announced_wait {
                    let labels = ids
                        .iter()
                        .map(|id| format!("#{id}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!("[waiting for {labels}]");
                    let _ = io::stdout().flush();
                    announced_wait = true;
                }
            }
        }
        match workers.worker_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(update) => {
                if let Some(line) = format_worker_update(update, session, workers) {
                    println!("{line}");
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err("result workers disconnected".into()),
        }
    }
}

fn resolve_references(
    task: &str,
    session: &Session,
    workers: &Workers,
) -> Result<ReferenceResolution, String> {
    let mut context = String::new();
    let mut seen = BTreeSet::new();
    let mut seen_watch_revisions = BTreeSet::new();
    let mut pending = BTreeSet::new();
    // A URL in the task stands for its stored GET result, so the full page is
    // included rather than the excerpt in the session context.
    for url in task_urls(task) {
        if let Some(id) = stored_resource_id(session, url) {
            if seen.insert(id) {
                let text = http_response_by_id(session, id).unwrap();
                context.push_str(&format!("\nReferenced HTTP result #{id}:\n{text}\n"));
            }
        } else if let Some(&id) = workers.loading_urls.get(url) {
            pending.insert(id);
        }
    }
    // Skip URL words so fragments and paths are not read as references.
    let task = task
        .split_whitespace()
        .filter(|word| !word.contains("://"))
        .collect::<Vec<_>>()
        .join(" ");
    for raw_token in task.split(|character: char| {
        !(character.is_ascii_alphanumeric()
            || character == '#'
            || character == '.'
            || character == '_'
            || character == '-'
            || character == '?'
            || character == '!')
    }) {
        let token = raw_token.trim_end_matches(['.', '?', '!']);
        let reference = if let Some((id, revision)) = session::watch_revision_reference(token) {
            let watch = session
                .live_commands
                .iter()
                .find(|watch| watch.id == id)
                .ok_or_else(|| format!("no watch found for #{id}"))?;
            if let Some(text) = watch_response_by_id(session, id, Some(revision)) {
                if seen_watch_revisions.insert((id, revision)) {
                    seen.insert(id);
                    context.push_str(&format!(
                        "\nReferenced {}\n",
                        watch_context_excerpt(&text, MAX_REFERENCED_WATCH_CONTEXT_BYTES)
                    ));
                }
            } else if watch.revisions.is_empty() && watch.error.is_none() && revision == 1 {
                pending.insert(id);
            } else {
                return Err(watch_reference_error(watch, Some(revision)));
            }
            None
        } else if let Some((model, access, id)) = session::agent_reference(token) {
            let actual = agent_label_by_id(session, id);
            if actual.is_none_or(|actual| {
                actual.model != model || access.is_some_and(|access| access != actual.access)
            }) {
                let label = access
                    .map(|access| {
                        AgentLabel::new(model, access, id)
                            .in_session(session)
                            .to_string()
                    })
                    .unwrap_or_else(|| format!("{model}#{id}"));
                return Err(format!("no reply found for {label}"));
            }
            Some((model, id))
        } else if let Some(id) = session::agent_number_reference(token) {
            if let Some(label) = agent_label_by_id(session, id) {
                Some((label.model, id))
            } else if let Some(watch) = session.live_commands.iter().find(|watch| watch.id == id) {
                if let Some(text) = watch_response_by_id(session, id, None) {
                    let revision = watch.revisions.len();
                    if seen_watch_revisions.insert((id, revision)) {
                        seen.insert(id);
                        context.push_str(&format!(
                            "\nReferenced {}\n",
                            watch_context_excerpt(&text, MAX_REFERENCED_WATCH_CONTEXT_BYTES)
                        ));
                    }
                } else if watch.error.is_none() {
                    pending.insert(id);
                } else {
                    return Err(watch_reference_error(watch, None));
                }
                None
            } else if let Some(text) = http_response_by_id(session, id) {
                if seen.insert(id) {
                    context.push_str(&format!("\nReferenced HTTP result #{id}:\n{text}\n"));
                }
                None
            } else {
                return Err(format!("no reply found for #{id}"));
            }
        } else {
            None
        };
        if let Some((model, id)) = reference {
            if seen.insert(id) {
                match agent_response(session, model, id, workers) {
                    Ok(text) => {
                        let label = AgentLabel {
                            model,
                            ..agent_label_by_id(session, id).unwrap()
                        };
                        context.push_str(&format!("\nReferenced reply {label}:\n{text}\n"));
                    }
                    Err(_) if workers.pending_agents.contains_key(&id) => {
                        pending.insert(id);
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }
    if pending.is_empty() {
        Ok(ReferenceResolution::Ready(ResolvedReferences {
            text: context,
            ids: seen,
        }))
    } else {
        Ok(ReferenceResolution::Pending(pending.into_iter().collect()))
    }
}

fn drain_updates(workers: &mut Workers, session: &mut Session) -> Vec<String> {
    let mut lines = Vec::new();
    while let Ok(update) = workers.worker_rx.try_recv() {
        if let Some(line) = format_worker_update(update, session, workers) {
            lines.push(line);
        }
    }
    let mut watch_updated = false;
    while let Ok(update) = workers.live_rx.try_recv() {
        workers.active_live = workers.active_live.saturating_sub(1);
        if let Some(live) = session
            .live_commands
            .iter_mut()
            .find(|live| live.id == update.id)
        {
            if update.started_at >= live.last_refresh {
                watch_updated = true;
                if let Some(line) = update_live_command(live, update.result, Instant::now()) {
                    lines.push(line);
                }
            }
        }
    }
    if watch_updated {
        resume_queued_agents(session, workers);
    }
    lines
}

fn update_live_command(
    live: &mut LiveCommand,
    result: Result<String, String>,
    checked_at: Instant,
) -> Option<String> {
    live.last_refresh = checked_at;
    match result {
        Ok(output) => {
            let first_result = live.revisions.is_empty();
            let changed = first_result || output != live.output || live.error.take().is_some();
            if live.revisions.last() != Some(&output) {
                live.revisions.push(output.clone());
            }
            live.output = output.clone();
            if live.last_run.is_none() {
                live.last_run = Some(output.clone());
            }
            changed.then(|| {
                format!(
                    "[watch #{}.{} {}] {output}",
                    live.id,
                    live.revisions.len(),
                    live.label()
                )
            })
        }
        Err(error) if live.error.as_deref() != Some(&error) => {
            live.error = Some(error.clone());
            if live.last_run.is_none() {
                live.last_run = Some(format!("error: {error}"));
            }
            Some(format!(
                "[watch #{} {} error] {error}",
                live.id,
                live.label()
            ))
        }
        Err(_) => None,
    }
}

fn format_worker_update(
    update: WorkerUpdate,
    session: &mut Session,
    workers: &mut Workers,
) -> Option<String> {
    match update {
        WorkerUpdate::Agent(update) => Some(format_update(update, session, workers)),
        WorkerUpdate::AgentProgress { id, status } => {
            if let Some(pending) = workers.pending_agents.get_mut(&id) {
                let label = pending.split(':').next().unwrap_or(pending);
                *pending = format!("{label}: {status}");
                if !workers.interactive {
                    return Some(pending.clone());
                }
            }
            None
        }
        WorkerUpdate::Http(update) => {
            workers.active_http = workers.active_http.saturating_sub(1);
            workers.loading_urls.retain(|_, id| *id != update.id);
            let line = match update.result {
                Ok(HttpResult::Resource(resource)) => {
                    let message = format!(
                        "GET#{} {} ({} {} bytes): …",
                        update.id,
                        resource_label(&resource),
                        resource.content_type,
                        resource.content.len()
                    );
                    workers.pending_http.remove(&update.id);
                    session.events.push(Event::Resource {
                        id: update.id,
                        resource,
                    });
                    Some(message)
                }
                Ok(HttpResult::Stored {
                    url,
                    redirect_targets,
                }) => {
                    workers.pending_http.remove(&update.id);
                    let requested = redirect_targets.keys().next().cloned().unwrap_or_default();
                    let stored = session
                        .events
                        .iter_mut()
                        .rev()
                        .find_map(|event| match event {
                            Event::Resource { id, resource } if resource.answers(&url) => {
                                Some((*id, resource))
                            }
                            _ => None,
                        });
                    match stored {
                        Some((id, resource)) => {
                            for (from, to) in redirect_targets {
                                resource.redirect_targets.entry(from).or_insert(to);
                            }
                            Some(format!(
                                "GET#{} {requested} redirects to {url}, already loaded as #{id}",
                                update.id
                            ))
                        }
                        None => {
                            session.last_status = 1;
                            Some(format!(
                                "GET#{} error: {requested} redirects to {url}, which is no longer stored",
                                update.id
                            ))
                        }
                    }
                }
                Ok(HttpResult::Head { url, output }) => {
                    let status = output.lines().next().unwrap_or("HTTP response");
                    let message = format!("HEAD#{} {url} ({status}): …", update.id);
                    workers.pending_http.remove(&update.id);
                    session.events.push(Event::HttpHead {
                        id: update.id,
                        url,
                        output,
                    });
                    Some(message)
                }
                Err(error) => {
                    workers.pending_http.remove(&update.id);
                    session.last_status = 1;
                    let label = match update.kind {
                        HttpRequestKind::Load => "GET",
                        HttpRequestKind::Head => "HEAD",
                    };
                    Some(format!("{label}#{} error: {error}", update.id))
                }
            };
            resume_queued_agents(session, workers);
            line
        }
    }
}

fn format_update(update: AgentUpdate, session: &mut Session, workers: &mut Workers) -> String {
    workers.active_agents = workers.active_agents.saturating_sub(1);
    workers.pending_agents.remove(&update.id);
    if workers.running_writer == Some(update.id) {
        workers.running_writer = None;
    }
    let label = AgentLabel::new(&update.model, update.access, update.id).in_session(session);
    let line = match update.result {
        Ok(reply) => {
            let display = match update.access {
                AgentAccess::ReadOnly => AgentDisplay::Answer(&reply.text),
                AgentAccess::ReadWrite => AgentDisplay::FullAnswer(&reply.text),
            };
            let line = render_agent(label, display, Styling::Terminal);
            session.events.push(Event::AgentResponse {
                id: update.id,
                model: update.model,
                text: reply.text,
                model_info: reply.model_info,
                elapsed: Some(update.elapsed),
            });
            line
        }
        Err(error) => render_agent(label, AgentDisplay::Error(&error), Styling::Terminal),
    };
    resume_queued_agents(session, workers);
    line
}

/// Holds queued agents while the user writes a `#` note, and resumes them once
/// the hold ends so they launch with the note in their context.
fn set_note_hold(session: &Session, workers: &mut Workers, holding: bool) {
    if workers.holding_for_note != holding {
        workers.holding_for_note = holding;
        resume_queued_agents(session, workers);
    }
}

fn resume_queued_agents(session: &Session, workers: &mut Workers) {
    if workers.holding_for_note {
        for (&id, request) in &workers.queued_agents {
            workers.pending_agents.insert(
                id,
                format!("{}: paused while you write a # note", request.label()),
            );
        }
        return;
    }
    let ids: Vec<usize> = workers.queued_agents.keys().copied().collect();
    for id in ids {
        let Some(request) = workers.queued_agents.get(&id) else {
            continue;
        };
        let resolution = resolve_references(&request.task, session, workers);
        match resolution {
            Ok(ReferenceResolution::Ready(references)) => {
                if let Some(predecessor) = mutating_predecessor(request, workers) {
                    let status = queued_behind_status(request, predecessor, session);
                    workers.pending_agents.insert(id, status);
                } else {
                    let request = workers.queued_agents.remove(&id).unwrap();
                    launch_agent(request, references, session, workers);
                }
            }
            Ok(ReferenceResolution::Pending(ids)) => {
                let status = ids
                    .iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                workers
                    .pending_agents
                    .insert(id, format!("{}: waiting for {status}", request.label()));
            }
            Err(error) => {
                let request = workers.queued_agents.remove(&id).unwrap();
                let _ = workers.worker_tx.send(WorkerUpdate::Agent(AgentUpdate {
                    elapsed: Duration::ZERO,
                    id,
                    model: request.model.name().to_owned(),
                    access: request.options.access,
                    result: Err(format!("referenced reply unavailable: {error}")),
                }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        agent_context, agent_response, agent_response_by_id, diff_lines, emphasize_prompt,
        expanded_agent_results, format_elapsed, format_update, highlight_answer,
        latest_agent_response, prompt, ready_references, render_agent, render_session_agent,
        reset_state, resolve_references, session_context, shortened_path, start_agents,
        AgentDisplay, AgentLabel, AgentUpdate, PromptLine, ReferenceResolution, Styling, Workers,
    };
    use crate::agent_cli::AgentUser;
    use crate::session::{
        AgentAccess, AgentModel, AgentOptions, AgentOutput, AgentTask, DiffMode, DisplayDetail,
        Event, PromptMode, ReasoningEffort, Session,
    };
    use crate::terminal::Terminal;
    use std::path::Path;
    use std::time::Duration;

    #[test]
    fn conversation_context_retains_early_instructions_and_large_replies() {
        let mut session = Session::new().unwrap();
        session.events.push(Event::Comment(
            "Always preserve the existing CLI syntax".into(),
        ));
        session.events.push(Event::AgentRequest {
            id: 1,
            input: "Keep mutating agents serial across all models".into(),
            access: AgentAccess::ReadWrite,
            user: AgentUser::Current,
            model: Some("opus".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Final,
            pi_model: None,
        });
        let reply = format!(
            "Earlier findings:\n{}\nEnd of earlier findings",
            "Important finding αβγ\n".repeat(2_000)
        );
        session.events.push(Event::AgentResponse {
            id: 1,
            model: "opus".into(),
            text: reply.clone(),
            model_info: None,
            elapsed: None,
        });
        for number in 0..100 {
            session.events.push(Event::Command {
                input: format!("command-{number}"),
                status: 0,
            });
        }
        session
            .events
            .push(Event::Comment("Latest instruction".into()));
        session.todos.push(crate::session::Todo {
            text: "Validate cancellation".into(),
            done: false,
        });
        let context = session_context(&session);
        assert!(context.starts_with(&format!("cwd: {}\n", session.cwd.display())));
        assert!(context.contains("comment: Always preserve the existing CLI syntax\n"));
        assert!(context.contains(
            "request (opus!#1, read-write): Keep mutating agents serial across all models\n"
        ));
        assert!(context.contains(&format!("response (opus!#1): {reply}\n")));
        assert!(context.contains("command: command-0 (exit 0)\n"));
        assert!(context.contains("command: command-99 (exit 0)\n"));
        assert!(
            context.ends_with("comment: Latest instruction\ntodo: - [ ] Validate cancellation\n")
        );
        assert!(
            context.find("Always preserve").unwrap() < context.find("Earlier findings").unwrap()
        );
        assert!(
            context.find("End of earlier findings").unwrap() < context.find("command-0").unwrap()
        );
    }

    #[test]
    fn web_excerpts_report_omissions_and_explicit_references_retain_full_content() {
        let mut session = Session::new().unwrap();
        session
            .events
            .push(Event::Comment("Preserve this instruction".into()));
        session.events.push(Event::Resource {
            id: 3,
            resource: crate::session::Resource {
                body_url: "https://example.com/large".into(),
                redirect_targets: Default::default(),
                content_type: "text/plain".into(),
                content: format!("{}TAIL_OF_PAGE", "α".repeat(9_000)),
            },
        });
        let context = session_context(&session);
        assert!(context.contains("comment: Preserve this instruction\n"));
        assert!(context.contains(&"α".repeat(8_000)));
        assert!(!context.contains("TAIL_OF_PAGE"));
        assert!(context.contains("more web page content omitted; refer to #3"));
        let workers = Workers::new(false);
        let references = ready_references("Read #3", &session, &workers).unwrap();
        assert!(references.text.contains(&"α".repeat(9_000)));
        assert!(references.text.contains("TAIL_OF_PAGE"));
        let automatic =
            super::session_context_filtered(&session, &references.ids, None, &session.cwd);
        assert!(!automatic.contains("resource:"));
        assert!(automatic.contains("comment: Preserve this instruction\n"));
    }

    #[test]
    fn html_excerpts_skip_inline_styles_and_references_keep_markup() {
        let mut session = Session::new().unwrap();
        session.events.push(Event::Resource {
            id: 4,
            resource: crate::session::Resource {
                body_url: "https://example.com/docs".into(),
                redirect_targets: Default::default(),
                content_type: "text/html; charset=utf-8".into(),
                content: format!(
                    "<html><head><style>{}</style></head><body><p>Article text</p></body></html>",
                    ".a { color: red }\n".repeat(1_000)
                ),
            },
        });
        let context = session_context(&session);
        assert!(context.contains("(text/html; charset=utf-8, page text without markup)"));
        assert!(context.contains("Article text"));
        assert!(!context.contains("color: red"));
        assert!(!context.contains("more web page content omitted"));
        let workers = Workers::new(false);
        let references = ready_references("Read #4", &session, &workers).unwrap();
        assert!(references.text.contains("<p>Article text</p>"));
    }

    #[test]
    fn task_urls_wait_for_loading_gets_and_reuse_stored_results() {
        let mut session = Session::new().unwrap();
        let mut workers = Workers::new(false);
        let url = "https://example.com/post";
        let task = format!("compare ({url}). with lynn");
        assert_eq!(super::task_urls(&task).collect::<Vec<_>>(), [url]);
        workers.loading_urls.insert(url.into(), 2);
        assert!(matches!(
            resolve_references(&task, &session, &workers).unwrap(),
            ReferenceResolution::Pending(ids) if ids == [2]
        ));
        workers.loading_urls.clear();
        session.events.push(Event::Resource {
            id: 2,
            resource: crate::session::Resource {
                body_url: url.into(),
                redirect_targets: Default::default(),
                content_type: "text/plain".into(),
                content: format!("{}TAIL_OF_PAGE", "β".repeat(9_000)),
            },
        });
        let references = ready_references(&task, &session, &workers).unwrap();
        assert!(references.ids.contains(&2));
        assert!(references.text.contains("Referenced HTTP result #2"));
        assert!(references.text.contains("TAIL_OF_PAGE"));
        assert_eq!(super::stored_resource_id(&session, url), Some(2));
        // URLs that redirected to a stored result find it too.
        session.events.push(Event::Resource {
            id: 4,
            resource: crate::session::Resource {
                body_url: "https://raw.example.com/c".into(),
                redirect_targets: [
                    (
                        "https://example.com/a".into(),
                        "https://example.com/b".into(),
                    ),
                    (
                        "https://example.com/b".into(),
                        "https://raw.example.com/c".into(),
                    ),
                    (
                        "https://example.com/d".into(),
                        "https://example.com/b".into(),
                    ),
                ]
                .into_iter()
                .collect(),
                content_type: "text/plain".into(),
                content: "C".into(),
            },
        });
        for url in ["a", "b", "d"] {
            let url = format!("https://example.com/{url}");
            assert_eq!(super::stored_resource_id(&session, &url), Some(4));
        }
        assert_eq!(
            super::stored_resource_id(&session, "https://raw.example.com/c"),
            Some(4)
        );
        let Some(Event::Resource { resource, .. }) = session.events.last() else {
            unreachable!()
        };
        assert_eq!(
            super::resource_label(resource),
            "https://example.com/a → https://example.com/b → https://raw.example.com/c; https://example.com/d → https://example.com/b"
        );
        assert!(super::stored_resource_urls(&session).contains("https://example.com/d"));
        // A fragment in a URL is not a reference, and unknown URLs are left to the agent.
        let references =
            ready_references("read https://example.com/#3 too", &session, &workers).unwrap();
        assert!(references.ids.is_empty());
    }

    #[test]
    fn down_expansion_shows_prompts_above_running_and_queued_statuses() {
        let mut session = Session::new().unwrap();
        let mut workers = Workers::new(false);
        for (id, model, input, status) in [
            (4, "opus", "Implement cancellation", "opus!#4: …"),
            (5, "sol", "Update the tests", "sol!#5: waiting for opus!#4"),
        ] {
            session.events.push(Event::AgentRequest {
                id,
                input: input.into(),
                access: AgentAccess::ReadWrite,
                user: AgentUser::Current,
                model: Some(model.into()),
                effort: ReasoningEffort::Default,
                output: AgentOutput::Final,
                pi_model: None,
            });
            workers.pending_agents.insert(id, status.into());
        }
        assert_eq!(
            super::pending_agent_statuses(&session, &workers, false),
            ["opus!#4: …", "sol!#5: waiting for opus!#4",]
        );
        session.detail = DisplayDetail::Expanded;
        assert!(expanded_agent_results(&mut session).is_empty());
        assert_eq!(
            super::pending_agent_statuses(&session, &workers, false),
            [
                "opus!#4 prompt: Implement cancellation",
                "opus!#4: …",
                "sol!#5 prompt: Update the tests",
                "sol!#5: waiting for opus!#4",
            ]
        );
        workers
            .pending_agents
            .insert(4, "opus!#4: Edit: src/main.rs".into());
        assert_eq!(
            &super::pending_agent_statuses(&session, &workers, false)[..2],
            [
                "opus!#4 prompt: Implement cancellation",
                "opus!#4: Edit: src/main.rs",
            ]
        );
        workers.pending_agents.remove(&4);
        assert_eq!(
            super::pending_agent_statuses(&session, &workers, false),
            [
                "sol!#5 prompt: Update the tests",
                "sol!#5: waiting for opus!#4",
            ]
        );
        // Expanding a pending status must not mark a future answer as already expanded.
        assert!(session.expanded_response_ids.is_empty());
    }

    #[test]
    fn pending_prompt_preserves_multiline_input_and_live_selector() {
        let mut session = Session::new().unwrap();
        let mut workers = Workers::new(false);
        session.events.push(Event::AgentRequest {
            id: 4,
            input: "Implement cancellation\nPreserve existing labels".into(),
            access: AgentAccess::ReadWrite,
            user: AgentUser::Current,
            model: Some("opus".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Live,
            pi_model: None,
        });
        session.detail = DisplayDetail::Expanded;
        workers
            .pending_agents
            .insert(4, "opus?!#4: Read: src/main.rs".into());
        assert_eq!(
            super::pending_agent_statuses(&session, &workers, true),
            [
                "\x1b[1mopus?!#4 prompt: Implement cancellation\nPreserve existing labels\x1b[0m",
                "opus?!#4: Read: src/main.rs",
            ]
        );
        session.detail = DisplayDetail::Compact;
        assert_eq!(
            super::pending_agent_statuses(&session, &workers, false),
            ["opus?!#4: Read: src/main.rs",]
        );
    }

    #[test]
    fn compact_status_shows_running_writer_prompt_on_one_line() {
        let mut session = Session::new().unwrap();
        let mut workers = Workers::new(false);
        for (id, input, access) in [
            (
                4,
                "Implement cancellation\nPreserve existing labels",
                AgentAccess::ReadWrite,
            ),
            (5, "Update the tests", AgentAccess::ReadWrite),
            (6, "Explain the queue", AgentAccess::ReadOnly),
        ] {
            session.events.push(Event::AgentRequest {
                id,
                input: input.into(),
                access,
                user: AgentUser::Current,
                model: Some("opus".into()),
                effort: ReasoningEffort::Default,
                output: AgentOutput::Final,
                pi_model: None,
            });
        }
        workers.running_writer = Some(4);
        workers
            .pending_agents
            .insert(4, "opus!#4: Edit: src/main.rs".into());
        workers
            .pending_agents
            .insert(5, "opus!#5: waiting for opus!#4".into());
        workers.pending_agents.insert(6, "opus?#6: …".into());
        // Only the running `!` request gets a prompt line; later lines become `…`.
        assert_eq!(
            super::pending_agent_statuses_with_width(&session, &workers, false, 80),
            [
                "opus!#4 prompt: Implement cancellation…",
                "opus!#4: Edit: src/main.rs",
                "opus!#5: waiting for opus!#4",
                "opus?#6: …",
            ]
        );
        // A long first line is cut so the prompt never wraps.
        assert_eq!(
            super::pending_agent_statuses_with_width(&session, &workers, false, 30)[0],
            "opus!#4 prompt: Implement ca…"
        );
        assert_eq!(super::one_line_prompt("  \n short ", 10), "short");
        assert_eq!(super::one_line_prompt("exactly", 7), "exactly");
        assert_eq!(super::one_line_prompt("exactly!", 7), "exactl…");
    }

    #[test]
    fn mutating_models_share_one_queue_and_read_only_requests_bypass_it() {
        let mut session = Session::new().unwrap();
        let mut workers = Workers::new(false);
        workers.running_writer = Some(1);
        workers.next_agent_id = 2;
        workers.pending_agents.insert(1, "opus!#1: …".into());
        workers.active_agents = 1;
        session.events.push(Event::AgentRequest {
            id: 1,
            input: "first change".into(),
            access: AgentAccess::ReadWrite,
            user: AgentUser::Current,
            model: Some("opus".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Final,
            pi_model: None,
        });
        for model in [AgentModel::Opus, AgentModel::Sol] {
            start_agents(
                AgentTask {
                    task: "another change",
                    models: vec![model],
                    options: AgentOptions::new(AgentAccess::ReadWrite),
                },
                &mut session,
                &mut workers,
            );
        }
        assert_eq!(
            workers.queued_agents.keys().copied().collect::<Vec<_>>(),
            [2, 3]
        );
        assert_eq!(workers.pending_agents[&2], "opus!#2: waiting for opus!#1");
        assert_eq!(workers.pending_agents[&3], "sol!#3: waiting for opus!#1");
        assert_eq!(workers.running_writer, Some(1));

        start_agents(
            AgentTask {
                task: "inspect",
                models: vec![AgentModel::Jsc],
                // Unsupported live output returns locally, without launching a provider.
                options: AgentOptions {
                    output: crate::session::AgentOutput::Live,
                    ..AgentOptions::new(AgentAccess::ReadOnly)
                },
            },
            &mut session,
            &mut workers,
        );
        assert!(!workers.queued_agents.contains_key(&4));
        let update = workers
            .worker_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        super::format_worker_update(update, &mut session, &mut workers);
        assert_eq!(workers.running_writer, Some(1));
        assert_eq!(workers.queued_agents.len(), 2);
    }

    #[test]
    fn yes_replies_as_the_user_that_ran_the_request() {
        let mut session = Session::new().unwrap();
        let mut workers = Workers::new(false);
        // A running write keeps the `yes!` replies queued instead of launching.
        workers.running_writer = Some(1);
        workers.next_agent_id = 3;
        workers.pending_agents.insert(1, "opus!#1: …".into());
        workers.active_agents = 1;
        for (id, user) in [(1, AgentUser::Current), (2, AgentUser::Staffer)] {
            session.events.push(Event::AgentRequest {
                id,
                input: "inspect".into(),
                access: AgentAccess::ReadOnly,
                user,
                model: Some("opus".into()),
                effort: ReasoningEffort::Default,
                output: AgentOutput::Final,
                pi_model: None,
            });
            session.events.push(Event::AgentResponse {
                id,
                model: "opus".into(),
                text: "Shall I go ahead?".into(),
                model_info: None,
                elapsed: None,
            });
        }
        super::reply_yes(
            Some(2),
            Some(AgentAccess::ReadWrite),
            &mut session,
            &mut workers,
        );
        super::reply_yes(
            Some(1),
            Some(AgentAccess::ReadWrite),
            &mut session,
            &mut workers,
        );
        assert_eq!(workers.queued_agents[&3].options.user, AgentUser::Staffer);
        assert_eq!(workers.queued_agents[&4].options.user, AgentUser::Current);
        assert!(
            session_context(&session).contains("request (opus!#3, read-write, staffer): yes #2\n")
        );
        assert!(session_context(&session).contains("request (opus!#4, read-write): yes #1\n"));
    }

    #[test]
    fn later_writes_do_not_overtake_an_earlier_write_waiting_for_references() {
        let mut session = Session::new().unwrap();
        let mut workers = Workers::new(false);
        workers.next_agent_id = 2;
        workers.pending_agents.insert(1, "opus?#1: …".into());
        workers.active_agents = 1;
        session.events.push(Event::AgentRequest {
            id: 1,
            input: "research".into(),
            access: AgentAccess::ReadOnly,
            user: AgentUser::Current,
            model: Some("opus".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Final,
            pi_model: None,
        });
        for (model, task) in [
            (AgentModel::Opus, "implement #1"),
            (AgentModel::Sol, "another change"),
        ] {
            start_agents(
                AgentTask {
                    task,
                    models: vec![model],
                    options: AgentOptions::new(AgentAccess::ReadWrite),
                },
                &mut session,
                &mut workers,
            );
        }
        assert_eq!(workers.running_writer, None);
        assert_eq!(workers.pending_agents[&2], "opus!#2: waiting for #1");
        assert_eq!(workers.pending_agents[&3], "sol!#3: waiting for opus!#2");
        assert_eq!(
            super::mutating_predecessor(&workers.queued_agents[&3], &workers),
            Some(2)
        );
    }

    #[test]
    fn writer_queue_advances_on_success_and_failure() {
        let mut session = Session::new().unwrap();
        let mut workers = Workers::new(false);
        workers.running_writer = Some(1);
        workers.next_agent_id = 2;
        workers.pending_agents.insert(1, "opus!#1: …".into());
        workers.active_agents = 1;
        session.events.push(Event::AgentRequest {
            id: 1,
            input: "first change".into(),
            access: AgentAccess::ReadWrite,
            user: AgentUser::Current,
            model: Some("opus".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Final,
            pi_model: None,
        });
        for _ in 0..2 {
            start_agents(
                AgentTask {
                    task: "inspect",
                    models: vec![AgentModel::Jsc],
                    options: AgentOptions {
                        output: crate::session::AgentOutput::Live,
                        ..AgentOptions::new(AgentAccess::ReadWrite)
                    },
                },
                &mut session,
                &mut workers,
            );
        }
        format_update(
            AgentUpdate {
                elapsed: Duration::ZERO,
                id: 1,
                model: "opus".into(),
                access: AgentAccess::ReadWrite,
                result: Ok("first change completed".into()),
            },
            &mut session,
            &mut workers,
        );
        assert_eq!(workers.running_writer, Some(2));
        assert!(!workers.queued_agents.contains_key(&2));
        assert!(workers.queued_agents.contains_key(&3));
        let update = workers
            .worker_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let line = super::format_worker_update(update, &mut session, &mut workers).unwrap();
        assert!(line.contains("live status is unavailable"));
        assert_eq!(workers.running_writer, Some(3));
        assert!(workers.queued_agents.is_empty());
        let update = workers
            .worker_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        super::format_worker_update(update, &mut session, &mut workers);
        assert_eq!(workers.running_writer, None);
        assert_eq!(workers.active_agents, 0);
        assert!(workers.pending_agents.is_empty());
    }

    #[test]
    fn typing_a_note_pauses_queued_agents_until_it_is_entered() {
        let mut session = Session::new().unwrap();
        let mut workers = Workers::new(false);
        workers.running_writer = Some(1);
        workers.next_agent_id = 2;
        workers.pending_agents.insert(1, "opus!#1: …".into());
        workers.active_agents = 1;
        session.events.push(Event::AgentRequest {
            id: 1,
            input: "first change".into(),
            access: AgentAccess::ReadWrite,
            user: AgentUser::Current,
            model: Some("opus".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Final,
            pi_model: None,
        });
        start_agents(
            AgentTask {
                task: "inspect",
                models: vec![AgentModel::Jsc],
                options: AgentOptions {
                    output: crate::session::AgentOutput::Live,
                    ..AgentOptions::new(AgentAccess::ReadWrite)
                },
            },
            &mut session,
            &mut workers,
        );
        assert_eq!(workers.pending_agents[&2], "jsc?!#2: waiting for opus!#1");

        super::set_note_hold(&session, &mut workers, true);
        assert_eq!(
            workers.pending_agents[&2],
            "jsc?!#2: paused while you write a # note"
        );
        // The running request is not paused.
        assert_eq!(workers.pending_agents[&1], "opus!#1: …");

        // #1 finishing while the note is being written must not launch #2.
        format_update(
            AgentUpdate {
                elapsed: Duration::ZERO,
                id: 1,
                model: "opus".into(),
                access: AgentAccess::ReadWrite,
                result: Ok("first change completed".into()),
            },
            &mut session,
            &mut workers,
        );
        assert_eq!(workers.running_writer, None);
        assert!(workers.queued_agents.contains_key(&2));
        assert_eq!(
            workers.pending_agents[&2],
            "jsc?!#2: paused while you write a # note"
        );

        session.events.push(Event::Comment("steer this way".into()));
        super::set_note_hold(&session, &mut workers, false);
        assert!(workers.queued_agents.is_empty());
        assert_eq!(workers.running_writer, Some(2));
        let update = workers
            .worker_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        super::format_worker_update(update, &mut session, &mut workers);
        assert_eq!(workers.running_writer, None);
    }

    #[test]
    fn shell_references_select_replies_http_results_and_watch_revisions() {
        let mut session = Session::new().unwrap();
        let mut workers = Workers::new(false);
        session.events.push(Event::AgentRequest {
            id: 7,
            input: "inspect".into(),
            access: AgentAccess::ReadOnly,
            user: AgentUser::Current,
            model: Some("opus".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Final,
            pi_model: None,
        });
        session.events.push(Event::AgentResponse {
            id: 7,
            model: "opus".into(),
            text: "answer without a newline".into(),
            model_info: None,
            elapsed: None,
        });
        session.events.push(Event::HttpHead {
            id: 12,
            url: "https://example.com".into(),
            output: "HTTP 200".into(),
        });
        session.live_commands.push(crate::session::LiveCommand {
            id: 3,
            cwd: session.cwd.clone(),
            target: crate::session::LiveTarget::Shell {
                program: "bash",
                command: "printf result".into(),
            },
            output: "new".into(),
            error: None,
            last_run: Some("old".into()),
            last_refresh: std::time::Instant::now(),
            revisions: vec!["old".into(), "new".into()],
        });
        let references = super::resolve_shell_references(
            "cat #7 opus?#7 '#99' #12 #3.1 #3 #7",
            &mut session,
            &mut workers,
        )
        .unwrap();
        assert_eq!(references.len(), 5);
        assert_eq!(
            references["#7"],
            "\nReferenced reply opus?#7:\nanswer without a newline\n"
        );
        assert_eq!(references["#7"], references["opus?#7"]);
        assert!(references["#12"].contains("Referenced HTTP result #12:"));
        assert!(references["#3.1"].contains("old"));
        assert!(references["#3"].contains("new"));
        assert!(references["#3"].contains("watch #3.2"));
        assert!(super::resolve_shell_references("cat #7 #99", &mut session, &mut workers).is_err());
    }

    #[test]
    fn shell_references_wait_for_pending_results_and_report_failures() {
        let mut session = Session::new().unwrap();
        let mut workers = Workers::new(false);
        session.events.push(Event::AgentRequest {
            id: 7,
            input: "inspect".into(),
            access: AgentAccess::ReadOnly,
            user: AgentUser::Current,
            model: Some("opus".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Final,
            pi_model: None,
        });
        workers.pending_agents.insert(7, "opus?#7: …".into());
        workers.active_agents = 1;
        let tx = workers.worker_tx.clone();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            tx.send(super::WorkerUpdate::Agent(AgentUpdate {
                elapsed: Duration::ZERO,
                id: 7,
                model: "opus".into(),
                access: AgentAccess::ReadOnly,
                result: Ok("completed answer".into()),
            }))
            .unwrap();
        });
        let references =
            super::resolve_shell_references("cat #7", &mut session, &mut workers).unwrap();
        sender.join().unwrap();
        assert!(references["#7"].contains("completed answer"));
        assert!(workers.pending_agents.is_empty());

        session.events.push(Event::AgentRequest {
            id: 8,
            input: "inspect".into(),
            access: AgentAccess::ReadOnly,
            user: AgentUser::Current,
            model: Some("luna".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Final,
            pi_model: None,
        });
        workers.pending_agents.insert(8, "luna?#8: …".into());
        workers.active_agents = 1;
        workers
            .worker_tx
            .send(super::WorkerUpdate::Agent(AgentUpdate {
                elapsed: Duration::ZERO,
                id: 8,
                model: "luna".into(),
                access: AgentAccess::ReadOnly,
                result: Err("agent failed".into()),
            }))
            .unwrap();
        assert!(super::resolve_shell_references("cat #8", &mut session, &mut workers).is_err());

        session.live_commands.push(crate::session::LiveCommand {
            id: 9,
            cwd: session.cwd.clone(),
            target: crate::session::LiveTarget::Shell {
                program: "bash",
                command: "printf result".into(),
            },
            output: String::new(),
            error: None,
            last_run: None,
            last_refresh: std::time::Instant::now(),
            revisions: Vec::new(),
        });
        workers.active_live = 1;
        let tx = workers.live_tx.clone();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            tx.send(super::LiveUpdate {
                id: 9,
                started_at: std::time::Instant::now(),
                result: Ok("first watch result".into()),
            })
            .unwrap();
        });
        let references =
            super::resolve_shell_references("cat #9.1", &mut session, &mut workers).unwrap();
        sender.join().unwrap();
        assert!(references["#9.1"].contains("first watch result"));
    }

    #[test]
    fn streamed_result_keeps_selector_in_final_and_recall() {
        let mut session = Session::new().unwrap();
        session.events.push(Event::AgentRequest {
            id: 1,
            input: "Update the README".into(),
            access: AgentAccess::ReadWrite,
            user: AgentUser::Current,
            model: Some("opus".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Live,
            pi_model: None,
        });
        let mut workers = Workers::new(false);
        let final_line = format_update(
            AgentUpdate {
                elapsed: Duration::ZERO,
                id: 1,
                model: "opus".into(),
                access: AgentAccess::ReadWrite,
                result: Ok("Updated README".into()),
            },
            &mut session,
            &mut workers,
        );
        assert_eq!(final_line, "opus?!#1: Updated README");
        assert_eq!(
            render_session_agent(
                &session,
                AgentLabel::new("opus", AgentAccess::ReadWrite, 1),
                AgentDisplay::FullAnswer("Updated README"),
                PromptLine::WhenExpanded
            ),
            "opus?!#1: Updated README"
        );
    }

    #[test]
    fn reported_model_is_shown_only_in_down_expansion() {
        let mut session = Session::new().unwrap();
        session.events.push(Event::AgentRequest {
            id: 1,
            input: "Inspect this".into(),
            access: AgentAccess::ReadOnly,
            user: AgentUser::Current,
            model: Some("opus".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Final,
            pi_model: None,
        });
        let mut workers = Workers::new(false);
        let line = format_update(
            AgentUpdate {
                elapsed: Duration::ZERO,
                id: 1,
                model: "opus".into(),
                access: AgentAccess::ReadOnly,
                result: Ok(super::AgentReply {
                    text: "Done".into(),
                    model_info: Some(super::ModelInfo::Reported(vec!["claude-fable-5-1".into()])),
                }),
            },
            &mut session,
            &mut workers,
        );
        assert_eq!(line, "opus?#1: Done");
        assert_eq!(
            render_session_agent(
                &session,
                AgentLabel::new("opus", AgentAccess::ReadOnly, 1),
                AgentDisplay::Answer("Done"),
                PromptLine::WhenExpanded
            ),
            line
        );
        assert_eq!(
            expanded_agent_results(&mut session),
            ["opus?#1 prompt: Inspect this\nopus?#1 [used claude-fable-5-1; 0.0s]: Done"]
        );
    }

    #[test]
    fn codex_requested_model_is_labeled_as_unverified_on_expansion() {
        let mut session = Session::new().unwrap();
        session.events.push(Event::AgentRequest {
            id: 1,
            input: "Inspect this".into(),
            access: AgentAccess::ReadOnly,
            user: AgentUser::Current,
            model: Some("luna".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Final,
            pi_model: None,
        });
        let mut workers = Workers::new(false);
        let line = format_update(
            AgentUpdate {
                elapsed: Duration::from_millis(12_400),
                id: 1,
                model: "luna".into(),
                access: AgentAccess::ReadOnly,
                result: Ok(super::AgentReply {
                    text: "Looks fine".into(),
                    model_info: Some(super::ModelInfo::Requested("gpt-6-luna".into())),
                }),
            },
            &mut session,
            &mut workers,
        );
        assert_eq!(line, "luna?#1: Looks fine");
        assert_eq!(expanded_agent_results(&mut session),
            ["luna?#1 prompt: Inspect this\nluna?#1 [requested gpt-6-luna; actual model unreported; 12.4s]: Looks fine"]);
    }

    #[test]
    fn response_times_use_seconds_then_minutes() {
        assert_eq!(format_elapsed(Duration::from_millis(840)), "0.8s");
        assert_eq!(format_elapsed(Duration::from_millis(59_940)), "59.9s");
        assert_eq!(format_elapsed(Duration::from_secs(185)), "3m 05s");
    }

    #[test]
    fn reset_restarts_counters_and_discards_old_worker_updates() {
        let mut session = Session::new().unwrap();
        session.events.push(Event::Comment("old context".into()));
        session.todos.push(crate::session::Todo {
            text: "old todo".into(),
            done: false,
        });
        session.mode = PromptMode::FmReadOnly;
        session.last_status = 7;
        let mut workers = Workers::new(false);
        workers.next_agent_id = 4;
        workers.next_http_id = 3;
        let old_sender = workers.worker_tx.clone();
        let write_gate = std::sync::Arc::clone(&workers.write_gate);
        let _write_guard = write_gate.lock().unwrap();
        let mut terminal = Terminal::new();

        reset_state(&mut session, &mut workers, &mut terminal).unwrap();

        assert!(std::sync::Arc::ptr_eq(&write_gate, &workers.write_gate));
        assert!(workers.write_gate.try_lock().is_err());
        assert_eq!(workers.next_agent_id, 1);
        assert_eq!(workers.next_http_id, 1);
        assert!(session.events.is_empty());
        assert!(session.todos.is_empty());
        assert!(session.live_commands.is_empty());
        assert!(session.mode == PromptMode::Shell);
        assert!(session.detail == DisplayDetail::Compact);
        assert_eq!(session.last_status, 0);
        assert!(old_sender
            .send(super::WorkerUpdate::Agent(super::AgentUpdate {
                elapsed: Duration::ZERO,
                id: 1,
                model: "codex".into(),
                access: AgentAccess::ReadOnly,
                result: Ok("stale".into()),
            }))
            .is_err());
    }

    #[test]
    fn prompt_path_uses_fish_style_abbreviation() {
        let home = Path::new("/Users/pgwsmith");
        assert_eq!(shortened_path(home, Some(home)), "~");
        assert_eq!(
            shortened_path(Path::new("/Users/pgwsmith/Collected/lynn"), Some(home)),
            "~/C/lynn"
        );
        assert_eq!(
            shortened_path(Path::new("/Users/pgwsmith/.config/fish"), Some(home)),
            "~/.c/fish"
        );
        assert_eq!(
            shortened_path(Path::new("/tmp/banana/sausage"), Some(home)),
            "/t/b/sausage"
        );
        assert_eq!(shortened_path(Path::new("/"), Some(home)), "/");
        assert_eq!(
            shortened_path(Path::new("/Users/pgwsmith2/lynn"), Some(home)),
            "/U/p/lynn"
        );
        assert_eq!(
            prompt(PromptMode::Shell, Path::new("/tmp/example"), true, false),
            "/tmp/example> "
        );
        assert_eq!(
            prompt(
                PromptMode::FmReadOnly,
                Path::new("/tmp/example"),
                true,
                false
            ),
            "fm? /tmp/example> "
        );
        assert_eq!(
            prompt(PromptMode::Shell, Path::new("/tmp/example"), true, true),
            "\x1b[1;36m/tmp/example>\x1b[0m "
        );
    }

    #[test]
    fn every_agent_uses_the_same_reply_label() {
        for model in ["fm", "claude", "codex", "pi"] {
            assert_eq!(
                render_agent(
                    AgentLabel::new(model, AgentAccess::ReadOnly, 4),
                    AgentDisplay::Working,
                    Styling::Plain
                ),
                format!("{model}?#4: …")
            );
            assert_eq!(
                render_agent(
                    AgentLabel::new(model, AgentAccess::ReadOnly, 4),
                    AgentDisplay::Answer("done"),
                    Styling::Plain
                ),
                format!("{model}?#4: done")
            );
            assert_eq!(
                render_agent(
                    AgentLabel::new(model, AgentAccess::ReadWrite, 4),
                    AgentDisplay::Error("failed"),
                    Styling::Plain
                ),
                format!("{model}!#4: error: failed")
            );
            assert_eq!(
                render_agent(
                    AgentLabel::new(model, AgentAccess::ReadWrite, 4),
                    AgentDisplay::FullAnswer("line one\nline two"),
                    Styling::Plain
                ),
                format!("{model}!#4: line one\nline two")
            );
        }
    }

    #[test]
    fn down_expands_existing_results_without_changing_future_updates() {
        let mut session = Session::new().unwrap();
        session.events.push(Event::AgentRequest {
            id: 1,
            input: "Inspect the parser".into(),
            access: AgentAccess::ReadOnly,
            user: AgentUser::Current,
            model: Some("opus".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Final,
            pi_model: None,
        });
        session.events.push(Event::AgentResponse {
            id: 1,
            model: "opus".into(),
            text: "Looks sound".into(),
            model_info: None,
            elapsed: None,
        });
        session.detail = DisplayDetail::Expanded;
        assert_eq!(
            expanded_agent_results(&mut session),
            ["opus?#1 prompt: Inspect the parser\nopus?#1: Looks sound"]
        );
        assert!(expanded_agent_results(&mut session).is_empty());

        session.events.push(Event::AgentRequest {
            id: 2,
            input: "Check the tests".into(),
            access: AgentAccess::ReadOnly,
            user: AgentUser::Current,
            model: Some("luna".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Final,
            pi_model: None,
        });
        let mut workers = Workers::new(false);
        let output = format_update(
            AgentUpdate {
                elapsed: Duration::ZERO,
                id: 2,
                model: "luna".into(),
                access: AgentAccess::ReadOnly,
                result: Ok("Tests pass".into()),
            },
            &mut session,
            &mut workers,
        );
        assert_eq!(output, "luna?#2: Tests pass");
        assert_eq!(
            render_session_agent(
                &session,
                AgentLabel::new("luna", AgentAccess::ReadOnly, 2),
                AgentDisplay::FullAnswer("Tests pass"),
                PromptLine::WhenExpanded
            ),
            "luna?#2: Tests pass"
        );
        assert_eq!(
            expanded_agent_results(&mut session),
            ["luna?#2 prompt: Check the tests\nluna?#2 [0.0s]: Tests pass"]
        );
    }

    #[test]
    fn reasoning_result_keeps_its_suffix_when_recalled() {
        let mut session = Session::new().unwrap();
        session.events.push(Event::AgentRequest {
            id: 3,
            input: "Inspect this".into(),
            access: AgentAccess::ReadOnly,
            user: AgentUser::Current,
            model: Some("opus".into()),
            effort: ReasoningEffort::ExtraHigh,
            output: AgentOutput::Final,
            pi_model: None,
        });
        let mut workers = Workers::new(false);
        assert_eq!(
            format_update(
                AgentUpdate {
                    elapsed: Duration::ZERO,
                    id: 3,
                    model: "opus".into(),
                    access: AgentAccess::ReadOnly,
                    result: Ok("Looks sound".into()),
                },
                &mut session,
                &mut workers
            ),
            "opus??#3: Looks sound"
        );
        assert_eq!(
            render_session_agent(
                &session,
                AgentLabel::new("opus", AgentAccess::ReadOnly, 3),
                AgentDisplay::FullAnswer("Looks sound"),
                PromptLine::WhenExpanded
            ),
            "opus??#3: Looks sound"
        );
        assert_eq!(
            render_agent(
                AgentLabel {
                    effort: ReasoningEffort::Maximum,
                    ..AgentLabel::new("sol", AgentAccess::ReadWrite, 4)
                },
                AgentDisplay::Working,
                Styling::Plain
            ),
            "sol!!!#4: …"
        );
    }

    #[test]
    fn expanded_request_is_bold_when_colored() {
        assert_eq!(
            emphasize_prompt("opus?#3 prompt: inspect this", true),
            "\x1b[1mopus?#3 prompt: inspect this\x1b[0m"
        );
        assert_eq!(
            emphasize_prompt("opus?#3 prompt: inspect this", false),
            "opus?#3 prompt: inspect this"
        );
    }

    #[test]
    fn named_reply_references_include_full_text() {
        let mut session = Session::new().unwrap();
        let workers = Workers::new(false);
        session.events.push(Event::AgentRequest {
            id: 3,
            input: "inspect".into(),
            access: AgentAccess::ReadOnly,
            user: AgentUser::Current,
            model: Some("codex".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Final,
            pi_model: None,
        });
        session.events.push(Event::AgentResponse {
            id: 3,
            model: "codex".into(),
            text: "the full\nmultiline finding".into(),
            model_info: None,
            elapsed: None,
        });
        for _ in 0..25 {
            session.events.push(Event::Comment("later context".into()));
        }
        assert!(session_context(&session).contains("multiline finding"));
        assert_eq!(
            ready_references("agree? codex?#3, codex?#3", &session, &workers)
                .unwrap()
                .text,
            "\nReferenced reply codex?#3:\nthe full\nmultiline finding\n"
        );
        assert_eq!(
            ready_references("agree? #3, codex?#3", &session, &workers)
                .unwrap()
                .text,
            "\nReferenced reply codex?#3:\nthe full\nmultiline finding\n"
        );
        session.events.push(Event::AgentRequest {
            id: 4,
            input: "edit".into(),
            access: AgentAccess::ReadWrite,
            user: AgentUser::Current,
            model: Some("opus".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Final,
            pi_model: None,
        });
        session.events.push(Event::AgentResponse {
            id: 4,
            model: "opus".into(),
            text: "changed the file".into(),
            model_info: None,
            elapsed: None,
        });
        assert_eq!(
            ready_references("check opus!#4", &session, &workers)
                .unwrap()
                .text,
            "\nReferenced reply opus!#4:\nchanged the file\n"
        );
        assert_eq!(
            ready_references("check opus?#4", &session, &workers).unwrap_err(),
            "no reply found for opus?#4"
        );
        assert_eq!(
            agent_response_by_id(&session, 3, &workers).unwrap(),
            (
                AgentLabel::new("codex", AgentAccess::ReadOnly, 3),
                "the full\nmultiline finding"
            )
        );
        assert_eq!(
            agent_response(&session, "codex", 3, &workers).unwrap(),
            "the full\nmultiline finding"
        );
        assert!(agent_response(&session, "claude", 3, &workers).is_err());
    }

    #[test]
    fn bare_hash_selects_the_latest_completed_agent_reply() {
        let mut session = Session::new().unwrap();
        assert_eq!(latest_agent_response(&session), None);
        session.events.push(Event::AgentRequest {
            id: 1,
            input: "first".into(),
            access: AgentAccess::ReadOnly,
            user: AgentUser::Current,
            model: Some("sol".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Final,
            pi_model: None,
        });
        session.events.push(Event::AgentRequest {
            id: 2,
            input: "second".into(),
            access: AgentAccess::ReadWrite,
            user: AgentUser::Current,
            model: Some("opus".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Final,
            pi_model: None,
        });
        session.events.push(Event::AgentResponse {
            id: 2,
            model: "opus".into(),
            text: "second finished first".into(),
            model_info: None,
            elapsed: None,
        });
        assert_eq!(
            latest_agent_response(&session),
            Some((
                AgentLabel::new("opus", AgentAccess::ReadWrite, 2),
                "second finished first"
            ))
        );
        session.events.push(Event::AgentResponse {
            id: 1,
            model: "sol".into(),
            text: "first finished last".into(),
            model_info: None,
            elapsed: None,
        });
        assert_eq!(
            latest_agent_response(&session),
            Some((
                AgentLabel::new("sol", AgentAccess::ReadOnly, 1),
                "first finished last"
            ))
        );
    }

    #[test]
    fn pending_reply_reference_reports_pending() {
        let mut session = Session::new().unwrap();
        let mut workers = Workers::new(false);
        session.events.push(Event::AgentRequest {
            id: 3,
            input: "inspect".into(),
            access: AgentAccess::ReadOnly,
            user: AgentUser::Current,
            model: Some("codex".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Final,
            pi_model: None,
        });
        workers.pending_agents.insert(3, "codex?#3: …".into());
        assert_eq!(
            ready_references("agree? codex?#3", &session, &workers).unwrap_err(),
            "codex?#3 is still working"
        );
        assert_eq!(
            ready_references("agree? #3", &session, &workers).unwrap_err(),
            "codex?#3 is still working"
        );
        assert_eq!(
            agent_response_by_id(&session, 99, &workers).unwrap_err(),
            "no reply found for #99"
        );
        assert_eq!(
            ready_references("agree? claude#3", &session, &workers).unwrap_err(),
            "no reply found for claude#3"
        );
    }

    #[test]
    fn pending_reply_queues_followup_and_failed_reply_cancels_it() {
        let mut session = Session::new().unwrap();
        let mut workers = Workers::new(false);
        workers.next_agent_id = 3;
        workers.active_agents = 1;
        workers.pending_agents.insert(2, "sol?#2: …".into());
        session.events.push(Event::AgentRequest {
            id: 2,
            input: "inspect".into(),
            access: AgentAccess::ReadOnly,
            user: AgentUser::Current,
            model: Some("sol".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Final,
            pi_model: None,
        });

        start_agents(
            AgentTask {
                task: "summarize #2",
                models: vec![AgentModel::Luna],
                options: AgentOptions::new(AgentAccess::ReadOnly),
            },
            &mut session,
            &mut workers,
        );
        assert_eq!(
            workers.pending_agents.get(&3).unwrap(),
            "luna?#3: waiting for #2"
        );
        assert!(workers.queued_agents.contains_key(&3));
        assert_eq!(workers.active_agents, 2);

        format_update(
            AgentUpdate {
                elapsed: Duration::ZERO,
                id: 2,
                model: "sol".into(),
                access: AgentAccess::ReadOnly,
                result: Err("failed".into()),
            },
            &mut session,
            &mut workers,
        );
        assert!(!workers.queued_agents.contains_key(&3));
        let update = workers.worker_rx.try_recv().unwrap();
        let line = super::format_worker_update(update, &mut session, &mut workers).unwrap();
        assert!(line.contains("referenced reply unavailable: sol?#2 has no reply"));
        assert!(workers.pending_agents.is_empty());
        assert_eq!(workers.active_agents, 0);
    }

    #[test]
    fn queued_request_waits_for_every_referenced_reply() {
        let mut session = Session::new().unwrap();
        let mut workers = Workers::new(false);
        for id in [1, 2] {
            session.events.push(Event::AgentRequest {
                id,
                input: "inspect".into(),
                access: AgentAccess::ReadOnly,
                user: AgentUser::Current,
                model: Some("sol".into()),
                effort: ReasoningEffort::Default,
                output: AgentOutput::Final,
                pi_model: None,
            });
            workers.pending_agents.insert(id, format!("sol?#{id}: …"));
        }
        let task = "compare #1 and #2";
        assert!(matches!(
            resolve_references(task, &session, &workers).unwrap(),
            ReferenceResolution::Pending(ids) if ids == [1, 2]
        ));
        workers.pending_agents.remove(&1);
        session.events.push(Event::AgentResponse {
            id: 1,
            model: "sol".into(),
            text: "first".into(),
            model_info: None,
            elapsed: None,
        });
        assert!(matches!(
            resolve_references(task, &session, &workers).unwrap(),
            ReferenceResolution::Pending(ids) if ids == [2]
        ));
        workers.pending_agents.remove(&2);
        session.events.push(Event::AgentResponse {
            id: 2,
            model: "sol".into(),
            text: "second".into(),
            model_info: None,
            elapsed: None,
        });
        assert!(matches!(
            resolve_references(task, &session, &workers).unwrap(),
            ReferenceResolution::Ready(references)
                if references.text.contains("Referenced reply sol?#1:\nfirst")
                    && references.text.contains("Referenced reply sol?#2:\nsecond")
        ));
    }

    #[test]
    fn queued_agent_builds_context_after_reply_and_includes_it_once() {
        let mut session = Session::new().unwrap();
        let mut workers = Workers::new(false);
        workers.next_agent_id = 3;
        session.events.push(Event::AgentRequest {
            id: 2,
            input: "inspect".into(),
            access: AgentAccess::ReadOnly,
            user: AgentUser::Current,
            model: Some("sol".into()),
            effort: ReasoningEffort::Default,
            output: AgentOutput::Final,
            pi_model: None,
        });
        workers.pending_agents.insert(2, "sol?#2: …".into());
        start_agents(
            AgentTask {
                task: "summarize #2",
                models: vec![AgentModel::Luna],
                options: AgentOptions::new(AgentAccess::ReadOnly),
            },
            &mut session,
            &mut workers,
        );
        let request_cwd = session.cwd.clone();
        session.cwd = Path::new("/tmp/changed-while-waiting").into();
        session
            .events
            .push(Event::Comment("added while waiting".into()));
        session.events.push(Event::AgentResponse {
            id: 2,
            model: "sol".into(),
            text: "unique reply".into(),
            model_info: None,
            elapsed: None,
        });
        workers.pending_agents.remove(&2);
        let ReferenceResolution::Ready(references) =
            resolve_references("summarize #2", &session, &workers).unwrap()
        else {
            panic!("reference should be ready");
        };
        let context = agent_context(&session, &workers.queued_agents[&3], &references);
        assert!(context.starts_with(&format!("cwd: {}\n", request_cwd.display())));
        assert!(context.contains("comment: added while waiting"));
        assert_eq!(context.matches("unique reply").count(), 1);
        assert!(!context.contains("response (sol?#2):"));
        assert_eq!(context.matches("request (luna?#3").count(), 1);
    }

    #[test]
    fn agent_results_highlight_bold_bullets_and_code() {
        assert_eq!(
            highlight_answer("**Checks and additions**\n- I checked `src/terminal.rs`"),
            "\x1b[1;36mChecks and additions\x1b[0m\n\x1b[36m•\x1b[0m I checked \x1b[36msrc/terminal.rs\x1b[0m"
        );
    }

    #[test]
    fn read_only_answers_are_not_clipped_or_reshaped() {
        let long = "abcdefghijk".repeat(20);
        assert_eq!(
            render_agent(
                AgentLabel::new("claude", AgentAccess::ReadOnly, 1),
                AgentDisplay::Answer(&long),
                Styling::Plain
            ),
            format!("claude?#1: {long}")
        );
        assert_eq!(
            render_agent(
                AgentLabel::new("sol", AgentAccess::ReadOnly, 2),
                AgentDisplay::Answer("first\nsecond"),
                Styling::Plain
            ),
            "sol?#2: first\nsecond"
        );
    }

    #[test]
    fn live_diff_modes_show_only_requested_lines() {
        assert_eq!(
            diff_lines("## main\n?? old", "## main\n?? new", DiffMode::Added),
            "+?? new"
        );
        assert_eq!(
            diff_lines(
                "## main\n?? old",
                "## main\n?? new",
                DiffMode::AddedAndRemoved
            ),
            "-?? old\n+?? new"
        );
        assert_eq!(diff_lines("old", "", DiffMode::Added), "");
        assert_eq!(diff_lines("old", "", DiffMode::AddedAndRemoved), "-old");
    }

    #[test]
    fn completed_head_result_is_available_to_agent_references() {
        let mut session = Session::new().unwrap();
        session.events.push(Event::HttpHead {
            id: 2,
            url: "https://example.com/feed.atom".into(),
            output: "HTTP 200\ncontent-type: application/atom+xml".into(),
        });
        let workers = Workers::new(false);
        let references = ready_references("Summarize #2", &session, &workers).unwrap();
        assert!(references.text.contains("Referenced HTTP result #2:"));
        assert!(references.text.contains("HTTP 200"));
        assert!(super::http_response_by_id(&session, 2)
            .unwrap()
            .contains("feed.atom"));
        let context =
            super::session_context_filtered(&session, &references.ids, None, &session.cwd);
        assert!(!context.contains("HEAD https://example.com/feed.atom"));
    }

    #[test]
    fn agents_and_http_results_share_numbers() {
        let mut workers = Workers::new(false);
        assert_eq!(workers.next_result_id(), 1);
        assert_eq!(workers.next_result_id(), 2);
        assert_eq!(workers.next_agent_id, 3);
        assert_eq!(workers.next_http_id, 3);
    }

    #[test]
    fn worker_update_wakes_the_terminal() {
        use std::io::Read;

        let mut workers = Workers::new(true);
        workers
            .worker_tx
            .send(super::WorkerUpdate::Agent(AgentUpdate {
                elapsed: Duration::ZERO,
                id: 1,
                model: "sol".into(),
                access: AgentAccess::ReadOnly,
                result: Err("test".into()),
            }))
            .unwrap();
        let mut byte = [0];
        workers.wake_reader.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [1]);
        assert!(workers.worker_rx.try_recv().is_ok());
    }

    #[test]
    fn watch_results_keep_numbered_revisions_and_support_references() {
        let mut session = Session::new().unwrap();
        session.live_commands.push(crate::session::LiveCommand {
            id: 1,
            cwd: session.cwd.clone(),
            target: crate::session::LiveTarget::Shell {
                program: "bash",
                command: "printf result".into(),
            },
            output: String::new(),
            error: None,
            last_run: None,
            last_refresh: std::time::Instant::now(),
            revisions: Vec::new(),
        });
        let workers = Workers::new(false);
        assert!(matches!(
            resolve_references("summarize #1", &session, &workers).unwrap(),
            ReferenceResolution::Pending(ids) if ids == [1]
        ));

        let watch = &mut session.live_commands[0];
        assert!(
            super::update_live_command(watch, Ok("first".into()), std::time::Instant::now())
                .unwrap()
                .contains("#1.1")
        );
        assert!(
            super::update_live_command(watch, Ok("first".into()), std::time::Instant::now())
                .is_none()
        );
        assert_eq!(watch.revisions.len(), 1);
        assert!(
            super::update_live_command(watch, Ok("second".into()), std::time::Instant::now())
                .unwrap()
                .contains("#1.2")
        );
        assert_eq!(watch.revisions.len(), 2);
        super::update_live_command(
            watch,
            Err("temporary failure".into()),
            std::time::Instant::now(),
        );
        assert_eq!(watch.revisions.len(), 2);

        assert!(super::watch_response_by_id(&session, 1, None)
            .unwrap()
            .contains("second"));
        assert!(super::watch_response_by_id(&session, 1, Some(1))
            .unwrap()
            .contains("first"));
        let ReferenceResolution::Ready(references) =
            resolve_references("compare #1.1 and #1.2 with #1.", &session, &workers).unwrap()
        else {
            panic!("watch results should be ready")
        };
        assert_eq!(references.text.matches("Referenced watch #1.1").count(), 1);
        assert_eq!(references.text.matches("Referenced watch #1.2").count(), 1);
        assert_eq!(references.ids.into_iter().collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn watch_context_excerpt_respects_utf8_and_reports_omission() {
        let output = format!("start\n{}\nend", "é".repeat(100));
        let excerpt = super::watch_context_excerpt(&output, 100);
        assert!(excerpt.starts_with("start\n"));
        assert!(excerpt.ends_with("\nend"));
        assert!(excerpt.contains(&format!("{} bytes total", output.len())));
        assert_eq!(super::watch_context_excerpt(&output, output.len()), output);
    }
}
