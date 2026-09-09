//! `xtask` runs repository checks through `cargo xtask <command>` and is not
//! published.
//!
//! The supported command is `dep-guard`, which checks direct dependency edges.
//! Arguments after the command are ignored. An unknown command such as `foo`
//! writes ``xtask: unknown command `foo` `` and the usage text to stderr, then
//! exits with failure. With no command, it writes only the usage text to stderr
//! and exits with failure. Invalid Unicode in the first argument is replaced
//! with `U+FFFD` and handled as an unknown command.

use std::process::ExitCode;

mod dep_guard;

fn main() -> ExitCode {
    let raw_command = std::env::args_os().nth(1);
    let command = raw_command.as_deref().map(|arg| arg.to_string_lossy());
    match command.as_deref() {
        Some("dep-guard") => dep_guard::run(),
        Some(other) => {
            eprintln!("xtask: unknown command `{other}`");
            usage();
            ExitCode::FAILURE
        }
        None => {
            usage();
            ExitCode::FAILURE
        }
    }
}

fn usage() {
    eprintln!("usage: cargo xtask <command>");
    eprintln!("commands:");
    eprintln!("  dep-guard   assert architecture dependency-direction rules");
}
