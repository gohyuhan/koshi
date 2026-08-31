//! The `koshi` binary entrypoint.

use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use clap::Parser;
use koshi::cli::{
    parse_session_ref, ActionsCommand, Cli, CliCommand, DebugCommand, FormatArg, InspectTarget,
    KeysCommand, ResolvedTargets, TabRef,
};
use koshi::config_command;
use koshi::doctor;
use koshi::keymap::{self, KeymapView};
use koshi::output;
use koshi::remote_cmd;
use koshi::session_control;
use koshi::share;
use koshi::targeting::{self, Route};
use koshi::updater;
use koshi::version;
use koshi_client::attach;
use koshi_core::command::{CliExitCode, Command, CommandResult, DetachArgs};
use koshi_daemon::pty_supervisor;
use koshi_daemon::router;
use koshi_daemon::session_server::{self, ResumeSupport};
use koshi_ipc::protocol::ConnectionToken;
use koshi_link::config;
use koshi_link::discovery::{self, SessionRow};
use koshi_link::error::CliError;
use koshi_link::in_session::InSessionContext;
use koshi_link::ipc_client;
use koshi_link::remote_client::{self, Reach, REACH_WAIT};

fn main() -> ExitCode {
    // Usage errors print through clap and exit 2; --help/--version exit 0.
    let cli = Cli::parse();

    // A failure prints `koshi: <error>` on standard error before the process
    // exits with that error's code.
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
/// `actions` query, the read-only `keys` queries and the `doctor` checks
/// render locally; the discovery queries render what the running sessions
/// report about themselves; the headless launch creates a session and prints
/// its id; the bare launch creates a session and attaches this terminal to
/// it; the action verbs travel a session's control socket as commands. Inside
/// a pane they go to the pane's own
/// session; outside one, the routing layer picks the target session from the
/// explicit `--session`/`--tab`/`--pane`/`--client` flags, else defaults to
/// the only running session. `--remote` names another machine and picks the
/// target from that machine's sessions instead, by the same rules. A verb the
/// socket does not serve yet reports IPC unavailable.
fn run(cli: &Cli) -> Result<(), CliError> {
    // `apply_beta_gate` sets the process-wide flag every `#[beta_feature]`
    // entry point reads. It runs before any verb dispatches, so one
    // `allow-beta-features` answer covers the CLI verbs and the interactive
    // launch alike.
    let app = config::load_app_layer();
    config::apply_beta_gate(app.clone());

    // `layout.new-pane-direction` from this machine's `koshi.kdl`. A
    // pane-opening verb given no `--direction` splits toward it; the session
    // holds no direction of its own.
    let new_pane_direction = config::new_pane_direction(app);

    // `to_action` with default targets answers only whether this is an action
    // verb. The command that travels the socket is built after routing
    // resolves the targets.
    let is_action = cli.command.as_ref().is_some_and(|command| {
        command
            .to_action(&ResolvedTargets::default(), new_pane_direction)
            .is_some()
    });

    // `--remote` runs with `attach`, with `list-sessions`, and with an action
    // verb. Every other verb, `--headless`, and a bare `koshi --remote
    // <server>` are refused.
    if cli.remote.is_some()
        && !is_action
        && !matches!(
            cli.command,
            Some(CliCommand::Attach { .. }) | Some(CliCommand::ListSessions { .. })
        )
    {
        return Err(CliError::InvalidArgs {
            detail: "--remote works with `attach`, `list-sessions`, and the action verbs, \
                     such as `koshi attach --remote <server>`"
                .to_string(),
        });
    }

    if let Some(CliCommand::Actions { command }) = &cli.command {
        // `actions` renders from the static action table on this machine and
        // asks nothing over IPC.
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

    if let Some(CliCommand::Share { command }) = &cli.command {
        // Every share verb asks the router, which owns the token store, over
        // this machine's own socket. A run outside every pane is never
        // refused. A run in a pane is refused while anyone is attached to that
        // pane's session from another machine.
        return share::run(command, InSessionContext::from_env()?.as_ref());
    }

    if let Some(CliCommand::Remote { command }) = &cli.command {
        // Every remote verb reads or writes the saved-server store on this
        // machine. It opens no connection and asks no running koshi.
        return remote_cmd::run(command);
    }

    if let Some(CliCommand::Doctor { format }) = &cli.command {
        // Doctor reads this machine's own files and asks the running router
        // one question. It dispatches no command and starts no router.
        return doctor::run(*format);
    }

    if let Some(CliCommand::ServeRouter {
        runtime_dir,
        wait_for_lock,
    }) = &cli.command
    {
        // This process becomes the router: it serves the control socket in
        // that directory until no session is left.
        let runtime_dir = match runtime_dir {
            Some(dir) => dir.clone(),
            None => ipc_client::runtime_dir()?,
        };
        return router::run_router(&runtime_dir, *wait_for_lock).map_err(|err| CliError::Runtime {
            detail: err.to_string(),
        });
    }

    if let Some(CliCommand::ServeSession {
        session_id,
        session_name,
        runtime_dir,
        profile,
        allow_other_users,
        resume,
        supervisor_token,
        supervisor_pid,
    }) = &cli.command
    {
        // This process becomes one session's server: the router started it
        // and gave it the identity to seed the session under, or the image it
        // replaces started it and gave it the state to come up from.
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
            resume.as_deref(),
            supervisor_token.as_deref(),
            *supervisor_pid,
        )
        .map_err(|err| CliError::Runtime {
            detail: err.to_string(),
        });
    }

    if let Some(CliCommand::ResumeSupport) = &cli.command {
        // A session server about to replace its own image runs the newly
        // installed binary this way, and reads this line to learn whether that
        // binary can take its carried state back.
        println!(
            "{}",
            serde_json::to_string(&ResumeSupport::of_this_build())
                .expect("a pair of numbers always encodes")
        );
        return Ok(());
    }

    if let Some(CliCommand::ServePtySupervisor {
        session_id,
        token,
        runtime_dir,
    }) = &cli.command
    {
        // This process becomes the holder of one session's panes: the session
        // server started it and gave it the session to serve and the secret a
        // link presents.
        let runtime_dir = match runtime_dir {
            Some(dir) => dir.clone(),
            None => ipc_client::runtime_dir()?,
        };
        return pty_supervisor::run_pty_supervisor(
            &runtime_dir,
            *session_id,
            ConnectionToken::new(token.clone()),
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

    if let Some(CliCommand::Version { format }) = &cli.command {
        // This program's own build. Nothing is asked over a socket.
        print!(
            "{}",
            output::render_client_version(&version::ClientVersion::of_this_build(), *format)
        );
        return Ok(());
    }

    if let Some(CliCommand::ServerVersion { session, format }) = &cli.command {
        // Each koshi server names its own build in its greeting; this
        // dispatches no command. The rows print whether or not every server
        // answered, and the exit code carries the gap.
        let rows = version::server_version_rows(session.as_ref())?;
        print!("{}", output::render_server_versions(&rows, *format));
        return match version::unreachable_servers(&rows) {
            Some(error) => Err(error),
            None => Ok(()),
        };
    }

    if let Some(command) = cli
        .command
        .as_ref()
        .filter(|command| command.is_discovery())
    {
        // The discovery queries read every running session's state and render
        // it here. They dispatch no command and never enter the routing layer
        // the action verbs use.
        return run_discovery(command, cli.remote.as_deref());
    }

    if let Some(CliCommand::Debug { command }) = &cli.command {
        return run_debug(command);
    }

    if let Some(CliCommand::KillSession { session }) = &cli.command {
        return finish_command(session_control::kill_session(session.as_ref())?);
    }

    if cli.headless {
        // The session is created and left running with nothing attached. Its
        // id prints as `[SESSION ID]: <id>` on standard output.
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
        // The update offer runs before the terminal enters raw mode, and reads
        // its answer from plain standard input. A failure never blocks the
        // launch.
        updater::maybe_prompt_startup_update();
        return koshi_client::app::run(cli.profile.as_deref());
    }

    // The in-session identity is read before any session verb dispatches, so a
    // broken pane environment reports itself rather than a missing daemon.
    let in_session = InSessionContext::from_env()?;

    // Attach is not an action verb, so it dispatches here rather than through
    // the routing layer. Typed inside a pane it moves that pane's client to
    // the named session; typed outside one it joins that session in this
    // terminal.
    if let Some(CliCommand::Attach { session, save_as }) = &cli.command {
        // With `--remote` the session is resolved and joined on the named
        // machine; a pane identity on this one is not read.
        if let Some(server) = &cli.remote {
            return attach::run_remote(server, save_as.as_deref(), session.as_deref());
        }
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

    if !is_action {
        return Err(CliError::IpcUnavailable {
            detail: "this command is not served over the control socket yet".to_string(),
        });
    }
    let cli_command = cli
        .command
        .as_ref()
        .expect("an action verb is always a parsed subcommand");

    // With `--remote` the target is picked from the sessions on the named
    // machine; the pane identity on this one is not read.
    let result = match &cli.remote {
        Some(server) => targeting::submit_remote(server, cli_command, new_pane_direction)?,
        None => match targeting::route(cli_command, in_session.as_ref())? {
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
                ipc_client::submit_external(session, cli_command.source_client(), command)?
            }
        },
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
///
/// `list-sessions` also lists the sessions on the saved servers: a bare one
/// sweeps every saved server and appends each session that answered, named
/// under its server in the `server` column; `--remote <server>` lists that one
/// server's sessions alone. A saved server that refused the secret, did not
/// answer, or pins no certificate yet is named on stderr and its sessions are
/// left out; only a session on this machine that could not answer fails the
/// listing.
fn run_discovery(command: &CliCommand, remote: Option<&str>) -> Result<(), CliError> {
    if let (CliCommand::ListSessions { format }, Some(server)) = (command, remote) {
        let arg = remote_client::resolve_server(server)?;
        let (mut link, _) =
            remote_client::connect_saved(&arg, None, Some(remote_client::REPLY_WAIT))?;
        let listed = remote_client::list_remote_sessions(&mut link)?;
        let label = arg.label();
        let rows: Vec<SessionRow> = listed
            .into_iter()
            .map(|row| SessionRow::new(row.id, &row.name, Some(label.clone())))
            .collect();
        print!("{}", output::render_sessions(&rows, *format));
        return Ok(());
    }

    let runtime_dir = ipc_client::runtime_dir()?;
    let found = targeting::scope_sessions(&runtime_dir, command.discovery_session())?;
    let sessions = found.sessions.as_slice();

    let rendered = match command {
        CliCommand::ListSessions { format } => {
            let mut rows = discovery::session_rows(sessions);
            for reach in remote_client::reach_all(REACH_WAIT) {
                match reach {
                    Reach::Reached {
                        server,
                        rows: listed,
                    } => {
                        rows.extend(
                            listed.into_iter().map(|row| {
                                SessionRow::new(row.id, &row.name, Some(server.clone()))
                            }),
                        );
                    }
                    Reach::Refused { server } => eprintln!(
                        "koshi: {server}: the saved secret was refused; \
                         run `koshi remote set-secret {server}`"
                    ),
                    Reach::CertificateChanged { server, detail } => {
                        eprintln!("koshi: {server}: {detail} its sessions are not listed");
                    }
                    Reach::Unreachable { server } => {
                        eprintln!("koshi: {server} did not answer; its sessions are not listed");
                    }
                    Reach::Unchecked { server } => eprintln!(
                        "koshi: {server} has no pinned certificate yet; \
                         run `koshi list-sessions --remote {server}` to connect and pin it"
                    ),
                }
            }
            output::render_sessions(&rows, *format)
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
                    session: session.to_string(),
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

    // Every discovery query other than an `inspect` is a listing.
    let listing = !matches!(command, CliCommand::Inspect { .. });
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
        DebugCommand::Events {
            since,
            filter,
            format,
        } => run_debug_events(*since, filter.as_deref(), *format),
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

/// Serve a `koshi debug events` from live state: find the sessions in scope,
/// ask each for its recent events, narrow them, and print them.
///
/// `since` keeps the events recorded within that much of now, and keeps every
/// event when it reaches back further than the clock can represent. `filter`
/// keeps the events whose name contains that text, matched ignoring case. Both
/// absent keeps every event the session remembers.
///
/// A session that refuses the request fails the command before anything
/// prints; a session that was listening but could not be probed fails it after
/// everything prints.
fn run_debug_events(
    since: Option<Duration>,
    filter: Option<&str>,
    format: FormatArg,
) -> Result<(), CliError> {
    let runtime_dir = ipc_client::runtime_dir()?;
    let found = targeting::scope_sessions(&runtime_dir, None)?;
    let oldest_kept = output::oldest_kept(SystemTime::now(), since);

    let sessions = found
        .sessions
        .iter()
        .map(|overview| {
            let events = ipc_client::fetch_recent_events(&runtime_dir, overview.session.id)?;
            Ok(output::SessionEvents {
                session: overview.session.id,
                name: overview.session.name.clone(),
                events: output::narrow(events, oldest_kept, filter),
            })
        })
        .collect::<Result<Vec<_>, CliError>>()?;
    print!("{}", output::render_recent_events(&sessions, format));

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
