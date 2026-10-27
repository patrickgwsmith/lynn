use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn background_jobs_offer_an_explicit_shell_without_running_the_line() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_lynn"))
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            br#"echo skipped && echo skipped &
echo $(echo skipped) &
echo luna?(this prompt contains &) &
echo 'literal&value'
echo escaped\&value
bash! true & wait; echo explicit
exit 0
"#,
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "literal&value\nescaped&value\nexplicit\n"
    );
    let error = String::from_utf8(output.stderr).unwrap();
    assert_eq!(error.matches("background jobs").count(), 3);
    assert!(error.contains("enter zsh!"));
}

#[test]
fn chains_preserve_builtin_state_and_expand_each_command_when_it_runs() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_lynn"))
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            br#"export LYNN_CHAIN_TEST=hello && echo $LYNN_CHAIN_TEST
cd / && pwd
false && echo skipped
echo status:$?
echo 'literal&&value' && echo escaped\&\&value
true && exit 7 && echo skipped
"#,
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "hello\n/\nstatus:1\nliteral&&value\nescaped&&value\n"
    );
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
}
