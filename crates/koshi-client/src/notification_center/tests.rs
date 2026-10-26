//! Tests for the viewer's notification center and toast: the keys and the
//! mouse events that open, close, focus, scroll and clear the center, the
//! toast's close control, the notification button, the toast's clock, and
//! the state each viewer keeps for itself.
//!
//! Every frame here is `80x24` with one tiled pane. The open center takes
//! `(54, 1)` `26x22`: its close control ` × ` at `(75, 1)` `3x1`, its clear
//! label at `(56, 22)` `7x1`, and its body at `(56, 2)` `22x20`. A toast in
//! the top-right corner takes the same columns from row 1, its close control
//! at `(75, 1)`.

use std::time::SystemTime;

use koshi_config::types::ToastPosition;
use koshi_core::action::ClientActionKind;
use koshi_core::geometry::Point;
use koshi_core::ids::PaneId;
use koshi_core::key::{BindingModifierFlags, Key, KeyChord, NamedKey};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseKind, ScrollDirection};
use koshi_core::notification::{NotificationQueueOwner, NotificationSeverity};
use koshi_renderer::snapshot::{NotificationButton, NotificationToast, Reconnecting};

use super::*;
use crate::floating_pane_tray::tests::build_tray_mouse_frame;
use crate::input::tests::build_client_viewing;
use crate::input::KeyOutcome;
use crate::mouse::tests::{
    build_left_mouse_press, build_mouse_event, build_mouse_frame, build_plain_mouse_pane,
};
use crate::tests::{build_core_bound_action, build_test_client, TEST_VIEWPORT_SIZE};
use crate::{LinkStatus, PaneMoveMode, PlacementInputAction};

/// A cell of the open center's body.
const CENTER_BODY_CELL: Point = Point {
    column: 60,
    row: 10,
};

/// A cell of the pane's content, left of the open center.
const PANE_CONTENT_CELL: Point = Point {
    column: 10,
    row: 10,
};

/// A cell of the center's close control, and of a top-right toast's.
const CLOSE_CONTROL_CELL: Point = Point { column: 76, row: 1 };

/// A cell of the center's clear label.
const CLEAR_LABEL_CELL: Point = Point {
    column: 58,
    row: 22,
};

/// A client that applied an `80x24` frame of one focused tiled pane, and
/// that frame with no viewer chrome painted.
fn build_center_client() -> (Client, MouseFrame) {
    let pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(&[build_plain_mouse_pane(pane_id)], Some(pane_id));
    (build_client_viewing(&mouse_frame), mouse_frame)
}

/// `mouse_frame` as `client` paints it now: with the viewer chrome `client`
/// holds.
fn paint_mouse_frame(client: &Client, mouse_frame: &MouseFrame) -> MouseFrame {
    let mut painted_mouse_frame = mouse_frame.clone();
    painted_mouse_frame.viewer_chrome =
        client.build_viewer_chrome(mouse_frame.client_snapshot.active_tab_id);
    painted_mouse_frame
}

/// The first tabline cell of the notification button `mouse_frame` paints.
fn find_notification_button_cell(mouse_frame: &MouseFrame) -> Point {
    (0..TEST_VIEWPORT_SIZE.column_count)
        .map(|column| Point { column, row: 0 })
        .find(|cell| {
            resolve_hit_region(mouse_frame.build_frame_layout(), *cell)
                == HitRegion::NotificationButton
        })
        .expect("the tabline draws the notification button")
}

/// Post `entry_count` `Info` entries to `client`'s own queue, with the texts
/// `entry 0`, `entry 1`, ….
fn post_entries(client: &mut Client, entry_count: usize) {
    for entry_index in 0..entry_count {
        client.post_core_notification(
            NotificationSeverity::Info,
            &format!("entry {entry_index}"),
            SystemTime::UNIX_EPOCH,
        );
    }
}

/// The texts of `client`'s entries, oldest arrival first.
fn list_entry_texts(client: &Client) -> Vec<String> {
    client
        .get_notification_history()
        .list_merged_entries()
        .map(|entry| entry.text.clone())
        .collect()
}

/// The open center of `client`, or `None` while it is closed.
fn get_center(client: &Client) -> Option<NotificationCenter> {
    client.get_notification_history().get_notification_center()
}

/// An open center scrolled to `scroll_row_offset`, holding the keyboard with
/// `is_focused`.
fn build_open_center(scroll_row_offset: usize, is_focused: bool) -> Option<NotificationCenter> {
    Some(NotificationCenter {
        scroll_row_offset,
        is_focused,
    })
}

/// A wheel tick `scroll_direction` at `position`.
fn build_wheel_tick(scroll_direction: ScrollDirection, position: Point) -> MouseInput {
    build_mouse_event(
        MouseKind::Scroll(scroll_direction),
        position,
        BindingModifierFlags::NONE,
    )
}

/// `start` moved `millisecond_count` milliseconds on.
fn after_milliseconds(start: Instant, millisecond_count: u64) -> Instant {
    start + Duration::from_millis(millisecond_count)
}

#[test]
fn the_toggle_action_opens_the_center_holding_the_keyboard_and_closes_it_with_no_command() {
    let (mut client, mouse_frame) = build_center_client();

    let open_action = client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );
    let opened_center = get_center(&client);
    let mode_while_open = client.get_active_input_mode();
    let close_action = client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );

    assert_eq!(open_action, PlacementInputAction::Consumed);
    assert_eq!(opened_center, build_open_center(0, true));
    assert_eq!(mode_while_open, LockMode::NotificationCenter);
    assert_eq!(close_action, PlacementInputAction::Consumed);
    assert_eq!(get_center(&client), None);
    assert_eq!(client.get_active_input_mode(), LockMode::Normal);
}

#[test]
fn the_keys_of_the_open_center_fire_its_actions() {
    let (mut client, mouse_frame) = build_center_client();
    client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );
    let current_time = Instant::now();

    let key_outcomes: Vec<KeyOutcome> = [
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Esc)),
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char('c')),
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Up)),
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Down)),
        KeyChord::from_parts(BindingModifierFlags::ALT, Key::Char('n')),
    ]
    .into_iter()
    .map(|key_chord| client.resolve_key(key_chord, current_time))
    .collect();

    assert_eq!(
        key_outcomes,
        vec![
            KeyOutcome::Fire(build_core_bound_action("close-notification-center")),
            KeyOutcome::Fire(build_core_bound_action("clear-notifications")),
            KeyOutcome::Fire(build_core_bound_action("scroll-notifications-up")),
            KeyOutcome::Fire(build_core_bound_action("scroll-notifications-down")),
            KeyOutcome::Fire(build_core_bound_action("toggle-notification-center")),
        ]
    );
}

#[test]
fn the_close_action_closes_the_center_and_keeps_every_entry() {
    let (mut client, mouse_frame) = build_center_client();
    post_entries(&mut client, 2);
    client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );

    client.apply_client_action(
        ClientActionKind::CloseNotificationCenter,
        Some(&mouse_frame),
    );

    assert_eq!(get_center(&client), None);
    assert_eq!(list_entry_texts(&client), vec!["entry 0", "entry 1"]);
    assert_eq!(
        client.build_notification_button(),
        Some(NotificationButton {
            unread_entry_count: 0
        })
    );
}

#[test]
fn the_clear_action_removes_every_entry_and_closes_the_center() {
    let (mut client, mouse_frame) = build_center_client();
    post_entries(&mut client, 2);
    client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );

    client.apply_client_action(ClientActionKind::ClearNotifications, Some(&mouse_frame));

    assert_eq!(get_center(&client), None);
    assert_eq!(list_entry_texts(&client), Vec::<String>::new());
    assert_eq!(client.build_notification_button(), None);
}

#[test]
fn the_clear_action_on_a_closed_center_keeps_every_entry() {
    let (mut client, mouse_frame) = build_center_client();
    post_entries(&mut client, 2);

    client.apply_client_action(ClientActionKind::ClearNotifications, Some(&mouse_frame));

    assert_eq!(list_entry_texts(&client), vec!["entry 0", "entry 1"]);
}

#[test]
fn the_scroll_actions_move_the_body_one_row_and_stop_at_its_ends() {
    let (mut client, mouse_frame) = build_center_client();
    post_entries(&mut client, 30);
    client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );

    client.apply_client_action(ClientActionKind::ScrollNotificationsUp, Some(&mouse_frame));
    let above_the_top = get_center(&client);
    client.apply_client_action(
        ClientActionKind::ScrollNotificationsDown,
        Some(&mouse_frame),
    );
    let one_row_down = get_center(&client);
    client.scroll_notification_center(false, 100, Some(&mouse_frame));
    let at_the_bottom = get_center(&client);
    client.apply_client_action(
        ClientActionKind::ScrollNotificationsDown,
        Some(&mouse_frame),
    );
    let below_the_bottom = get_center(&client);
    client.apply_client_action(ClientActionKind::ScrollNotificationsUp, Some(&mouse_frame));

    assert_eq!(above_the_top, build_open_center(0, true));
    assert_eq!(one_row_down, build_open_center(1, true));
    assert_eq!(at_the_bottom, build_open_center(40, true));
    assert_eq!(below_the_bottom, build_open_center(40, true));
    assert_eq!(get_center(&client), build_open_center(39, true));
}

#[test]
fn a_scroll_from_past_the_last_row_starts_from_the_row_the_center_draws() {
    let (mut client, mouse_frame) = build_center_client();
    post_entries(&mut client, 30);
    client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );

    client
        .notification_history
        .set_notification_center_scroll_row_offset(55);
    client.apply_client_action(ClientActionKind::ScrollNotificationsUp, Some(&mouse_frame));
    let after_the_up_action = get_center(&client);
    client
        .notification_history
        .set_notification_center_scroll_row_offset(55);
    client.apply_client_action(
        ClientActionKind::ScrollNotificationsDown,
        Some(&mouse_frame),
    );

    assert_eq!(after_the_up_action, build_open_center(39, true));
    assert_eq!(get_center(&client), build_open_center(40, true));
}

#[test]
fn the_scroll_actions_with_no_painted_frame_leave_the_body_where_it_is() {
    let (mut client, mouse_frame) = build_center_client();
    post_entries(&mut client, 30);
    client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );

    client.apply_client_action(ClientActionKind::ScrollNotificationsDown, None);

    assert_eq!(get_center(&client), build_open_center(0, true));
}

#[test]
fn a_wheel_tick_over_the_center_scrolls_it_by_the_scroll_line_count() {
    let (mut client, mouse_frame) = build_center_client();
    post_entries(&mut client, 30);
    client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );
    client.unfocus_notification_center();

    let down_answer = client.handle_notification_center_mouse(
        build_wheel_tick(ScrollDirection::Down, CENTER_BODY_CELL),
        &mouse_frame,
    );
    let after_the_down_tick = get_center(&client);
    let up_answer = client.handle_notification_center_mouse(
        build_wheel_tick(ScrollDirection::Up, CENTER_BODY_CELL),
        &mouse_frame,
    );
    let sideways_answer = client.handle_notification_center_mouse(
        build_wheel_tick(ScrollDirection::Right, CENTER_BODY_CELL),
        &mouse_frame,
    );

    assert_eq!(down_answer, Some(Vec::new()));
    assert_eq!(after_the_down_tick, build_open_center(3, false));
    assert_eq!(up_answer, Some(Vec::new()));
    assert_eq!(sideways_answer, Some(Vec::new()));
    assert_eq!(get_center(&client), build_open_center(0, false));
}

#[test]
fn a_wheel_tick_off_the_center_scrolls_it_only_while_it_holds_the_keyboard() {
    let (mut client, mouse_frame) = build_center_client();
    post_entries(&mut client, 30);
    client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );

    let focused_answer = client.handle_notification_center_mouse(
        build_wheel_tick(ScrollDirection::Down, PANE_CONTENT_CELL),
        &mouse_frame,
    );
    let after_the_focused_tick = get_center(&client);
    client.unfocus_notification_center();
    let unfocused_answer = client.handle_notification_center_mouse(
        build_wheel_tick(ScrollDirection::Down, PANE_CONTENT_CELL),
        &mouse_frame,
    );

    assert_eq!(focused_answer, Some(Vec::new()));
    assert_eq!(after_the_focused_tick, build_open_center(3, true));
    assert_eq!(unfocused_answer, None);
    assert_eq!(get_center(&client), build_open_center(3, false));
}

#[test]
fn a_press_off_the_center_takes_its_keyboard_and_a_press_on_it_gives_the_keyboard_back() {
    let (mut client, mouse_frame) = build_center_client();
    client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );

    let off_center_answer = client
        .handle_notification_center_mouse(build_left_mouse_press(PANE_CONTENT_CELL), &mouse_frame);
    let after_the_off_center_press = get_center(&client);
    let mode_after_the_off_center_press = client.get_active_input_mode();
    let on_center_answer = client
        .handle_notification_center_mouse(build_left_mouse_press(CENTER_BODY_CELL), &mouse_frame);

    assert_eq!(off_center_answer, None);
    assert_eq!(after_the_off_center_press, build_open_center(0, false));
    assert_eq!(mode_after_the_off_center_press, LockMode::Normal);
    assert_eq!(on_center_answer, Some(Vec::new()));
    assert_eq!(get_center(&client), build_open_center(0, true));
    assert_eq!(client.get_active_input_mode(), LockMode::NotificationCenter);
}

#[test]
fn a_left_press_on_the_close_control_closes_the_center_and_a_right_press_focuses_it() {
    let (mut client, mouse_frame) = build_center_client();
    post_entries(&mut client, 1);
    client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );
    client.unfocus_notification_center();

    let right_press_answer = client.handle_notification_center_mouse(
        build_mouse_event(
            MouseKind::Press(MouseButton::Right),
            CLOSE_CONTROL_CELL,
            BindingModifierFlags::NONE,
        ),
        &mouse_frame,
    );
    let after_the_right_press = get_center(&client);
    let left_press_answer = client
        .handle_notification_center_mouse(build_left_mouse_press(CLOSE_CONTROL_CELL), &mouse_frame);

    assert_eq!(right_press_answer, Some(Vec::new()));
    assert_eq!(after_the_right_press, build_open_center(0, true));
    assert_eq!(left_press_answer, Some(Vec::new()));
    assert_eq!(get_center(&client), None);
    assert_eq!(list_entry_texts(&client), vec!["entry 0"]);
}

#[test]
fn a_left_press_on_the_clear_label_clears_and_closes_the_center() {
    let (mut client, mouse_frame) = build_center_client();
    post_entries(&mut client, 2);
    client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );

    let clear_answer = client
        .handle_notification_center_mouse(build_left_mouse_press(CLEAR_LABEL_CELL), &mouse_frame);

    assert_eq!(clear_answer, Some(Vec::new()));
    assert_eq!(get_center(&client), None);
    assert_eq!(list_entry_texts(&client), Vec::<String>::new());
}

#[test]
fn a_pointer_motion_over_the_center_clears_the_pane_hover_and_a_drag_takes_the_next_path() {
    let (mut client, mouse_frame) = build_center_client();
    client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );
    client.hovered_pane_id = Some(PaneId::new());

    let motion_answer = client.handle_notification_center_mouse(
        build_mouse_event(
            MouseKind::Motion,
            CENTER_BODY_CELL,
            BindingModifierFlags::NONE,
        ),
        &mouse_frame,
    );
    let drag_answer = client.handle_notification_center_mouse(
        build_mouse_event(
            MouseKind::Drag(MouseButton::Left),
            CENTER_BODY_CELL,
            BindingModifierFlags::NONE,
        ),
        &mouse_frame,
    );
    let pane_motion_answer = client.handle_notification_center_mouse(
        build_mouse_event(
            MouseKind::Motion,
            PANE_CONTENT_CELL,
            BindingModifierFlags::NONE,
        ),
        &mouse_frame,
    );

    assert_eq!(motion_answer, Some(Vec::new()));
    assert_eq!(client.hovered_pane_id, None);
    assert_eq!(drag_answer, None);
    assert_eq!(pane_motion_answer, None);
}

#[test]
fn a_closed_center_leaves_every_mouse_event_to_the_next_path() {
    let (mut client, mouse_frame) = build_center_client();

    for mouse_input in [
        build_left_mouse_press(CENTER_BODY_CELL),
        build_left_mouse_press(CLOSE_CONTROL_CELL),
        build_wheel_tick(ScrollDirection::Down, CENTER_BODY_CELL),
    ] {
        assert_eq!(
            client.handle_notification_center_mouse(mouse_input, &mouse_frame),
            None,
            "{mouse_input:?}"
        );
    }
}

#[test]
fn a_press_on_the_notification_button_opens_the_center_and_a_second_press_closes_it() {
    let (mut client, mouse_frame) = build_center_client();
    post_entries(&mut client, 1);
    let painted_mouse_frame = paint_mouse_frame(&client, &mouse_frame);
    let closed_center_button_cell = find_notification_button_cell(&painted_mouse_frame);

    let opening_actions = client.handle_mouse(
        build_left_mouse_press(closed_center_button_cell),
        &painted_mouse_frame,
        Instant::now(),
    );
    let after_the_first_press = get_center(&client);
    let repainted_mouse_frame = paint_mouse_frame(&client, &mouse_frame);
    let open_center_button_cell = find_notification_button_cell(&repainted_mouse_frame);
    let center_answer = client.handle_notification_center_mouse(
        build_left_mouse_press(open_center_button_cell),
        &repainted_mouse_frame,
    );
    let closing_actions = client.handle_mouse(
        build_left_mouse_press(open_center_button_cell),
        &repainted_mouse_frame,
        Instant::now(),
    );

    assert_eq!(opening_actions, Vec::new());
    assert_eq!(after_the_first_press, build_open_center(0, true));
    assert_eq!(center_answer, None);
    assert_eq!(closing_actions, Vec::new());
    assert_eq!(get_center(&client), None);
}

#[test]
fn the_notification_button_counts_unread_entries_and_shows_only_with_an_entry() {
    let (mut client, mouse_frame) = build_center_client();
    let with_no_entry = client.build_notification_button();
    post_entries(&mut client, 3);
    let with_three_unread = client.build_notification_button();

    client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );

    assert_eq!(with_no_entry, None);
    assert_eq!(
        with_three_unread,
        Some(NotificationButton {
            unread_entry_count: 3
        })
    );
    assert_eq!(
        client.build_notification_button(),
        Some(NotificationButton {
            unread_entry_count: 0
        })
    );
}

#[test]
fn opening_the_center_is_refused_while_another_local_mode_is_on() {
    let (mut client, mouse_frame) = build_center_client();
    client.pane_move_mode = Some(PaneMoveMode::Choice);

    client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );

    assert_eq!(get_center(&client), None);
    assert_eq!(client.pane_move_mode, Some(PaneMoveMode::Choice));
}

#[test]
fn the_center_opens_with_no_link_and_the_base_mode_then_holds_the_keyboard() {
    let (mut client, mouse_frame) = build_center_client();
    client.set_link_status(LinkStatus::Redialing(Reconnecting {
        attempt: 1,
        retry_in_seconds: Some(2),
    }));

    client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );

    assert_eq!(get_center(&client), build_open_center(0, true));
    assert_eq!(client.get_active_input_mode(), LockMode::Normal);
}

#[test]
fn an_action_of_another_component_takes_the_keyboard_from_the_center_and_draws_over_it() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true]);
    let mut client = build_client_viewing(&mouse_frame);
    client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );

    client.apply_client_action(ClientActionKind::ToggleFloatingPaneTray, Some(&mouse_frame));

    assert_eq!(get_center(&client), build_open_center(0, false));
    assert_eq!(client.get_active_input_mode(), LockMode::FloatingPaneTray);
}

#[test]
fn opening_the_center_closes_the_floating_pane_tray() {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true]);
    let mut client = build_client_viewing(&mouse_frame);
    client.apply_client_action(ClientActionKind::ToggleFloatingPaneTray, Some(&mouse_frame));
    assert_eq!(client.get_active_input_mode(), LockMode::FloatingPaneTray);

    client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );

    assert_eq!(client.floating_pane_tray, None);
    assert_eq!(get_center(&client), build_open_center(0, true));
    assert_eq!(client.get_active_input_mode(), LockMode::NotificationCenter);
}

#[test]
fn a_lock_mode_change_takes_the_keyboard_from_the_center_and_keeps_it_open() {
    let (mut client, mouse_frame) = build_center_client();
    client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );

    client.set_lock_mode(LockMode::Locked);

    assert_eq!(get_center(&client), build_open_center(0, false));
    assert_eq!(client.get_active_input_mode(), LockMode::Locked);
}

#[test]
fn the_toast_takes_the_text_rows_of_its_preview_at_the_panel_width() {
    let (mut client, _mouse_frame) = build_center_client();
    let with_no_entry = client.build_notification_toast();

    client.post_core_notification(
        NotificationSeverity::Error,
        "pane …a1b2 killed by signal 9 while it wrote to a full disk",
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(with_no_entry, None);
    assert_eq!(
        client.build_notification_toast(),
        Some(NotificationToast {
            toast_position: ToastPosition::TopRight,
            text_row_count: 2,
            hidden_per_mille: 1000,
        })
    );
}

#[test]
fn the_toast_clock_slides_the_toast_in_and_names_its_next_wakeup() {
    let (mut client, _mouse_frame) = build_center_client();
    post_entries(&mut client, 1);
    let start = Instant::now();
    let wakeup_before_the_clock = client.compute_next_notification_wakeup(start);

    client.advance_notification_toast(start, false);
    client.advance_notification_toast(after_milliseconds(start, 500), false);

    assert_eq!(wakeup_before_the_clock, Some(Duration::ZERO));
    assert_eq!(
        client.build_notification_toast(),
        Some(NotificationToast {
            toast_position: ToastPosition::TopRight,
            text_row_count: 1,
            hidden_per_mille: 500,
        })
    );
    assert_eq!(
        client.compute_next_notification_wakeup(after_milliseconds(start, 520)),
        Some(Duration::from_millis(30))
    );
}

#[test]
fn reduced_motion_and_a_longer_toast_duration_set_the_toast_clock() {
    let (mut client, _mouse_frame) = build_center_client();
    client.client_config.should_reduce_motion = true;
    client.client_config.notifications.toast_duration_ms = 5000;
    post_entries(&mut client, 1);
    let start = Instant::now();

    client.advance_notification_toast(start, false);
    let first_frame = client.build_notification_toast();
    client.advance_notification_toast(after_milliseconds(start, 4999), false);
    let last_stay_frame = client.build_notification_toast();
    client.advance_notification_toast(after_milliseconds(start, 5000), false);

    let shown_toast = Some(NotificationToast {
        toast_position: ToastPosition::TopRight,
        text_row_count: 1,
        hidden_per_mille: 0,
    });
    assert_eq!(first_frame, shown_toast);
    assert_eq!(last_stay_frame, shown_toast);
    assert_eq!(client.build_notification_toast(), None);
}

#[test]
fn a_left_press_on_the_toast_close_control_withdraws_the_toast_and_keeps_its_entry() {
    let (mut client, mouse_frame) = build_center_client();
    post_entries(&mut client, 1);
    let start = Instant::now();
    client.advance_notification_toast(start, false);
    client.advance_notification_toast(after_milliseconds(start, 1000), false);
    let painted_mouse_frame = paint_mouse_frame(&client, &mouse_frame);

    let press_answer = client.handle_notification_toast_mouse(
        build_left_mouse_press(CLOSE_CONTROL_CELL),
        &painted_mouse_frame,
    );
    client.advance_notification_toast(after_milliseconds(start, 1000), false);
    client.advance_notification_toast(after_milliseconds(start, 2000), false);

    assert_eq!(press_answer, Some(Vec::new()));
    assert_eq!(client.build_notification_toast(), None);
    assert_eq!(list_entry_texts(&client), vec!["entry 0"]);
}

#[test]
fn a_right_press_on_the_toast_close_control_is_taken_and_keeps_the_toast() {
    let (mut client, mouse_frame) = build_center_client();
    post_entries(&mut client, 1);
    let start = Instant::now();
    client.advance_notification_toast(start, false);
    client.advance_notification_toast(after_milliseconds(start, 1000), false);
    let painted_mouse_frame = paint_mouse_frame(&client, &mouse_frame);

    let press_answer = client.handle_notification_toast_mouse(
        build_mouse_event(
            MouseKind::Press(MouseButton::Right),
            CLOSE_CONTROL_CELL,
            BindingModifierFlags::NONE,
        ),
        &painted_mouse_frame,
    );
    client.advance_notification_toast(after_milliseconds(start, 2000), false);

    assert_eq!(press_answer, Some(Vec::new()));
    assert_eq!(
        client.build_notification_toast(),
        Some(NotificationToast {
            toast_position: ToastPosition::TopRight,
            text_row_count: 1,
            hidden_per_mille: 0,
        })
    );
}

#[test]
fn a_press_on_the_toast_body_and_a_motion_on_its_close_control_take_the_next_path() {
    let (mut client, mouse_frame) = build_center_client();
    post_entries(&mut client, 1);
    let start = Instant::now();
    client.advance_notification_toast(start, false);
    client.advance_notification_toast(after_milliseconds(start, 1000), false);
    let painted_mouse_frame = paint_mouse_frame(&client, &mouse_frame);

    let body_answer = client.handle_notification_toast_mouse(
        build_left_mouse_press(Point { column: 60, row: 2 }),
        &painted_mouse_frame,
    );
    let motion_answer = client.handle_notification_toast_mouse(
        build_mouse_event(
            MouseKind::Motion,
            CLOSE_CONTROL_CELL,
            BindingModifierFlags::NONE,
        ),
        &painted_mouse_frame,
    );

    assert_eq!(body_answer, None);
    assert_eq!(motion_answer, None);
}

#[test]
fn two_viewers_of_one_session_keep_their_own_toast_setting_and_clear_state() {
    let session_id = SessionId::new();
    let mut session_queue =
        NotificationQueue::from_owner(NotificationQueueOwner::Session(session_id));
    let mut quiet_client = build_test_client();
    quiet_client.client_config.notifications.should_show_toasts = false;
    let mut toasting_client = build_test_client();
    for client in [&mut quiet_client, &mut toasting_client] {
        client
            .take_session_notification_queue(session_id, &session_queue)
            .expect("take the attach queue");
    }
    let posted_entry = session_queue
        .post_core_entry(
            NotificationSeverity::Error,
            "pane …a1b2 killed by signal 9",
            SystemTime::UNIX_EPOCH,
        )
        .clone();
    for client in [&mut quiet_client, &mut toasting_client] {
        client
            .take_session_notification_post(session_queue.get_revision(), posted_entry.clone())
            .expect("take the post");
    }

    toasting_client.open_notification_center();
    toasting_client.clear_notifications();

    assert_eq!(quiet_client.build_notification_toast(), None);
    assert_eq!(
        list_entry_texts(&quiet_client),
        vec!["pane …a1b2 killed by signal 9"]
    );
    assert_eq!(list_entry_texts(&toasting_client), Vec::<String>::new());
}
