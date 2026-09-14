//! The per-session server process: it owns one session's panes and PTYs and
//! answers that session's control socket.
//!
//! It runs with no terminal of its own. Startup reads `koshi.kdl`, installs
//! this session's log subscriber, builds the server, seeds the session under
//! the id and name it was started with, binds the control socket, and prints
//! one JSON line saying where that socket is — the only thing this process
//! ever writes to standard output. Then it serves the runtime inbox — applying
//! each event, timing renders, and handing every attached client its frame —
//! until the last pane's child exits, or a `core:quit` command is applied and
//! no client is still expected back from an image swap, and tears down.
//!
//! Where that control socket is bound depends on who may reach it: this user's
//! private runtime directory on its own, or the machine-wide shared directory
//! when `koshi.kdl`'s `allow-other-users` is on or `--allow-other-users` forces
//! it on for this session.
//!
//! ## Replacing its own image
//!
//! A restart request accepted over the control socket ends the serve loop into
//! the swap. The swap holds every pane's reader still, tells every attached
//! client to come back, writes the session's whole state to the resume file
//! beside the endpoint file, withdraws the control socket, and replaces this
//! process's image with the binary on disk. On Unix that is `execvp`, which
//! keeps the process id and every pane's terminal descriptor; on Windows it
//! starts the new image and ends, and the panes live in a helper process that
//! outlives both. The new image is started with `--resume`, takes every pane
//! back, rebuilds the session from the carried state, binds a fresh socket, and
//! deletes the file — and deletes it just the same when it cannot come up at
//! all. A new image that never starts leaves the file behind, and the router
//! removes it once it is older than
//! [`RESTART_WINDOW_DURATION`](koshi_ipc::endpoint::RESTART_WINDOW_DURATION).
//!
//! Nothing irreversible happens before the panes are held still, so a swap that
//! cannot start leaves the session serving in this process with every pane and
//! every reader running.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use koshi_config::layer::PartialKoshiConfig;
use koshi_core::geometry::Size;
use koshi_core::ids::{PaneId, SessionId};
use koshi_core::process::{KillPolicy, PtySize};
use koshi_ipc::endpoint::{resolve_resume_file_path, RESTART_WINDOW_DURATION};
use koshi_ipc::error::IpcError;
use koshi_ipc::router::{SessionServerReady, ROUTER_PROTOCOL_VERSION};
use koshi_observability::logging::init_tracing;
use koshi_pty::backend::state::{CarriedPtyPane, PtyBackend, PtyHandle, PtySink};
use koshi_runtime::ipc_server::IpcServer;
use koshi_runtime::resume::{self, ResumeBody, ResumeHeader, RESUME_FORMAT, RESUME_FORMAT_MIN};
use koshi_runtime::runtime::event::RuntimeEvent;
use koshi_runtime::runtime::pty_forward::InboxSink;
use koshi_runtime::server::{can_carry_panes, is_binary_runnable, RestartCheck, Server};
use koshi_storage::error::StorageError;
use serde::{Deserialize, Serialize};

use koshi_link::router_client::RUNTIME_DIRECTORY_FLAG;

#[cfg(unix)]
use koshi_pty::kill::PtyChildKillControl;
#[cfg(unix)]
use koshi_pty::portable::{find_terminal_master_name, set_terminal_cloexec, PortablePtyBackend};

#[cfg(windows)]
use koshi_ipc::protocol::ConnectionToken;
#[cfg(windows)]
use koshi_ipc::supervisor::compute_supervisor_socket_address;
#[cfg(windows)]
use koshi_pty::supervisor::SupervisorPtyBackend;

#[cfg(test)]
mod tests;

/// The size the session's first pane starts at. No client is attached yet, so
/// there is no terminal to read a size from; the first attach resizes it.
const STARTING_VIEWPORT: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// The subcommand `koshi` runs one session server under. The arguments after
/// it are the session id, the session name, [`RUNTIME_DIRECTORY_FLAG`] with the
/// directory the session serves, `--profile` when the create named a profile,
/// and `--allow-other-users` when the create asked for the other users of this
/// machine. The router starts a new session under it, and a session server
/// replacing its own image starts the new one under it.
pub const SESSION_SERVER_SUBCOMMAND: &str = "serve-session";

/// The flag telling a session server to let the other users of this machine
/// reach the session, whatever its `koshi.kdl` says. A session server
/// replacing its own image passes it on, so the rebound socket keeps that
/// reach.
pub(crate) const ALLOW_OTHER_USERS_FLAG: &str = "--allow-other-users";

/// The subcommand a session server runs the newly installed binary under to
/// read which resume-file formats it can take back. It prints one JSON line and
/// exits.
pub const RESUME_SUPPORT_SUBCOMMAND: &str = "resume-support";

/// The flag carrying the carried state to the image replacing this one. The
/// file it names is read once and then removed.
const RESUME_FLAG: &str = "--resume";

/// The flag carrying the secret of the link to the process holding this
/// session's panes. Windows only: on Unix the panes are the session server's
/// own children and there is no link.
const SUPERVISOR_TOKEN_FLAG: &str = "--supervisor-token";

/// The flag carrying the process id of the process holding this session's
/// panes. That id is part of the link's address, so the image replacing this
/// one needs it to reach the same panes. Passed on beside
/// [`SUPERVISOR_TOKEN_FLAG`], and Windows only for the same reason.
const SUPERVISOR_PID_FLAG: &str = "--supervisor-pid";

/// How long a client whose record came across an image swap has to attach
/// again. A record nobody claims when the window closes is detached.
const RECONNECT_GRACE_DURATION: Duration = Duration::from_secs(30);

/// How long the newly installed binary has to say which resume formats it
/// reads. One that has not answered by then is refused, so a binary that never
/// exits cannot hold the thread serving the session.
const RESUME_SUPPORT_WAIT_DURATION: Duration = Duration::from_secs(5);

/// How long a session server waits for the process holding its panes to start
/// listening.
#[cfg(windows)]
const SUPERVISOR_LINK_WAIT_DURATION: Duration = Duration::from_secs(10);

/// How long the wait for the process holding the panes pauses between attempts.
#[cfg(windows)]
const SUPERVISOR_LINK_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(50);

/// How long a swap waits for every told client to send its `Leaving` request.
/// The connections still open when it passes are closed.
const CLIENTS_LEFT_WAIT_DURATION: Duration = Duration::from_secs(1);

/// How long the wait for the told clients pauses between passes over the
/// runtime inbox.
const CLIENTS_LEFT_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(2);

/// The backend this session server drives its panes through.
///
/// On Unix every pane is this process's own child on this process's own
/// backend: `execvp` keeps the process id, so the children keep their parent
/// and their terminals across a swap. On Windows a pane's pseudoconsole cannot
/// leave the process that opened it, so the panes live in a helper process and
/// this backend is the link to it.
///
/// The swap reaches `pause_readers`, `resume_readers`, `flush_writers` and
/// `carried_panes` through this concrete type, so the session server keeps it
/// beside the `Arc<dyn PtyBackend>` the server holds.
#[cfg(unix)]
type PtyOwner = PortablePtyBackend;

/// The backend this session server drives its panes through. See the Unix
/// definition.
#[cfg(windows)]
type PtyOwner = SupervisorPtyBackend;

/// The one line `koshi resume-support` prints: which resume-file formats that
/// build takes back.
///
/// A session server asks the newly installed binary this before it does
/// anything it cannot undo: the install already replaced the old binary on
/// disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeSupport {
    /// The oldest resume-file format this build reads.
    #[serde(rename = "min")]
    pub minimum_resume_format: u32,
    /// The newest resume-file format this build reads.
    #[serde(rename = "max")]
    pub maximum_resume_format: u32,
}

impl ResumeSupport {
    /// What this build reads.
    #[must_use]
    pub fn from_current_build() -> ResumeSupport {
        ResumeSupport {
            minimum_resume_format: RESUME_FORMAT_MIN,
            maximum_resume_format: RESUME_FORMAT,
        }
    }
}

/// What this session server was started with: the identity the router gave it,
/// where it serves, and how the image that replaces it is started.
///
/// Every argument here is passed on to that image, so the resumed session comes
/// up under the same id and name, in the same directory, under the same
/// `--allow-other-users` flag. The profile is absent: it opened this session's
/// tabs and panes once, and the carried state is what brings them back.
struct SessionStart {
    /// The directory this session serves in.
    runtime_directory: PathBuf,
    /// The session's id, which the router picked.
    session_id: SessionId,
    /// The session's display name, which the router generated.
    session_name: String,
    /// Whether `--allow-other-users` was on this process's command line. It
    /// forces the socket's reach on whatever `koshi.kdl` says, so it is passed
    /// on and the rebound socket stays reachable by the same users. With the
    /// flag off, the rebound socket takes the reach `koshi.kdl` holds at that
    /// moment.
    is_other_user_access_allowed: bool,
    /// The path this program was started from. A swap runs the binary there.
    executable_path: PathBuf,
    /// The secret the link to the process holding the panes presents at Hello.
    /// `None` on Unix, where the panes are this process's own children.
    supervisor_token: Option<String>,
    /// The process id of that same process, which its link address is derived
    /// from. `None` on Unix, and set together with `supervisor_token` on
    /// Windows.
    supervisor_process_id: Option<u32>,
}

/// Why the serve loop ended.
#[derive(Debug, PartialEq, Eq)]
enum ServeOutcome {
    /// The session is over, on the terms [`run_session_serve_loop`] states.
    Ended,
    /// A restart request was accepted, so this process replaces its own image.
    Restart,
}

/// Run one session to its end: seed it under `session_id` and `session_name`,
/// serve its control socket inside `runtime_directory`, report readiness on standard
/// output, then loop until the session ends.
///
/// The ready line is printed only once the session is seeded and the socket is
/// bound. Any failure before that returns `Err` having printed nothing, so a
/// caller reading standard output sees end of stream and knows the session
/// never started.
///
/// `profile_name` names the profile the session opens its tabs and panes from.
/// `None`, a name no profile file answers to, and a profile that will not
/// launch each open one shell instead.
///
/// `allow_other_users_override` is the `--allow-other-users` flag the router
/// passes on: `Some(true)` serves the other users of this machine whatever
/// `koshi.kdl` says, and `None` leaves that answer to the file.
///
/// `resume_file_path` is the `--resume` flag the image being replaced passes on: the
/// carried state this session comes up from instead of being seeded. A resume
/// run reads no profile, since the carried state already holds the tabs and
/// panes the profile opened. `supervisor_token` and `supervisor_process_id` are the
/// `--supervisor-token` and `--supervisor-pid` flags that go with it on
/// Windows, naming the secret the link to the process holding the panes
/// presents and the process id its address is derived from.
#[allow(clippy::too_many_arguments)]
pub fn run_session_server(
    runtime_directory: &Path,
    session_id: SessionId,
    session_name: String,
    profile_name: Option<&str>,
    allow_other_users_override: Option<bool>,
    resume_file_path: Option<&Path>,
    supervisor_token: Option<&str>,
    supervisor_process_id: Option<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    let app_config = koshi_link::config::load_app_layer();
    let logging_parameters =
        koshi_link::config::build_logging_params(app_config.as_ref(), session_id);
    let (log_level, log_format) = (logging_parameters.log_level, logging_parameters.log_format);
    let _ = init_tracing(logging_parameters);
    // The first line written, so a log file that exists at all already says
    // which level and format the session ran under.
    tracing::info!(
        session_id = %session_id,
        level = ?log_level,
        format = ?log_format,
        "logging started"
    );

    let mut session_start = SessionStart {
        runtime_directory: runtime_directory.to_path_buf(),
        session_id,
        session_name: session_name.clone(),
        is_other_user_access_allowed: allow_other_users_override == Some(true),
        executable_path: std::env::current_exe()?,
        supervisor_token: supervisor_token.map(str::to_string),
        supervisor_process_id,
    };

    // This session's server: panes deliver their child's output straight into
    // this inbox from their own PTY reader threads.
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel::<RuntimeEvent>();
    let pty_sink: Arc<dyn PtySink> =
        Arc::new(InboxSink::from_event_sender(runtime_event_sender.clone()));

    let (mut session_server, pty_owner, mut ipc_server) = match resume_file_path {
        Some(resume_file_path) => resume_from_file(
            resume_file_path,
            &mut session_start,
            app_config,
            Arc::clone(&pty_sink),
            runtime_event_receiver,
            &runtime_event_sender,
        )?,
        None => seed_initial_session(
            &mut session_start,
            profile_name,
            app_config,
            Arc::clone(&pty_sink),
            runtime_event_receiver,
            &runtime_event_sender,
        )?,
    };
    install_restart_check(
        &mut session_server,
        &pty_owner,
        &session_start.executable_path,
    );

    report_ready(&ipc_server, resume_file_path.is_some())?;

    loop {
        match run_session_serve_loop(&mut session_server) {
            ServeOutcome::Ended => break,
            ServeOutcome::Restart => match swap_session_image(
                session_server,
                ipc_server,
                &pty_owner,
                &session_start,
                &runtime_event_sender,
            )? {
                // The session runs in another process from here; this one ends
                // without touching a single pane.
                None => return Ok(()),
                Some((restored_session_server, restored_ipc_server)) => {
                    session_server = restored_session_server;
                    ipc_server = restored_ipc_server;
                    install_restart_check(
                        &mut session_server,
                        &pty_owner,
                        &session_start.executable_path,
                    );
                }
            },
        }
    }

    // Every attached client is told the session ended, and holds that frame,
    // before anything is torn down: nothing else joins the threads writing to
    // the clients.
    session_server.announce_quit();

    // The socket stops before the panes are killed, so nothing advertises a
    // session that is ending.
    ipc_server.shutdown();
    session_server.shutdown();
    Ok(())
}

/// Seed the one session this process serves and bind its control socket.
///
/// No client is minted here: this process serves whoever attaches over the
/// control socket, and until one does the session holds none. A profile that
/// will not launch falls back to one shell, so the session always comes up.
fn seed_initial_session(
    session_start: &mut SessionStart,
    profile_name: Option<&str>,
    app_config: Option<PartialKoshiConfig>,
    pty_sink: Arc<dyn PtySink>,
    runtime_event_receiver: Receiver<RuntimeEvent>,
    runtime_event_sender: &Sender<RuntimeEvent>,
) -> Result<(Server, Arc<PtyOwner>, IpcServer), Box<dyn std::error::Error>> {
    let (mut session_server, pty_owner) = build_server_over_new_panes(
        session_start,
        app_config,
        pty_sink,
        runtime_event_receiver,
        runtime_event_sender,
    )?;

    let session_start_time = SystemTime::now();
    let profile_template_document =
        profile_name.and_then(koshi_link::config::load_profile_template);
    let is_profile_seeded = match profile_template_document {
        // The name is the router's, not a fresh one: the router registered this
        // session under it and a `koshi attach <name>` resolves against it.
        Some(profile_template_document) => match session_server.bootstrap_profile_named(
            session_start.session_id,
            session_start.session_name.clone(),
            profile_template_document,
            STARTING_VIEWPORT,
            session_start_time,
            None,
        ) {
            Ok(()) => true,
            Err(profile_start_error) => {
                tracing::warn!(
                    %profile_start_error,
                    "profile could not launch; starting a single shell"
                );
                false
            }
        },
        None => false,
    };
    if !is_profile_seeded {
        session_server.bootstrap_session(
            session_start.session_id,
            session_start.session_name.clone(),
            STARTING_VIEWPORT,
            session_start_time,
            None,
        )?;
    }

    let ipc_server = bind_session_socket(session_start, runtime_event_sender)?;
    Ok((session_server, pty_owner, ipc_server))
}

/// Come up from the state the previous process image carried out.
///
/// A body that reads gives the session back what [`ResumeBody`] carries, over
/// the panes taken back from the header. A body that does not read ends every
/// pane the header names and seeds one fresh shell under the same id and name,
/// so the session id the router registered still answers.
///
/// The previous image let its panes go — on Unix by ending, on Windows by
/// dropping its link — so every pane is taken back from a descriptor and a
/// process id, or from the helper process holding it.
/// [`resume_readers_and_rebuild`] is the other path: it rebuilds the same state
/// over panes that were never released.
///
/// The file is deleted on every way out of this call. It outlives the socket
/// being bound. While it exists, the router leaves this session's
/// advertisement in place through the swap.
fn resume_from_file(
    resume_file_path: &Path,
    session_start: &mut SessionStart,
    app_config: Option<PartialKoshiConfig>,
    pty_sink: Arc<dyn PtySink>,
    runtime_event_receiver: Receiver<RuntimeEvent>,
    runtime_event_sender: &Sender<RuntimeEvent>,
) -> Result<(Server, Arc<PtyOwner>, IpcServer), Box<dyn std::error::Error>> {
    let (resume_header, encoded_resume_body) = match resume::read_resume_header(resume_file_path) {
        Ok(resume_header_and_body) => resume_header_and_body,
        Err(resume_read_error) => {
            // A header that does not read names no pane, so nothing can be
            // taken back and nothing can be ended. The file goes with it.
            let _ = std::fs::remove_file(resume_file_path);
            return Err(resume_read_error.into());
        }
    };

    let resume_body = resume::read_resume_body(resume_header.resume_format, &encoded_resume_body);
    let rebuilt_session = build_from_carried_state(
        &resume_header,
        resume_body,
        session_start,
        app_config,
        pty_sink,
        runtime_event_receiver,
        runtime_event_sender,
    );
    // The state is in memory and the socket carries a fresh token, or nothing
    // came up at all; either way the file has done its work.
    let _ = std::fs::remove_file(resume_file_path);
    rebuilt_session
}

/// Build the server, the panes and the bound socket the carried state names, as
/// [`resume_from_file`] hands them on.
///
/// `resume_body` is what reading the carried body gave. The panes come back only when
/// that read worked and every pane the header names is taken back; either
/// failure ends every pane the header names and seeds one fresh shell instead.
///
/// # Errors
/// Returns the failure of fresh panes that could not be opened, of a fresh
/// session that could not be seeded, and of a control socket that could not be
/// bound.
fn build_from_carried_state(
    resume_header: &ResumeHeader,
    resume_body: Result<ResumeBody, StorageError>,
    session_start: &mut SessionStart,
    app_config: Option<PartialKoshiConfig>,
    pty_sink: Arc<dyn PtySink>,
    runtime_event_receiver: Receiver<RuntimeEvent>,
    runtime_event_sender: &Sender<RuntimeEvent>,
) -> Result<(Server, Arc<PtyOwner>, IpcServer), Box<dyn std::error::Error>> {
    let carried_session_state = match resume_body {
        Ok(resume_body) => {
            match take_panes_back(resume_header, Arc::clone(&pty_sink), session_start) {
                Ok((pty_owner, pty_handle_by_pane_id)) => {
                    Some((resume_body, pty_owner, pty_handle_by_pane_id))
                }
                Err(take_back_error) => {
                    tracing::error!(
                        %take_back_error,
                        "the carried panes could not be taken back; the session comes back with one shell"
                    );
                    None
                }
            }
        }
        Err(resume_read_error) => {
            tracing::error!(
                %resume_read_error,
                wrote = resume_header.resume_format,
                reads_from = RESUME_FORMAT_MIN,
                reads_to = RESUME_FORMAT,
                "the carried state could not be read; the session comes back with one shell"
            );
            release_carried_panes(resume_header, session_start, Arc::clone(&pty_sink));
            None
        }
    };

    // Either way the session comes back on the `koshi.kdl` that is on disk now.
    let (session_server, pty_owner) = match carried_session_state {
        Some((resume_body, pty_owner, pty_handle_by_pane_id)) => {
            let pty_backend: Arc<dyn PtyBackend> = pty_owner.clone();
            let mut session_server = Server::resume(
                pty_backend,
                runtime_event_receiver,
                runtime_event_sender.clone(),
                resume_body,
                pty_handle_by_pane_id,
                build_carried_pty_sizes(resume_header),
            );
            session_server.load_startup_config(app_config);
            start_reconnect_deadline(runtime_event_sender.clone());
            (session_server, pty_owner)
        }
        None => {
            let (mut session_server, pty_owner) = build_server_over_new_panes(
                session_start,
                app_config,
                pty_sink,
                runtime_event_receiver,
                runtime_event_sender,
            )?;
            // The identity the router registered, and the one the socket below
            // binds under, so the session id still answers.
            session_server.bootstrap_session(
                session_start.session_id,
                session_start.session_name.clone(),
                STARTING_VIEWPORT,
                SystemTime::now(),
                None,
            )?;
            (session_server, pty_owner)
        }
    };

    let ipc_server = bind_session_socket(session_start, runtime_event_sender)?;
    Ok((session_server, pty_owner, ipc_server))
}

/// Open the panes this session runs on.
///
/// On Unix they are this process's own children on its own backend. On Windows
/// they belong to a helper process this starts and outlive an image swap; the
/// secret its link presents and its process id are recorded on
/// `session_start`, since the image replacing this one needs both to reach the same
/// panes.
///
/// The helper's address carries its process id, so the helper started here
/// never binds the address a helper this session is leaving behind still holds.
///
/// # Errors
/// Returns the failure of a helper process that could not be started or could
/// not be reached, which the caller reports as the session failing to start.
fn open_session_panes(
    session_start: &mut SessionStart,
    pty_sink: Arc<dyn PtySink>,
) -> Result<Arc<PtyOwner>, Box<dyn std::error::Error>> {
    #[cfg(unix)]
    {
        let _ = session_start;
        Ok(Arc::new(PortablePtyBackend::with_pty_sink(pty_sink)))
    }
    #[cfg(windows)]
    {
        let supervisor_token = ConnectionToken::generate();
        let supervisor_process_id = crate::pty_supervisor::spawn_pty_supervisor(
            &session_start.runtime_directory,
            session_start.session_id,
            &supervisor_token,
        )?;
        let linked_pty_owner = link_to_supervisor(
            session_start.session_id,
            supervisor_process_id,
            &session_start.runtime_directory,
            &supervisor_token,
            pty_sink,
            &[],
        )?;
        session_start.supervisor_token = Some(supervisor_token.expose().to_string());
        session_start.supervisor_process_id = Some(supervisor_process_id);
        Ok(linked_pty_owner)
    }
}

/// Open fresh panes and build the server driving them, on the `koshi.kdl` that
/// is on disk now.
///
/// The server holds no session yet: the caller seeds one. Two callers reach it
/// — a first run, and a resume whose carried state could not be read.
///
/// # Errors
/// Returns the failure of panes that could not be opened.
fn build_server_over_new_panes(
    session_start: &mut SessionStart,
    app_config: Option<PartialKoshiConfig>,
    pty_sink: Arc<dyn PtySink>,
    runtime_event_receiver: Receiver<RuntimeEvent>,
    runtime_event_sender: &Sender<RuntimeEvent>,
) -> Result<(Server, Arc<PtyOwner>), Box<dyn std::error::Error>> {
    let pty_owner = open_session_panes(session_start, pty_sink)?;
    let pty_backend: Arc<dyn PtyBackend> = pty_owner.clone();
    let mut session_server = Server::from_runtime_parts(
        pty_backend,
        runtime_event_receiver,
        runtime_event_sender.clone(),
    );
    session_server.load_startup_config(app_config);
    Ok((session_server, pty_owner))
}

/// The panes taken back after an image swap: the backend driving them, and one
/// handle per pane for the rebuilt server to hold.
type TakenBackPtyState = (Arc<PtyOwner>, HashMap<PaneId, PtyHandle>);

/// Take every pane the resume header names back, and hand back the backend driving
/// them with one handle each.
///
/// A pane's terminal descriptor crossed the swap open, so each pane is taken
/// back from that descriptor and its child's process id by
/// [`take_one_pane_back`].
///
/// When one pane cannot be taken back, every pane the header names is ended
/// before the failure is returned, so the caller holds nothing half-owned.
///
/// # Errors
/// Returns whatever [`take_one_pane_back`] reports, the sentence naming a pane
/// the carried state names twice, and the sentence naming a descriptor the
/// carried state gives to two panes.
#[cfg(unix)]
fn take_panes_back(
    resume_header: &ResumeHeader,
    pty_sink: Arc<dyn PtySink>,
    _session_start: &SessionStart,
) -> Result<TakenBackPtyState, Box<dyn std::error::Error>> {
    header_names_each_pane_once(resume_header)?;

    let pty_owner = Arc::new(PortablePtyBackend::with_pty_sink(pty_sink));
    let mut pty_handle_by_pane_id = HashMap::new();
    // Which pane each descriptor was taken back on, so a number the header
    // names twice is refused rather than owned by two panes and closed twice.
    let mut pane_id_by_terminal_file_descriptor: HashMap<i32, PaneId> = HashMap::new();
    for (pane_index, carried_pane) in resume_header.carried_panes.iter().enumerate() {
        if let Some(terminal_file_descriptor) = carried_pane.terminal_fd {
            if let Some(&earlier_pane_id) =
                pane_id_by_terminal_file_descriptor.get(&terminal_file_descriptor)
            {
                end_panes_after_failure(resume_header, pane_index + 1);
                return Err(format!(
                    "pane {} carried descriptor {terminal_file_descriptor}, which pane {earlier_pane_id} was already taken back \
                     on, so it cannot be taken back",
                    carried_pane.pane_id
                )
                .into());
            }
        }
        match take_one_pane_back(&pty_owner, carried_pane) {
            Ok(pty_handle) => {
                pty_handle_by_pane_id.insert(carried_pane.pane_id, pty_handle);
                if let Some(terminal_file_descriptor) = carried_pane.terminal_fd {
                    pane_id_by_terminal_file_descriptor
                        .insert(terminal_file_descriptor, carried_pane.pane_id);
                }
            }
            Err(take_back_error) => {
                end_panes_after_failure(resume_header, pane_index + 1);
                return Err(take_back_error);
            }
        }
    }
    Ok((pty_owner, pty_handle_by_pane_id))
}

/// Refuse a carried state that names one pane more than once, or gives two
/// panes one child process id.
///
/// Every pane is taken back onto its own entry, keyed by pane id, so each id
/// appears once. Every pane's child is waited on by that pane's own watcher, so
/// each process id appears once as well. A process id of `0` names no child and
/// may repeat.
///
/// Before → after: a header naming panes `A`, `B` → each is taken back. A
/// header naming `A`, `A` → the sentence naming `A`, and no pane is touched. A
/// header naming `A(pid 4821)`, `B(pid 4821)` → the sentence naming `B` and
/// `A`, and no pane is touched.
///
/// # Errors
/// Returns the sentence naming the pane the carried state names twice, or the
/// pane whose process id an earlier pane already carries.
fn header_names_each_pane_once(
    resume_header: &ResumeHeader,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut named_pane_ids = HashSet::new();
    let mut pane_id_by_process_id: HashMap<u32, PaneId> = HashMap::new();
    for carried_pane in &resume_header.carried_panes {
        if !named_pane_ids.insert(carried_pane.pane_id) {
            return Err(format!(
                "pane {} is named twice by the carried state, so it cannot be taken back",
                carried_pane.pane_id
            )
            .into());
        }
        if carried_pane.process_id == 0 {
            continue;
        }
        if let Some(earlier_pane_id) =
            pane_id_by_process_id.insert(carried_pane.process_id, carried_pane.pane_id)
        {
            return Err(format!(
                "pane {} carries process id {}, which pane {earlier_pane_id} already carries, so it \
                 cannot be taken back",
                carried_pane.pane_id, carried_pane.process_id
            )
            .into());
        }
    }
    Ok(())
}

/// Take one pane back from the terminal descriptor and process id the header
/// carried, and hand back the handle the rebuilt server holds it by.
///
/// What the number names is read before this process owns it, in two steps.
///
/// 1. A number that names no pseudoterminal master is refused, so a number
///    naming an ordinary file, a pipe, this process's own standard error, or
///    nothing at all never becomes a pane's terminal.
/// 2. A number that names a master is refused when the header recorded which
///    terminal that master is paired with and the descriptor is now paired with
///    another one. A header that recorded no name leaves step 1 to decide.
///
/// Close-on-exec goes back on the descriptor once both steps pass. The exit
/// status the header carried goes to the pane as well, so a child the previous
/// image reaped is reported with the code it really ended with.
///
/// Before → after: the header carries `terminal_fd = 7` and
/// `terminal_name = "/dev/ttys009"` for pane 3, and descriptor 7 is now the
/// master of `/dev/ttys011` → the sentence naming both terminals comes back and
/// descriptor 7 is left alone.
///
/// # Errors
/// Returns the sentence naming a pane the header carried no descriptor for, the
/// sentence naming a descriptor that is no pseudoterminal master, the sentence
/// naming a descriptor whose terminal is not the one the header recorded, the OS
/// error of a descriptor whose flags cannot be set, and the failure of a pane
/// the backend could not take back.
#[cfg(unix)]
fn take_one_pane_back(
    pty_backend: &Arc<PortablePtyBackend>,
    carried_pane: &resume::CarriedPane,
) -> Result<PtyHandle, Box<dyn std::error::Error>> {
    use std::os::fd::{FromRawFd, OwnedFd};

    let terminal_file_descriptor = carried_pane.terminal_fd.ok_or_else(|| {
        format!(
            "pane {} carried no terminal descriptor, so it cannot be taken back",
            carried_pane.pane_id
        )
    })?;
    let Some(current_terminal_name) = find_terminal_master_name(terminal_file_descriptor) else {
        return Err(format!(
            "pane {} carried descriptor {terminal_file_descriptor}, which names no pseudoterminal master, \
             so it cannot be taken back",
            carried_pane.pane_id
        )
        .into());
    };
    if let Some(carried_terminal_name) = &carried_pane.terminal_name {
        if *carried_terminal_name != current_terminal_name {
            return Err(format!(
                "pane {} carried descriptor {terminal_file_descriptor} as the master of {carried_terminal_name}, which is now \
                 the master of {current_terminal_name}, so it cannot be taken back",
                carried_pane.pane_id
            )
            .into());
        }
    }
    set_terminal_cloexec(terminal_file_descriptor, true)?;
    // The descriptor crossed the swap open and names this process's own
    // pseudoterminal master, so it is this process's own from here.
    let terminal_file = unsafe { OwnedFd::from_raw_fd(terminal_file_descriptor) };
    Ok(pty_backend.adopt(
        carried_pane.pane_id,
        terminal_file,
        carried_pane.process_id,
        carried_pane.get_pty_size(),
        carried_pane.exit_status,
    )?)
}

/// End every pane the resume header names after a failure, and close the terminals
/// from `first_untouched_pane_index` onward.
///
/// Each child's whole process group is ended, which reaps its grandchildren
/// too. `first_untouched_pane_index` is the first pane this process never took over. The panes
/// before it belong to the backend, which closes them as it is dropped, apart
/// from the one whose hand-over failed: its descriptor is left as it is —
/// either the call that failed closed it, or this process never took it over
/// and the number stays as the swap left it until this process ends. An
/// `first_untouched_pane_index` of `0` is a failure before any pane was taken back, so every
/// terminal the header names is closed here.
///
/// Each descriptor number is closed at most once, and a number a pane before
/// `first_untouched_pane_index` also carries is not closed here at all.
///
/// Before → after: a header naming `A(fd 7)`, `B(fd 9)`, `C(fd 7)` with
/// `first_untouched_pane_index` `2` → descriptor 7 stays open, since `A` holds it.
#[cfg(unix)]
fn end_panes_after_failure(resume_header: &ResumeHeader, first_untouched_pane_index: usize) {
    for carried_pane in &resume_header.carried_panes {
        let _ = end_carried_child(carried_pane.process_id);
    }
    let mut closed_terminal_file_descriptors: HashSet<i32> = resume_header.carried_panes
        [..first_untouched_pane_index]
        .iter()
        .filter_map(|carried_pane| carried_pane.terminal_fd)
        .collect();
    for carried_pane in &resume_header.carried_panes[first_untouched_pane_index..] {
        if carried_pane
            .terminal_fd
            .is_some_and(|terminal_file_descriptor| {
                !closed_terminal_file_descriptors.insert(terminal_file_descriptor)
            })
        {
            continue;
        }
        close_carried_terminal(carried_pane);
    }
}

/// Close one carried pane's terminal descriptor.
///
/// What the number names is read first: only a pseudoterminal master is closed,
/// so a number that names an ordinary file, a pipe, a socket or this process's
/// own standard error is left open and logged.
///
/// Before → after: the header carries `terminal_fd = 2` and descriptor 2 is
/// this process's own standard error → descriptor 2 stays open.
#[cfg(unix)]
fn close_carried_terminal(carried_pane: &resume::CarriedPane) {
    use std::os::fd::{FromRawFd, OwnedFd};

    let Some(terminal_file_descriptor) = carried_pane.terminal_fd else {
        return;
    };
    if find_terminal_master_name(terminal_file_descriptor).is_none() {
        tracing::warn!(
            pane = %carried_pane.pane_id,
            terminal_fd = terminal_file_descriptor,
            "the carried state named a descriptor that is no pseudoterminal master; it stays open"
        );
        return;
    }
    drop(unsafe { OwnedFd::from_raw_fd(terminal_file_descriptor) });
}

/// Take every pane the header names back by linking to the helper process
/// holding them, and hand back the backend driving them with one handle each.
///
/// The panes never moved: the helper process opened every pseudoconsole and
/// still owns it. Linking names which panes this session claims, so the helper
/// ends any it holds that this session does not.
///
/// A link that cannot be made ends the panes over a link of its own. A helper
/// process that answers neither keeps its panes until its own idle window ends
/// it; a link secret or process id that was not passed on leaves no way to
/// reach it at all.
///
/// # Errors
/// Returns the sentence naming the missing link secret or process id, and the
/// failure of a helper process that could not be reached.
#[cfg(windows)]
fn take_panes_back(
    resume_header: &ResumeHeader,
    pty_sink: Arc<dyn PtySink>,
    session_start: &SessionStart,
) -> Result<TakenBackPtyState, Box<dyn std::error::Error>> {
    header_names_each_pane_once(resume_header)?;

    let supervisor_token = session_start.supervisor_token.as_deref().ok_or(
        "the secret of the link to the process holding the panes was not passed on, \
         so those panes cannot be reached",
    )?;
    let supervisor_process_id = session_start.supervisor_process_id.ok_or(
        "the process id of the process holding the panes was not passed on, \
         so those panes cannot be reached",
    )?;
    let claimed_pane_ids: Vec<PaneId> = resume_header
        .carried_panes
        .iter()
        .map(|carried_pane| carried_pane.pane_id)
        .collect();
    let pty_owner = match link_to_supervisor(
        session_start.session_id,
        supervisor_process_id,
        &session_start.runtime_directory,
        &ConnectionToken::from_secret(supervisor_token),
        Arc::clone(&pty_sink),
        &claimed_pane_ids,
    ) {
        Ok(pty_owner) => pty_owner,
        Err(link_error) => {
            // The link is the only way to reach the panes, so ending them is
            // tried once more over a link of its own.
            release_carried_panes(resume_header, session_start, pty_sink);
            return Err(link_error.into());
        }
    };
    let pty_handle_by_pane_id = claimed_pane_ids
        .iter()
        .map(|pane_id| (*pane_id, PtyHandle::from_detached_pane_id(*pane_id)))
        .collect();
    Ok((pty_owner, pty_handle_by_pane_id))
}

/// End every pane the header names, so a carried state that cannot be read
/// leaves no child running and no terminal open.
///
/// No pane was taken back here, so every terminal the header names is closed as
/// well — which is [`end_panes_after_failure`] with no pane left untouched.
#[cfg(unix)]
fn release_carried_panes(
    resume_header: &ResumeHeader,
    _session_start: &SessionStart,
    _pty_sink: Arc<dyn PtySink>,
) {
    end_panes_after_failure(resume_header, 0);
}

/// Record on `resume_header` the exit status each pane reports now.
///
/// The panes are read once to build the header and again just before it is
/// written. A child that ends between the two is reaped by this image's
/// watcher, and nothing else can answer for it afterwards. A status the header
/// already carries is kept: this only fills in the ones that settled late.
///
/// Before → after: pane 3's shell exits with code 7 after the header was built
/// → the header carries `exit: Some(ExitCode(7))` instead of `None`, and the
/// next image reports 7 rather than `-1`.
fn refresh_carried_exits(resume_header: &mut ResumeHeader, carried_pty_panes: &[CarriedPtyPane]) {
    for carried_pty_pane in carried_pty_panes {
        let Some(carried_pane_record) = resume_header
            .carried_panes
            .iter_mut()
            .find(|carried_pane| carried_pane.pane_id == carried_pty_pane.pane_id)
        else {
            continue;
        };
        if carried_pane_record.exit_status.is_none() {
            carried_pane_record.exit_status = carried_pty_pane.exit_status;
        }
    }
}

/// End the process group of one pane child a carried header names.
///
/// The process group is ended by `killpg`, whose argument is signed: `0` names
/// this process's own group, and a process id that does not fit a positive
/// `i32` wraps to a negative number naming another group. Both are refused.
///
/// Before → after: `pid = 4821` → that pane child's group is ended and `true`
/// comes back. `pid = 0` or `pid = 3_000_000_000` → nothing is signalled and
/// `false` comes back.
///
/// Hands back whether `killpg` was called on `pid`. A `killpg` that failed —
/// the group is already gone, or a member may not be signalled — still hands
/// back `true`.
#[cfg(unix)]
fn end_carried_child(process_id: u32) -> bool {
    if process_id == 0 || i32::try_from(process_id).is_err() {
        tracing::warn!(process_id, "the carried state named no pane child to end");
        return false;
    }
    let _ = PtyChildKillControl::from_process_id(process_id).force_kill_process_tree();
    true
}

/// End every pane the helper process holds, so a carried state that cannot be
/// read leaves no child running and no terminal open.
///
/// Linking while claiming no pane is what ends them: the helper process ends
/// every pane the session server does not claim. It is then told to end itself,
/// so the fresh session that follows starts a helper process of its own.
///
/// This path and [`take_panes_back`] both build the helper's address from the
/// identity the router started this process for and the helper's process id
/// passed on beside it, so the two always name the same helper. The header
/// names the panes, which the helper itself already knows, so nothing here
/// reads it.
#[cfg(windows)]
fn release_carried_panes(
    _resume_header: &ResumeHeader,
    session_start: &SessionStart,
    pty_sink: Arc<dyn PtySink>,
) {
    let Some(supervisor_token) = session_start.supervisor_token.as_deref() else {
        return;
    };
    let Some(supervisor_process_id) = session_start.supervisor_process_id else {
        return;
    };
    let Ok(pty_owner) = link_to_supervisor(
        session_start.session_id,
        supervisor_process_id,
        &session_start.runtime_directory,
        &ConnectionToken::from_secret(supervisor_token),
        pty_sink,
        &[],
    ) else {
        return;
    };
    let _ = pty_owner.shutdown_supervisor();
}

/// Open the link to the helper process holding `session_id`'s panes, claiming
/// `claimed` and no other pane.
///
/// `supervisor_process_id` is that helper's process id, which its address is derived
/// from.
///
/// A helper process that has just been started is not listening yet, so a link
/// that cannot be opened is tried again every [`SUPERVISOR_LINK_POLL_INTERVAL_DURATION`] until
/// [`SUPERVISOR_LINK_WAIT_DURATION`] runs out. That window bounds when the last attempt
/// starts, not how long one attempt lasts: an attempt that reaches the helper
/// waits its own bounded time for each answer, so the call can return one
/// answer wait past the window.
///
/// # Errors
/// Returns the last failure of a helper process that never answered.
#[cfg(windows)]
fn link_to_supervisor(
    session_id: SessionId,
    supervisor_process_id: u32,
    runtime_directory: &Path,
    supervisor_token: &ConnectionToken,
    pty_sink: Arc<dyn PtySink>,
    claimed_pane_ids: &[PaneId],
) -> Result<Arc<PtyOwner>, koshi_pty::error::PtyError> {
    let supervisor_socket_address =
        compute_supervisor_socket_address(runtime_directory, session_id, supervisor_process_id);
    let supervisor_link_deadline = Instant::now() + SUPERVISOR_LINK_WAIT_DURATION;
    loop {
        let linked_pty_owner = SupervisorPtyBackend::connect(
            &supervisor_socket_address,
            supervisor_token.clone(),
            Arc::clone(&pty_sink),
            claimed_pane_ids,
        );
        match linked_pty_owner {
            Ok(connected_pty_backend) => return Ok(Arc::new(connected_pty_backend)),
            Err(link_error) if Instant::now() >= supervisor_link_deadline => {
                return Err(link_error)
            }
            Err(_) => std::thread::sleep(SUPERVISOR_LINK_POLL_INTERVAL_DURATION),
        }
    }
}

/// The size each pane the header names holds, keyed by pane.
fn build_carried_pty_sizes(resume_header: &ResumeHeader) -> HashMap<PaneId, PtySize> {
    resume_header
        .carried_panes
        .iter()
        .map(|carried_pane| (carried_pane.pane_id, carried_pane.get_pty_size()))
        .collect()
}

/// Bind this session's control socket and write the endpoint file advertising
/// it, under the reach `--allow-other-users` and `koshi.kdl` give it.
///
/// Every binding goes through here — the first one, and the one a rebuilt
/// session takes — so the token a client waits for changes once per bind.
///
/// # Errors
/// Returns the failure of an address that could not be bound or an endpoint
/// file that could not be written.
fn bind_session_socket(
    session_start: &SessionStart,
    runtime_event_sender: &Sender<RuntimeEvent>,
) -> Result<IpcServer, IpcError> {
    let other_users_policy = koshi_link::config::resolve_other_users_policy(
        koshi_link::config::load_app_layer().as_ref(),
        session_start.is_other_user_access_allowed.then_some(true),
    );
    IpcServer::start(
        &session_start.runtime_directory,
        session_start.session_id,
        runtime_event_sender.clone(),
        other_users_policy,
    )
}

/// Print the one JSON line saying where this session's control socket is.
///
/// `is_resumed` marks a run that came up from carried state. The router read this
/// line when it first started the session and has closed its end of the pipe,
/// so a failed write on a resume run is logged and passed over. On a first run
/// it is a failed start.
///
/// # Errors
/// Returns the failure of a first run whose ready line could not be written.
fn report_ready(
    ipc_server: &IpcServer,
    is_resumed: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let ready_message = SessionServerReady {
        protocol_version: ROUTER_PROTOCOL_VERSION,
        socket_address: ipc_server.get_socket_address().to_string(),
    };
    let ready_line = serde_json::to_string(&ready_message)?;
    let mut session_standard_output = std::io::stdout();
    match writeln!(session_standard_output, "{ready_line}")
        .and_then(|()| session_standard_output.flush())
    {
        Ok(()) => Ok(()),
        Err(ready_line_write_error) if is_resumed => {
            tracing::debug!(%ready_line_write_error, "nothing was reading the ready line after the swap");
            Ok(())
        }
        Err(ready_line_write_error) => Err(ready_line_write_error.into()),
    }
}

/// Install what a restart request must promise before this session accepts it:
/// the binary a swap would run can be run, that binary reads the resume file
/// this build writes, every pane can cross the swap, and no pane is still being
/// written to.
///
/// Installed again on every server this process serves with, so a session that
/// came back from a swap that failed still answers the next restart.
fn install_restart_check(
    session_server: &mut Server,
    pty_owner: &Arc<PtyOwner>,
    executable_path: &Path,
) {
    let executable_path = executable_path.to_path_buf();
    let pty_owner = Arc::clone(pty_owner);
    // The three checks this process makes on its own run first; the new binary
    // is run only once all three pass.
    let restart_check: RestartCheck = Arc::new(move || {
        is_binary_runnable(&executable_path)?;
        can_carry_panes(&pty_owner.list_carried_panes())?;
        // A child that stopped reading its stdin blocks its pane's writer, and
        // the bytes behind that write cannot cross the swap.
        pty_owner
            .flush_writers()
            .map_err(|flush_writers_error| flush_writers_error.to_string())?;
        reads_the_format_this_build_writes(read_resume_support(&executable_path)?, &executable_path)
    });
    session_server.set_restart_check(restart_check);
}

/// Which resume-file formats the binary at `executable_path` takes back, as
/// `<executable_path> resume-support` prints them.
///
/// Running the binary also proves it runs at all on this machine, so a download
/// that arrived broken or built for another architecture is caught before the
/// swap.
///
/// The wait is bounded and the binary is ended either way. This runs on the
/// thread serving the session, which every pane's output also passes through.
///
/// # Errors
/// Returns the sentence naming the binary and what is wrong with it.
fn read_resume_support(executable_path: &Path) -> Result<ResumeSupport, String> {
    let mut child_process = std::process::Command::new(executable_path)
        .arg(RESUME_SUPPORT_SUBCOMMAND)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|process_spawn_error| {
            format!(
                "the binary at {} could not be run: {process_spawn_error}",
                executable_path.display()
            )
        })?;
    let child_standard_output = child_process
        .stdout
        .take()
        .expect("the binary was spawned with its standard output piped");
    let resume_support_line_result =
        read_session_server_line(child_standard_output).recv_timeout(RESUME_SUPPORT_WAIT_DURATION);
    // Ending it closes the pipe, which ends the thread reading it, so a binary
    // that never answered leaves behind neither a process nor a thread.
    let _ = child_process.kill();
    let _ = child_process.wait();

    match resume_support_line_result {
        Ok(resume_support_line) => {
            parse_resume_support(resume_support_line.trim()).map_err(|resume_support_parse_error| {
                format!(
                    "the binary at {} {resume_support_parse_error}",
                    executable_path.display()
                )
            })
        }
        Err(_resume_support_read_error) => Err(format!(
            "the binary at {} did not say which resume formats it reads within {} seconds",
            executable_path.display(),
            RESUME_SUPPORT_WAIT_DURATION.as_secs()
        )),
    }
}

/// Read the first line `stdout` carries on a thread of its own, and hand back
/// the channel it arrives on. A stream that ends before a newline sends
/// whatever it held.
fn read_session_server_line(child_standard_output: std::process::ChildStdout) -> Receiver<String> {
    let (resume_support_line_sender, resume_support_line_receiver) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("koshi-resume-support".to_string())
        .spawn(move || {
            let mut resume_support_line = String::new();
            let _ = BufReader::new(child_standard_output).read_line(&mut resume_support_line);
            let _ = resume_support_line_sender.send(resume_support_line);
        });
    resume_support_line_receiver
}

/// The resume-file formats one line of `koshi resume-support` names.
///
/// # Errors
/// Returns the sentence naming what the line held instead.
fn parse_resume_support(resume_support_line: &str) -> Result<ResumeSupport, String> {
    serde_json::from_str(resume_support_line).map_err(|resume_support_parse_error| {
        format!("does not say which resume formats it reads: {resume_support_parse_error}")
    })
}

/// Whether a build reading the formats `resume_support` names can read the resume file
/// this build writes.
///
/// # Errors
/// Returns the sentence naming both ranges.
fn reads_the_format_this_build_writes(
    resume_support: ResumeSupport,
    executable_path: &Path,
) -> Result<(), String> {
    if (resume_support.minimum_resume_format..=resume_support.maximum_resume_format)
        .contains(&RESUME_FORMAT)
    {
        return Ok(());
    }
    Err(format!(
        "the binary at {} reads resume formats {} to {}, and this one reads {RESUME_FORMAT_MIN} \
         to {RESUME_FORMAT} and writes {RESUME_FORMAT}",
        executable_path.display(),
        resume_support.minimum_resume_format,
        resume_support.maximum_resume_format
    ))
}

/// Whether `session_id` is replacing its own process image right now: its resume
/// file exists and is younger than [`RESTART_WINDOW_DURATION`].
///
/// The router asks this before it drops a session that stopped answering, and
/// again before it removes a resume file no session claims. A resume file older
/// than the window means the swap died, so the session is dropped as usual and
/// the file goes with it. A file stamped ahead of this machine's clock reads as
/// fresh.
#[must_use]
pub(crate) fn is_replacing_its_image(runtime_directory: &Path, session_id: SessionId) -> bool {
    let Ok(resume_file_modified_at) =
        std::fs::metadata(resolve_resume_file_path(runtime_directory, session_id))
            .and_then(|resume_file_metadata| resume_file_metadata.modified())
    else {
        return false;
    };
    resume_file_modified_at
        .elapsed()
        .map_or(true, |resume_file_age| {
            resume_file_age < RESTART_WINDOW_DURATION
        })
}

/// Start the one-shot timer that closes the wait for the clients whose records
/// came across an image swap.
///
/// Each of those clients that attaches again before the window closes keeps its
/// focus, zoom, scroll offset and selection. Whoever is left when the timer
/// fires is detached.
fn start_reconnect_deadline(runtime_event_sender: Sender<RuntimeEvent>) {
    let _ = std::thread::Builder::new()
        .name("koshi-session-reconnect".to_string())
        .spawn(move || {
            std::thread::sleep(RECONNECT_GRACE_DURATION);
            let _ = runtime_event_sender.send(RuntimeEvent::DropUnclaimedClients {
                unclaimed_client_deadline: Instant::now(),
            });
        });
}

/// Put the session back to serving in this process after the swap was
/// abandoned: every pane's reader goes back to its terminal, and the accepted
/// restart is taken back so the serve loop the caller returns to stops asking
/// for the swap.
///
/// Called only from the abandon paths that run before any client was told and
/// before any state moved, so the server this hands back is the one the session
/// carries on with.
fn restore_session_serving(mut session_server: Server, pty_owner: &Arc<PtyOwner>) -> Server {
    pty_owner.resume_readers();
    session_server.cancel_restart();
    session_server
}

/// Replace this process's image with the binary it was started from, carrying
/// the whole session and every pane across.
///
/// The order is what makes the swap lossless:
///
/// 1. Apply the inbox, hold every pane's reader still, apply the inbox again,
///    then wait for every pane's writer to finish. Once the readers are parked,
///    no byte has been read from a terminal without reaching an engine. Each
///    inbox pass hands every client what it produced, so the escape a copy
///    queued for a client's own terminal goes out here. Both passes detach a
///    client that hung up, since no client has been told anything yet.
/// 2. Tell every attached client, and wait until each one holds that frame:
///    nothing else joins the threads writing to the clients. From here every
///    path ends with a socket carrying a fresh token, which is what a client
///    that was told watches for.
/// 3. Wait for every told client to leave, applying the inbox on each pass. A
///    client that read the frame step 2 wrote sends `Leaving` and writes nothing
///    after it, so its connection ends once the session has read every key,
///    paste, mouse round and command it sent. All of them are applied here.
///    A client that stopped reading its socket never leaves, so the wait ends
///    after [`CLIENTS_LEFT_WAIT_DURATION`]. The intake then closes, ending the
///    connections that are left, and a last pass applies what they had already
///    handed over. Nothing arrives after that pass.
/// 4. Carry the state out and wait for every pane's writer again, so no byte
///    the session took for a child — a typed key, a paste, a reply to a device
///    query — is still queued in a thread the swap destroys. Then write the
///    carried state, withdraw the control socket so the new image can bind it,
///    and replace the image.
///
/// A session that keeps serving on either check in step 4 — a pane's writer
/// that will not settle, or carried state that cannot be written — keeps the
/// control socket it is already serving on and rotates its connection token.
/// The address is bound again only after the socket has been withdrawn for a
/// new image that then failed to start.
///
/// A `core:quit` applied by an inbox pass before step 2 abandons the swap and
/// ends the session in this process, on the terms [`run_session_serve_loop`] states.
///
/// `Ok(None)` means the session now runs in another process and this one ends
/// without touching a single pane. `Ok(Some(..))` hands back the server and the
/// control socket the session keeps serving on, which is what a swap that could
/// not start leaves behind; a swap abandoned on a quit comes back the same way,
/// with the quit standing on the server for the serve loop to end on.
///
/// # Errors
/// Returns the failure of a session that can neither swap nor be put back. Every
/// pane is ended first, so nothing is left running with no owner.
fn swap_session_image(
    mut session_server: Server,
    ipc_server: IpcServer,
    pty_owner: &Arc<PtyOwner>,
    session_start: &SessionStart,
    runtime_event_sender: &Sender<RuntimeEvent>,
) -> Result<Option<(Server, IpcServer)>, Box<dyn std::error::Error>> {
    apply_queued_runtime_events(&mut session_server, DetachPolicy::Apply);

    // Nothing has been told and nothing has moved, so a pane whose reader
    // cannot be held still leaves the session exactly as it was, with every
    // client still streaming. The two checks below stand on the same ground.
    if let Err(pause_readers_error) = pty_owner.pause_readers() {
        tracing::warn!(
            %pause_readers_error,
            "the panes could not be held still; the session keeps serving"
        );
        return Ok(Some((
            restore_session_serving(session_server, pty_owner),
            ipc_server,
        )));
    }
    apply_queued_runtime_events(&mut session_server, DetachPolicy::Apply);

    // A `core:quit` applied by either pass above ends the session in this
    // process: the serve loop the caller returns to reads the quit and ends on
    // the terms that loop states.
    if session_server.is_quit_requested() {
        tracing::info!("a quit arrived while the swap was starting; the session is ending");
        return Ok(Some((
            restore_session_serving(session_server, pty_owner),
            ipc_server,
        )));
    }

    // The pass above queues the replies to the device queries carried in the
    // chunks the parked readers delivered, so the writers are waited on after
    // it.
    if let Err(flush_writers_error) = pty_owner.flush_writers() {
        tracing::warn!(
            %flush_writers_error,
            "a pane is still being written to; the session keeps serving"
        );
        return Ok(Some((
            restore_session_serving(session_server, pty_owner),
            ipc_server,
        )));
    }

    session_server.announce_restarting();

    // Every told client sends `Leaving` and writes nothing after it, so its
    // connection ends once the session has read every key, paste, mouse round
    // and command it sent while the frame above was on its way. Each pass
    // applies what those connections handed over. A client that stopped reading
    // its socket never leaves, so the wait ends after CLIENTS_LEFT_WAIT_DURATION.
    let client_leave_deadline = Instant::now() + CLIENTS_LEFT_WAIT_DURATION;
    loop {
        drain_runtime_event_inbox(&mut session_server, DetachPolicy::Skip);
        let attached_client_count = ipc_server.attached_connections();
        if attached_client_count == 0 {
            break;
        }
        if Instant::now() >= client_leave_deadline {
            tracing::warn!(
                clients = attached_client_count,
                "a client did not leave within the wait; what it sends now is not read"
            );
            break;
        }
        std::thread::sleep(CLIENTS_LEFT_POLL_INTERVAL_DURATION);
    }

    // Nothing a client sends reaches the session from here. The pass below is
    // the last one, and it applies what a cut connection had already handed
    // over.
    ipc_server.close_intake();
    apply_queued_runtime_events(&mut session_server, DetachPolicy::Skip);

    // A `core:quit` applied by the pass above rides the swap out in the carried
    // state, with its kind, rather than ending the session here. The clients
    // have already been told to wait for the next socket, so the swap is what
    // brings them back; the next image serves until each carried client has
    // attached again or its window has closed, and ends then. A quit naming one
    // client only detaches it and carries nothing.
    if session_server.is_quit_requested() {
        tracing::info!("a quit arrived while the swap was starting; the next image carries it out");
    }

    let carried_pty_panes = pty_owner.list_carried_panes();
    let Some((mut resume_header, resume_body)) = session_server.carry_out(&carried_pty_panes)
    else {
        tracing::error!("this process holds no session to carry; the session keeps serving");
        return Ok(Some((
            restore_session_serving(session_server, pty_owner),
            ipc_server,
        )));
    };

    let resume_file_path =
        resolve_resume_file_path(&session_start.runtime_directory, session_start.session_id);

    // The pass above handed the panes' writers whatever it applied, so the
    // writers are waited on again. Every client has been told by now, so a pane
    // that cannot settle puts the session back on a socket carrying a fresh
    // token.
    if let Err(flush_writers_error) = pty_owner.flush_writers() {
        tracing::warn!(
            %flush_writers_error,
            "a pane is still being written to; the session keeps serving"
        );
        // The session keeps the socket it is serving on, and no resume file is
        // written: nothing binds this address again and no sweep finds it
        // withdrawn.
        return resume_readers_and_keep_socket(
            session_server,
            ipc_server,
            pty_owner,
            &resume_header,
            resume_body,
            session_start,
            runtime_event_sender,
        )
        .map(Some);
    }

    // The panes were read to build the header a few steps back, so a child that
    // ended in between was reaped by this image's watcher and its status is
    // known only here.
    refresh_carried_exits(&mut resume_header, &pty_owner.list_carried_panes());

    // Written before the socket is released. A session that cannot write it
    // keeps the socket it is serving on.
    if let Err(write_resume_file_error) =
        resume::write_resume_file(&resume_file_path, &resume_header, &resume_body)
    {
        tracing::error!(
            %write_resume_file_error,
            "the carried state could not be written; the session keeps serving"
        );
        return resume_readers_and_keep_socket(
            session_server,
            ipc_server,
            pty_owner,
            &resume_header,
            resume_body,
            session_start,
            runtime_event_sender,
        )
        .map(Some);
    }

    // The socket name and the endpoint file are released before the new image
    // binds them, and before the rebuild below binds them again.
    ipc_server.shutdown();

    if start_replacement_image(session_start, &resume_header, &resume_file_path) {
        return Ok(None);
    }

    match resume_readers_and_rebuild(
        session_server,
        pty_owner,
        &resume_header,
        resume_body,
        session_start,
        runtime_event_sender,
    ) {
        Ok(rebuilt_session) => Ok(Some(rebuilt_session)),
        Err(rebuild_error) => {
            // Nothing can serve these panes any more, so they are ended rather
            // than left running with no reader. The rebuild has already taken
            // the file away.
            for carried_pty_pane in pty_owner.list_carried_panes() {
                let _ = pty_owner.kill_pane(carried_pty_pane.pane_id, KillPolicy::Tree);
            }
            Err(rebuild_error)
        }
    }
}

/// Start the image replacing this one, from the state written at `resume_file_path`.
///
/// `true` means the session runs in another process from here and this one
/// ends. On Unix that answer never comes back: `execvp` replaces this process
/// in place, so a return at all means the swap did not start and every pane's
/// terminal has its close-on-exec flag back. `false` is that failure, logged
/// with the reason.
#[cfg(unix)]
fn start_replacement_image(
    session_start: &SessionStart,
    resume_header: &ResumeHeader,
    resume_file_path: &Path,
) -> bool {
    match keep_terminals_across_exec(resume_header) {
        Err(terminal_carry_error) => {
            tracing::error!(%terminal_carry_error, "a pane's terminal could not be carried; the session keeps serving");
        }
        // The call returns only when the exec failed, having put the SIGPIPE
        // ignore back.
        Ok(()) => {
            let restart_error = restart_session_by_exec(session_start, resume_file_path);
            tracing::error!(
                %restart_error,
                "the new image could not be started; the session keeps serving"
            );
        }
    }
    // No image was replaced, so every terminal is this process's own again and
    // takes the flag it was carried without back.
    put_close_on_exec_back(resume_header);
    false
}

/// Start the image replacing this one, from the state written at `resume_file_path`.
///
/// `true` means the new image was started and the session runs in it from here.
/// `false` is a start that failed, logged with the reason; the panes stay in the
/// helper process either way, so nothing about them changes.
#[cfg(windows)]
fn start_replacement_image(
    session_start: &SessionStart,
    _resume_header: &ResumeHeader,
    resume_file_path: &Path,
) -> bool {
    match hand_over_session_to_new_image(session_start, resume_file_path) {
        Ok(()) => true,
        Err(image_start_error) => {
            tracing::error!(%image_start_error, "the new image could not be started; the session keeps serving");
            false
        }
    }
}

/// What [`apply_queued_runtime_events`] does with a `ClientDetached` it drains.
#[derive(Clone, Copy)]
enum DetachPolicy {
    /// Apply it, so a client that hung up leaves the session's records.
    Apply,
    /// Pass it over, so the client keeps its record.
    Skip,
}

/// Apply everything already waiting in the runtime inbox, then hand every
/// client what applying it produced.
///
/// `detach_policy` says what a queued `ClientDetached` does. Every pass before the
/// restart is announced takes [`Detaches::Apply`]: no client has been told
/// anything yet, so a detach there is a client that really hung up, and the
/// session keeps serving without it whether the swap starts or is abandoned.
/// The passes after the announce take [`Detaches::Skip`]: every told client's
/// connection ends as that client leaves, and the swap carries each record
/// across so the client attaches again onto it. The grace window after the swap
/// drops a record nobody claims. The last of those passes runs after
/// [`IpcServer::close_intake`], so what a client sent is already in the inbox
/// when it starts and nothing arrives after it.
///
/// The push is what delivers the bytes a command queued for a client's own
/// terminal — the escape a copy writes to the clipboard — since the serve loop
/// that pushes has already returned.
fn apply_queued_runtime_events(session_server: &mut Server, detach_policy: DetachPolicy) {
    drain_runtime_event_inbox(session_server, detach_policy);
    session_server.push_frames();
}

/// Apply every event the runtime inbox holds, on the terms [`apply_queued_runtime_events`]
/// states, and push no frames. A push builds each subscriber's whole frame, so
/// a caller passing over the inbox repeatedly pushes once at the end.
fn drain_runtime_event_inbox(session_server: &mut Server, detach_policy: DetachPolicy) {
    while let Ok(runtime_event) = session_server.inbox_rx().try_recv() {
        if matches!(
            (detach_policy, &runtime_event),
            (DetachPolicy::Skip, RuntimeEvent::ClientDetached { .. })
        ) {
            continue;
        }
        let _ = session_server.handle_runtime_event(runtime_event);
    }
}

/// Put the session back on its feet in this process after a swap that did not
/// start, from the state it had already carried out.
///
/// The panes were never released: the backend still holds every one and every
/// watcher is still on its child, so the readers pick up where they stopped and
/// the rebuilt server takes [`PtyHandle::from_detached_pane_id`] handles over the panes that
/// same backend drives.
///
/// The control socket is bound again here, and its fresh token is what every
/// client that was told the session is restarting watches for. The caller has
/// already withdrawn the socket the session was serving on.
///
/// The resume file is deleted on every way out of this call, so a session that
/// comes back here and one that cannot come back anywhere both leave nothing on
/// the disk.
///
/// # Errors
/// Returns the failure of a control socket that could not be bound.
fn resume_readers_and_rebuild(
    session_server: Server,
    pty_owner: &Arc<PtyOwner>,
    resume_header: &ResumeHeader,
    resume_body: ResumeBody,
    session_start: &SessionStart,
    runtime_event_sender: &Sender<RuntimeEvent>,
) -> Result<(Server, IpcServer), Box<dyn std::error::Error>> {
    let mut rebuilt_session = resume_session_readers(
        session_server,
        pty_owner,
        resume_header,
        resume_body,
        runtime_event_sender,
    );

    let bound_session_socket = bind_session_socket(session_start, runtime_event_sender);
    let _ = std::fs::remove_file(resolve_resume_file_path(
        &session_start.runtime_directory,
        session_start.session_id,
    ));
    let session_socket = bound_session_socket?;
    finish_session_resume(&mut rebuilt_session, runtime_event_sender);
    Ok((rebuilt_session, session_socket))
}

/// Put the session back on its feet in this process, on `session_socket`, from the
/// state it had already carried out.
///
/// `session_socket` keeps its address and rotates its connection token. The panes were
/// never released, and the resume file is deleted.
///
/// # Errors
/// Returns the failure of advertising the fresh token. The panes are resumed
/// and the resume file is deleted either way.
fn resume_readers_and_keep_socket(
    session_server: Server,
    session_socket: IpcServer,
    pty_owner: &Arc<PtyOwner>,
    resume_header: &ResumeHeader,
    resume_body: ResumeBody,
    session_start: &SessionStart,
    runtime_event_sender: &Sender<RuntimeEvent>,
) -> Result<(Server, IpcServer), Box<dyn std::error::Error>> {
    let mut rebuilt_session = resume_session_readers(
        session_server,
        pty_owner,
        resume_header,
        resume_body,
        runtime_event_sender,
    );

    let token_rotation_result = session_socket.rotate_token();
    let _ = std::fs::remove_file(resolve_resume_file_path(
        &session_start.runtime_directory,
        session_start.session_id,
    ));
    token_rotation_result?;
    finish_session_resume(&mut rebuilt_session, runtime_event_sender);
    Ok((rebuilt_session, session_socket))
}

/// Resume every pane's reader and build the session back from `resume_body`, on the
/// `koshi.kdl` now on disk. Touches no control socket.
fn resume_session_readers(
    session_server: Server,
    pty_owner: &Arc<PtyOwner>,
    resume_header: &ResumeHeader,
    resume_body: ResumeBody,
    runtime_event_sender: &Sender<RuntimeEvent>,
) -> Server {
    pty_owner.resume_readers();

    let pty_handle_by_pane_id = resume_header
        .carried_panes
        .iter()
        .map(|carried_pane| {
            (
                carried_pane.pane_id,
                PtyHandle::from_detached_pane_id(carried_pane.pane_id),
            )
        })
        .collect();
    let pty_backend: Arc<dyn PtyBackend> = pty_owner.clone();
    let mut rebuilt_session = Server::resume(
        pty_backend,
        session_server.into_inbox_rx(),
        runtime_event_sender.clone(),
        resume_body,
        pty_handle_by_pane_id,
        build_carried_pty_sizes(resume_header),
    );
    // The session comes back on the `koshi.kdl` that is on disk now.
    rebuilt_session.load_startup_config(koshi_link::config::load_app_layer());
    rebuilt_session
}

/// Apply what the inbox holds, taking detaches, and arm the window a carried
/// client has to attach again in.
///
/// A detach for a client still awaiting its re-attach is dropped by the
/// runtime; a client that attached again and then hung up is detached here.
fn finish_session_resume(
    rebuilt_session: &mut Server,
    runtime_event_sender: &Sender<RuntimeEvent>,
) {
    apply_queued_runtime_events(rebuilt_session, DetachPolicy::Apply);
    start_reconnect_deadline(runtime_event_sender.clone());
}

/// The command that starts the image replacing this one: the same session, in
/// the same directory, under the same `--allow-other-users` flag, coming up
/// from the carried state at `resume_file_path`.
fn build_resume_command(
    session_start: &SessionStart,
    resume_file_path: &Path,
) -> std::process::Command {
    let mut process_command = std::process::Command::new(&session_start.executable_path);
    process_command
        .arg(SESSION_SERVER_SUBCOMMAND)
        .arg(session_start.session_id.to_string())
        .arg(&session_start.session_name)
        .arg(RUNTIME_DIRECTORY_FLAG)
        .arg(&session_start.runtime_directory)
        .arg(RESUME_FLAG)
        .arg(resume_file_path);
    if session_start.is_other_user_access_allowed {
        process_command.arg(ALLOW_OTHER_USERS_FLAG);
    }
    if let Some(supervisor_token) = &session_start.supervisor_token {
        process_command
            .arg(SUPERVISOR_TOKEN_FLAG)
            .arg(supervisor_token);
    }
    if let Some(supervisor_process_id) = session_start.supervisor_process_id {
        process_command
            .arg(SUPERVISOR_PID_FLAG)
            .arg(supervisor_process_id.to_string());
    }
    process_command
}

/// Let every terminal the header names cross the image swap, by clearing the
/// close-on-exec flag the descriptor carries.
///
/// The new image sets the flag again the moment it takes the pane back.
///
/// # Errors
/// Returns the OS error of a descriptor whose flags could not be read or
/// written.
#[cfg(unix)]
fn keep_terminals_across_exec(resume_header: &ResumeHeader) -> std::io::Result<()> {
    for carried_pane in &resume_header.carried_panes {
        if let Some(terminal_file_descriptor) = carried_pane.terminal_fd {
            set_terminal_cloexec(terminal_file_descriptor, false)?;
        }
    }
    Ok(())
}

/// Put the close-on-exec flag back on every terminal the header names, after a
/// swap that did not happen. A descriptor without the flag is inherited by the
/// next pane's child.
#[cfg(unix)]
fn put_close_on_exec_back(resume_header: &ResumeHeader) {
    for carried_pane in &resume_header.carried_panes {
        if let Some(terminal_file_descriptor) = carried_pane.terminal_fd {
            let _ = set_terminal_cloexec(terminal_file_descriptor, true);
        }
    }
}

/// Replace this process's running image with the binary the session was started
/// from. The call returns only when the exec failed, and hands back that error,
/// on the terms
/// [`exec_and_keep_ignoring_sigpipe`](crate::process::exec_and_keep_ignoring_sigpipe)
/// states.
///
/// A successful exec keeps every pane's terminal, whose close-on-exec flag was
/// cleared just before. The process id does not change, so each pane's child
/// keeps its parent and can still be waited on.
#[cfg(unix)]
fn restart_session_by_exec(
    session_start: &SessionStart,
    resume_file_path: &Path,
) -> std::io::Error {
    crate::process::exec_and_keep_ignoring_sigpipe(&mut build_resume_command(
        session_start,
        resume_file_path,
    ))
}

/// Start the binary the session was started from as the image replacing this
/// one, and leave it to bind the socket this process has already withdrawn.
///
/// The new image is detached with a process group of its own and no console,
/// and its input and output go nowhere. An error means nothing was started.
#[cfg(windows)]
fn hand_over_session_to_new_image(
    session_start: &SessionStart,
    resume_file_path: &Path,
) -> std::io::Result<()> {
    crate::process::configure_detached_process(&mut build_resume_command(
        session_start,
        resume_file_path,
    ))
    .spawn()
    .map(|_| ())
}

/// Serve the runtime inbox until the session ends: block until an event is due
/// (bounded by the next render deadline), apply it and any others already
/// queued, hand a fresh snapshot to any subscriber that lost a critical event,
/// push every attached client its frame when a render is due, and stop once the
/// inbox loses its last sender, a [`RuntimeEvent::Quit`] arrives, a `core:quit`
/// command is applied — in this loop or before it was entered, the swap
/// included — no pane is left running, or a restart request is accepted.
///
/// A quit waits while any client is still expected back from an image swap, so
/// that client attaches and reads what ended the session instead of finding one
/// that stopped answering. Its window empties that set, so the wait is bounded.
/// A session with no pane left running ends either way.
///
/// Serving the inbox is what makes the control socket work: a command
/// forwarded over it and a discovery query asking what this session holds both
/// arrive here as events.
///
/// This process paints nothing itself; the frames it builds go out over the
/// socket to the clients attached to it.
fn run_session_serve_loop(session_server: &mut Server) -> ServeOutcome {
    loop {
        // A `core:quit` applied outside this loop ends the session before the
        // wait below: the image swap applies whatever the inbox holds, and the
        // rebuild after a swap that did not start applies it again.
        //
        // A session still expecting a client back from an image swap keeps
        // serving instead, so that client attaches and reads the quit rather
        // than finding a session that stopped answering. Its window empties
        // the set, so this waits at most that long.
        if session_server.is_quit_requested() && !session_server.awaits_a_client() {
            return ServeOutcome::Ended;
        }
        let current_time = Instant::now();
        let runtime_event = match session_server.next_render_wakeup(current_time) {
            Some(render_wakeup_timeout) => match session_server
                .inbox_rx()
                .recv_timeout(render_wakeup_timeout)
            {
                Ok(runtime_event) => Some(runtime_event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return ServeOutcome::Ended,
            },
            None => match session_server.inbox_rx().recv() {
                Ok(runtime_event) => Some(runtime_event),
                Err(_) => return ServeOutcome::Ended,
            },
        };
        let mut should_quit = false;
        if let Some(runtime_event) = runtime_event {
            should_quit |= session_server
                .handle_runtime_event(runtime_event)
                .is_break();
        }
        // Apply anything else already queued before building one frame.
        while let Ok(runtime_event) = session_server.inbox_rx().try_recv() {
            should_quit |= session_server
                .handle_runtime_event(runtime_event)
                .is_break();
        }
        // A subscriber that lost a critical event is paused until it is handed
        // a fresh snapshot; queue that snapshot now so it is applied in this
        // pass and the frame pushed below is built from it.
        session_server.resync_lagged();
        if session_server.poll_render(Instant::now()) {
            session_server.push_frames();
        }
        if (should_quit || session_server.is_quit_requested()) && !session_server.awaits_a_client()
        {
            return ServeOutcome::Ended;
        }
        if !session_server.has_active_panes() {
            return ServeOutcome::Ended;
        }
        if session_server.is_restart_requested() {
            return ServeOutcome::Restart;
        }
    }
}
