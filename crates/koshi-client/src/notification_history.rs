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
//! The history also holds what the viewer shows of its entries: the
//! notification center with its selected card and its slide, the count of
//! entries that arrived since the center last opened, and the toast
//! ([`NotificationToastQueue`]). Each entry that
//! arrives live, while the center is closed and toasts are on, queues a
//! preview: an entry the viewer posts, an entry a `NotificationPosted` brings,
//! and an entry newer than every entry taken before from the same session
//! queue. The entries of the first queue the viewer takes from a session
//! preview nothing.

use std::num::NonZeroU32;
use std::time::{Duration, Instant, SystemTime};

use koshi_core::ids::{ClientId, SessionId};
use koshi_core::notification::{
    NotificationEntry, NotificationEpoch, NotificationId, NotificationQueue,
    NotificationQueueError, NotificationQueueOwner, NotificationSeverity, NotificationSource,
    MAX_NOTIFICATION_TEXT_BYTE_COUNT, MAX_QUEUED_NOTIFICATION_COUNT,
};
use koshi_core::text::sanitize_text_to_byte_count;
use koshi_renderer::notification::{
    NotificationCenterEntries, NotificationCenterEntry, NotificationToastContent,
    NotificationToastView, NotificationView, ToastTextRowCache,
};
use koshi_renderer::snapshot::NotificationCenter;

use crate::attach::{compute_placement_animation_progress, PLACEMENT_ANIMATION_DURATION};
use crate::notification_toast::{NotificationToastQueue, ToastPreview, ToastTiming};

/// The most entries one viewer's history holds: 256. The next entry evicts the
/// oldest.
const MAX_MERGED_NOTIFICATION_COUNT: usize = 256;

/// The thousandths of the center's width past the pane area's right edge
/// while it is fully out of view: 1000.
const FULLY_HIDDEN_PER_MILLE: u16 = 1000;

/// The notification center while it is open or slides out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NotificationCenterState {
    /// The body row drawn on the center's first body row, counted from `0`.
    scroll_row_offset: usize,
    /// The entry whose card the center's keys act on, or `None` while the
    /// history holds no entry.
    selected_notification_id: Option<NotificationId>,
    /// Where the center's slide stands.
    slide: NotificationCenterSlide,
}

/// Where the center's slide in or out stands.
///
/// A slide runs [`PLACEMENT_ANIMATION_DURATION`] with an ease-out, from
/// `start_hidden_per_mille` to `0` while the center opens and to `1000`
/// while it closes. It starts at the first advance of the clock after it was
/// asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NotificationCenterSlide {
    /// Whether the center slides out to close. A closing center takes no key
    /// and no click.
    is_closing: bool,
    /// The thousandths of the width past the edge when this slide started.
    start_hidden_per_mille: u16,
    /// When this slide started, or `None` before the clock first advanced
    /// with it.
    started_at: Option<Instant>,
    /// The thousandths of the width past the edge when the clock last
    /// advanced, from `0` to `1000`.
    hidden_per_mille: u16,
}

impl NotificationCenterSlide {
    /// A slide toward the place `is_closing` names, from
    /// `start_hidden_per_mille`, that starts at the next advance of the
    /// clock.
    fn from_start(is_closing: bool, start_hidden_per_mille: u16) -> Self {
        NotificationCenterSlide {
            is_closing,
            start_hidden_per_mille,
            started_at: None,
            hidden_per_mille: start_hidden_per_mille,
        }
    }

    /// The thousandths past the edge this slide ends at: `1000` while
    /// closing, and `0` while opening.
    fn get_end_hidden_per_mille(&self) -> u16 {
        if self.is_closing {
            FULLY_HIDDEN_PER_MILLE
        } else {
            0
        }
    }
}

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
    /// Whether `received_entry` arrived after the entry this mark names: a
    /// higher sequence, or the same sequence with a higher repeat count. With
    /// `self` at sequence 4 and repeat count 2, sequence 5 and sequence 4 at
    /// repeat count 3 arrived after it; sequence 3, and sequence 4 at repeat
    /// count 2, did not.
    fn is_older_than(self, received_entry: &NotificationEntry) -> bool {
        received_entry.notification_id.sequence > self.sequence
            || (received_entry.notification_id.sequence == self.sequence
                && received_entry.repeat_count > self.repeat_count)
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
    /// A `Resync` carries more than [`MAX_QUEUED_NOTIFICATION_COUNT`] runs of
    /// held command rejections.
    TooManyHeldRejections {
        /// How many runs it carries.
        held_rejection_count: usize,
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
            NotificationTakeError::TooManyHeldRejections {
                held_rejection_count,
            } => write!(
                formatter,
                "the session held {held_rejection_count} runs of command rejections, more than \
                 {MAX_QUEUED_NOTIFICATION_COUNT}"
            ),
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
    /// [`MAX_MERGED_NOTIFICATION_COUNT`], with the center's body rows last
    /// asked for. No two share an id.
    merged_entries: NotificationCenterEntries,
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
    /// The notification center while it is open or slides out, or `None`
    /// while it is closed.
    notification_center: Option<NotificationCenterState>,
    /// The toast and the previews that wait for it.
    notification_toast: NotificationToastQueue,
    /// The rows of the shown toast's text last asked for.
    toast_text_row_cache: ToastTextRowCache,
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
            merged_entries: NotificationCenterEntries::default(),
            dropped_entry_count: 0,
            unread_entry_count: 0,
            is_session_local: false,
            notification_center: None,
            notification_toast: NotificationToastQueue::default(),
            toast_text_row_cache: ToastTextRowCache::default(),
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
            .list_center_entries()
            .map(NotificationCenterEntry::get_notification_entry)
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

    /// The center as a frame draws it while it is open or slides out, or
    /// `None` while it is closed.
    #[must_use]
    pub(crate) fn get_notification_center(&self) -> Option<NotificationCenter> {
        self.notification_center
            .map(|center_state| NotificationCenter {
                scroll_row_offset: center_state.scroll_row_offset,
                selected_notification_id: center_state.selected_notification_id,
                hidden_per_mille: center_state.slide.hidden_per_mille,
            })
    }

    /// The center's whole state, to put back with
    /// [`set_notification_center_state`](Self::set_notification_center_state):
    /// `None` while the center is closed.
    #[must_use]
    pub(crate) fn get_notification_center_state(&self) -> Option<NotificationCenterState> {
        self.notification_center
    }

    /// Put back the center's whole state `notification_center_state`, as
    /// [`get_notification_center_state`](Self::get_notification_center_state)
    /// gave it.
    pub(crate) fn set_notification_center_state(
        &mut self,
        notification_center_state: Option<NotificationCenterState>,
    ) {
        if self.notification_center != notification_center_state {
            self.notification_center = notification_center_state;
            self.revision += 1;
        }
    }

    /// Whether the center is open: shown and not sliding out. Only an open
    /// center takes the viewer's keys and clicks.
    #[must_use]
    pub(crate) fn is_notification_center_open(&self) -> bool {
        self.notification_center
            .is_some_and(|center_state| !center_state.slide.is_closing)
    }

    /// Open the center, scrolled to its top with its newest entry selected.
    /// Every entry counts as read, and every toast preview goes
    /// ([`NotificationToastQueue::clear_previews`]). Does nothing while the
    /// center is open.
    ///
    /// With `should_reduce_motion`, the center shows in place at once.
    /// Otherwise it slides in from where it stands: from past the edge when
    /// it was closed, and from where a slide out left it.
    pub(crate) fn open_notification_center(&mut self, should_reduce_motion: bool) {
        let start_hidden_per_mille = match self.notification_center {
            Some(center_state) if !center_state.slide.is_closing => return,
            Some(center_state) => center_state.slide.hidden_per_mille,
            None => FULLY_HIDDEN_PER_MILLE,
        };
        let slide_start_hidden_per_mille = if should_reduce_motion {
            0
        } else {
            start_hidden_per_mille
        };
        self.notification_center = Some(NotificationCenterState {
            scroll_row_offset: 0,
            selected_notification_id: self
                .merged_entries
                .find_newest_center_entry()
                .map(|newest_entry| newest_entry.get_notification_entry().notification_id),
            slide: NotificationCenterSlide::from_start(false, slide_start_hidden_per_mille),
        });
        self.unread_entry_count = 0;
        self.notification_toast.clear_previews();
        self.revision += 1;
    }

    /// Close the center. Its entries stay. With `should_reduce_motion`, the
    /// center goes at once. Otherwise it slides out from where it stands and
    /// goes when the slide ends. Does nothing while the center is closed or
    /// slides out.
    pub(crate) fn close_notification_center(&mut self, should_reduce_motion: bool) {
        let Some(center_state) = &mut self.notification_center else {
            return;
        };
        if center_state.slide.is_closing {
            return;
        }
        if should_reduce_motion {
            self.notification_center = None;
        } else {
            center_state.slide =
                NotificationCenterSlide::from_start(true, center_state.slide.hidden_per_mille);
        }
        self.revision += 1;
    }

    /// Remove the center at once, with no slide, whether it is open or slides
    /// out. Does nothing while it is closed.
    pub(crate) fn remove_notification_center(&mut self) {
        if self.notification_center.take().is_some() {
            self.revision += 1;
        }
    }

    /// Draw the open center's body from row `scroll_row_offset`. Does nothing
    /// while the center is not open.
    pub(crate) fn set_notification_center_scroll_row_offset(&mut self, scroll_row_offset: usize) {
        let Some(center_state) = self.find_open_center_state() else {
            return;
        };
        if center_state.scroll_row_offset != scroll_row_offset {
            center_state.scroll_row_offset = scroll_row_offset;
            self.revision += 1;
        }
    }

    /// Select the card one entry newer than the selected one with
    /// `is_selecting_newer`, and one entry older otherwise. Returns the newly
    /// selected entry's id, or `None` when the selection did not move: the
    /// center is not open, it selects no entry, or the selected entry is the
    /// newest or the oldest in that direction.
    ///
    /// With entries `a`, `b` and `c`, oldest first, and `b` selected, a newer
    /// step selects `c`, and a newer step from `c` gives `None`.
    pub(crate) fn select_adjacent_entry(
        &mut self,
        is_selecting_newer: bool,
    ) -> Option<NotificationId> {
        let selected_notification_id = self.find_open_center_state()?.selected_notification_id?;
        let selected_entry_index = self.find_merged_entry_index(selected_notification_id)?;
        let next_entry_index = if is_selecting_newer {
            selected_entry_index + 1
        } else {
            selected_entry_index.checked_sub(1)?
        };
        let next_notification_id = self
            .merged_entries
            .find_center_entry_at(next_entry_index)?
            .get_notification_entry()
            .notification_id;
        self.set_selected_notification_id(Some(next_notification_id));
        Some(next_notification_id)
    }

    /// Select the card of the entry `notification_id`. Does nothing while the
    /// center is not open or the history holds no such entry.
    pub(crate) fn select_entry(&mut self, notification_id: NotificationId) {
        if self.is_notification_center_open()
            && self.find_merged_entry_index(notification_id).is_some()
        {
            self.set_selected_notification_id(Some(notification_id));
        }
    }

    /// Remove the entry `notification_id` and its toast previews. The dropped
    /// count stays, and the marks on the queues stay: a queue or post that
    /// carries the entry again adds it only as a newer arrival, such as a
    /// repeat. A selected entry moves the selection to the next older entry,
    /// else the next newer one
    /// ([`remove_merged_entry_at`](Self::remove_merged_entry_at)). Does
    /// nothing when the history holds no such entry.
    ///
    /// Example: with entries `a`, `b` and `c`, oldest first, and `b`
    /// selected, dismissing `b` leaves `a` and `c` with `a` selected.
    pub(crate) fn dismiss_merged_entry(&mut self, notification_id: NotificationId) {
        let Some(merged_entry_index) = self.find_merged_entry_index(notification_id) else {
            return;
        };
        self.remove_merged_entry_at(merged_entry_index);
        self.notification_toast.remove_previews(notification_id);
    }

    /// Remove every entry, the dropped count and the unread count, and drop
    /// every toast preview. An open center selects no entry and scrolls to
    /// its top. The marks on the viewer's own queue and on the session's queue
    /// stay: a queue or post of the same epoch that arrives after the clear
    /// adds only entries newer than every entry taken before.
    ///
    /// Example: after a clear, a `Resync` that carries the session's entry
    /// `pane …a1b2 killed by signal 9` again adds nothing, and the next new
    /// entry of the same queue is the only entry the center lists.
    pub(crate) fn clear_merged_entries(&mut self) {
        self.merged_entries.clear_center_entries();
        self.dropped_entry_count = 0;
        self.unread_entry_count = 0;
        if let Some(center_state) = &mut self.notification_center {
            center_state.scroll_row_offset = 0;
            center_state.selected_notification_id = None;
        }
        self.notification_toast.clear_previews();
        self.revision += 1;
    }

    /// What the center and the toast draw: every entry, the dropped count,
    /// and the shown toast preview with the cache of its text's rows. A
    /// summary preview reads as `[Koshi]` at the loudest severity among the
    /// entries it counts.
    #[must_use]
    pub(crate) fn build_notification_view(&self) -> NotificationView<'_> {
        NotificationView {
            center_entries: &self.merged_entries,
            dropped_entry_count: self.dropped_entry_count,
            shown_toast: self.find_toast_view(),
        }
    }

    /// The shown toast preview as the toast draws it, and the thousandths of
    /// its width past its side edge. `None` while no toast shows.
    #[must_use]
    pub(crate) fn find_shown_toast(&self) -> Option<(NotificationToastView<'_>, u16)> {
        let hidden_per_mille = self.notification_toast.find_shown_preview()?.1;
        Some((self.find_toast_view()?, hidden_per_mille))
    }

    /// Advance the toast's clock to `current_time` with the phase lengths of
    /// `toast_timing`, stopped while `is_toast_paused`
    /// ([`NotificationToastQueue::advance`]), and the center's slide to
    /// `current_time`.
    ///
    /// The slide starts at its first advance and eases out over
    /// [`PLACEMENT_ANIMATION_DURATION`]
    /// ([`compute_placement_animation_progress`]). With
    /// `toast_timing.should_reduce_motion`, it ends at once. A center whose
    /// slide out ends goes.
    ///
    /// Example: a center that opens from `1000` and advances 80 ms after its
    /// slide started stands at `125` thousandths past the edge.
    pub(crate) fn advance_notification_clocks(
        &mut self,
        current_time: Instant,
        toast_timing: ToastTiming,
        is_toast_paused: bool,
    ) {
        if self
            .notification_toast
            .advance(current_time, toast_timing, is_toast_paused)
        {
            self.revision += 1;
        }
        let Some(center_state) = &mut self.notification_center else {
            return;
        };
        let slide = &mut center_state.slide;
        let end_hidden_per_mille = slide.get_end_hidden_per_mille();
        let hidden_per_mille = if toast_timing.should_reduce_motion {
            end_hidden_per_mille
        } else {
            let started_at = *slide.started_at.get_or_insert(current_time);
            let progress = compute_placement_animation_progress(started_at, current_time);
            let start_hidden_per_mille = f32::from(slide.start_hidden_per_mille);
            (start_hidden_per_mille
                + (f32::from(end_hidden_per_mille) - start_hidden_per_mille) * progress)
                .round() as u16
        };
        if slide.hidden_per_mille != hidden_per_mille {
            slide.hidden_per_mille = hidden_per_mille;
            self.revision += 1;
        }
        if slide.is_closing && hidden_per_mille == FULLY_HIDDEN_PER_MILLE {
            self.notification_center = None;
            self.revision += 1;
        }
    }

    /// How long from `current_time` until the toast's clock or the center's
    /// slide next has something to move: the toast's next step
    /// ([`NotificationToastQueue::find_next_wakeup`]), and while the center
    /// slides, the shorter of `frame_interval` and the time its slide has
    /// left. A slide that has not started wakes at once. `None` while neither
    /// has something to move.
    #[must_use]
    pub(crate) fn find_next_notification_wakeup(
        &self,
        current_time: Instant,
        toast_timing: ToastTiming,
        frame_interval: Duration,
    ) -> Option<Duration> {
        let toast_wakeup = self
            .notification_toast
            .find_next_wakeup(current_time, toast_timing);
        let center_wakeup = self.notification_center.and_then(|center_state| {
            let slide = center_state.slide;
            if slide.hidden_per_mille == slide.get_end_hidden_per_mille() && !slide.is_closing {
                return None;
            }
            let Some(started_at) = slide.started_at else {
                return Some(Duration::ZERO);
            };
            let slide_remaining = PLACEMENT_ANIMATION_DURATION
                .saturating_sub(current_time.saturating_duration_since(started_at));
            Some(slide_remaining.min(frame_interval))
        });
        match (toast_wakeup, center_wakeup) {
            (Some(toast_wakeup), Some(center_wakeup)) => Some(toast_wakeup.min(center_wakeup)),
            (toast_wakeup, center_wakeup) => toast_wakeup.or(center_wakeup),
        }
    }

    /// The number of the shown toast
    /// ([`NotificationToastQueue::find_shown_toast_number`]), or `None` while
    /// no toast shows.
    #[must_use]
    pub(crate) fn find_shown_toast_number(&self) -> Option<u64> {
        self.notification_toast.find_shown_toast_number()
    }

    /// Withdraw the toast number `toast_number` from where it stands
    /// ([`NotificationToastQueue::request_withdraw`]) while it shows. Its
    /// entry stays. Does nothing while another toast or no toast shows.
    ///
    /// Example: toast `0` ends, and toast `1` shows. A withdraw of toast `0`
    /// leaves toast `1` as it is.
    pub(crate) fn request_toast_withdraw(&mut self, toast_number: u64) {
        if self.notification_toast.find_shown_toast_number() == Some(toast_number) {
            self.notification_toast.request_withdraw();
        }
    }

    /// Post `text` from koshi in this viewer at `severity` `post_count` times
    /// in a row, the last post arriving at `created_at`, to the viewer's own
    /// queue ([`NotificationQueue::post_core_entry`]), and take the entry the
    /// posts made or changed as the newest arrival. With `should_show_toasts`,
    /// the arrival queues a toast preview while the center is closed.
    ///
    /// Example: two posts of `core:resize-pane-left: below minimum size` give
    /// one entry with `repeat_count` 2, and so does one post with
    /// `post_count` 2.
    ///
    /// A history of a local session that holds a session entry from koshi
    /// with `severity` and the sanitized `text` posts nothing.
    pub(crate) fn post_core_entry(
        &mut self,
        severity: NotificationSeverity,
        text: &str,
        post_count: NonZeroU32,
        created_at: SystemTime,
        should_show_toasts: bool,
    ) {
        if self.is_session_local {
            let sanitized_text =
                sanitize_text_to_byte_count(text, MAX_NOTIFICATION_TEXT_BYTE_COUNT);
            let is_held_by_session = self.list_merged_entries().any(|merged_entry| {
                matches!(
                    merged_entry.notification_id.owner,
                    NotificationQueueOwner::Session(_)
                ) && is_core_notice(merged_entry, severity, &sanitized_text)
            });
            if is_held_by_session {
                return;
            }
        }
        let posted_entry = self
            .client_queue
            .post_core_entry(severity, text, post_count, created_at)
            .clone();
        let held_entry_index = self.find_merged_entry_index(posted_entry.notification_id);
        self.take_arrival(posted_entry, held_entry_index, should_show_toasts);
    }

    /// Take `notification_queue`, the queue of session `session_id`, as an
    /// attach answer or a `Resync` carries it.
    ///
    /// A queue at a revision below the last one taken from the same owner and
    /// epoch is left untaken. Otherwise each entry newer than every entry
    /// taken from that epoch is taken as the newest arrival, oldest first: an
    /// entry the history holds under its id moves to the newest place with the
    /// queue's fields, and any other entry is added there. Every entry that is
    /// not newer changes nothing, and one the history no longer holds stays
    /// out. A queue of another epoch keeps every entry the history holds and
    /// adds all of its own.
    ///
    /// Example: the viewer read the session's `Warning` `koshi.kdl: unknown
    /// key` ×1. A `Resync` that carries it ×2 moves it to the newest place as
    /// an unread entry.
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
    /// With `should_show_toasts`, each newer entry queues a toast preview
    /// while the center is closed, except in the first queue taken from its
    /// owner.
    ///
    /// # Errors
    /// [`NotificationTakeError::InvalidQueue`] when the queue fails
    /// [`NotificationQueue::validate_queued_entries`], and
    /// [`NotificationTakeError::ForeignQueue`] when its owner is not session
    /// `session_id`. Neither changes the history.
    pub(crate) fn take_session_queue(
        &mut self,
        session_id: SessionId,
        notification_queue: &NotificationQueue,
        should_show_toasts: bool,
    ) -> Result<(), NotificationTakeError> {
        notification_queue
            .validate_queued_entries()
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
                merged_through.sequence.saturating_add(1)
            });
        for queued_entry in notification_queue.list_queued_entries() {
            let received_entry = sanitize_received_entry(queued_entry.clone());
            self.count_skipped_sequences(&received_entry, &mut next_expected_sequence);
            let is_new = merged_through
                .is_none_or(|merged_through| merged_through.is_older_than(&received_entry));
            if !is_new {
                continue;
            }
            merged_through = Some(MergedThrough {
                sequence: received_entry.notification_id.sequence,
                repeat_count: received_entry.repeat_count,
            });
            let held_entry_index = self.find_merged_entry_index(received_entry.notification_id);
            self.take_arrival(received_entry, held_entry_index, should_preview_new_entries);
        }
        self.session_queue_mark = Some(SessionQueueMark {
            owner,
            epoch,
            revision: notification_queue.get_revision(),
            merged_through,
        });
        Ok(())
    }

    /// Take one `NotificationPosted` from the session: `posted_entry` at the
    /// queue's `revision`.
    ///
    /// A post at a revision not above the last one taken from the queue is
    /// dropped. Otherwise `posted_entry`, its text sanitized as
    /// [`take_session_queue`](Self::take_session_queue) states, is taken as
    /// the newest arrival when it is newer than every entry taken from the
    /// queue's epoch: an entry the history holds under its id moves to the
    /// newest place, and any other entry is added there. A posted entry that
    /// is not newer changes no entry. In a history of a local session, an
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
        posted_entry: NotificationEntry,
        should_show_toasts: bool,
    ) -> Result<(), NotificationTakeError> {
        let Some(session_queue_mark) = self.session_queue_mark.as_mut() else {
            return Err(NotificationTakeError::PostWithoutQueue {
                notification_id: posted_entry.notification_id,
            });
        };
        if posted_entry.notification_id.owner != session_queue_mark.owner
            || posted_entry.notification_id.epoch != session_queue_mark.epoch
        {
            return Err(NotificationTakeError::ForeignPost {
                notification_id: posted_entry.notification_id,
            });
        }
        if revision <= session_queue_mark.revision {
            return Ok(());
        }
        session_queue_mark.revision = revision;
        let merged_through = session_queue_mark.merged_through;
        let is_new =
            merged_through.is_none_or(|merged_through| merged_through.is_older_than(&posted_entry));
        if is_new {
            session_queue_mark.merged_through = Some(MergedThrough {
                sequence: posted_entry.notification_id.sequence,
                repeat_count: posted_entry.repeat_count,
            });
        }
        let mut next_expected_sequence = merged_through.map_or(0, |merged_through| {
            merged_through.sequence.saturating_add(1)
        });
        self.count_skipped_sequences(&posted_entry, &mut next_expected_sequence);
        if is_new {
            let held_entry_index = self.find_merged_entry_index(posted_entry.notification_id);
            self.take_arrival(
                sanitize_received_entry(posted_entry),
                held_entry_index,
                should_show_toasts,
            );
        }
        Ok(())
    }

    /// Add the sequences between `next_expected_sequence` and
    /// `received_entry`'s to the dropped count when `received_entry`'s
    /// sequence is at least `next_expected_sequence`, and expect the sequence
    /// after `received_entry`'s next. With `5` expected, an entry at sequence
    /// 8 adds 3, and an entry at sequence 4 adds nothing. The dropped count
    /// and the expected sequence saturate at [`u64::MAX`].
    fn count_skipped_sequences(
        &mut self,
        received_entry: &NotificationEntry,
        next_expected_sequence: &mut u64,
    ) {
        let received_sequence = received_entry.notification_id.sequence;
        if received_sequence < *next_expected_sequence {
            return;
        }
        let skipped_sequence_count = received_sequence - *next_expected_sequence;
        if skipped_sequence_count > 0 {
            self.dropped_entry_count = self
                .dropped_entry_count
                .saturating_add(skipped_sequence_count);
            self.revision += 1;
        }
        *next_expected_sequence = received_sequence.saturating_add(1);
    }

    /// Put `arrived_entry` at the newest place
    /// ([`push_merged_entry`](Self::push_merged_entry)). `held_entry_index` is
    /// the index of the entry held under `arrived_entry`'s id, or `None` when
    /// the history holds none. A held entry moves there with `arrived_entry`'s
    /// fields, and any other entry is added there. A held entry the center
    /// selected stays selected. With `should_show_toasts`, the arrival queues
    /// a toast preview while the center is not open.
    fn take_arrival(
        &mut self,
        arrived_entry: NotificationEntry,
        held_entry_index: Option<usize>,
        should_show_toasts: bool,
    ) {
        let selected_notification_id = self
            .notification_center
            .and_then(|center_state| center_state.selected_notification_id);
        let arrived_notification_id = arrived_entry.notification_id;
        if let Some(held_entry_index) = held_entry_index {
            self.remove_merged_entry_at(held_entry_index);
        }
        self.push_merged_entry(arrived_entry, should_show_toasts);
        if selected_notification_id == Some(arrived_notification_id) {
            self.set_selected_notification_id(selected_notification_id);
        }
    }

    /// Add `arrived_entry` at the newest place, as an unread entry while the
    /// center is not open. An open center that selects no entry selects
    /// `arrived_entry`.
    ///
    /// In a history of a local session, a session `arrived_entry` from koshi
    /// first removes every entry of the viewer's own queue from koshi with its
    /// severity and text, and their toast previews show `arrived_entry`
    /// ([`NotificationToastQueue::repoint_previews`]). A history then holding
    /// [`MAX_MERGED_NOTIFICATION_COUNT`] entries evicts its oldest
    /// ([`evict_oldest_entry`](Self::evict_oldest_entry)).
    ///
    /// With `should_preview` and the center closed, `arrived_entry` queues a
    /// toast preview ([`NotificationToastQueue::queue_preview`]). Otherwise a
    /// preview that shows `arrived_entry`'s id takes `arrived_entry` as its
    /// copy.
    fn push_merged_entry(&mut self, arrived_entry: NotificationEntry, should_preview: bool) {
        if self.is_session_local
            && matches!(
                arrived_entry.notification_id.owner,
                NotificationQueueOwner::Session(_)
            )
            && arrived_entry.source == NotificationSource::Core
        {
            let mut replaced_ids = Vec::new();
            let mut merged_entry_index = 0;
            while merged_entry_index < self.merged_entries.count_center_entries() {
                let merged_entry = self.merged_entries[merged_entry_index].get_notification_entry();
                if matches!(
                    merged_entry.notification_id.owner,
                    NotificationQueueOwner::Client(_)
                ) && is_core_notice(merged_entry, arrived_entry.severity, &arrived_entry.text)
                {
                    replaced_ids.push(merged_entry.notification_id);
                    self.remove_merged_entry_at(merged_entry_index);
                } else {
                    merged_entry_index += 1;
                }
            }
            if !replaced_ids.is_empty() {
                self.notification_toast
                    .repoint_previews(&replaced_ids, &arrived_entry);
            }
        }
        if self.merged_entries.count_center_entries() == MAX_MERGED_NOTIFICATION_COUNT {
            self.evict_oldest_entry();
        }
        let is_center_open = self.is_notification_center_open();
        if should_preview && !is_center_open {
            self.notification_toast.queue_preview(&arrived_entry);
        } else {
            self.notification_toast.update_previews(&arrived_entry);
        }
        if !is_center_open {
            self.unread_entry_count += 1;
        }
        if let Some(center_state) = self.find_open_center_state() {
            center_state
                .selected_notification_id
                .get_or_insert(arrived_entry.notification_id);
        }
        self.merged_entries.add_newest_center_entry(
            NotificationCenterEntry::from_notification_entry(arrived_entry),
        );
        self.revision += 1;
    }

    /// Add `dropped_entry_count` entries that never reached the viewer to the
    /// dropped count. The count saturates at [`u64::MAX`]. A
    /// `dropped_entry_count` of `0` changes nothing.
    pub(crate) fn add_dropped_entries(&mut self, dropped_entry_count: u64) {
        if dropped_entry_count == 0 {
            return;
        }
        self.dropped_entry_count = self.dropped_entry_count.saturating_add(dropped_entry_count);
        self.revision += 1;
    }

    /// Evict the oldest entry: the dropped count adds 1, and its toast
    /// previews go ([`NotificationToastQueue::remove_previews`]).
    fn evict_oldest_entry(&mut self) {
        let Some(evicted_entry) = self.remove_merged_entry_at(0) else {
            return;
        };
        self.dropped_entry_count += 1;
        self.notification_toast
            .remove_previews(evicted_entry.get_notification_entry().notification_id);
    }

    /// Remove the entry at `merged_entry_index` and return it, or `None` past
    /// the newest entry. An unread entry takes 1 from the unread count. A
    /// selected entry moves the center's selection to the entry older than
    /// it, else to the entry newer than it, else to no entry.
    ///
    /// A caller that adds the same entry back at once keeps the selection
    /// itself ([`take_arrival`](Self::take_arrival)).
    fn remove_merged_entry_at(
        &mut self,
        merged_entry_index: usize,
    ) -> Option<NotificationCenterEntry> {
        let first_unread_index =
            self.merged_entries.count_center_entries() - self.unread_entry_count;
        let removed_entry = self
            .merged_entries
            .remove_center_entry_at(merged_entry_index)?;
        if merged_entry_index >= first_unread_index {
            self.unread_entry_count -= 1;
        }
        let removed_notification_id = removed_entry.get_notification_entry().notification_id;
        let is_removed_entry_selected = self.notification_center.is_some_and(|center_state| {
            center_state.selected_notification_id == Some(removed_notification_id)
        });
        if is_removed_entry_selected {
            let next_selected_notification_id = merged_entry_index
                .checked_sub(1)
                .or_else(|| {
                    (merged_entry_index < self.merged_entries.count_center_entries())
                        .then_some(merged_entry_index)
                })
                .map(|next_entry_index| {
                    self.merged_entries[next_entry_index]
                        .get_notification_entry()
                        .notification_id
                });
            self.set_selected_notification_id(next_selected_notification_id);
        }
        self.revision += 1;
        Some(removed_entry)
    }

    /// Set the center's selected entry to `selected_notification_id`. Does
    /// nothing while the center is closed.
    fn set_selected_notification_id(&mut self, selected_notification_id: Option<NotificationId>) {
        let Some(center_state) = &mut self.notification_center else {
            return;
        };
        if center_state.selected_notification_id != selected_notification_id {
            center_state.selected_notification_id = selected_notification_id;
            self.revision += 1;
        }
    }

    /// The center's state while it is open, or `None` while it is closed or
    /// slides out.
    fn find_open_center_state(&mut self) -> Option<&mut NotificationCenterState> {
        self.notification_center
            .as_mut()
            .filter(|center_state| !center_state.slide.is_closing)
    }

    /// The shown toast preview as the toast draws it, with the cache of its
    /// text's rows, or `None` while no toast shows.
    fn find_toast_view(&self) -> Option<NotificationToastView<'_>> {
        let (toast_preview, _) = self.notification_toast.find_shown_preview()?;
        let toast_content = match toast_preview {
            ToastPreview::Entry(previewed_entry) => {
                NotificationToastContent::Entry(previewed_entry)
            }
            ToastPreview::Summary(summary_members) => NotificationToastContent::Summary {
                notification_count: summary_members.len(),
                severity: summary_members
                    .iter()
                    .map(|(_, member_severity)| *member_severity)
                    .max()?,
            },
        };
        Some(NotificationToastView {
            toast_content,
            toast_text_row_cache: &self.toast_text_row_cache,
        })
    }

    /// Where the entry with `notification_id` stands in the history, or
    /// `None` when the history holds none.
    fn find_merged_entry_index(&self, notification_id: NotificationId) -> Option<usize> {
        self.list_merged_entries()
            .position(|merged_entry| merged_entry.notification_id == notification_id)
    }
}

/// `received_entry` with its text stripped of every character
/// [`sanitize_text_to_byte_count`] removes and cut to
/// [`MAX_NOTIFICATION_TEXT_BYTE_COUNT`] bytes.
fn sanitize_received_entry(mut received_entry: NotificationEntry) -> NotificationEntry {
    received_entry.text =
        sanitize_text_to_byte_count(&received_entry.text, MAX_NOTIFICATION_TEXT_BYTE_COUNT);
    received_entry
}

/// Whether `notification_entry` comes from koshi with `severity` and `text`.
fn is_core_notice(
    notification_entry: &NotificationEntry,
    severity: NotificationSeverity,
    text: &str,
) -> bool {
    notification_entry.source == NotificationSource::Core
        && notification_entry.severity == severity
        && notification_entry.text == text
}

#[cfg(test)]
mod tests;
