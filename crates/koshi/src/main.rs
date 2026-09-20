//! The `koshi` binary entrypoint.

use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use clap::Parser;
use koshi::cli::{
    parse_session_reference, ActionsCommand, Cli, CliCommand, DebugCommand, InspectTarget,
    KeysCommand, OutputFormat, TabReference,
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
use koshi_link::remote_client::{self, Reach, REACH_TIMEOUT_DURATION};

const CLI_PARSER_STACK_SIZE_BYTES: usize = 2 * 1024 * 1024;

fn main() -> ExitCode {
    // Usage errors print through clap and exit 2; --help/--version exit 0.
    let cli = parse_cli_arguments();

    // A failure prints `koshi: <error>` on standard error before the process
    // exits with that error's code.
    let cli_exit_code = match run_cli_invocation(&cli) {
        Ok(()) => CliExitCode::Success,
        Err(cli_error) => {
            eprintln!("koshi: {cli_error}");
            CliExitCode::from(&cli_error)
        }
    };

    // Exit codes are 0..=4, always in u8 range.
    ExitCode::from(cli_exit_code.get_exit_code() as u8)
}

/// Parse command-line arguments on a dedicated stack and return the typed CLI.
/// Exit with code 1 when the parser thread cannot start or complete.
fn parse_cli_arguments() -> Cli {
    let parser_thread = match std::thread::Builder::new()
        .name("koshi-cli-parser".to_string())
        .stack_size(CLI_PARSER_STACK_SIZE_BYTES)
        .spawn(Cli::parse)
    {
        Ok(parser_thread) => parser_thread,
        Err(spawn_error) => {
            eprintln!("koshi: failed to start CLI parser: {spawn_error}");
            std::process::exit(CliExitCode::RuntimeAction.get_exit_code());
        }
    };

    match parser_thread.join() {
        Ok(cli) => cli,
        Err(_) => {
            eprintln!("koshi: CLI parser thread failed");
            std::process::exit(CliExitCode::RuntimeAction.get_exit_code());
        }
    }
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
fn run_cli_invocation(cli: &Cli) -> Result<(), CliError> {
    // `apply_beta_gate` sets the process-wide flag every `#[beta_feature]`
    // entry point reads. It runs before any verb dispatches, so one
    // `allow-beta-features` answer covers the CLI verbs and the interactive
    // launch alike.
    let app_config_layer = config::load_app_layer();
    config::apply_beta_gate(app_config_layer.clone());

    // `layout.new-pane-direction` from this machine's `koshi.kdl`. A
    // pane-opening verb given no `--direction` splits toward it; the session
    // holds no direction of its own.
    let new_pane_direction = config::resolve_new_pane_direction(app_config_layer);

    // The action verb is classified before routing. The command that travels
    // the socket is built after routing resolves the targets.
    let is_action = cli.command.as_ref().is_some_and(CliCommand::is_action_verb);

    // `--remote` runs with `attach`, with `list-sessions`, and with an action
    // verb. Every other verb, `--headless`, and a bare `koshi --remote
    // <server>` are refused.
    if cli.remote_server_reference.is_some()
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
        return config_command::run_config_command(command);
    }

    if let Some(CliCommand::Share { command }) = &cli.command {
        // Every share verb asks the router, which owns the token store, over
        // this machine's own socket. A run outside every pane is never
        // refused. A run in a pane is refused while anyone is attached to that
        // pane's session from another machine.
        return share::run_share_command(command, InSessionContext::from_env()?.as_ref());
    }

    if let Some(CliCommand::Remote { command }) = &cli.command {
        // Every remote verb reads or writes the saved-server store on this
        // machine. It opens no connection and asks no running koshi.
        return remote_cmd::run_remote_command(command);
    }

    if let Some(CliCommand::Doctor { output_format }) = &cli.command {
        // Doctor reads this machine's own files and asks the running router
        // one question. It dispatches no command and starts no router.
        return doctor::run_doctor_checks(*output_format);
    }

    if let Some(CliCommand::ServeRouter {
        runtime_directory,
        should_wait_for_lock,
    }) = &cli.command
    {
        // This process becomes the router: it serves the control socket in
        // that directory until no session is left.
        let runtime_directory = match runtime_directory {
            Some(runtime_directory_path) => runtime_directory_path.clone(),
            None => ipc_client::resolve_runtime_directory()?,
        };
        return router::run_router(&runtime_directory, *should_wait_for_lock).map_err(
            |runtime_error| CliError::Runtime {
                detail: runtime_error.to_string(),
            },
        );
    }

    if let Some(CliCommand::ServeSession {
        session_id,
        session_name,
        runtime_directory,
        profile_name,
        should_allow_other_users,
        resume_state_path,
        supervisor_token,
        supervisor_pid,
    }) = &cli.command
    {
        // This process becomes one session's server: the router started it
        // and gave it the identity to seed the session under, or the image it
        // replaces started it and gave it the state to come up from.
        let runtime_directory = match runtime_directory {
            Some(runtime_directory_path) => runtime_directory_path.clone(),
            None => ipc_client::resolve_runtime_directory()?,
        };
        return session_server::run_session_server(
            &runtime_directory,
            *session_id,
            session_name.clone(),
            profile_name.as_deref(),
            should_allow_other_users.then_some(true),
            resume_state_path.as_deref(),
            supervisor_token.as_deref(),
            *supervisor_pid,
        )
        .map_err(|runtime_error| CliError::Runtime {
            detail: runtime_error.to_string(),
        });
    }

    if let Some(CliCommand::ResumeSupport) = &cli.command {
        // A session server about to replace its own image runs the newly
        // installed binary this way, and reads this line to learn whether that
        // binary can take its carried state back.
        println!(
            "{}",
            serde_json::to_string(&ResumeSupport::from_current_build())
                .expect("a pair of numbers always encodes")
        );
        return Ok(());
    }

    if let Some(CliCommand::ServePtySupervisor {
        session_id,
        supervisor_token,
        runtime_directory,
    }) = &cli.command
    {
        // This process becomes the holder of one session's panes: the session
        // server started it and gave it the session to serve and the secret a
        // link presents.
        let runtime_directory = match runtime_directory {
            Some(runtime_directory_path) => runtime_directory_path.clone(),
            None => ipc_client::resolve_runtime_directory()?,
        };
        return pty_supervisor::run_pty_supervisor(
            &runtime_directory,
            *session_id,
            ConnectionToken::from_secret(supervisor_token.clone()),
        )
        .map_err(|runtime_error| CliError::Runtime {
            detail: runtime_error.to_string(),
        });
    }

    if let Some(CliCommand::Update) = &cli.command {
        // `update` runs locally: it talks to GitHub and the local filesystem,
        // not the session daemon.
        return updater::run_update_command();
    }

    if let Some(CliCommand::Version { output_format }) = &cli.command {
        // This program's own build. Nothing is asked over a socket.
        print!(
            "{}",
            output::render_client_version(
                &version::ClientVersion::build_client_version(),
                *output_format,
            )
        );
        return Ok(());
    }

    if let Some(CliCommand::ServerVersion {
        session_reference,
        output_format,
    }) = &cli.command
    {
        // Each koshi server names its own build in its greeting; this
        // dispatches no command. The rows print whether or not every server
        // answered, and the exit code carries the gap.
        let list_server_version_rows =
            version::list_server_version_rows(session_reference.as_ref())?;
        print!(
            "{}",
            output::render_server_versions(&list_server_version_rows, *output_format)
        );
        return match version::build_unreachable_server_error(&list_server_version_rows) {
            Some(unreachable_server_error) => Err(unreachable_server_error),
            None => Ok(()),
        };
    }

    if let Some(command) = cli
        .command
        .as_ref()
        .filter(|command| command.is_discovery_query())
    {
        // The discovery queries read every running session's state and render
        // it here. They dispatch no command and never enter the routing layer
        // the action verbs use.
        return run_discovery(command, cli.remote_server_reference.as_deref());
    }

    if let Some(CliCommand::Debug { command }) = &cli.command {
        return run_debug(command);
    }

    if let Some(CliCommand::KillSession { session_reference }) = &cli.command {
        return render_command_result(session_control::kill_session(session_reference.as_ref())?);
    }

    if cli.is_headless {
        // The session is created and left running with nothing attached. Its
        // id prints as `[SESSION ID]: <id>` on standard output.
        let runtime_directory = ipc_client::resolve_runtime_directory()?;
        let session_id = session_control::request_headless_session(
            &runtime_directory,
            cli.profile_name.as_deref(),
            cli.should_allow_other_users.then_some(true),
        )?;
        println!("[SESSION ID]: {session_id}");
        return Ok(());
    }

    if cli.is_interactive_launch() {
        // The update offer runs before the terminal enters raw mode, and reads
        // its answer from plain standard input. A failure never blocks the
        // launch.
        updater::maybe_prompt_startup_update();
        return koshi_client::app::run_default_client(cli.profile_name.as_deref());
    }

    // The in-session identity is read before any session verb dispatches, so a
    // broken pane environment reports itself rather than a missing daemon.
    let in_session_context = InSessionContext::from_env()?;

    // Attach is not an action verb, so it dispatches here rather than through
    // the routing layer. Typed inside a pane it moves that pane's client to
    // the named session; typed outside one it joins that session in this
    // terminal.
    if let Some(CliCommand::Attach {
        session_argument,
        save_as,
    }) = &cli.command
    {
        // With `--remote` the session is resolved and joined on the named
        // machine; a pane identity on this one is not read.
        if let Some(server_reference) = &cli.remote_server_reference {
            return attach::attach_remote_session(
                server_reference,
                save_as.as_deref(),
                session_argument.as_deref(),
            );
        }
        return match in_session_context.as_ref() {
            Some(in_session_context) => render_command_result(attach::switch_in_session(
                in_session_context,
                session_argument.as_deref(),
            )?),
            None => attach::attach_selected_session(session_argument.as_deref()),
        };
    }

    // Detach is not an action verb, so it dispatches here rather than through
    // the routing layer. Success prints nothing; a detach the session refuses
    // comes back as a rejected command.
    if let Some(CliCommand::Detach {
        detach_target,
        should_detach_all_clients,
    }) = &cli.command
    {
        return match (detach_target.as_deref(), should_detach_all_clients) {
            (None, false) => {
                let in_session_context = in_session_context
                    .as_ref()
                    .ok_or_else(|| CliError::InvalidArgs {
                    detail: "bare koshi detach only works inside a koshi session; outside one use koshi detach <id>".to_string(),
                    })?;
                render_command_result(ipc_client::submit_in_session_command(
                    in_session_context,
                    Command::Detach(DetachArgs { client_id: None }),
                )?)
            }
            (Some(target_text), false) => {
                render_command_result(session_control::detach_client_or_session(target_text)?)
            }
            (None, true) => {
                let in_session_context = in_session_context
                    .as_ref()
                    .ok_or_else(|| CliError::InvalidArgs {
                    detail: "bare koshi detach --all only works inside a koshi session; outside one use koshi detach --all <session>".to_string(),
                    })?;
                render_command_result(ipc_client::submit_in_session_command(
                    in_session_context,
                    Command::DetachAll,
                )?)
            }
            (Some(target_text), true) => {
                let target_session_reference =
                    parse_session_reference(target_text).map_err(|parse_error_detail| {
                        CliError::InvalidArgs {
                            detail: parse_error_detail,
                        }
                    })?;
                render_command_result(session_control::detach_all_session(Some(
                    &target_session_reference,
                ))?)
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
    let command_result = match &cli.remote_server_reference {
        Some(server_reference) => {
            targeting::submit_remote(server_reference, cli_command, new_pane_direction)?
        }
        None => match targeting::resolve_command_route(cli_command, in_session_context.as_ref())? {
            Route::InSession(resolved_targets) => {
                let in_session_context =
                    in_session_context.expect("an in-session route needs the pane identity");
                let (_, action_command) = cli_command
                    .build_action_command(&resolved_targets, new_pane_direction)
                    .expect("checked to be an action verb above");
                ipc_client::submit_in_session_command(&in_session_context, action_command)?
            }
            Route::External {
                session_id,
                targets: resolved_targets,
            } => {
                let (_, action_command) = cli_command
                    .build_action_command(&resolved_targets, new_pane_direction)
                    .expect("checked to be an action verb above");
                ipc_client::submit_external_command(
                    session_id,
                    cli_command.get_source_client_id(),
                    action_command,
                )?
            }
        },
    };

    render_command_result(command_result)
}

/// Print an applied command's created ids, or surface its rejection.
fn render_command_result(command_result: CommandResult) -> Result<(), CliError> {
    match command_result {
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
fn run_discovery(command: &CliCommand, remote_server: Option<&str>) -> Result<(), CliError> {
    if let (CliCommand::ListSessions { output_format }, Some(server)) = (command, remote_server) {
        let saved_server_argument = remote_client::resolve_server(server)?;
        let (mut remote_link, _) = remote_client::connect_saved_server(
            &saved_server_argument,
            None,
            Some(remote_client::REPLY_TIMEOUT_DURATION),
        )?;
        let remote_session_rows = remote_client::list_remote_sessions(&mut remote_link)?;
        let server_label = saved_server_argument.format_server_label();
        let session_rows: Vec<SessionRow> = remote_session_rows
            .into_iter()
            .map(|remote_session_row| {
                SessionRow::from_session(
                    remote_session_row.session_id,
                    &remote_session_row.session_name,
                    Some(server_label.clone()),
                )
            })
            .collect();
        print!("{}", output::render_sessions(&session_rows, *output_format));
        return Ok(());
    }

    let runtime_directory = ipc_client::resolve_runtime_directory()?;
    let discovered_sessions = targeting::resolve_session_scope(
        &runtime_directory,
        command.get_discovery_session_reference(),
    )?;
    let session_overviews = discovered_sessions.sessions.as_slice();

    let rendered_output = match command {
        CliCommand::ListSessions { output_format } => {
            let mut session_rows = discovery::build_session_rows(session_overviews);
            for reach in remote_client::reach_all_saved_servers(REACH_TIMEOUT_DURATION) {
                match reach {
                    Reach::Reached {
                        server_label,
                        session_rows: remote_session_rows,
                    } => {
                        session_rows.extend(remote_session_rows.into_iter().map(
                            |remote_session_row| {
                                SessionRow::from_session(
                                    remote_session_row.session_id,
                                    &remote_session_row.session_name,
                                    Some(server_label.clone()),
                                )
                            },
                        ));
                    }
                    Reach::Refused { server_label } => eprintln!(
                        "koshi: {server_label}: the saved secret was refused; \
                         run `koshi remote set-secret {server_label}`"
                    ),
                    Reach::CertificateChanged {
                        server_label,
                        certificate_error_detail,
                    } => {
                        eprintln!(
                            "koshi: {server_label}: {certificate_error_detail} its sessions are not listed"
                        );
                    }
                    Reach::Unreachable { server_label } => {
                        eprintln!(
                            "koshi: {server_label} did not answer; its sessions are not listed"
                        );
                    }
                    Reach::Unchecked { server_label } => eprintln!(
                        "koshi: {server_label} has no pinned certificate yet; \
                         run `koshi list-sessions --remote {server_label}` to connect and pin it"
                    ),
                }
            }
            output::render_sessions(&session_rows, *output_format)
        }
        CliCommand::ListTabs { output_format, .. } => output::render_tabs(
            &discovery::build_tab_rows(session_overviews),
            *output_format,
        ),
        CliCommand::ListPanes { output_format, .. } => output::render_panes(
            &discovery::build_pane_rows(session_overviews),
            *output_format,
        ),
        CliCommand::ListClients { output_format, .. } => output::render_clients(
            &discovery::build_client_rows(session_overviews),
            *output_format,
        ),
        CliCommand::Inspect { inspect_target } => match inspect_target {
            InspectTarget::Session {
                session_reference,
                output_format,
            } => {
                // The scope already resolved the named session, so the census
                // holds that one session; an empty census reports it as not
                // found.
                let session_overview =
                    session_overviews
                        .first()
                        .ok_or_else(|| CliError::SessionNotFound {
                            session_name: session_reference.to_string(),
                        })?;
                output::render_session(&session_overview.session, *output_format)
            }
            InspectTarget::Tab {
                tab_reference,
                output_format,
            } => {
                let tab_id = targeting::resolve_tab_reference(&discovered_sessions, tab_reference)?;
                output::render_tab(
                    &discovery::find_tab(&discovered_sessions, tab_id)?,
                    *output_format,
                )
            }
            InspectTarget::Pane {
                pane_id,
                output_format,
            } => output::render_pane(
                &discovery::find_pane(&discovered_sessions, *pane_id)?,
                *output_format,
            ),
            InspectTarget::Client {
                client_id,
                output_format,
            } => output::render_client(
                &discovery::find_client(&discovered_sessions, *client_id)?,
                *output_format,
            ),
        },
        _ => unreachable!("checked to be a discovery query above"),
    };
    print!("{rendered_output}");

    // Every discovery query other than an `inspect` is a listing.
    let is_listing = !matches!(command, CliCommand::Inspect { .. });
    match discovered_sessions.incomplete_listing() {
        Some(incomplete_listing_error) if is_listing => Err(incomplete_listing_error),
        _ => Ok(()),
    }
}

/// Serve a `koshi debug` dump from live state.
fn run_debug(command: &DebugCommand) -> Result<(), CliError> {
    match command {
        DebugCommand::DumpState { output_format } => run_dump_state(*output_format),
        DebugCommand::DumpLayout {
            tab_reference,
            output_format,
        } => run_dump_layout(tab_reference.as_ref(), *output_format),
        DebugCommand::Events {
            event_age_limit,
            event_name_filter,
            output_format,
        } => run_debug_events(
            *event_age_limit,
            event_name_filter.as_deref(),
            *output_format,
        ),
    }
}

/// Print every running session's full record, with each pane's command
/// arguments hidden.
///
/// Prints every session it reached, then fails when one could not answer.
fn run_dump_state(output_format: OutputFormat) -> Result<(), CliError> {
    let runtime_directory = ipc_client::resolve_runtime_directory()?;
    let mut discovered_sessions = discovery::fetch_all_session_overviews(&runtime_directory);
    discovery::redact_pane_commands(&mut discovered_sessions.sessions);
    print!(
        "{}",
        output::render_dump_state(&discovered_sessions.sessions, output_format)
    );
    match discovered_sessions.incomplete_listing() {
        Some(incomplete_listing_error) => Err(incomplete_listing_error),
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
fn run_dump_layout(
    tab_reference: Option<&TabReference>,
    output_format: OutputFormat,
) -> Result<(), CliError> {
    let runtime_directory = ipc_client::resolve_runtime_directory()?;
    let discovered_sessions = targeting::resolve_session_scope(&runtime_directory, None)?;

    let layouts = match tab_reference {
        Some(tab_reference) => {
            let tab_id = targeting::resolve_tab_reference(&discovered_sessions, tab_reference)?;
            let session_id = discovery::find_tab(&discovered_sessions, tab_id)?.session_id;
            vec![ipc_client::fetch_layout(
                &runtime_directory,
                session_id,
                Some(tab_id),
            )?]
        }
        None => discovered_sessions
            .sessions
            .iter()
            .map(|session_overview| {
                ipc_client::fetch_layout(
                    &runtime_directory,
                    session_overview.session.session_id,
                    None,
                )
            })
            .collect::<Result<Vec<_>, CliError>>()?,
    };
    print!("{}", output::render_layouts(&layouts, output_format));

    match discovered_sessions.incomplete_listing() {
        Some(incomplete_listing_error) => Err(incomplete_listing_error),
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
    since_duration: Option<Duration>,
    event_name_filter: Option<&str>,
    output_format: OutputFormat,
) -> Result<(), CliError> {
    let runtime_directory = ipc_client::resolve_runtime_directory()?;
    let discovered_sessions = targeting::resolve_session_scope(&runtime_directory, None)?;
    let oldest_event_time = output::compute_oldest_event_time(SystemTime::now(), since_duration);

    let session_events = discovered_sessions
        .sessions
        .iter()
        .map(|session_overview| {
            let recent_events = ipc_client::fetch_recent_events(
                &runtime_directory,
                session_overview.session.session_id,
            )?;
            Ok(output::SessionEvents {
                session_id: session_overview.session.session_id,
                session_name: session_overview.session.session_name.clone(),
                recent_events: output::filter_recent_events(
                    recent_events,
                    oldest_event_time,
                    event_name_filter,
                ),
            })
        })
        .collect::<Result<Vec<_>, CliError>>()?;
    print!(
        "{}",
        output::render_recent_events(&session_events, output_format)
    );

    match discovered_sessions.incomplete_listing() {
        Some(incomplete_listing_error) => Err(incomplete_listing_error),
        None => Ok(()),
    }
}

/// Serve a `koshi actions` query from the static action table: print the
/// rendered answer, or report an unknown action name.
fn run_actions(command: &ActionsCommand) -> Result<(), CliError> {
    match command {
        ActionsCommand::List { output_format } => {
            print!("{}", output::render_actions_list(*output_format));
            Ok(())
        }
        ActionsCommand::Explain {
            action_reference_text,
            output_format,
        } => match output::render_action_explain(action_reference_text, *output_format) {
            Some(rendered_output) => {
                print!("{rendered_output}");
                Ok(())
            }
            None => Err(CliError::UnknownAction {
                action_name: action_reference_text.clone(),
            }),
        },
    }
}

/// Serve a `koshi keys` query from the offline keymap view: the user's
/// keybinding file folded onto the built-in defaults. The running session's
/// own layers (`session`, `layout`) arrive with the IPC client.
fn run_keys_query(command: &KeysCommand) -> Result<(), CliError> {
    match command {
        KeysCommand::List {
            input_mode_name,
            scope,
            is_recommended,
            output_format,
        } => {
            if *is_recommended {
                print!("{}", output::render_keys_recommended(*output_format));
                return Ok(());
            }
            let keymap_view = keymap::load_keymap_view();
            warn_keymap_reverted(&keymap_view);
            print!(
                "{}",
                output::render_keys_list(
                    &keymap_view,
                    input_mode_name.as_deref(),
                    *scope,
                    *output_format,
                )
            );
            Ok(())
        }
        KeysCommand::Describe {
            key_sequence_text,
            output_format,
        } => {
            let keymap_view = keymap::load_keymap_view();
            warn_keymap_reverted(&keymap_view);
            match output::render_keys_describe(&keymap_view, key_sequence_text, *output_format) {
                Ok(Some(rendered_output)) => {
                    print!("{rendered_output}");
                    Ok(())
                }
                Ok(None) => Err(CliError::UnboundKey {
                    sequence: key_sequence_text.clone(),
                }),
                Err(parse_error_detail) => Err(CliError::InvalidArgs {
                    detail: parse_error_detail,
                }),
            }
        }
        KeysCommand::Conflicts { output_format } => {
            // An ignored file is part of the rendered answer itself, so no
            // stderr note is needed here.
            let keymap_view = keymap::load_keymap_view();
            print!(
                "{}",
                output::render_keys_conflicts(&keymap_view, *output_format)
            );
            Ok(())
        }
        KeysCommand::Validate {
            keybinding_file_path,
            output_format,
        } => {
            let validation_outcome =
                keymap::validate_keymap_file(keybinding_file_path).map_err(|read_error| {
                    CliError::InvalidArgs {
                        detail: format!(
                            "cannot read {}: {read_error}",
                            keybinding_file_path.display()
                        ),
                    }
                })?;
            print!(
                "{}",
                output::render_keys_validate(&validation_outcome, *output_format)
            );
            if output::does_validation_apply(&validation_outcome) {
                Ok(())
            } else {
                Err(CliError::InvalidKeymapFile {
                    keymap_file_path: keybinding_file_path.display().to_string(),
                })
            }
        }
    }
}

/// Warn on stderr when the user's keybinding file exists but was not
/// admitted, so the defaults-only answer on stdout is not mistaken for the
/// file's contents.
fn warn_keymap_reverted(keymap_view: &KeymapView) {
    if let Some(keymap_error) = &keymap_view.file_error_message {
        eprintln!("koshi: keybinding file ignored: {keymap_error}");
    } else if keymap_view.is_reverted_to_defaults {
        eprintln!(
            "koshi: keybinding file not applied (conflicts); showing built-in defaults — \
             run `koshi keys conflicts` for details"
        );
    }
}
