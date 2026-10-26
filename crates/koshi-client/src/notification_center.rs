//! The viewer's notification center and toast: posting and taking entries,
//! opening, closing, scrolling and clearing the center, selecting and
//! dismissing its cards, the clicks on the center, on the toast's close
//! control and on the tabline's notification button, and the clocks of the
//! toast and of the center's slide.
//!
//! The center and the toast are this viewer's alone. Nothing here sends a
//! request to the session. The open center holds the viewer's keys
//! (`LockMode::NotificationCenter`). A press off the open center closes it,
//! and the press then acts as it does with no center.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use koshi_core::constant::compute_frame_interval;
use koshi_core::ids::SessionId;
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind, ScrollDirection};
use koshi_core::notification::{
    NotificationEntry, NotificationId, NotificationQueue, NotificationSeverity,
};
use koshi_renderer::notification::{
    compute_notification_panel_column_count, find_drawn_notification_center_layout,
    find_notification_card_at, find_notification_card_row_range, NotificationCenterLayout,
};
use koshi_renderer::snapshot::{
    CommittedRegions, DrawnNotifications, FrameLayout, LinkStatus, MouseFrame, NotificationButton,
    NotificationCenter, NotificationToast, RenderSnapshot, ViewerChrome,
};
use koshi_renderer::{resolve_hit_region, HitRegion};

use crate::mouse::{build_frame_layout_without_overlays, MouseAction};
use crate::notification_history::NotificationTakeError;
use crate::notification_toast::ToastTiming;
use crate::Client;

impl Client {
    /// Post `text` from koshi at `severity` `post_count` times in a row to
    /// this viewer's own queue, the last post arriving at `created_at`
    /// ([`NotificationHistory::post_core_entry`](crate::notification_history::NotificationHistory::post_core_entry)).
    /// The entry queues a toast preview while toasts are on and the center is
    /// closed.
    pub(crate) fn post_core_notification(
        &mut self,
        severity: NotificationSeverity,
        text: &str,
        post_count: NonZeroU32,
        created_at: SystemTime,
    ) {
        let should_show_toasts = self.client_config.notifications.should_show_toasts;
        self.notification_history.post_core_entry(
            severity,
            text,
            post_count,
            created_at,
            should_show_toasts,
        );
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

    /// Take one `NotificationPosted` from the session: `posted_entry` at the
    /// queue's `revision`
    /// ([`NotificationHistory::take_session_post`](crate::notification_history::NotificationHistory::take_session_post)),
    /// with toasts as this viewer's settings say.
    ///
    /// # Errors
    /// The [`NotificationTakeError`] of a post the history does not take.
    pub(crate) fn take_session_notification_post(
        &mut self,
        revision: u64,
        posted_entry: NotificationEntry,
    ) -> Result<(), NotificationTakeError> {
        let should_show_toasts = self.client_config.notifications.should_show_toasts;
        self.notification_history
            .take_session_post(revision, posted_entry, should_show_toasts)
    }

    /// Close the center while it is open, and open it
    /// ([`open_notification_center`](Self::open_notification_center)) over
    /// `mouse_frame`, the frame this viewer last painted, while it is closed
    /// or slides out.
    pub(crate) fn toggle_notification_center(&mut self, mouse_frame: Option<&MouseFrame>) {
        if self.notification_history.is_notification_center_open() {
            self.close_notification_center();
        } else {
            self.open_notification_center(mouse_frame);
        }
    }

    /// Open the center on the right side of the pane area, scrolled to its
    /// top with its newest card selected and holding the viewer's keys. It
    /// slides in, or shows at once with `reduced-motion`. Every entry counts
    /// as read and every toast preview goes. Drops an open key sequence.
    ///
    /// Opens nothing while another local submode is on
    /// ([`find_viewer_local_input_mode`](Self::find_viewer_local_input_mode)),
    /// such as the floating pane tray, while the viewer is not
    /// [`LinkStatus::Linked`], with no painted frame, and when
    /// `mouse_frame` has no room for the center
    /// ([`find_drawn_notification_center_layout`]): a pane area narrower than
    /// 10 columns or shorter than 6 rows, or every pane suppressed.
    pub(crate) fn open_notification_center(&mut self, mouse_frame: Option<&MouseFrame>) {
        if self.find_viewer_local_input_mode().is_some() || self.link_status != LinkStatus::Linked {
            return;
        }
        let Some(mouse_frame) = mouse_frame else {
            return;
        };
        let resting_center = NotificationCenter {
            scroll_row_offset: 0,
            selected_notification_id: None,
            hidden_per_mille: 0,
        };
        if find_drawn_notification_center_layout(build_center_frame_layout(
            mouse_frame,
            resting_center,
        ))
        .is_none()
        {
            return;
        }
        self.notification_history
            .open_notification_center(self.client_config.should_reduce_motion);
        self.pending_key_sequence = None;
    }

    /// Close the open center and drop an open key sequence. The entries
    /// stay. The center slides out, or goes at once with `reduced-motion`.
    /// Does nothing while the center is closed or slides out.
    pub(crate) fn close_notification_center(&mut self) {
        if self.notification_history.is_notification_center_open() {
            self.notification_history
                .close_notification_center(self.client_config.should_reduce_motion);
            self.pending_key_sequence = None;
        }
    }

    /// Remove every entry from the open center and close it
    /// ([`NotificationHistory::clear_merged_entries`](crate::notification_history::NotificationHistory::clear_merged_entries),
    /// [`close_notification_center`](Self::close_notification_center)). Does
    /// nothing while the center is closed or slides out.
    pub(crate) fn clear_notifications(&mut self) {
        if self.notification_history.is_notification_center_open() {
            self.notification_history.clear_merged_entries();
            self.close_notification_center();
        }
    }

    /// Remove the selected card's entry from the open center
    /// ([`NotificationHistory::dismiss_merged_entry`](crate::notification_history::NotificationHistory::dismiss_merged_entry)),
    /// and bring the card selected next into view as `mouse_frame`, the frame
    /// this viewer last painted, lays out the center
    /// ([`reveal_notification_card`](Self::reveal_notification_card)). Does
    /// nothing while the center is not open or selects no card.
    pub(crate) fn dismiss_selected_notification(&mut self, mouse_frame: Option<&MouseFrame>) {
        if !self.notification_history.is_notification_center_open() {
            return;
        }
        let Some(selected_notification_id) = self
            .notification_history
            .get_notification_center()
            .and_then(|notification_center| notification_center.selected_notification_id)
        else {
            return;
        };
        self.notification_history
            .dismiss_merged_entry(selected_notification_id);
        if let Some(next_notification_id) = self
            .notification_history
            .get_notification_center()
            .and_then(|notification_center| notification_center.selected_notification_id)
        {
            self.reveal_notification_card(next_notification_id, mouse_frame);
        }
    }

    /// Select the card one entry newer than the selected one with
    /// `is_selecting_newer`, and one entry older otherwise, and bring it into
    /// view as `mouse_frame`, the frame this viewer last painted, lays out the
    /// center ([`reveal_notification_card`](Self::reveal_notification_card)).
    /// Does nothing while the center is not open, and at the newest or the
    /// oldest card in that direction.
    pub(crate) fn select_adjacent_notification(
        &mut self,
        is_selecting_newer: bool,
        mouse_frame: Option<&MouseFrame>,
    ) {
        if let Some(selected_notification_id) = self
            .notification_history
            .select_adjacent_entry(is_selecting_newer)
        {
            self.reveal_notification_card(selected_notification_id, mouse_frame);
        }
    }

    /// Scroll the open center the fewest rows that show the whole card of the
    /// entry `notification_id`, as `mouse_frame` lays the center out. A card
    /// taller than the body shows its top rows. Does nothing while the center
    /// is not open, with no painted frame, and when the frame has no room for
    /// the center.
    ///
    /// Example: in a 17-row body drawn from row `0`, a card on rows `15..20`
    /// scrolls the body to row `3`, and a card on rows `0..5` drawn from row
    /// `3` scrolls it to row `0`.
    fn reveal_notification_card(
        &mut self,
        notification_id: NotificationId,
        mouse_frame: Option<&MouseFrame>,
    ) {
        let Some((notification_center, center_layout)) = self.find_open_center_layout(mouse_frame)
        else {
            return;
        };
        let notification_view = self.notification_history.build_notification_view();
        let body_rect = center_layout.body_rect;
        let Some(card_row_range) = find_notification_card_row_range(
            notification_view,
            body_rect.size.column_count,
            notification_id,
        ) else {
            return;
        };
        let body_row_count = usize::from(body_rect.size.row_count);
        let drawn_scroll_row_offset = notification_view.compute_drawn_center_scroll_row_offset(
            body_rect,
            notification_center.scroll_row_offset,
        );
        let scroll_row_offset = if card_row_range.start < drawn_scroll_row_offset {
            card_row_range.start
        } else if card_row_range.end > drawn_scroll_row_offset + body_row_count {
            (card_row_range.end - body_row_count).min(card_row_range.start)
        } else {
            drawn_scroll_row_offset
        };
        self.notification_history
            .set_notification_center_scroll_row_offset(scroll_row_offset);
    }

    /// Scroll the open center `row_count` rows toward its newest entry with
    /// `is_scrolling_up`, and toward its oldest entry otherwise, as
    /// `mouse_frame`, the frame this viewer last painted, lays it out. The
    /// scroll starts from the row the center draws from, and stops at the
    /// body's first row and at the row that puts the center's last row on the
    /// body's last row
    /// ([`NotificationView::compute_drawn_center_scroll_row_offset`](koshi_renderer::notification::NotificationView::compute_drawn_center_scroll_row_offset)).
    /// Does nothing while the center is not open, with no painted frame, and
    /// when the frame has no room for the center.
    ///
    /// A center whose 17 body rows list 30 rows scrolls from row `0` to row
    /// `13` at most. At row `13`, a resize that gives the body 25 rows makes
    /// it draw from row `5`, and one row up then scrolls it to row `4`.
    pub(crate) fn scroll_notification_center(
        &mut self,
        is_scrolling_up: bool,
        row_count: usize,
        mouse_frame: Option<&MouseFrame>,
    ) {
        let Some((notification_center, center_layout)) = self.find_open_center_layout(mouse_frame)
        else {
            return;
        };
        let notification_view = self.notification_history.build_notification_view();
        let body_rect = center_layout.body_rect;
        let drawn_scroll_row_offset = notification_view.compute_drawn_center_scroll_row_offset(
            body_rect,
            notification_center.scroll_row_offset,
        );
        let scroll_row_offset = if is_scrolling_up {
            drawn_scroll_row_offset.saturating_sub(row_count)
        } else {
            notification_view.compute_drawn_center_scroll_row_offset(
                body_rect,
                drawn_scroll_row_offset.saturating_add(row_count),
            )
        };
        self.notification_history
            .set_notification_center_scroll_row_offset(scroll_row_offset);
    }

    /// The open center and where `mouse_frame` lays it out, or `None` while
    /// the center is not open, with no painted frame, and when the frame has
    /// no room for the center.
    fn find_open_center_layout(
        &self,
        mouse_frame: Option<&MouseFrame>,
    ) -> Option<(NotificationCenter, NotificationCenterLayout)> {
        if !self.notification_history.is_notification_center_open() {
            return None;
        }
        let notification_center = self.notification_history.get_notification_center()?;
        let center_layout = find_drawn_notification_center_layout(build_center_frame_layout(
            mouse_frame?,
            notification_center,
        ))?;
        Some((notification_center, center_layout))
    }

    /// Remove the center at once when the frame `render_snapshot`, painted
    /// with `committed_regions`, has no room for it
    /// ([`find_drawn_notification_center_layout`]): a pane area narrower than
    /// 10 columns or shorter than 6 rows, or every pane suppressed. The
    /// center then stays closed when the frame grows again. Does nothing
    /// while the center is closed.
    pub(crate) fn update_notification_center(
        &mut self,
        render_snapshot: &RenderSnapshot,
        committed_regions: &CommittedRegions,
    ) {
        let Some(notification_center) = self.notification_history.get_notification_center() else {
            return;
        };
        let frame_layout = render_snapshot.build_painted_frame_layout(
            ViewerChrome {
                notification_center: Some(notification_center),
                ..ViewerChrome::default()
            },
            committed_regions,
        );
        if find_drawn_notification_center_layout(frame_layout).is_none() {
            self.notification_history.remove_notification_center();
            self.pending_key_sequence = None;
        }
    }

    /// Answer a press on the toast's close control ` ✕ ` as `mouse_frame`,
    /// the frame this viewer last painted, drew it: a left press withdraws the
    /// toast that frame drew ([`DrawnNotifications::toast_number`]) while it
    /// still shows, and its entry stays in the center. Any press on the
    /// control is consumed and sends nothing. Returns `None` for every other
    /// event, which takes the next mouse path.
    ///
    /// Example: the frame drew toast `0`, and toast `1` replaced it before
    /// the next paint. A left press on the drawn ` ✕ ` leaves toast `1` as it
    /// is.
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
        if let (MouseButton::Left, Some(toast_number)) =
            (pressed_button, mouse_frame.drawn_notifications.toast_number)
        {
            self.notification_history
                .request_toast_withdraw(toast_number);
        }
        Some(Vec::new())
    }

    /// Answer `mouse_input` against `mouse_frame`, the frame this viewer last
    /// painted, while the center shows: open, or sliding out. The event is
    /// placed against the center as this viewer holds it
    /// ([`build_center_frame_layout`]). Returns the actions to send, or `None`
    /// when the event takes the next mouse path. Every event takes the next
    /// path while the center is closed.
    ///
    /// On the open center:
    ///
    /// - A left press on the close button closes the center, and a left
    ///   press on `Clear all` clears it
    ///   ([`clear_notifications`](Self::clear_notifications)).
    /// - A left press on a card's dismiss button removes that card's entry
    ///   ([`NotificationHistory::dismiss_merged_entry`](crate::notification_history::NotificationHistory::dismiss_merged_entry)),
    ///   and a left press elsewhere on a card selects it. The card is the one
    ///   `mouse_frame` drew under the press
    ///   ([`handle_notification_card_press`](Self::handle_notification_card_press)).
    /// - A wheel tick over the center scrolls it by `mouse.scroll-lines`
    ///   rows: up toward its newest entry, down toward its oldest. A sideways
    ///   tick does nothing.
    ///
    /// Off the open center, a press closes the center
    /// ([`close_notification_center`](Self::close_notification_center)) and
    /// returns `None`: the press then acts as it does with no center. A press
    /// on the notification button `mouse_frame` shows closes the center and
    /// sends nothing.
    ///
    /// Every other press and wheel tick over the center, and every event over
    /// a center that slides out, does nothing more. A pointer motion over the
    /// center clears the hovered pane and the placement handle hover. Every
    /// event the center takes sends nothing. A drag and a release return
    /// `None`, and a gesture under way goes on.
    pub(crate) fn handle_notification_center_mouse(
        &mut self,
        mouse_input: MouseInput,
        mouse_frame: &MouseFrame,
    ) -> Option<Vec<MouseAction>> {
        let notification_center = self.notification_history.get_notification_center()?;
        let is_center_open = self.notification_history.is_notification_center_open();
        let center_frame_layout = build_center_frame_layout(mouse_frame, notification_center);
        let hit_region = resolve_hit_region(center_frame_layout, mouse_input.position);
        let is_on_center = matches!(
            hit_region,
            HitRegion::NotificationCenterClose
                | HitRegion::NotificationCenterClearAll
                | HitRegion::NotificationCenterBody
                | HitRegion::NotificationCenter
        );
        match mouse_input.mouse_kind {
            MouseKind::Press(MouseButton::Left) if is_on_center && is_center_open => {
                match hit_region {
                    HitRegion::NotificationCenterClose => self.close_notification_center(),
                    HitRegion::NotificationCenterClearAll => self.clear_notifications(),
                    HitRegion::NotificationCenterBody => {
                        self.handle_notification_card_press(mouse_input, mouse_frame);
                    }
                    _ => {}
                }
                Some(Vec::new())
            }
            MouseKind::Press(_) if !is_on_center && is_center_open => {
                self.close_notification_center();
                let painted_hit_region = resolve_hit_region(
                    build_frame_layout_without_overlays(mouse_frame),
                    mouse_input.position,
                );
                (painted_hit_region == HitRegion::NotificationButton).then(Vec::new)
            }
            MouseKind::Scroll(scroll_direction) if is_on_center => {
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
            MouseKind::Press(_) if is_on_center => Some(Vec::new()),
            _ => None,
        }
    }

    /// Answer a left press of `mouse_input` on the body of the open center,
    /// on the card `mouse_frame`, the frame this viewer last painted, drew
    /// under it ([`DrawnNotifications::center_card_rows`]): a press on that
    /// card's dismiss button removes its entry, and a press elsewhere on it
    /// selects it. A press on no drawn card, and on a card whose entry is
    /// gone, does nothing.
    ///
    /// Example: the frame drew entry `b` as the newest card, and entry `c`
    /// arrived before the next paint. A press on the drawn `✕` of `b` removes
    /// `b`, and `c` stays.
    fn handle_notification_card_press(
        &mut self,
        mouse_input: MouseInput,
        mouse_frame: &MouseFrame,
    ) {
        let Some(center_layout) =
            find_drawn_notification_center_layout(mouse_frame.build_frame_layout())
        else {
            return;
        };
        let Some(resting_point) = center_layout.find_resting_point(mouse_input.position) else {
            return;
        };
        let Some(card_hit) = find_notification_card_at(
            &mouse_frame.drawn_notifications.center_card_rows,
            &center_layout,
            resting_point,
        ) else {
            return;
        };
        if card_hit.is_on_dismiss_button {
            self.notification_history
                .dismiss_merged_entry(card_hit.notification_id);
        } else {
            self.notification_history
                .select_entry(card_hit.notification_id);
        }
    }

    /// Advance the toast's clock to `current_time`, stopped while
    /// `is_toast_paused`, and the center's slide, with the timing this
    /// viewer's settings give
    /// ([`NotificationHistory::advance_notification_clocks`](crate::notification_history::NotificationHistory::advance_notification_clocks)).
    pub(crate) fn advance_notification_clocks(
        &mut self,
        current_time: Instant,
        is_toast_paused: bool,
    ) {
        let toast_timing = self.get_toast_timing();
        self.notification_history.advance_notification_clocks(
            current_time,
            toast_timing,
            is_toast_paused,
        );
    }

    /// How long from `current_time` until the toast's clock or the center's
    /// slide next has something to move, or `None` while neither has
    /// ([`NotificationHistory::find_next_notification_wakeup`](crate::notification_history::NotificationHistory::find_next_notification_wakeup)).
    /// A slide redraws at most once per frame interval of
    /// `max-frames-per-second`.
    #[must_use]
    pub(crate) fn compute_next_notification_wakeup(
        &self,
        current_time: Instant,
    ) -> Option<Duration> {
        self.notification_history.find_next_notification_wakeup(
            current_time,
            self.get_toast_timing(),
            compute_frame_interval(self.client_config.max_frames_per_second),
        )
    }

    /// The notification cards and the toast a frame laid out by
    /// `frame_layout` draws now: the card each body row of its center draws,
    /// shared with the rows the center keeps
    /// ([`NotificationView::compute_center_body_rows`](koshi_renderer::notification::NotificationView::compute_center_body_rows)),
    /// and the number of the shown toast while `frame_layout` draws a toast.
    #[must_use]
    pub(crate) fn build_drawn_notifications(
        &self,
        frame_layout: FrameLayout<'_>,
    ) -> DrawnNotifications {
        let center_card_rows = match (
            frame_layout.viewer_chrome.notification_center,
            find_drawn_notification_center_layout(frame_layout),
        ) {
            (Some(notification_center), Some(center_layout)) => {
                self.notification_history
                    .build_notification_view()
                    .compute_center_body_rows(
                        center_layout.body_rect,
                        notification_center.scroll_row_offset,
                    )
                    .card_rows
            }
            _ => Arc::default(),
        };
        DrawnNotifications {
            center_card_rows,
            toast_number: frame_layout
                .viewer_chrome
                .notification_toast
                .and(self.notification_history.find_shown_toast_number()),
        }
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
    /// of the panel width for the pane area of `committed_regions`
    /// ([`compute_notification_panel_column_count`],
    /// [`NotificationToastView::compute_toast_text_rows`]), and slid as far as
    /// its clock says. `None` while no toast shows.
    ///
    /// The text is wrapped only when the toast's text or the panel width
    /// changed since the last ask: a toast that slides wraps its text once.
    ///
    /// [`NotificationToastView::compute_toast_text_rows`]: koshi_renderer::notification::NotificationToastView::compute_toast_text_rows
    #[must_use]
    pub(crate) fn build_notification_toast(
        &self,
        committed_regions: &CommittedRegions,
    ) -> Option<NotificationToast> {
        let (shown_toast, hidden_per_mille) = self.notification_history.find_shown_toast()?;
        let toast_column_count = compute_notification_panel_column_count(
            committed_regions.solved_regions.pane_rect.size.column_count,
        );
        let text_row_count = shown_toast
            .compute_toast_text_rows(toast_column_count)
            .text_rows
            .len() as u16;
        Some(NotificationToast {
            toast_position: self.client_config.notifications.toast_position,
            text_row_count,
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

/// `mouse_frame`'s frame layout with `notification_center` shown: the viewer
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
