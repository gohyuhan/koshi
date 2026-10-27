//! The CLI side of the control socket: find the session's endpoint, connect,
//! open with Hello, submit one command, and read back its result.
//!
//! The endpoint file in the private runtime directory advertises the
//! session's socket address and connection token; reading it is the
//! same-user proof the Hello presents. The Hello and a `Prepare` carrying the
//! command are written back to back before either reply is read. Once the
//! session answers `Prepared`, a `Commit` runs the command: a submission costs
//! two round trips, whether or not it names a target client. A session that
//! settles on a version this build does not speak reads the Hello and the
//! `Prepare`, the caller is refused on the Hello answer, and the command never
//! runs. A caller that ends before it writes the `Commit` leaves nothing
//! behind in the session.
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
//! Asking a running session to restart is one more such `Prepare` and `Commit`
//! exchange. A session
//! that is not listening reads as `NotRunning`, not as an error. A session
//! of this user's whose endpoint file is gone while it replaces its process
//! image reads as restarting instead.
//!
//! A stand-in clipboard tool in a pane hands its copy to the pane's session
//! part by part, on the one connection that [`open_clipboard_copy_exchange`]
//! opens.

#[cfg(unix)]
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use koshi_core::command::{Command, CommandEnvelope, CommandResult, CommandSource};
use koshi_core::discovery::SessionOverview;
use koshi_core::event::RejectReason;
use koshi_core::ids::{format_session_target, ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::recent_event::RecentEvent;
use koshi_core::text::sanitize_reported_text;
#[cfg(unix)]
use koshi_ipc::endpoint::resolve_resume_file_path;
use koshi_ipc::endpoint::{
    compute_socket_address, is_replacing_its_image, EndpointFile, ServerProgramFile,
    RESTART_WINDOW_DURATION, RESUME_SUFFIX,
};
use koshi_ipc::error::IpcError;
use koshi_ipc::layout::SessionLayout;
use koshi_ipc::previous_release::{
    PreviousReleaseAnswer, PreviousReleaseErrorCode, PreviousReleaseRequest,
    PreviousReleaseRequestKind, PreviousReleaseResult,
};
use koshi_ipc::protocol::{
    ConnectionToken, IncomingResponse, IpcErrorCode, IpcRequest, IpcRequestKind, IpcResult,
    ProcessImageId,
};
use koshi_ipc::transport::{
    compute_remaining_duration_until, Connection, FrameReader, FrameWriter, CONNECT_WAIT_DURATION,
};
use koshi_ipc::wire::{MaybeKnown, WireName};
use uuid::Uuid;

use crate::error::CliError;
use crate::in_session::InSessionContext;
use crate::server_build::{find_refusing_server_build, RefusingServerBuild};
use crate::talk::{
    self, build_ipc_unavailable_error, build_peer_refusal_error, SessionHelloAnswer,
};

/// How long a session or the router has to answer once it is asked: a session
/// another local user started that `koshi list-sessions` asks to describe
/// itself, any session and the router that `koshi version` asks for its build,
/// the router that `koshi doctor` asks for its connections from another
/// machine, the session a stand-in clipboard tool hands a copy to, and a
/// session that answers the `Commit` of a request sent with an answer
/// deadline: the Quit that `koshi kill-session` sends and the Restart that
/// `koshi update` sends.
pub const SERVER_ANSWER_TIMEOUT_DURATION: Duration = Duration::from_secs(5);

/// The most sessions [`ask_sessions_at_once`] asks at the same time.
pub const MAX_SESSIONS_ASKED_AT_ONCE: usize = 16;

/// How long a wait for a session or the router that is replacing its own
/// process image pauses between reads of its endpoint file: 25 ms.
pub const RESTART_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(25);

/// How long the CLI waits, after a server of this user's refused this build's
/// protocol version, for that server to start replacing its own process
/// image: 3 s.
pub const REFUSED_SERVER_RESTART_START_WAIT_DURATION: Duration = Duration::from_secs(3);

/// How long the CLI and the router wait, after a server of this user's refused
/// the connection token its endpoint file carried, for that file to carry
/// another token: 5 s.
pub const REFUSED_TOKEN_ADVERTISE_WAIT_DURATION: Duration = Duration::from_secs(5);

/// The most sessions [`list_foreign_sessions`] lists for one owner, across
/// every folder of the shared directory.
#[cfg(unix)]
pub const MAX_SHARED_SESSION_COUNT_PER_OWNER: usize = 256;

/// The most directory entries one scan of the shared directory goes through:
/// the entries of the shared directory, and those of each user's folder the
/// scan opens. Reading one more stops [`list_foreign_sessions`] and
/// [`find_foreign_session_advertisement`].
#[cfg(unix)]
pub const MAX_SHARED_DIRECTORY_ENTRY_COUNT: usize = 65_536;

/// The most user folders one scan of the shared directory goes through.
/// Finding one more stops [`list_foreign_sessions`] and
/// [`find_foreign_session_advertisement`].
#[cfg(unix)]
pub const MAX_SHARED_USER_DIRECTORY_COUNT: usize = 256;

/// The private runtime directory holding every endpoint file, or
/// [`CliError::IpcUnavailable`] when the machine has none.
pub fn resolve_runtime_directory() -> Result<PathBuf, CliError> {
    koshi_paths::resolve_runtime_directory().ok_or_else(|| CliError::IpcUnavailable {
        detail: "no runtime directory found".to_string(),
    })
}

/// Submit `command` to the session this CLI runs inside and hand back the
/// dispatcher's result.
///
/// Reads the session's endpoint file, connects, writes the Hello and a
/// `Prepare` carrying the command back to back, reads the two replies in
/// order, then writes the `Commit` and reads the command's result. A missing
/// endpoint file or a socket nothing listens on reports the session as not
/// running ([`CliError::SessionNotFound`]). A Hello refused for its protocol
/// version is [`CliError::ProtocolVersionRefused`], one refused for its
/// connection token is [`CliError::ConnectionTokenRefused`], a reply in the
/// envelope of koshi 0.4.0 or older is [`CliError::PreviousReleaseServer`],
/// and every other failure to talk is [`CliError::IpcUnavailable`]. The result
/// itself — applied or rejected — comes back for the caller to map to an exit
/// code, with a rejection's hint filtered by [`sanitize_reported_text`].
///
/// A session of this user's that refuses this build's protocol version is
/// asked again once it has restarted, one that refuses the connection token is
/// asked again once it advertises another, and one met while it replaces its
/// process image is asked again through its new image, as
/// [`run_session_exchange_across_restart`] states. A session that refuses the
/// command because it is replacing its image, and has advertised no new
/// endpoint in time, is [`CliError::SessionRestarting`].
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
/// Naming one changes nothing about the exchange: the Hello and the `Prepare`
/// go out back to back, then the `Commit`, and it costs two round trips
/// either way. `owner_user_id` names the user whose session it is, as
/// [`load_session_endpoint`] reads it in `shared_sessions_base_directory`, the
/// shared directory the `koshi.kdl` layer the caller read at this process's
/// start names. `None` looks the session up in the runtime directory alone.
pub fn submit_external_command(
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
    owner_user_id: Option<u32>,
    client_id: Option<ClientId>,
    command: Command,
) -> Result<CommandResult, CliError> {
    submit_external_command_via_runtime_directory(
        &resolve_runtime_directory()?,
        shared_sessions_base_directory,
        session_id,
        owner_user_id,
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
    run_session_exchange_across_restart(
        runtime_directory,
        None,
        in_session_context.session_id,
        None,
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

/// What the session answers to one part of the copy that a stand-in
/// clipboard tool sends ([`open_clipboard_copy_exchange`],
/// [`ClipboardCopyExchange::send_copy_part`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardCopyAnswer {
    /// The session holds the part, and the tool sends the next part.
    PartTaken,
    /// The copy is over, and the session holds none of its bytes.
    Routed {
        /// `true` tells the tool to run the real clipboard tool with every
        /// byte of the copy: no client focuses the pane, a client on this
        /// machine focuses it, a client on another machine did not take the
        /// copy, `pane-clipboard-writes` is off, the session holds no such
        /// pane, or the copy passes `max-copy-bytes`. `false` means each
        /// client that focuses the pane is on another machine, and each one
        /// took the copy.
        should_run_clipboard_tool: bool,
    },
}

/// The copy of a stand-in clipboard tool, handed to the session of the
/// tool's pane part by part on the one connection that
/// [`open_clipboard_copy_exchange`] opened. Dropping it closes the
/// connection, and the session drops every part it holds.
pub struct ClipboardCopyExchange {
    /// The half of the connection that reads the session's answers.
    response_reader: FrameReader,
    /// The half of the connection that writes the parts.
    request_writer: FrameWriter,
    /// The request id of the last part sent.
    last_request_id: u64,
    /// The pane the stand-in clipboard tool runs in.
    pane_id: PaneId,
    /// The `OSC 52` targets the tool names, such as `c` for the clipboard.
    clipboard_targets: String,
    /// How long the session has to answer a part, counted from the call
    /// that sends the part.
    answer_timeout: Duration,
}

/// Open a connection to the session of the pane that a stand-in clipboard
/// tool runs in, send `first_copy_part`, the first part of the tool's copy
/// for `clipboard_targets`, and hand back the connection with the session's
/// answer.
///
/// The session and the pane are `in_session_context.session_id` and
/// `in_session_context.pane_id`, and the session's endpoint file is in
/// `runtime_directory`. The part goes as an [`IpcRequestKind::Prepare`]
/// carrying an [`IpcRequestKind::ClipboardCopy`] when `is_last_part`, and as an
/// [`IpcRequestKind::ClipboardCopyPart`] otherwise, with request id `2`. The
/// Hello and the part go out back to back, before either reply is read, through
/// [`run_session_exchange_across_restart`]: a session of this user's that
/// refuses this build's protocol version, or the connection token, or that is
/// met while it replaces its process image, gets the Hello and the same part
/// again once it has restarted or advertises another token. The exchange and
/// each wait end `answer_timeout` after the call. A `Prepared` answer is
/// followed by the `Commit` numbered `3`, whose answer has `answer_timeout`
/// from the moment it is written.
///
/// Example: the copy `hi` in one part opens with one `Prepare`. A pane that a
/// client on another machine focuses alone gets `Prepared`, then the `Commit`
/// gives `Routed { should_run_clipboard_tool: false }`.
///
/// # Errors
/// - What [`run_session_exchange_across_restart`] gives: a session that is
///   not running, a session that is restarting, and a write or read that
///   fails or does not end in time. The session then routes no byte of the
///   copy.
/// - [`CliError::CommittedRequestUnanswered`] when the `Commit` got no answer:
///   the session may route the copy.
/// - What [`build_peer_refusal_error`] gives for an answer that refuses the
///   part, such as `RequestFailed` from a session that refuses the copy, or
///   `UnsupportedKind` from one that does not know the request.
/// - [`CliError::IpcUnavailable`] naming the reply for every other answer
///   that is not `ClipboardCopyPartTaken` to a part that is not the last or
///   `ClipboardCopyRouted`.
pub fn open_clipboard_copy_exchange(
    runtime_directory: &Path,
    in_session_context: &InSessionContext,
    clipboard_targets: &str,
    first_copy_part: &[u8],
    is_last_part: bool,
    answer_timeout: Duration,
) -> Result<(ClipboardCopyExchange, ClipboardCopyAnswer), CliError> {
    let answer_deadline = Instant::now() + answer_timeout;
    let first_request_id = 2;
    run_session_exchange_across_restart(
        runtime_directory,
        None,
        in_session_context.session_id,
        None,
        Some(answer_deadline),
        |session_endpoint| {
            let (mut response_reader, mut request_writer, _, first_copy_result) =
                open_session_exchange(
                    session_endpoint,
                    in_session_context.session_id,
                    build_clipboard_copy_request(
                        first_request_id,
                        in_session_context.pane_id,
                        clipboard_targets,
                        first_copy_part,
                        is_last_part,
                    ),
                    Some(answer_deadline),
                )?;
            let first_copy_result = commit_prepared_clipboard_copy(
                &mut response_reader,
                &mut request_writer,
                first_request_id,
                is_last_part,
                first_copy_result,
                answer_timeout,
            )?;
            let first_copy_answer = parse_clipboard_copy_answer(first_copy_result, is_last_part)?;
            let copy_exchange = ClipboardCopyExchange {
                response_reader,
                request_writer,
                last_request_id: first_request_id,
                pane_id: in_session_context.pane_id,
                clipboard_targets: clipboard_targets.to_string(),
                answer_timeout,
            };
            Ok((copy_exchange, first_copy_answer))
        },
    )
}

impl ClipboardCopyExchange {
    /// Send `copy_part`, the next part of the copy, and hand back the
    /// session's answer. The caller sends a part only after the answer
    /// [`ClipboardCopyAnswer::PartTaken`] to the part before it.
    ///
    /// The part goes as an [`IpcRequestKind::Prepare`] carrying an
    /// [`IpcRequestKind::ClipboardCopy`] when `is_last_part`, and as an
    /// [`IpcRequestKind::ClipboardCopyPart`] otherwise, under the request id
    /// after the last part's. The write and the read of the answer end
    /// `answer_timeout` after the call. A `Prepared` answer is followed by a
    /// `Commit` under the next request id, whose answer has `answer_timeout`
    /// from the moment it is written.
    ///
    /// Example: the second part, sent `30` seconds after the first one, has
    /// its own `answer_timeout` and request id `3`.
    ///
    /// # Errors
    /// - [`CliError::SessionAnswerTimedOut`] for a failed write or read once
    ///   `answer_timeout` has passed, and what [`build_ipc_unavailable_error`]
    ///   gives for one before it. The session then routes no byte of the
    ///   copy.
    /// - [`CliError::CommittedRequestUnanswered`] when the `Commit` got no
    ///   answer: the session may route the copy.
    /// - What [`build_peer_refusal_error`] gives for an answer that refuses
    ///   the part.
    /// - [`CliError::IpcUnavailable`] naming the reply for every other answer
    ///   that is not `ClipboardCopyPartTaken` to a part that is not the last
    ///   or `ClipboardCopyRouted`.
    pub fn send_copy_part(
        &mut self,
        copy_part: &[u8],
        is_last_part: bool,
    ) -> Result<ClipboardCopyAnswer, CliError> {
        let answer_deadline = Instant::now() + self.answer_timeout;
        self.response_reader.set_deadline(Some(answer_deadline));
        self.request_writer.set_deadline(Some(answer_deadline));
        let build_exchange_error =
            |ipc_error: IpcError| build_session_exchange_error(ipc_error, Some(answer_deadline));
        self.last_request_id += 1;
        let copy_request = build_clipboard_copy_request(
            self.last_request_id,
            self.pane_id,
            &self.clipboard_targets,
            copy_part,
            is_last_part,
        );
        self.request_writer
            .send(&copy_request)
            .map_err(build_exchange_error)?;
        let copy_response: IncomingResponse = self
            .response_reader
            .recv_answer()
            .map_err(build_exchange_error)?;
        let copy_result = talk::SESSION_PEER_WORDS.take_response_result(copy_response)?;
        let copy_result = commit_prepared_clipboard_copy(
            &mut self.response_reader,
            &mut self.request_writer,
            self.last_request_id,
            is_last_part,
            copy_result,
            self.answer_timeout,
        )?;
        parse_clipboard_copy_answer(copy_result, is_last_part)
    }
}

/// [`submit_external_command`] against an explicit runtime directory: the whole
/// exchange, with the endpoint lookup rooted where the caller says.
/// `client_id` rides on the source.
///
/// `shared_sessions_base_directory` is searched for `session_id` when
/// `runtime_directory` holds no endpoint file for it, and `owner_user_id`
/// names the user whose session it is, through [`load_session_endpoint`].
///
/// A session of this user's that refuses this build's protocol version is
/// asked again once it has restarted, one that refuses the connection token is
/// asked again once it advertises another, and one met while it replaces its
/// process image is asked again through its new image, as
/// [`run_session_exchange_across_restart`] states.
pub fn submit_external_command_via_runtime_directory(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
    owner_user_id: Option<u32>,
    client_id: Option<ClientId>,
    command: Command,
) -> Result<CommandResult, CliError> {
    run_session_exchange_across_restart(
        runtime_directory,
        shared_sessions_base_directory,
        session_id,
        owner_user_id,
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
        _ => return command,
    };
    if working_directory.is_none() {
        *working_directory = std::env::current_dir().ok();
    }
    command
}

/// One command submission over `endpoint`: connect, send the Hello and the
/// enveloped command inside a `Prepare`, commit it, and read the command's
/// result, as [`exchange_prepared_session_request`] states. A pane-creating
/// command with no directory of its own gets this process's current
/// directory, and a rejection's hint is filtered by
/// [`filter_rejection_hint`](crate::talk::filter_rejection_hint).
///
/// With `answer_deadline`, the `Prepare` is answered by that moment, as
/// [`connect_to_session`] states, and the `Commit` has
/// [`SERVER_ANSWER_TIMEOUT_DURATION`] after it.
fn submit_command_envelope(
    endpoint: &EndpointFile,
    session_id: SessionId,
    command_source: CommandSource,
    command: Command,
    answer_deadline: Option<Instant>,
) -> Result<CommandResult, CliError> {
    let command_with_working_directory = apply_current_working_directory_to_command(command);
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        command_source,
        command_with_working_directory,
    );
    let submit_command_kind = IpcRequestKind::SubmitCommand {
        command_envelope: Box::new(command_envelope),
        fired_action: None,
    };
    let prepared_submit = exchange_prepared_session_request(
        endpoint,
        session_id,
        submit_command_kind,
        answer_deadline,
    )?;
    match prepared_submit.request_result? {
        IpcResult::CommandResult(command_result) => Ok(talk::filter_rejection_hint(command_result)),
        IpcResult::Error(refusal) => Err(build_peer_refusal_error(&refusal)),
        // An answer to the `Commit` that is not the command's result is
        // `CommittedRequestUnanswered`.
        unexpected_result => Err(CliError::CommittedRequestUnanswered {
            detail: talk::SESSION_PEER_WORDS
                .format_unexpected_reply_detail(unexpected_result.get_wire_name()),
        }),
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
///
/// # Errors
/// [`CliError::IpcUnavailable`] reading `session <asked id> described another
/// session, <answered id>` for an overview of any session but `session_id`.
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
            let answered_session_id = session_overview.session.session_id;
            if answered_session_id != session_id {
                return Err(CliError::IpcUnavailable {
                    detail: format!(
                        "session {session_id} described another session, {answered_session_id}"
                    ),
                });
            }
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
/// tab is described. The session name and each tab name come back filtered by
/// [`sanitize_reported_text`]: a tab named `"\u{1b}[2Jeditor"` reads back as
/// `"[2Jeditor"`.
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
/// asked again once it has restarted, one that refuses the connection token is
/// asked again once it advertises another, and one met while it replaces its
/// process image is asked again through its new image, as
/// [`run_session_exchange_across_restart`] states.
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
    let mut session_layout = run_session_exchange_across_restart(
        runtime_directory,
        shared_sessions_base_directory,
        session_id,
        None,
        None,
        |session_endpoint| match exchange_session_request(
            session_endpoint,
            session_id,
            ipc_request.clone(),
            None,
        )? {
            IpcResult::Layout(session_layout) => Ok(session_layout),
            IpcResult::Error(refusal) => Err(build_peer_refusal_error(&refusal)),
            unexpected_result => {
                Err(talk::SESSION_PEER_WORDS.build_unexpected_reply_error(&unexpected_result))
            }
        },
    )?;
    session_layout.session_name = sanitize_reported_text(&session_layout.session_name);
    for tab_layout in &mut session_layout.tabs {
        tab_layout.tab_name = sanitize_reported_text(&tab_layout.tab_name);
    }
    match tab_id {
        Some(tab_id) if session_layout.tabs.is_empty() => Err(CliError::CommandRejected {
            reason: RejectReason::TargetNotFound,
            help: Some(format!("no running session has tab {tab_id}")),
        }),
        _ => Ok(session_layout),
    }
}

/// Ask `session_id` for the events it published most recently, oldest first.
/// Each event name comes back filtered by [`sanitize_reported_text`].
///
/// A refusal carries the session's own sentence through.
///
/// `shared_sessions_base_directory` is searched for `session_id` when
/// `runtime_directory` holds no endpoint file for it, through
/// [`load_session_endpoint`].
///
/// A session of this user's that refuses this build's protocol version is
/// asked again once it has restarted, one that refuses the connection token is
/// asked again once it advertises another, and one met while it replaces its
/// process image is asked again through its new image, as
/// [`run_session_exchange_across_restart`] states.
pub fn fetch_recent_events(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
) -> Result<Vec<RecentEvent>, CliError> {
    let ipc_request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::RecentEvents,
    };
    let mut recent_events = run_session_exchange_across_restart(
        runtime_directory,
        shared_sessions_base_directory,
        session_id,
        None,
        None,
        |session_endpoint| match exchange_session_request(
            session_endpoint,
            session_id,
            ipc_request.clone(),
            None,
        )? {
            IpcResult::RecentEvents(recent_events) => Ok(recent_events),
            IpcResult::Error(refusal) => Err(build_peer_refusal_error(&refusal)),
            unexpected_result => {
                Err(talk::SESSION_PEER_WORDS.build_unexpected_reply_error(&unexpected_result))
            }
        },
    )?;
    for recent_event in &mut recent_events {
        recent_event.event_name = sanitize_reported_text(&recent_event.event_name).into();
    }
    Ok(recent_events)
}

/// The result of asking a running session to restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRestart {
    /// The session answered that it is replacing its own process image, or
    /// was already replacing it when the Restart reached it.
    Restarting {
        /// The process image that took the Restart, or refused it with
        /// `Restarting`, as its Hello named it. `None` for a session of koshi
        /// 0.3.0 or 0.4.0, whose Hello names no process image, and for a
        /// failure that [`is_ended_by_image_replacement`] accepts.
        asked_process_image_id: Option<ProcessImageId>,
    },
    /// Nothing advertises that session, or nothing listens behind the address
    /// it advertises. Nothing restarted.
    NotRunning,
    /// The session runs koshi 0.2.0 or one of its pre-releases, which has no
    /// restart request. Nothing restarted.
    WithoutRestartRequest,
    /// The session took the Restart and gave no answer: the write that handed
    /// it over, or the read of its answer, failed or ran out of time. The
    /// session may restart, or may have restarted.
    Unanswered {
        /// That failure, as [`IpcError::render_sanitized_message`] renders it.
        failure_detail: String,
        /// The process image that took the Restart, as its Hello named it.
        /// `None` for a session of koshi 0.3.0 or 0.4.0, whose Hello names no
        /// process image.
        asked_process_image_id: Option<ProcessImageId>,
    },
}

/// Ask the running session `session_id` to restart into the binary on disk.
///
/// Sends one Restart through `Prepare` and `Commit`, one more after each wait
/// stated below, and never starts a session. A session runs at most one of
/// them: a `Prepare` behind a refused Hello is refused. Every pane, its child process, its
/// terminal and its scrollback stay as they are. A client that was attached
/// attaches again and finds the session it left.
///
/// The connects, the waits, and every write and read before the `Commit` end
/// by `answer_deadline`, and a failure once it is reached is
/// [`CliError::SessionAnswerTimedOut`]: no `Commit` was written, and the
/// session runs no Restart. The `Commit` and its answer get
/// [`SERVER_ANSWER_TIMEOUT_DURATION`] more. A `Commit` that gets no answer
/// gives [`SessionRestart::Unanswered`].
///
/// A session that refuses the request with `Restarting` is already replacing
/// its image and gives [`SessionRestart::Restarting`]. So does every other
/// failure that [`is_ended_by_image_replacement`] accepts. An answer to the
/// `Commit` that is not a Restart answer gives [`SessionRestart::Unanswered`]
/// naming it.
///
/// A session that refuses the request otherwise gives what
/// [`build_peer_refusal_error`] builds from the sentence the session sent. A
/// session from this build refuses with `RequestFailed`, and a `v0.5.0-pr.1`
/// session refuses with `MalformedRequest`: both are
/// [`CliError::IpcUnavailable`]. A session that refuses this build's protocol
/// version at the Hello gives [`CliError::ProtocolVersionRefused`].
///
/// `shared_sessions_base_directory` is searched for `session_id` when
/// `runtime_directory` holds no endpoint file for it, through
/// [`load_session_endpoint`].
///
/// A session of this user's that refuses this build's protocol version is
/// asked again once it has restarted, and one that refuses the connection
/// token is asked again once it advertises another, as
/// [`run_session_exchange_with_restart_wait`] states. Example: a session that
/// restarted by itself into the program on disk is asked to restart once more,
/// and restarts into the same program.
///
/// A session that answers in the envelope of koshi 0.1.0 to 0.4.0
/// ([`CliError::PreviousReleaseServer`]) is asked again in the envelope of
/// koshi 0.2.0 to 0.4.0, with a Hello and a Restart, under the same
/// `answer_deadline`.
pub fn restart_running_session(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
    answer_deadline: Instant,
) -> Result<SessionRestart, CliError> {
    let restart_exchange = run_session_exchange_with_restart_wait(
        runtime_directory,
        shared_sessions_base_directory,
        session_id,
        None,
        Some(answer_deadline),
        |session_endpoint| {
            exchange_prepared_session_request(
                session_endpoint,
                session_id,
                IpcRequestKind::Restart,
                Some(answer_deadline),
            )
        },
    );
    let prepared_restart = match restart_exchange {
        Ok(prepared_restart) => prepared_restart,
        Err(restart_error)
            if is_ended_by_image_replacement(runtime_directory, session_id, &restart_error) =>
        {
            return Ok(SessionRestart::Restarting {
                asked_process_image_id: None,
            })
        }
        Err(CliError::SessionNotFound { .. }) => return Ok(SessionRestart::NotRunning),
        Err(CliError::PreviousReleaseServer { .. }) => {
            return restart_previous_release_session(
                runtime_directory,
                shared_sessions_base_directory,
                session_id,
                answer_deadline,
            )
        }
        Err(ipc_error) => return Err(ipc_error),
    };
    let asked_process_image_id = Some(prepared_restart.hello_answer.process_image_id);
    match prepared_restart.request_result {
        Ok(IpcResult::Restarting) => Ok(SessionRestart::Restarting {
            asked_process_image_id,
        }),
        Ok(IpcResult::Error(refusal)) if refusal.code == IpcErrorCode::Restarting => {
            Ok(SessionRestart::Restarting {
                asked_process_image_id,
            })
        }
        Ok(IpcResult::Error(refusal)) => Err(build_peer_refusal_error(&refusal)),
        Ok(unexpected_result) => Ok(SessionRestart::Unanswered {
            failure_detail: talk::SESSION_PEER_WORDS
                .format_unexpected_reply_detail(unexpected_result.get_wire_name()),
            asked_process_image_id,
        }),
        Err(CliError::CommittedRequestUnanswered { detail }) => Ok(SessionRestart::Unanswered {
            failure_detail: detail,
            asked_process_image_id,
        }),
        Err(ipc_error) => Err(ipc_error),
    }
}

/// Ask the session `session_id`, which koshi 0.2.0 to 0.4.0 started, to
/// restart into the program file it started from. The Hello of
/// [`PreviousReleaseRequestKind::build_hello_request`] and the Restart go out
/// back to back, in the envelope of that release.
///
/// - `Restarting`, which koshi 0.3.0 and 0.4.0 answer, gives
///   [`SessionRestart::Restarting`].
/// - A Restart refused with `unsupported_kind`, which koshi 0.2.0 answers, and
///   a Hello refused with `malformed_request`, which `v0.2.0-pr.1` answers,
///   give [`SessionRestart::WithoutRestartRequest`].
/// - A session no endpoint file advertises, or one nothing listens behind,
///   gives [`SessionRestart::NotRunning`].
/// - A Restart write that fails, and a read of either answer that fails, give
///   [`SessionRestart::Unanswered`] carrying that failure: a session of those
///   releases runs a Restart once it reads it.
///
/// The connect, the writes and the reads end by `answer_deadline`.
/// `shared_sessions_base_directory` is searched as [`load_session_endpoint`]
/// searches it.
///
/// # Errors
/// [`CliError::IpcUnavailable`] for every other refusal, carrying its sentence
/// filtered by [`sanitize_reported_text`], and for an answer of the wrong kind,
/// as [`build_previous_release_answer_error`] states. The failure of the
/// endpoint file read, the connect or the Hello write, which
/// [`build_session_exchange_error`] names: [`CliError::SessionAnswerTimedOut`]
/// once `answer_deadline` is reached.
fn restart_previous_release_session(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
    answer_deadline: Instant,
) -> Result<SessionRestart, CliError> {
    let Some((mut response_reader, mut request_writer, connection_token)) =
        connect_to_previous_release_session(
            runtime_directory,
            shared_sessions_base_directory,
            session_id,
            answer_deadline,
        )?
    else {
        return Ok(SessionRestart::NotRunning);
    };
    send_previous_release_request(
        &mut request_writer,
        1,
        PreviousReleaseRequestKind::build_hello_request(connection_token),
    )
    .map_err(|ipc_error| build_session_exchange_error(ipc_error, Some(answer_deadline)))?;
    let build_unanswered_restart = |ipc_error: IpcError| SessionRestart::Unanswered {
        failure_detail: ipc_error.render_sanitized_message(),
        asked_process_image_id: None,
    };
    if let Err(restart_write_error) =
        send_previous_release_request(&mut request_writer, 2, PreviousReleaseRequestKind::Restart)
    {
        return Ok(build_unanswered_restart(restart_write_error));
    }
    let hello_answer = match read_previous_release_result(&mut response_reader) {
        Ok(hello_answer) => hello_answer,
        Err(hello_read_error) => return Ok(build_unanswered_restart(hello_read_error)),
    };
    match hello_answer {
        PreviousReleaseResult::Hello { .. } => {}
        PreviousReleaseResult::Error(refusal)
            if refusal.code == PreviousReleaseErrorCode::MalformedRequest =>
        {
            return Ok(SessionRestart::WithoutRestartRequest);
        }
        hello_answer => return Err(build_previous_release_answer_error(hello_answer)),
    }
    match read_previous_release_result(&mut response_reader) {
        Ok(PreviousReleaseResult::Restarting) => Ok(SessionRestart::Restarting {
            asked_process_image_id: None,
        }),
        Ok(PreviousReleaseResult::Error(refusal))
            if refusal.code == PreviousReleaseErrorCode::UnsupportedKind =>
        {
            Ok(SessionRestart::WithoutRestartRequest)
        }
        Ok(restart_answer) => Err(build_previous_release_answer_error(restart_answer)),
        Err(restart_read_error) => Ok(build_unanswered_restart(restart_read_error)),
    }
}

/// The build the session `session_id`, which koshi 0.2.0 to 0.4.0 started,
/// names in its answer to the Hello of that release, filtered by
/// [`sanitize_reported_text`], such as `0.4.0`.
///
/// `Ok(None)` means no session is running under that id. An empty string is
/// the answer of koshi 0.2.0, which names no build. Sends nothing besides the
/// Hello.
///
/// The connect, the write and the read end by `answer_deadline`.
///
/// # Errors
/// [`CliError::IpcUnavailable`] for a refused Hello, carrying its sentence
/// filtered by [`sanitize_reported_text`], and for a `Restarting` answer:
/// `the session answered with an unexpected Restarting reply`. The failure of
/// the endpoint file read, the connect, the write or the read:
/// [`CliError::SessionAnswerTimedOut`] once `answer_deadline` is reached, and
/// what [`build_ipc_unavailable_error`] gives otherwise.
pub fn find_previous_release_session_version(
    runtime_directory: &Path,
    session_id: SessionId,
    answer_deadline: Instant,
) -> Result<Option<String>, CliError> {
    let Some((mut response_reader, mut request_writer, connection_token)) =
        connect_to_previous_release_session(runtime_directory, None, session_id, answer_deadline)?
    else {
        return Ok(None);
    };
    let build_exchange_error =
        |ipc_error: IpcError| build_session_exchange_error(ipc_error, Some(answer_deadline));
    send_previous_release_request(
        &mut request_writer,
        1,
        PreviousReleaseRequestKind::build_hello_request(connection_token),
    )
    .map_err(build_exchange_error)?;
    match read_previous_release_result(&mut response_reader).map_err(build_exchange_error)? {
        PreviousReleaseResult::Hello { build_version } => {
            Ok(Some(sanitize_reported_text(&build_version)))
        }
        hello_answer => Err(build_previous_release_answer_error(hello_answer)),
    }
}

/// The reading half and the writing half of a connection to the session
/// `session_id`, found as [`load_session_endpoint`] finds it, with the token
/// its endpoint file carries. Both halves keep `answer_deadline`. `Ok(None)`
/// means no endpoint file advertises the session, or nothing listens behind
/// it. The connect waits as [`connect_to_session`] states.
///
/// # Errors
/// The failure of the endpoint file read or of the connect.
fn connect_to_previous_release_session(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
    answer_deadline: Instant,
) -> Result<Option<(FrameReader, FrameWriter, ConnectionToken)>, CliError> {
    let session_endpoint = match load_session_endpoint(
        runtime_directory,
        shared_sessions_base_directory,
        session_id,
        None,
    ) {
        Ok(session_endpoint) => session_endpoint,
        Err(CliError::SessionNotFound { .. }) => return Ok(None),
        Err(load_error) => return Err(load_error),
    };
    match connect_to_session(&session_endpoint, session_id, Some(answer_deadline)) {
        Ok(session_connection) => {
            let (mut response_reader, mut request_writer) = session_connection.split();
            response_reader.set_deadline(Some(answer_deadline));
            request_writer.set_deadline(Some(answer_deadline));
            Ok(Some((
                response_reader,
                request_writer,
                session_endpoint.connection_token,
            )))
        }
        Err(CliError::SessionNotFound { .. }) => Ok(None),
        Err(connect_error) => Err(connect_error),
    }
}

/// Write `request_kind` as request `request_id` through `request_writer`, in
/// the envelope of koshi 0.2.0 to 0.4.0.
fn send_previous_release_request(
    request_writer: &mut FrameWriter,
    request_id: u64,
    request_kind: PreviousReleaseRequestKind,
) -> Result<(), IpcError> {
    request_writer.send(&PreviousReleaseRequest {
        request_id,
        request_kind,
    })
}

/// Read the next answer from `response_reader`, in the envelope of koshi
/// 0.2.0 to 0.4.0.
fn read_previous_release_result(
    response_reader: &mut FrameReader,
) -> Result<PreviousReleaseResult, IpcError> {
    let previous_release_answer: PreviousReleaseAnswer = response_reader.recv()?;
    Ok(previous_release_answer.answer_result)
}

/// The failure for `previous_release_result`, an answer that is not the one
/// its request asks for: a refusal is [`CliError::IpcUnavailable`] carrying
/// its sentence filtered by [`sanitize_reported_text`], and a `Hello` or a
/// `Restarting` is the failure
/// [`PeerWords::build_unexpected_wire_name_error`](talk::PeerWords::build_unexpected_wire_name_error)
/// names, such as `the session answered with an unexpected Restarting reply`.
fn build_previous_release_answer_error(previous_release_result: PreviousReleaseResult) -> CliError {
    match previous_release_result {
        PreviousReleaseResult::Error(refusal) => CliError::IpcUnavailable {
            detail: sanitize_reported_text(&refusal.message),
        },
        PreviousReleaseResult::Hello { .. } => {
            talk::SESSION_PEER_WORDS.build_unexpected_wire_name_error("Hello")
        }
        PreviousReleaseResult::Restarting => {
            talk::SESSION_PEER_WORDS.build_unexpected_wire_name_error("Restarting")
        }
    }
}

/// The build version the running session `session_id` reports in its Hello
/// answer, as [`find_running_session_hello_answer`] reads it.
///
/// # Errors
/// What [`find_running_session_hello_answer`] gives.
pub fn find_running_session_version(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
    owner_user_id: Option<u32>,
    answer_deadline: Option<Instant>,
) -> Result<Option<String>, CliError> {
    let session_hello_answer = find_running_session_hello_answer(
        runtime_directory,
        shared_sessions_base_directory,
        session_id,
        owner_user_id,
        answer_deadline,
    )?;
    Ok(session_hello_answer.map(|session_hello_answer| session_hello_answer.build_version))
}

/// The build and the process image the running session `session_id` names in
/// its Hello answer.
///
/// `Ok(None)` means no session is running under that id. Sends nothing besides
/// the Hello. With `answer_deadline`, the connect, the write and the read end
/// by that moment, as [`connect_to_session`] states, and a failure once it is
/// reached is [`CliError::SessionAnswerTimedOut`].
///
/// `shared_sessions_base_directory` is searched for `session_id` when
/// `runtime_directory` holds no endpoint file for it, and `owner_user_id`
/// names the user whose session it is, through [`load_session_endpoint`]. A
/// session that refuses the connection token is asked once more, as
/// [`run_session_exchange_with_token_wait`] states.
///
/// # Errors
/// The failure of the endpoint file read, a refused Hello, as
/// [`talk::parse_session_hello_answer`] states, and every failure to talk.
pub fn find_running_session_hello_answer(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
    owner_user_id: Option<u32>,
    answer_deadline: Option<Instant>,
) -> Result<Option<SessionHelloAnswer>, CliError> {
    match load_session_endpoint(
        runtime_directory,
        shared_sessions_base_directory,
        session_id,
        owner_user_id,
    ) {
        Ok(session_endpoint) => {
            let (_, session_hello_result) = run_session_exchange_with_token_wait(
                runtime_directory,
                session_id,
                session_endpoint,
                answer_deadline,
                |session_endpoint| {
                    find_session_hello_answer_from_endpoint(
                        session_endpoint,
                        session_id,
                        answer_deadline,
                    )
                },
            );
            session_hello_result
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
    let session_hello_answer = find_session_hello_answer_from_endpoint(
        &build_foreign_session_endpoint(socket_address.to_string()),
        session_id,
        Some(answer_deadline),
    )?;
    Ok(session_hello_answer.map(|session_hello_answer| session_hello_answer.build_version))
}

/// What the session `session_id` at `session_endpoint` names in its Hello
/// answer, or `Ok(None)` when nothing listens there. Sends nothing besides the
/// Hello, and `answer_deadline` bounds the connect, the write and the read as
/// [`find_running_session_hello_answer`] states.
fn find_session_hello_answer_from_endpoint(
    session_endpoint: &EndpointFile,
    session_id: SessionId,
    answer_deadline: Option<Instant>,
) -> Result<Option<SessionHelloAnswer>, CliError> {
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
    let hello_response: IncomingResponse = response_reader
        .recv_answer()
        .map_err(build_exchange_error)?;
    talk::parse_session_hello_answer(hello_response).map(Some)
}

/// What `ask_session` answers for each of `session_asks`, in the order of
/// `session_asks`.
///
/// Up to [`MAX_SESSIONS_ASKED_AT_ONCE`] sessions are asked at the same time:
/// the calling thread asks, and so does each thread it starts beside it, one
/// fewer than that limit at most. A thread that cannot start leaves its share
/// to the threads that run. With `Some(answer_deadline)`, a session still
/// unasked once `answer_deadline` is reached is not asked, and answers
/// [`CliError::SessionAnswerTimedOut`]. With `None`, every session is asked. A
/// panic in `ask_session` reaches the caller.
///
/// Example: 40 sessions, 3 of them stopped with `SIGSTOP`, are asked by 16
/// threads at once. The 3 stopped ones hold one thread each until
/// `answer_deadline`, and the other 37 are answered by the other 13 threads
/// meanwhile.
pub fn ask_sessions_at_once<SessionAsk: Sync, SessionAnswer: Send>(
    session_asks: &[SessionAsk],
    answer_deadline: Option<Instant>,
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
            let session_answer = if answer_deadline
                .is_some_and(|answer_deadline| Instant::now() >= answer_deadline)
            {
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
/// token other than `replaced_connection_token`, or under any token when it is
/// `None`, and hand that endpoint file back. `None` when `restart_deadline`
/// passes with no such file advertised.
///
/// A session server mints a fresh token every time it binds: a token other
/// than `replaced_connection_token` is the session's new image serving. The
/// process id in the file is not compared; on Unix, `execvp` keeps it.
///
/// The file is read every [`RESTART_POLL_INTERVAL_DURATION`] until the
/// deadline, and the last pause ends at the deadline. A missing or unreadable
/// file is read again. The first read happens before the deadline is checked:
/// with a deadline already passed, that one read still takes a session that is
/// already back.
#[must_use]
pub fn wait_for_new_session_endpoint(
    runtime_directory: &Path,
    session_id: SessionId,
    replaced_connection_token: Option<&ConnectionToken>,
    restart_deadline: Instant,
) -> Option<EndpointFile> {
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
    loop {
        if let Ok(endpoint) = EndpointFile::load_from_path(&endpoint_path) {
            if replaced_connection_token != Some(&endpoint.connection_token) {
                return Some(endpoint);
            }
        }
        if Instant::now() >= restart_deadline {
            return None;
        }
        std::thread::sleep(compute_poll_pause_duration(restart_deadline));
    }
}

/// The endpoint the session `session_id` advertises once it has restarted,
/// after it refused this build's protocol version over `refused_endpoint`
/// with the sentence `refusal_detail`.
///
/// Only a session of this user's is waited for: one that `refused_endpoint`
/// names with a token that is not empty, whose endpoint file in
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
    if refused_endpoint.connection_token.is_empty()
        || !is_own_session(runtime_directory, session_id)
    {
        return Err(CliError::ProtocolVersionRefused {
            detail: refusal_detail,
        });
    }
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
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
    let start_deadline = bound_by_answer_deadline(
        Instant::now() + REFUSED_SERVER_RESTART_START_WAIT_DURATION,
        answer_deadline,
    );
    loop {
        if let Ok(endpoint) = EndpointFile::load_from_path(&endpoint_path) {
            if endpoint.connection_token != refused_endpoint.connection_token {
                return Ok(endpoint);
            }
        }
        if is_replacing_its_image(runtime_directory, session_id) {
            let restart_deadline =
                bound_by_answer_deadline(Instant::now() + RESTART_WINDOW_DURATION, answer_deadline);
            if let Some(restarted_endpoint) = wait_for_new_session_endpoint(
                runtime_directory,
                session_id,
                Some(&refused_endpoint.connection_token),
                restart_deadline,
            ) {
                return Ok(restarted_endpoint);
            }
            break;
        }
        if Instant::now() >= start_deadline {
            break;
        }
        std::thread::sleep(compute_poll_pause_duration(start_deadline));
    }
    Err(build_refusal_error(&find_refusing_server_build(
        &program_file_path,
        refused_endpoint.process_id,
    )))
}

/// Run `make_exchange` over the endpoint of the session `session_id` of the
/// user `owner_user_id` names, found as [`load_session_endpoint`] finds it,
/// through [`run_session_exchange_with_token_wait`]. An exchange that ends in
/// [`CliError::ProtocolVersionRefused`] runs once more, over the endpoint
/// [`wait_for_refused_session_restart`] hands back, and that wait's failure is
/// what this gives otherwise. `answer_deadline` bounds each wait.
///
/// # Errors
/// What [`load_session_endpoint`] gives, then what the last `make_exchange`
/// gives, or the failure of the wait.
pub fn run_session_exchange_with_restart_wait<ExchangeAnswer>(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
    owner_user_id: Option<u32>,
    answer_deadline: Option<Instant>,
    mut make_exchange: impl FnMut(&EndpointFile) -> Result<ExchangeAnswer, CliError>,
) -> Result<ExchangeAnswer, CliError> {
    let session_endpoint = load_session_endpoint(
        runtime_directory,
        shared_sessions_base_directory,
        session_id,
        owner_user_id,
    )?;
    let (exchanged_endpoint, exchange_result) = run_session_exchange_with_token_wait(
        runtime_directory,
        session_id,
        session_endpoint,
        answer_deadline,
        &mut make_exchange,
    );
    match exchange_result {
        Err(CliError::ProtocolVersionRefused {
            detail: refusal_detail,
        }) => {
            let restarted_endpoint = wait_for_refused_session_restart(
                runtime_directory,
                session_id,
                &exchanged_endpoint,
                refusal_detail,
                answer_deadline,
            )?;
            make_exchange(&restarted_endpoint)
        }
        exchange_result => exchange_result,
    }
}

/// Run `make_exchange` over the endpoint of the session `session_id` of the
/// user `owner_user_id` names through
/// [`run_session_exchange_with_restart_wait`], and run it once more over the
/// endpoint [`wait_for_restarted_session_endpoint`] hands back when that
/// exchange met the session while it replaced its process image. A session
/// named with an `owner_user_id` is asked once.
///
/// A request this runs once more did not run: `make_exchange` gives a refusal
/// or an end of the exchange before any `Commit` as an error, and
/// [`CliError::CommittedRequestUnanswered`] for every failure after one, which
/// is never run again.
///
/// Example: `koshi action write-chars hi` reaches a session that has started
/// to replace its image. The session refuses the `Prepare` with `Restarting`,
/// its new image advertises a fresh endpoint 300 ms after the refusal, and
/// `hi` reaches the pane through that image.
///
/// # Errors
/// What the last exchange gives. When no restarted endpoint comes, what the
/// first exchange gave.
pub fn run_session_exchange_across_restart<ExchangeAnswer>(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
    owner_user_id: Option<u32>,
    answer_deadline: Option<Instant>,
    mut make_exchange: impl FnMut(&EndpointFile) -> Result<ExchangeAnswer, CliError>,
) -> Result<ExchangeAnswer, CliError> {
    let mut presented_connection_token: Option<ConnectionToken> = None;
    let first_exchange_result = run_session_exchange_with_restart_wait(
        runtime_directory,
        shared_sessions_base_directory,
        session_id,
        owner_user_id,
        answer_deadline,
        |session_endpoint| {
            presented_connection_token = Some(session_endpoint.connection_token.clone());
            make_exchange(session_endpoint)
        },
    );
    if owner_user_id.is_some() {
        return first_exchange_result;
    }
    run_exchange_again_after_restart(
        runtime_directory,
        session_id,
        presented_connection_token.as_ref(),
        answer_deadline,
        first_exchange_result,
        make_exchange,
    )
}

/// Run `make_exchange` once more over the endpoint
/// [`wait_for_restarted_session_endpoint`] hands back when
/// `first_exchange_result`, the end of an exchange with the session
/// `session_id` that presented `presented_connection_token`, met that session
/// while it replaced its process image.
///
/// # Errors
/// What the second exchange gives. With no second exchange, the error of
/// `first_exchange_result`.
pub fn run_exchange_again_after_restart<ExchangeAnswer>(
    runtime_directory: &Path,
    session_id: SessionId,
    presented_connection_token: Option<&ConnectionToken>,
    answer_deadline: Option<Instant>,
    first_exchange_result: Result<ExchangeAnswer, CliError>,
    make_exchange: impl FnOnce(&EndpointFile) -> Result<ExchangeAnswer, CliError>,
) -> Result<ExchangeAnswer, CliError> {
    let Err(exchange_error) = &first_exchange_result else {
        return first_exchange_result;
    };
    match wait_for_restarted_session_endpoint(
        runtime_directory,
        session_id,
        presented_connection_token,
        exchange_error,
        answer_deadline,
    ) {
        Some(restarted_endpoint) => make_exchange(&restarted_endpoint),
        None => first_exchange_result,
    }
}

/// The endpoint to run an exchange with the session `session_id` once more
/// over, after the exchange that presented `presented_connection_token` met
/// the session while it replaced its process image and ended in
/// `exchange_error`. `None` presented token means no endpoint file was read.
/// The empty presented token, which another user's session is asked with,
/// gives `None` at once.
///
/// When [`is_ended_by_image_replacement`] accepts `exchange_error`, the wait
/// is [`wait_for_new_session_endpoint`] for a token other than the presented
/// one, for up to [`RESTART_WINDOW_DURATION`] and at most until
/// `answer_deadline`.
///
/// `None` for every other failure, for a wait that ends with nothing
/// advertised, and for a wait that ends less than 1 ms before
/// `answer_deadline`.
pub fn wait_for_restarted_session_endpoint(
    runtime_directory: &Path,
    session_id: SessionId,
    presented_connection_token: Option<&ConnectionToken>,
    exchange_error: &CliError,
    answer_deadline: Option<Instant>,
) -> Option<EndpointFile> {
    if presented_connection_token.is_some_and(ConnectionToken::is_empty)
        || !is_ended_by_image_replacement(runtime_directory, session_id, exchange_error)
    {
        return None;
    }
    let restart_deadline =
        bound_by_answer_deadline(Instant::now() + RESTART_WINDOW_DURATION, answer_deadline);
    let restarted_endpoint = wait_for_new_session_endpoint(
        runtime_directory,
        session_id,
        presented_connection_token,
        restart_deadline,
    )?;
    if is_answer_deadline_reached(answer_deadline) {
        return None;
    }
    Some(restarted_endpoint)
}

/// Whether `exchange_error`, the end of an exchange with the session
/// `session_id`, met that session while it replaced its process image:
/// [`CliError::SessionRestarting`], or [`CliError::IpcUnavailable`] or
/// [`CliError::SessionNotFound`] while [`is_replacing_its_image`] accepts the
/// session in `runtime_directory`. An exchange that ended so ran no request
/// past its `Commit`.
///
/// Example: a connection that ends before its answer, 200 ms after the
/// session wrote its resume file, gives `true`. The same end with no resume
/// file gives `false`.
#[must_use]
pub fn is_ended_by_image_replacement(
    runtime_directory: &Path,
    session_id: SessionId,
    exchange_error: &CliError,
) -> bool {
    match exchange_error {
        CliError::SessionRestarting { .. } => true,
        CliError::IpcUnavailable { .. } | CliError::SessionNotFound { .. } => {
            is_replacing_its_image(runtime_directory, session_id)
        }
        _ => false,
    }
}

/// Run `make_exchange` over `session_endpoint`, the endpoint the session
/// `session_id` advertised, and hand back the endpoint the last exchange ran
/// over with what that exchange gave.
///
/// An exchange that gives [`CliError::ConnectionTokenRefused`] met a session
/// that does not accept the token `session_endpoint` carries. For a session of
/// this user's, one whose endpoint file in `runtime_directory` can be read or
/// that [`is_replacing_its_image`] accepts, this waits with
/// [`wait_for_new_session_endpoint`] for an endpoint file under another token,
/// for up to [`REFUSED_TOKEN_ADVERTISE_WAIT_DURATION`] and at most until
/// `answer_deadline`, then runs `make_exchange` once more over that file when
/// 1 ms or more is left until `answer_deadline`. The refusal of another user's
/// session, which `session_endpoint` names with the empty token, a refusal
/// whose wait ends with the token unchanged, and a refusal whose wait ends
/// with less than 1 ms left until `answer_deadline` are handed back as they
/// are. Example: a session that restarted in place accepts its
/// fresh token before it writes its endpoint file; a command that read the file
/// before the write is refused, then runs once more under the fresh token.
pub fn run_session_exchange_with_token_wait<ExchangeAnswer>(
    runtime_directory: &Path,
    session_id: SessionId,
    session_endpoint: EndpointFile,
    answer_deadline: Option<Instant>,
    mut make_exchange: impl FnMut(&EndpointFile) -> Result<ExchangeAnswer, CliError>,
) -> (EndpointFile, Result<ExchangeAnswer, CliError>) {
    let exchange_result = make_exchange(&session_endpoint);
    if !matches!(
        exchange_result,
        Err(CliError::ConnectionTokenRefused { .. })
    ) || session_endpoint.connection_token.is_empty()
        || !is_own_session(runtime_directory, session_id)
    {
        return (session_endpoint, exchange_result);
    }
    let advertise_deadline = bound_by_answer_deadline(
        Instant::now() + REFUSED_TOKEN_ADVERTISE_WAIT_DURATION,
        answer_deadline,
    );
    match wait_for_new_session_endpoint(
        runtime_directory,
        session_id,
        Some(&session_endpoint.connection_token),
        advertise_deadline,
    ) {
        Some(advertised_endpoint) if !is_answer_deadline_reached(answer_deadline) => {
            let exchange_result = make_exchange(&advertised_endpoint);
            (advertised_endpoint, exchange_result)
        }
        Some(_) | None => (session_endpoint, exchange_result),
    }
}

/// Whether the session `session_id` is this user's: its endpoint file in
/// `runtime_directory` can be read, or [`is_replacing_its_image`] accepts it.
fn is_own_session(runtime_directory: &Path, session_id: SessionId) -> bool {
    EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .is_ok()
        || is_replacing_its_image(runtime_directory, session_id)
}

/// `wait_end`, or `answer_deadline` when one is given and comes first.
pub(crate) fn bound_by_answer_deadline(
    wait_end: Instant,
    answer_deadline: Option<Instant>,
) -> Instant {
    match answer_deadline {
        Some(answer_deadline) => wait_end.min(answer_deadline),
        None => wait_end,
    }
}

/// The pause before a wait reads its endpoint file again:
/// [`RESTART_POLL_INTERVAL_DURATION`], or the time left until `wait_deadline`
/// when less is left. `0` once `wait_deadline` has passed. Example: with 10 ms
/// left, the pause is 10 ms.
pub(crate) fn compute_poll_pause_duration(wait_deadline: Instant) -> Duration {
    RESTART_POLL_INTERVAL_DURATION.min(wait_deadline.saturating_duration_since(Instant::now()))
}

/// The failure an exchange with the session `session_id` ends in while that
/// session replaces its process image: [`CliError::SessionRestarting`] reading
/// `session session-<uuid> is restarting; ask again in a moment`.
#[must_use]
pub fn build_session_restarting_error(session_id: SessionId) -> CliError {
    CliError::SessionRestarting {
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
/// advertises, as `list_foreign_sessions` reads them on Linux and macOS. On
/// Windows no listing is read, and every listing is empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForeignSessionListing {
    /// Each listed session's id and where the shared directory advertises
    /// it, in no particular order. No id is here twice.
    pub foreign_sessions: Vec<(SessionId, ForeignSessionAdvertisement)>,
    /// Each session id more than one socket advertises, in no particular
    /// order.
    pub duplicated_sessions: Vec<DuplicatedForeignSession>,
    /// How many more entries named like a session the shared directory holds
    /// past `MAX_SHARED_SESSION_COUNT_PER_OWNER` of one owner. Each is counted
    /// by its name alone. An entry after a listing stops at
    /// `MAX_SHARED_DIRECTORY_ENTRY_COUNT` or `MAX_SHARED_USER_DIRECTORY_COUNT`
    /// is not read, and not counted.
    pub unlisted_session_count: usize,
    /// The first read that failed while listing, other than at a path that
    /// advertises nothing. That failure is written to the log once, at warn
    /// level. Each failure after it is neither kept nor logged. `None` when
    /// every read succeeded. The sessions under that path are neither listed
    /// nor counted, and a runtime directory that cannot be read lists no
    /// session at all. A listing that stops at
    /// `MAX_SHARED_DIRECTORY_ENTRY_COUNT` or `MAX_SHARED_USER_DIRECTORY_COUNT`
    /// counts as a failed read of the shared directory.
    pub unread_path: Option<UnreadPath>,
}

impl ForeignSessionListing {
    /// Keep `read_error`, from reading `looked_up_path`, through
    /// [`record_unread_path`](Self::record_unread_path), unless
    /// [`is_unadvertised_path_error`] accepts it.
    #[cfg(unix)]
    fn record_failed_read(&mut self, looked_up_path: &Path, read_error: &std::io::Error) {
        if !is_unadvertised_path_error(read_error) {
            self.record_unread_path(UnreadPath::from_read_error(looked_up_path, read_error));
        }
    }

    /// Keep `unread_path` as [`unread_path`](Self::unread_path), and log one
    /// warning naming it, unless a failure is already kept.
    #[cfg(unix)]
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
/// [`MAX_SHARED_DIRECTORY_ENTRY_COUNT`], [`MAX_SHARED_USER_DIRECTORY_COUNT`]
/// and [`MAX_SHARED_SESSION_COUNT_PER_OWNER`].
#[cfg(unix)]
#[derive(Default)]
struct SharedDirectoryScan {
    /// Entries read so far: of the shared directory, and of each user's folder
    /// opened.
    read_entry_count: usize,
    /// User folders found so far.
    found_user_directory_count: usize,
    /// Entries named like a session checked so far, by the user id that owns
    /// the folder holding them.
    checked_session_count_by_owner: HashMap<u32, usize>,
}

#[cfg(unix)]
impl SharedDirectoryScan {
    /// Count one more entry named like a session in a folder that
    /// `owner_user_id` owns. `true` while that owner has at most
    /// [`MAX_SHARED_SESSION_COUNT_PER_OWNER`] such entries counted, and
    /// `false` for each one past it, which is not counted.
    fn count_checked_session(&mut self, owner_user_id: u32) -> bool {
        let checked_session_count = self
            .checked_session_count_by_owner
            .entry(owner_user_id)
            .or_insert(0);
        if *checked_session_count == MAX_SHARED_SESSION_COUNT_PER_OWNER {
            return false;
        }
        *checked_session_count += 1;
        true
    }

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
#[cfg(unix)]
fn is_unadvertised_path_error(read_error: &std::io::Error) -> bool {
    matches!(
        read_error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::NotADirectory
            | std::io::ErrorKind::PermissionDenied
    )
}

/// Where the shared directory advertises one session another local user
/// started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignSessionAdvertisement {
    /// The user id that owns the folder and the socket advertising the
    /// session, such as `1001`.
    pub owner_user_id: u32,
    /// The control-socket address reaching the session.
    pub socket_address: String,
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
    /// The lowest of `owner_user_ids` whose sockets advertise the id once:
    /// `<user id>/<session id>` reaches that one socket. `None` when every
    /// owner's sockets advertise it more than once.
    pub reachable_owner_user_id: Option<u32>,
}

impl DuplicatedForeignSession {
    /// The duplicate `session_id` reads as, from each of its `advertisements`.
    /// Example: advertisements by the owners `[1002, 1001, 1002]` give
    /// `advertisement_count` 3, `owner_user_ids` `[1001, 1002]` and
    /// `reachable_owner_user_id` `Some(1001)`.
    #[cfg(unix)]
    fn from_advertisements(
        session_id: SessionId,
        advertisements: &[ForeignSessionAdvertisement],
    ) -> DuplicatedForeignSession {
        let mut advertising_owner_user_ids: Vec<u32> = advertisements
            .iter()
            .map(|advertisement| advertisement.owner_user_id)
            .collect();
        advertising_owner_user_ids.sort_unstable();
        let reachable_owner_user_id = advertising_owner_user_ids
            .chunk_by(|first_owner_user_id, second_owner_user_id| {
                first_owner_user_id == second_owner_user_id
            })
            .find(|owner_advertisements| owner_advertisements.len() == 1)
            .map(|owner_advertisements| owner_advertisements[0]);
        advertising_owner_user_ids.dedup();
        DuplicatedForeignSession {
            session_id,
            advertisement_count: advertisements.len(),
            owner_user_ids: advertising_owner_user_ids,
            reachable_owner_user_id,
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
    /// by user ids 1001, 1002; koshi reaches none of them; name the owner to
    /// reach one, such as 1001/session-<uuid>`. One owner reads `by user id
    /// 1001`. The example names `reachable_owner_user_id`. With none, the
    /// sentence ends at `none of them`.
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
        )?;
        let Some(reachable_owner_user_id) = self.reachable_owner_user_id else {
            return Ok(());
        };
        write!(
            formatter,
            "; name the owner to reach one, such as {}",
            format_session_target(self.session_id, Some(reachable_owner_user_id))
        )
    }
}

/// Why `find_foreign_session_advertisement` reaches no socket for a session
/// id.
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
/// entries, counting those of the shared directory and those of every user's
/// folder it opens. It also goes through at most
/// [`MAX_SHARED_USER_DIRECTORY_COUNT`] user folders. Reading one more entry,
/// or finding one more folder, stops the listing: the sessions found before
/// the stop are listed, nothing after it is read, and the shared directory is
/// kept, read as `it holds more than 65536 entries` or `it holds more than
/// 256 user folders`. Example: another user who makes 300 empty folders named
/// `100000` to `100299` stops the listing at the 257th user folder it finds.
///
/// Each user's sockets sit in a folder named after that user's id in
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
#[cfg(unix)]
#[must_use]
pub fn list_foreign_sessions(
    shared_sessions_base_directory: &Path,
    runtime_directory: &Path,
) -> ForeignSessionListing {
    list_foreign_sessions_with_scan(
        shared_sessions_base_directory,
        runtime_directory,
        SharedDirectoryScan::default(),
    )
}

/// [`list_foreign_sessions`], counting on from the entries and user folders
/// `shared_directory_scan` already holds. Example: a scan that holds 65,535
/// entries reads one more entry, and stops at the entry after it.
#[cfg(unix)]
fn list_foreign_sessions_with_scan(
    shared_sessions_base_directory: &Path,
    runtime_directory: &Path,
    mut shared_directory_scan: SharedDirectoryScan,
) -> ForeignSessionListing {
    use std::os::unix::fs::MetadataExt;

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
    let own_user_directory_name = match find_own_user_directory_name(runtime_directory) {
        Ok(own_user_directory_name) => own_user_directory_name,
        Err(unread_path) => {
            foreign_session_listing.record_unread_path(unread_path);
            return foreign_session_listing;
        }
    };
    let mut advertisements_by_session_id: HashMap<SessionId, Vec<ForeignSessionAdvertisement>> =
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
                    foreign_session_listing.record_failed_read(&user_directory_path, &read_error);
                    continue 'user_directories;
                }
            };
            let Some(session_id) = find_named_foreign_session(&session_entry, &own_session_ids)
            else {
                continue;
            };
            if !shared_directory_scan.count_checked_session(owner_user_id) {
                foreign_session_listing.unlisted_session_count += 1;
                continue;
            }
            let session_metadata = match session_entry.metadata() {
                Ok(session_metadata) => session_metadata,
                Err(read_error) => {
                    foreign_session_listing.record_failed_read(&session_entry.path(), &read_error);
                    continue;
                }
            };
            if is_socket_owned_by(&session_metadata, owner_user_id) {
                advertisements_by_session_id
                    .entry(session_id)
                    .or_default()
                    .push(ForeignSessionAdvertisement {
                        owner_user_id,
                        socket_address: compute_socket_address(&user_directory_path, session_id),
                    });
            }
        }
    }
    for (session_id, advertisements) in advertisements_by_session_id {
        match advertisements.as_slice() {
            [advertisement] => foreign_session_listing
                .foreign_sessions
                .push((session_id, advertisement.clone())),
            _ => foreign_session_listing.duplicated_sessions.push(
                DuplicatedForeignSession::from_advertisements(session_id, &advertisements),
            ),
        }
    }
    foreign_session_listing
}

/// The session `directory_entry` names when its name is `session-<uuid>.sock`
/// and that session is not in `own_session_ids`. `None` for every other
/// entry. Reads the name only: no system call.
///
/// Example: `session-<uuid>.sock` gives that session, and `notes` and
/// `session-<uuid>` give `None`.
#[cfg(unix)]
fn find_named_foreign_session(
    directory_entry: &std::fs::DirEntry,
    own_session_ids: &HashSet<SessionId>,
) -> Option<SessionId> {
    let session_id = parse_session_id(directory_entry.file_name().to_str()?, ".sock")?;
    (!own_session_ids.contains(&session_id)).then_some(session_id)
}

/// Whether `socket_metadata`, read without following a link, is a socket that
/// `owner_user_id` owns.
#[cfg(unix)]
fn is_socket_owned_by(socket_metadata: &std::fs::Metadata, owner_user_id: u32) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    socket_metadata.file_type().is_socket() && socket_metadata.uid() == owner_user_id
}

/// Where `shared_sessions_base_directory` advertises the session `session_id`
/// another local user started, or `Ok(None)` when it advertises none.
///
/// No user's folder is listed, and no cap on sessions applies: the one path
/// `session-<uuid>.sock` is looked up in each folder named after another
/// user's id in decimal, such as `1001`. An entry of the shared directory that
/// names no user's folder costs no system call beyond the directory read. A
/// socket counts when it is a socket and the owner of the folder holding it
/// owns it. The lookup goes through at most
/// [`MAX_SHARED_DIRECTORY_ENTRY_COUNT`] entries of the shared directory and at
/// most [`MAX_SHARED_USER_DIRECTORY_COUNT`] user folders.
///
/// With `owner_user_id` of `None`, an id whose endpoint file or resume file
/// sits in `runtime_directory` is this user's, and gives `Ok(None)`: another
/// user's socket under that id is never taken for it. With an `owner_user_id`,
/// only a folder that user owns is looked in, and no file in
/// `runtime_directory` is looked up. Example: `Some(1001)` for an id that the
/// folders of users 1001 and 1002 both advertise gives the socket in the folder
/// of user 1001. A shared directory, a folder or a socket path that is gone, is
/// not a directory, or that its owner lets no other user read advertises
/// nothing. Example: the socket of `session_id` in another user's folder at
/// mode `0700` cannot be looked up, and gives `Ok(None)`.
///
/// # Errors
/// [`ForeignSessionLookupError::AdvertisedMoreThanOnce`] when sockets in two
/// folders that are looked in advertise the id.
/// [`ForeignSessionLookupError::PathUnreadable`] when the endpoint file or the
/// resume file of `session_id` in `runtime_directory` cannot be looked up, when
/// the owner of `runtime_directory` cannot be read, when a read of the shared
/// directory fails another way, such as with `EMFILE`, and when the shared
/// directory holds one entry past [`MAX_SHARED_DIRECTORY_ENTRY_COUNT`] or one
/// user folder past [`MAX_SHARED_USER_DIRECTORY_COUNT`]. Those two name the
/// shared directory, read as `it holds more than 65536 entries` and
/// `it holds more than 256 user folders`.
#[cfg(unix)]
pub fn find_foreign_session_advertisement(
    shared_sessions_base_directory: &Path,
    runtime_directory: &Path,
    session_id: SessionId,
    owner_user_id: Option<u32>,
) -> Result<Option<ForeignSessionAdvertisement>, ForeignSessionLookupError> {
    find_foreign_session_advertisement_with_scan(
        shared_sessions_base_directory,
        runtime_directory,
        session_id,
        owner_user_id,
        SharedDirectoryScan::default(),
    )
}

/// [`find_foreign_session_advertisement`], counting on from the entries and
/// user folders `shared_directory_scan` already holds.
#[cfg(unix)]
fn find_foreign_session_advertisement_with_scan(
    shared_sessions_base_directory: &Path,
    runtime_directory: &Path,
    session_id: SessionId,
    owner_user_id: Option<u32>,
    shared_directory_scan: SharedDirectoryScan,
) -> Result<Option<ForeignSessionAdvertisement>, ForeignSessionLookupError> {
    use std::os::unix::fs::MetadataExt;

    if owner_user_id.is_none() {
        for own_session_file_path in [
            EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id),
            resolve_resume_file_path(runtime_directory, session_id),
        ] {
            if is_file_present(&own_session_file_path)? {
                return Ok(None);
            }
        }
    }
    let own_user_directory_name = find_own_user_directory_name(runtime_directory)?;
    let directory_entries = match std::fs::read_dir(shared_sessions_base_directory) {
        Ok(directory_entries) => directory_entries,
        Err(read_error) => {
            validate_unadvertised_path_error(shared_sessions_base_directory, &read_error)?;
            return Ok(None);
        }
    };
    let mut shared_directory_scan = shared_directory_scan;
    let mut advertisements: Vec<ForeignSessionAdvertisement> = Vec::new();
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
        if owner_user_id.is_some_and(|owner_user_id| owner_user_id != user_directory_metadata.uid())
        {
            continue;
        }
        let socket_address = compute_socket_address(&user_directory_path, session_id);
        let socket_metadata = match std::fs::symlink_metadata(&socket_address) {
            Ok(socket_metadata) => socket_metadata,
            Err(read_error) => {
                validate_unadvertised_path_error(Path::new(&socket_address), &read_error)?;
                continue;
            }
        };
        if is_socket_owned_by(&socket_metadata, user_directory_metadata.uid()) {
            advertisements.push(ForeignSessionAdvertisement {
                owner_user_id: user_directory_metadata.uid(),
                socket_address,
            });
        }
    }
    match advertisements.as_slice() {
        [] => Ok(None),
        [advertisement] => Ok(Some(advertisement.clone())),
        _ => Err(ForeignSessionLookupError::AdvertisedMoreThanOnce {
            duplicated_session: DuplicatedForeignSession::from_advertisements(
                session_id,
                &advertisements,
            ),
        }),
    }
}

/// `Ok(())` when `read_error`, from reading `looked_up_path` in the shared
/// directory, says that path advertises nothing, as
/// [`is_unadvertised_path_error`] decides.
///
/// # Errors
/// [`ForeignSessionLookupError::PathUnreadable`] naming `looked_up_path` for
/// every other failure.
#[cfg(unix)]
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
#[cfg(unix)]
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

/// Whether `owner_user_id` is this user's id: the id that owns
/// `runtime_directory`, as `find_own_user_directory_name` reads it. `false`
/// when `runtime_directory` does not exist, and always on Windows, where no
/// user id names a folder.
///
/// # Errors
/// [`ForeignSessionLookupError::PathUnreadable`] naming `runtime_directory`
/// when it exists and its owner cannot be read.
fn is_own_user_id(
    runtime_directory: &Path,
    owner_user_id: u32,
) -> Result<bool, ForeignSessionLookupError> {
    #[cfg(unix)]
    {
        Ok(find_own_user_directory_name(runtime_directory)? == Some(owner_user_id.to_string()))
    }
    #[cfg(windows)]
    {
        let _ = (runtime_directory, owner_user_id);
        Ok(false)
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

/// What [`open_session_exchange`] gives for `ipc_request`, with the connection
/// closed once the result is read.
fn exchange_session_request(
    endpoint: &EndpointFile,
    session_id: SessionId,
    ipc_request: IpcRequest,
    answer_deadline: Option<Instant>,
) -> Result<IpcResult, CliError> {
    let (_, _, _, ipc_result) =
        open_session_exchange(endpoint, session_id, ipc_request, answer_deadline)?;
    Ok(ipc_result)
}

/// What [`exchange_prepared_session_request`] gives once the session answered
/// the Hello and the `Prepare`.
struct PreparedRequestExchange {
    /// The session's answer to the Hello.
    hello_answer: SessionHelloAnswer,
    /// What [`commit_after_prepare_answer`] gives for the `Prepare`'s answer:
    /// the answer to the request the `Prepare` handed over, or the failure
    /// that ended the exchange after the `Prepare` was answered.
    request_result: Result<IpcResult, CliError>,
}

/// What the session at `endpoint` answers to the Hello and to
/// `prepared_request_kind`, a request that changes the session, handed over
/// with `Prepare` and run with `Commit` on one connection.
///
/// The Hello and the `Prepare`, numbered 2, go out back to back through
/// [`open_session_exchange`], which `answer_deadline` bounds. Then
/// [`commit_after_prepare_answer`] takes the `Prepare`'s answer. With
/// `answer_deadline`, the `Commit` and its answer have
/// [`SERVER_ANSWER_TIMEOUT_DURATION`] from the moment the `Commit` is
/// written. With `None`, nothing ends them early.
///
/// Example: a session whose dispatcher is busy for 10 seconds, asked with a
/// deadline 5 seconds away, gives [`CliError::SessionAnswerTimedOut`], and the
/// request never runs.
///
/// # Errors
/// What [`open_session_exchange`] gives, and the request never runs. What
/// [`commit_after_prepare_answer`] gives after that is
/// [`PreparedRequestExchange::request_result`].
fn exchange_prepared_session_request(
    endpoint: &EndpointFile,
    session_id: SessionId,
    prepared_request_kind: IpcRequestKind,
    answer_deadline: Option<Instant>,
) -> Result<PreparedRequestExchange, CliError> {
    let prepare_request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Prepare(Box::new(prepared_request_kind)),
    };
    let (mut response_reader, mut request_writer, hello_answer, prepare_result) =
        open_session_exchange(endpoint, session_id, prepare_request, answer_deadline)?;
    let commit_timeout = answer_deadline.map(|_| SERVER_ANSWER_TIMEOUT_DURATION);
    let request_result = commit_after_prepare_answer(
        prepare_result,
        &mut response_reader,
        &mut request_writer,
        commit_timeout,
    );
    Ok(PreparedRequestExchange {
        hello_answer,
        request_result,
    })
}

/// The answer of the request that a `Prepare` handed the session, given
/// `prepare_result`, the session's answer to that `Prepare`, on the connection
/// that `response_reader` and `request_writer` carry.
///
/// - `Prepared`: [`commit_prepared_request`] writes the `Commit`, numbered 3,
///   with `commit_timeout`, and its answer is handed back.
/// - A refusal: handed back as it is. The session holds nothing.
///
/// # Errors
/// [`CliError::IpcUnavailable`] naming the reply for an answer that is neither
/// `Prepared` nor a refusal. No `Commit` is written, and the request never
/// runs. Then what [`commit_prepared_request`] gives.
pub(crate) fn commit_after_prepare_answer(
    prepare_result: IpcResult,
    response_reader: &mut FrameReader,
    request_writer: &mut FrameWriter,
    commit_timeout: Option<Duration>,
) -> Result<IpcResult, CliError> {
    match prepare_result {
        IpcResult::Prepared => {
            commit_prepared_request(response_reader, request_writer, 3, commit_timeout)
        }
        refusal @ IpcResult::Error(_) => Ok(refusal),
        unexpected_result => {
            Err(talk::SESSION_PEER_WORDS.build_unexpected_reply_error(&unexpected_result))
        }
    }
}

/// Write a `Commit`, numbered `commit_request_id`, for the request the session
/// holds on the connection that `response_reader` and `request_writer` carry,
/// and hand back the session's answer to it: the held request's own answer.
///
/// Both halves take a deadline `commit_timeout` after the `Commit` starts to be
/// written, for the write and the read. `None` waits however long the answer
/// takes.
///
/// # Errors
/// [`CliError::CommittedRequestUnanswered`] naming the failure when the write
/// or the read fails or runs past that deadline, and naming the reply when the
/// answer is a result this build has no name for: the session may run the
/// request, or may have run it.
pub fn commit_prepared_request(
    response_reader: &mut FrameReader,
    request_writer: &mut FrameWriter,
    commit_request_id: u64,
    commit_timeout: Option<Duration>,
) -> Result<IpcResult, CliError> {
    let commit_deadline = commit_timeout.map(|commit_timeout| Instant::now() + commit_timeout);
    response_reader.set_deadline(commit_deadline);
    request_writer.set_deadline(commit_deadline);
    let commit_request = IpcRequest {
        request_id: commit_request_id,
        request_kind: IpcRequestKind::Commit,
    };
    request_writer
        .send(&commit_request)
        .map_err(talk::build_committed_request_unanswered_error)?;
    let commit_response: IncomingResponse = response_reader
        .recv_answer()
        .map_err(talk::build_committed_request_unanswered_error)?;
    match commit_response.answer_result {
        MaybeKnown::Known(commit_result) => Ok(commit_result),
        MaybeKnown::Unknown { variant_name } => Err(CliError::CommittedRequestUnanswered {
            detail: talk::SESSION_PEER_WORDS.format_unexpected_reply_detail(&variant_name),
        }),
    }
}

/// Connect to `endpoint`, open with the Hello, and run `ipc_request` on the same
/// connection. Returns the reading half and the writing half of the
/// connection, which stay open, the session's answer to the Hello, and
/// `ipc_request`'s result; a failed Hello is an error.
///
/// The Hello and `ipc_request` go out back to back before either reply is read,
/// and the server answers every request in order: the exchange costs one
/// round trip.
///
/// With `answer_deadline`, the connect and every write and read after it end
/// by that moment, as [`connect_to_session`] states, and a failure once it is
/// reached is [`CliError::SessionAnswerTimedOut`]. Both halves keep that
/// deadline. With `None`, the connect waits at most [`CONNECT_WAIT_DURATION`],
/// and nothing ends the writes and reads early.
fn open_session_exchange(
    endpoint: &EndpointFile,
    session_id: SessionId,
    ipc_request: IpcRequest,
    answer_deadline: Option<Instant>,
) -> Result<(FrameReader, FrameWriter, SessionHelloAnswer, IpcResult), CliError> {
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
    let hello_response: IncomingResponse = response_reader
        .recv_answer()
        .map_err(build_exchange_error)?;
    let hello_answer = talk::parse_session_hello_answer(hello_response)?;

    let ipc_response: IncomingResponse = response_reader
        .recv_answer()
        .map_err(build_exchange_error)?;
    let ipc_result = talk::SESSION_PEER_WORDS.take_response_result(ipc_response)?;
    Ok((response_reader, request_writer, hello_answer, ipc_result))
}

/// The request that carries `copy_part` of a stand-in clipboard tool's copy
/// under `request_id`: an [`IpcRequestKind::Prepare`] carrying an
/// [`IpcRequestKind::ClipboardCopy`] for `pane_id` and `clipboard_targets`
/// when `is_last_part`, and an [`IpcRequestKind::ClipboardCopyPart`]
/// otherwise.
fn build_clipboard_copy_request(
    request_id: u64,
    pane_id: PaneId,
    clipboard_targets: &str,
    copy_part: &[u8],
    is_last_part: bool,
) -> IpcRequest {
    let copied_bytes = copy_part.to_vec();
    let request_kind = if is_last_part {
        IpcRequestKind::Prepare(Box::new(IpcRequestKind::ClipboardCopy {
            pane_id,
            clipboard_targets: clipboard_targets.to_string(),
            copied_bytes,
        }))
    } else {
        IpcRequestKind::ClipboardCopyPart { copied_bytes }
    };
    IpcRequest {
        request_id,
        request_kind,
    }
}

/// The answer to the clipboard copy part sent under `copy_request_id`:
/// `copy_result` as it is, or, when the part is the last one (`is_last_part`)
/// and `copy_result` is [`IpcResult::Prepared`], the answer to the `Commit`
/// numbered `copy_request_id + 1` that [`commit_prepared_request`] writes,
/// which has `answer_timeout` from that write.
///
/// # Errors
/// What [`commit_prepared_request`] gives, and
/// [`CliError::CommittedRequestUnanswered`] naming the reply for an answer to
/// the `Commit` that is neither `ClipboardCopyRouted` nor a refusal: the
/// session may route the copy.
fn commit_prepared_clipboard_copy(
    response_reader: &mut FrameReader,
    request_writer: &mut FrameWriter,
    copy_request_id: u64,
    is_last_part: bool,
    copy_result: IpcResult,
    answer_timeout: Duration,
) -> Result<IpcResult, CliError> {
    if !is_last_part || copy_result != IpcResult::Prepared {
        return Ok(copy_result);
    }
    let committed_result = commit_prepared_request(
        response_reader,
        request_writer,
        copy_request_id + 1,
        Some(answer_timeout),
    )?;
    match committed_result {
        IpcResult::ClipboardCopyRouted { .. } | IpcResult::Error(_) => Ok(committed_result),
        unexpected_result => Err(CliError::CommittedRequestUnanswered {
            detail: talk::SESSION_PEER_WORDS
                .format_unexpected_reply_detail(unexpected_result.get_wire_name()),
        }),
    }
}

/// The [`ClipboardCopyAnswer`] in `copy_result`, the session's answer to one
/// part of a clipboard copy: [`IpcResult::ClipboardCopyPartTaken`] to a part
/// that is not the last gives `PartTaken`, and
/// [`IpcResult::ClipboardCopyRouted`] gives `Routed`.
///
/// # Errors
/// - What [`build_peer_refusal_error`] gives for a refused part, such as a
///   session that refuses the copy with `RequestFailed` or does not know the
///   request with `UnsupportedKind`.
/// - [`CliError::IpcUnavailable`] naming the reply for every other answer,
///   such as `ClipboardCopyPartTaken` to the last part.
fn parse_clipboard_copy_answer(
    copy_result: IpcResult,
    is_last_part: bool,
) -> Result<ClipboardCopyAnswer, CliError> {
    match copy_result {
        IpcResult::ClipboardCopyPartTaken if !is_last_part => Ok(ClipboardCopyAnswer::PartTaken),
        IpcResult::ClipboardCopyRouted {
            should_run_clipboard_tool,
        } => Ok(ClipboardCopyAnswer::Routed {
            should_run_clipboard_tool,
        }),
        IpcResult::Error(refusal) => Err(build_peer_refusal_error(&refusal)),
        unexpected_result => {
            Err(talk::SESSION_PEER_WORDS.build_unexpected_reply_error(&unexpected_result))
        }
    }
}

/// The error a failed connect, write or read of a session exchange ends in:
/// [`CliError::SessionAnswerTimedOut`] once [`is_answer_deadline_reached`]
/// accepts `answer_deadline`, and the failure through
/// [`build_ipc_unavailable_error`] otherwise. [`IpcError::PeerIsOtherUser`]
/// goes through `build_ipc_unavailable_error` at any time.
fn build_session_exchange_error(ipc_error: IpcError, answer_deadline: Option<Instant>) -> CliError {
    let is_other_user_refusal = matches!(ipc_error, IpcError::PeerIsOtherUser { .. });
    if is_answer_deadline_reached(answer_deadline) && !is_other_user_refusal {
        return CliError::SessionAnswerTimedOut;
    }
    build_ipc_unavailable_error(ipc_error)
}

/// Whether `answer_deadline` is reached, as [`compute_remaining_duration_until`]
/// decides: less than 1 ms is left. `None` is never reached.
pub(crate) fn is_answer_deadline_reached(answer_deadline: Option<Instant>) -> bool {
    answer_deadline
        .is_some_and(|answer_deadline| compute_remaining_duration_until(answer_deadline).is_err())
}

/// How to reach `session_id`: the endpoint file in `runtime_directory`, or — for a
/// session another local user started — what `shared_sessions_base_directory`
/// advertises for it, through `find_foreign_session_advertisement`.
///
/// A session another local user started writes its endpoint file into that
/// user's own runtime directory, which this user may not read and does not
/// need: the shared directory names the socket, and the token presented is
/// empty. `shared_sessions_base_directory` of `None` searches
/// `runtime_directory` alone, and on Windows `runtime_directory` is always
/// searched alone. An id no searched place holds means no running koshi
/// advertises that session: [`CliError::SessionNotFound`].
///
/// A session of this user's whose endpoint file is gone while
/// [`is_replacing_its_image`] accepts it is restarting, and the shared
/// directory is not searched for it.
///
/// `owner_user_id` of `None` searches as above. Another user's id looks in the
/// folders of `shared_sessions_base_directory` that user owns, and reads no
/// endpoint file of this user's: the endpoint carries the empty token.
///
/// # Errors
/// [`CliError::SessionNotFound`] for an id no searched place holds, and for the
/// endpoint file of a koshi 0.1.0 window that [`is_koshi_0_1_0_window_closed`]
/// finds closed. With an `owner_user_id`, the name it carries is
/// `<user id>/session-<uuid>`. [`CliError::InvalidArgs`] for an `owner_user_id`
/// that owns `runtime_directory`; for the user id `1001`, its message reads
/// ``1001/<session id> names a session of yours; name it by its id alone: `<session id>` ``.
/// The failure [`build_session_restarting_error`] gives for a session that is
/// restarting; [`CliError::IpcUnavailable`] carrying the sentence of a failure
/// to read the owner of `runtime_directory`, or of a failure that
/// `find_foreign_session_advertisement` gives; and [`CliError::IpcUnavailable`]
/// for an endpoint file that exists and cannot be read, a koshi 0.1.0 window's
/// that is still open included.
pub fn load_session_endpoint(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
    owner_user_id: Option<u32>,
) -> Result<EndpointFile, CliError> {
    if let Some(owner_user_id) = owner_user_id {
        if is_own_user_id(runtime_directory, owner_user_id)? {
            return Err(CliError::InvalidArgs {
                detail: format!(
                    "{} names a session of yours; name it by its id alone: `{session_id}`",
                    format_session_target(session_id, Some(owner_user_id))
                ),
            });
        }
        return load_foreign_session_endpoint(
            runtime_directory,
            shared_sessions_base_directory,
            session_id,
            Some(owner_user_id),
        );
    }
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
    match EndpointFile::load_from_path(&endpoint_path) {
        Ok(endpoint) => Ok(endpoint),
        Err(IpcError::EndpointFileMissing { .. }) => {
            if is_replacing_its_image(runtime_directory, session_id) {
                return Err(build_session_restarting_error(session_id));
            }
            load_foreign_session_endpoint(
                runtime_directory,
                shared_sessions_base_directory,
                session_id,
                None,
            )
        }
        Err(IpcError::Koshi010WindowEndpointFile { .. })
            if is_koshi_0_1_0_window_closed(runtime_directory, session_id) =>
        {
            Err(CliError::SessionNotFound {
                session_name: session_id.to_string(),
            })
        }
        Err(ipc_error) => Err(CliError::IpcUnavailable {
            detail: ipc_error.to_string(),
        }),
    }
}

/// How to reach the session `session_id` of another local user: the socket
/// `find_foreign_session_advertisement` finds in `shared_sessions_base_directory`
/// for `owner_user_id`, with the empty token. `shared_sessions_base_directory`
/// of `None`, and every lookup on Windows, finds nothing.
///
/// # Errors
/// [`CliError::SessionNotFound`] naming what [`format_session_target`] gives
/// for `session_id` and `owner_user_id` when nothing is found, and
/// [`CliError::IpcUnavailable`] carrying the sentence of the lookup's failure.
fn load_foreign_session_endpoint(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
    owner_user_id: Option<u32>,
) -> Result<EndpointFile, CliError> {
    #[cfg(unix)]
    let foreign_socket_address = match shared_sessions_base_directory {
        Some(shared_sessions_base_directory) => find_foreign_session_advertisement(
            shared_sessions_base_directory,
            runtime_directory,
            session_id,
            owner_user_id,
        )?
        .map(|foreign_session_advertisement| foreign_session_advertisement.socket_address),
        None => None,
    };
    #[cfg(windows)]
    let foreign_socket_address: Option<String> = {
        let _ = (runtime_directory, shared_sessions_base_directory);
        None
    };
    foreign_socket_address
        .map(build_foreign_session_endpoint)
        .ok_or_else(|| CliError::SessionNotFound {
            session_name: format_session_target(session_id, owner_user_id),
        })
}

/// Whether nothing listens at
/// [`compute_socket_address`]`(runtime_directory, session_id)`, where a koshi
/// 0.1.0 window of the session `session_id` listens while it is open. Its
/// endpoint file names no process to check. Any connect failure other than
/// [`IpcError::NoListener`] gives `false`. A connect that succeeds is closed
/// at once.
///
/// Example: a window that crashed left `session-<uuid>.json` behind, and
/// nothing listens at its socket: `true`.
#[must_use]
pub fn is_koshi_0_1_0_window_closed(runtime_directory: &Path, session_id: SessionId) -> bool {
    matches!(
        Connection::connect(&compute_socket_address(runtime_directory, session_id)),
        Err(IpcError::NoListener { .. })
    )
}

/// Connect to the advertised socket, waiting at most
/// [`CONNECT_WAIT_DURATION`] for the connect to complete.
///
/// An endpoint whose token is not empty is this user's own session: the
/// connect goes through [`Connection::connect_to_own_server_within`], and
/// keeps the connection only when this OS user serves the socket. An endpoint
/// with the empty token, which another local user's session asks for, is
/// reached with [`Connection::connect_within`].
///
/// With `answer_deadline`, the wait also ends by that moment, and no connect
/// starts once [`compute_remaining_duration_until`] finds it reached: that is
/// [`CliError::SessionAnswerTimedOut`]. Example: a deadline 300 ms away gives
/// the connect 300 ms, and one already passed connects to nothing.
///
/// # Errors
/// An address nothing listens on reports the session as not running
/// ([`CliError::SessionNotFound`]). Another OS user serving the socket of an
/// own session is [`CliError::IpcUnavailable`] carrying
/// [`IpcError::PeerIsOtherUser`]. Every other failure is
/// [`CliError::SessionAnswerTimedOut`] once `answer_deadline` is reached, and
/// what [`build_ipc_unavailable_error`] gives otherwise.
pub fn connect_to_session(
    endpoint: &EndpointFile,
    session_id: SessionId,
    answer_deadline: Option<Instant>,
) -> Result<Connection, CliError> {
    let connect_wait_duration = match answer_deadline {
        Some(answer_deadline) => compute_remaining_duration_until(answer_deadline)
            .map_err(|_| CliError::SessionAnswerTimedOut)?
            .min(CONNECT_WAIT_DURATION),
        None => CONNECT_WAIT_DURATION,
    };
    let connect_outcome = if endpoint.connection_token.is_empty() {
        Connection::connect_within(&endpoint.socket_address, connect_wait_duration)
    } else {
        Connection::connect_to_own_server_within(&endpoint.socket_address, connect_wait_duration)
    };
    connect_outcome.map_err(|ipc_error| match ipc_error {
        IpcError::NoListener { .. } => CliError::SessionNotFound {
            session_name: session_id.to_string(),
        },
        ipc_error => build_session_exchange_error(ipc_error, answer_deadline),
    })
}

#[cfg(test)]
mod tests;
