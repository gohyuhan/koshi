//! Per-client frame pacing and each pane's timed renders.
//!
//! The dispatcher thread does not repaint on a blind loop. Each attached
//! client's subscription holds one [`FramePace`]: whether the client's view
//! changed since its last frame, and when its next frame may go. A change
//! marks the subscriptions of the clients it shows to
//! ([`FramePace::mark_view_changed`]). A burst of changes before the next
//! frame gives one frame, and a client gets a frame at most once per its
//! frame interval: a child that writes many times inside one interval gives
//! one frame for that interval, and a view that does not change gives no
//! frame. The frame after the
//! client's own key, paste or mouse event goes at once
//! ([`FramePace::allow_next_frame_now`]).
//!
//! # Time is injected, never read
//!
//! Neither type here calls `Instant::now()`. The event loop passes the
//! current [`Instant`] into every decision, and each decision is a pure
//! function of its inputs. A wall-clock jump, such as an NTP (Network Time
//! Protocol) correction or a daylight-saving change, does not move an
//! [`Instant`], and does not move the pace. The pace stays on the dispatcher
//! thread and is never serialized.
//!
//! # Timed renders
//!
//! [`PaneRenderDeadlines`] holds one kind of timed render for each pane: its
//! next image animation frame, or the end of its open synchronized-output
//! group. It is ordered by time: the earliest deadline and the panes that are
//! due are read without reading any other pane.

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use koshi_core::ids::PaneId;

/// One subscriber's frame pacing: whether its client's view changed since
/// its last frame, and when its next frame may go.
///
/// Frames go on a grid of `frame_interval` steps. A frame that goes less than
/// one interval after the time it was allowed keeps the grid: the next frame
/// is allowed one interval after that allowed time. Any other frame starts
/// the grid at the time it went. Example — a 16666667 ns interval, a view
/// that changes every millisecond, and each frame sent at the first
/// millisecond it is due: frames go at 0 ms, 17 ms, 34 ms and 51 ms, and 60
/// of them go in one second.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FramePace {
    /// The least time between two frames on the grid.
    frame_interval: Duration,
    /// When the next frame may go. `None` until the first frame, which goes
    /// at once.
    next_frame_allowed_at: Option<Instant>,
    /// Whether the client's view changed since its last frame. A new pace
    /// starts with it set: a new subscriber is owed its first frame.
    has_view_change: bool,
    /// Whether the next frame goes without waiting for `next_frame_allowed_at`.
    /// The client's own key, paste or mouse event sets it, and the next frame
    /// that goes clears it.
    is_next_frame_immediate: bool,
}

impl FramePace {
    /// A pace whose frames go at most once per `frame_interval`, owed its
    /// first frame, which may go at once. `frame_interval` is at least 1 ns:
    /// [`find_grid_frame_time`](Self::find_grid_frame_time) divides by it.
    pub fn new(frame_interval: Duration) -> Self {
        FramePace {
            frame_interval,
            next_frame_allowed_at: None,
            has_view_change: true,
            is_next_frame_immediate: false,
        }
    }

    /// Record that the client's view changed. Marking twice before a frame
    /// still gives one frame.
    pub fn mark_view_changed(&mut self) {
        self.has_view_change = true;
    }

    /// Let the next frame go without waiting for the grid. A view that has
    /// not changed still gives no frame.
    pub fn allow_next_frame_now(&mut self) {
        self.is_next_frame_immediate = true;
    }

    /// Record that no frame is owed for the change marked so far, and move no
    /// time on the grid.
    pub fn clear_view_change(&mut self) {
        self.has_view_change = false;
    }

    /// How long until a frame is due at `current_time`. `None` when the view
    /// did not change. `Some(Duration::ZERO)` when a frame is due now: before
    /// the first frame, after [`allow_next_frame_now`](Self::allow_next_frame_now),
    /// or once the allowed time passed. Otherwise the time left until the
    /// allowed time.
    pub fn compute_frame_wait(&self, current_time: Instant) -> Option<Duration> {
        if !self.has_view_change {
            return None;
        }
        if self.is_next_frame_immediate {
            return Some(Duration::ZERO);
        }
        Some(
            self.next_frame_allowed_at
                .map_or(Duration::ZERO, |next_frame_allowed_at| {
                    next_frame_allowed_at.saturating_duration_since(current_time)
                }),
        )
    }

    /// The first time at or after `earliest_time` at which a frame may go on
    /// this pace's grid, whether or not the view changed: the allowed time
    /// plus a whole number of intervals. `earliest_time` itself before the
    /// first frame and after
    /// [`allow_next_frame_now`](Self::allow_next_frame_now).
    ///
    /// Example — a 16666667 ns interval with the next frame allowed at 100 ms:
    /// `90 ms` gives 100 ms, `105 ms` gives 116666667 ns, and `120 ms` gives
    /// 133333334 ns.
    pub fn find_grid_frame_time(&self, earliest_time: Instant) -> Instant {
        if self.is_next_frame_immediate {
            return earliest_time;
        }
        let Some(next_frame_allowed_at) = self.next_frame_allowed_at else {
            return earliest_time;
        };
        if next_frame_allowed_at >= earliest_time {
            return next_frame_allowed_at;
        }
        let late_nanoseconds = (earliest_time - next_frame_allowed_at).as_nanos();
        let interval_nanoseconds = self.frame_interval.as_nanos();
        let grid_step_count = late_nanoseconds.div_ceil(interval_nanoseconds);
        let Ok(grid_step_nanoseconds) = u64::try_from(grid_step_count * interval_nanoseconds)
        else {
            return earliest_time;
        };
        next_frame_allowed_at
            .checked_add(Duration::from_nanos(grid_step_nanoseconds))
            .unwrap_or(earliest_time)
    }

    /// Record a frame that went at `sent_at`: clear the view change and the
    /// allowance that [`allow_next_frame_now`](Self::allow_next_frame_now)
    /// gave, and allow the next frame one interval further along the grid, as
    /// the type states.
    pub fn record_frame_sent(&mut self, sent_at: Instant) {
        let next_frame_allowed_at = match self.next_frame_allowed_at {
            Some(allowed_at)
                if sent_at >= allowed_at && sent_at < allowed_at + self.frame_interval =>
            {
                allowed_at + self.frame_interval
            }
            _ => sent_at + self.frame_interval,
        };
        self.next_frame_allowed_at = Some(next_frame_allowed_at);
        self.has_view_change = false;
        self.is_next_frame_immediate = false;
    }
}

/// Each pane's next timed render, ordered by time. A pane with no timed render
/// has no entry.
///
/// Example — pane `A` due at `t + 5 ms` and pane `B` due at `t + 2 ms`:
/// [`find_earliest_render_deadline`](Self::find_earliest_render_deadline) gives
/// `t + 2 ms`, and [`take_due_pane_ids`](Self::take_due_pane_ids) at `t + 3 ms`
/// gives `[B]` and leaves `A`.
#[derive(Debug, Default)]
pub struct PaneRenderDeadlines {
    /// Each pane's deadline.
    render_deadline_by_pane_id: HashMap<PaneId, Instant>,
    /// The same deadlines, earliest first.
    pane_ids_by_render_deadline: BTreeSet<(Instant, PaneId)>,
}

impl PaneRenderDeadlines {
    /// Set `pane_id`'s deadline to `render_deadline`, replacing the one it
    /// held. `None` removes the pane's entry.
    pub fn set_pane_render_deadline(&mut self, pane_id: PaneId, render_deadline: Option<Instant>) {
        if let Some(previous_render_deadline) = self.render_deadline_by_pane_id.remove(&pane_id) {
            self.pane_ids_by_render_deadline
                .remove(&(previous_render_deadline, pane_id));
        }
        if let Some(render_deadline) = render_deadline {
            self.render_deadline_by_pane_id
                .insert(pane_id, render_deadline);
            self.pane_ids_by_render_deadline
                .insert((render_deadline, pane_id));
        }
    }

    /// The earliest deadline any pane holds, or `None` when no pane holds one.
    pub fn find_earliest_render_deadline(&self) -> Option<Instant> {
        self.pane_ids_by_render_deadline
            .first()
            .map(|(render_deadline, _)| *render_deadline)
    }

    /// Every pane's deadline, earliest first.
    pub fn list_render_deadlines(&self) -> impl Iterator<Item = (Instant, PaneId)> + '_ {
        self.pane_ids_by_render_deadline.iter().copied()
    }

    /// Remove every pane whose deadline is at or before `current_time`, and
    /// return those panes, earliest deadline first.
    pub fn take_due_pane_ids(&mut self, current_time: Instant) -> Vec<PaneId> {
        let mut due_pane_ids = Vec::new();
        while let Some((render_deadline, pane_id)) =
            self.pane_ids_by_render_deadline.first().copied()
        {
            if render_deadline > current_time {
                break;
            }
            self.pane_ids_by_render_deadline.pop_first();
            self.render_deadline_by_pane_id.remove(&pane_id);
            due_pane_ids.push(pane_id);
        }
        due_pane_ids
    }
}

#[cfg(test)]
pub(crate) mod tests;
