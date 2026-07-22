//! The runtime event inbox.
//!
//! [`RuntimeEvent`] is the single typed channel the dispatcher thread drains.
//! Every asynchronous trigger the runtime must react to — child output, child
//! exit, a client resize, a periodic tick, terminal input, an IPC command, a
//! plugin command — arrives as one variant, so the dispatcher consumes every
//! trigger from one shared `std::sync::mpsc` inbox.
//!
//! These are *input* triggers, distinct from the *output* facts the dispatcher
//! emits ([`koshi_core::event::Event`]): a [`RuntimeEvent::ChildExit`] is the raw
//! notification that a child died, while the emitted `PaneProcessExited` is the
//! resulting domain fact.
//!
//! The inbox stays in-process — producers send into it directly — so
//! `RuntimeEvent` is not `Serialize`, unlike the command and event vocabulary
//! that crosses the IPC socket.

use std::sync::mpsc::Sender;
use std::time::SystemTime;

use koshi_core::{
    command::{CommandEnvelope, CommandResult},
    discovery::SessionOverview,
    geometry::Size,
    ids::{ClientId, PaneId, SessionId, TabId},
    key::KeyChord,
    mouse::MouseInput,
    process::ExitStatus,
};

/// A trigger the dispatcher thread reacts to, drained from the runtime inbox.
///
/// One variant per runtime event source. Construction is the producer's job
/// (the per-pane PTY threads, the input reader, the IPC server, the plugin
/// host, the timer); the dispatcher matches on the variant to decide what to
/// mutate and which [`koshi_core::event::Event`] facts to emit.
#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    /// Raw bytes a child process wrote to its PTY.
    PtyOutput {
        /// Pane whose child produced the output.
        pane_id: PaneId,
        /// The bytes read from the PTY, fed verbatim to the pane's terminal.
        bytes: Vec<u8>,
    },
    /// A child process ended.
    ChildExit {
        /// Pane whose child exited.
        pane_id: PaneId,
        /// How the child ended: an exit code or a terminating signal.
        status: ExitStatus,
        /// When the producer observed the exit, carried on the event so the
        /// handler never reads the clock itself.
        exited_at: SystemTime,
    },
    /// A client's outer terminal changed size.
    Resize {
        /// Client whose terminal was resized.
        client_id: ClientId,
        /// The client's new size in cells, before size reconciliation.
        size: Size,
    },
    /// A client joined a session and began viewing one of its tabs.
    ClientAttached {
        /// Session the client attached to.
        session_id: SessionId,
        /// The arriving client.
        client_id: ClientId,
        /// The client's terminal size in cells.
        viewport: Size,
        /// The tab the client begins viewing.
        active_tab: TabId,
        /// When the producer observed the attach, carried on the event so the
        /// handler never reads the clock itself.
        attached_at: SystemTime,
    },
    /// A client left, stopping its view of whatever tab it held.
    ClientDetached {
        /// The departing client.
        client_id: ClientId,
    },
    /// A periodic tick for time-driven refreshes such as cursor blink.
    Timer,
    /// A request to stop the event loop and shut the process down. Produced by
    /// the quit keybinding or by outer-input reaching end of stream.
    Quit,
    /// One decoded outer-terminal key awaiting keybinding resolution. Carries
    /// the chord alone: the bytes a fallthrough writes are encoded from it
    /// when they are written, against the receiving pane's mode.
    KeyInput {
        /// Client whose terminal produced the key.
        client_id: ClientId,
        /// Canonical chord used for keymap lookup.
        chord: KeyChord,
    },
    /// One decoded outer-terminal mouse event awaiting hit-testing and routing.
    MouseInput {
        /// Client whose terminal produced the mouse event.
        client_id: ClientId,
        /// The decoded event: kind, cell position, and modifiers.
        mouse: MouseInput,
    },
    /// Text the client's outer terminal pasted — the OS paste key pressed in
    /// the terminal koshi runs in, delivered whole so no character of it can
    /// fire a keybinding.
    HostPaste {
        /// Client whose terminal pasted.
        client_id: ClientId,
        /// The pasted text, exactly as the outer terminal delivered it.
        text: String,
    },
    /// A command delivered over the IPC socket, from external or in-session
    /// CLI. Carries the reply sender the connection thread waits on: the
    /// dispatcher sends the command's result into it, and the connection
    /// thread writes that result back over the socket.
    Ipc {
        /// The command as it arrived over the socket.
        envelope: CommandEnvelope,
        /// Where the dispatcher sends the command's result.
        reply: Sender<CommandResult>,
    },
    /// A discovery request delivered over the IPC socket: the caller asks
    /// this process to describe its session. Carries the reply sender the
    /// connection thread waits on; the dispatcher answers with the overview
    /// built from live state, or `None` when no session is running.
    IpcDiscovery {
        /// Where the dispatcher sends the overview.
        reply: Sender<Option<SessionOverview>>,
    },
    /// A capability-checked command issued by a plugin.
    Plugin(CommandEnvelope),
}

#[cfg(test)]
mod tests;
