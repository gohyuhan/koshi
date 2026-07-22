//! The `koshi` binary entrypoint.

use std::process::ExitCode;

use clap::Parser;
use koshi::cli::{ActionsCommand, Cli, CliCommand, KeysCommand};
use koshi::error::CliError;
use koshi::in_session::InSessionContext;
use koshi::ipc_client;
use koshi::keymap::{self, KeymapView};
use koshi::output;
use koshi::updater;
use koshi_core::command::{CliExitCode, CommandResult};

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
/// `actions` query and the read-only `keys` queries render locally; the
/// interactive launch runs the app; the action verbs read the in-session
/// identity from the environment and travel the session's control socket as
/// commands. Run outside a session, or with a verb the socket does not serve
/// yet, they report IPC unavailable.
fn run(cli: &Cli) -> Result<(), CliError> {
    if let Some(CliCommand::Actions { command }) = &cli.command {
        // `actions` introspects the static action table, so it renders locally
        // rather than being served over IPC like the session verbs.
        return run_actions(command);
    }

    if let Some(CliCommand::Keys { command }) = &cli.command {
        // Every keys verb is a read-only query folding the user's keybinding
        // file onto the built-in defaults locally.
        return run_keys_query(command);
    }

    if let Some(CliCommand::Update) = &cli.command {
        // `update` runs locally: it talks to GitHub and the local filesystem,
        // not the session daemon.
        return updater::run_update_command();
    }

    if cli.is_interactive_launch() {
        // Offer a newer release before entering raw mode, so the prompt is a
        // plain stdin read; failures never block the launch.
        updater::maybe_prompt_startup_update();
        return koshi::app::run(cli.profile.as_deref()).map_err(|err| CliError::Runtime {
            detail: err.to_string(),
        });
    }

    // Session verbs read the in-session identity first, so a broken pane
    // environment reports itself rather than as a missing daemon.
    let in_session = InSessionContext::from_env()?;

    let Some(context) = in_session else {
        // External targeting (count rules, explicit --session/--tab/--pane)
        // is served by its own resolution layer; this build routes only
        // in-session commands.
        return Err(CliError::IpcUnavailable {
            detail: "no koshi daemon is reachable".to_string(),
        });
    };

    // The action verbs travel the socket as commands; the remaining verbs
    // (discovery listings, lifecycle) have their own serving layers.
    let Some((_, command)) = cli.command.as_ref().and_then(CliCommand::to_action) else {
        return Err(CliError::IpcUnavailable {
            detail: "this command is not served over the control socket yet".to_string(),
        });
    };

    match ipc_client::submit_in_session(&context, command)? {
        CommandResult::Ok { .. } => Ok(()),
        CommandResult::Rejected { reason, help, .. } => {
            Err(CliError::CommandRejected { reason, help })
        }
    }
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

/// Serve a `koshi keys` query from the offline keymap view: the user's
/// keybinding file folded onto the built-in defaults. The running session's
/// own layers (`session`, `layout`) arrive with the IPC client.
fn run_keys_query(command: &KeysCommand) -> Result<(), CliError> {
    match command {
        KeysCommand::List {
            mode,
            scope,
            recommended,
            format,
        } => {
            if *recommended {
                print!("{}", output::render_keys_recommended(*format));
                return Ok(());
            }
            let view = keymap::load_keymap_view();
            warn_keymap_reverted(&view);
            print!(
                "{}",
                output::render_keys_list(&view, mode.as_deref(), *scope, *format)
            );
            Ok(())
        }
        KeysCommand::Describe { sequence, format } => {
            let view = keymap::load_keymap_view();
            warn_keymap_reverted(&view);
            match output::render_keys_describe(&view, sequence, *format) {
                Ok(Some(rendered)) => {
                    print!("{rendered}");
                    Ok(())
                }
                Ok(None) => Err(CliError::UnboundKey {
                    sequence: sequence.clone(),
                }),
                Err(detail) => Err(CliError::InvalidArgs { detail }),
            }
        }
        KeysCommand::Conflicts { format } => {
            // An ignored file is part of the rendered answer itself, so no
            // stderr note is needed here.
            let view = keymap::load_keymap_view();
            print!("{}", output::render_keys_conflicts(&view, *format));
            Ok(())
        }
        KeysCommand::Validate { path, format } => {
            let outcome = keymap::validate_file(path).map_err(|err| CliError::InvalidArgs {
                detail: format!("cannot read {}: {err}", path.display()),
            })?;
            print!("{}", output::render_keys_validate(&outcome, *format));
            if output::validation_applies(&outcome) {
                Ok(())
            } else {
                Err(CliError::InvalidKeymapFile {
                    path: path.display().to_string(),
                })
            }
        }
    }
}

/// Warn on stderr when the user's keybinding file exists but was not
/// admitted, so the defaults-only answer on stdout is not mistaken for the
/// file's contents.
fn warn_keymap_reverted(view: &KeymapView) {
    if let Some(error) = &view.file_error {
        eprintln!("koshi: keybinding file ignored: {error}");
    } else if view.reverted {
        eprintln!(
            "koshi: keybinding file not applied (conflicts); showing built-in defaults — \
             run `koshi keys conflicts` for details"
        );
    }
}
