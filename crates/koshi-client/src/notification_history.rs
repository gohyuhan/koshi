//! The notifications one viewer holds: the entries its own queue takes, and
//! the entries it takes from its session's queue, merged in one bounded
//! history.
//!
//! The viewer's own queue is a [`NotificationQueue`] owned by its client. The
//! session's queue arrives whole in the attach answer and in each `Resync`,
//! and one post at a time in each `NotificationPosted`. The history holds each
//! entry once, by its id, in the order the entries arrived, oldest first.
//!
//! Example: the session posts `pane …a1b2 killed by signal 9`, then this
//! viewer posts `core:resize-pane-left: below minimum size`. The history holds
//! both, the session's entry first. A `Resync` that carries the session's
//! entry again adds nothing.
//!
//! A viewer of a session on its own machine holds one entry per notice. When
//! the viewer and the session both post the `Warning`
//! `/home/user/.config/koshi/koshi.kdl: unknown key frobnicate`, the history
//! holds the session's entry only.
//!
//! The history also holds what the viewer shows of its entries: the open
//! notification center, the count of entries that arrived since the center
//! last opened, and the toast ([`NotificationToastQueue`]). Each entry that
//! arrives live, while the center is closed and toasts are on, queues a
//! preview: an entry the viewer posts, an entry a `NotificationPosted` brings,
//! and an entry newer than every entry taken before from the same session
//! queue. The entries of the first queue the viewer takes from a session
//! preview nothing.

use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime};

use koshi_core::ids::{ClientId, SessionId};
use koshi_core::notification::{
    NotificationEntry, NotificationEpoch, NotificationId, NotificationQueue,
    NotificationQueueError, NotificationQueueOwner, NotificationSeverity, NotificationSource,
    MAX_NOTIFICATION_TEXT_BYTE_COUNT,
};
use koshi_core::text::sanitize_text_to_byte_count;
use koshi_renderer::notification::{
    NotificationCenterEntry, NotificationToastContent, NotificationView,
};
use koshi_renderer::snapshot::NotificationCenter;

use crate::notification_toast::{NotificationToastQueue, ToastPreview, ToastTiming};

/// The most entries one viewer's history holds: 256. The next entry evicts the
/// oldest.
const MAX_MERGED_NOTIFICATION_COUNT: usize = 256;

/// The newest entry of one session queue epoch that the history took: its
/// sequence, and its repeat count when the history took it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MergedThrough {
    /// The entry's sequence.
    sequence: u64,
    /// The entry's `repeat_count` when the history took it.
    repeat_count: u32,
}

impl MergedThrough {
    /// Whether `entry` arrived after the entry this mark names: a higher
    /// sequence, or the same sequence with a higher repeat count. With
    /// `self` at sequence 4 and repeat count 2, sequence 5 and sequence 4 at
    /// repeat count 3 arrived after it; sequence 3, and sequence 4 at repeat
    /// count 2, did not.
    fn is_older_than(self, entry: &NotificationEntry) -> bool {
        entry.id.sequence > self.sequence
            || (entry.id.sequence == self.sequence && entry.repeat_count > self.repeat_count)
    }
}

/// What this viewer last took from its session's notification queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SessionQueueMark {
    /// The queue's owner: the session this viewer is attached to.
    owner: NotificationQueueOwner,
    /// The queue's epoch.
    epoch: NotificationEpoch,
    /// The queue's revision when this viewer last took from it.
    revision: u64,
    /// The newest entry of this epoch the history took, or `None` before it
    /// took one.
    merged_through: Option<MergedThrough>,
}

/// Why a notification queue or post from the session was not taken. Each one
/// leaves the history as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NotificationTakeError {
    /// The queue breaks its own bounds or holds another queue's entry.
    InvalidQueue(NotificationQueueError),
    /// The queue's owner is not the session this viewer is attached to.
    ForeignQueue {
        /// The owner the queue named.
        owner: NotificationQueueOwner,
    },
    /// A post arrived before any queue from the session.
    PostWithoutQueue {
        /// The id of the posted entry.
        notification_id: NotificationId,
    },
    /// A post names another owner or epoch than the queue this viewer took.
    ForeignPost {
        /// The id of the posted entry.
        notification_id: NotificationId,
    },
}

impl std::fmt::Display for NotificationTakeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotificationTakeError::InvalidQueue(queue_error) => write!(formatter, "{queue_error}"),
            NotificationTakeError::ForeignQueue { .. } => {
                formatter.write_str("the notification queue belongs to another session")
            }
            NotificationTakeError::PostWithoutQueue { .. } => formatter
                .write_str("a notification arrived before the session's notification queue"),
            NotificationTakeError::ForeignPost { .. } => {
                formatter.write_str("a notification names another queue than the session's")
            }
        }
    }
}

/// One viewer's notifications: its own queue, its mark on its session's
/// queue, the merged history, and what the viewer shows of it.
#[derive(Debug, Clone)]
pub(crate) struct NotificationHistory {
    /// The viewer's own queue, owned by its client.
    client_queue: NotificationQueue,
    /// What the viewer last took from its session's queue, or `None` before
    /// it took any.
    session_queue_mark: Option<SessionQueueMark>,
    /// Every entry the history holds, oldest arrival first, at most
    /// [`MAX_MERGED_NOTIFICATION_COUNT`]. No two share an id.
    merged_entries: VecDeque<NotificationCenterEntry>,
    /// How many entries the viewer can no longer show since the history last
    /// cleared: each entry the history evicted for room, and each entry a
    /// session queue evicted before this viewer took it.
    dropped_entry_count: u64,
    /// How many of the newest entries arrived since the center last opened,
    /// at most the entries the history holds. `0` while the center is open.
    unread_entry_count: usize,
    /// Whether the session runs on this viewer's machine. Such a history
    /// holds no entry of the viewer's own queue that has the severity and
    /// text of an entry from koshi it took from the session's queue.
    is_session_local: bool,
    /// The open notification center, or `None` while it is closed.
    notification_center: Option<NotificationCenter>,
    /// The toast and the previews that wait for it.
    notification_toast: NotificationToastQueue,
    /// Counts each change to what the center, the toast or the tabline's
    /// notification button draws, from `0`.
    revision: u64,
}

impl NotificationHistory {
    /// An empty history whose own queue `client_id` owns, in a new epoch,
    /// with the center closed and no toast.
    pub(crate) fn from_client_id(client_id: ClientId) -> Self {
        NotificationHistory {
            client_queue: NotificationQueue::from_owner(NotificationQueueOwner::Client(client_id)),
            session_queue_mark: None,
            merged_entries: VecDeque::new(),
            dropped_entry_count: 0,
            unread_entry_count: 0,
            is_session_local: false,
            notification_center: None,
            notification_toast: NotificationToastQueue::default(),
            revision: 0,
        }
    }

    /// Record whether the session runs on this viewer's machine. A history
    /// starts with `false`. The entries the history holds stay as they are.
    pub(crate) fn set_session_local(&mut self, is_session_local: bool) {
        self.is_session_local = is_session_local;
    }

    /// Record `client_id` as the owner of the viewer's own queue. A
    /// `client_id` other than the queue's owner starts a new, empty own queue
    /// in a new epoch; the history keeps every entry it holds.
    ///
    /// Example: a viewer rejoins as a new client after a session restart. Its
    /// next post takes sequence 0 under the new client, and the entries it
    /// posted as the old client stay in the history.
    pub(crate) fn set_client_id(&mut self, client_id: ClientId) {
        let owner = NotificationQueueOwner::Client(client_id);
        if self.client_queue.get_owner() != owner {
            self.client_queue = NotificationQueue::from_owner(owner);
        }
    }

    /// Every entry the history holds, oldest arrival first.
    pub(crate) fn list_merged_entries(&self) -> impl Iterator<Item = &NotificationEntry> {
        self.merged_entries
            .iter()
            .map(NotificationCenterEntry::get_entry)
    }

    /// How many of the newest entries arrived since the center last opened.
    #[must_use]
    pub(crate) fn get_unread_entry_count(&self) -> usize {
        self.unread_entry_count
    }

    /// The count of changes to what the center, the toast or the
    /// notification button draws.
    #[must_use]
    pub(crate) fn get_revision(&self) -> u64 {
        self.revision
    }

    /// The open center, or `None` while it is closed.
    #[must_use]
    pub(crate) fn get_notification_center(&self) -> Option<NotificationCenter> {
        self.notification_center
    }

    /// Open the center, scrolled to its top and holding the viewer's
    /// keyboard. Every entry counts as read, and every toast preview goes
    /// ([`NotificationToastQueue::clear_previews`]). Does nothing while the
    /// center is open.
    pub(crate) fn open_notification_center(&mut self) {
        if self.notification_center.is_some() {
            return;
        }
        self.notification_center = Some(NotificationCenter {
            scroll_row_offset: 0,
            is_focused: true,
        });
        self.unread_entry_count = 0;
        self.notification_toast.clear_previews();
        self.revision += 1;
    }

    /// Close the center. Its entries stay. Does nothing while it is closed.
    pub(crate) fn close_notification_center(&mut self) {
        if self.notification_center.take().is_some() {
            self.revision += 1;
        }
    }

    /// Give the open center the viewer's keyboard with `is_focused`, or take
    /// it away. Does nothing while the center is closed.
    pub(crate) fn set_notification_center_focus(&mut self, is_focused: bool) {
        if let Some(notification_center) = &mut self.notification_center {
            if notification_center.is_focused != is_focused {
                notification_center.is_focused = is_focused;
                self.revision += 1;
            }
        }
    }

    /// Draw the open center's body from row `scroll_row_offset`. Does nothing
    /// while the center is closed.
    pub(crate) fn set_notification_center_scroll_row_offset(&mut self, scroll_row_offset: usize) {
        if let Some(notification_center) = &mut self.notification_center {
            if notification_center.scroll_row_offset != scroll_row_offset {
                notification_center.scroll_row_offset = scroll_row_offset;
                self.revision += 1;
            }
        }
    }

    /// Remove every entry, the dropped count and the unread count, close the
    /// center and drop every toast preview. The marks on the viewer's own
    /// queue and on the session's queue stay: a queue or post of the same
    /// epoch that arrives after the clear adds only entries newer than every
    /// entry taken before.
    ///
    /// Example: after a clear, a `Resync` that carries the session's entry
    /// `pane …a1b2 killed by signal 9` again adds nothing, and the next new
    /// entry of the same queue is the only entry the center lists.
    pub(crate) fn clear_entries(&mut self) {
        self.merged_entries.clear();
        self.dropped_entry_count = 0;
        self.unread_entry_count = 0;
        self.notification_center = None;
        self.notification_toast.clear_previews();
        self.revision += 1;
    }

    /// What the center and the toast draw: every entry, the dropped count,
    /// and the shown toast preview. A summary preview reads as `[Koshi]` at
    /// the loudest severity among the entries it counts.
    #[must_use]
    pub(crate) fn build_notification_view(&self) -> NotificationView<'_> {
        NotificationView {
            entries: &self.merged_entries,
            dropped_entry_count: self.dropped_entry_count,
            toast_content: self.find_toast_content(),
        }
    }

    /// The shown toast preview as the toast draws it, and the thousandths of
    /// its width past its side edge. `None` while no toast shows.
    #[must_use]
    pub(crate) fn find_shown_toast(&self) -> Option<(NotificationToastContent<'_>, u16)> {
        let hidden_per_mille = self.notification_toast.find_shown_preview()?.1;
        Some((self.find_toast_content()?, hidden_per_mille))
    }

    /// Advance the toast's clock to `current_time` with the phase lengths of
    /// `toast_timing`, stopped while `is_paused`
    /// ([`NotificationToastQueue::advance`]).
    pub(crate) fn advance_notification_toast(
        &mut self,
        current_time: Instant,
        toast_timing: ToastTiming,
        is_paused: bool,
    ) {
        if self
            .notification_toast
            .advance(current_time, toast_timing, is_paused)
        {
            self.revision += 1;
        }
    }

    /// How long from `current_time` until the toast's clock next has
    /// something to move ([`NotificationToastQueue::find_next_wakeup`]).
    #[must_use]
    pub(crate) fn find_next_toast_wakeup(
        &self,
        current_time: Instant,
        toast_timing: ToastTiming,
    ) -> Option<Duration> {
        self.notification_toast
            .find_next_wakeup(current_time, toast_timing)
    }

    /// Withdraw the shown toast from where it stands
    /// ([`NotificationToastQueue::request_withdraw`]). Its entry stays.
    pub(crate) fn request_toast_withdraw(&mut self) {
        self.notification_toast.request_withdraw();
    }

    /// Post `text` from koshi in this viewer at `severity`, arriving at
    /// `created_at`, to the viewer's own queue
    /// ([`NotificationQueue::post_core_entry`]), and take the entry the post
    /// made or changed as the newest arrival. With `should_show_toasts`, the
    /// arrival queues a toast preview while the center is closed.
    ///
    /// Example: two posts of `core:resize-pane-left: below minimum size` give
    /// one entry with `repeat_count` 2.
    ///
    /// A history of a local session that holds a session entry from koshi
    /// with `severity` and the sanitized `text` posts nothing.
    pub(crate) fn post_core_entry(
        &mut self,
        severity: NotificationSeverity,
        text: &str,
        created_at: SystemTime,
        should_show_toasts: bool,
    ) {
        if self.is_session_local {
            let sanitized_text =
                sanitize_text_to_byte_count(text, MAX_NOTIFICATION_TEXT_BYTE_COUNT);
            let is_held_by_session = self.list_merged_entries().any(|merged_entry| {
                matches!(merged_entry.id.owner, NotificationQueueOwner::Session(_))
                    && is_core_notice(merged_entry, severity, &sanitized_text)
            });
            if is_held_by_session {
                return;
            }
        }
        let posted_entry = self
            .client_queue
            .post_core_entry(severity, text, created_at)
            .clone();
        let held_entry_index = self.find_merged_entry_index(posted_entry.id);
        self.take_arrival(posted_entry, held_entry_index, should_show_toasts);
    }

    /// Take `notification_queue`, the queue of session `session_id`, as an
    /// attach answer or a `Resync` carries it.
    ///
    /// A queue at a revision below the last one taken from the same owner and
    /// epoch is left untaken. Otherwise each entry the history holds is
    /// replaced where it stands, and each entry newer than every entry taken
    /// from that epoch is added, oldest first. An entry of that epoch the
    /// history no longer holds, and that is not newer, stays out. A queue of
    /// another epoch keeps every entry the history holds and adds all of its
    /// own.
    ///
    /// Each taken entry's text loses every character
    /// [`sanitize_text_to_byte_count`] removes and is cut to
    /// [`MAX_NOTIFICATION_TEXT_BYTE_COUNT`] bytes. In a history of a local
    /// session, each entry from koshi added removes the viewer's own entries
    /// with its severity and text.
    ///
    /// The dropped count adds the entries the queue evicted before this
    /// viewer took them: the sequences between the newest entry taken from
    /// the epoch, or the epoch's start, and the queue's next newer entry. A
    /// first queue whose oldest entry has sequence 40 adds 40.
    ///
    /// With `should_show_toasts`, each added entry queues a toast preview
    /// while the center is closed, except in the first queue taken from its
    /// owner.
    ///
    /// # Errors
    /// [`NotificationTakeError::InvalidQueue`] when the queue fails
    /// [`NotificationQueue::validate_entries`], and
    /// [`NotificationTakeError::ForeignQueue`] when its owner is not session
    /// `session_id`. Neither changes the history.
    pub(crate) fn take_session_queue(
        &mut self,
        session_id: SessionId,
        notification_queue: &NotificationQueue,
        should_show_toasts: bool,
    ) -> Result<(), NotificationTakeError> {
        notification_queue
            .validate_entries()
            .map_err(NotificationTakeError::InvalidQueue)?;
        let owner = notification_queue.get_owner();
        if owner != NotificationQueueOwner::Session(session_id) {
            return Err(NotificationTakeError::ForeignQueue { owner });
        }
        let epoch = notification_queue.get_epoch();
        let is_first_queue_of_owner = self
            .session_queue_mark
            .is_none_or(|session_queue_mark| session_queue_mark.owner != owner);
        let should_preview_new_entries = should_show_toasts && !is_first_queue_of_owner;
        let mut merged_through = None;
        if let Some(session_queue_mark) = self.session_queue_mark {
            if session_queue_mark.owner == owner && session_queue_mark.epoch == epoch {
                if notification_queue.get_revision() < session_queue_mark.revision {
                    return Ok(());
                }
                merged_through = session_queue_mark.merged_through;
            }
        }
        let mut next_expected_sequence = merged_through
            .map_or(0, |merged_through: MergedThrough| {
                merged_through.sequence + 1
            });
        for queued_entry in notification_queue.list_entries() {
            let received_entry = sanitize_received_entry(queued_entry.clone());
            self.count_skipped_sequences(&received_entry, &mut next_expected_sequence);
            let held_entry_index = self.find_merged_entry_index(received_entry.id);
            let is_new = merged_through
                .is_none_or(|merged_through| merged_through.is_older_than(&received_entry));
            if is_new {
                merged_through = Some(MergedThrough {
                    sequence: received_entry.id.sequence,
                    repeat_count: received_entry.repeat_count,
                });
            }
            match held_entry_index {
                Some(entry_index) => {
                    self.notification_toast.update_entry(&received_entry);
                    self.merged_entries[entry_index] =
                        NotificationCenterEntry::from_entry(received_entry);
                    self.revision += 1;
                }
                None if is_new => {
                    self.push_merged_entry(received_entry, should_preview_new_entries);
                }
                None => {}
            }
        }
        self.session_queue_mark = Some(SessionQueueMark {
            owner,
            epoch,
            revision: notification_queue.get_revision(),
            merged_through,
        });
        Ok(())
    }

    /// Take one `NotificationPosted` from the session: `entry` at the queue's
    /// `revision`.
    ///
    /// A post at a revision not above the last one taken from the queue is
    /// dropped. Otherwise the entry, its text sanitized as
    /// [`take_session_queue`](Self::take_session_queue) states, is taken as
    /// the newest arrival: an entry the history holds under its id moves to
    /// the newest place, and an entry newer than every entry taken from the
    /// queue's epoch is added there. In a history of a local session, an
    /// entry from koshi removes the viewer's own entries with its severity
    /// and text. The dropped count adds the sequences the post skips, as
    /// [`take_session_queue`](Self::take_session_queue) counts them. With
    /// `should_show_toasts`, the arrival queues a toast preview while the
    /// center is closed.
    ///
    /// # Errors
    /// [`NotificationTakeError::PostWithoutQueue`] before any queue was
    /// taken, and [`NotificationTakeError::ForeignPost`] when the entry's id
    /// names another owner or epoch than the queue taken last. Neither
    /// changes the history.
    pub(crate) fn take_session_post(
        &mut self,
        revision: u64,
        entry: NotificationEntry,
        should_show_toasts: bool,
    ) -> Result<(), NotificationTakeError> {
        let Some(session_queue_mark) = self.session_queue_mark.as_mut() else {
            return Err(NotificationTakeError::PostWithoutQueue {
                notification_id: entry.id,
            });
        };
        if entry.id.owner != session_queue_mark.owner || entry.id.epoch != session_queue_mark.epoch
        {
            return Err(NotificationTakeError::ForeignPost {
                notification_id: entry.id,
            });
        }
        if revision <= session_queue_mark.revision {
            return Ok(());
        }
        session_queue_mark.revision = revision;
        let merged_through = session_queue_mark.merged_through;
        let is_new =
            merged_through.is_none_or(|merged_through| merged_through.is_older_than(&entry));
        if is_new {
            session_queue_mark.merged_through = Some(MergedThrough {
                sequence: entry.id.sequence,
                repeat_count: entry.repeat_count,
            });
        }
        let mut next_expected_sequence =
            merged_through.map_or(0, |merged_through| merged_through.sequence + 1);
        self.count_skipped_sequences(&entry, &mut next_expected_sequence);
        let held_entry_index = self.find_merged_entry_index(entry.id);
        if is_new || held_entry_index.is_some() {
            self.take_arrival(
                sanitize_received_entry(entry),
                held_entry_index,
                should_show_toasts,
            );
        }
        Ok(())
    }

    /// Add the sequences between `next_expected_sequence` and `entry`'s to the
    /// dropped count when `entry`'s sequence is at least
    /// `next_expected_sequence`, and expect the sequence after `entry`'s
    /// next. With `5` expected, an entry at sequence 8 adds 3, and an entry at
    /// sequence 4 adds nothing.
    fn count_skipped_sequences(
        &mut self,
        entry: &NotificationEntry,
        next_expected_sequence: &mut u64,
    ) {
        let entry_sequence = entry.id.sequence;
        if entry_sequence < *next_expected_sequence {
            return;
        }
        let skipped_sequence_count = entry_sequence - *next_expected_sequence;
        if skipped_sequence_count > 0 {
            self.dropped_entry_count += skipped_sequence_count;
            self.revision += 1;
        }
        *next_expected_sequence = entry_sequence + 1;
    }

    /// Put `entry` at the newest place ([`push_merged_entry`](Self::push_merged_entry)).
    /// `held_entry_index` is the index of the entry held under `entry`'s id,
    /// or `None` when the history holds none. A held entry moves there with
    /// `entry`'s fields, and any other entry is added there. With
    /// `should_show_toasts`, the arrival queues a toast preview while the
    /// center is closed.
    fn take_arrival(
        &mut self,
        entry: NotificationEntry,
        held_entry_index: Option<usize>,
        should_show_toasts: bool,
    ) {
        if let Some(entry_index) = held_entry_index {
            self.remove_merged_entry_at(entry_index);
        }
        self.push_merged_entry(entry, should_show_toasts);
    }

    /// Add `entry` at the newest place, as an unread entry while the center is
    /// closed.
    ///
    /// In a history of a local session, a session `entry` from koshi first
    /// removes every entry of the viewer's own queue from koshi with its
    /// severity and text, and their toast previews show `entry`
    /// ([`NotificationToastQueue::replace_entries`]). A history then holding
    /// [`MAX_MERGED_NOTIFICATION_COUNT`] entries evicts its oldest
    /// ([`evict_oldest_entry`](Self::evict_oldest_entry)).
    ///
    /// With `should_preview` and the center closed, `entry` queues a toast
    /// preview ([`NotificationToastQueue::queue_entry`]). Otherwise a preview
    /// that shows `entry`'s id takes `entry` as its copy.
    fn push_merged_entry(&mut self, entry: NotificationEntry, should_preview: bool) {
        if self.is_session_local
            && matches!(entry.id.owner, NotificationQueueOwner::Session(_))
            && entry.source == NotificationSource::Core
        {
            let mut replaced_ids = Vec::new();
            let mut entry_index = 0;
            while entry_index < self.merged_entries.len() {
                let merged_entry = self.merged_entries[entry_index].get_entry();
                if matches!(merged_entry.id.owner, NotificationQueueOwner::Client(_))
                    && is_core_notice(merged_entry, entry.severity, &entry.text)
                {
                    replaced_ids.push(merged_entry.id);
                    self.remove_merged_entry_at(entry_index);
                } else {
                    entry_index += 1;
                }
            }
            if !replaced_ids.is_empty() {
                self.notification_toast
                    .replace_entries(&replaced_ids, &entry);
            }
        }
        if self.merged_entries.len() == MAX_MERGED_NOTIFICATION_COUNT {
            self.evict_oldest_entry();
        }
        let is_center_open = self.notification_center.is_some();
        if should_preview && !is_center_open {
            self.notification_toast.queue_entry(&entry);
        } else {
            self.notification_toast.update_entry(&entry);
        }
        if !is_center_open {
            self.unread_entry_count += 1;
        }
        self.merged_entries
            .push_back(NotificationCenterEntry::from_entry(entry));
        self.revision += 1;
    }

    /// Evict the oldest entry: the dropped count adds 1, and its toast
    /// previews go ([`NotificationToastQueue::remove_entry`]).
    fn evict_oldest_entry(&mut self) {
        let Some(evicted_entry) = self.remove_merged_entry_at(0) else {
            return;
        };
        self.dropped_entry_count += 1;
        self.notification_toast
            .remove_entry(evicted_entry.get_entry().id);
    }

    /// Remove the entry at `entry_index` and return it, or `None` past the
    /// newest entry. An unread entry takes 1 from the unread count.
    fn remove_merged_entry_at(&mut self, entry_index: usize) -> Option<NotificationCenterEntry> {
        let first_unread_index = self.merged_entries.len() - self.unread_entry_count;
        let removed_entry = self.merged_entries.remove(entry_index)?;
        if entry_index >= first_unread_index {
            self.unread_entry_count -= 1;
        }
        self.revision += 1;
        Some(removed_entry)
    }

    /// The shown toast preview as the toast draws it, or `None` while no
    /// toast shows.
    fn find_toast_content(&self) -> Option<NotificationToastContent<'_>> {
        let (toast_preview, _) = self.notification_toast.find_shown_preview()?;
        match toast_preview {
            ToastPreview::Entry(entry) => Some(NotificationToastContent::Entry(entry)),
            ToastPreview::Summary(summary_members) => Some(NotificationToastContent::Summary {
                notification_count: summary_members.len(),
                severity: summary_members
                    .iter()
                    .map(|(_, member_severity)| *member_severity)
                    .max()?,
            }),
        }
    }

    /// Where the entry with `notification_id` stands in the history, or
    /// `None` when the history holds none.
    fn find_merged_entry_index(&self, notification_id: NotificationId) -> Option<usize> {
        self.list_merged_entries()
            .position(|merged_entry| merged_entry.id == notification_id)
    }
}

/// `entry` with its text stripped of every character
/// [`sanitize_text_to_byte_count`] removes and cut to
/// [`MAX_NOTIFICATION_TEXT_BYTE_COUNT`] bytes.
fn sanitize_received_entry(mut entry: NotificationEntry) -> NotificationEntry {
    entry.text = sanitize_text_to_byte_count(&entry.text, MAX_NOTIFICATION_TEXT_BYTE_COUNT);
    entry
}

/// Whether `entry` comes from koshi with `severity` and `text`.
fn is_core_notice(entry: &NotificationEntry, severity: NotificationSeverity, text: &str) -> bool {
    entry.source == NotificationSource::Core && entry.severity == severity && entry.text == text
}

#[cfg(test)]
mod tests;
