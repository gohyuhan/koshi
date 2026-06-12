//! `xtask` — repository automation runner. Not shipped; invoked via
//! `cargo xtask <command>`. Currently hosts the dependency-direction guard
//! that enforces the architecture's allowed crate-dependency edges.

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
