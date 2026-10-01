//! The CLI side of the control socket: find the session's endpoint, connect,
//! open with Hello, submit one command, and read back its result.
//!
//! The endpoint file in the private runtime directory advertises the
//! session's socket address and connection token; reading it is the
//! same-user proof the Hello presents. The Hello and the command are written
//! back to back before either reply is read: a submission costs one round
//! trip, whether or not it names a target client. A session that settles on a
//! version this build does not speak reads both, and the caller is refused on
//! the Hello answer and never given a command result.
//!
//! A session another local user started advertises no endpoint file here. It
//! is found by the name of its socket in the machine-wide shared directory
//! instead, and reached with the empty token that session asks another user
//! for. A lookup by id reads only the path of that id in each user's folder. A
//! listing reads every entry, and lists at most 256 sessions of one owner on
//! Unix and 256 in all on Windows. It says how many more it held, and which
//! read failed. Both stop after 65,536 directory entries, or on Unix after 256
//! user folders, and then report the shared directory as unread.
//!
//! Asking a running session to restart is one more such exchange. A session
//! that is not listening reads as `NotRunning`, not as an error. A session
//! of this user's whose endpoint file is gone while it replaces its process
//! image reads as restarting instead.

#[cfg(unix)]
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use koshi_core::command::{Command, CommandEnvelope, CommandResult, CommandSource};
use koshi_core::discovery::SessionOverview;
use koshi_core::event::RejectReason;
use koshi_core::ids::{ClientId, CommandId, SessionId, TabId};
use koshi_core::recent_event::RecentEvent;
#[cfg(windows)]
use koshi_ipc::endpoint::resolve_advertisement_marker_path;
use koshi_ipc::endpoint::{
    compute_socket_address, is_replacing_its_image, resolve_resume_file_path, EndpointFile,
    ServerProgramFile, RESTART_WINDOW_DURATION, RESUME_SUFFIX,
};
use koshi_ipc::error::IpcError;
use koshi_ipc::layout::SessionLayout;
use koshi_ipc::protocol::{
    ConnectionToken, IncomingResponse, IpcRequest, IpcRequestKind, IpcResult,
};
use koshi_ipc::transport::{compute_time_left_until, Connection, CONNECT_WAIT_DURATION};
use uuid::Uuid;

use crate::error::CliError;
use crate::in_session::InSessionContext;
use crate::server_build::{find_refusing_server_build, RefusingServerBuild};
use crate::talk::{self, build_ipc_unavailable_error, build_peer_refusal_error};

/// How long a session has to answer once it is asked: a session another
/// local user started that `koshi list-sessions` asks to describe itself, and
/// any session `koshi version` asks for its build.
pub const SESSION_ANSWER_TIMEOUT_DURATION: Duration = Duration::from_secs(5);

/// The most sessions [`ask_sessions_at_once`] asks at the same time.
pub const MAX_SESSIONS_ASKED_AT_ONCE: usize = 16;

/// How long a wait for a session or the router that is replacing its own
/// process image pauses between reads of its endpoint file: 25 ms.
pub const RESTART_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(25);

/// How long the CLI waits, after a server of this user's refused this build's
/// protocol version, for that server to start replacing its own process
/// image: 3 s.
pub const REFUSED_SERVER_RESTART_START_WAIT_DURATION: Duration = Duration::from_secs(3);

/// The most sessions [`list_foreign_sessions`] lists for one owner on Unix,
/// across every folder of the shared directory.
pub const MAX_SHARED_SESSION_COUNT_PER_OWNER: usize = 256;

/// The most sessions [`list_foreign_sessions`] lists on Windows, across every
/// user: a marker file names no owner.
pub const MAX_SHARED_MARKER_COUNT: usize = 256;

/// The most directory entries one scan of the shared directory goes through:
/// the entries of the shared directory, and on Unix those of each user's
/// folder the scan opens. Reading one more stops [`list_foreign_sessions`] and
/// [`find_foreign_session_address`].
pub const MAX_SHARED_DIRECTORY_ENTRY_COUNT: usize = 65_536;

/// The most user folders one scan of the shared directory goes through on
/// Unix. Finding one more stops [`list_foreign_sessions`] and
/// [`find_foreign_session_address`].
pub const MAX_SHARED_USER_DIRECTORY_COUNT: usize = 256;

/// The private runtime directory holding every endpoint file, or
/// [`CliError::IpcUnavailable`] when the machine has none.
pub fn resolve_runtime_directory() -> Result<PathBuf, CliError> {
    koshi_paths::resolve_runtime_directory().ok_or_else(|| CliError::IpcUnavailable {
        detail: "no runtime directory found".to_string(),
    })
}

/// The shared directory holding the sessions other local users started, as
/// this user's `koshi.kdl` names it now:
/// [`find_shared_sessions_base_directory`](crate::config::find_shared_sessions_base_directory)
/// over the config directory `koshi_paths::resolve_config_directory` gives.
/// `None` while `allow-other-users` is off, and on a machine with no config
/// directory.
#[must_use]
pub fn resolve_shared_sessions_base_directory() -> Option<PathBuf> {
    crate::config::find_shared_sessions_base_directory(
        koshi_paths::resolve_config_directory().as_deref(),
    )
}

/// Submit `command` to the session this CLI runs inside and hand back the
/// dispatcher's result.
///
/// Reads the session's endpoint file, connects, writes the Hello and the
/// command back to back, and reads the two replies in order. A missing
/// endpoint file or a socket nothing listens on reports the session as not
/// running ([`CliError::SessionNotFound`]); every other failure to talk is
/// [`CliError::IpcUnavailable`]. The result itself — applied or rejected —
/// comes back for the caller to map to an exit code, with a rejection's hint
/// filtered by [`sanitize_reported_text`](koshi_core::text::sanitize_reported_text).
///
/// A session of this user's that refuses this build's protocol version is
/// asked again once it has restarted, as
/// [`run_session_exchange_with_restart_wait`] states.
pub fn submit_in_session_command(
    in_session_context: &InSessionContext,
    command: Command,
) -> Result<CommandResult, CliError> {
    submit_command_via_runtime_directory(&resolve_runtime_directory()?, in_session_context, command)
}

/// Submit `command` to the running session `session_id` as an external
/// invocation — a `koshi` command typed outside any pane, or inside a pane
/// but targeting another session. Same exchange and error mapping as
/// [`submit_in_session_command`]; the envelope's source is
/// [`CommandSource::ExternalCli`]: the runtime resolves its defaults through
/// the target session's acting client.
///
/// `client_id` is the client the command acts for, and rides on the source.
/// Naming one changes nothing about the exchange: the Hello and the command go
/// out back to back, and it costs one round trip either way.
pub fn submit_external_command(
    session_id: SessionId,
    client_id: Option<ClientId>,
    command: Command,
) -> Result<CommandResult, CliError> {
    submit_external_command_via_runtime_directory(
        &resolve_runtime_directory()?,
        resolve_shared_sessions_base_directory().as_deref(),
        session_id,
        client_id,
        command,
    )
}

/// [`submit_in_session_command`] against an explicit runtime directory: the whole
/// exchange, with the endpoint lookup rooted where the caller says.
fn submit_command_via_runtime_directory(
    runtime_directory: &Path,
    in_session_context: &InSessionContext,
    command: Command,
) -> Result<CommandResult, CliError> {
    run_session_exchange_with_restart_wait(
        runtime_directory,
        None,
        in_session_context.session_id,
        None,
        |endpoint_file| {
            let command_source = CommandSource::from_in_session_cli(
                in_session_context.session_id,
                in_session_context.client_id,
                in_session_context.pane_id,
                PathBuf::from(&endpoint_file.socket_address),
            );
            submit_command_envelope(
                endpoint_file,
                in_session_context.session_id,
                command_source,
                command.clone(),
                None,
            )
        },
    )
}

/// [`submit_external_command`] against an explicit runtime directory: the whole
/// exchange, with the endpoint lookup rooted where the caller says.
/// `client_id` rides on the source.
///
/// `shared_sessions_base_directory` is searched for `session_id` when
/// `runtime_directory` holds no endpoint file for it, through
/// [`load_session_endpoint`].
///
/// A session of this user's that refuses this build's protocol version is
/// asked again once it has restarted, as
/// [`run_session_exchange_with_restart_wait`] states.
pub fn submit_external_command_via_runtime_directory(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
    client_id: Option<ClientId>,
    command: Command,
) -> Result<CommandResult, CliError> {
    run_session_exchange_with_restart_wait(
        runtime_directory,
        shared_sessions_base_directory,
        session_id,
        None,
        |endpoint_file| {
            let command_source = CommandSource::from_external_cli(Some(session_id), client_id);
            submit_command_envelope(
                endpoint_file,
                session_id,
                command_source,
                command.clone(),
                None,
            )
        },
    )
}

/// Submit `command` to the session `session_id` at `endpoint` as an external
/// invocation acting for no client, in the exchange
/// [`submit_external_command`] makes.
///
/// With `answer_deadline`, the exchange ends by that moment, as
/// [`connect_to_session`] states. An exchange still unfinished then is
/// [`CliError::SessionAnswerTimedOut`]. With `None`, the exchange waits for
/// the answer however long it takes.
pub fn submit_external_command_to_endpoint(
    endpoint: &EndpointFile,
    session_id: SessionId,
    command: Command,
    answer_deadline: Option<Instant>,
) -> Result<CommandResult, CliError> {
    let command_source = CommandSource::from_external_cli(Some(session_id), None);
    submit_command_envelope(
        endpoint,
        session_id,
        command_source,
        command,
        answer_deadline,
    )
}

/// Fill a pane-creating command's unset working directory with this CLI
/// process's own, read here at send time. A command that already names a
/// directory is left alone, and every other command carries none.
fn apply_current_working_directory_to_command(mut command: Command) -> Command {
    let working_directory = match &mut command {
        Command::NewPane(new_pane_arguments) => &mut new_pane_arguments.working_directory,
        Command::NewTab(new_tab_arguments) => &mut new_tab_arguments.working_directory,
        Command::RunCommandPane(run_command_pane_arguments) => {
            &mut run_command_pane_arguments.working_directory
        }
        _ => return command,
    };
    if working_directory.is_none() {
        *working_directory = std::env::current_dir().ok();
    }
    command
}

/// One command submission over `endpoint`: connect, send the Hello and the
/// enveloped command, and read the command's result. A pane-creating command
/// with no directory of its own gets this process's current directory, and a
/// rejection's hint is filtered by
/// [`filter_rejection_hint`](crate::talk::filter_rejection_hint).
///
/// With `answer_deadline`, the exchange ends by that moment, as
/// [`connect_to_session`] states.
fn submit_command_envelope(
    endpoint: &EndpointFile,
    session_id: SessionId,
    command_source: CommandSource,
    command: Command,
    answer_deadline: Option<Instant>,
) -> Result<CommandResult, CliError> {
    let prepared_command = apply_current_working_directory_to_command(command);
    let command_envelope =
        CommandEnvelope::from_parts(CommandId::new(), command_source, prepared_command);
    let ipc_request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::SubmitCommand(Box::new(command_envelope)),
    };
    match exchange_session_request(endpoint, session_id, ipc_request, answer_deadline)? {
        IpcResult::CommandResult(command_result) => Ok(talk::filter_rejection_hint(command_result)),
        IpcResult::Error(refusal) => Err(build_peer_refusal_error(&refusal)),
        unexpected_result => {
            Err(talk::SESSION_PEER_WORDS.build_unexpected_reply_error(&unexpected_result))
        }
    }
}

/// Ask the session `session_id` listening at `socket_address` to describe itself, as
/// a session another local user started: the address is the one the shared
/// directory advertised, and the token presented is empty. The exchange ends
/// by `answer_deadline`, as [`fetch_session_overview_from_endpoint`] states.
pub fn fetch_foreign_session_overview(
    session_id: SessionId,
    socket_address: &str,
    answer_deadline: Instant,
) -> Result<SessionOverview, CliError> {
    fetch_session_overview_from_endpoint(
        &build_foreign_session_endpoint(socket_address.to_string()),
        session_id,
        Some(answer_deadline),
    )
}

/// Ask the session `session_id` at `endpoint` to describe itself, in one
/// Discovery exchange.
///
/// The answer passes through
/// [`filter_session_overview_text`](crate::discovery::filter_session_overview_text) before it
/// is handed back: every name, title, working directory and argv in it is
/// filtered, whichever session answered.
///
/// With `answer_deadline`, the connect and every write and read after it end
/// by that moment, as [`connect_to_session`] states. An exchange still
/// unfinished then is [`CliError::SessionAnswerTimedOut`]. With `None`, the
/// exchange waits for the answer however long it takes.
pub fn fetch_session_overview_from_endpoint(
    endpoint: &EndpointFile,
    session_id: SessionId,
    answer_deadline: Option<Instant>,
) -> Result<SessionOverview, CliError> {
    let ipc_request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Discovery,
    };
    match exchange_session_request(endpoint, session_id, ipc_request, answer_deadline)? {
        IpcResult::Overview(mut session_overview) => {
            crate::discovery::filter_session_overview_text(&mut session_overview);
            Ok(session_overview)
        }
        IpcResult::Error(refusal) => Err(build_peer_refusal_error(&refusal)),
        unexpected_result => {
            Err(talk::SESSION_PEER_WORDS.build_unexpected_reply_error(&unexpected_result))
        }
    }
}

/// Ask the running session `session_id` to describe its layout: each tab's
/// split tree, and the rectangles each viewing client solves it to
/// ([`SessionLayout`]). `tab` narrows the answer to one tab; absent, every
/// tab is described.
///
/// Naming a `tab` the session no longer holds is a target failure, not an
/// empty answer: the session describes no tab, and this reports the tab as
/// missing. Example: the tab closes between the caller resolving it and the
/// session answering.
///
/// A refusal carries the session's own sentence through.
///
/// `shared_sessions_base_directory` is searched for `session_id` when
/// `runtime_directory` holds no endpoint file for it, through
/// [`load_session_endpoint`].
///
/// A session of this user's that refuses this build's protocol version is
/// asked again once it has restarted, as
/// [`run_session_exchange_with_restart_wait`] states.
pub fn fetch_layout(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
    tab_id: Option<TabId>,
) -> Result<SessionLayout, CliError> {
    let ipc_request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Layout { tab_id },
    };
    let layout_result = run_session_exchange_with_restart_wait(
        runtime_directory,
        shared_sessions_base_directory,
        session_id,
        None,
        |session_endpoint| {
            exchange_session_request(session_endpoint, session_id, ipc_request.clone(), None)
        },
    )?;
    match layout_result {
        IpcResult::Layout(session_layout) => match tab_id {
            Some(tab_id) if session_layout.tabs.is_empty() => Err(CliError::CommandRejected {
                reason: RejectReason::TargetNotFound,
                help: Some(format!("no running session has tab {tab_id}")),
            }),
            _ => Ok(session_layout),
        },
        IpcResult::Error(refusal) => Err(build_peer_refusal_error(&refusal)),
        unexpected_result => {
            Err(talk::SESSION_PEER_WORDS.build_unexpected_reply_error(&unexpected_result))
        }
    }
}

/// Ask `session_id` for the events it published most recently, oldest first.
///
/// A refusal carries the session's own sentence through.
///
/// `shared_sessions_base_directory` is searched for `session_id` when
/// `runtime_directory` holds no endpoint file for it, through
/// [`load_session_endpoint`].
///
/// A session of this user's that refuses this build's protocol version is
/// asked again once it has restarted, as
/// [`run_session_exchange_with_restart_wait`] states.
pub fn fetch_recent_events(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
) -> Result<Vec<RecentEvent>, CliError> {
    let ipc_request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::RecentEvents,
    };
    let recent_events_result = run_session_exchange_with_restart_wait(
        runtime_directory,
        shared_sessions_base_directory,
        session_id,
        None,
        |session_endpoint| {
            exchange_session_request(session_endpoint, session_id, ipc_request.clone(), None)
        },
    )?;
    match recent_events_result {
        IpcResult::RecentEvents(recent_events) => Ok(recent_events),
        IpcResult::Error(refusal) => Err(build_peer_refusal_error(&refusal)),
        unexpected_result => {
            Err(talk::SESSION_PEER_WORDS.build_unexpected_reply_error(&unexpected_result))
        }
    }
}

/// The result of asking a running session to restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRestart {
    /// The session answered that it is replacing its own process image.
    Restarting,
    /// Nothing advertises that session, or nothing listens behind the address
    /// it advertises. Nothing restarted.
    NotRunning,
}

/// Ask the running session `session_id` to restart into the binary on disk.
///
/// Sends one Restart exchange, and one more after the restart wait stated
/// below, and never starts a session. Every pane, its child process, its
/// terminal and its scrollback stay as they are. A client that was attached
/// attaches again and finds the session it left.
///
/// A session that refuses the request gives what [`build_peer_refusal_error`]
/// builds from the sentence the session sent. A session from this build
/// refuses with `RequestFailed`, and a `v0.5.0-pr.1` session refuses with
/// `MalformedRequest`: both are [`CliError::IpcUnavailable`]. A session that
/// refuses this build's protocol version at the Hello gives
/// [`CliError::ProtocolVersionRefused`].
///
/// `shared_sessions_base_directory` is searched for `session_id` when
/// `runtime_directory` holds no endpoint file for it, through
/// [`load_session_endpoint`].
///
/// A session of this user's that refuses this build's protocol version is
/// asked again once it has restarted, as
/// [`run_session_exchange_with_restart_wait`] states. Example: a session that
/// restarted by itself into the program on disk is asked to restart once more,
/// and restarts into the same program.
pub fn restart_running_session(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
) -> Result<SessionRestart, CliError> {
    let ipc_request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Restart,
    };
    let restart_result = run_session_exchange_with_restart_wait(
        runtime_directory,
        shared_sessions_base_directory,
        session_id,
        None,
        |session_endpoint| {
            exchange_session_request(session_endpoint, session_id, ipc_request.clone(), None)
        },
    );
    match restart_result {
        Ok(IpcResult::Restarting) => Ok(SessionRestart::Restarting),
        Ok(IpcResult::Error(refusal)) => Err(build_peer_refusal_error(&refusal)),
        Ok(unexpected_result) => {
            Err(talk::SESSION_PEER_WORDS.build_unexpected_reply_error(&unexpected_result))
        }
        Err(CliError::SessionNotFound { .. }) => Ok(SessionRestart::NotRunning),
        Err(ipc_error) => Err(ipc_error),
    }
}

/// The build version the running session `session_id` reports in its Hello
/// answer.
///
/// `Ok(None)` means no session is running under that id. Sends nothing besides
/// the Hello. With `answer_deadline`, the connect, the write and the read end
/// by that moment, as [`connect_to_session`] states, and a failure once it is
/// reached is [`CliError::SessionAnswerTimedOut`].
///
/// `shared_sessions_base_directory` is searched for `session_id` when
/// `runtime_directory` holds no endpoint file for it, through
/// [`load_session_endpoint`].
pub fn find_running_session_version(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
    answer_deadline: Option<Instant>,
) -> Result<Option<String>, CliError> {
    match load_session_endpoint(
        runtime_directory,
        shared_sessions_base_directory,
        session_id,
    ) {
        Ok(session_endpoint) => {
            find_session_version_from_endpoint(&session_endpoint, session_id, answer_deadline)
        }
        Err(CliError::SessionNotFound { .. }) => Ok(None),
        Err(ipc_error) => Err(ipc_error),
    }
}

/// The build version the session `session_id` listening at `socket_address`
/// reports in its Hello answer, as a session another local user started: the
/// address is the one the shared directory advertised, and the token presented
/// is empty. Answers as [`find_running_session_version`] does, with
/// `answer_deadline` bounding the connect, the write and the read.
pub fn find_foreign_session_version(
    session_id: SessionId,
    socket_address: &str,
    answer_deadline: Instant,
) -> Result<Option<String>, CliError> {
    find_session_version_from_endpoint(
        &build_foreign_session_endpoint(socket_address.to_string()),
        session_id,
        Some(answer_deadline),
    )
}

/// The build version the session `session_id` at `session_endpoint` reports
/// in its Hello answer, or `Ok(None)` when nothing listens there. Sends nothing
/// besides the Hello, and `answer_deadline` bounds the connect, the write and
/// the read as [`find_running_session_version`] states.
fn find_session_version_from_endpoint(
    session_endpoint: &EndpointFile,
    session_id: SessionId,
    answer_deadline: Option<Instant>,
) -> Result<Option<String>, CliError> {
    let (mut response_reader, mut request_writer) =
        match connect_to_session(session_endpoint, session_id, answer_deadline) {
            Ok(connection) => connection.split(),
            Err(CliError::SessionNotFound { .. }) => return Ok(None),
            Err(ipc_error) => return Err(ipc_error),
        };
    response_reader.set_deadline(answer_deadline);
    request_writer.set_deadline(answer_deadline);
    let build_exchange_error =
        |ipc_error: IpcError| build_session_exchange_error(ipc_error, answer_deadline);

    let hello_request = IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::build_hello_request(
            session_endpoint.connection_token.clone(),
        ),
    };
    request_writer
        .send(&hello_request)
        .map_err(build_exchange_error)?;
    let hello_response: IncomingResponse = response_reader.recv().map_err(build_exchange_error)?;
    talk::parse_session_hello_version(hello_response).map(|(_, version)| Some(version))
}

/// What `ask_session` answers for each of `session_asks`, in the order of
/// `session_asks`.
///
/// Up to [`MAX_SESSIONS_ASKED_AT_ONCE`] sessions are asked at the same time:
/// the calling thread asks, and so does each thread it starts beside it, one
/// fewer than that limit at most. A thread that cannot start leaves its share
/// to the threads that run. A session still unasked once `answer_deadline` is
/// reached is not asked, and answers [`CliError::SessionAnswerTimedOut`]. A
/// panic in `ask_session` reaches the caller.
///
/// Example: 40 sessions, 3 of them stopped with `SIGSTOP`, are asked by 16
/// threads at once. The 3 stopped ones hold one thread each until
/// `answer_deadline`, and the other 37 are answered by the other 13 threads
/// meanwhile.
pub fn ask_sessions_at_once<SessionAsk: Sync, SessionAnswer: Send>(
    session_asks: &[SessionAsk],
    answer_deadline: Instant,
    ask_session: impl Fn(&SessionAsk) -> Result<SessionAnswer, CliError> + Sync,
) -> Vec<Result<SessionAnswer, CliError>> {
    let next_session_ask_index = AtomicUsize::new(0);
    let ask_remaining_sessions = || {
        let mut indexed_session_answers = Vec::new();
        loop {
            let session_ask_index = next_session_ask_index.fetch_add(1, Ordering::Relaxed);
            let Some(session_ask) = session_asks.get(session_ask_index) else {
                return indexed_session_answers;
            };
            let session_answer = if Instant::now() >= answer_deadline {
                Err(CliError::SessionAnswerTimedOut)
            } else {
                ask_session(session_ask)
            };
            indexed_session_answers.push((session_ask_index, session_answer));
        }
    };
    let helper_thread_count = session_asks
        .len()
        .min(MAX_SESSIONS_ASKED_AT_ONCE)
        .saturating_sub(1);
    let mut indexed_session_answers = std::thread::scope(|thread_scope| {
        let helper_threads: Vec<_> = (0..helper_thread_count)
            .map_while(|_| {
                std::thread::Builder::new()
                    .name("koshi-session-ask".to_string())
                    .spawn_scoped(thread_scope, ask_remaining_sessions)
                    .ok()
            })
            .collect();
        let mut indexed_session_answers = ask_remaining_sessions();
        for helper_thread in helper_threads {
            match helper_thread.join() {
                Ok(helper_session_answers) => {
                    indexed_session_answers.extend(helper_session_answers)
                }
                Err(panic_payload) => std::panic::resume_unwind(panic_payload),
            }
        }
        indexed_session_answers
    });
    indexed_session_answers.sort_by_key(|(session_ask_index, _)| *session_ask_index);
    indexed_session_answers
        .into_iter()
        .map(|(_, session_answer)| session_answer)
        .collect()
}

/// Every session with an endpoint file in `runtime_directory`, in no particular
/// order. A file is counted by its name alone (`session-<uuid>.json`);
/// whether anything still listens behind it is the caller's probe to make.
/// A `runtime_directory` that does not exist holds no sessions.
///
/// # Errors
/// [`UnreadPath`] naming `runtime_directory` when reading it fails other than
/// as missing.
pub fn list_advertised_sessions(runtime_directory: &Path) -> Result<Vec<SessionId>, UnreadPath> {
    list_session_ids_named_by_suffix(runtime_directory, ".json")
}

/// Every session with a resume file in `runtime_directory`, in no particular order.
/// A file is counted by its name alone (`session-<uuid>` plus
/// [`RESUME_SUFFIX`]); whether that session still runs is the caller's check to
/// make. A `runtime_directory` that does not exist holds no sessions.
///
/// # Errors
/// [`UnreadPath`] naming `runtime_directory` when reading it fails other than
/// as missing.
pub fn list_sessions_with_resume_files(
    runtime_directory: &Path,
) -> Result<Vec<SessionId>, UnreadPath> {
    list_session_ids_named_by_suffix(runtime_directory, RESUME_SUFFIX)
}

/// Every session of this user's that `runtime_directory` holds, each once, in
/// no particular order: each session with an endpoint file, and each session
/// [`is_replacing_its_image`] accepts, whose endpoint file is gone while its new
/// image starts. A resume file as old as
/// [`RESTART_WINDOW_DURATION`] or
/// older adds nothing. A `runtime_directory` that does not exist holds no
/// sessions.
///
/// Example: endpoint files for `A` and `B`, and a resume file written 2 seconds
/// ago for `C` with no endpoint file, give `A`, `B` and `C`.
///
/// # Errors
/// [`UnreadPath`] naming `runtime_directory` when reading it fails other than
/// as missing.
pub fn list_own_sessions(runtime_directory: &Path) -> Result<Vec<SessionId>, UnreadPath> {
    let mut own_session_ids = list_advertised_sessions(runtime_directory)?;
    for session_id in list_sessions_with_resume_files(runtime_directory)? {
        if !own_session_ids.contains(&session_id)
            && is_replacing_its_image(runtime_directory, session_id)
        {
            own_session_ids.push(session_id);
        }
    }
    Ok(own_session_ids)
}

/// Wait for `session_id` to advertise a socket in `runtime_directory` under a
/// token other than `connection_token`, and hand that endpoint file back.
/// `None` when `restart_deadline` passes with the connection token still
/// unchanged.
///
/// A session server mints a fresh token every time it binds: a token other
/// than `connection_token` is the session's new image serving. The process id
/// in the file is not compared; on Unix, `execvp` keeps it.
///
/// The file is read every [`RESTART_POLL_INTERVAL_DURATION`] until the
/// deadline, and a missing or unreadable file is read again. The first read
/// happens before the deadline is checked: with a deadline already passed,
/// that one read still takes a session that is already back.
#[must_use]
pub fn wait_for_new_session_endpoint(
    runtime_directory: &Path,
    session_id: SessionId,
    connection_token: &ConnectionToken,
    restart_deadline: Instant,
) -> Option<EndpointFile> {
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
    loop {
        if let Ok(endpoint) = EndpointFile::load_from_path(&endpoint_path) {
            if endpoint.connection_token != *connection_token {
                return Some(endpoint);
            }
        }
        if Instant::now() >= restart_deadline {
            return None;
        }
        std::thread::sleep(RESTART_POLL_INTERVAL_DURATION);
    }
}

/// The endpoint the session `session_id` advertises once it has restarted,
/// after it refused this build's protocol version over `refused_endpoint`
/// with the sentence `refusal_detail`.
///
/// Only a session of this user's is waited for: one whose endpoint file in
/// `runtime_directory` can be read, or that [`is_replacing_its_image`]
/// accepts, and whose program file says a restart may still come, as
/// [`RefusingServerBuild::is_restart_expected`] states. The wait reads the
/// endpoint file every [`RESTART_POLL_INTERVAL_DURATION`]:
///
/// 1. A file naming a token other than `refused_endpoint`'s ends the wait with
///    that file.
/// 2. Once [`is_replacing_its_image`] accepts the session, the wait goes on
///    for up to [`RESTART_WINDOW_DURATION`] more, as
///    [`wait_for_new_session_endpoint`] states.
/// 3. With neither within [`REFUSED_SERVER_RESTART_START_WAIT_DURATION`], the
///    wait ends.
///
/// Every wait also ends at `answer_deadline` when one is given. Example: a
/// session on `0.5.0` whose program file now holds `0.6.0` refuses a `0.6.0`
/// CLI, restarts into `0.6.0` about a second after the refusal, and the
/// wait hands back its new endpoint file.
///
/// # Errors
/// [`CliError::ProtocolVersionRefused`]: carrying `refusal_detail` alone for
/// another user's session, at once. For a session of this user's that is not
/// waited for, or whose wait ended, carrying `refusal_detail`, then `; `, then
/// what [`RefusingServerBuild::format_refusal_hint`] gives for the program
/// file read once the wait ends, with the clause `end it with: koshi
/// kill-session <session id>`.
pub fn wait_for_refused_session_restart(
    runtime_directory: &Path,
    session_id: SessionId,
    refused_endpoint: &EndpointFile,
    refusal_detail: String,
    answer_deadline: Option<Instant>,
) -> Result<EndpointFile, CliError> {
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
    let is_own_session = EndpointFile::load_from_path(&endpoint_path).is_ok()
        || is_replacing_its_image(runtime_directory, session_id);
    if !is_own_session {
        return Err(CliError::ProtocolVersionRefused {
            detail: refusal_detail,
        });
    }
    let program_file_path =
        ServerProgramFile::resolve_session_program_file_path(runtime_directory, session_id);
    let build_refusal_error =
        |refusing_server_build: &RefusingServerBuild| CliError::ProtocolVersionRefused {
            detail: format!(
                "{refusal_detail}; {}",
                refusing_server_build
                    .format_refusal_hint(&format!("end it with: koshi kill-session {session_id}"))
            ),
        };
    let refusing_server_build =
        find_refusing_server_build(&program_file_path, refused_endpoint.process_id);
    if !refusing_server_build.is_restart_expected() {
        return Err(build_refusal_error(&refusing_server_build));
    }
    let bound_by_answer_deadline = |wait_end: Instant| match answer_deadline {
        Some(answer_deadline) => wait_end.min(answer_deadline),
        None => wait_end,
    };
    let start_deadline =
        bound_by_answer_deadline(Instant::now() + REFUSED_SERVER_RESTART_START_WAIT_DURATION);
    loop {
        if let Ok(endpoint) = EndpointFile::load_from_path(&endpoint_path) {
            if endpoint.connection_token != refused_endpoint.connection_token {
                return Ok(endpoint);
            }
        }
        if is_replacing_its_image(runtime_directory, session_id) {
            let restart_deadline =
                bound_by_answer_deadline(Instant::now() + RESTART_WINDOW_DURATION);
            if let Some(restarted_endpoint) = wait_for_new_session_endpoint(
                runtime_directory,
                session_id,
                &refused_endpoint.connection_token,
                restart_deadline,
            ) {
                return Ok(restarted_endpoint);
            }
            break;
        }
        if Instant::now() >= start_deadline {
            break;
        }
        std::thread::sleep(RESTART_POLL_INTERVAL_DURATION);
    }
    Err(build_refusal_error(&find_refusing_server_build(
        &program_file_path,
        refused_endpoint.process_id,
    )))
}

/// Run `make_exchange` over the endpoint of the session `session_id`, found as
/// [`load_session_endpoint`] finds it. An exchange that ends in
/// [`CliError::ProtocolVersionRefused`] runs once more, over the endpoint
/// [`wait_for_refused_session_restart`] hands back, and that wait's failure is
/// what this gives otherwise. `answer_deadline` bounds the wait.
///
/// # Errors
/// What [`load_session_endpoint`] gives, then what the last `make_exchange`
/// gives, or the failure of the wait.
pub fn run_session_exchange_with_restart_wait<ExchangeAnswer>(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
    answer_deadline: Option<Instant>,
    mut make_exchange: impl FnMut(&EndpointFile) -> Result<ExchangeAnswer, CliError>,
) -> Result<ExchangeAnswer, CliError> {
    let session_endpoint = load_session_endpoint(
        runtime_directory,
        shared_sessions_base_directory,
        session_id,
    )?;
    match make_exchange(&session_endpoint) {
        Err(CliError::ProtocolVersionRefused {
            detail: refusal_detail,
        }) => {
            let restarted_endpoint = wait_for_refused_session_restart(
                runtime_directory,
                session_id,
                &session_endpoint,
                refusal_detail,
                answer_deadline,
            )?;
            make_exchange(&restarted_endpoint)
        }
        exchange_result => exchange_result,
    }
}

/// The failure an exchange with the session `session_id` ends in while that
/// session replaces its process image: [`CliError::IpcUnavailable`] reading
/// `session session-<uuid> is restarting; ask again in a moment`.
#[must_use]
pub fn build_session_restarting_error(session_id: SessionId) -> CliError {
    CliError::IpcUnavailable {
        detail: format!("session {session_id} is restarting; ask again in a moment"),
    }
}

/// Every session `runtime_directory` holds a file for whose name is `session-<uuid>`
/// plus `file_name_suffix`, in no particular order. A `runtime_directory` that does not
/// exist holds no sessions.
///
/// # Errors
/// [`UnreadPath`] naming `runtime_directory` when opening it fails other than as
/// missing, or when reading an entry of it fails.
fn list_session_ids_named_by_suffix(
    runtime_directory: &Path,
    file_name_suffix: &str,
) -> Result<Vec<SessionId>, UnreadPath> {
    let directory_entries = match std::fs::read_dir(runtime_directory) {
        Ok(directory_entries) => directory_entries,
        Err(read_error) if read_error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(read_error) => return Err(UnreadPath::from_read_error(runtime_directory, &read_error)),
    };
    let mut session_ids = Vec::new();
    for directory_entry in directory_entries {
        let directory_entry = directory_entry
            .map_err(|read_error| UnreadPath::from_read_error(runtime_directory, &read_error))?;
        let session_id = directory_entry
            .file_name()
            .to_str()
            .and_then(|file_name| parse_session_id(file_name, file_name_suffix));
        session_ids.extend(session_id);
    }
    Ok(session_ids)
}

/// The session a file named `session-<uuid>` plus `file_name_suffix` names. Any other
/// name is `None`.
fn parse_session_id(file_name: &str, file_name_suffix: &str) -> Option<SessionId> {
    let session_uuid_text = file_name
        .strip_suffix(file_name_suffix)?
        .strip_prefix("session-")?;
    Some(SessionId::from_uuid(
        Uuid::parse_str(session_uuid_text).ok()?,
    ))
}

/// The sessions other local users started that the shared directory
/// advertises, as [`list_foreign_sessions`] reads them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForeignSessionListing {
    /// Each listed session's id and the control-socket address reaching it,
    /// in no particular order. No id is here twice.
    pub foreign_sessions: Vec<(SessionId, String)>,
    /// Each session id more than one socket advertises, in no particular
    /// order. Always empty on Windows.
    pub duplicated_sessions: Vec<DuplicatedForeignSession>,
    /// How many more entries named like a session the shared directory holds
    /// past the caps: past [`MAX_SHARED_SESSION_COUNT_PER_OWNER`] of one owner
    /// on Unix, and past [`MAX_SHARED_MARKER_COUNT`] on Windows. Each is
    /// counted by its name alone. An entry after a listing stops at
    /// [`MAX_SHARED_DIRECTORY_ENTRY_COUNT`] or
    /// [`MAX_SHARED_USER_DIRECTORY_COUNT`] is not read, and not counted.
    pub unlisted_session_count: usize,
    /// The first read that failed while listing, other than at a path that
    /// advertises nothing. That failure is written to the log once, at warn
    /// level. Each failure after it is neither kept nor logged. `None` when
    /// every read succeeded. The sessions under that path are neither listed
    /// nor counted, and a runtime directory that cannot be read lists no
    /// session at all. A listing that stops at
    /// [`MAX_SHARED_DIRECTORY_ENTRY_COUNT`] or
    /// [`MAX_SHARED_USER_DIRECTORY_COUNT`] counts as a failed read of the
    /// shared directory.
    pub unread_path: Option<UnreadPath>,
}

impl ForeignSessionListing {
    /// Keep `read_error`, from reading `looked_up_path`, through
    /// [`record_unread_path`](Self::record_unread_path), unless
    /// [`is_unadvertised_path_error`] accepts it.
    fn record_failed_read(&mut self, looked_up_path: &Path, read_error: &std::io::Error) {
        if !is_unadvertised_path_error(read_error) {
            self.record_unread_path(UnreadPath::from_read_error(looked_up_path, read_error));
        }
    }

    /// Keep `unread_path` as [`unread_path`](Self::unread_path), and log one
    /// warning naming it, unless a failure is already kept.
    fn record_unread_path(&mut self, unread_path: UnreadPath) {
        if self.unread_path.is_some() {
            return;
        }
        tracing::warn!(%unread_path, "the shared sessions under this path are not listed");
        self.unread_path = Some(unread_path);
    }
}

/// One path a listing of sessions could not read: the sessions under it are
/// unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadPath {
    /// The path read, such as `/tmp/koshi/1002`.
    pub looked_up_path: PathBuf,
    /// The failure the read ended in, such as `Input/output error (os error 5)`,
    /// or the limit a scan of the shared directory stopped at, such as `it
    /// holds more than 256 user folders`.
    pub read_error_text: String,
}

impl UnreadPath {
    /// The unread path that `read_error`, from reading `looked_up_path`, gives.
    #[must_use]
    pub fn from_read_error(looked_up_path: &Path, read_error: &std::io::Error) -> UnreadPath {
        UnreadPath {
            looked_up_path: looked_up_path.to_path_buf(),
            read_error_text: read_error.to_string(),
        }
    }
}

impl std::fmt::Display for UnreadPath {
    /// `/tmp/koshi/1002 could not be read: Input/output error (os error 5)`.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} could not be read: {}",
            self.looked_up_path.display(),
            self.read_error_text
        )
    }
}

/// What one scan of the shared directory under a base path has read, against
/// [`MAX_SHARED_DIRECTORY_ENTRY_COUNT`] and, on Unix,
/// [`MAX_SHARED_USER_DIRECTORY_COUNT`].
#[derive(Default)]
struct SharedDirectoryScan {
    /// Entries read so far: of the shared directory, and on Unix of each
    /// user's folder opened.
    read_entry_count: usize,
    /// User folders found so far.
    #[cfg(unix)]
    found_user_directory_count: usize,
}

impl SharedDirectoryScan {
    /// Count one more entry read.
    ///
    /// # Errors
    /// [`UnreadPath`] naming `shared_sessions_base_directory` and reading `it
    /// holds more than 65536 entries`, for the entry past
    /// [`MAX_SHARED_DIRECTORY_ENTRY_COUNT`] and each one after it.
    fn count_read_entry(
        &mut self,
        shared_sessions_base_directory: &Path,
    ) -> Result<(), UnreadPath> {
        self.read_entry_count += 1;
        if self.read_entry_count > MAX_SHARED_DIRECTORY_ENTRY_COUNT {
            return Err(UnreadPath {
                looked_up_path: shared_sessions_base_directory.to_path_buf(),
                read_error_text: format!(
                    "it holds more than {MAX_SHARED_DIRECTORY_ENTRY_COUNT} entries"
                ),
            });
        }
        Ok(())
    }

    /// Count one more user folder found.
    ///
    /// # Errors
    /// [`UnreadPath`] naming `shared_sessions_base_directory` and reading `it
    /// holds more than 256 user folders`, for the folder past
    /// [`MAX_SHARED_USER_DIRECTORY_COUNT`] and each one after it.
    #[cfg(unix)]
    fn count_found_user_directory(
        &mut self,
        shared_sessions_base_directory: &Path,
    ) -> Result<(), UnreadPath> {
        self.found_user_directory_count += 1;
        if self.found_user_directory_count > MAX_SHARED_USER_DIRECTORY_COUNT {
            return Err(UnreadPath {
                looked_up_path: shared_sessions_base_directory.to_path_buf(),
                read_error_text: format!(
                    "it holds more than {MAX_SHARED_USER_DIRECTORY_COUNT} user folders"
                ),
            });
        }
        Ok(())
    }
}

/// Whether `read_error`, from reading a path of the shared directory, says that
/// path advertises nothing: it is gone ([`NotFound`](std::io::ErrorKind::NotFound)),
/// it is not a directory ([`NotADirectory`](std::io::ErrorKind::NotADirectory)),
/// or its owner lets this user neither list nor search it
/// ([`PermissionDenied`](std::io::ErrorKind::PermissionDenied)). Every other
/// failure, such as `EMFILE` or `EIO`, gives `false`.
fn is_unadvertised_path_error(read_error: &std::io::Error) -> bool {
    matches!(
        read_error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::NotADirectory
            | std::io::ErrorKind::PermissionDenied
    )
}

/// One session id that more than one socket in the shared directory
/// advertises. koshi reaches none of those sockets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicatedForeignSession {
    /// The id the sockets advertise.
    pub session_id: SessionId,
    /// How many sockets advertise it: 2 or more.
    pub advertisement_count: usize,
    /// The users owning those sockets, each once, lowest first.
    pub owner_user_ids: Vec<u32>,
}

impl DuplicatedForeignSession {
    /// The duplicate `session_id` reads as, from the owner of each socket
    /// advertising it, one entry per socket. Example: owners `[1002, 1001,
    /// 1002]` give `advertisement_count` 3 and `owner_user_ids` `[1001, 1002]`.
    #[cfg(unix)]
    fn from_socket_owner_user_ids(
        session_id: SessionId,
        mut socket_owner_user_ids: Vec<u32>,
    ) -> DuplicatedForeignSession {
        let advertisement_count = socket_owner_user_ids.len();
        socket_owner_user_ids.sort_unstable();
        socket_owner_user_ids.dedup();
        DuplicatedForeignSession {
            session_id,
            advertisement_count,
            owner_user_ids: socket_owner_user_ids,
        }
    }

    /// The failure a lookup of this session ends in: [`CliError::IpcUnavailable`]
    /// carrying the sentence the [`Display`](std::fmt::Display) form gives.
    #[must_use]
    pub fn build_refusal_error(&self) -> CliError {
        CliError::IpcUnavailable {
            detail: self.to_string(),
        }
    }
}

impl std::fmt::Display for DuplicatedForeignSession {
    /// `session session-<uuid> is advertised 2 times in the shared directory,
    /// by user ids 1001, 1002; koshi reaches none of them`. One owner reads
    /// `by user id 1001`.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let owner_noun = if self.owner_user_ids.len() == 1 {
            "user id"
        } else {
            "user ids"
        };
        let owner_user_id_list = self
            .owner_user_ids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<String>>()
            .join(", ");
        write!(
            formatter,
            "session {} is advertised {} times in the shared directory, by {owner_noun} \
             {owner_user_id_list}; koshi reaches none of them",
            self.session_id, self.advertisement_count
        )
    }
}

/// Why [`find_foreign_session_address`] reaches no socket for a session id.
#[derive(Debug, thiserror::Error)]
pub enum ForeignSessionLookupError {
    /// More than one socket advertises the id.
    #[error("{duplicated_session}")]
    AdvertisedMoreThanOnce {
        /// The id, how many sockets advertise it, and their owners.
        duplicated_session: DuplicatedForeignSession,
    },
    /// A path the lookup reads could not be read.
    #[error("{unread_path}")]
    PathUnreadable {
        /// The path and the failure its read ended in.
        unread_path: UnreadPath,
    },
}

impl From<UnreadPath> for ForeignSessionLookupError {
    /// [`ForeignSessionLookupError::PathUnreadable`] carrying `unread_path`.
    fn from(unread_path: UnreadPath) -> ForeignSessionLookupError {
        ForeignSessionLookupError::PathUnreadable { unread_path }
    }
}

impl From<ForeignSessionLookupError> for CliError {
    /// [`CliError::IpcUnavailable`] carrying the lookup failure's sentence.
    fn from(lookup_error: ForeignSessionLookupError) -> CliError {
        CliError::IpcUnavailable {
            detail: lookup_error.to_string(),
        }
    }
}

/// Every session another local user started that `shared_sessions_base_directory` advertises,
/// each id more than one socket advertises, and how many more sessions past the caps.
///
/// Every entry of the directory is read, up to the limits below. An entry
/// that names no session costs no system call beyond the directory read. A
/// session id `runtime_directory` holds an endpoint file or a resume file of
/// is left out on both platforms: this user's own session, never a foreign
/// one standing in under the same id, even while that session replaces its
/// process image. Whether anything still listens behind a session is the
/// caller's probe to make.
///
/// The first read that fails is kept in
/// [`unread_path`](ForeignSessionListing::unread_path). A user's folder or a
/// socket whose read fails is skipped, and the listing goes on with the next
/// one. A read of the shared directory or of a folder that fails part way
/// keeps the sessions read before the failure, and lists nothing after it in
/// that directory. A path that is gone, is not a directory, or that its owner
/// lets no other user read advertises nothing, and is not kept. A
/// `runtime_directory` that cannot be read lists nothing, and is kept.
/// Example: another user's folder at mode `0700` lists none
/// of its sessions and keeps nothing, and a folder whose read fails with
/// `EMFILE` lists none of its sessions and is kept.
///
/// One listing goes through at most [`MAX_SHARED_DIRECTORY_ENTRY_COUNT`]
/// entries, counting those of the shared directory and, on Unix, those of
/// every user's folder it opens. On Unix it also goes through at most
/// [`MAX_SHARED_USER_DIRECTORY_COUNT`] user folders. Reading one more entry,
/// or finding one more folder, stops the listing: the sessions found before
/// the stop are listed, nothing after it is read, and the shared directory is
/// kept, read as `it holds more than 65536 entries` or `it holds more than
/// 256 user folders`. Example: another user who makes 300 empty folders named
/// `100000` to `100299` stops the listing at the 257th user folder it finds.
///
/// On Unix each user's sockets sit in a folder named after that user's id in
/// decimal with no sign and no leading zero, such as `1001`. This user's own
/// folder and a link to a folder are skipped. A session counts when its file is
/// named `session-<uuid>.sock`, is a socket, and its owner is the owner of the
/// folder holding it. Of one owner, across every folder that owner holds, the
/// first [`MAX_SHARED_SESSION_COUNT_PER_OWNER`] entries named like a session
/// are checked, and each further one is counted in
/// [`unlisted_session_count`](ForeignSessionListing::unlisted_session_count).
/// Example: another user who plants 10,000 sockets in their folder has 256 of
/// them listed and 9,744 counted. An id that sockets in two folders advertise
/// is in [`duplicated_sessions`](ForeignSessionListing::duplicated_sessions)
/// and not in [`foreign_sessions`](ForeignSessionListing::foreign_sessions). A
/// `runtime_directory` that does not exist skips no folder; one whose owner
/// cannot be read lists nothing.
///
/// On Windows the markers share one flat directory. A marker counts when its
/// name is `session-<uuid>`. The first [`MAX_SHARED_MARKER_COUNT`] are listed,
/// and each further one is counted.
#[must_use]
pub fn list_foreign_sessions(
    shared_sessions_base_directory: &Path,
    runtime_directory: &Path,
) -> ForeignSessionListing {
    let mut foreign_session_listing = ForeignSessionListing::default();
    let own_session_ids: HashSet<SessionId> = match (
        list_advertised_sessions(runtime_directory),
        list_sessions_with_resume_files(runtime_directory),
    ) {
        (Ok(endpoint_session_ids), Ok(resume_session_ids)) => endpoint_session_ids
            .into_iter()
            .chain(resume_session_ids)
            .collect(),
        (Err(unread_path), _) | (_, Err(unread_path)) => {
            foreign_session_listing.record_unread_path(unread_path);
            return foreign_session_listing;
        }
    };
    let directory_entries = match std::fs::read_dir(shared_sessions_base_directory) {
        Ok(directory_entries) => directory_entries,
        Err(read_error) => {
            foreign_session_listing.record_failed_read(shared_sessions_base_directory, &read_error);
            return foreign_session_listing;
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let own_user_directory_name = match find_own_user_directory_name(runtime_directory) {
            Ok(own_user_directory_name) => own_user_directory_name,
            Err(unread_path) => {
                foreign_session_listing.record_unread_path(unread_path);
                return foreign_session_listing;
            }
        };
        let mut shared_directory_scan = SharedDirectoryScan::default();
        let mut checked_session_count_by_owner: HashMap<u32, usize> = HashMap::new();
        let mut advertisements_by_session_id: HashMap<SessionId, Vec<(u32, String)>> =
            HashMap::new();
        'user_directories: for directory_entry in directory_entries {
            if let Err(unread_path) =
                shared_directory_scan.count_read_entry(shared_sessions_base_directory)
            {
                foreign_session_listing.record_unread_path(unread_path);
                break;
            }
            let directory_entry = match directory_entry {
                Ok(directory_entry) => directory_entry,
                Err(read_error) => {
                    foreign_session_listing
                        .record_failed_read(shared_sessions_base_directory, &read_error);
                    break;
                }
            };
            let Some(user_directory_path) =
                find_user_directory(&directory_entry, own_user_directory_name.as_deref())
            else {
                continue;
            };
            if let Err(unread_path) =
                shared_directory_scan.count_found_user_directory(shared_sessions_base_directory)
            {
                foreign_session_listing.record_unread_path(unread_path);
                break;
            }
            let user_directory_metadata = match directory_entry.metadata() {
                Ok(user_directory_metadata) => user_directory_metadata,
                Err(read_error) => {
                    foreign_session_listing.record_failed_read(&user_directory_path, &read_error);
                    continue;
                }
            };
            let session_entries = match std::fs::read_dir(&user_directory_path) {
                Ok(session_entries) => session_entries,
                Err(read_error) => {
                    foreign_session_listing.record_failed_read(&user_directory_path, &read_error);
                    continue;
                }
            };
            let owner_user_id = user_directory_metadata.uid();
            let checked_session_count = checked_session_count_by_owner
                .entry(owner_user_id)
                .or_insert(0);
            for session_entry in session_entries {
                if let Err(unread_path) =
                    shared_directory_scan.count_read_entry(shared_sessions_base_directory)
                {
                    foreign_session_listing.record_unread_path(unread_path);
                    break 'user_directories;
                }
                let session_entry = match session_entry {
                    Ok(session_entry) => session_entry,
                    Err(read_error) => {
                        foreign_session_listing
                            .record_failed_read(&user_directory_path, &read_error);
                        continue 'user_directories;
                    }
                };
                let Some(session_id) =
                    find_named_foreign_session(&session_entry, ".sock", &own_session_ids)
                else {
                    continue;
                };
                if *checked_session_count == MAX_SHARED_SESSION_COUNT_PER_OWNER {
                    foreign_session_listing.unlisted_session_count += 1;
                    continue;
                }
                *checked_session_count += 1;
                let session_metadata = match session_entry.metadata() {
                    Ok(session_metadata) => session_metadata,
                    Err(read_error) => {
                        foreign_session_listing
                            .record_failed_read(&session_entry.path(), &read_error);
                        continue;
                    }
                };
                if is_socket_owned_by(&session_metadata, owner_user_id) {
                    advertisements_by_session_id
                        .entry(session_id)
                        .or_default()
                        .push((
                            owner_user_id,
                            compute_socket_address(&user_directory_path, session_id),
                        ));
                }
            }
        }
        for (session_id, advertisements) in advertisements_by_session_id {
            match advertisements.as_slice() {
                [(_, socket_address)] => foreign_session_listing
                    .foreign_sessions
                    .push((session_id, socket_address.clone())),
                _ => foreign_session_listing.duplicated_sessions.push(
                    DuplicatedForeignSession::from_socket_owner_user_ids(
                        session_id,
                        advertisements
                            .iter()
                            .map(|(owner_user_id, _)| *owner_user_id)
                            .collect(),
                    ),
                ),
            }
        }
    }
    #[cfg(windows)]
    {
        let mut shared_directory_scan = SharedDirectoryScan::default();
        for directory_entry in directory_entries {
            if let Err(unread_path) =
                shared_directory_scan.count_read_entry(shared_sessions_base_directory)
            {
                foreign_session_listing.record_unread_path(unread_path);
                break;
            }
            let directory_entry = match directory_entry {
                Ok(directory_entry) => directory_entry,
                Err(read_error) => {
                    foreign_session_listing
                        .record_failed_read(shared_sessions_base_directory, &read_error);
                    break;
                }
            };
            let Some(session_id) =
                find_named_foreign_session(&directory_entry, "", &own_session_ids)
            else {
                continue;
            };
            if foreign_session_listing.foreign_sessions.len() == MAX_SHARED_MARKER_COUNT {
                foreign_session_listing.unlisted_session_count += 1;
                continue;
            }
            foreign_session_listing.foreign_sessions.push((
                session_id,
                compute_socket_address(shared_sessions_base_directory, session_id),
            ));
        }
    }
    foreign_session_listing
}

/// The session `directory_entry` names when its name is `session-<uuid>` plus
/// `file_name_suffix` and that session is not in `own_session_ids`. `None`
/// for every other entry. Reads the name only: no system call.
///
/// Example: with the suffix `.sock`, `session-<uuid>.sock` gives that session,
/// and `notes` and `session-<uuid>` give `None`.
fn find_named_foreign_session(
    directory_entry: &std::fs::DirEntry,
    file_name_suffix: &str,
    own_session_ids: &HashSet<SessionId>,
) -> Option<SessionId> {
    let session_id = parse_session_id(directory_entry.file_name().to_str()?, file_name_suffix)?;
    (!own_session_ids.contains(&session_id)).then_some(session_id)
}

/// Whether `socket_metadata`, read without following a link, is a socket that
/// `owner_user_id` owns.
#[cfg(unix)]
fn is_socket_owned_by(socket_metadata: &std::fs::Metadata, owner_user_id: u32) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    socket_metadata.file_type().is_socket() && socket_metadata.uid() == owner_user_id
}

/// The control-socket address of the session `session_id` another local user
/// started, as `shared_sessions_base_directory` advertises it, or `Ok(None)`
/// when it advertises none.
///
/// No user's folder is listed, and no cap on sessions applies: on Unix the one
/// path `session-<uuid>.sock` is looked up in each folder named after another
/// user's id in decimal, such as `1001`, and on Windows the one marker
/// `session-<uuid>`. An entry of the
/// shared directory that names no user's folder costs no system call beyond
/// the directory read. A Unix socket counts when it is a socket and the owner
/// of the folder holding it owns it. On Unix the lookup goes through at most
/// [`MAX_SHARED_DIRECTORY_ENTRY_COUNT`] entries of the shared directory and
/// at most [`MAX_SHARED_USER_DIRECTORY_COUNT`] user folders.
///
/// An id whose endpoint file or resume file sits in `runtime_directory` is
/// this user's, and gives `Ok(None)`: another user's socket under that id is
/// never taken for it. A shared directory, a folder or a socket path that is
/// gone, is not a directory, or that its owner lets no other user read
/// advertises nothing. Example: the socket of `session_id` in another user's
/// folder at mode `0700` cannot be looked up, and gives `Ok(None)`.
///
/// # Errors
/// [`ForeignSessionLookupError::AdvertisedMoreThanOnce`] when sockets in two
/// folders advertise the id. [`ForeignSessionLookupError::PathUnreadable`]
/// when the endpoint file or the resume file of `session_id` in
/// `runtime_directory` cannot be looked up, when the Windows marker cannot be
/// looked up, on Unix when the owner of `runtime_directory` cannot be read,
/// when a read of the shared directory fails another way, such as with
/// `EMFILE`, and on Unix when the shared directory holds one entry past
/// [`MAX_SHARED_DIRECTORY_ENTRY_COUNT`] or one user folder past
/// [`MAX_SHARED_USER_DIRECTORY_COUNT`]. Those two name the shared directory,
/// read as `it holds more than 65536 entries` and `it holds more than 256
/// user folders`.
pub fn find_foreign_session_address(
    shared_sessions_base_directory: &Path,
    runtime_directory: &Path,
    session_id: SessionId,
) -> Result<Option<String>, ForeignSessionLookupError> {
    for own_session_file_path in [
        EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id),
        resolve_resume_file_path(runtime_directory, session_id),
    ] {
        if is_file_present(&own_session_file_path)? {
            return Ok(None);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let own_user_directory_name = find_own_user_directory_name(runtime_directory)?;
        let directory_entries = match std::fs::read_dir(shared_sessions_base_directory) {
            Ok(directory_entries) => directory_entries,
            Err(read_error) => {
                validate_unadvertised_path_error(shared_sessions_base_directory, &read_error)?;
                return Ok(None);
            }
        };
        let mut shared_directory_scan = SharedDirectoryScan::default();
        let mut advertisements: Vec<(u32, String)> = Vec::new();
        for directory_entry in directory_entries {
            shared_directory_scan.count_read_entry(shared_sessions_base_directory)?;
            let directory_entry = directory_entry.map_err(|read_error| {
                UnreadPath::from_read_error(shared_sessions_base_directory, &read_error)
            })?;
            let Some(user_directory_path) =
                find_user_directory(&directory_entry, own_user_directory_name.as_deref())
            else {
                continue;
            };
            shared_directory_scan.count_found_user_directory(shared_sessions_base_directory)?;
            let user_directory_metadata = match directory_entry.metadata() {
                Ok(user_directory_metadata) => user_directory_metadata,
                Err(read_error) => {
                    validate_unadvertised_path_error(&user_directory_path, &read_error)?;
                    continue;
                }
            };
            let socket_address = compute_socket_address(&user_directory_path, session_id);
            let socket_metadata = match std::fs::symlink_metadata(&socket_address) {
                Ok(socket_metadata) => socket_metadata,
                Err(read_error) => {
                    validate_unadvertised_path_error(Path::new(&socket_address), &read_error)?;
                    continue;
                }
            };
            if is_socket_owned_by(&socket_metadata, user_directory_metadata.uid()) {
                advertisements.push((user_directory_metadata.uid(), socket_address));
            }
        }
        match advertisements.as_slice() {
            [] => Ok(None),
            [(_, socket_address)] => Ok(Some(socket_address.clone())),
            _ => Err(ForeignSessionLookupError::AdvertisedMoreThanOnce {
                duplicated_session: DuplicatedForeignSession::from_socket_owner_user_ids(
                    session_id,
                    advertisements
                        .iter()
                        .map(|(owner_user_id, _)| *owner_user_id)
                        .collect(),
                ),
            }),
        }
    }
    #[cfg(windows)]
    {
        let advertisement_marker_path =
            resolve_advertisement_marker_path(shared_sessions_base_directory, session_id);
        match std::fs::symlink_metadata(&advertisement_marker_path) {
            Ok(_) => Ok(Some(compute_socket_address(
                shared_sessions_base_directory,
                session_id,
            ))),
            Err(read_error) => {
                validate_unadvertised_path_error(&advertisement_marker_path, &read_error)?;
                Ok(None)
            }
        }
    }
}

/// `Ok(())` when `read_error`, from reading `looked_up_path` in the shared
/// directory, says that path advertises nothing, as
/// [`is_unadvertised_path_error`] decides.
///
/// # Errors
/// [`ForeignSessionLookupError::PathUnreadable`] naming `looked_up_path` for
/// every other failure.
fn validate_unadvertised_path_error(
    looked_up_path: &Path,
    read_error: &std::io::Error,
) -> Result<(), ForeignSessionLookupError> {
    if is_unadvertised_path_error(read_error) {
        return Ok(());
    }
    Err(UnreadPath::from_read_error(looked_up_path, read_error).into())
}

/// Whether anything sits at `file_path`, looked up without following a link.
///
/// # Errors
/// [`UnreadPath`] naming `file_path` for a lookup that fails other than with
/// "not found", such as one this user may not search.
fn is_file_present(file_path: &Path) -> Result<bool, UnreadPath> {
    match std::fs::symlink_metadata(file_path) {
        Ok(_) => Ok(true),
        Err(read_error) if read_error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(read_error) => Err(UnreadPath::from_read_error(file_path, &read_error)),
    }
}

/// The name of this user's own folder in the shared directory: the user id
/// that owns `runtime_directory`, such as `501`. `Ok(None)` when
/// `runtime_directory` does not exist: this user then holds no session, and
/// no folder is this user's.
///
/// # Errors
/// [`UnreadPath`] naming `runtime_directory` when it exists and its owner
/// cannot be read.
#[cfg(unix)]
fn find_own_user_directory_name(runtime_directory: &Path) -> Result<Option<String>, UnreadPath> {
    use std::os::unix::fs::MetadataExt;

    match std::fs::metadata(runtime_directory) {
        Ok(directory_metadata) => Ok(Some(directory_metadata.uid().to_string())),
        Err(read_error) if read_error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(read_error) => Err(UnreadPath::from_read_error(runtime_directory, &read_error)),
    }
}

/// The path of `directory_entry` when it is one user's folder of the shared
/// directory: its name is a user id written in decimal with no sign and no
/// leading zero, such as `1001`, the name is not `own_user_directory_name`,
/// and the entry is a directory, not a link to one. `None` for every other
/// entry. Reads the name and the type the directory read already gave: no
/// system call.
///
/// Example: `1001` gives its path, while `01001`, `+1001`, `notes` and a
/// regular file named `1001` give `None`.
#[cfg(unix)]
fn find_user_directory(
    directory_entry: &std::fs::DirEntry,
    own_user_directory_name: Option<&str>,
) -> Option<PathBuf> {
    let file_name = directory_entry.file_name();
    let user_directory_name = file_name.to_str()?;
    let user_id = user_directory_name.parse::<u32>().ok()?;
    if user_id.to_string() != user_directory_name
        || own_user_directory_name == Some(user_directory_name)
    {
        return None;
    }
    let is_directory = directory_entry
        .file_type()
        .is_ok_and(|file_type| file_type.is_dir());
    is_directory.then(|| directory_entry.path())
}

/// How to reach the session another local user started at `socket_address`: the
/// address the shared directory advertised, and the empty token that session
/// asks another user for. That user's own endpoint file stays unread.
fn build_foreign_session_endpoint(socket_address: String) -> EndpointFile {
    EndpointFile {
        socket_address,
        connection_token: ConnectionToken::from_secret(""),
        process_id: 0,
    }
}

/// Connect to `endpoint`, open with the Hello, and run `ipc_request` on the same
/// connection. Returns `ipc_request`'s result; a failed Hello is an error.
///
/// The Hello and `ipc_request` go out back to back before either reply is read,
/// and the server answers every request in order: the exchange costs one
/// round trip.
///
/// With `answer_deadline`, the connect and every write and read after it end
/// by that moment, as [`connect_to_session`] states, and a failure once it is
/// reached is [`CliError::SessionAnswerTimedOut`]. With `None`, the connect
/// waits at most [`CONNECT_WAIT_DURATION`], and nothing ends the writes and
/// reads early.
fn exchange_session_request(
    endpoint: &EndpointFile,
    session_id: SessionId,
    ipc_request: IpcRequest,
    answer_deadline: Option<Instant>,
) -> Result<IpcResult, CliError> {
    let (mut response_reader, mut request_writer) =
        connect_to_session(endpoint, session_id, answer_deadline)?.split();
    response_reader.set_deadline(answer_deadline);
    request_writer.set_deadline(answer_deadline);
    let build_exchange_error =
        |ipc_error: IpcError| build_session_exchange_error(ipc_error, answer_deadline);
    let hello_request = IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::build_hello_request(endpoint.connection_token.clone()),
    };
    request_writer
        .send(&hello_request)
        .map_err(build_exchange_error)?;

    request_writer
        .send(&ipc_request)
        .map_err(build_exchange_error)?;
    let hello_response: IncomingResponse = response_reader.recv().map_err(build_exchange_error)?;
    talk::parse_session_hello_version(hello_response)?;

    let ipc_response: IncomingResponse = response_reader.recv().map_err(build_exchange_error)?;
    talk::SESSION_PEER_WORDS.take_response_result(ipc_response)
}

/// The error a failed write or read of a session exchange ends in:
/// [`CliError::SessionAnswerTimedOut`] once [`is_answer_deadline_reached`]
/// accepts `answer_deadline`, and the failure through
/// [`build_ipc_unavailable_error`] otherwise.
fn build_session_exchange_error(ipc_error: IpcError, answer_deadline: Option<Instant>) -> CliError {
    if is_answer_deadline_reached(answer_deadline) {
        return CliError::SessionAnswerTimedOut;
    }
    build_ipc_unavailable_error(ipc_error)
}

/// Whether `answer_deadline` is reached, as [`compute_time_left_until`]
/// decides: less than 1 ms is left. `None` is never reached.
fn is_answer_deadline_reached(answer_deadline: Option<Instant>) -> bool {
    answer_deadline.is_some_and(|answer_deadline| compute_time_left_until(answer_deadline).is_err())
}

/// How to reach `session_id`: the endpoint file in `runtime_directory`, or — for a
/// session another local user started — what `shared_sessions_base_directory`
/// advertises for it, through [`find_foreign_session_address`].
///
/// A session another local user started writes its endpoint file into that
/// user's own runtime directory, which this user may not read and does not
/// need: the shared directory names the socket, and the token presented is
/// empty. `shared_sessions_base_directory` of `None` searches
/// `runtime_directory` alone. An id no searched place holds means no running
/// koshi advertises that session: [`CliError::SessionNotFound`].
///
/// A session of this user's whose endpoint file is gone while
/// [`is_replacing_its_image`] accepts it is restarting, and the shared
/// directory is not searched for it.
///
/// # Errors
/// [`CliError::SessionNotFound`] for an id no searched place holds; the
/// failure [`build_session_restarting_error`] gives for a session that is
/// restarting; [`CliError::IpcUnavailable`] carrying the sentence of a failure
/// [`find_foreign_session_address`] gives; and [`CliError::IpcUnavailable`]
/// for an endpoint file that exists and cannot be read.
pub fn load_session_endpoint(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
) -> Result<EndpointFile, CliError> {
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
    match EndpointFile::load_from_path(&endpoint_path) {
        Ok(endpoint) => Ok(endpoint),
        Err(IpcError::EndpointFileMissing { .. }) => {
            if is_replacing_its_image(runtime_directory, session_id) {
                return Err(build_session_restarting_error(session_id));
            }
            let foreign_socket_address = match shared_sessions_base_directory {
                Some(shared_sessions_base_directory) => find_foreign_session_address(
                    shared_sessions_base_directory,
                    runtime_directory,
                    session_id,
                )?,
                None => None,
            };
            foreign_socket_address
                .map(build_foreign_session_endpoint)
                .ok_or_else(|| CliError::SessionNotFound {
                    session_name: session_id.to_string(),
                })
        }
        Err(ipc_error) => Err(CliError::IpcUnavailable {
            detail: ipc_error.to_string(),
        }),
    }
}

/// Connect to the advertised socket, waiting at most
/// [`CONNECT_WAIT_DURATION`] for the connect to complete.
///
/// With `answer_deadline`, the wait also ends by that moment, and no connect
/// starts once [`compute_time_left_until`] finds it reached: that is
/// [`CliError::SessionAnswerTimedOut`]. Example: a deadline 300 ms away gives
/// the connect 300 ms, and one already passed connects to nothing.
///
/// # Errors
/// An address nothing listens on reports the session as not running
/// ([`CliError::SessionNotFound`]). Any other failure once `answer_deadline`
/// is reached is [`CliError::SessionAnswerTimedOut`]. Every other transport
/// failure is [`CliError::IpcUnavailable`].
pub fn connect_to_session(
    endpoint: &EndpointFile,
    session_id: SessionId,
    answer_deadline: Option<Instant>,
) -> Result<Connection, CliError> {
    let connect_wait_duration = match answer_deadline {
        Some(answer_deadline) => compute_time_left_until(answer_deadline)
            .map_err(|_| CliError::SessionAnswerTimedOut)?
            .min(CONNECT_WAIT_DURATION),
        None => CONNECT_WAIT_DURATION,
    };
    Connection::connect_within(&endpoint.socket_address, connect_wait_duration).map_err(
        |ipc_error| match ipc_error {
            IpcError::NoListener { .. } => CliError::SessionNotFound {
                session_name: session_id.to_string(),
            },
            _ if is_answer_deadline_reached(answer_deadline) => CliError::SessionAnswerTimedOut,
            ipc_error => CliError::IpcUnavailable {
                detail: ipc_error.to_string(),
            },
        },
    )
}

#[cfg(test)]
mod tests;
