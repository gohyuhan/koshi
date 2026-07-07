//! The `koshi` binary entrypoint.

use std::process::ExitCode;

use clap::Parser;
use koshi_cli::cli::Cli;
use koshi_core::command::CliExitCode;

fn main() -> ExitCode {
    // Usage errors print through clap and exit 2; --help/--version exit 0.
    let cli = Cli::parse();

    let code = if cli.is_interactive_launch() {
        match koshi_cli::app::run() {
            Ok(()) => CliExitCode::Success,
            Err(err) => {
                eprintln!("koshi: {err}");
                CliExitCode::RuntimeAction
            }
        }
    } else {
        // Every non-bare invocation is served over IPC by the daemon; this
        // build carries no IPC client, so the parsed command cannot be sent.
        eprintln!("koshi: IPC unavailable: no koshi daemon is reachable");
        CliExitCode::IpcUnavailable
    };

    // Exit codes are 0..=4, always in u8 range.
    ExitCode::from(code.code() as u8)
}
