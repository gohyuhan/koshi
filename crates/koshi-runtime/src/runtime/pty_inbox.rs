//! Getting a pane's child output and exit to the dispatcher, and recording a
//! spawned pane's PTY.
//!
//! [`InboxSink`] puts each pane's child output and exit into that pane's own
//! [`PaneOutputQueue`]. A queue that goes from empty to holding something sends
//! one [`RuntimeEvent::PtyReadable`] into the inbox: the inbox holds at most
//! one wake per pane. The dispatcher takes the output back in turns of at most
//! [`PANE_OUTPUT_TURN_BYTE_LIMIT`] bytes.
//!
//! A queue holds at most [`PANE_OUTPUT_QUEUE_BYTE_LIMIT`] bytes before a push:
//! a reader that finds it full waits until the dispatcher takes bytes or the
//! pane closes. The reader then reads nothing, the kernel's terminal buffer
//! fills, and the child waits in `write`.
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Instant;

use koshi_core::ids::PaneId;
use koshi_core::process::{ExitStatus, PtySize};
use koshi_pty::backend::state::PtySink;
use koshi_terminal::engine::TerminalEngine;
use koshi_terminal::scrollback::ScrollbackLimit;

use crate::runtime::event::RuntimeEvent;
use crate::server::Server;

/// The byte count at which a pane's output queue is full: a push that finds
/// this many bytes or more queued waits, when its sink waits.
pub const PANE_OUTPUT_QUEUE_BYTE_LIMIT: usize = 128 * 1024;

/// The most bytes the dispatcher takes from one pane's queue in one turn. A
/// turn takes whole chunks, and always at least one.
pub const PANE_OUTPUT_TURN_BYTE_LIMIT: usize = 64 * 1024;

/// Each pane's output queue, by pane. An entry whose queue is dropped stays
/// until the next queue starts.
type PaneOutputQueueRegistry = Mutex<HashMap<PaneId, Weak<PaneOutputQueue>>>;

/// One pane's child output and exit, from its reader to the dispatcher.
pub struct PaneOutputQueue {
    /// The pane this queue carries.
    pane_id: PaneId,
    /// The chunks, their byte count, the exit, and the wake and close flags,
    /// read and written as one.
    queue_state: Mutex<PaneOutputQueueState>,
    /// Wakes a reader waiting for room when bytes are taken or the queue
    /// closes.
    room_condition: Condvar,
    /// The sink's registry, which a closed queue leaves.
    queue_registry: Weak<PaneOutputQueueRegistry>,
}

/// What one pane's output queue holds.
#[derive(Debug, Default)]
struct PaneOutputQueueState {
    /// The chunks not yet taken, oldest first.
    output_chunks: VecDeque<Vec<u8>>,
    /// The byte count of `output_chunks`.
    queued_byte_count: usize,
    /// The child's exit, taken after the last chunk.
    exit_status: Option<ExitStatus>,
    /// Set when a wake went to the inbox and the dispatcher has not yet found
    /// the queue empty since.
    is_wake_sent: bool,
    /// Set once the dispatcher let the pane go: a push that holds this queue
    /// returns `false` and queues nothing.
    is_closed: bool,
}

/// What the dispatcher takes from a pane's queue in one turn.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PaneOutputTurn {
    /// The chunks taken, oldest first, at most [`PANE_OUTPUT_TURN_BYTE_LIMIT`]
    /// bytes together unless the first chunk alone is larger.
    pub(crate) output_chunks: Vec<Vec<u8>>,
    /// The child's exit, taken only once no chunk is left.
    pub(crate) exit_status: Option<ExitStatus>,
    /// Whether the queue still holds chunks or an exit after this turn.
    pub(crate) has_more_output: bool,
}

impl fmt::Debug for PaneOutputQueue {
    /// Writes the pane id only: `PaneOutputQueue { pane_id: .. }`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PaneOutputQueue")
            .field("pane_id", &self.pane_id)
            .finish_non_exhaustive()
    }
}

impl PaneOutputQueue {
    /// Take one turn: the oldest chunks up to [`PANE_OUTPUT_TURN_BYTE_LIMIT`]
    /// bytes together, at least one, and the exit once no chunk is left. Wakes
    /// a reader waiting for room. A queue left empty clears its wake flag, and
    /// the next push sends a new wake.
    ///
    /// Example — chunks of 40 KiB, 30 KiB and 10 KiB give a turn of the 40 KiB
    /// chunk alone; the next turn takes the other two.
    pub(crate) fn take_output_turn(&self) -> PaneOutputTurn {
        let mut queue_state = self.queue_state.lock().expect("pane output queue");
        let mut output_chunks = Vec::new();
        let mut taken_byte_count = 0usize;
        while let Some(next_chunk_byte_count) = queue_state.output_chunks.front().map(Vec::len) {
            if !output_chunks.is_empty()
                && taken_byte_count + next_chunk_byte_count > PANE_OUTPUT_TURN_BYTE_LIMIT
            {
                break;
            }
            taken_byte_count += next_chunk_byte_count;
            output_chunks.extend(queue_state.output_chunks.pop_front());
        }
        queue_state.queued_byte_count -= taken_byte_count;
        let exit_status = if queue_state.output_chunks.is_empty() {
            queue_state.exit_status.take()
        } else {
            None
        };
        let has_more_output = !queue_state.output_chunks.is_empty();
        if !has_more_output {
            queue_state.is_wake_sent = false;
        }
        drop(queue_state);
        self.room_condition.notify_all();
        PaneOutputTurn {
            output_chunks,
            exit_status,
            has_more_output,
        }
    }

    /// Let the pane go: drop every chunk and the exit the queue holds, make
    /// every push that holds this queue return `false`, wake a reader waiting
    /// for room, and leave the sink's registry. A push for the same pane after
    /// this call finds no queue and starts a new one.
    pub(crate) fn close_output_queue(&self) {
        {
            let mut queue_state = self.queue_state.lock().expect("pane output queue");
            queue_state.is_closed = true;
            queue_state.output_chunks.clear();
            queue_state.queued_byte_count = 0;
            queue_state.exit_status = None;
        }
        self.room_condition.notify_all();
        let Some(queue_registry) = self.queue_registry.upgrade() else {
            return;
        };
        let mut output_queue_by_pane_id = queue_registry.lock().expect("pane output queues");
        if output_queue_by_pane_id
            .get(&self.pane_id)
            .is_some_and(|registered_queue| std::ptr::eq(registered_queue.as_ptr(), self))
        {
            output_queue_by_pane_id.remove(&self.pane_id);
        }
    }
}

/// A [`PtySink`] that puts each pane's child output and exit into that pane's
/// [`PaneOutputQueue`], and wakes the dispatcher through the runtime inbox.
pub struct InboxSink {
    /// The inbox each queue's wake is sent on.
    inbox_sender: Sender<RuntimeEvent>,
    /// Each pane's queue, held while the dispatcher, a wake in the inbox, or a
    /// push holds it.
    output_queue_by_pane_id: Arc<PaneOutputQueueRegistry>,
    /// Whether a push into a full queue waits for room. `false` only for a
    /// sink whose one caller serves every pane, and must never wait.
    should_wait_for_queue_room: bool,
}

impl InboxSink {
    /// A sink feeding `inbox_sender`, whose pushes into a full queue wait for
    /// room: each pane's reader thread is its only caller for that pane.
    #[must_use]
    pub fn from_event_sender(inbox_sender: Sender<RuntimeEvent>) -> Self {
        InboxSink {
            inbox_sender,
            output_queue_by_pane_id: Arc::default(),
            should_wait_for_queue_room: true,
        }
    }

    /// A sink feeding `inbox_sender` whose pushes never wait: one thread
    /// delivers every pane's output, and the far side bounds each pane's
    /// queue.
    #[must_use]
    pub fn from_event_sender_without_queue_wait(inbox_sender: Sender<RuntimeEvent>) -> Self {
        InboxSink {
            should_wait_for_queue_room: false,
            ..InboxSink::from_event_sender(inbox_sender)
        }
    }

    /// `pane_id`'s queue. A pane whose queue was dropped or never started gets
    /// a new empty queue, and every entry whose queue was dropped leaves the
    /// registry first.
    fn find_or_start_output_queue(&self, pane_id: PaneId) -> Arc<PaneOutputQueue> {
        let mut output_queue_by_pane_id = self
            .output_queue_by_pane_id
            .lock()
            .expect("pane output queues");
        if let Some(pane_output_queue) = output_queue_by_pane_id
            .get(&pane_id)
            .and_then(Weak::upgrade)
        {
            return pane_output_queue;
        }
        output_queue_by_pane_id.retain(|_, registered_queue| registered_queue.strong_count() > 0);
        let pane_output_queue = Arc::new(PaneOutputQueue {
            pane_id,
            queue_state: Mutex::default(),
            room_condition: Condvar::new(),
            queue_registry: Arc::downgrade(&self.output_queue_by_pane_id),
        });
        output_queue_by_pane_id.insert(pane_id, Arc::downgrade(&pane_output_queue));
        pane_output_queue
    }

    /// Send `pane_output_queue`'s wake. A closed inbox closes the queue and
    /// returns `false`.
    fn send_readable_wake(&self, pane_output_queue: Arc<PaneOutputQueue>) -> bool {
        let send_result = self
            .inbox_sender
            .send(RuntimeEvent::PtyReadable { pane_output_queue });
        let Err(send_error) = send_result else {
            return true;
        };
        if let RuntimeEvent::PtyReadable { pane_output_queue } = send_error.0 {
            pane_output_queue.close_output_queue();
        }
        false
    }
}

impl PtySink for InboxSink {
    /// Queue one chunk of child output on `pane_id`'s queue. While the queue
    /// holds [`PANE_OUTPUT_QUEUE_BYTE_LIMIT`] bytes or more, a waiting sink
    /// waits first. Returns `false` when the queue this push found closes
    /// before the chunk is queued, as while the push waits for room, or when
    /// the inbox is closed, which tells the reader to stop reading this pane.
    /// A push that starts after the pane's queue closed starts a new queue. An
    /// empty chunk queues nothing and returns `true`.
    fn accept_output_bytes(&self, pane_id: PaneId, output_bytes: Vec<u8>) -> bool {
        if output_bytes.is_empty() {
            return true;
        }
        let pane_output_queue = self.find_or_start_output_queue(pane_id);
        let should_send_wake = {
            let mut queue_state = pane_output_queue
                .queue_state
                .lock()
                .expect("pane output queue");
            while self.should_wait_for_queue_room
                && !queue_state.is_closed
                && queue_state.queued_byte_count >= PANE_OUTPUT_QUEUE_BYTE_LIMIT
            {
                queue_state = pane_output_queue
                    .room_condition
                    .wait(queue_state)
                    .expect("pane output queue");
            }
            if queue_state.is_closed {
                return false;
            }
            queue_state.queued_byte_count += output_bytes.len();
            queue_state.output_chunks.push_back(output_bytes);
            !std::mem::replace(&mut queue_state.is_wake_sent, true)
        };
        !should_send_wake || self.send_readable_wake(pane_output_queue)
    }

    /// Store the child's exit on `pane_id`'s queue, behind every chunk queued
    /// before it. The backend calls this after the pane's last output, and
    /// reads the pane no further. A closed queue or a closed inbox drops it.
    fn accept_exit_status(&self, pane_id: PaneId, exit_status: ExitStatus) {
        let pane_output_queue = self.find_or_start_output_queue(pane_id);
        let should_send_wake = {
            let mut queue_state = pane_output_queue
                .queue_state
                .lock()
                .expect("pane output queue");
            if queue_state.is_closed {
                return;
            }
            queue_state.exit_status = Some(exit_status);
            !std::mem::replace(&mut queue_state.is_wake_sent, true)
        };
        if should_send_wake {
            self.send_readable_wake(pane_output_queue);
        }
    }
}

impl Server {
    /// Take in `pane_output_queue`'s wake. The queue of a live pane is held,
    /// and the pane joins the end of the turn rotation. The queue of a pane
    /// that is not live is closed: its chunks and exit are dropped, and a push
    /// that holds the queue, as while it waits for room, returns `false`.
    pub(crate) fn handle_pty_readable(&mut self, pane_output_queue: Arc<PaneOutputQueue>) {
        let pane_id = pane_output_queue.pane_id;
        if !self.live_pane_ids.contains(&pane_id) {
            pane_output_queue.close_output_queue();
            return;
        }
        self.pane_output_queue_by_pane_id
            .insert(pane_id, pane_output_queue);
        self.ready_output_pane_ids.push_back(pane_id);
    }

    /// Give the first pane in the turn rotation one turn: feed each chunk of
    /// [`PaneOutputQueue::take_output_turn`] to the pane's engine
    /// ([`handle_pty_output`](Self::handle_pty_output)), report the turn's
    /// byte count to the backend
    /// ([`grant_pane_output`](koshi_pty::backend::state::PtyBackend::grant_pane_output)),
    /// then handle the exit the turn took
    /// ([`handle_child_exit`](Self::handle_child_exit)) and publish its
    /// events. Returns the pane when its queue still holds output, for the
    /// caller to put back at the end of the rotation. A pane whose queue the
    /// server no longer holds gets no turn.
    pub(crate) fn serve_output_turn(&mut self) -> Option<PaneId> {
        let pane_id = self.ready_output_pane_ids.pop_front()?;
        let pane_output_queue = Arc::clone(self.pane_output_queue_by_pane_id.get(&pane_id)?);
        let output_turn = pane_output_queue.take_output_turn();
        for output_chunk in &output_turn.output_chunks {
            self.handle_pty_output(pane_id, output_chunk);
        }
        let parsed_byte_count: usize = output_turn.output_chunks.iter().map(Vec::len).sum();
        self.get_pty_backend()
            .grant_pane_output(pane_id, parsed_byte_count);
        if let Some(exit_status) = output_turn.exit_status {
            let child_exit_events = self.handle_child_exit(pane_id, exit_status);
            self.publish_events(&child_exit_events);
        }
        output_turn.has_more_output.then_some(pane_id)
    }

    /// Whether the turn rotation holds a pane: a live pane whose queue holds
    /// output or an exit, or a removed pane whose turn has not come.
    #[must_use]
    pub fn has_queued_pane_output(&self) -> bool {
        !self.ready_output_pane_ids.is_empty()
    }

    /// Serve turns until no queue the server holds has output or an exit
    /// left. Wakes still in the inbox are not read: a pane whose wake waits
    /// there is not served.
    pub fn drain_pane_output_queues(&mut self) {
        while self.has_queued_pane_output() {
            if let Some(pane_id) = self.serve_output_turn() {
                self.ready_output_pane_ids.push_back(pane_id);
            }
        }
    }

    /// Close and drop `pane_id`'s output queue: its chunks and exit are
    /// dropped, and a push that holds the queue, as while it waits for room,
    /// returns `false`. A pane whose queue the server does not hold is passed
    /// over.
    pub(crate) fn remove_pane_output_queue(&mut self, pane_id: PaneId) {
        if let Some(pane_output_queue) = self.pane_output_queue_by_pane_id.remove(&pane_id) {
            pane_output_queue.close_output_queue();
        }
    }

    /// Register a freshly spawned pane's PTY: mark `pane_id` live, record
    /// `pty_size`, and add a new terminal engine of `pty_size` capped by the
    /// config's `scrollback.maximum_line_count` and
    /// `scrollback.maximum_byte_count`, as
    /// [`insert_terminal_engine`](Self::insert_terminal_engine) states. Every
    /// spawn path calls this. A record already held for `pane_id` is replaced.
    pub(crate) fn park_pane_pty(&mut self, pane_id: PaneId, pty_size: PtySize) {
        self.live_pane_ids.insert(pane_id);
        self.pty_size_by_pane_id.insert(pane_id, pty_size);
        let scrollback_config = &self.config.scrollback;
        let scrollback_limit = ScrollbackLimit::from_line_and_byte_limits(
            scrollback_config.maximum_line_count,
            scrollback_config.maximum_byte_count,
        );
        self.insert_terminal_engine(
            pane_id,
            TerminalEngine::with_scrollback(pty_size, scrollback_limit),
        );
    }

    /// Hold `terminal_engine` as `pane_id`'s engine, replacing any engine held
    /// for it, and record the engine's next timed renders
    /// ([`refresh_pane_render_deadlines`](Self::refresh_pane_render_deadlines)).
    /// While `pane-clipboard-writes` allows writes, the engine first starts
    /// reading the `OSC 52` copies of at most `max-copy-bytes` decoded bytes in
    /// the pane's output
    /// ([`TerminalEngine::start_reading_clipboard_copies`]).
    pub(crate) fn insert_terminal_engine(
        &mut self,
        pane_id: PaneId,
        mut terminal_engine: TerminalEngine,
    ) {
        let pane_clipboard_writes = self.config.pane_clipboard_writes;
        if pane_clipboard_writes.should_allow_writes {
            terminal_engine
                .start_reading_clipboard_copies(pane_clipboard_writes.maximum_copy_byte_count);
        }
        self.terminal_engine_by_pane_id
            .insert(pane_id, terminal_engine);
        self.refresh_pane_render_deadlines(pane_id, Instant::now());
    }
}

#[cfg(test)]
mod tests;
