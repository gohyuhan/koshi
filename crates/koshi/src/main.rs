//! The `koshi` binary entrypoint.

use std::process::ExitCode;

use clap::Parser;
use koshi::cli::{ActionsCommand, Cli, CliCommand, InspectTarget, KeysCommand, ResolvedTargets};
use koshi::config_command;
use koshi::discovery::{self, Discovered};
use koshi::error::CliError;
use koshi::in_session::InSessionContext;
use koshi::ipc_client;
use koshi::keymap::{self, KeymapView};
use koshi::output;
use koshi::session_control;
use koshi::targeting::{self, Route};
use koshi::updater;
use koshi_core::command::{CliExitCode, CommandResult};
use koshi_core::ids::SessionId;

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
/// discovery queries render what the running sessions report about
/// themselves; the interactive launch runs the app; the action verbs travel
/// a session's control socket as commands. Inside a pane they go to the pane's own
/// session; outside one, the routing layer picks the target session from the
/// explicit `--session`/`--tab`/`--pane`/`--client` flags, else defaults to
/// the only running session. A verb the socket does not serve yet reports
/// IPC unavailable.
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

    if let Some(CliCommand::Config { command }) = &cli.command {
        return config_command::run(command);
    }

    if let Some(CliCommand::Update) = &cli.command {
        // `update` runs locally: it talks to GitHub and the local filesystem,
        // not the session daemon.
        return updater::run_update_command();
    }

    if let Some(command) = cli.command.as_ref().filter(|command| is_discovery(command)) {
        // The discovery queries read every running session's state and render
        // locally; they dispatch no command, so they never enter the routing
        // layer the action verbs use.
        return run_discovery(command);
    }

    if let Some(CliCommand::KillSession { session }) = &cli.command {
        return finish_command(session_control::kill_session(session.as_deref())?);
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

    // The action verbs travel a socket as commands; the remaining verbs
    // (discovery listings, lifecycle) have their own serving layers. The
    // probe with default targets only asks "is this an action verb" — the
    // real command is built after routing resolves the targets.
    let is_action = cli
        .command
        .as_ref()
        .is_some_and(|command| command.to_action(&ResolvedTargets::default()).is_some());
    if !is_action {
        return Err(CliError::IpcUnavailable {
            detail: "this command is not served over the control socket yet".to_string(),
        });
    }
    let cli_command = cli
        .command
        .as_ref()
        .expect("an action verb is always a parsed subcommand");

    let result = match targeting::route(cli_command, in_session.as_ref())? {
        Route::InSession(targets) => {
            let context = in_session.expect("an in-session route needs the pane identity");
            let (_, command) = cli_command
                .to_action(&targets)
                .expect("checked to be an action verb above");
            ipc_client::submit_in_session(&context, command)?
        }
        Route::External { session, targets } => {
            let (_, command) = cli_command
                .to_action(&targets)
                .expect("checked to be an action verb above");
            ipc_client::submit_external(session, command)?
        }
    };

    finish_command(result)
}

/// Print an applied command's created ids, or surface its rejection.
fn finish_command(result: CommandResult) -> Result<(), CliError> {
    match result {
        CommandResult::Ok { emitted_events, .. } => {
            print!("{}", output::render_created_events(&emitted_events));
            Ok(())
        }
        CommandResult::Rejected { reason, help, .. } => {
            Err(CliError::CommandRejected { reason, help })
        }
    }
}

/// Whether `command` is a discovery query: a `list-*` verb or an `inspect`
/// form.
fn is_discovery(command: &CliCommand) -> bool {
    matches!(
        command,
        CliCommand::ListSessions { .. }
            | CliCommand::ListTabs { .. }
            | CliCommand::ListPanes { .. }
            | CliCommand::ListClients { .. }
            | CliCommand::Inspect { .. }
    )
}

/// The one session a discovery query is scoped to: a listing's `--session`
/// flag, or the session an `inspect session` names. Every other query spans
/// all running sessions.
fn discovery_session(command: &CliCommand) -> Option<SessionId> {
    match command {
        CliCommand::ListTabs { session, .. }
        | CliCommand::ListPanes { session, .. }
        | CliCommand::ListClients { session, .. } => *session,
        CliCommand::Inspect {
            target: InspectTarget::Session { session, .. },
        } => Some(*session),
        _ => None,
    }
}

/// Serve a discovery query from live state: probe the running sessions the
/// query is scoped to, keep the rows it asked for, and print them.
///
/// A scoped query asks one session and reports it as not running when
/// nothing answers; an unscoped one spans every session, so nothing running
/// is an empty answer — the header row alone — not an error.
///
/// A listing claims to be the whole picture, so it prints its rows and then
/// reports a session that could not answer as a failure. An `inspect` claims
/// one entity: finding it proves it exists whatever the other sessions would
/// have said, so a successful one is a success.
fn run_discovery(command: &CliCommand) -> Result<(), CliError> {
    let runtime_dir = ipc_client::runtime_dir()?;
    let found = match discovery_session(command) {
        Some(session_id) => Discovered::of(discovery::fetch_one(&runtime_dir, session_id)?),
        None => discovery::fetch_all(&runtime_dir),
    };
    let sessions = found.sessions.as_slice();

    let rendered = match command {
        CliCommand::ListSessions { format } => {
            output::render_sessions(&discovery::session_rows(sessions), *format)
        }
        CliCommand::ListTabs { format, .. } => {
            output::render_tabs(&discovery::tab_rows(sessions), *format)
        }
        CliCommand::ListPanes { format, .. } => {
            output::render_panes(&discovery::pane_rows(sessions), *format)
        }
        CliCommand::ListClients { format, .. } => {
            output::render_clients(&discovery::client_rows(sessions), *format)
        }
        CliCommand::Inspect { target } => match target {
            InspectTarget::Session { session, format } => {
                output::render_session(&discovery::find_session(&found, *session)?, *format)
            }
            InspectTarget::Tab { tab, format } => {
                output::render_tab(&discovery::find_tab(&found, *tab)?, *format)
            }
            InspectTarget::Pane { pane, format } => {
                output::render_pane(&discovery::find_pane(&found, *pane)?, *format)
            }
            InspectTarget::Client { client, format } => {
                output::render_client(&discovery::find_client(&found, *client)?, *format)
            }
        },
        _ => unreachable!("checked to be a discovery query above"),
    };
    print!("{rendered}");

    let listing = matches!(
        command,
        CliCommand::ListSessions { .. }
            | CliCommand::ListTabs { .. }
            | CliCommand::ListPanes { .. }
            | CliCommand::ListClients { .. }
    );
    match found.incomplete_listing() {
        Some(error) if listing => Err(error),
        _ => Ok(()),
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
