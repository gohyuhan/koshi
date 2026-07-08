//! The `koshi` binary entrypoint.

use std::process::ExitCode;

use clap::Parser;
use koshi_cli::cli::{ActionsCommand, Cli, CliCommand};
use koshi_cli::output;
use koshi_core::command::CliExitCode;

fn main() -> ExitCode {
    // Usage errors print through clap and exit 2; --help/--version exit 0.
    let cli = Cli::parse();

    let code = if let Some(CliCommand::Actions { command }) = &cli.command {
        // `actions` introspects the static action table, so it renders locally
        // rather than being served over IPC like the session verbs.
        run_actions(command)
    } else if cli.is_interactive_launch() {
        match koshi_cli::app::run() {
            Ok(()) => CliExitCode::Success,
            Err(err) => {
                eprintln!("koshi: {err}");
                CliExitCode::RuntimeAction
            }
        }
    } else {
        // The session verbs are served over IPC by the daemon; this build
        // carries no IPC client, so the parsed command cannot be sent.
        eprintln!("koshi: IPC unavailable: no koshi daemon is reachable");
        CliExitCode::IpcUnavailable
    };

    // Exit codes are 0..=4, always in u8 range.
    ExitCode::from(code.code() as u8)
}

/// Serve a `koshi actions` query from the static action table: print the
/// rendered answer, or report an unknown action name.
fn run_actions(command: &ActionsCommand) -> CliExitCode {
    match command {
        ActionsCommand::List { format } => {
            print!("{}", output::render_actions_list(*format));
            CliExitCode::Success
        }
        ActionsCommand::Explain { action, format } => {
            match output::render_action_explain(action, *format) {
                Some(rendered) => {
                    print!("{rendered}");
                    CliExitCode::Success
                }
                None => {
                    eprintln!("koshi: unknown action: {action}");
                    CliExitCode::UsageOrConfig
                }
            }
        }
    }
}
