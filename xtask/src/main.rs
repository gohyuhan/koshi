//! `xtask` — repository automation runner, invoked as
//! `cargo xtask <command>` and never shipped.
//!
//! The one command is `dep-guard`, which checks the allowed crate-dependency
//! edges. Arguments after the command are ignored. Any other command, for
//! example `foo`, prints ``xtask: unknown command `foo` `` and then the usage
//! text on stderr and exits with a failure code. No argument at all prints
//! the usage text alone and exits with a failure code. An argument that is not
//! valid Unicode is read with each bad byte replaced by `U+FFFD` and reported
//! as an unknown command.

use std::process::ExitCode;

mod dep_guard;

fn main() -> ExitCode {
    let command = std::env::args_os().nth(1);
    match command
        .as_deref()
        .map(|arg| arg.to_string_lossy())
        .as_deref()
    {
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
