//! Tests for the `xtask` command line: an argument that is not valid Unicode
//! is reported as an unknown command rather than ending the process.

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
