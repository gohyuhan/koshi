//! The `koshi` binary entrypoint.

use std::process::ExitCode;

use clap::Parser;
use koshi::attach;
use koshi::cli::{
    parse_session_ref, ActionsCommand, Cli, CliCommand, DebugCommand, FormatArg, InspectTarget,
    KeysCommand, ResolvedTargets, SessionRef, TabRef,
};
use koshi::config;
use koshi::config_command;
use koshi::discovery;
use koshi::error::CliError;
use koshi::in_session::InSessionContext;
use koshi::ipc_client;
use koshi::keymap::{self, KeymapView};
use koshi::output;
use koshi::router;
use koshi::session_control;
use koshi::session_server;
use koshi::targeting::{self, Route};
use koshi::updater;
use koshi_core::command::{CliExitCode, Command, CommandResult, DetachArgs};

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
/// themselves; the headless launch creates a session and prints its id; the
/// bare launch creates a session and attaches this terminal to it; the action
/// verbs travel
/// a session's control socket as commands. Inside a pane they go to the pane's own
/// session; outside one, the routing layer picks the target session from the
/// explicit `--session`/`--tab`/`--pane`/`--client` flags, else defaults to
/// the only running session. A verb the socket does not serve yet reports
/// IPC unavailable.
fn run(cli: &Cli) -> Result<(), CliError> {
    // An entry point marked `#[beta_feature]` reads a process-wide flag and
    // takes no gate argument, so the flag is set before any verb dispatches:
    // one `allow-beta-features` answer covers the CLI verbs and the
    // interactive launch alike.
    let app = config::load_app_layer();
    config::apply_beta_gate(app.clone());

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

    if let Some(CliCommand::ServeRouter { runtime_dir }) = &cli.command {
        // This process becomes the router: it serves the control socket in
        // that directory until no session is left.
        let runtime_dir = match runtime_dir {
            Some(dir) => dir.clone(),
            None => ipc_client::runtime_dir()?,
        };
        return router::run_router(&runtime_dir).map_err(|err| CliError::Runtime {
            detail: err.to_string(),
        });
    }

    if let Some(CliCommand::ServeSession {
        session_id,
        session_name,
        runtime_dir,
        profile,
        allow_other_users,
    }) = &cli.command
    {
        // This process becomes one session's server: the router started it
        // and gave it the identity to seed the session under.
        let runtime_dir = match runtime_dir {
            Some(dir) => dir.clone(),
            None => ipc_client::runtime_dir()?,
        };
        return session_server::run_session_server(
            &runtime_dir,
            *session_id,
            session_name.clone(),
            profile.as_deref(),
            allow_other_users.then_some(true),
        )
        .map_err(|err| CliError::Runtime {
            detail: err.to_string(),
        });
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

    if let Some(CliCommand::Debug { command }) = &cli.command {
        return run_debug(command);
    }

    if let Some(CliCommand::KillSession { session }) = &cli.command {
        return finish_command(session_control::kill_session(session.as_ref())?);
    }

    if cli.headless {
        // The session is created and left running with nothing attached, so
        // the id it prints is how the shell reaches it again.
        let runtime_dir = ipc_client::runtime_dir()?;
        let session_id = session_control::request_headless_session(
            &runtime_dir,
            cli.profile.as_deref(),
            cli.allow_other_users.then_some(true),
        )?;
        println!("[SESSION ID]: {session_id}");
        return Ok(());
    }

    if cli.is_interactive_launch() {
        // Offer a newer release before entering raw mode, so the prompt is a
        // plain stdin read; failures never block the launch.
        updater::maybe_prompt_startup_update();
        return koshi::app::run(cli.profile.as_deref());
    }

    // Session verbs read the in-session identity first, so a broken pane
    // environment reports itself rather than as a missing daemon.
    let in_session = InSessionContext::from_env()?;

    // Attach is not an action verb, so it dispatches here rather than through
    // the routing layer. Typed inside a pane it moves that pane's client to
    // the named session; typed outside one it joins that session in this
    // terminal.
    if let Some(CliCommand::Attach { session }) = &cli.command {
        return match in_session.as_ref() {
            Some(context) => {
                finish_command(attach::switch_in_session(context, session.as_deref())?)
            }
            None => attach::run(session.as_deref()),
        };
    }

    // Detach is not an action verb, so it dispatches here rather than through
    // the routing layer. Success prints nothing; a detach the session refuses
    // comes back as a rejected command.
    if let Some(CliCommand::Detach { target, all }) = &cli.command {
        return match (target.as_deref(), all) {
            (None, false) => {
                let context = in_session.as_ref().ok_or_else(|| CliError::InvalidArgs {
                    detail: "bare koshi detach only works inside a koshi session; outside one use koshi detach <id>".to_string(),
                })?;
                finish_command(ipc_client::submit_in_session(
                    context,
                    Command::Detach(DetachArgs { client: None }),
                )?)
            }
            (Some(raw), false) => finish_command(session_control::detach_client_or_session(raw)?),
            (None, true) => {
                let context = in_session.as_ref().ok_or_else(|| CliError::InvalidArgs {
                    detail: "bare koshi detach --all only works inside a koshi session; outside one use koshi detach --all <session>".to_string(),
                })?;
                finish_command(ipc_client::submit_in_session(context, Command::DetachAll)?)
            }
            (Some(raw), true) => {
                let session =
                    parse_session_ref(raw).map_err(|detail| CliError::InvalidArgs { detail })?;
                finish_command(session_control::detach_all_session(Some(&session))?)
            }
        };
    }

    // This CLI is a client, so it reads its own `layout.new-pane-direction`
    // out of `koshi.kdl` and puts it on the pane-opening verbs that were given
    // no `--direction`. The session holds no split direction to fall back on.
    let new_pane_direction = config::new_pane_direction(app);

    // The action verbs travel a socket as commands; the remaining verbs
    // (discovery listings, lifecycle) have their own serving layers. The
    // probe with default targets only asks "is this an action verb" — the
    // real command is built after routing resolves the targets.
    let is_action = cli.command.as_ref().is_some_and(|command| {
        command
            .to_action(&ResolvedTargets::default(), new_pane_direction)
            .is_some()
    });
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
                .to_action(&targets, new_pane_direction)
                .expect("checked to be an action verb above");
            ipc_client::submit_in_session(&context, command)?
        }
        Route::External { session, targets } => {
            let (_, command) = cli_command
                .to_action(&targets, new_pane_direction)
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

/// The one session a discovery query is scoped to, by id or name: a
/// listing's `--session` flag, or the session an `inspect session` names.
/// Every other query spans all running sessions.
fn discovery_session(command: &CliCommand) -> Option<&SessionRef> {
    match command {
        CliCommand::ListTabs { session, .. }
        | CliCommand::ListPanes { session, .. }
        | CliCommand::ListClients { session, .. } => session.as_ref(),
        CliCommand::Inspect {
            target: InspectTarget::Session { session, .. },
        } => Some(session),
        _ => None,
    }
}

/// Serve a discovery query from live state: probe the running sessions the
/// query is scoped to, keep the rows it asked for, and print them.
///
/// A query scoped by session id asks that one session and reports it as not
/// running when nothing answers; one scoped by session name asks every
/// session and keeps the one that matches, refusing when two share the name.
/// An unscoped query spans every session, so nothing running is an empty
/// answer — the header row alone — not an error.
///
/// A listing claims to be the whole picture, so it prints its rows and then
/// reports a session that could not answer as a failure. An `inspect` claims
/// one entity: finding it proves it exists whatever the other sessions would
/// have said, so a successful one is a success.
fn run_discovery(command: &CliCommand) -> Result<(), CliError> {
    let runtime_dir = ipc_client::runtime_dir()?;
    let found = targeting::scope_sessions(&runtime_dir, discovery_session(command))?;
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
                // The scope already resolved the named session, so the census
                // holds that one session; an empty census reports it as not
                // found.
                let overview = sessions.first().ok_or_else(|| CliError::SessionNotFound {
                    session: match session {
                        SessionRef::Id(id) => id.to_string(),
                        SessionRef::Name(name) => name.clone(),
                    },
                })?;
                output::render_session(&overview.session, *format)
            }
            InspectTarget::Tab { tab, format } => {
                let tab_id = targeting::tab_by_ref(&found, tab)?;
                output::render_tab(&discovery::find_tab(&found, tab_id)?, *format)
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

/// Serve a `koshi debug` dump from live state.
fn run_debug(command: &DebugCommand) -> Result<(), CliError> {
    match command {
        DebugCommand::DumpState { format } => run_dump_state(*format),
        DebugCommand::DumpLayout { tab, format } => run_dump_layout(tab.as_ref(), *format),
    }
}

/// Print every running session's full record, with each pane's command
/// arguments hidden.
///
/// Prints every session it reached, then fails when one could not answer.
fn run_dump_state(format: FormatArg) -> Result<(), CliError> {
    let runtime_dir = ipc_client::runtime_dir()?;
    let mut found = discovery::fetch_all(&runtime_dir);
    discovery::redact_pane_commands(&mut found.sessions);
    print!("{}", output::render_dump_state(&found.sessions, format));
    match found.incomplete_listing() {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Serve a `koshi debug dump-layout` from live state: find the sessions in
/// scope, ask each for its layout, and print them.
///
/// `--tab` naming no running tab fails the lookup, and so does a tab that
/// closes between that lookup and the session's answer. A session that refuses
/// the layout request fails the command before anything prints; a session that
/// was listening but could not be probed fails it after everything prints.
fn run_dump_layout(tab: Option<&TabRef>, format: FormatArg) -> Result<(), CliError> {
    let runtime_dir = ipc_client::runtime_dir()?;
    let found = targeting::scope_sessions(&runtime_dir, None)?;

    let layouts = match tab {
        Some(tab_ref) => {
            let tab_id = targeting::tab_by_ref(&found, tab_ref)?;
            let session_id = discovery::find_tab(&found, tab_id)?.session_id;
            vec![ipc_client::fetch_layout(
                &runtime_dir,
                session_id,
                Some(tab_id),
            )?]
        }
        None => found
            .sessions
            .iter()
            .map(|overview| ipc_client::fetch_layout(&runtime_dir, overview.session.id, None))
            .collect::<Result<Vec<_>, CliError>>()?,
    };
    print!("{}", output::render_layouts(&layouts, format));

    match found.incomplete_listing() {
        Some(error) => Err(error),
        None => Ok(()),
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
