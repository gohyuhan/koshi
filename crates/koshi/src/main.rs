//! The `koshi` binary entrypoint.

use std::process::ExitCode;

use clap::Parser;
use koshi::cli::{ActionsCommand, Cli, CliCommand};
use koshi::error::CliError;
use koshi::output;
use koshi_core::command::CliExitCode;

fn main() -> ExitCode {
    // Usage errors print through clap and exit 2; --help/--version exit 0.
    let cli = Cli::parse();

    // Every path funnels through one result, so a single conversion maps the
    // outcome to the process exit code.
    let code = match run(&cli) {
        Ok(()) => CliExitCode::Success,
        Err(err) => {
            eprintln!("koshi: {err}");
            CliExitCode::from(&err)
        }
    };

    // Exit codes are 0..=4, always in u8 range.
    ExitCode::from(code.code() as u8)
}

/// Run one parsed invocation, reporting failures as a [`CliError`]. The
/// `actions` query renders locally; the interactive launch runs the app; every
/// other verb needs the IPC client this build does not carry.
fn run(cli: &Cli) -> Result<(), CliError> {
    if let Some(CliCommand::Actions { command }) = &cli.command {
        // `actions` introspects the static action table, so it renders locally
        // rather than being served over IPC like the session verbs.
        return run_actions(command);
    }

    if cli.is_interactive_launch() {
        return koshi::app::run().map_err(|err| CliError::Runtime {
            detail: err.to_string(),
        });
    }

    // The session verbs are served over IPC by the daemon; this build carries
    // no IPC client, so the parsed command cannot be sent.
    Err(CliError::IpcUnavailable {
        detail: "no koshi daemon is reachable".to_string(),
    })
}

/// Serve a `koshi actions` query from the static action table: print the
/// rendered answer, or report an unknown action name.
fn run_actions(command: &ActionsCommand) -> Result<(), CliError> {
    match command {
        ActionsCommand::List { format } => {
            print!("{}", output::render_actions_list(*format));
            Ok(())
        }
        ActionsCommand::Explain { action, format } => {
            match output::render_action_explain(action, *format) {
                Some(rendered) => {
                    print!("{rendered}");
                    Ok(())
                }
                None => Err(CliError::UnknownAction {
                    name: action.clone(),
                }),
            }
        }
    }
}
