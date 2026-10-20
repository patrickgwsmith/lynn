use std::collections::VecDeque;
use std::io::{self, Read};
use std::path::Path;
use std::process::Stdio;
use std::thread;

const MAX_LIVE_LINES: usize = 4_000;
const MAX_LIVE_BYTES: usize = 1024 * 1024;
const TRUNCATED_MARKER: &str = "\n... middle of output omitted ...\n";

pub fn run(program: &str, script: &str, cwd: &Path) -> Result<String, String> {
    let mut child = super::shell::reader_shell_command(program, cwd, true)
        .arg("-c")
        .arg(script)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{program}?: {error}"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_reader = thread::spawn(move || read_capped(stdout));
    let stderr_reader = thread::spawn(move || read_capped(stderr));
    let status = child.wait().map_err(|error| error.to_string())?;
    let (stdout, _) = stdout_reader
        .join()
        .map_err(|_| "stdout reader stopped".to_owned())?
        .map_err(|error| error.to_string())?;
    let (stderr, _) = stderr_reader
        .join()
        .map_err(|_| "stderr reader stopped".to_owned())?
        .map_err(|error| error.to_string())?;
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    let mut text = stdout.trim_end().to_owned();
    if !stderr.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("stderr: ");
        text.push_str(stderr.trim_end());
    }
    text = limit(&text);
    if !status.success() {
        return Err(format!("exit {status}: {text}"));
    }
    if text.is_empty() {
        text.push_str("(no output)");
    }
    Ok(text)
}

fn read_capped(mut pipe: impl Read) -> io::Result<(Vec<u8>, bool)> {
    let payload_bytes = MAX_LIVE_BYTES - TRUNCATED_MARKER.len();
    let head_bytes = payload_bytes / 2;
    let tail_bytes = payload_bytes - head_bytes;
    let mut head = Vec::new();
    let mut tail: VecDeque<u8> = VecDeque::new();
    let mut truncated = false;
    let mut buffer = [0; 8192];
    loop {
        let count = pipe.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let keep = head_bytes.saturating_sub(head.len()).min(count);
        head.extend_from_slice(&buffer[..keep]);
        tail.extend(&buffer[keep..count]);
        if tail.len() > tail_bytes {
            tail.drain(..tail.len() - tail_bytes);
            truncated = true;
        }
    }
    let mut result = head;
    if truncated {
        result.extend_from_slice(TRUNCATED_MARKER.as_bytes());
    }
    result.extend(tail);
    Ok((result, truncated))
}

fn limit(text: &str) -> String {
    let lines: Vec<_> = text.lines().collect();
    let selected = if lines.len() > MAX_LIVE_LINES {
        let half = MAX_LIVE_LINES / 2;
        format!(
            "{}{}{}",
            lines[..half].join("\n"),
            TRUNCATED_MARKER,
            lines[lines.len() - half..].join("\n")
        )
    } else {
        text.to_owned()
    };
    if selected.len() <= MAX_LIVE_BYTES {
        selected
    } else {
        keep_ends(&selected, MAX_LIVE_BYTES)
    }
}

fn keep_ends(text: &str, max_bytes: usize) -> String {
    let payload_bytes = max_bytes.saturating_sub(TRUNCATED_MARKER.len());
    let mut head_end = payload_bytes / 2;
    let mut tail_start = text.len() - (payload_bytes - head_end);
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!(
        "{}{}{}",
        &text[..head_end],
        TRUNCATED_MARKER,
        &text[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::{limit, read_capped, MAX_LIVE_BYTES, MAX_LIVE_LINES};

    #[test]
    fn large_live_output_is_capped_and_drained() {
        let mut data = vec![b'x'; MAX_LIVE_BYTES];
        data.extend(vec![b'y'; 100]);
        let (captured, truncated) = read_capped(data.as_slice()).unwrap();
        assert_eq!(captured.len(), MAX_LIVE_BYTES);
        assert!(truncated);
        let display = limit(&String::from_utf8(captured).unwrap());
        assert!(display.starts_with("xxxx"));
        assert!(display.ends_with("yyyy"));
        assert!(display.contains("... middle of output omitted ..."));
    }

    #[test]
    fn long_line_keeps_its_start_and_end() {
        let text = format!("begin{}end", "x".repeat(MAX_LIVE_BYTES));
        let display = limit(&text);
        assert!(display.starts_with("begin"));
        assert!(display.ends_with("end"));
        assert_eq!(display.len(), MAX_LIVE_BYTES);
    }

    #[test]
    fn line_limit_keeps_four_thousand_lines() {
        let text = (0..MAX_LIVE_LINES + 1)
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let display = limit(&text);
        assert_eq!(display.lines().count(), MAX_LIVE_LINES + 1);
        assert!(display.contains("\n1999\n... middle of output omitted ...\n2001\n"));
        assert!(display.ends_with("4000"));
    }
}
