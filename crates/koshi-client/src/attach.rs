//! The attached client: join a running session and become a second window onto
//! it.
//!
//! A session runs in one of two homes, and the home is picked before the first
//! connection opens. A session on this machine is joined over its control
//! socket: the router turns the value after `koshi attach` into that session's
//! address, starting a router first when none runs, and the session's endpoint
//! file holds the token the Hello presents. The Hello and the Attach are
//! written back to back, so joining costs one round trip.
//!
//! A session on another machine is joined over TLS through the server serving
//! it: `koshi attach --remote <server> [session]` presents the secret that
//! server saved, the certificate pinned on the first connection is the only one
//! accepted, and the server resolves the session against the sessions that
//! secret reaches. The server presents that session's endpoint token and writes
//! the Hello on this client's behalf, since a caller on another machine cannot
//! read that file. From the Attach on, both homes speak the same frames on the
//! same loop.
//!
//! With no value at all, `koshi attach` offers the sessions running for this
//! user beside the sessions on every saved server that answered inside one
//! deadline, and the one picked names both the session and its home.
//!
//! Everything that can refuse the join happens before the terminal changes
//! mode: a refused lookup, a refused secret, a session the secret does not
//! reach, a refused Hello, a refused Attach. Once the session answers
//! `Attached`, the terminal enters raw mode and the alternate screen
//! behind a cleanup guard, so every way out
//! — a detach, the session ending, a dead session server, or a panic — leaves
//! the outer terminal as it was found.
//!
//! A session replacing its own process image is not a way out. The session says
//! so before it goes; this client leaves the terminal in every mode it is in
//! and comes back as the same client. On this machine it reads that session's
//! endpoint file until it names a new connection token and joins the new
//! socket; on a server it dials that server again, so the certificate, the
//! secret and the scope are checked again. The session's first frame there
//! paints the same panes back, so the screen does not flicker. A session that
//! moves this client to another session comes back the same way.
//!
//! A dropped link to a session on a server is not a way out either, while
//! `remote-reconnect` is on. The viewer draws
//! `RECONNECTING (attempt 1, retry in 1s)` on its tab strip, counting the
//! seconds down as it waits, and dials that server again — after 1 second, then
//! 2, 4, 8, and 8 seconds before every dial after that — until it joins or 120
//! seconds pass. A dial the server answers with a refusal — a certificate that
//! is not the pinned one, a secret it does not admit, a session that secret does
//! not reach, or a protocol version this build does not accept — is not dialed
//! again: every identical dial gets the same answer, so the viewer stops there.
//! Each dial presents the secret the last attach
//! minted, and the session hands that attach's view back for it: the same
//! active tab, the same focused and zoomed pane of each tab, and the same
//! scroll offset of each pane. A session that no longer holds that view mints a
//! fresh client, and the viewer takes that id as its own. Everything typed
//! while the viewer had no link is dropped and never sent. On the new link the
//! viewer reads the terminal's size again and reports it. The screen keeps the
//! last frame it drew, at the size it was drawn for, until the viewer joins
//! again — a terminal resized over that stretch repaints when the first frame
//! of the new link arrives. A viewer that stops dialing restores its terminal,
//! then prints the cause it stopped on, `the session continues without you`, and
//! the command that reattaches, and exits non-zero. With `remote-reconnect` off,
//! and for a session on this machine, a dropped link ends the client.
//!
//! From there the connection carries traffic both ways. The session composes
//! this terminal's own frame — at this terminal's size and scroll position —
//! and pushes it down the event stream, which this loop paints. This terminal's
//! keys, pastes and resizes travel back up the same connection: a key the
//! viewer's keymap does not bind goes up as a key press, a binding that fires
//! is resolved against the action table here and goes up as the commands it
//! runs, a paste goes up whole, and a resize goes up as the new viewport and
//! the pane area left by the built-in rows.
//! Every request leaves on its own writer thread, so a session slow to take
//! the bytes backs that thread up and never this terminal's input. The stream
//! also carries escapes a pane sent to this terminal itself rather than to the
//! picture — an OSC 52 clipboard write — which the loop writes straight to
//! stdout.
//!
//! The mouse is captured for this terminal, and the frame each paint drew is
//! kept beside the viewer, so a mouse event can be placed against the surfaces
//! that were on the screen when it happened. The viewer decides what each event
//! means at once and writes it at once: nothing mouse-shaped ever waits for the
//! session's answer, so as many rounds are on the wire as the loop decided. One
//! pass of the loop writes one round, so the events that arrived together fold
//! into it and every answer only reconciles what the session did.
//!
//! The keymap, the colors, the pane under the pointer, the tab strip's
//! position, and the sequence being typed belong to this terminal. The session
//! reports the input mode and mouse-select state in events and frames. Each pass
//! compares these values with the state shown on the screen and repaints when
//! one changes.

use std::io;
use std::io::Write;
use std::mem::take;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::crossterm::event::{EnableBracketedPaste, EnableMouseCapture};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{enable_raw_mode, size, EnterAlternateScreen};
use ratatui::crossterm::tty::IsTty;
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport};
use serde_json::value::RawValue;

use crate::input::KeyOutcome;
use crate::mouse::MouseAction;
use crate::{core_pane_area, Client};
use koshi_config::types::BoundAction;
use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, SwitchSessionArgs, VisualCommand,
};
use koshi_core::geometry::{Direction, PaneArea, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId, TabId};
use koshi_core::key::KeySequence;
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseAnswer, MouseInput, MouseKind};
use koshi_core::registry::ActionRegistry;
use koshi_core::resolve::{resolve_action, DispatchPlan};
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::error::IpcError;
use koshi_ipc::event::{IncomingEvent, SessionEvent};
use koshi_ipc::frame::PaintedFrame;
use koshi_ipc::protocol::{
    ConnectionToken, EventFilterSpec, IncomingResponse, IpcRequest, IpcRequestKind, IpcResult,
    WireMouseAction,
};
use koshi_ipc::remote_wire::{RemoteServerFrame, RemoteSessionRow};
use koshi_ipc::router::{RouterRequestKind, RouterResult, SessionAddress, SessionSelector};
use koshi_ipc::transport::{Connection, FrameReader, FrameWriter};
use koshi_ipc::wire::{MaybeKnown, WireName};
use koshi_observability::cleanup::{install_panic_hook, TerminalCleanupGuard};
use koshi_renderer::snapshot::{
    CommittedRegions, CursorStyle, MouseFrame, Reconnecting, RenderSnapshot, ViewerChrome,
};
use koshi_runtime::runtime::event::RuntimeEvent;

use crate::app;
use crate::attach::paint::to_snapshot;
use koshi_core::ids::parse_prefixed_uuid;
use koshi_ipc::endpoint::RESTART_WINDOW;
use koshi_link::discovery::{self, SessionRow};
use koshi_link::error::CliError;
use koshi_link::in_session::InSessionContext;
use koshi_link::ipc_client;
use koshi_link::remote_client::{self, DialError, Reach, ServerArg, REACH_WAIT};
use koshi_link::router_client::router_request;
use koshi_link::talk;

/// Rebuilding the snapshot this terminal paints from the frame the session
/// sent.
pub mod paint;

#[cfg(test)]
mod tests;

/// The size an attaching client reports when the terminal size cannot be read,
/// which is what a `koshi attach` with redirected output finds.
const FALLBACK_VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// The `request_id` the first request the loop sends carries. The Hello is 1
/// and the Attach is 2.
const FIRST_LOOP_REQUEST_ID: u64 = 3;

/// How long the wait for a session that is replacing its own process image
/// pauses between reads of that session's endpoint file. It bounds how long the
/// user's terminal sits still after the swap finishes.
const RESTART_POLL: Duration = Duration::from_millis(25);

/// How long the wait for a session on a server that is replacing its own
/// process image pauses between dials. Each dial runs the whole admission —
/// TLS, the secret, and the scope check — so it is paced wider than the read of
/// a local endpoint file.
const REMOTE_RESTART_POLL: Duration = Duration::from_millis(250);

/// How long the first redial after a remote viewer's link dropped waits before
/// it dials: 1 second.
const FIRST_REDIAL_WAIT: Duration = Duration::from_secs(1);

/// The longest one redial waits before it dials: 8 seconds.
const MAX_REDIAL_WAIT: Duration = Duration::from_secs(8);

/// How long a remote viewer keeps redialing after its link dropped: 120
/// seconds, which is how long a session holds a detached client's view under
/// its resume token.
const REDIAL_WINDOW: Duration = Duration::from_secs(120);

/// The number the first connection of one attachment carries. Coming back after
/// the session replaces its own process image counts up from here.
const FIRST_CONNECTION: u64 = 0;

/// The most decided-but-unwritten mouse actions this client holds, and the most
/// unanswered border moves it remembers. Both cap the memory a session that
/// answers slowly can make this client hold.
///
/// The number is one burst: a 200 ms round trip at 250 mouse events a second is
/// 50 events, and the busiest event — a press that clears a highlight and
/// extends a new one — decides 2 actions, so 100 actions covers that burst.
/// 256 is that burst two and a half times over, so an ordinary slow answer
/// leaves both whole and only a session that stopped answering trims.
const MAX_PENDING_MOUSE: usize = 256;

/// One border move already written and not yet answered.
///
/// A [`MouseAction::Resize`] names the whole distance from the drag anchor, and
/// [`Client::note_resize_applied`] moves that anchor only when an answer comes
/// back, so every move decided before that answer names cells an earlier move
/// already asked for. This entry is what takes them off: it lives from the write
/// until the answer carrying the same `request_id`, `pane` and `side`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SentBorderMove {
    /// The round this move went out in.
    request_id: u64,
    /// The pane whose border the move was asked for.
    pane: PaneId,
    /// Which of the pane's borders the move was asked for.
    side: Direction,
    /// The signed cells the move asked for: `step * count`, so `1` grows the
    /// pane by one cell and `-3` shrinks it by three.
    cells: i32,
}

/// The viewer state a frame paint uses: its chrome, the mode and mouse-select
/// state shown by the frame, and the sequence the hint bar displays.
///
/// [`Screen`] holds the value the frame on the screen was drawn with, and
/// compares it against a fresh read at the end of every loop pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ViewerPaint {
    /// The pane under the pointer and where the tab strip sits, for the tab the
    /// frame on the screen shows.
    pub(crate) chrome: ViewerChrome,
    /// The input mode the hint bar lists bindings for.
    pub(crate) mode: LockMode,
    /// Whether the frame's viewer takes the mouse for text selection.
    pub(crate) mouse_select: bool,
    /// The multi-chord sequence being typed, which the hint bar draws as a
    /// breadcrumb ahead of the chords that continue it. `None` when no sequence
    /// is open.
    pub(crate) pending: Option<KeySequence>,
}

impl ViewerPaint {
    /// Build the viewer state shown by `snapshot` without changing `client`.
    ///
    /// A mode change drops an open sequence when the frame is adopted, so the
    /// sequence is kept only when the frame reports the current mode.
    pub(crate) fn from_frame(client: &Client, snapshot: &RenderSnapshot) -> Self {
        let pending = if client.lock_mode() == snapshot.client.lock_mode {
            client.pending_sequence().cloned()
        } else {
            None
        };
        ViewerPaint {
            chrome: client.chrome(snapshot.client.active_tab),
            mode: snapshot.client.lock_mode,
            mouse_select: snapshot.client.mouse_select,
            pending,
        }
    }

    /// Read what `client` currently contributes to a frame showing `active_tab`.
    ///
    /// `active_tab` is the tab the frame on the screen shows. A tab-strip peek
    /// made on any other tab does not apply.
    fn read(client: &Client, active_tab: TabId) -> Self {
        ViewerPaint {
            chrome: client.chrome(active_tab),
            mode: client.lock_mode(),
            mouse_select: client.mouse_select(),
            pending: client.pending_sequence().cloned(),
        }
    }
}

/// This terminal's screen: the frame drawn on it, the committed region solve,
/// and what the viewer contributed to that frame.
///
/// Every draw goes through here, so one place decides whether the screen is out
/// of date. [`draw`](Self::draw) puts a frame the session sent on the screen and
/// returns the mouse view for that paint. [`refresh`](Self::refresh) ends every
/// loop pass and draws the frame already there again when the viewer has moved
/// under it — which is what shows a change no frame reports, such as an opened
/// key sequence.
struct Screen<B: Backend> {
    /// The ratatui terminal the renderer paints into.
    terminal: Terminal<B>,
    /// The window title committed with the last successful paint.
    last_title: String,
    /// The cursor style committed with the last successful paint.
    last_cursor: Option<CursorStyle>,
    /// The snapshot last drawn, kept so a viewer-only change can draw it
    /// again without re-reading the frame. Its grids travel behind `Arc`s, so
    /// keeping and cloning it moves no cell data. `None` until the first
    /// draw.
    last_snapshot: Option<RenderSnapshot>,
    /// The region solve and input revision committed with the frame on the
    /// screen. It starts with the compiled-in two-row solve.
    committed_regions: CommittedRegions,
    /// What the viewer contributed to the frame on the screen. `None` until the
    /// first draw.
    shown: Option<ViewerPaint>,
}

impl<B: Backend> Screen<B> {
    /// A screen that has drawn nothing yet.
    fn new(terminal: Terminal<B>, viewport: Size) -> Self {
        Screen {
            terminal,
            last_title: String::new(),
            last_cursor: None,
            last_snapshot: None,
            committed_regions: CommittedRegions::core(viewport, 0),
            shown: None,
        }
    }

    /// Draw one frame the session sent, and hand back the frame a mouse event
    /// is placed against. Returns `None` when the terminal rejects the paint.
    ///
    /// It paints from the incoming frame state, then adopts that state only
    /// after the terminal accepts the paint. A locked frame therefore draws the
    /// locked hint bar without changing the viewer when the paint fails.
    ///
    /// The returned [`MouseFrame`] holds the committed region solve, where the
    /// surfaces sit, and what the cells under them were. That is what the next
    /// mouse event is answered from.
    fn draw(&mut self, client: &mut Client, frame: Box<PaintedFrame>) -> Option<MouseFrame> {
        let snapshot = to_snapshot(&frame);
        let committed_regions = self.regions_for(snapshot.client.viewport);
        let frame_paint = ViewerPaint::from_frame(client, &snapshot);
        if !paint(
            &mut self.terminal,
            client,
            &snapshot,
            &committed_regions,
            &frame_paint,
            &mut self.last_title,
            &mut self.last_cursor,
        ) {
            return None;
        }
        adopt_frame(client, &snapshot);
        self.committed_regions = committed_regions.clone();
        self.shown = Some(frame_paint);
        self.last_snapshot = Some(snapshot.clone());
        Some(MouseFrame::with_regions(snapshot, committed_regions))
    }

    /// Draw the frame already on the screen again when the viewer has moved
    /// under it: a new hovered pane, a scrolled tab strip, another input mode,
    /// or a key sequence opened or closed.
    ///
    /// `active_tab` is the tab that frame shows, and `None` before any frame has
    /// been drawn. A viewer that has not moved is left alone, so an idle pass
    /// draws nothing. A resize waits for the next session frame, so the painted
    /// frame and its committed region solve stay paired.
    fn refresh(&mut self, client: &Client, active_tab: Option<TabId>) {
        let Some(active_tab) = active_tab else {
            return;
        };
        if client.viewport() != self.committed_regions.viewport {
            return;
        }
        let current = ViewerPaint::read(client, active_tab);
        if self.shown.as_ref() == Some(&current) {
            return;
        }
        let Some(snapshot) = self.last_snapshot.clone() else {
            return;
        };
        if paint(
            &mut self.terminal,
            client,
            &snapshot,
            &self.committed_regions,
            &current,
            &mut self.last_title,
            &mut self.last_cursor,
        ) {
            self.shown = Some(current);
        }
    }

    /// Select the compiled-in region solve for a painted frame's viewport.
    ///
    /// A changed frame viewport is a new region input. The revision increases
    /// only when that input changes, so another frame with the same viewport
    /// keeps the same revision.
    fn regions_for(&self, viewport: Size) -> CommittedRegions {
        let input_revision = if viewport == self.committed_regions.viewport {
            self.committed_regions.input_revision
        } else {
            self.committed_regions.input_revision.saturating_add(1)
        };
        CommittedRegions::core(viewport, input_revision)
    }
}

/// Paint one snapshot with its committed region solve and report whether the
/// terminal accepted the frame.
fn paint<B: Backend>(
    terminal: &mut Terminal<B>,
    client: &Client,
    snapshot: &RenderSnapshot,
    committed_regions: &CommittedRegions,
    frame_paint: &ViewerPaint,
    last_title: &mut String,
    last_cursor: &mut Option<CursorStyle>,
) -> bool {
    match app::paint_frame(
        terminal,
        client,
        snapshot,
        committed_regions,
        frame_paint,
        last_title,
        last_cursor,
    ) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, "could not paint the frame");
            false
        }
    }
}

/// How an attached client's event stream ended.
#[derive(Debug)]
enum Ending {
    /// The server detached this client. The session keeps running.
    Detached,
    /// The session shut down and said so before closing.
    SessionEnded,
    /// The connection broke: the session server is gone.
    Died,
    /// This terminal went away while the session kept running.
    TerminalGone,
    /// The session moved this client to the session named here.
    Switch(SessionId),
    /// The session is replacing its own process image. The loop waits for the
    /// session's new socket and attaches again on it. A loop that cannot ends
    /// here and reports the same death a broken connection reports.
    Restarting,
    /// A remote viewer's link broke and [`redial`] gave up, carrying the cause it
    /// gave up on. The session keeps running without this viewer.
    LinkLost(Box<CliError>),
}

/// Two endings are equal when they are the same variant carrying the same
/// fields. A [`Ending::Switch`] compares its [`SessionId`], and a
/// [`Ending::LinkLost`] compares the text its cause prints, which is what the
/// viewer shows.
impl PartialEq for Ending {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Ending::Detached, Ending::Detached)
            | (Ending::SessionEnded, Ending::SessionEnded)
            | (Ending::Died, Ending::Died)
            | (Ending::TerminalGone, Ending::TerminalGone)
            | (Ending::Restarting, Ending::Restarting) => true,
            (Ending::Switch(left), Ending::Switch(right)) => left == right,
            (Ending::LinkLost(left), Ending::LinkLost(right)) => {
                left.to_string() == right.to_string()
            }
            _ => false,
        }
    }
}

/// One thing the loop reacts to: a frame read off the session's event stream,
/// or an event read from this terminal.
///
/// Both arrive on one channel, so one blocking read serves the session and the
/// keyboard at once.
enum Incoming {
    /// A frame the session wrote, or the read that failed.
    Frame {
        /// Which connection the reader that sent this was reading. A client
        /// that came back after the session replaced its own process image
        /// reads a later connection: connection 0 is the first, 1 the one after
        /// the first restart, and so on.
        connection: u64,
        /// The frame itself, or the read that failed.
        frame: Result<SessionEvent, IpcError>,
    },
    /// A key, a resize, or this terminal hanging up. Boxed to keep this type
    /// close to the size of a frame.
    Input(Box<RuntimeEvent>),
}

/// This client's side of the connection: the queue the writer thread takes
/// requests from, the action table a fired binding is resolved against, and the
/// number the next request carries.
struct Uplink {
    /// The queue [`spawn_uplink_writer`]'s thread writes from. Every request
    /// the loop sends goes here.
    requests: mpsc::Sender<IpcRequest>,
    /// The action table a fired binding is turned into commands with. The
    /// session owns its own table and runs those commands; this one only names
    /// what they are.
    registry: ActionRegistry,
    /// The `request_id` the next request carries.
    next_request_id: u64,
}

impl Uplink {
    /// Queue one request for the writer thread, numbering it, and give back the
    /// `request_id` it carried. The queue takes it whatever the socket is
    /// doing, so this never waits.
    ///
    /// A request over the frame cap is dropped by the writer and never
    /// answered. A queue nobody takes from is a writer thread that has ended,
    /// which only a broken connection does; the reading half meets that same
    /// connection and ends the loop.
    fn send(&mut self, kind: IpcRequestKind) -> u64 {
        let request_id = self.next_request_id;
        let request = IpcRequest { request_id, kind };
        self.next_request_id += 1;
        let _ = self.requests.send(request);
        request_id
    }

    /// Resolve one fired binding against the action table and send every
    /// command it runs, in order.
    ///
    /// `new_pane_direction` is this viewer's own setting, so a pane-opening
    /// binding that names no direction already says where the pane goes by the
    /// time the command leaves. An action the table refuses, and one the plugin
    /// host owns, send nothing.
    fn submit(&mut self, client: &Client, bound: BoundAction) {
        let direction = client.config().layout.new_pane_direction;
        let Ok(plan) = resolve_action(&bound.action, &bound.args, &self.registry, direction) else {
            return;
        };
        for command in commands(plan) {
            let envelope = CommandEnvelope::new(
                CommandId::new(),
                CommandSource::key_binding(client.id()),
                SystemTime::now(),
                command,
            );
            self.send(IpcRequestKind::SubmitCommand(Box::new(envelope)));
        }
    }
}

/// Where the session this client joins runs.
///
/// One home is picked before the first connection opens and holds for every
/// connection after it, so a switch and a restart re-enter the session the same
/// way the first join entered it.
enum Home {
    /// A session on this machine, joined through the endpoint file it
    /// advertises under `runtime_dir`.
    Local {
        /// This user's runtime directory, holding one endpoint file per
        /// session.
        runtime_dir: PathBuf,
    },
    /// A session on another machine, joined through the server serving it.
    Remote {
        /// The saved record every dial presents: the address, the secret, and
        /// the certificate fingerprint pinned on the first connection.
        server: ServerArg,
    },
}

/// One open connection into a session, past the join.
struct Joined {
    /// The frames the session sends.
    reader: FrameReader,
    /// The frames this client sends.
    writer: FrameWriter,
    /// The client the server minted for this terminal.
    client_id: ClientId,
    /// The session the server says that client joined.
    session_id: SessionId,
    /// The token this connection was opened under: on this machine the
    /// session's endpoint token, which changes every time that session binds a
    /// new socket, and on a server the secret that server admitted.
    token: ConnectionToken,
    /// The secret this attach minted, presented on the next attach to get this
    /// attach's view back. `None` from a session server that mints none.
    resume_token: Option<ConnectionToken>,
    /// Whether the session echoed the pane-area field in its attach result.
    /// `false` records the fixed two-row compatibility mode for this viewer.
    pane_area_supported: bool,
}

/// Resolve what the user typed to one running session, and report where it
/// listens.
///
/// `selector` is a `session-<uuid>` id, a bare UUID, or a session display
/// name. `None` picks one from the sessions running for this user instead:
/// nothing running is a failure, one session is taken straight away, and more
/// than one is printed as a numbered list to answer on stdin.
fn resolve_session(runtime_dir: &Path, selector: Option<&str>) -> Result<SessionAddress, CliError> {
    let selector = match selector {
        Some(selector) => selector.to_string(),
        // No remote rows are offered, so every place is a local one.
        None => match choose(runtime_dir, Vec::new())? {
            Picked::Local(id) => id,
            Picked::Remote(at) => {
                unreachable!("a listing offered no remote rows and settled on place {at}")
            }
        },
    };
    lookup(runtime_dir, &selector)
}

/// Join a running session in this terminal as a new client.
///
/// `selector` is a `session-<uuid>` id, a bare UUID, or a session display
/// name. `None` picks one from the sessions running for this user and the
/// sessions on every saved server that answered: no session running anywhere
/// is a failure, exactly one session on this machine is taken straight away,
/// and anything else — several sessions, or one session on a saved server —
/// is printed as a numbered list to answer on stdin.
pub fn run(selector: Option<&str>) -> Result<(), CliError> {
    let runtime_dir = ipc_client::runtime_dir()?;
    let Some(selector) = selector else {
        return attach_picked(runtime_dir);
    };
    let address = lookup(&runtime_dir, selector)?;
    attach_home(
        &Home::Local { runtime_dir },
        SessionSelector::Id(address.id),
    )
}

/// Join a session on the machine `server` names in this terminal.
///
/// `server` is either the name this machine saved that server under or the
/// `host:port` it listens on. `save_as` is the name to save a server reached
/// for the first time under, so later commands name it instead of its address.
///
/// `selector` is a `session-<uuid>` id, a bare UUID, or a session display name,
/// and the server resolves it against the sessions this machine's secret
/// reaches. `None` lists those sessions instead: one is taken straight away,
/// and more than one is printed as a numbered list to answer on stdin. Nothing
/// here creates a session.
///
/// # Errors
/// [`CliError::Runtime`] when the server does not admit the secret, when the
/// certificate it presents is not the pinned one, and when nothing this secret
/// reaches is running on it.
pub fn run_remote(
    server: &str,
    save_as: Option<&str>,
    selector: Option<&str>,
) -> Result<(), CliError> {
    let arg = remote_client::resolve_server(server)?;
    // This connection carries one listing. The attachment below dials its own.
    let (mut link, saved) =
        remote_client::connect_saved(&arg, save_as, Some(remote_client::REPLY_WAIT))?;
    let target = match selector {
        Some(selector) => selector_of(selector),
        None => choose_remote(server, &remote_client::list_remote_sessions(&mut link)?)?,
    };
    // The attachment dials its own connection, so this one is finished with.
    drop(link);
    attach_home(
        &Home::Remote {
            server: ServerArg::Saved(saved),
        },
        target,
    )
}

/// Join the session a bare `koshi attach` picks, from this user's own sessions
/// and the sessions on every saved server that answered.
///
/// A saved server that answered and did not admit its secret prints one line
/// on stderr naming the command that replaces that secret. A server not heard
/// from inside [`REACH_WAIT`] prints one stderr line and is left off the list.
fn attach_picked(runtime_dir: PathBuf) -> Result<(), CliError> {
    let reached = reachable_rows();
    let offered = reached
        .iter()
        .map(|(server, row)| SessionRow::new(row.id, &row.name, Some(server.clone())))
        .collect();
    let (server, row) = match choose(&runtime_dir, offered)? {
        Picked::Local(id) => {
            let address = lookup(&runtime_dir, &id)?;
            return attach_home(
                &Home::Local { runtime_dir },
                SessionSelector::Id(address.id),
            );
        }
        Picked::Remote(at) => &reached[at],
    };
    attach_home(
        &Home::Remote {
            server: remote_client::resolve_server(server)?,
        },
        SessionSelector::Id(row.id),
    )
}

/// The sessions on every saved server that answered inside [`REACH_WAIT`], each
/// beside the name of the server serving it.
///
/// A refused secret prints one stderr line naming the command that replaces
/// it. A server not heard from, and a server pinning no certificate yet, each
/// print one stderr line and contribute no rows.
fn reachable_rows() -> Vec<(String, RemoteSessionRow)> {
    let mut offered = Vec::new();
    for reach in remote_client::reach_all(REACH_WAIT) {
        match reach {
            Reach::Reached { server, rows } => {
                offered.extend(rows.into_iter().map(|row| (server.clone(), row)));
            }
            Reach::Refused { server } => eprintln!(
                "{server}: the saved secret was refused; \
                 run `koshi remote set-secret {server}`"
            ),
            Reach::Unreachable { server } => {
                eprintln!("koshi: {server} did not answer; its sessions are not listed");
            }
            Reach::Unchecked { server } => eprintln!(
                "koshi: {server} has no pinned certificate yet; \
                 run `koshi attach --remote {server}` to connect and pin it"
            ),
        }
    }
    offered
}

/// The session a `koshi attach --remote <server>` with no session named joins,
/// picked from the sessions that server's secret reaches.
///
/// One row is the answer on its own; more than one is printed and the number
/// typed on stdin picks the row. This runs before the terminal enters raw mode,
/// so the prompt is a plain stdin read.
///
/// # Errors
/// [`CliError::Runtime`] when the secret reaches no running session on that
/// server.
fn choose_remote(server: &str, rows: &[RemoteSessionRow]) -> Result<SessionSelector, CliError> {
    if rows.is_empty() {
        return Err(CliError::Runtime {
            detail: format!("no session is reachable on {server}"),
        });
    }
    let listed: Vec<SessionRow> = rows
        .iter()
        .map(|row| SessionRow::new(row.id, &row.name, Some(server.to_string())))
        .collect();
    let at = settle_on(&listed)?;
    Ok(SessionSelector::Id(listed[at].id))
}

/// Ask the session this CLI runs inside to move its own client to another
/// session.
///
/// `selector` names the session to move to: a `session-<uuid>` id, a bare
/// UUID, or a session display name, and `None` picks one from the sessions
/// running for this user. A session on another machine is never offered, since
/// this session cannot move a client into one. The session moves the client
/// this terminal already holds.
pub fn switch_in_session(
    context: &InSessionContext,
    selector: Option<&str>,
) -> Result<CommandResult, CliError> {
    let runtime_dir = ipc_client::runtime_dir()?;
    let address = resolve_session(&runtime_dir, selector)?;
    ipc_client::submit_in_session(
        context,
        Command::SwitchSession(SwitchSessionArgs {
            client: None,
            session: address.id,
        }),
    )
}

/// Join the session `session_id` names and run until this client detaches, the
/// session ends, the connection breaks, or the session moves this client to
/// another session, in which case this attaches there and keeps running.
///
/// Paints every frame the session composes for this terminal and sends this
/// terminal's keys, mouse and resizes back. A broken connection reports the
/// cause and how to reattach, and exits non-zero; the other endings print what
/// happened and exit zero.
pub(crate) fn attach_session(runtime_dir: &Path, session_id: SessionId) -> Result<(), CliError> {
    attach_home(
        &Home::Local {
            runtime_dir: runtime_dir.to_path_buf(),
        },
        SessionSelector::Id(session_id),
    )
}

/// Join the session `target` names in `home` and run until nothing moves this
/// client on.
///
/// A session that moves this client to another one is attached to next, in the
/// same home: a client on a server dials that server again for it, so the
/// certificate, the secret and the scope are all checked again before the next
/// session paints anything.
fn attach_home(home: &Home, target: SessionSelector) -> Result<(), CliError> {
    let mut target = target;
    while let Some(next) = attach_once(home, &target)? {
        target = SessionSelector::Id(next);
    }
    Ok(())
}

/// Join the session `target` names in `home` and run one attachment of it,
/// handing back the session to attach to next when this one moved the client
/// on.
///
/// The terminal enters raw mode and the alternate screen behind a cleanup
/// guard this call owns, and leaves both before it returns, so the terminal is
/// restored between one session and the next.
///
/// A session that replaces its own process image is handled inside this one
/// attachment: the client comes back as the same client — on the session's new
/// socket on this machine, and through a fresh dial of the server otherwise —
/// and the terminal keeps every mode it is in, so nothing on the screen
/// flickers.
fn attach_once(home: &Home, target: &SessionSelector) -> Result<Option<SessionId>, CliError> {
    let Joined {
        reader,
        writer,
        client_id,
        session_id,
        token,
        resume_token,
        pane_area_supported,
    } = dial(home, target)?;

    // The session accepted the client, so the terminal may change mode now.
    // The hooks undo every mode this function sets, and the panic hook shares
    // them, so an unwinding panic restores the terminal too and then writes a
    // crash report into the data directory.
    let cleanup = TerminalCleanupGuard::new();
    app::register_terminal_restore(&cleanup);
    let _panic_guard = install_panic_hook(&cleanup, koshi_paths::data_dir());
    // A terminal that refuses any of these modes still streams: the failure is
    // logged and the loop runs on.
    let _ =
        enable_raw_mode().inspect_err(|error| tracing::warn!(%error, "could not enter raw mode"));
    let _ = execute!(io::stdout(), EnterAlternateScreen)
        .inspect_err(|error| tracing::warn!(%error, "could not enter the alternate screen"));
    // Capture mouse events so koshi can hit-test clicks (tabs, panes, scroll).
    // This is terminal-global: while on, programs inside panes and native text
    // selection do not see the mouse until koshi forwards it.
    let _ = execute!(io::stdout(), EnableMouseCapture)
        .inspect_err(|error| tracing::warn!(%error, "could not capture the mouse"));
    // Ask the outer terminal to bracket a paste, so the clipboard arrives as
    // one block instead of a burst of keys and no character of it can fire a
    // keybinding.
    let _ = execute!(io::stdout(), EnableBracketedPaste)
        .inspect_err(|error| tracing::warn!(%error, "could not enable bracketed paste"));
    // The ratatui terminal owns the output side; the renderer paints its
    // buffer. A terminal that reports no size — which is what a `koshi
    // attach` with redirected output finds — gets a buffer of
    // [`FALLBACK_VIEWPORT`], the size this client told the session it has.
    let terminal = Terminal::new(CrosstermBackend::new(io::stdout())).unwrap_or_else(|error| {
        tracing::warn!(%error, "could not size the output terminal");
        Terminal::with_options(
            CrosstermBackend::new(io::stdout()),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(
                    0,
                    0,
                    FALLBACK_VIEWPORT.cols,
                    FALLBACK_VIEWPORT.rows,
                )),
            },
        )
        .expect("a fixed viewport reads no terminal size")
    });

    // One channel, three producers: the reading half of every connection this
    // attachment holds in turn, this terminal's input thread, and the loop
    // itself, which keeps a sender to start the reader it comes back on. A
    // broken connection reaches the loop as the failed read its own reader
    // writes here, so the loop ends on that frame, not on the channel closing.
    let (incoming_tx, incoming_rx) = mpsc::channel();
    spawn_frame_reader(reader, FIRST_CONNECTION, incoming_tx.clone());

    // Standard input that is not a terminal has no keys to read, which is what
    // `koshi attach` started with its input redirected has. It runs as a viewer
    // that types nothing, and no input thread is started for it. Every read
    // failure a started thread reports is therefore this terminal going away.
    if io::stdin().is_tty() {
        let (input_tx, input_rx) = mpsc::channel();
        app::spawn_input_thread(input_tx, client_id);
        spawn_input_relay(input_rx, incoming_tx.clone());
    } else {
        tracing::info!("standard input is not a terminal, so this client reads no keys");
    }

    // The viewer half: this terminal's own keymap, colors and hint bar, read
    // from this user's config files. Its frames arrive over the connection
    // rather than over a session subscription, so the receiver it holds has no
    // sender. It also holds the cleanup guard, since the outer terminal that
    // guard restores is this viewer's.
    // `load` collects its warnings instead of logging them, so they are
    // replayed here.
    let (loaded, config_warnings) = koshi_link::config::load();
    for warning in &config_warnings {
        tracing::warn!("{warning}");
    }
    let (_events_tx, events_rx) = mpsc::channel();
    let mut client = app::viewer(
        client_id,
        viewport(),
        events_rx,
        cleanup,
        pane_area_supported,
        loaded,
    );
    let mut uplink = Uplink {
        requests: spawn_uplink_writer(writer),
        registry: ActionRegistry::new(),
        next_request_id: FIRST_LOOP_REQUEST_ID,
    };
    let mut screen = Screen::new(terminal, client.viewport());

    let ending = run_attachment(
        home,
        session_id,
        client_id,
        token,
        resume_token,
        &mut client,
        &mut screen,
        &mut uplink,
        incoming_tx,
        incoming_rx,
    );

    // Restore the terminal before anything is printed, so the message lands on
    // the shell's own screen rather than the alternate one, and nothing follows
    // it. Dropping the screen drops the ratatui terminal it holds, which shows
    // the cursor a painted frame hid, and that cursor belongs on the alternate
    // screen; dropping the client then runs the cleanup guard it holds, which
    // leaves that screen.
    drop(screen);
    drop(client);
    report(home, ending, session_id)
}

/// Run one attachment: paint every frame the session sends, send this
/// terminal's keys, mouse and resizes back, and report how the stream ended.
///
/// One loop serves both homes. Everything transport-shaped is already settled
/// by the time it starts: the connection arrives as the two halves behind
/// `uplink` and `incoming_rx`, and a session replacing its own process image
/// comes back through [`come_back`], which re-enters `home` the way that home
/// is entered.
///
/// A remote viewer whose link breaks comes back through [`redial`], which is
/// handed this loop's `client` and `screen`: it paints the
/// `RECONNECTING (attempt 1, retry in 1s)` tag once a second, dials the server
/// again on a widening pause, and drops everything typed while it had no link. A
/// dial it gives up on ends the loop as [`Ending::LinkLost`] carrying the cause.
/// A local viewer whose link breaks ends the loop, as it always has.
///
/// `token` is the token the open connection was opened under, which
/// [`come_back`] reads and stamps with the token of the connection this client
/// comes back on.
///
/// `client_id` is the client the open connection joined as. A redial that a
/// session answers with a fresh client replaces it, and the viewer's own id
/// with it.
///
/// `resume_token` is the secret the open connection's attach minted, presented
/// on the next redial to get that attach's view back. Every reconnection
/// stamps it with the secret its own attach minted.
#[allow(clippy::too_many_arguments)]
fn run_attachment<B: Backend>(
    home: &Home,
    session_id: SessionId,
    mut client_id: ClientId,
    mut token: ConnectionToken,
    mut resume_token: Option<ConnectionToken>,
    client: &mut Client,
    screen: &mut Screen<B>,
    uplink: &mut Uplink,
    incoming_tx: mpsc::Sender<Incoming>,
    incoming_rx: mpsc::Receiver<Incoming>,
) -> Ending {
    // Which connection the loop is reading. Coming back after the session
    // replaces its own process image counts up, and the loop drops every frame
    // that does not carry this number.
    let mut current_connection: u64 = FIRST_CONNECTION;
    let mut last_frame: Option<MouseFrame> = None;
    // What the viewer has decided and not yet written. The pass that decided it
    // ends by writing all of it, so this holds one pass's worth: the events that
    // arrived together. It is bounded by [`MAX_PENDING_MOUSE`], which every path
    // that adds to it goes through [`hold`] to keep.
    let mut pending: Vec<MouseAction> = Vec::new();
    // The border moves written and not yet answered, newest last. A move names
    // the whole distance from the drag anchor and the anchor advances only on an
    // answer, so these are the cells the next move for the same border must not
    // ask for a second time.
    let mut sent: Vec<SentBorderMove> = Vec::new();

    loop {
        let now = Instant::now();
        let received = match earliest(client.next_key_wakeup(now), client.next_mouse_wakeup(now)) {
            Some(timeout) => match incoming_rx.recv_timeout(timeout) {
                Ok(received) => Some(received),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break Ending::Died,
            },
            None => match incoming_rx.recv() {
                Ok(received) => Some(received),
                Err(_) => break Ending::Died,
            },
        };
        // Everything else already queued is taken in the same pass, so the loop
        // stays level with its channel. The drain bounds nothing on its own: an
        // iteration costs well under a millisecond, so a batch normally holds
        // the single event that woke it. A batch carrying several mouse events
        // is the only thing that makes a pile.
        let mut batch: Vec<Incoming> = received.into_iter().collect();
        while let Ok(received) = incoming_rx.try_recv() {
            batch.push(received);
        }

        let mut ended = None;
        for received in batch {
            match received {
                // A frame read from a connection this client has already left:
                // the reader of the connection before a restart ends by
                // reporting that socket closing.
                Incoming::Frame { connection, .. } if connection != current_connection => {}
                Incoming::Frame { frame, .. } => {
                    if let Some(ending) = classify(&frame) {
                        ended = Some(ending);
                        break;
                    }
                    match frame {
                        Ok(SessionEvent::Painted { frame }) => {
                            if let Some(mouse_frame) = screen.draw(client, frame) {
                                last_frame = Some(mouse_frame);
                            }
                        }
                        Ok(SessionEvent::MouseAnswer {
                            request_id,
                            answers,
                        }) => {
                            if let Some(frame) = last_frame.as_ref() {
                                apply_answer(
                                    client,
                                    frame,
                                    &mut sent,
                                    request_id,
                                    answers,
                                    &mut pending,
                                );
                            }
                        }
                        // The stream dropped events, so the answers to the
                        // border moves now on the wire may be among them. A
                        // remembered move whose answer never lands would take
                        // its cells off every later move for that border for
                        // good, so a resync forgets them all and the next move
                        // asks for its whole distance from the drag anchor.
                        // Nothing is released by this: no round ever waited.
                        Ok(SessionEvent::Resync { .. }) => sent.clear(),
                        // Bytes a pane aimed at this terminal, such as an OSC 52
                        // clipboard write, go to it verbatim.
                        Ok(SessionEvent::HostWrite { bytes }) => {
                            let mut out = io::stdout();
                            let _ = out.write_all(&bytes);
                            let _ = out.flush();
                        }
                        // Every other frame reports a change to the session's
                        // structure, and the next painted frame carries that
                        // change whole, so only a painted frame is drawn.
                        _ => {}
                    }
                }
                // An input thread runs only for a terminal that had keys to
                // read, so its hangup is this terminal going away while the
                // session runs on.
                Incoming::Input(event) if matches!(*event, RuntimeEvent::Quit) => {
                    ended = Some(Ending::TerminalGone);
                    break;
                }
                Incoming::Input(event) => match *event {
                    // A mouse event belongs to the viewer: the frame it painted
                    // says which pane the pointer is over, what that pane's
                    // program asked for, and which gesture is under way. Before
                    // the first paint there is no frame to place it against.
                    RuntimeEvent::MouseInput { mouse, .. } => {
                        if let Some(frame) = last_frame.as_ref() {
                            handle_mouse_event(client, frame, mouse, &mut pending);
                        }
                    }
                    event => handle_input(client, uplink, event),
                },
            }
        }
        if let Some(ending) = ended {
            let halves = match ending {
                Ending::Restarting => {
                    // The session is replacing its own process image. The
                    // terminal keeps every mode it is in and the screen is left
                    // alone; the session's first frame on the new connection
                    // paints the panes again.
                    //
                    // The last request on this connection. The queue is written
                    // in order, so it leaves behind every key already on it.
                    uplink.send(IpcRequestKind::Leaving);
                    come_back(home, session_id, client_id, &mut token, &mut resume_token)
                }
                // The link broke. A viewer of a session on a server, with
                // `remote-reconnect` on, dials that server again while the
                // tabline reads `RECONNECTING (attempt 1, retry in 1s)`;
                // nothing typed over that stretch is sent. A dial this client
                // gave up on ends as [`Ending::LinkLost`] carrying its cause. A
                // viewer with `remote-reconnect` off, and a viewer of a session
                // on this machine, end here.
                Ending::Died => match home {
                    Home::Remote { server } if client.config().remote_reconnect => {
                        match redial(
                            server,
                            session_id,
                            resume_token.as_ref(),
                            client,
                            screen,
                            last_frame.as_ref().map(|frame| frame.client.active_tab),
                        ) {
                            Ok(joined) => {
                                client_id = joined.client_id;
                                client.set_id(joined.client_id);
                                resume_token = joined.resume_token;
                                client.end_mouse_gestures();
                                pending.clear();
                                // The panes the old frame placed may be gone.
                                // Mouse events wait for the new connection's
                                // first frame to be placed against.
                                last_frame = None;
                                if drop_input_from_the_blackout(&incoming_rx) {
                                    break Ending::TerminalGone;
                                }
                                Some((joined.reader, joined.writer, joined.pane_area_supported))
                            }
                            Err(cause) => break Ending::LinkLost(cause),
                        }
                    }
                    Home::Remote { .. } | Home::Local { .. } => None,
                },
                Ending::Detached
                | Ending::SessionEnded
                | Ending::TerminalGone
                | Ending::Switch(_)
                | Ending::LinkLost(_) => None,
            };
            let Some((reader, writer, pane_area_supported)) = halves else {
                break ending;
            };
            client.set_pane_area_supported(pane_area_supported);
            current_connection += 1;
            spawn_frame_reader(reader, current_connection, incoming_tx.clone());
            // Dropping the queue the old connection's writer thread reads from
            // is what ends that thread.
            uplink.requests = spawn_uplink_writer(writer);
            uplink.next_request_id = FIRST_LOOP_REQUEST_ID;
            report_terminal_size(client, uplink);
            // The new connection numbers its rounds from the start, so no
            // answer to a border move written on the old one can arrive. The
            // next move asks for its whole distance from the drag anchor.
            sent.clear();
            continue;
        }
        fire_expired_key_sequence(client, uplink, Instant::now());
        // A selection drag held past a pane's edge keeps scrolling while the
        // pointer sits still, so the clock drives it. Asking on every iteration
        // is what re-arms the timer at each firing.
        if let Some(frame) = last_frame.as_ref() {
            hold(
                &mut pending,
                client.expire_mouse_scroll(Instant::now(), frame),
            );
        }
        // Every pass ends here, whether or not it drew a frame: the events it
        // handled may have moved the viewer after that frame was drawn.
        screen.refresh(
            client,
            last_frame.as_ref().map(|frame| frame.client.active_tab),
        );
        flush_round(uplink, &mut sent, &mut pending);
    }
}

/// Open one connection into the session `target` names in `home` and join it as
/// a client.
///
/// On this machine the session's endpoint file names the socket and holds the
/// token the Hello presents, and a display name is resolved by the router
/// first. On a server the whole admission runs — TLS with the pinned
/// certificate, the secret, and the scope check on the session asked for — and
/// the server resolves the name against the sessions that secret reaches.
fn dial(home: &Home, target: &SessionSelector) -> Result<Joined, CliError> {
    match home {
        Home::Local { runtime_dir } => {
            // The router turns a display name into a session's address before
            // this terminal joins it, and every dial after the first names the
            // session by id.
            let session_id = match target {
                SessionSelector::Id(session_id) => *session_id,
                SessionSelector::Name(name) => lookup(runtime_dir, name)?.id,
            };
            let endpoint = ipc_client::read_endpoint(runtime_dir, session_id)?;
            let mut connection = ipc_client::connect(&endpoint, session_id)?;
            let (client_id, session_id, resume_token, pane_area_supported) =
                join(&mut connection, &endpoint.token, None)?;
            let (reader, writer) = connection.split();
            Ok(Joined {
                reader,
                writer,
                client_id,
                session_id,
                token: endpoint.token,
                resume_token,
                pane_area_supported,
            })
        }
        Home::Remote { server } => dial_remote(server, target, None, None).map_err(CliError::from),
    }
}

/// Dial `server`, ask it for the session `target` names, and join that session
/// as a client.
///
/// The serving machine presents that session's endpoint token and writes the
/// Hello on this client's behalf, so the first frame read back is the session
/// server's answer to that Hello. The Attach after it is this client's own,
/// `resume` names the client record to come back as, and `resume_token` is the
/// secret the last attach minted, presented to get that attach's view back.
///
/// # Errors
/// [`DialError::Unreachable`] when the path to the server failed: the
/// connection could not be opened, or a frame of the join could not be written
/// or read. [`DialError::Refused`] when the server answered and every identical
/// dial after it gets the same answer: the certificate it presents is not the
/// pinned one, it does not admit the secret, the admitted secret does not reach
/// `target`, the protocol versions do not overlap, or its answer is a frame this
/// attach cannot read.
fn dial_remote(
    server: &ServerArg,
    target: &SessionSelector,
    resume: Option<ClientId>,
    resume_token: Option<&ConnectionToken>,
) -> Result<Joined, DialError> {
    // The join is held to JOIN_WAIT; the clock comes off once it is joined.
    let (link, saved) = remote_client::connect_saved(server, None, Some(remote_client::JOIN_WAIT))?;
    let (mut reader, mut writer) =
        remote_client::attach_remote(link, target.clone()).map_err(DialError::Unreachable)?;
    settle_forwarded_hello(&mut reader, target)?;
    writer
        .send(&attach_request(resume, resume_token))
        .map_err(link_failed)?;
    let reply = reader.recv().map_err(link_failed)?;
    let (client_id, session_id, minted, pane_area) =
        take_attached(reply).map_err(DialError::Refused)?;

    // Joined: both halves block for as long as it takes from here.
    reader.set_deadline(None);
    writer.set_deadline(None);
    Ok(Joined {
        reader,
        writer,
        client_id,
        session_id,
        token: saved.secret,
        resume_token: minted,
        pane_area_supported: pane_area.is_some(),
    })
}

/// Read the answer to the Hello the serving machine wrote on this client's
/// behalf, and settle the protocol version from it.
///
/// Two senders write this one frame: the serving machine writes a refusal when
/// the secret it admitted does not reach `target`, and otherwise the session
/// server's own answer arrives unread through the bridge. The frame is held as
/// its JSON text and decoded as a refusal first, then as an
/// [`IncomingResponse`].
///
/// # Errors
/// [`DialError::Unreachable`] when the frame could not be read at all.
/// [`DialError::Refused`] for every answer that did arrive and does not join:
/// the serving machine's refusal, an answer this attach cannot read, and a
/// protocol version this build does not accept.
fn settle_forwarded_hello(
    reader: &mut FrameReader,
    target: &SessionSelector,
) -> Result<(), DialError> {
    let frame: Box<RawValue> = reader.recv().map_err(link_failed)?;
    if let Ok(RemoteServerFrame::Refused { .. }) = serde_json::from_str(frame.get()) {
        return Err(DialError::Refused(CliError::Runtime {
            detail: format!(
                "the token this server saved does not reach session {}",
                target_name(target)
            ),
        }));
    }
    let reply: IncomingResponse = serde_json::from_str(frame.get()).map_err(|error| {
        DialError::Refused(CliError::IpcUnavailable {
            detail: format!("the server answered with a frame this attach cannot read: {error}"),
        })
    })?;
    settle_version(reply).map_err(DialError::Refused)
}

/// The [`DialError::Unreachable`] a failed read or write on the open link maps
/// to, carrying [`talk::talk_failed`]'s message.
fn link_failed(error: IpcError) -> DialError {
    DialError::Unreachable(talk::talk_failed(error))
}

/// How a selector reads in a message: the id itself, or the display name.
fn target_name(target: &SessionSelector) -> String {
    match target {
        SessionSelector::Id(session_id) => session_id.to_string(),
        SessionSelector::Name(name) => name.clone(),
    }
}

/// Come back into `session_id` after it said it is replacing its own process
/// image, and hand back the two halves of the connection this client comes back
/// on and whether the session echoed the pane-area field.
///
/// On this machine [`rejoin`] waits for the session's new socket, and `token`
/// is stamped with the token that socket was advertised under. On a server the
/// whole dial runs again — no endpoint file for that session exists on this
/// machine — until the serving machine reaches the restarted session or
/// [`RESTART_WINDOW`] passes. Each dial is paced by [`REMOTE_RESTART_POLL`],
/// and the pause comes first, so the dial meets the session's new image rather
/// than the one it is replacing.
///
/// `resume_token` is stamped with the secret the attach this client came back
/// on minted: the local rejoin records none, and a fresh dial of a server
/// records the one that dial minted.
///
/// The Attach presents no resume token either way: this client comes back by
/// naming `client_id`, and a session that still holds that record hands its
/// view straight back.
///
/// `None` for every way the client cannot come back, including a session that
/// no longer holds this client's record. The caller reports each of them as the
/// session ending unexpectedly. The boolean is false when the session omits
/// the pane-area field.
fn come_back(
    home: &Home,
    session_id: SessionId,
    client_id: ClientId,
    token: &mut ConnectionToken,
    resume_token: &mut Option<ConnectionToken>,
) -> Option<(FrameReader, FrameWriter, bool)> {
    match home {
        Home::Local { runtime_dir } => {
            let (endpoint, connection, pane_area_supported) =
                rejoin(runtime_dir, session_id, client_id, token)?;
            *token = endpoint.token;
            *resume_token = None;
            let (reader, writer) = connection.split();
            Some((reader, writer, pane_area_supported))
        }
        Home::Remote { server } => {
            let deadline = Instant::now() + RESTART_WINDOW;
            loop {
                thread::sleep(REMOTE_RESTART_POLL);
                match dial_remote(
                    server,
                    &SessionSelector::Id(session_id),
                    Some(client_id),
                    None,
                )
                .map_err(CliError::from)
                {
                    Ok(joined) if joined.client_id != client_id => {
                        tracing::warn!(
                            %session_id,
                            "the restarted session no longer held this client and minted a new one"
                        );
                        return None;
                    }
                    Ok(joined) => {
                        *token = joined.token;
                        *resume_token = joined.resume_token;
                        return Some((joined.reader, joined.writer, joined.pane_area_supported));
                    }
                    Err(error) => {
                        if Instant::now() >= deadline {
                            tracing::warn!(%error, "could not reach the restarted session");
                            return None;
                        }
                    }
                }
            }
        }
    }
}

/// Dial `server` for `session_id` again after a remote viewer's link dropped,
/// and hand back the connection it joined on.
///
/// The pause comes before each dial and widens as
/// [`next_redial_wait`] says: 1 second, 2, 4, 8, then 8 before every dial after
/// that. A pause that would end past [`REDIAL_WINDOW`] — 120 seconds from the
/// first pause — is not taken, and no dial follows it: the answer is the last
/// dial's cause. A dial already under way runs to its own timeout, so that
/// answer can arrive after the window closes.
///
/// A [`DialError::Refused`] ends this at once and is the answer: the server
/// answered, and every identical dial after it gets the same answer, so waiting
/// changes nothing. Only a [`DialError::Unreachable`] is dialed again.
///
/// The pause is taken one second at a time. Each slice records
/// `Reconnecting { attempt, retry_in_seconds }` on `client` and draws the frame
/// already on the screen again through `screen`, so the tabline tag counts down
/// `RECONNECTING (attempt 1, retry in 1s)` before the first dial and
/// `attempt 2, retry in 2s` … `retry in 1s` before the second. `active_tab` is
/// the tab that frame shows, and `None` before any frame has been drawn. On
/// every answer, and before returning either way, `client` is put back to no
/// dialing under way; a dial that joined repaints once more, so the tag leaves
/// the screen before the new connection's first frame arrives.
///
/// `resume_token` is the secret the last attach minted. The session hands that
/// attach's view back for it — the active tab, each tab's focused and zoomed
/// pane, and each pane's scroll offset. The session drops that view 120 seconds
/// after it saw the link end, which is earlier than this window starts, so a
/// dial late in the window joins with a fresh view instead.
///
/// A session that no longer holds the view mints a fresh client, and the
/// returned [`Joined`] names it: the viewer takes that client id as its own and
/// keeps running.
///
/// # Errors
/// The cause of the dial this gave up on: the refusal that ended it, or the last
/// unreachable-path cause before the window closed.
fn redial<B: Backend>(
    server: &ServerArg,
    session_id: SessionId,
    resume_token: Option<&ConnectionToken>,
    client: &mut Client,
    screen: &mut Screen<B>,
    active_tab: Option<TabId>,
) -> Result<Joined, Box<CliError>> {
    redial_with(
        || dial_remote(server, &SessionSelector::Id(session_id), None, resume_token),
        session_id,
        client,
        screen,
        active_tab,
    )
}

/// [`redial`]'s loop over any dial: pause, paint the countdown, call `dial`,
/// and classify its answer — a [`DialError::Refused`] ends the loop at once, a
/// [`DialError::Unreachable`] widens the pause and dials again while the pause
/// fits [`REDIAL_WINDOW`].
///
/// # Errors
/// The cause of the dial this gave up on: the refusal that ended it, or the last
/// unreachable-path cause before the window closed.
fn redial_with<B: Backend>(
    mut dial: impl FnMut() -> Result<Joined, DialError>,
    session_id: SessionId,
    client: &mut Client,
    screen: &mut Screen<B>,
    active_tab: Option<TabId>,
) -> Result<Joined, Box<CliError>> {
    let started = Instant::now();
    let mut wait = FIRST_REDIAL_WAIT;
    let mut attempt: u32 = 1;
    let cause = loop {
        let seconds = u32::try_from(wait.as_secs()).unwrap_or(u32::MAX);
        for retry_in_seconds in (1..=seconds).rev() {
            client.set_reconnecting(Some(Reconnecting {
                attempt,
                retry_in_seconds,
            }));
            screen.refresh(client, active_tab);
            thread::sleep(Duration::from_secs(1));
        }
        match dial() {
            Ok(joined) => {
                client.set_reconnecting(None);
                screen.refresh(client, active_tab);
                return Ok(joined);
            }
            Err(DialError::Refused(error)) => break error,
            Err(DialError::Unreachable(error)) => {
                wait = next_redial_wait(wait);
                attempt += 1;
                if !pause_fits(started.elapsed(), wait) {
                    break error;
                }
            }
        }
    };
    client.set_reconnecting(None);
    tracing::warn!(%cause, %session_id, "could not join the session again");
    Err(Box::new(cause))
}

/// Whether a pause of `wait`, begun `elapsed` after the first one, ends inside
/// [`REDIAL_WINDOW`].
///
/// `elapsed` 111 seconds with an 8-second `wait` ends at 119 and fits;
/// 112 seconds with the same `wait` ends at 120 and does not.
fn pause_fits(elapsed: Duration, wait: Duration) -> bool {
    elapsed + wait < REDIAL_WINDOW
}

/// The wait one failed redial hands the next: `wait` doubled, held at
/// [`MAX_REDIAL_WAIT`].
///
/// From [`FIRST_REDIAL_WAIT`] that walks 1 second → 2 → 4 → 8 → 8, and stays at
/// 8 seconds however many dials follow.
fn next_redial_wait(wait: Duration) -> Duration {
    (wait * 2).min(MAX_REDIAL_WAIT)
}

/// Read the terminal's size, record it on `client`, and report the viewport and
/// built-in pane area on `uplink`'s connection.
///
/// The `Resize` carries `Reported(viewport.rows - 2)`, with rows saturating at
/// zero. An `80x24` terminal therefore reports an `80x22` pane area.
fn report_terminal_size(client: &mut Client, uplink: &mut Uplink) {
    let size = viewport();
    client.set_viewport(size);
    uplink.send(IpcRequestKind::Resize {
        viewport: size,
        pane_area: Some(core_pane_area(size)),
    });
}

/// Take everything this terminal typed while the link was down off the loop's
/// channel and answer whether the terminal went away.
///
/// Every event on the channel is dropped, keys, pastes, mouse events and
/// resizes alike, and none is sent. Frames read from the connection that broke
/// are dropped too. The terminal's size is read again once the new connection
/// is up.
///
/// `true` when a [`RuntimeEvent::Quit`] was among them, which is this terminal
/// going away. The drain still runs to the end, so nothing typed before it is
/// left on the channel.
fn drop_input_from_the_blackout(incoming_rx: &mpsc::Receiver<Incoming>) -> bool {
    let mut terminal_gone = false;
    while let Ok(received) = incoming_rx.try_recv() {
        let Incoming::Input(event) = received else {
            continue;
        };
        if matches!(*event, RuntimeEvent::Quit) {
            terminal_gone = true;
        }
    }
    terminal_gone
}

/// The session a listing settles on, picked from the sessions running for this
/// user and the rows in `remote`.
///
/// The local rows are the ones `koshi list-sessions` prints, from the same sweep
/// of the runtime directory, so nothing here probes anything that listing does
/// not. `remote` holds one row per session on a saved server that answered,
/// each carrying that server in its `server` field; a picker for a session
/// switch passes none, since a session on another machine is not one this
/// session can move a client to. A single local row is the answer on its own.
/// Every other non-empty listing — several rows, or one row on a saved server —
/// is printed and the number typed on stdin picks the row. This runs before
/// the terminal enters raw mode, so the prompt is a plain stdin read.
///
/// A session that is listening but could not answer leaves both "nothing is
/// running" and "this is the only one" unprovable, so a list of under two local
/// rows reports that session instead of settling on either.
///
/// The answer names where the picked row sat. The local rows come first and
/// `remote` follows, so a place past the local count is
/// [`Picked::Remote`] at that many places into `remote`.
fn choose(runtime_dir: &Path, remote: Vec<SessionRow>) -> Result<Picked, CliError> {
    let found = discovery::fetch_all(runtime_dir);
    let mut rows = discovery::session_rows(&found.sessions);
    if rows.len() < 2 && !found.is_complete() {
        return Err(found.unanswered("cannot tell which session to attach to"));
    }
    let local = rows.len();
    rows.extend(remote);
    if rows.is_empty() {
        return Err(CliError::NoSessions);
    }
    let at = if settles_unasked(rows.len(), local) {
        0
    } else {
        pick(&rows, &ask(&rows)?)?
    };
    Ok(match at.checked_sub(local) {
        Some(remote_at) => Picked::Remote(remote_at),
        None => Picked::Local(rows[at].id.to_string()),
    })
}

/// Whether a listing of `total` rows, the first `local` of them on this
/// machine, settles on its only row without asking: exactly one row, and that
/// row is local. A single remote row, and every longer listing, is asked.
fn settles_unasked(total: usize, local: usize) -> bool {
    total == 1 && local == 1
}

/// Which row a listing settled on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Picked {
    /// A session running for this user on this machine, named by its id.
    Local(String),
    /// A session on a saved server: where in the listing's `remote` rows it sat.
    Remote(usize),
}

/// Where in `rows` a listing settles. A list of one row settles on it without
/// printing anything; a longer list is printed by [`ask`] and the number typed
/// on stdin names the row.
///
/// # Errors
/// [`CliError::NoSessions`] for an empty `rows`. [`CliError::InvalidArgs`] when
/// stdin cannot be read, and when the line is not one of the listed numbers.
fn settle_on(rows: &[SessionRow]) -> Result<usize, CliError> {
    match rows {
        [] => Err(CliError::NoSessions),
        [_] => Ok(0),
        many => pick(many, &ask(many)?),
    }
}

/// Print one numbered line per session — number, name, id, and for a session
/// on a saved server `(remote: <server>)` — and read back the line the user
/// answers with. The prompt names the range, `[1-3]`, or `[1]` for a single
/// row.
///
/// A line that cannot be read names the number that was expected.
fn ask(rows: &[SessionRow]) -> Result<String, CliError> {
    for (index, row) in rows.iter().enumerate() {
        match &row.server {
            Some(server) => {
                println!("{}) {} {} (remote: {server})", index + 1, row.name, row.id);
            }
            None => println!("{}) {} {}", index + 1, row.name, row.id),
        }
    }
    let range = match rows.len() {
        1 => String::from("1"),
        count => format!("1-{count}"),
    };
    print!("attach to which session? [{range}] ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|error| CliError::InvalidArgs {
            detail: format!(
                "expected a session number 1 to {}, and stdin could not be read: {error}",
                rows.len()
            ),
        })?;
    Ok(line)
}

/// Where in `rows` a listing settles: the place the number on `line` names.
///
/// Empty `rows` is [`CliError::NoSessions`]; a number outside
/// `1..=rows.len()`, and a line that is not a number, are
/// [`CliError::InvalidArgs`] naming the range.
fn pick(rows: &[SessionRow], line: &str) -> Result<usize, CliError> {
    if rows.is_empty() {
        return Err(CliError::NoSessions);
    }
    let typed = line.trim();
    typed
        .parse::<usize>()
        .ok()
        .and_then(|number| {
            let at = number.checked_sub(1)?;
            (at < rows.len()).then_some(at)
        })
        .ok_or_else(|| CliError::InvalidArgs {
            detail: format!(
                "`{typed}` is not one of the listed sessions; \
                 expected a number 1 to {}",
                rows.len()
            ),
        })
}

/// Ask the router where the session `selector` names listens, starting a
/// router first when none is running.
///
/// A value that reads as a session id (`session-<uuid>` or a bare UUID) is
/// that id; anything else is a display name for the router to match.
fn lookup(runtime_dir: &Path, selector: &str) -> Result<SessionAddress, CliError> {
    let selector = selector_of(selector);
    match router_request(runtime_dir, RouterRequestKind::AttachLookup { selector })? {
        RouterResult::Found(address) => Ok(address),
        RouterResult::Error(refusal) => Err(CliError::IpcUnavailable {
            detail: refusal.message,
        }),
        other => Err(CliError::IpcUnavailable {
            detail: format!(
                "the router answered an attach lookup with {}",
                other.wire_name()
            ),
        }),
    }
}

/// What the user typed, as the selector both the router and a remote server
/// resolve: a `session-<uuid>` id or a bare UUID is that id, and anything else
/// is a display name for the far side to match.
fn selector_of(selector: &str) -> SessionSelector {
    match parse_prefixed_uuid(selector, "session") {
        Ok(uuid) => SessionSelector::Id(SessionId::from_uuid(uuid)),
        Err(_) => SessionSelector::Name(selector.to_string()),
    }
}

/// Join the session on an open connection: write the Hello and the Attach back
/// to back, then read both replies in order. Returns the client the server
/// minted for this terminal, the session it says that client joined, the secret
/// this attach minted, and whether the server echoed the pane-area field.
///
/// The client names no identity of its own — the server mints the client id
/// and answers with it — so every value comes from the reply.
///
/// `resume` names the client record to come back as: a client returning after
/// the session replaced its own process image carries it, and a first join
/// leaves it `None`. The server hands that record back when it still holds it,
/// the tab that record was viewing still exists, and no connection is
/// streaming for it, and mints a fresh client otherwise, so the returned id is
/// the one the caller holds from here either way.
///
/// The Attach presents no resume token: a join over a connection this machine
/// opened names the client record it comes back as instead.
fn join(
    connection: &mut Connection,
    token: &ConnectionToken,
    resume: Option<ClientId>,
) -> Result<(ClientId, SessionId, Option<ConnectionToken>, bool), CliError> {
    let hello = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::hello(token.clone()),
    };
    connection.send(&hello).map_err(talk::talk_failed)?;
    connection
        .send(&attach_request(resume, None))
        .map_err(talk::talk_failed)?;

    settle_version(connection.recv().map_err(talk::talk_failed)?)?;
    let (client_id, session_id, resume_token, pane_area) =
        take_attached(connection.recv().map_err(talk::talk_failed)?)?;
    Ok((client_id, session_id, resume_token, pane_area.is_some()))
}

/// The Attach this client writes, numbered 2: the request that follows the
/// Hello on every connection into a session.
///
/// `resume` names the client record to come back as, and is `None` on a first
/// join. `resume_token` is the secret the last attach minted, presented to get
/// that attach's view back, and is `None` on a first join and whenever no
/// token was minted. Reports the pane area left by the built-in two-row UI.
fn attach_request(resume: Option<ClientId>, resume_token: Option<&ConnectionToken>) -> IpcRequest {
    let viewport = viewport();
    IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::Attach {
            viewport,
            filter: EventFilterSpec::All,
            resume,
            resume_token: resume_token.cloned(),
            pane_area: Some(core_pane_area(viewport)),
        },
    }
}

/// Check the protocol version a Hello answer settled on against the range this
/// build asked for.
fn settle_version(reply: IncomingResponse) -> Result<(), CliError> {
    match talk::SESSION.take_result(reply)? {
        IpcResult::Hello {
            protocol_version, ..
        } => talk::SESSION.settled_version(protocol_version),
        IpcResult::Error(refusal) => Err(talk::refused(&refusal)),
        other => Err(talk::SESSION.unexpected_reply(&other)),
    }
}

/// The client the server minted for this terminal, the session it says that
/// client joined, the secret this attach minted, and the pane area the server
/// echoed, out of an Attach answer. A missing echo identifies an older session
/// that uses the fixed two-row compatibility layout.
fn take_attached(
    reply: IncomingResponse,
) -> Result<
    (
        ClientId,
        SessionId,
        Option<ConnectionToken>,
        Option<PaneArea>,
    ),
    CliError,
> {
    match talk::SESSION.take_result(reply)? {
        IpcResult::Attached {
            client_id,
            session_id,
            resume_token,
            pane_area,
            ..
        } => Ok((client_id, session_id, resume_token, pane_area)),
        IpcResult::Error(refusal) => Err(talk::refused(&refusal)),
        other => Err(talk::SESSION.unexpected_reply(&other)),
    }
}

/// Come back to a session that is replacing its own process image: wait for its
/// new socket, connect to it, and join again as `client_id`. Returns the
/// endpoint file, the open connection, and whether the session echoed pane area.
/// The boolean is false for an older session that omits the field.
///
/// `token` is the token this client attached under; the wait watches it for a
/// change.
///
/// `None` for every way the client cannot come back: another local user's
/// session, which advertises no endpoint file this user can read; a session
/// that has not come back inside [`RESTART_WINDOW`]; a new socket that refuses
/// the connection or the join; and a session that no longer held this client's
/// record and minted a fresh one. The caller reports every one of them as the
/// session ending unexpectedly.
fn rejoin(
    runtime_dir: &Path,
    session_id: SessionId,
    client_id: ClientId,
    token: &ConnectionToken,
) -> Option<(EndpointFile, Connection, bool)> {
    if token.expose().is_empty() {
        tracing::warn!(
            %session_id,
            "another local user's session is restarting, and this user cannot read its endpoint file"
        );
        return None;
    }
    let deadline = Instant::now() + RESTART_WINDOW;
    let Some(endpoint) = wait_for_new_endpoint(runtime_dir, session_id, token, deadline) else {
        tracing::warn!(%session_id, "the session advertised no new socket after its restart");
        return None;
    };
    let mut connection = ipc_client::connect(&endpoint, session_id)
        .inspect_err(|error| tracing::warn!(%error, "could not reach the restarted session"))
        .ok()?;
    let (rejoined, _, _, pane_area_supported) =
        join(&mut connection, &endpoint.token, Some(client_id))
            .inspect_err(
                |error| tracing::warn!(%error, "the restarted session refused this client"),
            )
            .ok()?;
    if rejoined != client_id {
        tracing::warn!(
            %session_id,
            "the restarted session no longer held this client and minted a new one"
        );
        return None;
    }
    Some((endpoint, connection, pane_area_supported))
}

/// Wait for `session_id` to advertise a socket under a token other than
/// `token`, and hand that endpoint file back. `None` when `deadline` passes
/// with the token still unchanged.
///
/// A session server mints a fresh token every time it binds, so another token
/// means the session's new image is serving. The process id in the file says
/// nothing: `execvp` keeps it, so a Unix swap comes back under the same one.
///
/// The file is read every [`RESTART_POLL`] until the deadline. A missing or
/// unreadable file is what the swap leaves while the socket is down, so the
/// wait reads again. The first read happens before the deadline is checked, so
/// a deadline already passed still takes a session that is already back.
fn wait_for_new_endpoint(
    runtime_dir: &Path,
    session_id: SessionId,
    token: &ConnectionToken,
    deadline: Instant,
) -> Option<EndpointFile> {
    let path = EndpointFile::path(runtime_dir, session_id);
    loop {
        if let Ok(endpoint) = EndpointFile::read(&path) {
            if endpoint.token != *token {
                return Some(endpoint);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(RESTART_POLL);
    }
}

/// This terminal's size in cells, or [`FALLBACK_VIEWPORT`] when it has none to
/// report.
fn viewport() -> Size {
    match size() {
        Ok((cols, rows)) => Size { cols, rows },
        Err(error) => {
            tracing::warn!(%error, "could not read the terminal size");
            FALLBACK_VIEWPORT
        }
    }
}

/// Read the session's frames on their own thread and put each on the loop's
/// channel. A failed read is put there too and ends the thread: it is the
/// frame the loop classifies as a death.
///
/// A frame this build has no variant for is dropped here and never reaches the
/// loop. It comes from a newer session server, and the frames around it still
/// draw.
///
/// `connection` numbers the connection being read, and every frame carries it,
/// so the loop can tell this reader's frames from those of a connection it has
/// already left.
fn spawn_frame_reader(
    mut reader: FrameReader,
    connection: u64,
    incoming_tx: mpsc::Sender<Incoming>,
) {
    let _ = thread::Builder::new()
        .name("koshi-attach-reader".to_string())
        .spawn(move || loop {
            let frame = match reader.recv::<IncomingEvent>() {
                Ok(MaybeKnown::Known(event)) => Ok(event),
                Ok(MaybeKnown::Unknown { name }) => {
                    tracing::debug!(%name, "session frame this build does not have");
                    continue;
                }
                Err(error) => Err(error),
            };
            let broken = frame.is_err();
            if incoming_tx
                .send(Incoming::Frame { connection, frame })
                .is_err()
                || broken
            {
                break;
            }
        })
        .expect("spawn attach reader thread");
}

/// Write the loop's requests on their own thread and give back the queue they
/// are handed to.
///
/// A write blocks until the session has taken the bytes, so it is done here:
/// the loop only puts the request on the queue, and a session reading slowly
/// backs the queue up instead of holding this terminal's input.
///
/// Requests leave in the order they were queued and nothing here is folded or
/// reordered: a [`WireMouseAction::Forward`] is one report the pane's program
/// must see, and every request carries the `request_id` the session answers
/// under. The pile the loop holds is where folding happens, in [`hold`].
///
/// A request over the frame cap — a paste of more text than one frame carries —
/// is refused with nothing written, and that request alone is dropped; the next
/// one goes out. Any other failed write ends the thread.
fn spawn_uplink_writer(mut writer: FrameWriter) -> mpsc::Sender<IpcRequest> {
    let (requests_tx, requests_rx) = mpsc::channel::<IpcRequest>();
    let _ = thread::Builder::new()
        .name("koshi-attach-writer".to_string())
        .spawn(move || {
            for request in requests_rx {
                match writer.send(&request) {
                    Ok(()) => {}
                    Err(IpcError::FrameTooLarge { len, max }) => {
                        tracing::warn!(
                            len,
                            max,
                            kind = request.kind.name(),
                            "request over the cap was not sent"
                        );
                    }
                    Err(error) => {
                        tracing::warn!(%error, "could not send to the session");
                        break;
                    }
                }
            }
        })
        .expect("spawn attach writer thread");
    requests_tx
}

/// Move every event the terminal-input thread produces onto the loop's own
/// channel, so the session's frames and this terminal's input arrive on one
/// receiver.
fn spawn_input_relay(input_rx: mpsc::Receiver<RuntimeEvent>, incoming_tx: mpsc::Sender<Incoming>) {
    let _ = thread::Builder::new()
        .name("koshi-attach-input".to_string())
        .spawn(move || {
            for event in input_rx {
                if incoming_tx.send(Incoming::Input(Box::new(event))).is_err() {
                    break;
                }
            }
        })
        .expect("spawn attach input relay thread");
}

/// Answer one event read from this terminal.
///
/// A key belongs to the viewer that received it: the keymap, the input mode and
/// any open sequence all live here, so this decides what the press means and
/// the session sees only the answer — the commands a binding runs, or a press
/// to write. A press that goes to the pane's program also ends the selection
/// gesture under way, since the input is the program's. A resize records the
/// viewport and pane area, since the session reconciles tab sizes from every
/// viewer's report. Pasted text goes up whole and ends the gesture too; the
/// session writes it into the pane, bracketing it when that pane asked for
/// bracketed paste. A paste of more text than one frame carries never leaves
/// this terminal.
///
/// [`RuntimeEvent::Quit`] never reaches here: the loop reads it as
/// [`Ending::TerminalGone`] and stops. An input thread runs only for a terminal
/// that had keys to read, so a read failure from one is that terminal going
/// away.
fn handle_input(client: &mut Client, uplink: &mut Uplink, event: RuntimeEvent) {
    match event {
        RuntimeEvent::KeyInput { chord, .. } => match client.resolve_key(chord, Instant::now()) {
            KeyOutcome::Fire(bound) => uplink.submit(client, bound),
            KeyOutcome::PassThrough(chord) => {
                // The key belongs to the program in the pane, so a selection
                // gesture over it is over.
                client.end_mouse_selection();
                uplink.send(IpcRequestKind::KeyPress { chord });
            }
            // Held or dropped: nothing reaches the session. A chord that opens
            // or closes a sequence moves the breadcrumb the hint bar draws, and
            // the pass draws it. A discard moves nothing and draws nothing.
            KeyOutcome::Pending | KeyOutcome::Discard => {}
        },
        RuntimeEvent::Resize {
            size, pane_area, ..
        } => {
            client.set_viewport(size);
            let pane_area = pane_area.unwrap_or_else(|| core_pane_area(size));
            uplink.send(IpcRequestKind::Resize {
                viewport: size,
                pane_area: Some(pane_area),
            });
        }
        RuntimeEvent::HostPaste { text, .. } => {
            // The text belongs to the program in the pane, so a selection
            // gesture over it is over.
            client.end_mouse_selection();
            uplink.send(IpcRequestKind::Paste { text });
        }
        _ => {}
    }
}

/// Fire the viewer's open key sequence if its ambiguity deadline has passed at
/// `now`, sending the commands it resolves to up the connection.
///
/// A sequence that is both a complete binding and a longer one's prefix fires
/// when its deadline passes. The viewer holds it, so it decides; the session
/// only runs what comes back.
fn fire_expired_key_sequence(client: &mut Client, uplink: &mut Uplink, now: Instant) {
    let Some(bound) = client.expire_key_sequence(now) else {
        return;
    };
    uplink.submit(client, bound);
}

/// Answer one mouse event read from this terminal against `frame`, the frame
/// this terminal last painted, and add everything the viewer decided to
/// `pending` through [`hold`].
///
/// Viewer state moves at once — the hovered pane, the gesture under way, the
/// capture a press takes — so a drag keeps tracking the pointer while earlier
/// rounds are still unanswered. Every [`MouseAction::Forward`] carrying a press
/// records the capture here, through [`Client::note_press_forwarded`], before
/// the round is written.
fn handle_mouse_event(
    client: &mut Client,
    frame: &MouseFrame,
    mouse: MouseInput,
    pending: &mut Vec<MouseAction>,
) {
    client.apply_events();
    let actions = client.handle_mouse(mouse, frame, Instant::now());
    for action in &actions {
        if let MouseAction::Forward { pane, mouse } = action {
            if let MouseKind::Press(button) = mouse.kind {
                client.note_press_forwarded(*pane, button);
            }
        }
    }
    hold(pending, actions);
}

/// Add `actions` to the pile waiting for the next write and keep the pile at
/// [`MAX_PENDING_MOUSE`].
///
/// A pile over the cap is folded first, which loses nothing: the fold states
/// the same movement in fewer actions. What is still over the cap after that is
/// trimmed by dropping the oldest scrolls, which move this viewer's own view of
/// a pane and nothing else, so an overrun costs scrollback distance on the
/// oldest wheel ticks of the burst.
///
/// Nothing else is ever dropped. A [`MouseAction::Forward`] is one report the
/// pane's program must see, and [`MouseAction::AltScrollArrows`] is arrow keys
/// that program reads; a [`MouseAction::Command`] runs once, and a
/// [`MouseAction::Resize`] states a border's whole distance from its drag
/// anchor. A pile holding no scrolls therefore stays over the cap rather than
/// break any of those.
fn hold(pending: &mut Vec<MouseAction>, actions: Vec<MouseAction>) {
    pending.extend(actions);
    if pending.len() <= MAX_PENDING_MOUSE {
        return;
    }
    *pending = coalesce(take(pending));
    let mut over = pending.len().saturating_sub(MAX_PENDING_MOUSE);
    pending.retain(|action| {
        let dropped = over > 0 && matches!(action, MouseAction::Scroll { .. });
        over -= usize::from(dropped);
        !dropped
    });
}

/// Take in what the session did with one round of mouse actions. An answer
/// releases nothing — it only reconciles this viewer's state with what the
/// session did.
///
/// Every answer says which gesture it belongs to on its own: a `Scrolled` names
/// its pane and a `Resized` names its pane, side and step. Answers are therefore
/// applied whatever order they arrive in, and an answer for a gesture that has
/// since ended changes nothing.
///
/// A wheel tick's `Scrolled` is applied too and does nothing: `Client`'s
/// `scroll_from_top` — the slot `note_scroll_applied` reads — is written only
/// by `Client::expire_mouse_scroll`, so only a scroll the edge timer asked for
/// finds anything there.
///
/// A `Resized` does three things for the border it names. It moves that border's
/// drag anchor over the cells the session took — an answer for a border the
/// viewer has since let go of leaves the drag alone, which
/// `Client::note_resize_applied` checks. It re-bases the buffered moves for that
/// same border, which were measured from the anchor this answer advances, so the
/// cells the session already took come off them. And it forgets the
/// [`SentBorderMove`] the round recorded, so those cells stop coming off the
/// next move for that border.
fn apply_answer(
    client: &mut Client,
    frame: &MouseFrame,
    sent: &mut Vec<SentBorderMove>,
    request_id: u64,
    answers: Vec<MouseAnswer>,
    pending: &mut Vec<MouseAction>,
) {
    for answer in answers {
        match answer {
            MouseAnswer::Scrolled { pane, top } => {
                hold(pending, client.note_scroll_applied(pane, top, frame));
            }
            MouseAnswer::Resized {
                pane,
                side,
                step,
                applied,
            } => {
                client.note_resize_applied(pane, side, step, applied);
                rebase_border_moves(pending, pane, side, step, applied);
                // The oldest match is the one this answer reports: the session
                // answers a round's moves in the order they were written.
                if let Some(index) = sent.iter().position(|written| {
                    written.request_id == request_id && written.pane == pane && written.side == side
                }) {
                    sent.remove(index);
                }
            }
        }
    }
}

/// Take the cells the session just moved off every buffered border move for
/// `pane`'s `side`.
///
/// A buffered move names the whole distance from the drag anchor to the pointer
/// it was decided at, and the answered move travelled `applied` cells of that
/// same distance in the direction `step` names. What is left to ask for is
/// therefore each buffered move's own signed distance minus `step * applied`,
/// which is zero when the session already went the whole way and flips sign when
/// the pointer crossed back past the border.
///
/// A drag that buffered 4 cells down while a round of 3 cells down was out is
/// left asking for 1. A buffered move for any other pane or side is left as it
/// is: it measures from its own anchor, which this answer did not move.
fn rebase_border_moves(
    pending: &mut [MouseAction],
    pane: PaneId,
    side: Direction,
    step: i16,
    applied: u16,
) {
    let done = i32::from(step) * i32::from(applied);
    for action in pending {
        let MouseAction::Resize {
            pane: moved,
            side: edge,
            step,
            count,
        } = action
        else {
            continue;
        };
        if *moved != pane || *edge != side {
            continue;
        }
        let left = i32::from(*step) * i32::from(*count) - done;
        *step = if left < 0 { -1 } else { 1 };
        *count = u16::try_from(left.unsigned_abs()).unwrap_or(u16::MAX);
    }
}

/// Hand the pile to the writer thread, whatever else is already out there. The
/// one place anything mouse-shaped is sent.
///
/// Nothing is waited for and nothing is timed: the pile is only ever what one
/// pass of the loop decided, so a link fast enough to keep the loop level sends
/// the single action that woke it and folds nothing.
///
/// A border move names the whole distance from its drag anchor, so the cells the
/// moves already on the wire asked for come off it here, and so do the cells
/// this round's own earlier moves for that same border ask for: the session
/// travels every move in a round, one after another. Each move that survives
/// that is recorded in `sent` under the `request_id` it went out with. A move
/// left with no cells to travel is dropped after the fold rather than before it,
/// so the fold still sees it and keeps it as the newest of the border's moves.
fn flush_round(
    uplink: &mut Uplink,
    sent: &mut Vec<SentBorderMove>,
    pending: &mut Vec<MouseAction>,
) {
    if pending.is_empty() {
        return;
    }
    let mut round = coalesce(take(pending));
    let mut moves: Vec<(PaneId, Direction, i32)> = Vec::new();
    for action in &mut round {
        if let MouseAction::Resize {
            pane,
            side,
            step,
            count,
        } = action
        {
            let this_round: i32 = moves
                .iter()
                .filter(|(moved, edge, _)| *moved == *pane && *edge == *side)
                .map(|(_, _, cells)| cells)
                .sum();
            let left =
                i32::from(*step) * i32::from(*count) - asked_for(sent, *pane, *side) - this_round;
            *step = if left < 0 { -1 } else { 1 };
            *count = u16::try_from(left.unsigned_abs()).unwrap_or(u16::MAX);
            if *count != 0 {
                moves.push((*pane, *side, i32::from(*step) * i32::from(*count)));
            }
        }
    }
    round.retain(|action| !matches!(action, MouseAction::Resize { count: 0, .. }));
    let Some(request_id) = send_round(uplink, round) else {
        return;
    };
    sent.extend(moves.into_iter().map(|(pane, side, cells)| SentBorderMove {
        request_id,
        pane,
        side,
        cells,
    }));
    // A session that stops answering never trims this, so the oldest entries go
    // once it holds a burst's worth. A forgotten move leaves its cells counted
    // as never asked, so the next move for that border asks for them again.
    if sent.len() > MAX_PENDING_MOUSE {
        sent.drain(..sent.len() - MAX_PENDING_MOUSE);
    }
}

/// The signed cells every written-and-unanswered move for `pane`'s `side` has
/// already asked for, positive to grow the pane.
///
/// Two unanswered moves that each grow the pane by 3 cells come to 6, so a
/// third move naming 7 cells of travel from the drag anchor asks for 1.
fn asked_for(sent: &[SentBorderMove], pane: PaneId, side: Direction) -> i32 {
    sent.iter()
        .filter(|written| written.pane == pane && written.side == side)
        .map(|written| written.cells)
        .sum()
}

/// Send `actions` as one request and give back the `request_id` it went out
/// under, or `None` for an empty list, which is written as nothing at all.
///
/// One round, one id, one answer: the whole list travels as a single
/// [`IpcRequestKind::Mouse`], which the session answers exactly once.
fn send_round(uplink: &mut Uplink, actions: Vec<MouseAction>) -> Option<u64> {
    if actions.is_empty() {
        return None;
    }
    let round: Vec<WireMouseAction> = actions.into_iter().map(wire).collect();
    Some(uplink.send(IpcRequestKind::Mouse(round)))
}

/// The wire spelling of one action the viewer decided, variant for variant.
fn wire(action: MouseAction) -> WireMouseAction {
    match action {
        MouseAction::Scroll { pane, up, lines } => WireMouseAction::Scroll { pane, up, lines },
        MouseAction::Forward { pane, mouse } => WireMouseAction::Forward { pane, mouse },
        MouseAction::AltScrollArrows { pane, up, count } => {
            WireMouseAction::AltScrollArrows { pane, up, count }
        }
        MouseAction::Resize {
            pane,
            side,
            step,
            count,
        } => WireMouseAction::Resize {
            pane,
            side,
            step,
            count,
        },
        MouseAction::Command(command) => WireMouseAction::Command(Box::new(command)),
    }
}

/// Fold a pile of actions into the shortest run that means the same thing.
///
/// Only neighbours fold, so an action of another kind between two foldable ones
/// keeps them apart: a scroll, a forward, then a scroll stays three actions.
///
/// - Two scrolls over one pane in one direction become one scroll of the summed
///   `lines`: the session moves the view by a count, so two counts are their
///   sum.
/// - Two alternate-scroll runs over one pane in one direction become one run of
///   the summed `count`: the pane's program receives arrow keys, so two runs are
///   the same keys back to back.
/// - Two selection changes for one pane keep the newer: each carries the whole
///   highlight — kind, anchor and cursor — so the newer states all of it.
/// - Two border moves for one pane and side keep the newer: every buffered move
///   carries the full distance from the drag anchor to the pointer it was
///   decided at — an answer that advances the anchor re-bases the older ones in
///   [`rebase_border_moves`] — so they all measure from one place and the newest
///   is the whole move.
/// - A forward always pushes: each report is a separate event the pane's program
///   must see.
fn coalesce(actions: Vec<MouseAction>) -> Vec<MouseAction> {
    let mut folded: Vec<MouseAction> = Vec::with_capacity(actions.len());
    for action in actions {
        let unfolded = match folded.last_mut() {
            Some(tail) => fold(tail, action),
            None => Some(action),
        };
        if let Some(action) = unfolded {
            folded.push(action);
        }
    }
    folded
}

/// Fold `next` into `tail` when the two state one movement twice, and give back
/// `next` when they do not.
fn fold(tail: &mut MouseAction, next: MouseAction) -> Option<MouseAction> {
    match (tail, next) {
        (
            MouseAction::Scroll { pane, up, lines },
            MouseAction::Scroll {
                pane: onto,
                up: same_way,
                lines: more,
            },
        ) if *pane == onto && *up == same_way => {
            *lines += more;
            None
        }
        (
            MouseAction::AltScrollArrows { pane, up, count },
            MouseAction::AltScrollArrows {
                pane: onto,
                up: same_way,
                count: more,
            },
        ) if *pane == onto && *up == same_way => {
            *count += more;
            None
        }
        (
            MouseAction::Resize {
                pane,
                side,
                step,
                count,
            },
            MouseAction::Resize {
                pane: onto,
                side: same_side,
                step: newer_step,
                count: newer_count,
            },
        ) if *pane == onto && *side == same_side => {
            *step = newer_step;
            *count = newer_count;
            None
        }
        (
            MouseAction::Command(Command::Visual(VisualCommand::SetSelection(held))),
            MouseAction::Command(Command::Visual(VisualCommand::SetSelection(newer))),
        ) if held.pane == newer.pane => {
            *held = newer;
            None
        }
        (_, next) => Some(next),
    }
}

/// The sooner of two deadlines, or `None` when neither is set.
fn earliest(left: Option<Duration>, right: Option<Duration>) -> Option<Duration> {
    [left, right].into_iter().flatten().min()
}

/// Every command a plan runs, in order. A plugin host call runs none from here:
/// the plugin host lives on the session.
fn commands(plan: DispatchPlan) -> Vec<Command> {
    match plan {
        DispatchPlan::Command(command) => vec![command],
        DispatchPlan::Sequence(plans) => plans.into_iter().flat_map(commands).collect(),
        DispatchPlan::PluginHostCall { .. } => Vec::new(),
    }
}

/// Take the three things the session decides about this viewer out of the frame
/// it is about to draw: the lock mode, the active tab, and whether mouse-select
/// is on.
///
/// The caller runs this after the frame paint succeeds. The hint bar lists the
/// bindings of the mode this sets.
fn adopt_frame(client: &mut Client, snapshot: &RenderSnapshot) {
    client.set_lock_mode(snapshot.client.lock_mode);
    client.note_active_tab(snapshot.client.active_tab);
    client.set_mouse_select(snapshot.client.mouse_select);
}

/// Classify one frame read from the event stream. `None` keeps the loop
/// reading; `Some` ends it.
///
/// Any failure to read is the same ending: the peer closing the socket — which
/// is what a session server exiting or being killed does — surfaces as a read
/// error, so no timeout is involved.
fn classify(frame: &Result<SessionEvent, IpcError>) -> Option<Ending> {
    match frame {
        Ok(SessionEvent::Detached) => Some(Ending::Detached),
        Ok(SessionEvent::Quit) => Some(Ending::SessionEnded),
        Ok(SessionEvent::Restarting) => Some(Ending::Restarting),
        Ok(SessionEvent::SwitchTo { session_id }) => Some(Ending::Switch(*session_id)),
        Ok(_) => None,
        Err(_) => Some(Ending::Died),
    }
}

/// Print how the stream ended and hand back both the process outcome and the
/// session to attach to next: a broken connection names the cause and how to
/// reattach, and exits non-zero; a switch names the session and prints
/// nothing.
///
/// The way back names the machine the session runs on, so a session on another
/// machine names that server rather than this one.
///
/// A restart reaches here only when the client could not come back on the
/// session's new socket, so it names the same cause and the same way back.
///
/// A remote viewer that gave up dialing again names the cause it gave up on,
/// then `the session continues without you`, then that same way back.
fn report(
    home: &Home,
    ending: Ending,
    session_id: SessionId,
) -> Result<Option<SessionId>, CliError> {
    match ending {
        Ending::Detached => {
            println!("detached from session {session_id}");
            Ok(None)
        }
        Ending::SessionEnded => {
            println!("the session ended");
            Ok(None)
        }
        Ending::Switch(target) => Ok(Some(target)),
        Ending::Died | Ending::Restarting => Err(CliError::Runtime {
            detail: format!(
                "the session ended unexpectedly\n  {}",
                way_back(home, session_id)
            ),
        }),
        Ending::LinkLost(cause) => Err(CliError::Runtime {
            detail: format!(
                "{cause}\n  the session continues without you\n  {}",
                way_back(home, session_id)
            ),
        }),
        // Nothing is left to read a message, so this ending is logged rather
        // than printed. The session drops this client when the connection
        // closes behind it.
        Ending::TerminalGone => {
            tracing::info!(%session_id, "this terminal went away; leaving the session running");
            Ok(None)
        }
    }
}

/// How to reach `session_id` again from where it runs: the command that shows
/// whether it still runs, and the attach command to come back on.
///
/// A session on this machine reads `run \`koshi list-sessions\`; …`. One on a
/// server names that server in both commands — `koshi attach --remote my-box`
/// shows that server's sessions, since `list-sessions` answers from this
/// machine alone.
fn way_back(home: &Home, session_id: SessionId) -> String {
    match home {
        Home::Local { .. } => format!(
            "run `koshi list-sessions`; if session {session_id} is still listed, \
             reattach with `koshi attach {session_id}`"
        ),
        Home::Remote { server } => {
            let server = server.label();
            format!(
                "run `koshi attach --remote {server}` to see that server's sessions; \
                 if session {session_id} is among them, reattach with \
                 `koshi attach --remote {server} {session_id}`"
            )
        }
    }
}
