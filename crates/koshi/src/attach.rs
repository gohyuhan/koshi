//! The attached client: join a running session over its control socket and
//! become a second window onto it.
//!
//! The body here joins a session by id. The value after `koshi attach` is
//! resolved to one before it: the router turns that value into a session's
//! address, starting a router first when none runs, and with no value the
//! sessions running for this user are offered and the one picked becomes that
//! value. The session's endpoint file holds the token the Hello presents; the
//! Hello and the Attach are written back to back, so joining costs one round
//! trip.
//!
//! Everything that can refuse the join happens before the terminal changes
//! mode: a refused lookup, a refused Hello, a refused Attach. Once the session
//! answers `Attached`, the terminal enters raw mode and the alternate screen
//! behind a cleanup guard, so every way out
//! — a detach, the session ending, a dead session server, or a panic — leaves
//! the outer terminal as it was found.
//!
//! From there the connection carries traffic both ways. The session composes
//! this terminal's own frame — at this terminal's size and scroll position —
//! and pushes it down the event stream, which this loop paints. This terminal's
//! keys, pastes and resizes travel back up the same connection: a key the
//! viewer's keymap does not bind goes up as a key press, a binding that fires
//! is resolved against the action table here and goes up as the commands it
//! runs, a paste goes up whole, and a resize goes up as the new viewport.
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
//! The keymap, the colors and the hint bar are this terminal's own, read from
//! this user's config files.

use std::io;
use std::io::Write;
use std::mem::take;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Instant, SystemTime};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{EnableBracketedPaste, EnableMouseCapture};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{enable_raw_mode, size, EnterAlternateScreen};
use ratatui::crossterm::tty::IsTty;
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport};

use koshi_beta::beta_feature;
use koshi_client::input::KeyOutcome;
use koshi_client::mouse::MouseAction;
use koshi_client::Client;
use koshi_config::types::BoundAction;
use koshi_core::command::{
    Command, CommandEnvelope, CommandResult, CommandSource, SwitchSessionArgs, VisualCommand,
};
use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::{ClientId, CommandId, PaneId, SessionId};
use koshi_core::mouse::{MouseAnswer, MouseInput, MouseKind};
use koshi_core::registry::ActionRegistry;
use koshi_core::resolve::{resolve_action, DispatchPlan};
use koshi_ipc::error::IpcError;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::frame::PaintedFrame;
use koshi_ipc::protocol::{
    ConnectionToken, EventFilterSpec, IpcRequest, IpcRequestKind, IpcResponse, IpcResult,
    WireMouseAction, PROTOCOL_VERSION,
};
use koshi_ipc::router::{RouterRequestKind, RouterResult, SessionAddress, SessionSelector};
use koshi_ipc::transport::{Connection, FrameReader, FrameWriter};
use koshi_observability::cleanup::{install_panic_hook, TerminalCleanupGuard};
use koshi_renderer::snapshot::{CursorStyle, MouseFrame, RenderSnapshot};
use koshi_runtime::runtime::event::RuntimeEvent;

use crate::app;
use crate::attach::paint::to_snapshot;
use crate::cli::parse_prefixed_uuid;
use crate::discovery::{self, SessionRow};
use crate::error::CliError;
use crate::in_session::InSessionContext;
use crate::ipc_client;
use crate::router_client::router_request;

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

/// How an attached client's event stream ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

/// One thing the loop reacts to: a frame read off the session's event stream,
/// or an event read from this terminal.
///
/// Both arrive on one channel, so one blocking read serves the session and the
/// keyboard at once.
enum Incoming {
    /// A frame the session wrote, or the read that failed.
    Frame(Result<SessionEvent, IpcError>),
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
        None => choose(runtime_dir)?,
    };
    lookup(runtime_dir, &selector)
}

/// Join a running session in this terminal as a new client.
///
/// `selector` is a `session-<uuid>` id, a bare UUID, or a session display
/// name. `None` picks one from the sessions running for this user instead:
/// nothing running is a failure, one session is taken straight away, and more
/// than one is printed as a numbered list to answer on stdin.
#[beta_feature(otherwise = Err(CliError::Runtime {
    detail: koshi_beta::blocked_message("koshi attach"),
}))]
pub fn run(selector: Option<&str>) -> Result<(), CliError> {
    let runtime_dir = ipc_client::runtime_dir()?;
    let address = resolve_session(&runtime_dir, selector)?;
    attach_session(&runtime_dir, address.id)
}

/// Ask the session this CLI runs inside to move its own client to another
/// session.
///
/// `selector` names the session to move to, resolved exactly as [`run`]
/// resolves it. This terminal already holds a client, so the session moves that
/// one rather than a second client opening on top of it.
#[beta_feature(otherwise = Err(CliError::Runtime {
    detail: koshi_beta::blocked_message("koshi attach"),
}))]
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
    let mut session_id = session_id;
    while let Some(next) = attach_once(runtime_dir, session_id)? {
        session_id = next;
    }
    Ok(())
}

/// Join the session `session_id` names and run one attachment of it, handing
/// back the session to attach to next when this one moved the client on.
///
/// The terminal enters raw mode and the alternate screen behind a cleanup
/// guard this call owns, and leaves both before it returns, so the terminal is
/// restored between one session and the next.
fn attach_once(runtime_dir: &Path, session_id: SessionId) -> Result<Option<SessionId>, CliError> {
    let endpoint = ipc_client::read_endpoint(runtime_dir, session_id)?;
    let mut connection = ipc_client::connect(&endpoint, session_id)?;
    let (client_id, session_id) = join(&mut connection, &endpoint.token)?;

    // The session accepted the client, so the terminal may change mode now.
    // The hooks undo every mode this function sets, and the panic hook shares
    // them, so an unwinding panic restores the terminal too.
    let cleanup = TerminalCleanupGuard::new();
    app::register_terminal_restore(&cleanup);
    let _panic_guard = install_panic_hook(&cleanup);
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
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout())).unwrap_or_else(|error| {
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

    // One channel, two producers: the connection's reading half and this
    // terminal's input thread.
    let (incoming_tx, incoming_rx) = mpsc::channel();
    let (reader, writer) = connection.split();
    spawn_frame_reader(reader, incoming_tx.clone());

    // Standard input that is not a terminal has no keys to read, which is what
    // `koshi attach` started with its input redirected has. It runs as a viewer
    // that types nothing, and no input thread is started for it. Every read
    // failure a started thread reports is therefore this terminal going away.
    if io::stdin().is_tty() {
        let (input_tx, input_rx) = mpsc::channel();
        app::spawn_input_thread(input_tx, client_id);
        spawn_input_relay(input_rx, incoming_tx);
    } else {
        tracing::info!("standard input is not a terminal, so this client reads no keys");
        // The frame reader holds the only other sender, so its end must close
        // the channel the loop waits on.
        drop(incoming_tx);
    }

    // The viewer half: this terminal's own keymap, colors and hint bar, read
    // from this user's config files. Its frames arrive over the connection
    // rather than over a session subscription, so the receiver it holds has no
    // sender. It also holds the cleanup guard, since the outer terminal that
    // guard restores is this viewer's.
    // `load` collects its warnings instead of logging them, so they are
    // replayed here.
    let (loaded, config_warnings) = crate::config::load();
    for warning in &config_warnings {
        tracing::warn!("{warning}");
    }
    let (_events_tx, events_rx) = mpsc::channel();
    let mut client = app::viewer(client_id, viewport(), events_rx, cleanup, loaded);
    let mut uplink = Uplink {
        requests: spawn_uplink_writer(writer),
        registry: ActionRegistry::new(),
        next_request_id: FIRST_LOOP_REQUEST_ID,
    };
    let mut last_title = String::new();
    let mut last_cursor = None;
    let mut last_frame: Option<MouseFrame> = None;
    let mut last_painted: Option<Box<PaintedFrame>> = None;
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

    let ending = loop {
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

        // Set when the frame on the screen is unchanged but this viewer's own
        // chrome moved — a new hovered pane, a scrolled tab strip.
        let mut chrome_moved = false;
        let mut ended = None;
        for received in batch {
            match received {
                Incoming::Frame(frame) => {
                    if let Some(ending) = classify(&frame) {
                        ended = Some(ending);
                        break;
                    }
                    match frame {
                        Ok(SessionEvent::Painted { frame }) => {
                            let snapshot = to_snapshot(&frame);
                            // The session is authoritative over this client's
                            // input mode and its mouse-select mode, and painting
                            // is how the viewer learns which tab it is on.
                            paint(
                                &mut terminal,
                                &client,
                                &snapshot,
                                &mut last_title,
                                &mut last_cursor,
                            );
                            last_frame = Some(adopt_frame(&mut client, snapshot));
                            last_painted = Some(frame);
                        }
                        Ok(SessionEvent::MouseAnswer {
                            request_id,
                            answers,
                        }) => {
                            if let Some(frame) = last_frame.as_ref() {
                                apply_answer(
                                    &mut client,
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
                            let tab = frame.client.active_tab;
                            let before = client.chrome(tab);
                            handle_mouse_event(&mut client, frame, mouse, &mut pending);
                            chrome_moved |= client.chrome(tab) != before;
                        }
                    }
                    event => handle_input(&mut client, &mut uplink, event),
                },
            }
        }
        if let Some(ending) = ended {
            break ending;
        }
        fire_expired_key_sequence(&mut client, &mut uplink, Instant::now());
        // A selection drag held past a pane's edge keeps scrolling while the
        // pointer sits still, so the clock drives it. Asking on every iteration
        // is what re-arms the timer at each firing.
        if let Some(frame) = last_frame.as_ref() {
            hold(
                &mut pending,
                client.expire_mouse_scroll(Instant::now(), frame),
            );
        }
        // The hovered pane and the tab strip's position are this viewer's own,
        // and no session mutation marks them stale, so the repaint is local:
        // the frame the session last sent, drawn again with the new chrome.
        if chrome_moved {
            if let Some(frame) = last_painted.as_ref() {
                let snapshot = to_snapshot(frame);
                paint(
                    &mut terminal,
                    &client,
                    &snapshot,
                    &mut last_title,
                    &mut last_cursor,
                );
            }
        }
        flush_round(&mut uplink, &mut sent, &mut pending);
    };

    // Restore the terminal before anything is printed, so the message lands on
    // the shell's own screen rather than the alternate one, and nothing follows
    // it. Dropping the ratatui terminal shows the cursor a painted frame hid,
    // which belongs on the alternate screen; dropping the client then runs the
    // cleanup guard it holds, which leaves that screen.
    drop(terminal);
    drop(client);
    report(ending, session_id)
}

/// The session a bare `koshi attach` joins, picked from the sessions running for
/// this user.
///
/// The rows are the ones `koshi list-sessions` prints, from the same sweep of
/// the runtime directory, so nothing here probes anything that listing does
/// not and nothing remote is involved. One row is the answer on its own; more
/// than one is printed and the number typed on stdin picks the row. This runs
/// before the terminal enters raw mode, so the prompt is a plain stdin read.
///
/// A session that is listening but could not answer leaves both "nothing is
/// running" and "this is the only one" unprovable, so a list of under two rows
/// reports that session instead of settling on either.
fn choose(runtime_dir: &Path) -> Result<String, CliError> {
    let found = discovery::fetch_all(runtime_dir);
    let rows = discovery::session_rows(&found.sessions);
    if rows.len() < 2 && !found.is_complete() {
        return Err(found.unanswered("cannot tell which session to attach to"));
    }
    // Only a list longer than one row has anything to pick, so only that asks.
    let line = if rows.len() > 1 {
        ask(&rows)?
    } else {
        String::new()
    };
    pick(&rows, &line)
}

/// Print one numbered line per session — number, name, id — and read back the
/// line the user answers with.
///
/// A line that cannot be read names the number that was expected.
fn ask(rows: &[SessionRow]) -> Result<String, CliError> {
    for (index, row) in rows.iter().enumerate() {
        println!("{}) {} {}", index + 1, row.name, row.id);
    }
    print!("attach to which session? [1-{}] ", rows.len());
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

/// The session id a listing settles on: the only row's id when the listing has
/// one row, and otherwise the id of the row the number on `line` names.
///
/// `line` is read only when the listing has more than one row. A listing with
/// no rows has nothing to attach to; a number outside the printed range, and a
/// line that is not a number, both name the range that was expected.
fn pick(rows: &[SessionRow], line: &str) -> Result<String, CliError> {
    match rows {
        [] => Err(CliError::NoSessions),
        [only] => Ok(only.id.to_string()),
        many => {
            let typed = line.trim();
            typed
                .parse::<usize>()
                .ok()
                .and_then(|number| many.get(number.checked_sub(1)?))
                .map(|row| row.id.to_string())
                .ok_or_else(|| CliError::InvalidArgs {
                    detail: format!(
                        "`{typed}` is not one of the listed sessions; \
                         expected a number 1 to {}",
                        many.len()
                    ),
                })
        }
    }
}

/// Ask the router where the session `selector` names listens, starting a
/// router first when none is running.
///
/// A value that reads as a session id (`session-<uuid>` or a bare UUID) is
/// that id; anything else is a display name for the router to match.
fn lookup(runtime_dir: &Path, selector: &str) -> Result<SessionAddress, CliError> {
    let selector = match parse_prefixed_uuid(selector, "session") {
        Ok(uuid) => SessionSelector::Id(SessionId::from_uuid(uuid)),
        Err(_) => SessionSelector::Name(selector.to_string()),
    };
    match router_request(runtime_dir, RouterRequestKind::AttachLookup { selector })? {
        RouterResult::Found(address) => Ok(address),
        RouterResult::Error(refusal) => Err(CliError::IpcUnavailable {
            detail: refusal.message,
        }),
        other => Err(CliError::IpcUnavailable {
            detail: format!("the router answered an attach lookup with {other:?}"),
        }),
    }
}

/// Join the session on an open connection: write the Hello and the Attach back
/// to back, then read both replies in order. Returns the client the server
/// minted for this terminal and the session it says that client joined.
///
/// The client names no identity of its own — the server mints the client id
/// and answers with it — so both values come from the reply.
fn join(
    connection: &mut Connection,
    token: &ConnectionToken,
) -> Result<(ClientId, SessionId), CliError> {
    let hello = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            protocol_version: PROTOCOL_VERSION,
            token: token.clone(),
        },
    };
    let attach = IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::Attach {
            viewport: viewport(),
            filter: EventFilterSpec::All,
        },
    };
    connection.send(&hello).map_err(ipc_client::talk_failed)?;
    connection.send(&attach).map_err(ipc_client::talk_failed)?;

    let hello_reply: IpcResponse = connection.recv().map_err(ipc_client::talk_failed)?;
    match hello_reply.result {
        IpcResult::Hello => {}
        IpcResult::Error(refusal) => return Err(ipc_client::refused(&refusal)),
        other => return Err(ipc_client::unexpected_reply(&other)),
    }

    let attach_reply: IpcResponse = connection.recv().map_err(ipc_client::talk_failed)?;
    match attach_reply.result {
        IpcResult::Attached {
            client_id,
            session_id,
            ..
        } => Ok((client_id, session_id)),
        IpcResult::Error(refusal) => Err(ipc_client::refused(&refusal)),
        other => Err(ipc_client::unexpected_reply(&other)),
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
fn spawn_frame_reader(mut reader: FrameReader, incoming_tx: mpsc::Sender<Incoming>) {
    let _ = thread::Builder::new()
        .name("koshi-attach-reader".to_string())
        .spawn(move || loop {
            let frame = reader.recv::<SessionEvent>();
            let broken = frame.is_err();
            if incoming_tx.send(Incoming::Frame(frame)).is_err() || broken {
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
/// is refused with nothing written, so that request alone is dropped and the
/// next one goes out. Any other failed write ends the thread; the frame reader
/// meets the same broken connection and ends the loop.
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
/// gesture under way, since the input is the program's. A resize is recorded
/// here and reported up, since the session reconciles tab sizes from every
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
            // Held or dropped: nothing reaches the session, and the next frame
            // redraws the hint bar from viewer state.
            KeyOutcome::Pending | KeyOutcome::Discard => {}
        },
        RuntimeEvent::Resize { size, .. } => {
            client.set_viewport(size);
            uplink.send(IpcRequestKind::Resize { viewport: size });
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
/// rounds are still unanswered. The capture is recorded here, on
/// the viewer's own decision to forward the press, rather than on the session's
/// report that the pane took it.
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
        let drop = over > 0 && matches!(action, MouseAction::Scroll { .. });
        over -= usize::from(drop);
        !drop
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
fn earliest(
    left: Option<std::time::Duration>,
    right: Option<std::time::Duration>,
) -> Option<std::time::Duration> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(timeout), None) | (None, Some(timeout)) => Some(timeout),
        (None, None) => None,
    }
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

/// Take what the session decides about this viewer out of the frame it just
/// drew, and hand back the frame a mouse event is placed against.
///
/// The session owns this client's lock mode, its active tab and whether
/// mouse-select is on, so each is read from the frame rather than kept locally.
/// The returned [`MouseFrame`] holds where the surfaces sat and what the cells
/// under them were, which is what the next mouse event is answered from.
fn adopt_frame(client: &mut Client, snapshot: RenderSnapshot) -> MouseFrame {
    client.set_lock_mode(snapshot.client.lock_mode);
    client.note_active_tab(snapshot.client.active_tab);
    client.set_mouse_select(snapshot.client.mouse_select);
    MouseFrame::from(snapshot)
}

/// Paint one frame the session sent, with [`app::paint_frame`].
///
/// A draw that fails is logged rather than ending the loop, and the next frame
/// repaints the whole viewport.
fn paint(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    client: &Client,
    snapshot: &RenderSnapshot,
    last_title: &mut String,
    last_cursor: &mut Option<CursorStyle>,
) {
    let _ = app::paint_frame(terminal, client, snapshot, last_title, last_cursor)
        .inspect_err(|error| tracing::warn!(%error, "could not paint the frame"));
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
        Ok(SessionEvent::SwitchTo { session_id }) => Some(Ending::Switch(*session_id)),
        Ok(_) => None,
        Err(_) => Some(Ending::Died),
    }
}

/// Print how the stream ended and hand back both the process outcome and the
/// session to attach to next: a broken connection names the cause and how to
/// reattach, and exits non-zero; a switch names the session and prints
/// nothing.
fn report(ending: Ending, session_id: SessionId) -> Result<Option<SessionId>, CliError> {
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
        Ending::Died => Err(CliError::Runtime {
            detail: format!(
                "the session ended unexpectedly\n  \
                 run `koshi list-sessions`; if session {session_id} is still listed, \
                 reattach with `koshi attach {session_id}`"
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
