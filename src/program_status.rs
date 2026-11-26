//! Reports what Lynn is doing with the Program Status Protocol (OSC 7501), so
//! a terminal such as Rex can show that agents are working, or that a reply
//! is ready, while you look at another tab.
//!
//! Lynn behaves like a shell: it marks each prompt with OSC 133 A and keeps
//! its own records under the `lynn` id, leaving the root record to the
//! programs you run. At a new prompt the terminal drops their `working` and
//! `blocked` records but keeps their `done` and `error` ones.
//!
//! See <https://www.superlogical.com/rex/docs/build/program-status>.

use std::collections::BTreeMap;
use std::io::{self, Write};

/// The id of Lynn's own record. Request records sit beneath it.
pub const ROOT_ID: &str = "lynn";

/// Longest decoded `msg` the protocol allows.
const MSG_LIMIT: usize = 2048;
/// Longest decoded `title` the protocol allows.
const TITLE_LIMIT: usize = 192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Working,
    Done,
    Error,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Working => "working",
            State::Done => "done",
            State::Error => "error",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub state: State,
    pub title: Option<String>,
    pub msg: Option<String>,
}

impl Record {
    pub fn new(state: State) -> Self {
        Self {
            state,
            title: None,
            msg: None,
        }
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn msg(mut self, msg: impl Into<String>) -> Self {
        self.msg = Some(msg.into());
        self
    }
}

/// Lynn's records by id, such as `lynn` and `lynn/r28`.
pub type Records = BTreeMap<String, Record>;

/// Sends only the records that changed since the last report, since the
/// protocol needs no heartbeat.
pub struct Reporter {
    enabled: bool,
    sent: Records,
}

impl Reporter {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            sent: Records::new(),
        }
    }

    /// Marks the start of a prompt. The terminal drops `working` records
    /// there, Lynn's included, so the next [`Reporter::sync`] sends them all
    /// again.
    pub fn prompt_start(&mut self) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let mut stdout = io::stdout().lock();
        write_prompt_start(&mut stdout)?;
        stdout.flush()?;
        self.sent.clear();
        Ok(())
    }

    pub fn sync(&mut self, records: Records) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let mut stdout = io::stdout().lock();
        self.write_changes(&mut stdout, records)?;
        stdout.flush()
    }

    /// Removes all of Lynn's records, as Lynn exits.
    pub fn clear(&mut self) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let mut stdout = io::stdout().lock();
        write_clear(&mut stdout, ROOT_ID)?;
        stdout.flush()?;
        self.sent.clear();
        Ok(())
    }

    fn write_changes(&mut self, output: &mut impl Write, records: Records) -> io::Result<()> {
        for id in self.sent.keys() {
            if !records.contains_key(id) {
                write_clear(output, id)?;
            }
        }
        for (id, record) in &records {
            if self.sent.get(id) != Some(record) {
                write_report(output, id, record)?;
            }
        }
        self.sent = records;
        Ok(())
    }
}

fn write_prompt_start(output: &mut impl Write) -> io::Result<()> {
    output.write_all(b"\x1b]133;A\x1b\\")
}

fn write_clear(output: &mut impl Write, id: &str) -> io::Result<()> {
    write!(output, "\x1b]7501;state=clear:id={id}\x1b\\")
}

/// Writes one report. Every report names `app`, since a report replaces its
/// record; children would inherit it, but `lynn` itself needs it.
fn write_report(output: &mut impl Write, id: &str, record: &Record) -> io::Result<()> {
    write!(output, "\x1b]7501;state={}:id={id}", record.state.name())?;
    if id == ROOT_ID {
        output.write_all(b":app=lynn")?;
    }
    if let Some(title) = record.title.as_deref() {
        write!(output, ":title={}", base64(&one_line(title, TITLE_LIMIT)))?;
    }
    if let Some(msg) = record.msg.as_deref() {
        write!(output, ":msg={}", base64(&one_line(msg, MSG_LIMIT)))?;
    }
    output.write_all(b"\x1b\\")
}

/// Joins `text` onto one line, since the terminal refuses control
/// characters, and cuts it to `limit` bytes on a character boundary.
fn one_line(text: &str, limit: usize) -> String {
    let mut line = String::new();
    for word in text.split(|c: char| c.is_whitespace() || c.is_control()) {
        if word.is_empty() {
            continue;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if line.len() > limit {
        let mut end = limit;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
    }
    line
}

fn base64(text: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = text.as_bytes();
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                encoded.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(f: impl FnOnce(&mut Vec<u8>) -> io::Result<()>) -> String {
        let mut output = Vec::new();
        f(&mut output).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn encodes_base64_with_padding() {
        assert_eq!(base64(""), "");
        assert_eq!(base64("f"), "Zg==");
        assert_eq!(base64("fo"), "Zm8=");
        assert_eq!(base64("foo"), "Zm9v");
        assert_eq!(base64("Installing updates"), "SW5zdGFsbGluZyB1cGRhdGVz");
        assert_eq!(
            base64("Upgraded 12 packages"),
            "VXBncmFkZWQgMTIgcGFja2FnZXM="
        );
    }

    #[test]
    fn reports_lynn_with_its_app() {
        let report =
            written(|output| write_report(output, ROOT_ID, &Record::new(State::Done).msg("foo")));
        assert_eq!(
            report,
            "\x1b]7501;state=done:id=lynn:app=lynn:msg=Zm9v\x1b\\"
        );
    }

    #[test]
    fn request_records_inherit_the_app() {
        let report = written(|output| {
            write_report(
                output,
                "lynn/r3",
                &Record::new(State::Working).title("fo").msg("f"),
            )
        });
        assert_eq!(
            report,
            "\x1b]7501;state=working:id=lynn/r3:title=Zm8=:msg=Zg==\x1b\\"
        );
    }

    #[test]
    fn keeps_text_on_one_line_without_control_characters() {
        assert_eq!(
            one_line("fix\nthe\t parser\x1b]0;x\x07", 100),
            "fix the parser ]0;x"
        );
        assert_eq!(one_line("  ", 100), "");
    }

    #[test]
    fn cuts_text_on_a_character_boundary() {
        assert_eq!(one_line("aé", 2), "a");
        assert_eq!(one_line(&"x".repeat(3000), MSG_LIMIT).len(), MSG_LIMIT);
    }

    #[test]
    fn sends_only_changes_and_clears_removed_records() {
        let mut reporter = Reporter::new(true);
        let working = Records::from([
            (ROOT_ID.into(), Record::new(State::Working)),
            ("lynn/r1".into(), Record::new(State::Working).title("f")),
        ]);
        let first = written(|output| reporter.write_changes(output, working.clone()));
        assert_eq!(
            first,
            "\x1b]7501;state=working:id=lynn:app=lynn\x1b\\\
             \x1b]7501;state=working:id=lynn/r1:title=Zg==\x1b\\"
        );
        assert_eq!(
            written(|output| reporter.write_changes(output, working)),
            ""
        );
        let done = Records::from([(ROOT_ID.into(), Record::new(State::Done))]);
        assert_eq!(
            written(|output| reporter.write_changes(output, done)),
            "\x1b]7501;state=clear:id=lynn/r1\x1b\\\
             \x1b]7501;state=done:id=lynn:app=lynn\x1b\\"
        );
    }

    #[test]
    fn marks_the_prompt_start() {
        assert_eq!(written(write_prompt_start), "\x1b]133;A\x1b\\");
    }
}
