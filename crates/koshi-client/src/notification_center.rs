//! The viewer's notification center and toast: posting and taking entries,
//! opening, closing, focusing, scrolling and clearing the center, the clicks
//! on the center, on the toast's close control and on the tabline's
//! notification button, and the toast's clock.
//!
//! The center and the toast are this viewer's alone. Nothing here sends a
//! request to the session. The open center holds the viewer's keyboard while
//! it is focused (`LockMode::NotificationCenter`): a press off the center
//! takes the focus away and keeps the center open, and a press on the center
//! gives it back.

use std::time::{Duration, Instant, SystemTime};

use koshi_core::ids::SessionId;
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind, ScrollDirection};
use koshi_core::notification::{NotificationEntry, NotificationQueue, NotificationSeverity};
use koshi_renderer::notification::{
    compute_maximum_center_scroll_row_offset, compute_notification_panel_column_count,
    count_toast_text_rows, find_drawn_notification_center_layout,
};
use koshi_renderer::snapshot::{
    FrameLayout, MouseFrame, NotificationButton, NotificationCenter, NotificationToast,
    ViewerChrome,
};
use koshi_renderer::{resolve_hit_region, HitRegion};

use crate::mouse::MouseAction;
use crate::notification_history::NotificationTakeError;
use crate::notification_toast::ToastTiming;
use crate::Client;

impl Client {
    /// Post `text` from koshi at `severity` to this viewer's own queue,
    /// arriving at `created_at`
    /// ([`NotificationHistory::post_core_entry`](crate::notification_history::NotificationHistory::post_core_entry)).
    /// The entry queues a toast preview while toasts are on and the center is
    /// closed.
    pub(crate) fn post_core_notification(
        &mut self,
        severity: NotificationSeverity,
        text: &str,
        created_at: SystemTime,
    ) {
        let should_show_toasts = self.client_config.notifications.should_show_toasts;
        self.notification_history
            .post_core_entry(severity, text, created_at, should_show_toasts);
    }

    /// Take `notification_queue`, the queue of session `session_id`
    /// ([`NotificationHistory::take_session_queue`](crate::notification_history::NotificationHistory::take_session_queue)),
    /// with toasts as this viewer's settings say.
    ///
    /// # Errors
    /// The [`NotificationTakeError`] of a queue the history does not take.
    pub(crate) fn take_session_notification_queue(
        &mut self,
        session_id: SessionId,
        notification_queue: &NotificationQueue,
    ) -> Result<(), NotificationTakeError> {
        let should_show_toasts = self.client_config.notifications.should_show_toasts;
        self.notification_history.take_session_queue(
            session_id,
            notification_queue,
            should_show_toasts,
        )
    }

    /// Take one `NotificationPosted` from the session: `entry` at the queue's
    /// `revision`
    /// ([`NotificationHistory::take_session_post`](crate::notification_history::NotificationHistory::take_session_post)),
    /// with toasts as this viewer's settings say.
    ///
    /// # Errors
    /// The [`NotificationTakeError`] of a post the history does not take.
    pub(crate) fn take_session_notification_post(
        &mut self,
        revision: u64,
        entry: NotificationEntry,
    ) -> Result<(), NotificationTakeError> {
        let should_show_toasts = self.client_config.notifications.should_show_toasts;
        self.notification_history
            .take_session_post(revision, entry, should_show_toasts)
    }

    /// Close the center while it is open, and open it
    /// ([`open_notification_center`](Self::open_notification_center)) while
    /// it is closed. An open center closes whether or not it holds the
    /// keyboard.
    pub(crate) fn toggle_notification_center(&mut self) {
        if self
            .notification_history
            .get_notification_center()
            .is_some()
        {
            self.close_notification_center();
        } else {
            self.open_notification_center();
        }
    }

    /// Open the center on the right side of the pane area, scrolled to its
    /// newest entry and holding the keyboard. Every entry counts as read and
    /// every toast preview goes. Drops an open key sequence.
    ///
    /// Opens nothing while another local submode is on
    /// ([`find_viewer_local_input_mode`](Self::find_viewer_local_input_mode)).
    pub(crate) fn open_notification_center(&mut self) {
        if self.find_viewer_local_input_mode().is_some() {
            return;
        }
        self.notification_history.open_notification_center();
        self.pending_key_sequence = None;
    }

    /// Close the center and drop an open key sequence. The entries stay.
    /// Does nothing while the center is closed.
    pub(crate) fn close_notification_center(&mut self) {
        if self
            .notification_history
            .get_notification_center()
            .is_some()
        {
            self.notification_history.close_notification_center();
            self.pending_key_sequence = None;
        }
    }

    /// Remove every entry from the center and close it
    /// ([`NotificationHistory::clear_entries`](crate::notification_history::NotificationHistory::clear_entries)),
    /// and drop an open key sequence. Does nothing while the center is
    /// closed.
    pub(crate) fn clear_notifications(&mut self) {
        if self
            .notification_history
            .get_notification_center()
            .is_some()
        {
            self.notification_history.clear_entries();
            self.pending_key_sequence = None;
        }
    }

    /// Give the open center the keyboard. Does nothing while the center is
    /// closed, and while another local submode is on
    /// ([`find_viewer_local_input_mode`](Self::find_viewer_local_input_mode)).
    fn focus_notification_center(&mut self) {
        if self.find_viewer_local_input_mode().is_some() {
            return;
        }
        self.notification_history
            .set_notification_center_focus(true);
    }

    /// Take the keyboard from the open center, which stays open, and drop an
    /// open key sequence of the center's mode. Does nothing while the center
    /// is closed or does not hold the keyboard.
    pub(crate) fn unfocus_notification_center(&mut self) {
        let is_focused = self
            .notification_history
            .get_notification_center()
            .is_some_and(|notification_center| notification_center.is_focused);
        if is_focused {
            self.notification_history
                .set_notification_center_focus(false);
            self.pending_key_sequence = None;
        }
    }

    /// Scroll the open center `row_count` rows toward its newest entry with
    /// `is_scrolling_up`, and toward its oldest entry otherwise, as
    /// `mouse_frame`, the frame this viewer last painted, lays it out. The
    /// scroll starts from the row the center draws from, and stops at the
    /// body's first row and at [`compute_maximum_center_scroll_row_offset`].
    /// Does nothing while the center is closed, with no painted frame, and
    /// when the frame has no room for the center.
    ///
    /// A center whose 20 body rows list 30 rows scrolls from row `0` to row
    /// `10` at most. At row `10`, a resize that gives the body 25 rows makes
    /// it draw from row `5`, and one row up then scrolls it to row `4`.
    pub(crate) fn scroll_notification_center(
        &mut self,
        is_scrolling_up: bool,
        row_count: usize,
        mouse_frame: Option<&MouseFrame>,
    ) {
        let (Some(notification_center), Some(mouse_frame)) = (
            self.notification_history.get_notification_center(),
            mouse_frame,
        ) else {
            return;
        };
        let Some(center_layout) = find_drawn_notification_center_layout(build_center_frame_layout(
            mouse_frame,
            notification_center,
        )) else {
            return;
        };
        let maximum_scroll_row_offset = compute_maximum_center_scroll_row_offset(
            self.notification_history.build_notification_view(),
            center_layout.body_rect,
        );
        let drawn_scroll_row_offset = notification_center
            .scroll_row_offset
            .min(maximum_scroll_row_offset);
        let scroll_row_offset = if is_scrolling_up {
            drawn_scroll_row_offset.saturating_sub(row_count)
        } else {
            drawn_scroll_row_offset
                .saturating_add(row_count)
                .min(maximum_scroll_row_offset)
        };
        self.notification_history
            .set_notification_center_scroll_row_offset(scroll_row_offset);
    }

    /// Answer a press on the toast's close control ` × ` as `mouse_frame`,
    /// the frame this viewer last painted, drew it: a left press withdraws the
    /// toast, and its entry stays in the center. Any press on the control is
    /// consumed and sends nothing. Returns `None` for every other event, which
    /// takes the next mouse path.
    pub(crate) fn handle_notification_toast_mouse(
        &mut self,
        mouse_input: MouseInput,
        mouse_frame: &MouseFrame,
    ) -> Option<Vec<MouseAction>> {
        let MouseKind::Press(pressed_button) = mouse_input.mouse_kind else {
            return None;
        };
        if resolve_hit_region(mouse_frame.build_frame_layout(), mouse_input.position)
            != HitRegion::NotificationToastClose
        {
            return None;
        }
        if pressed_button == MouseButton::Left {
            self.notification_history.request_toast_withdraw();
        }
        Some(Vec::new())
    }

    /// Answer `mouse_input` against `mouse_frame`, the frame this viewer last
    /// painted, while the center is open. The event is placed against the
    /// center as this viewer holds it ([`build_center_frame_layout`]).
    /// Returns the actions to send, or `None` when the event takes the next
    /// mouse path. Every event takes the next path while the center is
    /// closed.
    ///
    /// - A left press on the close control closes the center, and a left
    ///   press on the clear label clears it
    ///   ([`clear_notifications`](Self::clear_notifications)). Any other
    ///   press on the center gives it the keyboard
    ///   ([`focus_notification_center`](Self::focus_notification_center)).
    /// - A press off the center takes the keyboard from it, and returns
    ///   `None`: the press then acts as it does with no center.
    /// - A wheel tick scrolls the center by `mouse.scroll-lines` rows
    ///   while it is over the center, and while the center holds the
    ///   keyboard: up toward its newest entry, down toward its oldest. A
    ///   sideways tick there does nothing.
    /// - A pointer motion over the center clears the hovered pane and the
    ///   placement handle hover.
    /// - Every event above that the center takes sends nothing. A drag and a
    ///   release return `None`, and a gesture under way goes on.
    pub(crate) fn handle_notification_center_mouse(
        &mut self,
        mouse_input: MouseInput,
        mouse_frame: &MouseFrame,
    ) -> Option<Vec<MouseAction>> {
        let notification_center = self.notification_history.get_notification_center()?;
        let hit_region = resolve_hit_region(
            build_center_frame_layout(mouse_frame, notification_center),
            mouse_input.position,
        );
        let is_on_center = matches!(
            hit_region,
            HitRegion::NotificationCenterClose
                | HitRegion::NotificationCenterClear
                | HitRegion::NotificationCenter
        );
        match mouse_input.mouse_kind {
            MouseKind::Press(pressed_button) if is_on_center => {
                match (pressed_button, hit_region) {
                    (MouseButton::Left, HitRegion::NotificationCenterClose) => {
                        self.close_notification_center();
                    }
                    (MouseButton::Left, HitRegion::NotificationCenterClear) => {
                        self.clear_notifications();
                    }
                    _ => self.focus_notification_center(),
                }
                Some(Vec::new())
            }
            MouseKind::Press(_) => {
                self.unfocus_notification_center();
                None
            }
            MouseKind::Scroll(scroll_direction)
                if is_on_center || notification_center.is_focused =>
            {
                let row_count = usize::from(self.client_config.mouse.scroll_line_count);
                match scroll_direction {
                    ScrollDirection::Up => {
                        self.scroll_notification_center(true, row_count, Some(mouse_frame));
                    }
                    ScrollDirection::Down => {
                        self.scroll_notification_center(false, row_count, Some(mouse_frame));
                    }
                    ScrollDirection::Left | ScrollDirection::Right => {}
                }
                Some(Vec::new())
            }
            MouseKind::Motion if is_on_center => {
                self.hovered_pane_id = None;
                self.placement_handle_hover = None;
                Some(Vec::new())
            }
            _ => None,
        }
    }

    /// Advance the toast's clock to `current_time`, stopped while
    /// `is_paused`, with the phase lengths this viewer's settings give
    /// ([`NotificationHistory::advance_notification_toast`](crate::notification_history::NotificationHistory::advance_notification_toast)).
    pub(crate) fn advance_notification_toast(&mut self, current_time: Instant, is_paused: bool) {
        let toast_timing = self.get_toast_timing();
        self.notification_history
            .advance_notification_toast(current_time, toast_timing, is_paused);
    }

    /// How long from `current_time` until the toast's clock next has
    /// something to move, or `None` while it has nothing to move
    /// ([`NotificationHistory::find_next_toast_wakeup`](crate::notification_history::NotificationHistory::find_next_toast_wakeup)).
    #[must_use]
    pub(crate) fn compute_next_notification_wakeup(
        &self,
        current_time: Instant,
    ) -> Option<Duration> {
        self.notification_history
            .find_next_toast_wakeup(current_time, self.get_toast_timing())
    }

    /// The tabline's notification button while the history holds an entry,
    /// with the count of unread entries, or `None`.
    #[must_use]
    pub(crate) fn build_notification_button(&self) -> Option<NotificationButton> {
        self.notification_history
            .list_merged_entries()
            .next()
            .map(|_| NotificationButton {
                unread_entry_count: self.notification_history.get_unread_entry_count(),
            })
    }

    /// The shown toast as this viewer draws it: in the corner its
    /// `toast-position` names, with the text rows its preview takes in a box
    /// of the panel width for this viewer's viewport
    /// ([`count_toast_text_rows`]), and slid as far as its clock says. `None`
    /// while no toast shows.
    #[must_use]
    pub(crate) fn build_notification_toast(&self) -> Option<NotificationToast> {
        let (toast_content, hidden_per_mille) = self.notification_history.find_shown_toast()?;
        let toast_column_count =
            compute_notification_panel_column_count(self.viewport_size.column_count);
        Some(NotificationToast {
            toast_position: self.client_config.notifications.toast_position,
            text_row_count: count_toast_text_rows(&toast_content.format_text(), toast_column_count),
            hidden_per_mille,
        })
    }

    /// The phase lengths this viewer's settings give the toast:
    /// `toast-duration-ms` for the stay phase, and `reduced-motion`.
    fn get_toast_timing(&self) -> ToastTiming {
        ToastTiming {
            stay_duration: Duration::from_millis(u64::from(
                self.client_config.notifications.toast_duration_ms,
            )),
            should_reduce_motion: self.client_config.should_reduce_motion,
        }
    }
}

/// `mouse_frame`'s frame layout with `notification_center` open: the viewer
/// chrome it was painted with, the center set to `notification_center`, and
/// no floating pane tray and no toast.
fn build_center_frame_layout(
    mouse_frame: &MouseFrame,
    notification_center: NotificationCenter,
) -> FrameLayout<'_> {
    let mut frame_layout = mouse_frame.build_frame_layout();
    frame_layout.viewer_chrome = ViewerChrome {
        notification_center: Some(notification_center),
        floating_pane_tray: None,
        notification_toast: None,
        ..mouse_frame.viewer_chrome
    };
    frame_layout
}

#[cfg(test)]
mod tests;
