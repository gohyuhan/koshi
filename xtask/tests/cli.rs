//! Tests exact failure output for missing and unknown commands, including a
//! non-UTF-8 command argument on Unix.

fn run_xtask(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(args)
        .output()
        .expect("the xtask binary runs")
}

#[test]
fn no_command_prints_usage_and_exits_with_failure() {
    let out = run_xtask(&[]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, b"");
    assert_eq!(
        String::from_utf8(out.stderr).expect("usage is UTF-8"),
        "usage: cargo xtask <command>\n\
         commands:\n\
         \x20 dep-guard   assert architecture dependency-direction rules\n"
    );
}

#[test]
fn unknown_command_prints_diagnostic_and_usage_and_exits_with_failure() {
    let out = run_xtask(&["unknown"]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, b"");
    assert_eq!(
        String::from_utf8(out.stderr).expect("diagnostic is UTF-8"),
        "xtask: unknown command `unknown`\n\
         usage: cargo xtask <command>\n\
         commands:\n\
         \x20 dep-guard   assert architecture dependency-direction rules\n"
    );
}

#[cfg(unix)]
#[test]
fn a_non_utf8_argument_is_reported_as_an_unknown_command() {
    use std::os::unix::ffi::OsStrExt;

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg(std::ffi::OsStr::from_bytes(b"\xff"))
        .output()
        .expect("the xtask binary runs");

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "xtask: unknown command `\u{fffd}`\n\
         usage: cargo xtask <command>\n\
         commands:\n\
         \x20 dep-guard   assert architecture dependency-direction rules\n"
    );
}
