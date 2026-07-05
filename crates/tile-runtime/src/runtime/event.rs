//! The runtime event inbox.
//!
//! [`RuntimeEvent`] is the single typed channel the dispatcher thread drains.
//! Every asynchronous trigger the runtime must react to — child output, child
//! exit, a client resize, a periodic tick, terminal input, an IPC command, a
//! plugin command — arrives as one variant, so the dispatcher consumes one
//! `std::sync::mpsc` inbox instead of a separate channel per source.
//!
//! These are *input* triggers, distinct from the *output* facts the dispatcher
//! emits ([`tile_core::event::Event`]): a [`RuntimeEvent::ChildExit`] is the raw
//! notification that a child died, while the emitted `PaneProcessExited` is the
//! resulting domain fact.
//!
//! The inbox stays in-process — producers send into it directly — so
//! `RuntimeEvent` is not `Serialize`, unlike the command and event vocabulary
//! that crosses the IPC socket.

use std::time::SystemTime;

use tile_core::{
    command::CommandEnvelope,
    geometry::Size,
    ids::{ClientId, PaneId},
    process::ExitStatus,
};

/// A trigger the dispatcher thread reacts to, drained from the runtime inbox.
///
/// One variant per runtime event source. Construction is the producer's job
/// (the per-pane PTY threads, the input reader, the IPC server, the plugin
/// host, the timer); the dispatcher matches on the variant to decide what to
/// mutate and which [`tile_core::event::Event`] facts to emit.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// A periodic tick for time-driven refreshes such as cursor blink.
    Timer,
    /// Raw input bytes read from a client's terminal, awaiting decoding.
    OuterInput {
        /// Client the input came from.
        client_id: ClientId,
        /// The raw terminal bytes, decoded later into a command or passthrough.
        bytes: Vec<u8>,
    },
    /// A command delivered over the IPC socket, from external or in-session CLI.
    Ipc(CommandEnvelope),
    /// A capability-checked command issued by a plugin.
    Plugin(CommandEnvelope),
}

#[cfg(test)]
mod tests;
