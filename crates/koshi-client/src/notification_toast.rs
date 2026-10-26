//! One viewer's toast: the preview it shows, the previews that wait behind
//! it, and the phase and slide of the shown preview.
//!
//! A preview shows one entry, or the summary that counts the entries that
//! have no preview of their own. One preview shows at a time, in arrival
//! order. A shown preview runs three phases: appear slides it in from past its
//! side edge, stay holds it in place, and withdraw slides it out again.
//!
//! | phase    | no preview waits      | a preview waits |
//! |----------|-----------------------|-----------------|
//! | appear   | 1 s                   | 0.5 s           |
//! | stay     | `toast-duration-ms`   | 0.5 s           |
//! | withdraw | 1 s                   | 0.5 s           |
//!
//! A phase reads its length each time the clock advances. A preview that
//! starts to wait ends the shown phase 0.5 s after the phase started, but not
//! before the clock first advances with that preview waiting, and not after
//! the phase's full length. A phase whose waiting previews all go runs its
//! full length from the same start. A phase that ended never runs again. With
//! reduced motion, appear and withdraw take no time and stay keeps its length.
//!
//! Example: with `toast-duration-ms 60000`, a toast stays from 1 s, and 3
//! entries arrive at 50 s. The stay ends at 50 s, the toast withdraws until
//! 50.5 s, and each of the 3 entries then shows in turn.
//!
//! The clock steps every 50 ms. The slide moves the toast by whole thousandths
//! of its width: 1 s of appear at 300 ms leaves `700` thousandths past the
//! edge.
//!
//! Example: 40 entries in one second show the first entry, keep the next 3
//! waiting, and count the other 36 in one summary that shows last. A summary
//! counts no entry that arrives once it shows: an entry that arrives while the
//! summary shows waits behind it.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use koshi_core::notification::{NotificationEntry, NotificationId, NotificationSeverity};

/// The most entry previews that wait behind the shown preview: 3. An entry
/// that arrives while 3 wait joins the summary.
const MAX_WAITING_TOAST_COUNT: usize = 3;

/// The step of the toast's clock: 50 ms.
const TOAST_FRAME_INTERVAL: Duration = Duration::from_millis(50);

/// How long appear and withdraw run while no preview waits: 1 s.
const NORMAL_SLIDE_DURATION: Duration = Duration::from_secs(1);

/// How long each phase runs while a preview waits, and a fast withdraw: 0.5 s.
const BACKLOG_PHASE_DURATION: Duration = Duration::from_millis(500);

/// The thousandths of a toast's width past its side edge while it is fully
/// out of view: 1000.
const FULLY_HIDDEN_PER_MILLE: u16 = 1000;

/// What one preview shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToastPreview {
    /// One entry, as the history last took it.
    Entry(NotificationEntry),
    /// The entries that have no preview of their own, oldest first: each
    /// entry's id and severity.
    Summary(Vec<(NotificationId, NotificationSeverity)>),
}

/// How fast a withdraw runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WithdrawSpeed {
    /// The length of the withdraw phase: 1 s while no preview waits, 0.5 s
    /// while one does.
    Normal,
    /// 0.5 s, whatever waits.
    Fast,
}

/// The phase of the shown preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToastPhase {
    /// Sliding in from past the side edge.
    Appear,
    /// Holding in place.
    Stay,
    /// Sliding out past the side edge.
    Withdraw {
        /// The thousandths of the width past the edge when the withdraw
        /// started: `0` from a toast in place.
        start_hidden_per_mille: u16,
        /// How fast the withdraw runs.
        withdraw_speed: WithdrawSpeed,
    },
}

/// The preview on screen, and where its phases stand.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ShownToast {
    /// The toast's number: the count of toasts the queue showed before it.
    toast_number: u64,
    /// What the toast shows.
    preview: ToastPreview,
    /// The phase it runs.
    phase: ToastPhase,
    /// When the phase started, or `None` before the clock first advanced
    /// with this preview shown.
    phase_started_at: Option<Instant>,
    /// The thousandths of the toast's width past its side edge when the clock
    /// last advanced, from `0` to `1000`.
    hidden_per_mille: u16,
    /// A withdraw that the next advance of the clock starts.
    withdraw_request: Option<WithdrawSpeed>,
}

impl ShownToast {
    /// The toast number `toast_number` of `preview`, which starts its appear
    /// phase at the next advance of the clock, fully past its edge.
    fn from_preview(toast_number: u64, preview: ToastPreview) -> Self {
        ShownToast {
            toast_number,
            preview,
            phase: ToastPhase::Appear,
            phase_started_at: None,
            hidden_per_mille: FULLY_HIDDEN_PER_MILLE,
            withdraw_request: None,
        }
    }
}

/// The lengths that one viewer's settings give the toast's phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ToastTiming {
    /// The stay phase while no preview waits: `toast-duration-ms`.
    pub(crate) stay_duration: Duration,
    /// Whether appear and withdraw take no time: `reduced-motion`.
    pub(crate) should_reduce_motion: bool,
}

/// One viewer's toast and its preview queue.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NotificationToastQueue {
    /// The preview on screen, or `None` while no toast shows.
    shown_toast: Option<ShownToast>,
    /// The entry previews that wait, oldest first, at most
    /// [`MAX_WAITING_TOAST_COUNT`].
    waiting_entries: VecDeque<NotificationEntry>,
    /// The summary that waits behind every waiting entry, or `None`. Its
    /// members are never empty.
    waiting_summary: Option<Vec<(NotificationId, NotificationSeverity)>>,
    /// When the clock stopped, or `None` while it runs.
    paused_at: Option<Instant>,
    /// When the clock first advanced with a preview waiting, since it last
    /// advanced with none waiting. `None` while none waits, and before the
    /// clock advances with one waiting.
    waiting_preview_seen_at: Option<Instant>,
    /// How many toasts the queue showed: the number the next shown toast
    /// takes. A clear keeps it.
    shown_toast_count: u64,
}

impl NotificationToastQueue {
    /// Preview `arrived_entry`, an arrival of the history.
    ///
    /// A preview that already shows `arrived_entry`'s id takes
    /// `arrived_entry` as its copy, so a repeat shows its new `×N`, and
    /// nothing is queued. Otherwise, in this order: the waiting summary counts
    /// `arrived_entry`; with no toast shown, `arrived_entry` shows; with fewer
    /// than [`MAX_WAITING_TOAST_COUNT`] waiting, `arrived_entry` waits; and
    /// else `arrived_entry` starts the waiting summary. A shown summary counts
    /// no new entry.
    ///
    /// Example: a summary of 36 entries shows, and an `Error` arrives. The
    /// `Error` waits behind the summary and shows after it.
    pub(crate) fn queue_preview(&mut self, arrived_entry: &NotificationEntry) {
        if self.update_previews(arrived_entry) {
            return;
        }
        let summary_member = (arrived_entry.notification_id, arrived_entry.severity);
        if let Some(summary_members) = &mut self.waiting_summary {
            summary_members.push(summary_member);
            return;
        }
        if self.shown_toast.is_none() {
            self.show_preview(ToastPreview::Entry(arrived_entry.clone()));
        } else if self.waiting_entries.len() < MAX_WAITING_TOAST_COUNT {
            self.waiting_entries.push_back(arrived_entry.clone());
        } else {
            self.waiting_summary = Some(vec![summary_member]);
        }
    }

    /// Take `changed_entry` as the copy of every entry preview with its id,
    /// shown or waiting. Returns whether any preview, the summaries included,
    /// shows its id.
    pub(crate) fn update_previews(&mut self, changed_entry: &NotificationEntry) -> bool {
        let mut has_preview = false;
        if let Some(shown_toast) = &mut self.shown_toast {
            has_preview = match &mut shown_toast.preview {
                ToastPreview::Entry(shown_entry)
                    if shown_entry.notification_id == changed_entry.notification_id =>
                {
                    *shown_entry = changed_entry.clone();
                    true
                }
                ToastPreview::Entry(_) => false,
                ToastPreview::Summary(summary_members) => summary_members
                    .iter()
                    .any(|(member_id, _)| *member_id == changed_entry.notification_id),
            };
        }
        for waiting_entry in &mut self.waiting_entries {
            if waiting_entry.notification_id == changed_entry.notification_id {
                *waiting_entry = changed_entry.clone();
                has_preview = true;
            }
        }
        has_preview
            || self
                .waiting_summary
                .as_ref()
                .is_some_and(|summary_members| {
                    summary_members
                        .iter()
                        .any(|(member_id, _)| *member_id == changed_entry.notification_id)
                })
    }

    /// Point every preview of `replacing_entry` or of the entries
    /// `replaced_ids` at `replacing_entry`: the history holds
    /// `replacing_entry` in their place. Only the first of those previews
    /// stays, in this order: the shown preview, the waiting entries oldest
    /// first, then the waiting summary's members oldest first. A waiting
    /// summary that counts nothing more goes.
    ///
    /// Example: the viewer's own entry `koshi.kdl: unknown key` shows when
    /// the session's entry with the same text arrives. The toast goes on
    /// showing, now as the session's entry.
    pub(crate) fn repoint_previews(
        &mut self,
        replaced_ids: &[NotificationId],
        replacing_entry: &NotificationEntry,
    ) {
        let is_replaced = |notification_id: NotificationId| {
            notification_id == replacing_entry.notification_id
                || replaced_ids.contains(&notification_id)
        };
        let mut is_replacing_entry_previewed = false;
        if let Some(shown_toast) = &mut self.shown_toast {
            match &mut shown_toast.preview {
                ToastPreview::Entry(shown_entry) => {
                    if is_replaced(shown_entry.notification_id) {
                        *shown_entry = replacing_entry.clone();
                        is_replacing_entry_previewed = true;
                    }
                }
                ToastPreview::Summary(summary_members) => repoint_summary_members(
                    summary_members,
                    replacing_entry,
                    &is_replaced,
                    &mut is_replacing_entry_previewed,
                ),
            }
        }
        self.waiting_entries.retain_mut(|waiting_entry| {
            if !is_replaced(waiting_entry.notification_id) {
                return true;
            }
            if is_replacing_entry_previewed {
                return false;
            }
            *waiting_entry = replacing_entry.clone();
            is_replacing_entry_previewed = true;
            true
        });
        if let Some(summary_members) = &mut self.waiting_summary {
            repoint_summary_members(
                summary_members,
                replacing_entry,
                &is_replaced,
                &mut is_replacing_entry_previewed,
            );
            if summary_members.is_empty() {
                self.waiting_summary = None;
            }
        }
    }

    /// Drop every preview of the entry `notification_id`, which the history
    /// no longer holds: a waiting entry goes, a summary stops counting it, and
    /// a waiting summary that counts nothing more goes. A shown preview of the
    /// entry withdraws fast from where it stands at the next advance of the
    /// clock. A shown summary that counts nothing more goes at once, and the
    /// next preview shows from the next advance of the clock.
    pub(crate) fn remove_previews(&mut self, notification_id: NotificationId) {
        self.waiting_entries
            .retain(|waiting_entry| waiting_entry.notification_id != notification_id);
        if let Some(summary_members) = &mut self.waiting_summary {
            summary_members.retain(|(member_id, _)| *member_id != notification_id);
            if summary_members.is_empty() {
                self.waiting_summary = None;
            }
        }
        let Some(shown_toast) = &mut self.shown_toast else {
            return;
        };
        let is_shown_preview_gone = match &mut shown_toast.preview {
            ToastPreview::Entry(shown_entry) => shown_entry.notification_id == notification_id,
            ToastPreview::Summary(summary_members) => {
                summary_members.retain(|(member_id, _)| *member_id != notification_id);
                summary_members.is_empty()
            }
        };
        if !is_shown_preview_gone {
            return;
        }
        if matches!(shown_toast.preview, ToastPreview::Entry(_)) {
            shown_toast.withdraw_request = Some(WithdrawSpeed::Fast);
        } else {
            self.show_next_preview();
        }
    }

    /// Withdraw the shown preview from where it stands at the next advance of
    /// the clock, at the speed of the withdraw phase. Does nothing while no
    /// toast shows, or while it withdraws.
    pub(crate) fn request_withdraw(&mut self) {
        let Some(shown_toast) = &mut self.shown_toast else {
            return;
        };
        if !matches!(shown_toast.phase, ToastPhase::Withdraw { .. })
            && shown_toast.withdraw_request.is_none()
        {
            shown_toast.withdraw_request = Some(WithdrawSpeed::Normal);
        }
    }

    /// Drop every preview at once, the shown one included. The clock keeps
    /// its pause.
    pub(crate) fn clear_previews(&mut self) {
        self.shown_toast = None;
        self.waiting_entries.clear();
        self.waiting_summary = None;
    }

    /// Advance the clock to `current_time` with the phase lengths of
    /// `toast_timing`.
    ///
    /// While `is_paused`, the clock stops, and nothing moves. When it runs
    /// again, the shown phase resumes where it stopped. A shown preview that
    /// the clock has not yet advanced with starts its appear phase at
    /// `current_time`. A requested withdraw starts at `current_time`, from the
    /// slide position at `current_time`; a toast still fully past its edge
    /// ends there. Each phase that ended by `current_time` gives way to the
    /// next one at its own end ([`compute_phase_ended_at`]), and a withdraw
    /// that ended shows the next preview, the waiting entries first, then the
    /// waiting summary. The next preview starts its appear phase at
    /// `current_time`, so a clock that advances late runs no part of a preview
    /// that was never drawn. A preview that waits at `current_time`, and that the
    /// clock has not seen waiting before, counts as seen at `current_time`.
    /// The slide position reads the time in the phase in whole 50 ms steps.
    ///
    /// Returns whether the preview on screen changed: a new preview shows, or
    /// no preview shows anymore.
    pub(crate) fn advance(
        &mut self,
        current_time: Instant,
        toast_timing: ToastTiming,
        is_paused: bool,
    ) -> bool {
        if is_paused {
            self.paused_at.get_or_insert(current_time);
            return false;
        }
        if let Some(paused_at) = self.paused_at.take() {
            if let Some(phase_started_at) = self
                .shown_toast
                .as_mut()
                .and_then(|shown_toast| shown_toast.phase_started_at.as_mut())
            {
                *phase_started_at += current_time.saturating_duration_since(paused_at);
            }
        }
        let mut is_preview_changed = false;
        loop {
            if self.has_waiting_preview() {
                self.waiting_preview_seen_at.get_or_insert(current_time);
            } else {
                self.waiting_preview_seen_at = None;
            }
            let waiting_preview_seen_at = self.waiting_preview_seen_at;
            let Some(shown_toast) = &mut self.shown_toast else {
                return is_preview_changed;
            };
            let phase_started_at = *shown_toast.phase_started_at.get_or_insert(current_time);
            let phase_ended_at = compute_phase_ended_at(
                shown_toast.phase,
                phase_started_at,
                toast_timing,
                waiting_preview_seen_at,
            );
            let phase_duration = phase_ended_at.saturating_duration_since(phase_started_at);
            if let Some(withdraw_speed) = shown_toast.withdraw_request.take() {
                let start_hidden_per_mille = compute_hidden_per_mille(
                    shown_toast.phase,
                    current_time.saturating_duration_since(phase_started_at),
                    phase_duration,
                );
                if start_hidden_per_mille == FULLY_HIDDEN_PER_MILLE {
                    self.show_next_preview();
                    is_preview_changed = true;
                    continue;
                }
                shown_toast.phase = ToastPhase::Withdraw {
                    start_hidden_per_mille,
                    withdraw_speed,
                };
                shown_toast.phase_started_at = Some(current_time);
                continue;
            }
            if current_time < phase_ended_at {
                shown_toast.hidden_per_mille = compute_hidden_per_mille(
                    shown_toast.phase,
                    current_time.saturating_duration_since(phase_started_at),
                    phase_duration,
                );
                return is_preview_changed;
            }
            shown_toast.phase_started_at = Some(phase_ended_at);
            match shown_toast.phase {
                ToastPhase::Appear => shown_toast.phase = ToastPhase::Stay,
                ToastPhase::Stay => {
                    shown_toast.phase = ToastPhase::Withdraw {
                        start_hidden_per_mille: 0,
                        withdraw_speed: WithdrawSpeed::Normal,
                    };
                }
                ToastPhase::Withdraw { .. } => {
                    self.show_next_preview();
                    is_preview_changed = true;
                }
            }
        }
    }

    /// How long from `current_time` until the clock next has something to
    /// move, with the phase lengths of `toast_timing`: the next 50 ms step of
    /// a slide, capped at the end of its phase, and the end of a stay phase.
    /// A preview that waits, and that the clock has not seen waiting, counts
    /// as seen at `current_time`. `Duration::ZERO` for a preview the clock has
    /// not yet advanced with, and for a requested withdraw. `None` while the
    /// clock is paused, and while no toast shows.
    #[must_use]
    pub(crate) fn find_next_wakeup(
        &self,
        current_time: Instant,
        toast_timing: ToastTiming,
    ) -> Option<Duration> {
        if self.paused_at.is_some() {
            return None;
        }
        let shown_toast = self.shown_toast.as_ref()?;
        let Some(phase_started_at) = shown_toast.phase_started_at else {
            return Some(Duration::ZERO);
        };
        if shown_toast.withdraw_request.is_some() {
            return Some(Duration::ZERO);
        }
        let waiting_preview_seen_at = self
            .has_waiting_preview()
            .then(|| self.waiting_preview_seen_at.unwrap_or(current_time));
        let phase_ended_at = compute_phase_ended_at(
            shown_toast.phase,
            phase_started_at,
            toast_timing,
            waiting_preview_seen_at,
        );
        let next_change_at = match shown_toast.phase {
            ToastPhase::Stay => phase_ended_at,
            ToastPhase::Appear | ToastPhase::Withdraw { .. } => {
                let elapsed_step_count =
                    count_elapsed_steps(current_time.saturating_duration_since(phase_started_at));
                let next_step_at = phase_started_at
                    + TOAST_FRAME_INTERVAL
                        * u32::try_from(elapsed_step_count + 1).unwrap_or(u32::MAX);
                next_step_at.min(phase_ended_at)
            }
        };
        Some(next_change_at.saturating_duration_since(current_time))
    }

    /// The shown preview and the thousandths of its width past its side edge,
    /// or `None` while no toast shows.
    #[must_use]
    pub(crate) fn find_shown_preview(&self) -> Option<(&ToastPreview, u16)> {
        self.shown_toast
            .as_ref()
            .map(|shown_toast| (&shown_toast.preview, shown_toast.hidden_per_mille))
    }

    /// The number of the shown toast ([`ShownToast::toast_number`]), or
    /// `None` while no toast shows.
    #[must_use]
    pub(crate) fn find_shown_toast_number(&self) -> Option<u64> {
        self.shown_toast
            .as_ref()
            .map(|shown_toast| shown_toast.toast_number)
    }

    /// Whether an entry preview or the summary waits behind the shown
    /// preview.
    fn has_waiting_preview(&self) -> bool {
        !self.waiting_entries.is_empty() || self.waiting_summary.is_some()
    }

    /// Show the oldest waiting entry, else the waiting summary, starting its
    /// appear phase at the next advance of the clock. With nothing waiting, no
    /// toast shows.
    fn show_next_preview(&mut self) {
        let next_preview = match self.waiting_entries.pop_front() {
            Some(waiting_entry) => Some(ToastPreview::Entry(waiting_entry)),
            None => self.waiting_summary.take().map(ToastPreview::Summary),
        };
        match next_preview {
            Some(next_preview) => self.show_preview(next_preview),
            None => self.shown_toast = None,
        }
    }

    /// Show `preview` as the next toast number, starting its appear phase at
    /// the next advance of the clock.
    fn show_preview(&mut self, preview: ToastPreview) {
        self.shown_toast = Some(ShownToast::from_preview(self.shown_toast_count, preview));
        self.shown_toast_count += 1;
    }
}

/// Point the first of `summary_members` that `is_replaced` names at
/// `replacing_entry`, and drop the others it names. With
/// `is_replacing_entry_previewed`, an earlier preview already shows
/// `replacing_entry`, and every member `is_replaced` names goes. Sets
/// `is_replacing_entry_previewed` when a member then names `replacing_entry`.
fn repoint_summary_members(
    summary_members: &mut Vec<(NotificationId, NotificationSeverity)>,
    replacing_entry: &NotificationEntry,
    is_replaced: &impl Fn(NotificationId) -> bool,
    is_replacing_entry_previewed: &mut bool,
) {
    summary_members.retain_mut(|(member_id, member_severity)| {
        if !is_replaced(*member_id) {
            return true;
        }
        if *is_replacing_entry_previewed {
            return false;
        }
        *member_id = replacing_entry.notification_id;
        *member_severity = replacing_entry.severity;
        *is_replacing_entry_previewed = true;
        true
    });
}

/// How long `phase` runs with the lengths of `toast_timing`, while
/// `has_waiting_preview` says whether a preview waits behind it.
fn compute_phase_duration(
    phase: ToastPhase,
    toast_timing: ToastTiming,
    has_waiting_preview: bool,
) -> Duration {
    match phase {
        ToastPhase::Appear | ToastPhase::Withdraw { .. } if toast_timing.should_reduce_motion => {
            Duration::ZERO
        }
        ToastPhase::Withdraw {
            withdraw_speed: WithdrawSpeed::Fast,
            ..
        } => BACKLOG_PHASE_DURATION,
        _ if has_waiting_preview => BACKLOG_PHASE_DURATION,
        ToastPhase::Appear | ToastPhase::Withdraw { .. } => NORMAL_SLIDE_DURATION,
        ToastPhase::Stay => toast_timing.stay_duration,
    }
}

/// When `phase`, started at `phase_started_at`, ends with the lengths of
/// `toast_timing`. With `waiting_preview_seen_at` `None`, no preview waits,
/// and the phase runs its full length. With a preview waiting since
/// `waiting_preview_seen_at`, the phase runs its length while a preview
/// waits, but does not end before `waiting_preview_seen_at` and does not run
/// past its full length.
///
/// A 60 s stay that started at 1 s, with a preview waiting since 50 s, ends
/// at 50 s. The same stay with a preview waiting since 1.2 s ends at 1.5 s.
fn compute_phase_ended_at(
    phase: ToastPhase,
    phase_started_at: Instant,
    toast_timing: ToastTiming,
    waiting_preview_seen_at: Option<Instant>,
) -> Instant {
    let full_phase_ended_at = phase_started_at + compute_phase_duration(phase, toast_timing, false);
    let Some(waiting_preview_seen_at) = waiting_preview_seen_at else {
        return full_phase_ended_at;
    };
    let waiting_phase_ended_at =
        phase_started_at + compute_phase_duration(phase, toast_timing, true);
    waiting_phase_ended_at
        .max(waiting_preview_seen_at)
        .min(full_phase_ended_at)
}

/// The whole 50 ms steps in `elapsed_duration`.
fn count_elapsed_steps(elapsed_duration: Duration) -> u128 {
    elapsed_duration.as_millis() / TOAST_FRAME_INTERVAL.as_millis()
}

/// The thousandths of the toast's width past its side edge `elapsed_duration`
/// into `phase`, which runs `phase_duration`. The elapsed time counts in
/// whole 50 ms steps, and a phase of no length counts as ended.
///
/// Appear runs from `1000` to `0`, stay holds `0`, and withdraw runs from its
/// start to `1000`. 1 s of appear at 325 ms gives `700`.
fn compute_hidden_per_mille(
    phase: ToastPhase,
    elapsed_duration: Duration,
    phase_duration: Duration,
) -> u16 {
    let stepped_millisecond_count =
        count_elapsed_steps(elapsed_duration) * TOAST_FRAME_INTERVAL.as_millis();
    let progress_per_mille = (stepped_millisecond_count * 1000)
        .checked_div(phase_duration.as_millis())
        .unwrap_or(1000)
        .min(1000) as u16;
    match phase {
        ToastPhase::Appear => FULLY_HIDDEN_PER_MILLE - progress_per_mille,
        ToastPhase::Stay => 0,
        ToastPhase::Withdraw {
            start_hidden_per_mille,
            ..
        } => {
            let remaining_per_mille = u32::from(FULLY_HIDDEN_PER_MILLE - start_hidden_per_mille);
            let withdrawn_per_mille = remaining_per_mille * u32::from(progress_per_mille) / 1000;
            start_hidden_per_mille + withdrawn_per_mille as u16
        }
    }
}

#[cfg(test)]
mod tests;
