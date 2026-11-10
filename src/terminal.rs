use crate::session::DisplayDetail;
use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthChar;

#[derive(Default)]
struct EditorDisplay {
    cursor_row: usize,
}

struct RawMode {
    original: libc::termios,
}

impl RawMode {
    fn new() -> io::Result<Self> {
        let fd = io::stdin().as_raw_fd();
        let mut original = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = original;
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mode = Self { original };
        let mut stdout = io::stdout().lock();
        stdout.write_all(b"\x1b[?2004h")?;
        stdout.flush()?;
        Ok(mode)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let mut stdout = io::stdout().lock();
        let _ = stdout.write_all(b"\x1b[?2004l");
        let _ = stdout.flush();
        unsafe {
            libc::tcsetattr(io::stdin().as_raw_fd(), libc::TCSANOW, &self.original);
        }
    }
}

pub struct Terminal {
    history: Vec<String>,
    kill_ring: Vec<u8>,
    references: HashMap<usize, ReferenceSource>,
    code_spans: Vec<String>,
    /// The line most recently entered, while nothing has been printed below it,
    /// so a number assigned to it can be shown on that line.
    entered: Option<EnteredLine>,
}

struct EnteredLine {
    /// The prompt and styled input as drawn.
    styled: String,
    /// How many rows up the line starts from the cursor.
    rows: usize,
}

/// What a numbered session entry came from, so `#n` can share its selector's colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceSource {
    /// A read-only agent request such as `opus? ...`.
    ReadOnlyAgent,
    /// A read-write agent request such as `opus! ...` or `opus?! ...`.
    ReadWriteAgent,
    /// A reader-account watch or fetch such as `+ bash? ...`, `+ HEAD ...` or `HEAD ...`.
    Reader,
    /// A `# note`.
    Note,
}

/// What the editor shows before the input and what Tab can complete from it.
#[derive(Clone, Copy)]
pub struct PromptView<'a> {
    /// The prompt shown in compact mode.
    pub prompt: &'a str,
    /// The prompt shown in expanded mode.
    pub full_prompt: &'a str,
    pub detail: DisplayDetail,
    pub color: bool,
    pub todos: &'a [crate::session::Todo],
}

impl<'a> PromptView<'a> {
    /// The prompt text for the current display detail.
    fn text(&self) -> &'a str {
        match self.detail {
            DisplayDetail::Compact => self.prompt,
            DisplayDetail::Expanded => self.full_prompt,
        }
    }
}

pub enum ReadResult {
    Line(String),
    Interrupt,
    Eof,
    Expand,
}

pub fn columns() -> usize {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::ioctl(io::stdout().as_raw_fd(), libc::TIOCGWINSZ, &mut size) };
    if result == 0 && size.ws_col > 0 {
        size.ws_col as usize
    } else {
        80
    }
}

pub fn clear_screen() -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(b"\x1b[2J\x1b[H")?;
    stdout.flush()
}

pub fn set_title(title: &str) -> io::Result<()> {
    if std::env::var("TERM").ok().as_deref() == Some("dumb") {
        return Ok(());
    }
    let mut stdout = io::stdout().lock();
    write_title(&mut stdout, title)?;
    stdout.flush()
}

fn write_title(output: &mut impl Write, title: &str) -> io::Result<()> {
    let title: String = title
        .chars()
        .map(|character| {
            if character.is_control() {
                '?'
            } else {
                character
            }
        })
        .collect();
    write!(output, "\x1b]2;{title}\x1b\\")
}

impl Terminal {
    pub fn new() -> Self {
        Self {
            history: Vec::new(),
            kill_ring: Vec::new(),
            references: HashMap::new(),
            code_spans: Vec::new(),
            entered: None,
        }
    }

    /// Forget the entered line because output now follows it.
    pub fn forget_entered(&mut self) {
        self.entered = None;
    }

    /// Redraw the line just entered with `label` after it, such as a watch's
    /// `#79`. Returns false, drawing nothing, when that line is no longer the
    /// last thing on screen.
    pub fn label_entered(&mut self, label: &str, color: bool) -> io::Result<bool> {
        let Some(entered) = self.entered.take() else {
            return Ok(false);
        };
        if unsafe { libc::isatty(1) } != 1 {
            return Ok(false);
        }
        let label = if color {
            format!("  {}{label}\x1b[0m", SelectorKind::ReaderShell.ansi())
        } else {
            format!("  {label}")
        };
        let mut stdout = io::stdout().lock();
        stdout.write_all(b"\r")?;
        if entered.rows > 0 {
            write!(stdout, "\x1b[{}A", entered.rows)?;
        }
        stdout.write_all(b"\r\x1b[0J")?;
        write_entered_line(&mut stdout, &format!("{}{label}", entered.styled))?;
        stdout.flush()?;
        Ok(true)
    }

    /// Set which source each `#n` refers to; unknown numbers keep the plain reference colour.
    pub fn set_references(&mut self, references: HashMap<usize, ReferenceSource>) {
        self.references = references;
    }

    /// Set the `code` spans from recent answers, most recent first, that Tab
    /// offers after an opening backtick.
    pub fn set_code_spans(&mut self, code_spans: Vec<String>) {
        self.code_spans = code_spans;
    }

    pub fn read_line(
        &mut self,
        view: PromptView,
        mut statuses: Vec<String>,
        wake_fd: RawFd,
        mut notices: impl FnMut(&[u8]) -> (Vec<String>, Vec<String>),
    ) -> io::Result<ReadResult> {
        self.entered = None;
        let prompt = view.text();
        let color = view.color;
        let _raw = RawMode::new()?;
        let mut bytes = Vec::<u8>::new();
        let mut cursor = 0;
        let mut history_pos = self.history.len();
        let mut saved = Vec::new();
        let mut last_kill = None;
        let mut display = EditorDisplay::default();
        let mut status_rows = write_statuses(io::stdout().lock(), &statuses)?;
        redraw(
            &mut display,
            &bytes,
            cursor,
            &self.history,
            &self.references,
            prompt,
            color,
        )?;
        loop {
            let key = match read_byte_or_notice(wake_fd)? {
                Some(key) => key,
                None => {
                    let (updates, new_statuses) = notices(&bytes);
                    if show_notices(
                        &updates,
                        new_statuses,
                        &mut statuses,
                        &mut status_rows,
                        &mut display,
                    )? {
                        redraw(
                            &mut display,
                            &bytes,
                            cursor,
                            &self.history,
                            &self.references,
                            prompt,
                            color,
                        )?;
                    }
                    continue;
                }
            };
            let previous_kill = last_kill.take();
            match key {
                b'\r' | b'\n' => {
                    let mut styled = prompt.as_bytes().to_vec();
                    write_input_style(
                        &mut styled,
                        &format_line(&bytes),
                        color,
                        InputStyle::Entered,
                        &self.references,
                    )?;
                    let styled = String::from_utf8_lossy(&styled).into_owned();
                    let mut stdout = io::stdout().lock();
                    clear_editor(&mut stdout, status_rows, &mut display)?;
                    let rows = write_entered_line(&mut stdout, &styled)?;
                    stdout.flush()?;
                    self.entered = Some(EnteredLine { styled, rows });
                    let line = String::from_utf8_lossy(&bytes).into_owned();
                    if !line.trim().is_empty() && self.history.last() != Some(&line) {
                        self.history.push(line.clone());
                    }
                    return Ok(ReadResult::Line(line));
                }
                3 => {
                    let mut stdout = io::stdout().lock();
                    clear_editor(&mut stdout, status_rows, &mut display)?;
                    write!(stdout, "{}", prompt)?;
                    write_input_style(
                        &mut stdout,
                        &format_line(&bytes),
                        color,
                        InputStyle::Entered,
                        &self.references,
                    )?;
                    stdout.write_all(b"^C\r\n")?;
                    stdout.flush()?;
                    return Ok(ReadResult::Interrupt);
                }
                4 if bytes.is_empty() => {
                    let mut stdout = io::stdout().lock();
                    clear_editor(&mut stdout, status_rows, &mut display)?;
                    stdout.write_all(b"\r\n")?;
                    stdout.flush()?;
                    return Ok(ReadResult::Eof);
                }
                4 => {
                    if cursor < bytes.len() {
                        let end = next_char(&bytes, cursor);
                        bytes.drain(cursor..end);
                    }
                }
                127 | 8 => {
                    if cursor > 0 {
                        let start = prev_char(&bytes, cursor);
                        bytes.drain(start..cursor);
                        cursor = start;
                    }
                }
                1 => cursor = 0,
                2 => cursor = prev_char(&bytes, cursor),
                6 => cursor = next_char(&bytes, cursor),
                5 => cursor = bytes.len(),
                11 => {
                    let end = bytes.len();
                    kill(
                        &mut bytes,
                        cursor,
                        end,
                        &mut self.kill_ring,
                        previous_kill,
                        KillDirection::Forward,
                    );
                    last_kill = Some(KillDirection::Forward);
                }
                21 => {
                    kill(
                        &mut bytes,
                        0,
                        cursor,
                        &mut self.kill_ring,
                        previous_kill,
                        KillDirection::Backward,
                    );
                    cursor = 0;
                    last_kill = Some(KillDirection::Backward);
                }
                23 => {
                    let start = backward_word(&bytes, cursor);
                    kill(
                        &mut bytes,
                        start,
                        cursor,
                        &mut self.kill_ring,
                        previous_kill,
                        KillDirection::Backward,
                    );
                    cursor = start;
                    last_kill = Some(KillDirection::Backward);
                }
                25 => {
                    bytes.splice(cursor..cursor, self.kill_ring.iter().copied());
                    cursor += self.kill_ring.len();
                }
                16 if history_pos > 0 => {
                    if history_pos == self.history.len() {
                        saved = bytes.clone();
                    }
                    history_pos -= 1;
                    bytes = self.history[history_pos].as_bytes().to_vec();
                    cursor = bytes.len();
                }
                14 if history_pos < self.history.len() => {
                    history_pos += 1;
                    bytes = if history_pos == self.history.len() {
                        saved.clone()
                    } else {
                        self.history[history_pos].as_bytes().to_vec()
                    };
                    cursor = bytes.len();
                }
                9 => {
                    if let Ok(line) = std::str::from_utf8(&bytes) {
                        let completion =
                            completions(line, cursor, &self.history, view.todos, &self.code_spans);
                        if !completion.matches.is_empty() {
                            let replacement = if completion.matches.len() == 1 {
                                completion.matches[0].clone()
                            } else {
                                common_prefix(&completion.matches)
                            };
                            if replacement.len() > line[completion.start..cursor].len()
                                || !replacement.starts_with(&line[completion.start..cursor])
                            {
                                let replacement = if completion.end < bytes.len()
                                    && bytes[completion.end].is_ascii_whitespace()
                                {
                                    replacement.trim_end_matches(' ').as_bytes().to_vec()
                                } else {
                                    replacement.into_bytes()
                                };
                                bytes.splice(
                                    completion.start..completion.end,
                                    replacement.iter().copied(),
                                );
                                cursor = completion.start + replacement.len();
                            } else if completion.matches.len() > 1 {
                                let mut stdout = io::stdout().lock();
                                clear_editor(&mut stdout, status_rows, &mut display)?;
                                write!(stdout, "{}\r\n", completion.matches.join("  "))?;
                                status_rows = write_statuses(&mut stdout, &statuses)?;
                                stdout.flush()?;
                            }
                        }
                    }
                }
                27 => {
                    match read_byte_timeout(50)? {
                        Some(b'b') => cursor = backward_word(&bytes, cursor),
                        Some(b'f') => cursor = forward_word(&bytes, cursor),
                        Some(b'd') => {
                            let end = forward_word(&bytes, cursor);
                            kill(
                                &mut bytes,
                                cursor,
                                end,
                                &mut self.kill_ring,
                                previous_kill,
                                KillDirection::Forward,
                            );
                            last_kill = Some(KillDirection::Forward);
                        }
                        Some(127 | 8) => {
                            let start = backward_word(&bytes, cursor);
                            kill(
                                &mut bytes,
                                start,
                                cursor,
                                &mut self.kill_ring,
                                previous_kill,
                                KillDirection::Backward,
                            );
                            cursor = start;
                            last_kill = Some(KillDirection::Backward);
                        }
                        Some(b'[') => match read_byte_timeout(50)? {
                            Some(b'A') if history_pos > 0 => {
                                if history_pos == self.history.len() {
                                    saved = bytes.clone();
                                }
                                history_pos -= 1;
                                bytes = self.history[history_pos].as_bytes().to_vec();
                                cursor = bytes.len();
                            }
                            Some(b'B') if history_pos < self.history.len() => {
                                history_pos += 1;
                                bytes = if history_pos == self.history.len() {
                                    saved.clone()
                                } else {
                                    self.history[history_pos].as_bytes().to_vec()
                                };
                                cursor = bytes.len();
                            }
                            Some(b'B') if bytes.is_empty() => {
                                let mut stdout = io::stdout().lock();
                                clear_editor(&mut stdout, status_rows, &mut display)?;
                                stdout.flush()?;
                                return Ok(ReadResult::Expand);
                            }
                            Some(b'C') if cursor == bytes.len() => {
                                let line = format_line(&bytes);
                                if let Some(entry) = self.history.iter().rev().find(|entry| {
                                    entry.starts_with(&line) && entry.len() > line.len()
                                }) {
                                    bytes = entry.as_bytes().to_vec();
                                    cursor = bytes.len();
                                }
                            }
                            Some(b'C') => cursor = next_char(&bytes, cursor),
                            Some(b'D') => cursor = prev_char(&bytes, cursor),
                            Some(b'3')
                                if read_byte_timeout(50)? == Some(b'~') && cursor < bytes.len() =>
                            {
                                let end = next_char(&bytes, cursor);
                                bytes.drain(cursor..end);
                            }
                            Some(b'2')
                                if read_byte_timeout(50)? == Some(b'0')
                                    && read_byte_timeout(50)? == Some(b'0')
                                    && read_byte_timeout(50)? == Some(b'~') =>
                            {
                                let paste = read_bracketed_paste()?;
                                let length = paste.len();
                                bytes.splice(cursor..cursor, paste);
                                cursor += length;
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
                12 => {
                    clear_screen()?;
                    display = EditorDisplay::default();
                    status_rows = write_statuses(io::stdout().lock(), &statuses)?;
                }
                b' ' if bytes == b"?" && cursor == 1 => {
                    // `? text` has no meaning on its own, so a space after a
                    // bare `?` recalls the latest agent selector like Tab.
                    if let Some(completion) = history_completion("?", 1, &self.history) {
                        let replacement = completion.matches[0].as_bytes();
                        bytes.splice(
                            completion.start..completion.end,
                            replacement.iter().copied(),
                        );
                        cursor = completion.start + replacement.len();
                    } else {
                        bytes.insert(cursor, b' ');
                        cursor += 1;
                    }
                }
                c if c >= 32 => {
                    bytes.insert(cursor, c);
                    cursor += 1;
                }
                _ => {}
            }
            // The caller sees each edit, so it can react to the line as it is
            // typed (for example, holding queued agents while a `#` note is written).
            let (updates, new_statuses) = notices(&bytes);
            show_notices(
                &updates,
                new_statuses,
                &mut statuses,
                &mut status_rows,
                &mut display,
            )?;
            redraw(
                &mut display,
                &bytes,
                cursor,
                &self.history,
                &self.references,
                prompt,
                color,
            )?;
        }
    }
}

/// Prints background updates and replaces the status lines above the editor.
/// Returns whether anything changed, in which case the editor must be redrawn.
fn show_notices(
    updates: &[String],
    new_statuses: Vec<String>,
    statuses: &mut Vec<String>,
    status_rows: &mut usize,
    display: &mut EditorDisplay,
) -> io::Result<bool> {
    if updates.is_empty() && new_statuses == *statuses {
        return Ok(false);
    }
    let mut stdout = io::stdout().lock();
    clear_editor(&mut stdout, *status_rows, display)?;
    write_notices(&mut stdout, updates)?;
    *statuses = new_statuses;
    *status_rows = write_statuses(&mut stdout, statuses)?;
    stdout.flush()?;
    Ok(true)
}

fn clear_editor(
    mut output: impl Write,
    status_count: usize,
    display: &mut EditorDisplay,
) -> io::Result<()> {
    output.write_all(b"\r")?;
    let rows_up = status_count + display.cursor_row;
    if rows_up > 0 {
        write!(output, "\x1b[{rows_up}A")?;
    }
    display.cursor_row = 0;
    output.write_all(b"\r\x1b[0J")
}

fn write_statuses(output: impl Write, statuses: &[String]) -> io::Result<usize> {
    write_statuses_with_width(output, statuses, columns().saturating_sub(1).max(2))
}

fn write_statuses_with_width(
    mut output: impl Write,
    statuses: &[String],
    width: usize,
) -> io::Result<usize> {
    let mut rows = 0;
    for status in statuses {
        rows += write_wrapped(&mut output, &format!("{status}\n"), width)?.row;
    }
    Ok(rows)
}

fn write_notices(mut output: impl Write, updates: &[String]) -> io::Result<()> {
    for update in updates {
        for line in update.split('\n') {
            output.write_all(line.strip_suffix('\r').unwrap_or(line).as_bytes())?;
            output.write_all(b"\r\n")?;
        }
    }
    Ok(())
}

fn read_byte() -> io::Result<Option<u8>> {
    let mut byte = 0u8;
    loop {
        let count = unsafe { libc::read(0, &mut byte as *mut u8 as *mut libc::c_void, 1) };
        if count == 1 {
            return Ok(Some(byte));
        }
        if count == 0 {
            return Ok(None);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn read_byte_timeout(timeout_ms: i32) -> io::Result<Option<u8>> {
    let mut fd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut fd, 1, timeout_ms) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(None);
        }
        return Err(error);
    }
    if ready == 0 {
        return Ok(None);
    }
    if fd.revents & libc::POLLHUP != 0 {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "terminal input closed",
        ));
    }
    read_byte()
}

fn read_byte_or_notice(wake_fd: RawFd) -> io::Result<Option<u8>> {
    let mut fds = [
        libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake_fd,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::Interrupted {
            Ok(None)
        } else {
            Err(error)
        };
    }
    if fds[0].revents & libc::POLLHUP != 0 {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "terminal input closed",
        ));
    }
    if fds[0].revents & libc::POLLIN != 0 {
        return read_byte();
    }
    if fds[1].revents & libc::POLLIN != 0 {
        let mut buffer = [0u8; 256];
        loop {
            let count = unsafe {
                libc::read(
                    wake_fd,
                    buffer.as_mut_ptr() as *mut libc::c_void,
                    buffer.len(),
                )
            };
            if count <= 0 {
                let error = io::Error::last_os_error();
                if count == 0 || error.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
        return Ok(None);
    }
    Err(io::Error::other("terminal or wake socket is unavailable"))
}

fn read_bracketed_paste() -> io::Result<Vec<u8>> {
    const END: &[u8] = b"\x1b[201~";
    let mut bytes = Vec::new();
    loop {
        let byte = read_byte()?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "bracketed paste was not closed",
            )
        })?;
        bytes.push(byte);
        if bytes.ends_with(END) {
            bytes.truncate(bytes.len() - END.len());
            return Ok(normalize_paste(&bytes));
        }
    }
}

fn normalize_paste(bytes: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut after_cr = false;
    for &byte in bytes {
        match byte {
            b'\r' => {
                normalized.push(b'\n');
                after_cr = true;
            }
            b'\n' if after_cr => after_cr = false,
            b'\n' => normalized.push(b'\n'),
            b'\t' => {
                normalized.extend_from_slice(b"    ");
                after_cr = false;
            }
            32..=126 | 128..=255 => {
                normalized.push(byte);
                after_cr = false;
            }
            _ => after_cr = false,
        }
    }
    normalized
}

fn prev_char(bytes: &[u8], mut pos: usize) -> usize {
    pos = pos.saturating_sub(1);
    while pos > 0 && bytes[pos] & 0xc0 == 0x80 {
        pos -= 1;
    }
    pos
}
fn next_char(bytes: &[u8], mut pos: usize) -> usize {
    if pos < bytes.len() {
        pos += 1;
    }
    while pos < bytes.len() && bytes[pos] & 0xc0 == 0x80 {
        pos += 1;
    }
    pos
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KillDirection {
    Backward,
    Forward,
}

fn kill(
    bytes: &mut Vec<u8>,
    start: usize,
    end: usize,
    kill_ring: &mut Vec<u8>,
    previous: Option<KillDirection>,
    direction: KillDirection,
) {
    if start == end {
        return;
    }
    let removed: Vec<u8> = bytes.drain(start..end).collect();
    if previous == Some(direction) {
        if direction == KillDirection::Backward {
            kill_ring.splice(0..0, removed);
        } else {
            kill_ring.extend(removed);
        }
    } else {
        *kill_ring = removed;
    }
}

fn whitespace_at(bytes: &[u8], start: usize, end: usize) -> bool {
    std::str::from_utf8(&bytes[start..end])
        .ok()
        .and_then(|text| text.chars().next())
        .is_some_and(char::is_whitespace)
}

fn backward_word(bytes: &[u8], mut pos: usize) -> usize {
    while pos > 0 {
        let start = prev_char(bytes, pos);
        if !whitespace_at(bytes, start, pos) {
            break;
        }
        pos = start;
    }
    while pos > 0 {
        let start = prev_char(bytes, pos);
        if whitespace_at(bytes, start, pos) {
            break;
        }
        pos = start;
    }
    pos
}

fn forward_word(bytes: &[u8], mut pos: usize) -> usize {
    while pos < bytes.len() {
        let end = next_char(bytes, pos);
        if !whitespace_at(bytes, pos, end) {
            break;
        }
        pos = end;
    }
    while pos < bytes.len() {
        let end = next_char(bytes, pos);
        if whitespace_at(bytes, pos, end) {
            break;
        }
        pos = end;
    }
    pos
}

fn format_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[derive(Default)]
struct ScreenPosition {
    row: usize,
    column: usize,
}

fn write_wrapped(mut output: impl Write, styled: &str, width: usize) -> io::Result<ScreenPosition> {
    let mut position = ScreenPosition::default();
    let mut chars = styled.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\x1b' && chars.peek() == Some(&'[') {
            output.write_all(b"\x1b[")?;
            chars.next();
            for next in chars.by_ref() {
                write!(output, "{next}")?;
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
            continue;
        }
        if character == '\n' {
            output.write_all(b"\r\n")?;
            position.row += 1;
            position.column = 0;
            continue;
        }
        let cells = UnicodeWidthChar::width(character).unwrap_or(0);
        if cells > 0 && position.column + cells > width {
            output.write_all(b"\r\n")?;
            position.row += 1;
            position.column = 0;
        }
        write!(output, "{character}")?;
        position.column += cells;
    }
    if position.column == width {
        output.write_all(b"\r\n")?;
        position.row += 1;
        position.column = 0;
    }
    Ok(position)
}

/// Writes an entered line, wrapped like the editor, and moves to the next row.
/// Returns how many rows the cursor ends below the line's first row.
fn write_entered_line(output: impl Write, styled: &str) -> io::Result<usize> {
    write_entered_line_with_width(output, styled, columns().saturating_sub(1).max(2))
}

fn write_entered_line_with_width(
    mut output: impl Write,
    styled: &str,
    width: usize,
) -> io::Result<usize> {
    let end = write_wrapped(&mut output, styled, width)?;
    if end.column > 0 || end.row == 0 {
        output.write_all(b"\r\n")?;
        Ok(end.row + 1)
    } else {
        Ok(end.row)
    }
}

fn redraw(
    display: &mut EditorDisplay,
    bytes: &[u8],
    cursor: usize,
    history: &[String],
    references: &HashMap<usize, ReferenceSource>,
    prompt: &str,
    color: bool,
) -> io::Result<()> {
    let line = format_line(bytes);
    let suggestion = if cursor == bytes.len() && !line.is_empty() {
        history
            .iter()
            .rev()
            .find(|entry| entry.starts_with(&line) && entry.len() > line.len())
            .map(|entry| &entry[line.len()..])
            .unwrap_or("")
    } else {
        ""
    };
    let width = columns().saturating_sub(1).max(2);
    let mut styled = prompt.as_bytes().to_vec();
    write_input_style(&mut styled, &line, color, InputStyle::Entered, references)?;
    if !suggestion.is_empty() {
        write!(styled, "\x1b[2m{suggestion}\x1b[0m")?;
    }
    let styled = String::from_utf8(styled)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let prefix = format!("{prompt}{}", format_line(&bytes[..cursor]));
    let target = write_wrapped(io::sink(), &prefix, width)?;
    let mut stdout = io::stdout().lock();
    clear_editor(&mut stdout, 0, display)?;
    let end = write_wrapped(&mut stdout, &styled, width)?;
    if end.row > target.row {
        write!(stdout, "\x1b[{}A", end.row - target.row)?;
    } else if target.row > end.row {
        write!(stdout, "\x1b[{}B", target.row - end.row)?;
    }
    write!(stdout, "\r\x1b[{}G", target.column + 1)?;
    display.cursor_row = target.row;
    stdout.flush()
}

#[derive(Clone, Copy)]
enum InputStyle {
    Example,
    Entered,
}

fn write_input(output: impl Write, line: &str, color: bool) -> io::Result<()> {
    write_input_style(output, line, color, InputStyle::Example, &HashMap::new())
}

#[cfg(test)]
fn write_entered_input(output: impl Write, line: &str, color: bool) -> io::Result<()> {
    write_input_style(output, line, color, InputStyle::Entered, &HashMap::new())
}

fn write_input_style(
    mut output: impl Write,
    line: &str,
    color: bool,
    style: InputStyle,
    references: &HashMap<usize, ReferenceSource>,
) -> io::Result<()> {
    if !color {
        return output.write_all(line.as_bytes());
    }
    if matches!(style, InputStyle::Entered) {
        output.write_all(b"\x1b[1m")?;
    }
    write_input_parts(&mut output, line, style, references)?;
    if matches!(style, InputStyle::Entered) {
        output.write_all(b"\x1b[0m")?;
    }
    Ok(())
}

fn write_input_parts(
    mut output: impl Write,
    line: &str,
    style: InputStyle,
    references: &HashMap<usize, ReferenceSource>,
) -> io::Result<()> {
    if let Some((start, end, kind)) = selector_highlight(line) {
        write!(
            output,
            "{}{}{}\x1b[0m",
            &line[..start],
            kind.ansi(),
            &line[start..end]
        )?;
        if matches!(style, InputStyle::Entered) {
            output.write_all(b"\x1b[1m")?;
        }
        if &line[start..end] == "@staffer" {
            // Only colour what follows when the line is an accepted staffer
            // agent request, so `@staffer yes!` does not look like a reply.
            if matches!(
                crate::session::classify(&line[start..]),
                crate::session::Input::Agents(_)
            ) {
                return write_input_parts(output, &line[end..], style, references);
            }
        }
        if kind.is_agent() {
            // Colour `#3`, `opus#3` and `#3.2` so a reference typed by accident shows up.
            write_references(&mut output, &line[end..], style, references)?;
        } else {
            output.write_all(&line.as_bytes()[end..])?;
        }
    } else if matches!(style, InputStyle::Entered) {
        if let Some((start, end, known)) = command_span(line) {
            // Known commands are blue and unknown ones red; arguments stay plain bold.
            let ansi = if known { COMMAND_ANSI } else { "\x1b[1;31m" };
            write!(
                output,
                "{}{ansi}{}\x1b[0m\x1b[1m{}",
                &line[..start],
                &line[start..end],
                &line[end..]
            )?;
        } else {
            output.write_all(line.as_bytes())?;
        }
    } else {
        output.write_all(line.as_bytes())?;
    }
    Ok(())
}

const REFERENCE_ANSI: &str = "\x1b[1;36m";

/// The colour for a reference: its source's selector colour, or cyan when unknown.
fn reference_ansi(token: &str, references: &HashMap<usize, ReferenceSource>) -> &'static str {
    let id = crate::session::watch_revision_reference(token)
        .map(|(id, _)| id)
        .or_else(|| crate::session::agent_reference(token).map(|(_, _, id)| id))
        .or_else(|| crate::session::agent_number_reference(token));
    match id.and_then(|id| references.get(&id)) {
        Some(ReferenceSource::ReadOnlyAgent) => SelectorKind::ReadOnlyAgent.ansi(),
        Some(ReferenceSource::ReadWriteAgent) => SelectorKind::ReadWriteAgent.ansi(),
        Some(ReferenceSource::Reader) => SelectorKind::ReaderShell.ansi(),
        Some(ReferenceSource::Note) => NOTE_REFERENCE_ANSI,
        None => REFERENCE_ANSI,
    }
}

/// Notes run nothing, so references to them are grey rather than a selector colour.
const NOTE_REFERENCE_ANSI: &str = "\x1b[1;90m";

fn write_references(
    mut output: impl Write,
    text: &str,
    style: InputStyle,
    references: &HashMap<usize, ReferenceSource>,
) -> io::Result<()> {
    let mut written = 0;
    for (start, token) in crate::session::reference_tokens(text) {
        if !crate::session::is_reference(token) {
            continue;
        }
        let end = start + token.len();
        let ansi = reference_ansi(token, references);
        write!(output, "{}{ansi}{token}\x1b[0m", &text[written..start])?;
        if matches!(style, InputStyle::Entered) {
            output.write_all(b"\x1b[1m")?;
        }
        written = end;
    }
    output.write_all(&text.as_bytes()[written..])
}

const COMMAND_ANSI: &str = "\x1b[1;34m";

/// The first command word's byte span and whether it names a runnable command.
fn command_span(line: &str) -> Option<(usize, usize, bool)> {
    let cwd = std::env::current_dir().ok()?;
    let path = std::env::var_os("PATH")?;
    command_span_in(line, &cwd, &path)
}

#[cfg(test)]
fn unknown_command_span_in(line: &str, cwd: &Path, path: &OsStr) -> Option<(usize, usize)> {
    command_span_in(line, cwd, path)
        .filter(|&(_, _, known)| !known)
        .map(|(start, end, _)| (start, end))
}

fn command_span_in(line: &str, cwd: &Path, path: &OsStr) -> Option<(usize, usize, bool)> {
    let start = line.len() - line.trim_start().len();
    let rest = &line[start..];
    let end = rest
        .char_indices()
        .find(|&(index, character)| {
            character.is_whitespace()
                || matches!(character, '|' | '<' | '>')
                || rest[index..].starts_with("&&")
        })
        .map(|(index, _)| index)
        .unwrap_or(rest.len());
    let word = &rest[..end];
    if word.is_empty()
        || word.starts_with(['#', '+', '-', '?', '!', '@', '$', '\'', '"'])
        || word.starts_with("https://")
        || word.starts_with("http://")
        || word
            .split_once('=')
            .is_some_and(|(name, _)| valid_assignment_name(name))
    {
        return None;
    }
    let known = matches!(word, "cd" | "exit" | "export" | "unset" | "reset")
        || command_exists_in(word, cwd, path);
    Some((start, start + end, known))
}

fn valid_assignment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn command_exists_in(command: &str, cwd: &Path, path: &OsStr) -> bool {
    let is_executable = |candidate: &Path| {
        std::fs::metadata(candidate)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    };
    if command.contains('/') {
        return is_executable(&cwd.join(command));
    }
    std::env::split_paths(path).any(|directory| is_executable(&cwd.join(directory).join(command)))
}

pub fn styled_input(line: &str, color: bool) -> String {
    let mut output = Vec::with_capacity(line.len() + 16);
    write_input(&mut output, line, color).expect("writing to a Vec cannot fail");
    String::from_utf8(output).expect("ANSI styling preserves UTF-8")
}

#[derive(Debug, PartialEq, Eq)]
enum SelectorKind {
    /// `opus?`, `opus??`, emoji selectors: green like the read-only `bash?`.
    ReadOnlyAgent,
    /// `opus!`, `opus!!`, `opus?!`: magenta.
    ReadWriteAgent,
    /// `yes` and `yes!` replies, which may inherit either access; magenta.
    Reply,
    ReaderShell,
    HostShell,
}

impl SelectorKind {
    fn ansi(&self) -> &'static str {
        match self {
            Self::ReadWriteAgent | Self::Reply => "\x1b[1;35m",
            Self::ReadOnlyAgent | Self::ReaderShell => "\x1b[1;32m",
            Self::HostShell => "\x1b[1;33m",
        }
    }

    fn is_agent(&self) -> bool {
        matches!(
            self,
            Self::ReadOnlyAgent | Self::ReadWriteAgent | Self::Reply
        )
    }
}

fn selector_highlight(line: &str) -> Option<(usize, usize, SelectorKind)> {
    let mut start = line.len() - line.trim_start().len();
    let mut live = false;
    if let Some(after_plus) = line[start..].strip_prefix('+') {
        let trimmed = after_plus.trim_start();
        start += 1 + after_plus.len() - trimmed.len();
        live = true;
    }
    let rest = &line[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let selector = &rest[..end];
    let kind = if matches!(selector, "bash?" | "zsh?" | "@staffer" | "HEAD" | "FEED") {
        SelectorKind::ReaderShell
    } else if !live && matches!(selector, "bash!" | "zsh!") {
        SelectorKind::HostShell
    } else if let Some(kind) = (!live).then(|| agent_selector_kind(selector)).flatten() {
        kind
    } else if !live
        && matches!(selector, "yes" | "yes!")
        && matches!(
            crate::session::classify(rest),
            crate::session::Input::Yes { .. }
        )
    {
        // `yes`, `yes!` and `yes! #3` reply to an agent rather than run the `yes` command.
        // `yes!` asks for write access; plain `yes` keeps the answer's own access.
        if selector == "yes!" {
            SelectorKind::ReadWriteAgent
        } else {
            SelectorKind::Reply
        }
    } else {
        return None;
    };
    Some((start, start + end, kind))
}

/// Whether `selector` names an agent, and if so whether it is read-only or read-write.
fn agent_selector_kind(selector: &str) -> Option<SelectorKind> {
    let models = |names: &str| names.split('/').all(crate::session::is_agent_model);
    if crate::session::AgentModel::from_emoji(selector).is_some() {
        return Some(SelectorKind::ReadOnlyAgent);
    }
    for (suffix, kind) in [
        ("?!", SelectorKind::ReadWriteAgent),
        ("!!!", SelectorKind::ReadWriteAgent),
        ("!!", SelectorKind::ReadWriteAgent),
        ("???", SelectorKind::ReadOnlyAgent),
        ("??", SelectorKind::ReadOnlyAgent),
        ("?", SelectorKind::ReadOnlyAgent),
    ] {
        if selector.strip_suffix(suffix).is_some_and(models) {
            return Some(kind);
        }
    }
    if selector
        .strip_suffix('!')
        .is_some_and(crate::session::is_agent_model)
    {
        return Some(SelectorKind::ReadWriteAgent);
    }
    match crate::session::pi_model_selector(selector) {
        Some((_, "?")) => Some(SelectorKind::ReadOnlyAgent),
        Some((_, "!")) => Some(SelectorKind::ReadWriteAgent),
        _ => None,
    }
}

struct Completion {
    start: usize,
    end: usize,
    matches: Vec<String>,
}

fn write_access_request(line: &str) -> Option<String> {
    let request = match crate::session::classify(line) {
        crate::session::Input::Agents(request)
            if request.options.access == crate::session::AgentAccess::ReadOnly
                && request.models.len() == 1 =>
        {
            request
        }
        _ => return None,
    };
    let start = if request.options.user == crate::agent_cli::AgentUser::Staffer {
        let prefix = line.len() - line.trim_start().len();
        let rest = &line[prefix + "@staffer".len()..];
        prefix + "@staffer".len() + rest.len() - rest.trim_start().len()
    } else {
        line.len() - line.trim_start().len()
    };
    let end = start
        + line[start..]
            .find(char::is_whitespace)
            .unwrap_or(line.len() - start);
    let selector = &line[start..end];
    if !selector.ends_with('?') {
        return None;
    }
    Some(format!(
        "{}{}{}",
        &line[..start],
        selector.replace('?', "!"),
        &line[end..]
    ))
}

fn history_completion(line: &str, cursor: usize, history: &[String]) -> Option<Completion> {
    if cursor != 1 {
        return None;
    }
    let replacement = match line {
        "!" => write_access_request(history.last()?)?,
        "?" => history.iter().rev().find_map(|entry| {
            let models = match crate::session::classify(entry) {
                crate::session::Input::Agents(request) if !request.models.is_empty() => {
                    request.models
                }
                crate::session::Input::InteractiveAgent { model, .. } => vec![model],
                crate::session::Input::EnterFm => vec![crate::session::AgentModel::Fm],
                _ => return None,
            };
            Some(format!(
                "{}? ",
                models
                    .iter()
                    .map(|model| model.name())
                    .collect::<Vec<_>>()
                    .join("/")
            ))
        })?,
        _ => return None,
    };
    Some(Completion {
        start: 0,
        end: 1,
        matches: vec![replacement],
    })
}

fn todo_completion(
    line: &str,
    cursor: usize,
    todos: &[crate::session::Todo],
) -> Option<Completion> {
    if cursor != line.len() {
        return None;
    }
    let marker = if line.starts_with("- [x]") || line.starts_with("- [X]") {
        5
    } else {
        return None;
    };
    let typed = &line[marker..];
    if !typed.is_empty() && !typed.starts_with(' ') {
        return None;
    }
    let matches: Vec<_> = todos
        .iter()
        .filter(|todo| !todo.done)
        .map(|todo| format!(" {}", todo.text))
        .filter(|text| text.starts_with(typed))
        .collect();
    (!matches.is_empty()).then_some(Completion {
        start: marker,
        end: cursor,
        matches,
    })
}

/// After an unclosed backtick, completes `code` spans from recent answers and
/// closes the span. Returns `None` outside a span or when nothing matches, so
/// Tab falls back to path completion.
fn code_span_completion(line: &str, cursor: usize, code_spans: &[String]) -> Option<Completion> {
    let before = &line[..cursor];
    if before.matches('`').count().is_multiple_of(2) {
        return None;
    }
    let start = before.rfind('`')? + 1;
    let typed = &before[start..];
    let end = if line[cursor..].starts_with('`') {
        cursor + 1
    } else {
        cursor
    };
    let mut matches: Vec<String> = Vec::new();
    for span in code_spans {
        let candidate = format!("{span}`");
        if span.starts_with(typed) && !matches.contains(&candidate) {
            matches.push(candidate);
        }
    }
    (!matches.is_empty()).then_some(Completion {
        start,
        end,
        matches,
    })
}

/// The single-backtick `code` spans in `text`, in order, skipping fenced blocks.
pub fn code_spans_in(text: &str) -> Vec<String> {
    let mut spans = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let parts: Vec<&str> = line.split('`').collect();
        // Odd parts are inside backticks; the last part is only inside if
        // the line closes it.
        for part in parts.iter().skip(1).step_by(2).take((parts.len() - 1) / 2) {
            let part = part.trim();
            if !part.is_empty() && part.len() <= 200 {
                spans.push(part.to_owned());
            }
        }
    }
    spans
}

fn completions(
    line: &str,
    cursor: usize,
    history: &[String],
    todos: &[crate::session::Todo],
    code_spans: &[String],
) -> Completion {
    if let Some(completion) = todo_completion(line, cursor, todos) {
        return completion;
    }
    if let Some(completion) = code_span_completion(line, cursor, code_spans) {
        return completion;
    }
    if let Some(completion) = history_completion(line, cursor, history) {
        return completion;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    completions_in(line, cursor, &cwd)
}

fn completions_in(line: &str, cursor: usize, cwd: &Path) -> Completion {
    let (start, value, style, active_quote, command) = completion_token(&line[..cursor]);
    let end = completion_end(line, cursor, active_quote);
    let mut names = BTreeSet::new();
    let staffer_agent = line[..start].trim() == "@staffer";
    if (command || staffer_agent) && !value.contains('/') && !value.starts_with('~') {
        let built_ins: &[&str] = if staffer_agent {
            &[
                "claude?", "claude!", "opus?", "opus!", "sonnet?", "sonnet!", "codex?", "codex!",
                "Opus?", "Opus!", "sol?", "sol!", "luna?", "luna!", "terra?", "terra!", "pi?",
                "pi!", "agy?", "agy!", "flash?", "flash!",
            ]
        } else {
            &[
                "cd", "exit", "export", "unset", "reset", "bash!", "zsh!", "bash?", "zsh?",
                "@staffer", "fm?", "fm!", "claude?", "claude!", "opus?", "opus!", "sonnet?",
                "Opus?", "Opus!", "sonnet!", "codex?", "codex!", "sol?", "sol!", "luna?", "luna!",
                "terra?", "terra!", "pi?", "pi!", "agy?", "agy!", "flash?", "flash!", "🎵", "🎶",
                "☀️", "🌙", "🥧", "🍎",
            ]
        };
        for built_in in built_ins {
            if built_in.starts_with(&value) {
                names.insert(render_completion(built_in, style, false));
            }
        }
        if value == "pi" || value.starts_with("pi.") {
            // Listed on each Tab so new Pi models need no new Lynn release.
            for selector in pi_model_selectors(&crate::pi::models()) {
                if selector.starts_with(&value) {
                    names.insert(render_completion(&selector, style, false));
                }
            }
        }
        for model in [
            "claude", "fable", "opus", "Opus", "sonnet", "haiku", "codex", "sol", "luna", "terra",
            "agy", "flash",
        ] {
            for suffix in ["??", "???", "!!", "!!!"] {
                let selector = format!("{model}{suffix}");
                if selector.starts_with(&value) {
                    names.insert(render_completion(&selector, style, false));
                }
            }
        }
        if let Some(paths) = command.then(|| std::env::var_os("PATH")).flatten() {
            for dir in std::env::split_paths(&paths) {
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if name.starts_with(&value) && entry.path().is_file() {
                            names.insert(render_completion(&name, style, false));
                        }
                    }
                }
            }
        }
    }
    if value == "~" {
        names.insert(render_completion("~", style, true));
    } else {
        let (directory, stem) = value.rsplit_once('/').unwrap_or(("", value.as_str()));
        let directory = if directory.is_empty() && value.starts_with('/') {
            "/"
        } else {
            directory
        };
        let lookup = if directory == "~" {
            std::env::var_os("HOME").map(PathBuf::from)
        } else if let Some(rest) = directory.strip_prefix("~/") {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest))
        } else {
            Some(cwd.join(if directory.is_empty() { "." } else { directory }))
        };
        if let Some(lookup) = lookup {
            if let Ok(entries) = std::fs::read_dir(lookup) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if !name.starts_with(stem) || (name.starts_with('.') && !stem.starts_with('.'))
                    {
                        continue;
                    }
                    let candidate = if directory.is_empty() {
                        name
                    } else {
                        if directory == "/" {
                            format!("/{name}")
                        } else {
                            format!("{directory}/{name}")
                        }
                    };
                    let is_dir = entry.path().is_dir();
                    names.insert(render_completion(&candidate, style, is_dir));
                }
            }
        }
    }
    Completion {
        start,
        end,
        matches: names.into_iter().collect(),
    }
}

/// `pi.<id>?` and `pi.<id>!` for each Pi model, with the provider only where IDs clash.
fn pi_model_selectors(models: &[String]) -> Vec<String> {
    let id = |model: &str| model.split_once('/').map_or(model, |(_, id)| id).to_owned();
    let mut selectors = Vec::new();
    for model in models {
        let name = if models.iter().filter(|other| id(other) == id(model)).count() == 1 {
            id(model)
        } else {
            model.clone()
        };
        selectors.extend(["?", "!"].map(|suffix| format!("pi.{name}{suffix}")));
    }
    selectors
}

fn completion_token(prefix: &str) -> (usize, String, Option<char>, Option<char>, bool) {
    let mut start = 0;
    let mut value = String::new();
    let mut quote = None;
    let mut style = None;
    let mut escaped = false;
    let mut in_word = false;
    let mut command = true;
    for (index, character) in prefix.char_indices() {
        if escaped {
            value.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            if !in_word {
                start = index;
                in_word = true;
            }
            escaped = true;
        } else if let Some(active) = quote {
            if character == active {
                quote = None;
            } else {
                value.push(character);
            }
        } else if character == '\'' || character == '"' {
            if !in_word {
                start = index;
                style = Some(character);
                in_word = true;
            }
            quote = Some(character);
        } else if character.is_whitespace() || matches!(character, '|' | '<' | '>') {
            if in_word {
                command = false;
            }
            if character == '|' {
                command = true;
            } else if matches!(character, '<' | '>') {
                command = false;
            }
            start = index + character.len_utf8();
            value.clear();
            style = None;
            in_word = false;
        } else {
            if !in_word {
                start = index;
                in_word = true;
            }
            value.push(character);
        }
    }
    (start, value, style, quote, command)
}

fn completion_end(line: &str, cursor: usize, mut quote: Option<char>) -> usize {
    let mut escaped = false;
    for (offset, character) in line[cursor..].char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' && quote != Some('\'') {
            escaped = true;
        } else if quote == Some(character) {
            quote = None;
        } else if quote.is_none() {
            if character == '\'' || character == '"' {
                quote = Some(character);
            } else if character.is_whitespace() || matches!(character, '|' | '<' | '>') {
                return cursor + offset;
            }
        }
    }
    line.len()
}

fn render_completion(value: &str, quote: Option<char>, directory: bool) -> String {
    let mut value = value.to_owned();
    if directory {
        value.push('/');
    }
    match quote {
        Some('\'') => format!(
            "'{}{}",
            value.replace('\'', "'\\''"),
            if directory { "" } else { "' " }
        ),
        Some('"') => {
            let escaped = value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('$', "\\$");
            format!("\"{escaped}{}", if directory { "" } else { "\" " })
        }
        _ => {
            let mut escaped = String::new();
            for character in value.chars() {
                if character.is_whitespace()
                    || matches!(character, '\'' | '"' | '\\' | '$' | '|' | '<' | '>')
                {
                    escaped.push('\\');
                }
                escaped.push(character);
            }
            if !directory {
                escaped.push(' ');
            }
            escaped
        }
    }
}

fn common_prefix(strings: &[String]) -> String {
    let Some(first) = strings.first() else {
        return String::new();
    };
    let mut prefix = first.clone();
    for item in &strings[1..] {
        while !item.starts_with(&prefix) {
            prefix.pop();
        }
    }
    prefix
}

#[cfg(test)]
mod code_span_tests {
    use super::{code_span_completion, code_spans_in};

    #[test]
    fn finds_inline_code_spans_but_not_fenced_blocks() {
        let text = "A new `agent_selector_kind` function in `src/terminal.rs`.\n\
                    ```\nlet `ignored` = 1;\n```\nAn `unclosed span";
        assert_eq!(
            code_spans_in(text),
            ["agent_selector_kind", "src/terminal.rs"]
        );
    }

    #[test]
    fn tab_after_a_backtick_completes_and_closes_code_spans() {
        let spans = [
            "agent_selector_kind".to_owned(),
            "agent_label_by_id".to_owned(),
            "src/main.rs".to_owned(),
            "agent_selector_kind".to_owned(),
        ];
        let line = "opus? what does `agent_sel";
        let completion = code_span_completion(line, line.len(), &spans).unwrap();
        assert_eq!(completion.start, line.len() - "agent_sel".len());
        assert_eq!(completion.end, line.len());
        assert_eq!(completion.matches, ["agent_selector_kind`"]);

        let line = "opus? `agent";
        let completion = code_span_completion(line, line.len(), &spans).unwrap();
        assert_eq!(
            completion.matches,
            ["agent_selector_kind`", "agent_label_by_id`"]
        );

        // An existing closing backtick is replaced rather than doubled.
        let line = "opus? `src`";
        let completion = code_span_completion(line, line.len() - 1, &spans).unwrap();
        assert_eq!(completion.end, line.len());
        assert_eq!(completion.matches, ["src/main.rs`"]);

        // Outside a span, or with no match, Tab falls back to other completion.
        let line = "opus? `src/main.rs` agent";
        assert!(code_span_completion(line, line.len(), &spans).is_none());
        let line = "opus? `zzz";
        assert!(code_span_completion(line, line.len(), &spans).is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        backward_word, clear_editor, completions_in, forward_word, history_completion, kill,
        normalize_paste, pi_model_selectors, selector_highlight, styled_input,
        unknown_command_span_in, write_entered_input, write_entered_line_with_width, write_input,
        write_input_style, write_notices, write_statuses_with_width, write_title, write_wrapped,
        EditorDisplay, InputStyle, KillDirection, PromptView, ReferenceSource, SelectorKind,
    };
    use crate::session::DisplayDetail;
    use std::collections::HashMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn entered_line_reports_the_rows_to_move_up_to_relabel_it() {
        let mut output = Vec::new();
        assert_eq!(
            write_entered_line_with_width(&mut output, "> + bash? git status", 10).unwrap(),
            2
        );
        assert_eq!(output, b"> + bash? \r\ngit status\r\n");
        let mut output = Vec::new();
        assert_eq!(
            write_entered_line_with_width(&mut output, "> ls", 10).unwrap(),
            1
        );
        assert_eq!(output, b"> ls\r\n");
    }

    #[test]
    fn pi_selectors_add_the_provider_only_for_clashing_ids() {
        let models = ["kapitsa/qwen3.5-9b", "a/shared", "b/shared"].map(String::from);
        assert_eq!(
            pi_model_selectors(&models),
            [
                "pi.qwen3.5-9b?",
                "pi.qwen3.5-9b!",
                "pi.a/shared?",
                "pi.a/shared!",
                "pi.b/shared?",
                "pi.b/shared!",
            ]
        );
    }

    #[test]
    fn terminal_title_uses_short_path_and_filters_control_sequences() {
        let mut output = Vec::new();
        write_title(&mut output, "~/C/project\x1b]0;injected\x07").unwrap();
        assert_eq!(output, b"\x1b]2;~/C/project?]0;injected?\x1b\\");
    }

    #[test]
    fn expanded_prompt_has_no_deadline() {
        let view = |detail| PromptView {
            prompt: "short> ",
            full_prompt: "full/path> ",
            detail,
            color: false,
            todos: &[],
        };
        assert_eq!(view(DisplayDetail::Compact).text(), "short> ");
        assert_eq!(view(DisplayDetail::Expanded).text(), "full/path> ");
    }

    #[test]
    fn pasted_lines_keep_blank_lines_and_do_not_carry_terminal_controls() {
        assert_eq!(
            normalize_paste(b"opus? first\r\n\r\n\tsecond\nthird\x1b[31m"),
            b"opus? first\n\n    second\nthird[31m"
        );
    }

    #[test]
    fn recognized_selectors_have_distinct_colors() {
        assert_eq!(
            selector_highlight("opus/luna? compare"),
            Some((0, 10, SelectorKind::ReadOnlyAgent))
        );
        assert_eq!(
            selector_highlight("  fm! change this"),
            Some((2, 5, SelectorKind::ReadWriteAgent))
        );
        assert_eq!(
            selector_highlight("codex! add LICENSE"),
            Some((0, 6, SelectorKind::ReadWriteAgent))
        );
        assert_eq!(
            selector_highlight("pi.qwen3.5-9b? hello"),
            Some((0, 14, SelectorKind::ReadOnlyAgent))
        );
        assert_eq!(selector_highlight("pi.qwen3.5-9b?? hello"), None);
        assert_eq!(
            selector_highlight("@staffer opus? LICENSE?"),
            Some((0, 8, SelectorKind::ReaderShell))
        );
        assert_eq!(
            selector_highlight("bash? git status"),
            Some((0, 5, SelectorKind::ReaderShell))
        );
        assert_eq!(
            selector_highlight("  +bash? git status"),
            Some((3, 8, SelectorKind::ReaderShell))
        );
        assert_eq!(
            selector_highlight("+ zsh? pwd"),
            Some((2, 6, SelectorKind::ReaderShell))
        );
        assert_eq!(
            selector_highlight("zsh! rm file"),
            Some((0, 4, SelectorKind::HostShell))
        );
        assert_eq!(selector_highlight("unknown? inspect"), None);
        assert_eq!(
            selector_highlight("Opus? inspect"),
            Some((0, 5, SelectorKind::ReadOnlyAgent))
        );
        assert_eq!(
            selector_highlight("opus?? inspect"),
            Some((0, 6, SelectorKind::ReadOnlyAgent))
        );
        assert_eq!(
            selector_highlight("sol!!! inspect"),
            Some((0, 6, SelectorKind::ReadWriteAgent))
        );
        assert_eq!(
            selector_highlight("opus/luna?? compare"),
            Some((0, 11, SelectorKind::ReadOnlyAgent))
        );
        assert_eq!(
            selector_highlight("opus! change"),
            Some((0, 5, SelectorKind::ReadWriteAgent))
        );
        assert_eq!(
            selector_highlight("opus?! change"),
            Some((0, 6, SelectorKind::ReadWriteAgent))
        );
        assert_eq!(
            selector_highlight("opus!! change"),
            Some((0, 6, SelectorKind::ReadWriteAgent))
        );
        assert_eq!(selector_highlight("echo opus/luna?"), None);
        assert_eq!(selector_highlight("+bash! echo hi"), None);
        for emoji in ["🎵", "🎶", "🥧", "🍎"] {
            assert_eq!(
                selector_highlight(&format!("{emoji} inspect this")),
                Some((0, emoji.len(), SelectorKind::ReadOnlyAgent))
            );
        }
        assert_eq!(
            selector_highlight("☀️ inspect this"),
            Some((0, "☀️".len(), SelectorKind::ReadOnlyAgent))
        );
        assert_eq!(
            selector_highlight("🌙 inspect this"),
            Some((0, "🌙".len(), SelectorKind::ReadOnlyAgent))
        );

        let mut colored = Vec::new();
        write_input(&mut colored, "opus/luna? compare", true).unwrap();
        assert_eq!(colored, b"\x1b[1;32mopus/luna?\x1b[0m compare");
        colored.clear();
        write_input(&mut colored, "+ bash? git status", true).unwrap();
        assert_eq!(colored, b"+ \x1b[1;32mbash?\x1b[0m git status");
        colored.clear();
        write_input(&mut colored, "+ HEAD https://example.com", true).unwrap();
        assert_eq!(colored, b"+ \x1b[1;32mHEAD\x1b[0m https://example.com");
        colored.clear();
        write_input(&mut colored, "bash! rm file", true).unwrap();
        assert_eq!(colored, b"\x1b[1;33mbash!\x1b[0m rm file");
        colored.clear();
        write_input(&mut colored, "@staffer opus? inspect", true).unwrap();
        assert_eq!(
            colored,
            b"\x1b[1;32m@staffer\x1b[0m \x1b[1;32mopus?\x1b[0m inspect"
        );
        colored.clear();
        write_input(&mut colored, "opus! change", true).unwrap();
        assert_eq!(colored, b"\x1b[1;35mopus!\x1b[0m change");
        let mut plain = Vec::new();
        write_input(&mut plain, "bash! rm file", false).unwrap();
        assert_eq!(plain, b"bash! rm file");
    }

    #[test]
    fn help_examples_use_prompt_selector_colors() {
        assert_eq!(
            styled_input("  opus? task          Ask Opus", true),
            "  \x1b[1;32mopus?\x1b[0m task          Ask Opus"
        );
        assert_eq!(
            styled_input("  + bash? command     Watch as staffer", true),
            "  + \x1b[1;32mbash?\x1b[0m command     Watch as staffer"
        );
        assert_eq!(
            styled_input("  bash! command       Run Bash", true),
            "  \x1b[1;33mbash!\x1b[0m command       Run Bash"
        );
        assert_eq!(styled_input("  opus? task", false), "  opus? task");
    }

    #[test]
    fn entered_commands_and_agent_tasks_are_bold() {
        let mut output = Vec::new();
        write_entered_input(&mut output, "ls", true).unwrap();
        assert_eq!(output, b"\x1b[1m\x1b[1;34mls\x1b[0m\x1b[1m\x1b[0m");
        output.clear();
        // Only the command name is blue; its arguments stay plain bold.
        write_entered_input(&mut output, "  ls -la src", true).unwrap();
        assert_eq!(
            output,
            b"\x1b[1m  \x1b[1;34mls\x1b[0m\x1b[1m -la src\x1b[0m"
        );
        output.clear();
        write_entered_input(&mut output, "cd src", true).unwrap();
        assert_eq!(output, b"\x1b[1m\x1b[1;34mcd\x1b[0m\x1b[1m src\x1b[0m");
        output.clear();
        // Notes have no command name to colour.
        write_entered_input(&mut output, "# note", true).unwrap();
        assert_eq!(output, b"\x1b[1m# note\x1b[0m");
        output.clear();
        write_entered_input(&mut output, "opus? inspect this", true).unwrap();
        assert_eq!(
            output,
            b"\x1b[1m\x1b[1;32mopus?\x1b[0m\x1b[1m inspect this\x1b[0m"
        );
        output.clear();
        write_entered_input(&mut output, "@staffer opus? inspect", true).unwrap();
        assert_eq!(
            output,
            b"\x1b[1m\x1b[1;32m@staffer\x1b[0m\x1b[1m \x1b[1;32mopus?\x1b[0m\x1b[1m inspect\x1b[0m"
        );
        output.clear();
        write_entered_input(&mut output, "ls", false).unwrap();
        assert_eq!(output, b"ls");
        output.clear();
        write_entered_input(&mut output, "yes!", true).unwrap();
        assert_eq!(output, b"\x1b[1m\x1b[1;35myes!\x1b[0m\x1b[1m\x1b[0m");
        output.clear();
        write_entered_input(&mut output, "yes! #3", true).unwrap();
        assert_eq!(
            output,
            b"\x1b[1m\x1b[1;35myes!\x1b[0m\x1b[1m \x1b[1;36m#3\x1b[0m\x1b[1m\x1b[0m"
        );
        output.clear();
        write_input(&mut output, "yes", true).unwrap();
        assert_eq!(output, b"\x1b[1;35myes\x1b[0m");
        output.clear();
        // `yes hello` runs the shell's `yes`, so it is not an agent reply.
        write_input(&mut output, "yes hello", true).unwrap();
        assert_eq!(output, b"yes hello");
        output.clear();
        // `@staffer yes!` is rejected, so only `@staffer` is coloured.
        write_input(&mut output, "@staffer yes!", true).unwrap();
        assert_eq!(output, b"\x1b[1;32m@staffer\x1b[0m yes!");
    }

    #[test]
    fn agent_prompts_colour_references() {
        assert_eq!(
            styled_input("opus? compare #3 with opus?#12, #4.2 and #1.", true),
            "\x1b[1;32mopus?\x1b[0m compare \x1b[1;36m#3\x1b[0m with \
             \x1b[1;36mopus?#12\x1b[0m, \x1b[1;36m#4.2\x1b[0m and \x1b[1;36m#1\x1b[0m."
        );
        // Escaped, hex-colour, zero-led and URL-fragment forms are not references.
        assert_eq!(
            styled_input(r"opus? not #\1, #123456, #01 or https://x.dev/#2", true),
            "\x1b[1;32mopus?\x1b[0m not #\\1, #123456, #01 or https://x.dev/#2"
        );
        // Shell commands keep their own reference handling and are not coloured.
        assert_eq!(styled_input("cat #3", true), "cat #3");
    }

    #[test]
    fn references_take_their_source_colour() {
        let references = HashMap::from([
            (3, ReferenceSource::ReadOnlyAgent),
            (6, ReferenceSource::ReadWriteAgent),
            (4, ReferenceSource::Reader),
            (7, ReferenceSource::Note),
        ]);
        let mut output = Vec::new();
        write_input_style(
            &mut output,
            "opus? compare #3, opus?#3, #6, #4 and #4.2 with #5 per #7",
            true,
            InputStyle::Example,
            &references,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\x1b[1;32mopus?\x1b[0m compare \x1b[1;32m#3\x1b[0m, \x1b[1;32mopus?#3\x1b[0m, \
             \x1b[1;35m#6\x1b[0m, \x1b[1;32m#4\x1b[0m and \x1b[1;32m#4.2\x1b[0m with \
             \x1b[1;36m#5\x1b[0m per \x1b[1;90m#7\x1b[0m"
        );
    }

    #[test]
    fn unknown_first_command_is_red_without_coloring_its_arguments() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let unknown = format!("lynn_missing_command_{unique}");
        let mut output = Vec::new();
        write_entered_input(&mut output, &format!("{unknown} hello.txt"), true).unwrap();
        assert_eq!(
            output,
            format!("\x1b[1m\x1b[1;31m{unknown}\x1b[0m\x1b[1m hello.txt\x1b[0m").as_bytes()
        );
    }

    #[test]
    fn command_lookup_checks_builtins_path_and_executable_files() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("lynn-command-color-{unique}"));
        fs::create_dir_all(&root).unwrap();
        let cat = root.join("cat");
        fs::write(&cat, "").unwrap();
        fs::set_permissions(&cat, fs::Permissions::from_mode(0o755)).unwrap();
        let dog = root.join("dog");
        fs::write(&dog, "").unwrap();
        fs::set_permissions(&dog, fs::Permissions::from_mode(0o644)).unwrap();

        let lookup = |line| unknown_command_span_in(line, &root, root.as_os_str());
        assert_eq!(lookup("cat hello.txt"), None);
        assert_eq!(lookup("cat&&echo hello"), None);
        assert_eq!(lookup("dog&&echo hello"), Some((0, 3)));
        assert_eq!(lookup("  dog hello.txt"), Some((2, 5)));
        assert_eq!(lookup("./cat hello.txt"), None);
        assert_eq!(lookup("./dog hello.txt"), Some((0, 5)));
        assert_eq!(lookup("cd elsewhere"), None);
        assert_eq!(lookup("NAME=value"), None);
        assert_eq!(lookup("opus task"), Some((0, 4)));
        assert_eq!(lookup("# a note"), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bang_tab_recalls_the_last_read_only_request_with_write_access() {
        for (previous, expected) in [
            ("opus? explain this", "opus! explain this"),
            (
                "  sol??  inspect café\nthen explain  ",
                "  sol!!  inspect café\nthen explain  ",
            ),
            ("luna??? inspect this", "luna!!! inspect this"),
            ("@staffer opus? inspect", "@staffer opus! inspect"),
        ] {
            let history = vec![previous.to_owned()];
            let completion = history_completion("!", 1, &history).unwrap();
            assert_eq!((completion.start, completion.end), (0, 1));
            assert_eq!(completion.matches, [expected]);
        }
    }

    #[test]
    fn bang_tab_requires_a_bare_bang_and_an_eligible_last_command() {
        assert!(history_completion("!", 1, &[]).is_none());
        let history = vec!["opus? inspect this".to_owned()];
        for (line, cursor) in [("", 0), ("!", 0), ("! ", 2), ("!change", 1), ("echo !", 6)] {
            assert!(history_completion(line, cursor, &history).is_none());
        }
        for previous in [
            "ls",
            "echo opus? inspect",
            "opus! change",
            "opus?! stream",
            "opus?",
            "opus/luna? compare",
            "bash? pwd",
        ] {
            let history = vec!["opus? older request".to_owned(), previous.to_owned()];
            assert!(history_completion("!", 1, &history).is_none());
        }
    }

    #[test]
    fn question_tab_recalls_the_latest_agent_without_its_instruction() {
        for (previous, expected) in [
            ("opus? explain this", "opus? "),
            ("sol!!! implement this", "sol? "),
            ("Opus?", "opus? "),
            ("@staffer luna! implement this", "luna? "),
            ("opus/luna?? compare", "opus/luna? "),
            ("🎵 explain this", "opus? "),
            ("fm?", "fm? "),
        ] {
            let history = vec![
                "codex? older request".to_owned(),
                previous.to_owned(),
                "ls".to_owned(),
            ];
            let completion = history_completion("?", 1, &history).unwrap();
            assert_eq!((completion.start, completion.end), (0, 1));
            assert_eq!(completion.matches, [expected]);
        }
    }

    #[test]
    fn question_tab_requires_a_bare_question_and_agent_history() {
        assert!(history_completion("?", 1, &[]).is_none());
        let history = vec![
            "ls".to_owned(),
            "bash? pwd".to_owned(),
            "# opus? explain".to_owned(),
        ];
        assert!(history_completion("?", 1, &history).is_none());
        let history = vec!["opus? explain this".to_owned()];
        for (line, cursor) in [("?", 0), ("? ", 2), ("?task", 1), ("echo ?", 6)] {
            assert!(history_completion(line, cursor, &history).is_none());
        }
    }

    #[test]
    fn tab_completes_argument_paths_and_escapes_spaces() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("lynn-completion-{}-{unique}", std::process::id()));
        fs::create_dir_all(root.join("My Docs")).unwrap();
        fs::write(root.join("report.txt"), "").unwrap();
        fs::write(root.join("My Docs").join("a b.txt"), "").unwrap();
        fs::write(root.join(".hidden"), "").unwrap();

        let matches = |line: &str| completions_in(line, line.len(), &root).matches;
        assert_eq!(matches("cat rep"), ["report.txt "]);
        assert_eq!(matches("cd My\\ D"), ["My\\ Docs/"]);
        assert_eq!(matches("cat My\\ Docs/a"), ["My\\ Docs/a\\ b.txt "]);
        assert_eq!(matches("cat 'My D"), ["'My Docs/"]);
        assert_eq!(matches("cat .h"), [".hidden "]);
        assert!(matches("@staffer opu").contains(&"opus? ".to_owned()));
        assert!(matches("@staffer opu").contains(&"opus! ".to_owned()));
        assert!(matches("opus??").contains(&"opus??? ".to_owned()));
        assert!(matches("sol!!").contains(&"sol!!! ".to_owned()));
        assert!(!matches("cat ").iter().any(|name| name.contains(".hidden")));
        let absolute = format!("cat {}/rep", root.display());
        assert_eq!(
            matches(&absolute),
            [format!("{}/report.txt ", root.display())]
        );
        let middle = completions_in("cat rep other", "cat rep".len(), &root);
        assert_eq!(middle.end, "cat rep".len());
        assert_eq!(middle.matches, ["report.txt "]);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn background_notices_use_terminal_line_endings() {
        let mut output = Vec::new();
        write_notices(
            &mut output,
            &[
                "[head #1] example\nHTTP 200\ncontent-type: text/html\nTime to headers: 5 ms"
                    .into(),
            ],
        )
        .unwrap();
        assert_eq!(
            output,
            b"[head #1] example\r\nHTTP 200\r\ncontent-type: text/html\r\nTime to headers: 5 ms\r\n"
        );
    }

    #[test]
    fn active_status_is_cleared_before_echoing_a_command() {
        let mut output = Vec::new();
        let mut display = EditorDisplay { cursor_row: 2 };
        clear_editor(&mut output, 1, &mut display).unwrap();
        assert_eq!(output, b"\r\x1b[3A\r\x1b[0J");
        assert_eq!(display.cursor_row, 0);
    }

    #[test]
    fn expanded_statuses_count_wrapped_and_multiline_rows_for_clearing() {
        let mut output = Vec::new();
        let rows = write_statuses_with_width(
            &mut output,
            &["\x1b[1m12345678901\nnext\x1b[0m".into(), "job: …".into()],
            10,
        )
        .unwrap();
        assert_eq!(
            output,
            b"\x1b[1m1234567890\r\n1\r\nnext\x1b[0m\r\njob: \xe2\x80\xa6\r\n"
        );
        assert_eq!(rows, 4);
        output.clear();
        let mut display = EditorDisplay { cursor_row: 2 };
        clear_editor(&mut output, rows, &mut display).unwrap();
        assert_eq!(output, b"\r\x1b[6A\r\x1b[0J");
    }

    #[test]
    fn exact_width_status_has_no_extra_blank_row() {
        let mut output = Vec::new();
        let rows = write_statuses_with_width(&mut output, &["1234567890".into()], 10).unwrap();
        assert_eq!(rows, 1);
        assert_eq!(output, b"1234567890\r\n");
    }

    #[test]
    fn long_input_wraps_before_terminal_edge() {
        let mut output = Vec::new();
        let position = write_wrapped(&mut output, "prompt> a long command", 10).unwrap();
        assert_eq!(output, b"prompt> a \r\nlong comma\r\nnd");
        assert_eq!((position.row, position.column), (2, 2));
        output.clear();
        let position = write_wrapped(&mut output, "\x1b[1;36mprompt>\x1b[0m abc", 10).unwrap();
        assert_eq!(position.row, 1);
        assert_eq!(position.column, 1);
    }

    #[test]
    fn clear_editor_without_wrap_remains_simple() {
        let mut output = Vec::new();
        let mut display = EditorDisplay::default();
        clear_editor(&mut output, 1, &mut display).unwrap();
        assert_eq!(output, b"\r\x1b[1A\r\x1b[0J");
    }

    #[test]
    fn word_motion_and_kill_keep_utf8_boundaries() {
        let mut line = "git αβ  status".as_bytes().to_vec();
        let end = line.len();
        let start = backward_word(&line, end);
        assert_eq!(&line[start..], b"status");
        assert_eq!(forward_word(&line, 4), "git αβ".len());
        let mut ring = Vec::new();
        kill(
            &mut line,
            start,
            end,
            &mut ring,
            None,
            KillDirection::Backward,
        );
        let next_start = backward_word(&line, line.len());
        let next_end = line.len();
        kill(
            &mut line,
            next_start,
            next_end,
            &mut ring,
            Some(KillDirection::Backward),
            KillDirection::Backward,
        );
        assert_eq!(String::from_utf8(ring).unwrap(), "αβ  status");
    }
}
