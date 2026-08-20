//! `xtask` — repository automation runner, invoked as
//! `cargo xtask <command>` and never shipped.
//!
//! The one command is `dep-guard`, which checks the allowed crate-dependency
//! edges. Any other argument, and no argument at all, prints the usage text on
//! stderr and exits with a failure code.

use std::process::ExitCode;

mod dep_guard;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
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
