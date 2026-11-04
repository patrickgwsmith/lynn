//! Loads PATH from the user's login shell when no shell started Lynn, such as
//! when a terminal runs Lynn directly as its startup command. Without this,
//! Lynn inherits only the bare system PATH and can't find tools installed in
//! places like `~/.local/bin` or Homebrew.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MARKER: &str = "__LYNN_LOGIN_PATH__";
const TIMEOUT: Duration = Duration::from_secs(5);

/// Replaces PATH with the login shell's PATH if `SHLVL` is unset, which means
/// Lynn wasn't started from a shell. Call before any threads start.
pub fn load_if_started_without_shell() {
    if std::env::var_os("SHLVL").is_some() {
        return;
    }
    let Some(shell) = std::env::var_os("SHELL") else {
        return;
    };
    let shell = Path::new(&shell);
    if !shell.is_absolute() {
        return;
    }
    if let Some(path) = login_shell_path(shell) {
        std::env::set_var("PATH", path);
    }
}

/// Runs `shell` as an interactive login shell, so both its profile and rc
/// files apply, and returns the PATH it ends up with.
fn login_shell_path(shell: &Path) -> Option<String> {
    let script = format!("printf '{MARKER}%s{MARKER}' \"$PATH\"");
    let mut child = Command::new(shell)
        .args(["-l", "-i", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut output = String::new();
    child.stdout.take()?.read_to_string(&mut output).ok()?;
    path_between_markers(&output)
}

/// Finds the PATH between the markers, ignoring anything the shell's startup
/// files printed around it.
fn path_between_markers(output: &str) -> Option<String> {
    let start = output.rfind(MARKER)?;
    let before = &output[..start];
    let open = before.rfind(MARKER)? + MARKER.len();
    let path = &output[open..start];
    (!path.is_empty()).then(|| path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_is_read_between_markers_despite_startup_output() {
        let output = format!("welcome!\n{MARKER}/opt/homebrew/bin:/usr/bin{MARKER}");
        assert_eq!(
            path_between_markers(&output).as_deref(),
            Some("/opt/homebrew/bin:/usr/bin")
        );
        assert_eq!(path_between_markers("no markers"), None);
        assert_eq!(path_between_markers(&format!("{MARKER}{MARKER}")), None);
    }

    #[test]
    fn login_shell_path_reads_path_from_a_real_shell() {
        let path = login_shell_path(Path::new("/bin/sh")).expect("PATH from /bin/sh");
        assert!(path.contains("/usr/bin") || path.contains("/bin"));
    }
}
