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
    let raw_command_argument = std::env::args_os().nth(1);
    let requested_command = raw_command_argument
        .as_deref()
        .map(|command_argument| command_argument.to_string_lossy());
    match requested_command.as_deref() {
        Some("dep-guard") => dep_guard::run_dependency_guard(),
        Some(unknown_command) => {
            eprintln!("xtask: unknown command `{unknown_command}`");
            print_usage();
            ExitCode::FAILURE
        }
        None => {
            print_usage();
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    eprintln!("usage: cargo xtask <command>");
    eprintln!("commands:");
    eprintln!("  dep-guard   assert architecture dependency-direction rules");
}
