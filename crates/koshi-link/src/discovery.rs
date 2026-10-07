//! Answering the discovery queries across every running koshi.
//!
//! Each running process answers one question — describe yourself, as a
//! [`koshi_core::discovery::SessionOverview`]. This module does the rest
//! locally. It asks every session of this user's, each endpoint file in the
//! runtime directory and each session restarting into a new build. While
//! `allow-other-users` is on, it also asks every session the shared directory
//! advertises for the other local users of this machine. Every session is
//! asked at the same time. The module drops the sessions nothing listens
//! behind, and counts each session that could not be asked and each directory
//! that could not be read. It turns the answers into the rows a listing prints
//! or the single record an `inspect` prints.
//!
//! A listing row is an id chain plus the names on it: a pane row names its
//! pane, its tab, and its session, in the form a `--pane`/`--tab`/`--session`
//! flag takes. The full detail of one
//! entity — creation time, working directory, argv, lock state — belongs to
//! `inspect`, which renders the `koshi-core` structs themselves.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use koshi_core::discovery::{ClientDiscovery, PaneDiscovery, SessionOverview, TabDiscovery};
use koshi_core::event::RejectReason;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::redact::redact_command_argv;
use koshi_core::text::{format_counted_noun, sanitize_reported_text};
use koshi_ipc::endpoint::{is_refusal_from_live_session, is_replacing_its_image, EndpointFile};
use koshi_ipc::error::IpcError;
use koshi_ipc::validate::reclaim_stale_socket;
use serde::Serialize;

use crate::error::CliError;
use crate::ipc_client::{self, DuplicatedForeignSession, ForeignSessionListing, UnreadPath};

/// One `list-sessions` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionRow {
    /// Stable session id.
    pub session_id: SessionId,
    /// The session's display name.
    pub session_name: String,
    /// The saved server this session runs on, by the name it was saved under
    /// or its `host:port` address. `None` for a session on this machine.
    pub server_name_or_address: Option<String>,
}

impl SessionRow {
    /// One row for `session_id`, naming `server_name_or_address`, with
    /// `session_name` filtered by
    /// [`sanitize_reported_text`].
    ///
    /// `SessionRow::from_session(id, "web\u{7f}srv", None).session_name` is
    /// `"websrv"`.
    #[must_use]
    pub fn from_session(
        session_id: SessionId,
        session_name: &str,
        server_name_or_address: Option<String>,
    ) -> Self {
        SessionRow {
            session_id,
            session_name: sanitize_reported_text(session_name),
            server_name_or_address,
        }
    }
}

/// One `list-tabs` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TabRow {
    /// Stable tab id.
    pub tab_id: TabId,
    /// The tab's display name.
    pub tab_name: String,
    /// The session holding the tab.
    pub session_id: SessionId,
    /// That session's display name.
    pub session_name: String,
}

/// One `list-panes` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaneRow {
    /// Stable pane id.
    pub pane_id: PaneId,
    /// The pane's title, once the child has set one.
    pub pane_name: Option<String>,
    /// The tab holding the pane.
    pub tab_id: TabId,
    /// That tab's display name.
    pub tab_name: String,
    /// The session holding the pane.
    pub session_id: SessionId,
    /// That session's display name.
    pub session_name: String,
}

/// One `list-clients` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClientRow {
    /// Stable client id.
    pub client_id: ClientId,
    /// The session the client is attached to.
    pub session_id: SessionId,
    /// That session's display name.
    pub session_name: String,
}

/// What one sweep found: every session that answered, plus how many running
/// sessions could not be asked and how many paths could not be read.
///
/// With one running session unasked, the paths that would answer "no running
/// session has pane X" or "the one session is the default" report the gap
/// instead. A session that is gone is not counted as unasked.
#[derive(Debug, Default)]
pub struct Discovered {
    /// The sessions that answered, sorted by name and then by id.
    pub sessions: Vec<SessionOverview>,
    /// How many sessions did not answer: each session that was listening but
    /// could not finish the exchange, each session restarting, each session
    /// id the shared directory advertises more than once, and each session the
    /// shared directory holds past its caps.
    pub unasked_session_count: usize,
    /// How many paths the listings of sessions could not read, as
    /// [`SessionCensusPlan::unread_paths`] holds them. The sessions under each
    /// one are unknown.
    pub unread_path_count: usize,
}

impl Discovered {
    /// One session, asked directly and answered — a complete census of the
    /// only session the query is about.
    #[must_use]
    pub fn from_overview(session_overview: SessionOverview) -> Discovered {
        Discovered {
            sessions: vec![session_overview],
            unasked_session_count: 0,
            unread_path_count: 0,
        }
    }

    /// Whether every running session answered, and every listing read
    /// everything.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.unasked_session_count == 0 && self.unread_path_count == 0
    }

    /// Sort the sessions by name and then id, the order
    /// [`sessions`](Self::sessions) documents.
    pub fn sort_sessions(&mut self) {
        self.sessions
            .sort_by(|left_session_overview, right_session_overview| {
                left_session_overview
                    .session
                    .session_name
                    .cmp(&right_session_overview.session.session_name)
                    .then(
                        left_session_overview
                            .session
                            .session_id
                            .cmp(&right_session_overview.session.session_id),
                    )
            });
    }

    /// The failure for a target that none of the answering sessions holds:
    /// genuinely not found when every session answered, otherwise a report
    /// that one of them could not be asked.
    pub fn build_missing_target_error(
        &self,
        target_kind: &str,
        target_identifier: &str,
    ) -> CliError {
        if self.is_complete() {
            CliError::CommandRejected {
                reason: RejectReason::TargetNotFound,
                help: Some(format!(
                    "no running session has {target_kind} {target_identifier}"
                )),
            }
        } else {
            self.build_unanswered_error(&format!(
                "{target_kind} {target_identifier} is in none of the sessions that answered"
            ))
        }
    }

    /// The failure for a `--session` no answering session matched: not
    /// running when every session answered, otherwise a report that one
    /// could not be asked.
    pub fn build_missing_session_error(&self, session_name: &str) -> CliError {
        if self.is_complete() {
            CliError::SessionNotFound {
                session_name: session_name.to_string(),
            }
        } else {
            self.build_unanswered_error(&format!(
                "`{session_name}` is not among the sessions that answered"
            ))
        }
    }

    /// The failure a listing ends with when it could not see everything, or
    /// `None` when it could.
    ///
    /// A listing prints the rows it has either way, and the exit code carries
    /// the gap: `koshi list-panes` with one session unable to answer prints the
    /// other sessions' panes and still exits 4.
    #[must_use]
    pub fn find_incomplete_listing_error(&self) -> Option<CliError> {
        if self.is_complete() {
            None
        } else {
            Some(self.build_unanswered_error("this listing is incomplete"))
        }
    }

    /// A failure that names `error_detail`, how many running sessions went
    /// unasked, and how many paths could not be read. Each count is singular
    /// only at exactly 1. The session count is left out while it is 0 and a
    /// path could not be read, and the path count is left out while it is 0.
    ///
    /// `build_unanswered_error("this listing is incomplete")` with
    /// `unasked_session_count` 1 gives
    /// `"this listing is incomplete (1 running session did not answer)"`.
    /// With `unread_path_count` 1 as well it gives `"this listing is
    /// incomplete (1 running session did not answer; 1 path could not be
    /// read)"`, and with `unread_path_count` 1 alone it gives `"this listing
    /// is incomplete (1 path could not be read)"`.
    pub fn build_unanswered_error(&self, error_detail: &str) -> CliError {
        let mut unanswered_clauses = Vec::new();
        if self.unasked_session_count > 0 || self.unread_path_count == 0 {
            unanswered_clauses.push(format!(
                "{} did not answer",
                format_counted_noun(
                    self.unasked_session_count,
                    "running session",
                    "running sessions"
                )
            ));
        }
        if self.unread_path_count > 0 {
            unanswered_clauses.push(format_unread_path_count(self.unread_path_count));
        }
        CliError::IpcUnavailable {
            detail: format!("{error_detail} ({})", unanswered_clauses.join("; ")),
        }
    }
}

/// `unread_path_count` as a clause: `1 path could not be read`, and `3 paths
/// could not be read` for `3`.
#[must_use]
pub fn format_unread_path_count(unread_path_count: usize) -> String {
    format!(
        "{} could not be read",
        format_counted_noun(unread_path_count, "path", "paths")
    )
}

/// The sessions one census asks, and what it knows it cannot ask before
/// asking, as [`plan_session_census`] gathers them.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SessionCensusPlan {
    /// Each session to ask, with the control-socket address the shared
    /// directory gives a session another local user started, or `None` for a
    /// session of this user's. This user's sessions come first.
    pub session_asks: Vec<(SessionId, Option<String>)>,
    /// Each session id the shared directory advertises more than once. None of
    /// them is asked.
    pub duplicated_sessions: Vec<DuplicatedForeignSession>,
    /// How many sessions the shared directory holds past its caps, as
    /// [`ForeignSessionListing::unlisted_session_count`](ipc_client::ForeignSessionListing::unlisted_session_count)
    /// counts them.
    pub unlisted_session_count: usize,
    /// Each path a listing could not read, each once: `runtime_directory` when
    /// this user's sessions cannot be listed, and the
    /// [`unread_path`](ipc_client::ForeignSessionListing::unread_path) of the
    /// shared listing.
    pub unread_paths: Vec<UnreadPath>,
}

/// Every session a census asks: this user's own, as
/// [`list_own_sessions`](ipc_client::list_own_sessions) gives them from
/// `runtime_directory`, and while `shared_sessions_base_directory` is given,
/// the ones other local users started, as
/// [`list_foreign_sessions`](ipc_client::list_foreign_sessions) lists them.
///
/// Example: a runtime directory with an endpoint file for `A`, and a shared
/// directory whose read fails with `EIO`, give one ask for `A` and one unread
/// path naming the shared directory.
#[must_use]
pub fn plan_session_census(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
) -> SessionCensusPlan {
    let own_session_listing = ipc_client::list_own_sessions(runtime_directory);
    let foreign_session_listing =
        shared_sessions_base_directory.map(|shared_sessions_base_directory| {
            ipc_client::list_foreign_sessions(shared_sessions_base_directory, runtime_directory)
        });
    SessionCensusPlan::from_session_listings(own_session_listing, foreign_session_listing)
}

impl SessionCensusPlan {
    /// The census plan of `own_session_listing`, as
    /// [`list_own_sessions`](ipc_client::list_own_sessions) gives it, and of
    /// `foreign_session_listing`, as
    /// [`list_foreign_sessions`](ipc_client::list_foreign_sessions) gives it,
    /// or `None` when no shared directory is listed. An unread path that both
    /// listings give is kept once.
    ///
    /// Example: own sessions `[A]`, and a foreign listing of `B` that holds 2
    /// sessions past its caps, give the asks `[(A, None), (B, Some(<address of
    /// B>))]` and an `unlisted_session_count` of 2.
    fn from_session_listings(
        own_session_listing: Result<Vec<SessionId>, UnreadPath>,
        foreign_session_listing: Option<ForeignSessionListing>,
    ) -> SessionCensusPlan {
        let mut census_plan = SessionCensusPlan::default();
        match own_session_listing {
            Ok(own_session_ids) => census_plan.session_asks.extend(
                own_session_ids
                    .into_iter()
                    .map(|session_id| (session_id, None)),
            ),
            Err(unread_path) => census_plan.unread_paths.push(unread_path),
        }
        let Some(foreign_session_listing) = foreign_session_listing else {
            return census_plan;
        };
        census_plan.session_asks.extend(
            foreign_session_listing
                .foreign_sessions
                .into_iter()
                .map(|(session_id, socket_address)| (session_id, Some(socket_address))),
        );
        census_plan.duplicated_sessions = foreign_session_listing.duplicated_sessions;
        census_plan.unlisted_session_count = foreign_session_listing.unlisted_session_count;
        if let Some(unread_path) = foreign_session_listing.unread_path {
            if !census_plan.unread_paths.contains(&unread_path) {
                census_plan.unread_paths.push(unread_path);
            }
        }
        census_plan
    }
}

/// Ask every session of this user's that `runtime_directory` holds to describe
/// itself, and every session `shared_sessions_base_directory` advertises for
/// the other local users of this machine. `shared_sessions_base_directory` is
/// the directory
/// [`resolve_shared_sessions_base_directory`](ipc_client::resolve_shared_sessions_base_directory)
/// names; `None` asks only this user's sessions.
///
/// Up to [`MAX_SESSIONS_ASKED_AT_ONCE`](ipc_client::MAX_SESSIONS_ASKED_AT_ONCE)
/// sessions are asked at the same time through
/// [`ask_sessions_at_once`](ipc_client::ask_sessions_at_once), and every
/// exchange ends by one deadline,
/// [`SESSION_ANSWER_TIMEOUT_DURATION`](ipc_client::SESSION_ANSWER_TIMEOUT_DURATION)
/// after the census starts: each connect, each write and read, and the
/// recheck of a refused connect.
/// Example: 30 sessions, one of them stopped with `SIGSTOP`, are answered in
/// 5 seconds, not 5 seconds per stopped session.
///
/// This user's sessions are the ones [`list_own_sessions`](ipc_client::list_own_sessions)
/// gives, each asked through [`fetch_session_overview`] with no shared
/// directory. A session that is gone contributes no rows and is not counted
/// as unasked. A session of this user's that is gone also loses its endpoint
/// file and its socket file, through [`fetch_session_overview`]. A session
/// that is listening but cannot finish the exchange, and one that is
/// restarting, contribute no rows either, say so on stderr, and are counted.
///
/// The sessions of other users are the ones
/// [`list_foreign_sessions`](ipc_client::list_foreign_sessions) lists, each
/// asked at the address it lists and never swept. Each id it finds advertised
/// more than once is counted, and says so on stderr. So is the count of
/// sessions past its caps, on one stderr line.
///
/// The sessions come from [`plan_session_census`], and
/// [`print_census_gap_notes`] says on stderr what it left unasked. Each path
/// a listing could not read is counted in
/// [`unread_path_count`](Discovered::unread_path_count).
#[must_use]
pub fn fetch_all_session_overviews(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
) -> Discovered {
    let answer_deadline = Instant::now() + ipc_client::SESSION_ANSWER_TIMEOUT_DURATION;
    let census_plan = plan_session_census(runtime_directory, shared_sessions_base_directory);
    fetch_planned_session_overviews(runtime_directory, census_plan, answer_deadline)
}

/// Ask every session `census_plan` holds to describe itself, as
/// [`fetch_all_session_overviews`] describes, with every exchange ending by
/// `answer_deadline`. The sessions of this user's are looked up in
/// `runtime_directory`.
///
/// Each session past the caps and each duplicated id counts in
/// [`unasked_session_count`](Discovered::unasked_session_count), and each
/// unread path counts in [`unread_path_count`](Discovered::unread_path_count).
/// Example: a plan of 2 sessions past the caps and nothing to ask gives no
/// sessions and an `unasked_session_count` of 2.
fn fetch_planned_session_overviews(
    runtime_directory: &Path,
    census_plan: SessionCensusPlan,
    answer_deadline: Instant,
) -> Discovered {
    print_census_gap_notes(&census_plan);
    let mut discovered_sessions = Discovered {
        unasked_session_count: census_plan.unlisted_session_count,
        unread_path_count: census_plan.unread_paths.len(),
        ..Discovered::default()
    };
    for duplicated_session in &census_plan.duplicated_sessions {
        record_discovery_answer(
            &mut discovered_sessions,
            duplicated_session.session_id,
            Err(duplicated_session.build_refusal_error()),
        );
    }
    let session_asks = census_plan.session_asks;
    let session_answers = ipc_client::ask_sessions_at_once(
        &session_asks,
        answer_deadline,
        |(session_id, foreign_socket_address)| match foreign_socket_address {
            None => {
                fetch_session_overview(runtime_directory, None, *session_id, Some(answer_deadline))
            }
            Some(foreign_socket_address) => ipc_client::fetch_foreign_session_overview(
                *session_id,
                foreign_socket_address,
                answer_deadline,
            ),
        },
    );
    for ((session_id, _), session_answer) in session_asks.iter().zip(session_answers) {
        record_discovery_answer(&mut discovered_sessions, *session_id, session_answer);
    }
    discovered_sessions.sort_sessions();
    discovered_sessions
}

/// Say on stderr what `census_plan` leaves unasked before any session is
/// asked: one line for the sessions the shared directory holds past its caps,
/// and one line per unread path. Nothing prints when it leaves nothing.
///
/// Example: `300` sessions past the caps print `koshi: 300 sessions in the
/// shared directory were not asked: they are past the listing limit`, and `1`
/// prints `koshi: 1 session in the shared directory was not asked: it is past
/// the listing limit`. An unread `/tmp/koshi/1002` prints `koshi: some
/// sessions were not asked: /tmp/koshi/1002 could not be read: Input/output
/// error (os error 5)`.
pub fn print_census_gap_notes(census_plan: &SessionCensusPlan) {
    match census_plan.unlisted_session_count {
        0 => {}
        1 => eprintln!(
            "koshi: 1 session in the shared directory was not asked: it is past the listing limit"
        ),
        unlisted_session_count => eprintln!(
            "koshi: {unlisted_session_count} sessions in the shared directory were not asked: \
             they are past the listing limit"
        ),
    }
    for unread_path in &census_plan.unread_paths {
        eprintln!("koshi: some sessions were not asked: {unread_path}");
    }
}

/// Fold what the session `session_id` answered into `discovered_sessions`: an
/// overview becomes a row, a session that is gone adds nothing, and every
/// other failure prints on stderr and increments `unasked_session_count`.
fn record_discovery_answer(
    discovered_sessions: &mut Discovered,
    session_id: SessionId,
    session_overview_result: Result<SessionOverview, CliError>,
) {
    match session_overview_result {
        Ok(session_overview) => discovered_sessions.sessions.push(session_overview),
        Err(CliError::SessionNotFound { .. }) => {}
        Err(cli_error) => {
            eprintln!("koshi: session {session_id} did not answer: {cli_error}");
            discovered_sessions.unasked_session_count += 1;
        }
    }
}

/// How long, from its first connect, an attempt whose connect was refused
/// connects again while [`is_refusal_from_live_session`] accepts the session's
/// process: 1 second.
pub const REFUSED_SESSION_RECHECK_WINDOW_DURATION: Duration = Duration::from_secs(1);

/// The pause between two connects of that recheck: 50 ms.
pub const REFUSED_SESSION_RECHECK_INTERVAL_DURATION: Duration = Duration::from_millis(50);

/// Make `make_attempt` once, and again while `is_refused` accepts what it
/// gave and [`is_refusal_from_live_session`] accepts the process the endpoint
/// file of `session_id` in `runtime_directory` names. Hands back what the last
/// attempt gave.
///
/// The recheck ends [`REFUSED_SESSION_RECHECK_WINDOW_DURATION`] after the
/// first attempt starts, or at `answer_deadline` when that comes first. After
/// each refused attempt it waits [`REFUSED_SESSION_RECHECK_INTERVAL_DURATION`],
/// or until the recheck ends when that is sooner, and no attempt starts once
/// the recheck has ended. Example: a session that keeps refusing while
/// `answer_deadline` is 120 ms away gets attempts at about 0, 50 and 100 ms,
/// and the refusal of the one at 100 ms is handed back at 120 ms.
///
/// On Linux and Windows the check gives `false`, so one attempt is made. The
/// router's probes and descriptions, and [`fetch_session_overview`], make
/// their attempts through this.
/// Example: on macOS a session killed with `SIGKILL` refuses a connect a
/// moment before its process is gone; the attempt made after the process is
/// gone hands back the refusal.
pub fn repeat_while_live_session_refuses<AttemptOutcome>(
    runtime_directory: &Path,
    session_id: SessionId,
    answer_deadline: Option<Instant>,
    mut make_attempt: impl FnMut() -> AttemptOutcome,
    is_refused: impl Fn(&AttemptOutcome) -> bool,
) -> AttemptOutcome {
    let recheck_window_end = Instant::now() + REFUSED_SESSION_RECHECK_WINDOW_DURATION;
    let recheck_end = match answer_deadline {
        Some(answer_deadline) => recheck_window_end.min(answer_deadline),
        None => recheck_window_end,
    };
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
    let mut attempt_outcome = make_attempt();
    while is_refused(&attempt_outcome) {
        let recheck_time_left = recheck_end.saturating_duration_since(Instant::now());
        if recheck_time_left.is_zero() {
            break;
        }
        let is_process_live =
            EndpointFile::load_from_path(&endpoint_path).is_ok_and(|endpoint_file| {
                is_refusal_from_live_session(&endpoint_path, endpoint_file.process_id)
            });
        if !is_process_live {
            break;
        }
        std::thread::sleep(recheck_time_left.min(REFUSED_SESSION_RECHECK_INTERVAL_DURATION));
        if Instant::now() >= recheck_end {
            break;
        }
        attempt_outcome = make_attempt();
    }
    attempt_outcome
}

/// Ask the one session `session_id` to describe itself, sweeping what it
/// left behind if it is gone.
///
/// Each attempt reads how to reach the session and asks it through
/// [`run_session_exchange_with_restart_wait`](ipc_client::run_session_exchange_with_restart_wait):
/// a session of this user's that refuses this build's protocol version is
/// asked again once it has restarted. A refused connect is made again
/// through [`repeat_while_live_session_refuses`]. Nothing listening then is
/// [`CliError::SessionNotFound`]. When that last attempt read this user's own
/// endpoint file, the session's files go through `delete_stale_session_files`,
/// and a session that function keeps is the [`CliError::IpcUnavailable`] it
/// gives. Something listening that replies in the envelope of koshi 0.4.0 or
/// older is [`CliError::PreviousReleaseServer`]. Something listening whose
/// exchange failed in any other way — a token that no longer matches, say — is
/// [`CliError::IpcUnavailable`].
///
/// With `answer_deadline`, each connect and every write and read after it end
/// by that moment, as
/// [`fetch_session_overview_from_endpoint`](ipc_client::fetch_session_overview_from_endpoint)
/// states, and the recheck of a refused connect and the wait for a restart end
/// by it too. With `None`, the exchange waits for the answer however long it
/// takes.
///
/// `shared_sessions_base_directory` is searched for `session_id` when
/// `runtime_directory` holds no endpoint file for it, through
/// [`ipc_client::load_session_endpoint`].
pub fn fetch_session_overview(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    session_id: SessionId,
    answer_deadline: Option<Instant>,
) -> Result<SessionOverview, CliError> {
    let mut asked_endpoint_file: Option<EndpointFile> = None;
    let session_overview_result = repeat_while_live_session_refuses(
        runtime_directory,
        session_id,
        answer_deadline,
        || {
            asked_endpoint_file = None;
            ipc_client::run_session_exchange_with_restart_wait(
                runtime_directory,
                shared_sessions_base_directory,
                session_id,
                answer_deadline,
                |session_endpoint| {
                    let session_overview_result = ipc_client::fetch_session_overview_from_endpoint(
                        session_endpoint,
                        session_id,
                        answer_deadline,
                    );
                    asked_endpoint_file = Some(session_endpoint.clone());
                    session_overview_result
                },
            )
        },
        |session_overview_result| {
            matches!(
                session_overview_result,
                Err(CliError::SessionNotFound { .. })
            )
        },
    );
    if let (Err(CliError::SessionNotFound { .. }), Some(asked_endpoint_file)) =
        (&session_overview_result, &asked_endpoint_file)
    {
        delete_stale_session_files(runtime_directory, session_id, asked_endpoint_file)?;
    }
    session_overview_result
}

/// Remove what the session `session_id` of this user's left behind once
/// nothing listened at what `asked_endpoint_file` names: its endpoint file in
/// `runtime_directory`, and the socket file that endpoint file names. Checked
/// in this order, and the first that holds keeps every file:
///
/// 1. [`is_replacing_its_image`] accepts the session: it is restarting.
/// 2. The endpoint file is gone: nothing is left to remove. A session that
///    [`is_replacing_its_image`] accepts once the file is gone is restarting.
/// 3. The endpoint file cannot be read, or holds anything but
///    `asked_endpoint_file`, such as the new connection token of a session that
///    restarted since: it is kept.
/// 4. [`is_refusal_from_live_session`] accepts the process the endpoint file
///    names.
///
/// A foreign endpoint, which no endpoint file of this user's holds, removes
/// nothing. Every removal is best-effort — a file already removed, or one this
/// user may not remove, leaves the listing unaffected.
///
/// # Errors
/// The failure [`ipc_client::build_session_restarting_error`] gives, for a
/// session restarting. [`CliError::IpcUnavailable`] naming the read failure for
/// an endpoint file that cannot be read, and the same restarting failure for
/// one that holds anything but `asked_endpoint_file`.
/// [`CliError::IpcUnavailable`] reading `process 5000 runs but accepts no
/// connection` when the endpoint file names process `5000` and
/// [`is_refusal_from_live_session`] accepts it.
fn delete_stale_session_files(
    runtime_directory: &Path,
    session_id: SessionId,
    asked_endpoint_file: &EndpointFile,
) -> Result<(), CliError> {
    if is_replacing_its_image(runtime_directory, session_id) {
        return Err(ipc_client::build_session_restarting_error(session_id));
    }
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
    let current_endpoint_file = match EndpointFile::load_from_path(&endpoint_path) {
        Ok(current_endpoint_file) => current_endpoint_file,
        Err(IpcError::EndpointFileMissing { .. }) => {
            if is_replacing_its_image(runtime_directory, session_id) {
                return Err(ipc_client::build_session_restarting_error(session_id));
            }
            return Ok(());
        }
        Err(endpoint_file_error) => {
            return Err(CliError::IpcUnavailable {
                detail: endpoint_file_error.to_string(),
            });
        }
    };
    if current_endpoint_file != *asked_endpoint_file {
        return Err(ipc_client::build_session_restarting_error(session_id));
    }
    if is_refusal_from_live_session(&endpoint_path, current_endpoint_file.process_id) {
        return Err(CliError::IpcUnavailable {
            detail: format!(
                "process {} runs but accepts no connection",
                current_endpoint_file.process_id
            ),
        });
    }
    let _ = reclaim_stale_socket(&current_endpoint_file.socket_address);
    let _ = std::fs::remove_file(&endpoint_path);
    Ok(())
}

/// The `list-sessions` answer: one row per running session. Every row's
/// `server_name_or_address` is `None` — each session in `session_overviews` runs on this machine.
#[must_use]
pub fn build_session_rows(session_overviews: &[SessionOverview]) -> Vec<SessionRow> {
    session_overviews
        .iter()
        .map(|session_overview| {
            SessionRow::from_session(
                session_overview.session.session_id,
                &session_overview.session.session_name,
                None,
            )
        })
        .collect()
}

/// The `list-tabs` answer: every tab of every listed session, in tab-bar
/// order within each session.
#[must_use]
pub fn build_tab_rows(session_overviews: &[SessionOverview]) -> Vec<TabRow> {
    session_overviews
        .iter()
        .flat_map(|session_overview| {
            session_overview.tabs.iter().map(|tab_discovery| TabRow {
                tab_id: tab_discovery.tab_id,
                tab_name: sanitize_reported_text(&tab_discovery.tab_name),
                session_id: session_overview.session.session_id,
                session_name: sanitize_reported_text(&session_overview.session.session_name),
            })
        })
        .collect()
}

/// The `list-panes` answer: every pane of every listed session, in the
/// overview's own order — tab-bar order, then layout order within a tab.
///
/// A pane whose tab is not in the overview's tab list is left out.
#[must_use]
pub fn build_pane_rows(session_overviews: &[SessionOverview]) -> Vec<PaneRow> {
    session_overviews
        .iter()
        .flat_map(|session_overview| {
            session_overview.panes.iter().filter_map(|pane_discovery| {
                let tab_discovery = session_overview
                    .tabs
                    .iter()
                    .find(|tab_discovery| tab_discovery.tab_id == pane_discovery.tab_id)?;
                Some(PaneRow {
                    pane_id: pane_discovery.pane_id,
                    pane_name: pane_discovery
                        .pane_title
                        .as_deref()
                        .map(sanitize_reported_text),
                    tab_id: tab_discovery.tab_id,
                    tab_name: sanitize_reported_text(&tab_discovery.tab_name),
                    session_id: session_overview.session.session_id,
                    session_name: sanitize_reported_text(&session_overview.session.session_name),
                })
            })
        })
        .collect()
}

/// The `list-clients` answer: every client attached to every listed session.
#[must_use]
pub fn build_client_rows(session_overviews: &[SessionOverview]) -> Vec<ClientRow> {
    session_overviews
        .iter()
        .flat_map(|session_overview| {
            session_overview
                .clients
                .iter()
                .map(|client_discovery| ClientRow {
                    client_id: client_discovery.client_id,
                    session_id: session_overview.session.session_id,
                    session_name: sanitize_reported_text(&session_overview.session.session_name),
                })
        })
        .collect()
}

/// Filter every string `session_overview` took from the session that answered through
/// [`sanitize_reported_text`]: the session name, each tab name, and each pane's
/// title, working directory and argv. Ids, times, sizes and counts are left as
/// they are.
///
/// Every overview read off a socket passes through this before a listing row,
/// an `inspect` record, or a `--session <name>` lookup reads it.
///
/// A pane whose argv is `["sh", "-c", "\u{1b}[2J"]` reads back as
/// `["sh", "-c", "[2J"]`.
pub fn filter_session_overview_text(session_overview: &mut SessionOverview) {
    session_overview.session.session_name =
        sanitize_reported_text(&session_overview.session.session_name);
    for tab_discovery in &mut session_overview.tabs {
        tab_discovery.tab_name = sanitize_reported_text(&tab_discovery.tab_name);
    }
    for pane_discovery in &mut session_overview.panes {
        pane_discovery.pane_title = pane_discovery
            .pane_title
            .as_deref()
            .map(sanitize_reported_text);
        pane_discovery.working_directory =
            pane_discovery
                .working_directory
                .as_ref()
                .map(|working_directory| {
                    PathBuf::from(sanitize_reported_text(&working_directory.to_string_lossy()))
                });
        if let Some(command_argv) = &mut pane_discovery.command_argv {
            for command_argument in command_argv.iter_mut() {
                *command_argument = sanitize_reported_text(command_argument);
            }
        }
    }
}

/// Hide the arguments of every pane's command across `session_overviews`, leaving
/// each program name visible.
pub fn redact_pane_commands(session_overviews: &mut [SessionOverview]) {
    for session_overview in session_overviews.iter_mut() {
        for pane_discovery in session_overview.panes.iter_mut() {
            pane_discovery.command_argv = pane_discovery
                .command_argv
                .as_deref()
                .map(redact_command_argv);
        }
    }
}

/// The tab `tab_id` names, in full, wherever it is running.
///
/// No answering session holding it gives [`Discovered::build_missing_target_error`]'s failure for
/// `"tab"`.
pub fn find_tab(discovered_sessions: &Discovered, tab_id: TabId) -> Result<TabDiscovery, CliError> {
    discovered_sessions
        .sessions
        .iter()
        .flat_map(|session_overview| session_overview.tabs.iter())
        .find(|tab_discovery| tab_discovery.tab_id == tab_id)
        .cloned()
        .ok_or_else(|| discovered_sessions.build_missing_target_error("tab", &tab_id.to_string()))
}

/// The pane `pane_id` names, in full, wherever it is running.
///
/// No answering session holding it gives [`Discovered::build_missing_target_error`]'s failure for
/// `"pane"`.
pub fn find_pane(
    discovered_sessions: &Discovered,
    pane_id: PaneId,
) -> Result<PaneDiscovery, CliError> {
    discovered_sessions
        .sessions
        .iter()
        .flat_map(|session_overview| session_overview.panes.iter())
        .find(|pane_discovery| pane_discovery.pane_id == pane_id)
        .cloned()
        .ok_or_else(|| discovered_sessions.build_missing_target_error("pane", &pane_id.to_string()))
}

/// The client `client_id` names, in full, wherever it is attached.
///
/// No answering session holding it gives [`Discovered::build_missing_target_error`]'s failure for
/// `"client"`.
pub fn find_client(
    discovered_sessions: &Discovered,
    client_id: ClientId,
) -> Result<ClientDiscovery, CliError> {
    discovered_sessions
        .sessions
        .iter()
        .flat_map(|session_overview| session_overview.clients.iter())
        .find(|client_discovery| client_discovery.client_id == client_id)
        .cloned()
        .ok_or_else(|| {
            discovered_sessions.build_missing_target_error("client", &client_id.to_string())
        })
}

#[cfg(test)]
mod tests;
