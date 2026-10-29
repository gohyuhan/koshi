//! Tests for the viewer's notification center and toast: the keys and the
//! mouse events that open, close, select, dismiss, scroll and clear the
//! center, the center's fit and its slide, the toast's close control, the
//! notification button, the toast's clock, and the state each viewer keeps
//! for itself.
//!
//! Every frame here is `80x24` with one tiled pane. The open center takes
//! `(56, 1)` `24x22`: its close button `✕` at `(76, 2)`, `Clear all` at
//! `(63, 21)` `9x1`, and its body at `(57, 4)` `22x17`. Each card of a
//! one-row text takes 4 body rows, its dismiss button `✕` on its top border
//! at column `75`. A toast in the top-right corner takes the same columns
//! from row 1, its close control ` ✕ ` at `(75, 1)` `3x1`.

use std::time::SystemTime;

use koshi_config::types::ToastPosition;
use koshi_core::action::ClientActionKind;
use koshi_core::geometry::{Point, Size};
use koshi_core::ids::PaneId;
use koshi_core::key::{
    BindingModifierFlags, Key, KeyChord, KeySequence, NamedKey, PendingKeySequence,
};
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
    build_render_snapshot_from_mouse_frame,
};
use crate::tests::{build_core_bound_action, TEST_VIEWPORT_SIZE};
use crate::{LinkStatus, PaneMoveMode, PlacementInputAction};

/// A cell of the open center's body below its cards.
const CENTER_BODY_CELL: Point = Point {
    column: 60,
    row: 18,
};

/// A cell of the open center's titlebar title.
const CENTER_TITLE_CELL: Point = Point { column: 60, row: 2 };

/// A cell of the pane's content, left of the open center.
const PANE_CONTENT_CELL: Point = Point {
    column: 10,
    row: 10,
};

/// The center's close button `✕`.
const CENTER_CLOSE_BUTTON_CELL: Point = Point { column: 76, row: 2 };

/// A cell of the center's `Clear all`.
const CLEAR_ALL_CELL: Point = Point {
    column: 65,
    row: 21,
};

/// The dismiss button `✕` of the newest card, on body row `0`.
const NEWEST_CARD_DISMISS_BUTTON_CELL: Point = Point { column: 75, row: 4 };

/// A header cell of the second newest card, on body row `5`.
const SECOND_CARD_HEADER_CELL: Point = Point { column: 60, row: 9 };

/// The dismiss button `✕` of the second newest card, on body row `4`.
const SECOND_CARD_DISMISS_BUTTON_CELL: Point = Point { column: 75, row: 8 };

/// A cell of a top-right toast's close control ` ✕ `.
const TOAST_CLOSE_CONTROL_CELL: Point = Point { column: 76, row: 1 };

/// A client that applied an `80x24` frame of one focused tiled pane, with
/// `reduced-motion` on, and that frame with no viewer chrome painted. The
/// center opens and closes with no slide.
fn build_center_client() -> (Client, MouseFrame) {
    let (mut client, mouse_frame) = build_sliding_center_client();
    client.client_config.should_reduce_motion = true;
    (client, mouse_frame)
}

/// A client that applied an `80x24` frame of one focused tiled pane, with
/// `reduced-motion` off, and that frame with no viewer chrome painted.
fn build_sliding_center_client() -> (Client, MouseFrame) {
    let pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(&[build_plain_mouse_pane(pane_id)], Some(pane_id));
    (build_client_viewing(&mouse_frame), mouse_frame)
}

/// `mouse_frame` as `client` paints it now: with the viewer chrome `client`
/// holds, and the notification cards and the toast that chrome draws.
fn paint_mouse_frame(client: &Client, mouse_frame: &MouseFrame) -> MouseFrame {
    let mut painted_mouse_frame = mouse_frame.clone();
    painted_mouse_frame.viewer_chrome = client.build_viewer_chrome(
        mouse_frame.client_snapshot.active_tab_id,
        &mouse_frame.committed_regions,
    );
    painted_mouse_frame.drawn_notifications =
        client.build_drawn_notifications(painted_mouse_frame.build_frame_layout());
    painted_mouse_frame
}

/// `mouse_frame` with a `viewport_size` viewport and the compiled-in region
/// solve of that viewport.
fn resize_mouse_frame(mouse_frame: &MouseFrame, viewport_size: Size) -> MouseFrame {
    let mut resized_mouse_frame = mouse_frame.clone();
    resized_mouse_frame.client_snapshot.viewport_size = viewport_size;
    resized_mouse_frame.committed_regions = CommittedRegions::build_core(viewport_size, 0);
    resized_mouse_frame
}

/// The first tabline cell of the notification button `mouse_frame` paints.
fn get_notification_button_cell(mouse_frame: &MouseFrame) -> Point {
    (0..TEST_VIEWPORT_SIZE.column_count)
        .map(|column| Point { column, row: 0 })
        .find(|cell| {
            resolve_hit_region(mouse_frame.build_frame_layout(), *cell)
                == HitRegion::NotificationButton
        })
        .expect("the tabline draws the notification button")
}

/// Post `own_entry_count` `Info` entries to `client`'s own queue, with the texts
/// `entry 0`, `entry 1`, ….
fn post_own_entries(client: &mut Client, own_entry_count: usize) {
    for own_entry_index in 0..own_entry_count {
        client.post_core_notification(
            NotificationSeverity::Info,
            &format!("entry {own_entry_index}"),
            NonZeroU32::MIN,
            SystemTime::UNIX_EPOCH,
        );
    }
}

/// The texts of `client`'s entries, oldest arrival first.
fn list_merged_entry_texts(client: &Client) -> Vec<String> {
    client
        .get_notification_history()
        .list_merged_entries()
        .map(|merged_entry| merged_entry.text.clone())
        .collect()
}

/// The center of `client` while it is open or slides out, or `None` while it
/// is closed.
fn get_center(client: &Client) -> Option<NotificationCenter> {
    client.get_notification_history().get_notification_center()
}

/// The text of the entry the center of `client` selects, or `None` while it
/// selects none or is closed.
fn find_selected_text(client: &Client) -> Option<String> {
    let selected_notification_id = get_center(client)?.selected_notification_id?;
    client
        .get_notification_history()
        .list_merged_entries()
        .find(|merged_entry| merged_entry.notification_id == selected_notification_id)
        .map(|merged_entry| merged_entry.text.clone())
}

/// The body row the center of `client` is scrolled to, or `None` while it is
/// closed.
fn find_scroll_row_offset(client: &Client) -> Option<usize> {
    get_center(client).map(|notification_center| notification_center.scroll_row_offset)
}

/// Open the center of `client` over `mouse_frame` with the toggle action.
fn open_center(client: &mut Client, mouse_frame: &MouseFrame) {
    client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(mouse_frame),
    );
}

/// A wheel tick `scroll_direction` at `position`.
fn build_wheel_tick(scroll_direction: ScrollDirection, position: Point) -> MouseInput {
    build_mouse_event(
        MouseKind::Scroll(scroll_direction),
        position,
        BindingModifierFlags::NONE,
    )
}

/// A right press at `position`.
fn build_right_mouse_press(position: Point) -> MouseInput {
    build_mouse_event(
        MouseKind::Press(MouseButton::Right),
        position,
        BindingModifierFlags::NONE,
    )
}

/// `start` moved `millisecond_count` milliseconds on.
fn compute_instant_after_milliseconds(start: Instant, millisecond_count: u64) -> Instant {
    start + Duration::from_millis(millisecond_count)
}

#[test]
fn the_toggle_action_opens_the_center_on_its_newest_card_and_closes_it_with_no_command() {
    let (mut client, mouse_frame) = build_center_client();
    post_own_entries(&mut client, 2);

    let open_action = client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );
    let opened_center = get_center(&client);
    let selected_text = find_selected_text(&client);
    let mode_while_open = client.get_active_input_mode();
    let close_action = client.apply_client_action(
        ClientActionKind::ToggleNotificationCenter,
        Some(&mouse_frame),
    );

    assert_eq!(open_action, PlacementInputAction::Consumed);
    assert_eq!(
        opened_center.map(|notification_center| (
            notification_center.scroll_row_offset,
            notification_center.hidden_per_mille
        )),
        Some((0, 0))
    );
    assert_eq!(selected_text, Some("entry 1".to_string()));
    assert_eq!(mode_while_open, LockMode::NotificationCenter);
    assert_eq!(close_action, PlacementInputAction::Consumed);
    assert_eq!(get_center(&client), None);
    assert_eq!(client.get_active_input_mode(), LockMode::Normal);
}

#[test]
fn the_keys_of_the_open_center_fire_its_actions() {
    let (mut client, mouse_frame) = build_center_client();
    open_center(&mut client, &mouse_frame);
    let current_time = Instant::now();

    let key_outcomes: Vec<KeyOutcome> = [
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Esc)),
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char('c')),
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char('x')),
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
            KeyOutcome::Fire(build_core_bound_action("dismiss-selected-notification")),
            KeyOutcome::Fire(build_core_bound_action("select-newer-notification")),
            KeyOutcome::Fire(build_core_bound_action("select-older-notification")),
            KeyOutcome::Fire(build_core_bound_action("toggle-notification-center")),
        ]
    );
}

#[test]
fn the_close_action_closes_the_center_and_keeps_every_entry() {
    let (mut client, mouse_frame) = build_center_client();
    post_own_entries(&mut client, 2);
    open_center(&mut client, &mouse_frame);

    client.apply_client_action(
        ClientActionKind::CloseNotificationCenter,
        Some(&mouse_frame),
    );

    assert_eq!(get_center(&client), None);
    assert_eq!(list_merged_entry_texts(&client), vec!["entry 0", "entry 1"]);
    assert_eq!(
        client.build_notification_button(),
        NotificationButton {
            unread_entry_count: 0
        }
    );
}

#[test]
fn the_clear_action_removes_every_entry_and_closes_the_center() {
    let (mut client, mouse_frame) = build_center_client();
    post_own_entries(&mut client, 2);
    open_center(&mut client, &mouse_frame);

    client.apply_client_action(ClientActionKind::ClearNotifications, Some(&mouse_frame));

    assert_eq!(get_center(&client), None);
    assert_eq!(list_merged_entry_texts(&client), Vec::<String>::new());
    assert_eq!(
        client.build_notification_button(),
        NotificationButton {
            unread_entry_count: 0
        }
    );
}

#[test]
fn the_clear_and_dismiss_actions_on_a_closed_center_keep_every_entry() {
    let (mut client, mouse_frame) = build_center_client();
    post_own_entries(&mut client, 2);

    client.apply_client_action(ClientActionKind::ClearNotifications, Some(&mouse_frame));
    client.apply_client_action(
        ClientActionKind::DismissSelectedNotification,
        Some(&mouse_frame),
    );

    assert_eq!(list_merged_entry_texts(&client), vec!["entry 0", "entry 1"]);
}

#[test]
fn the_select_actions_step_one_card_and_scroll_the_fewest_rows_that_show_it() {
    let (mut client, mouse_frame) = build_center_client();
    post_own_entries(&mut client, 30);
    open_center(&mut client, &mouse_frame);

    client.apply_client_action(
        ClientActionKind::SelectNewerNotification,
        Some(&mouse_frame),
    );
    let past_the_newest = (find_selected_text(&client), find_scroll_row_offset(&client));
    let mut older_steps = Vec::new();
    for _ in 0..5 {
        client.apply_client_action(
            ClientActionKind::SelectOlderNotification,
            Some(&mouse_frame),
        );
        older_steps.push((find_selected_text(&client), find_scroll_row_offset(&client)));
    }
    let mut newer_steps = Vec::new();
    for _ in 0..5 {
        client.apply_client_action(
            ClientActionKind::SelectNewerNotification,
            Some(&mouse_frame),
        );
        newer_steps.push((find_selected_text(&client), find_scroll_row_offset(&client)));
    }

    // Card `entry 29 - n` takes body rows `4n..4n + 4`, and the body shows
    // 17 rows.
    let build_step =
        |text: &str, scroll_row_offset| (Some(text.to_string()), Some(scroll_row_offset));
    assert_eq!(past_the_newest, build_step("entry 29", 0));
    assert_eq!(
        older_steps,
        [
            build_step("entry 28", 0),
            build_step("entry 27", 0),
            build_step("entry 26", 0),
            build_step("entry 25", 3),
            build_step("entry 24", 7),
        ]
    );
    assert_eq!(
        newer_steps,
        [
            build_step("entry 25", 7),
            build_step("entry 26", 7),
            build_step("entry 27", 7),
            build_step("entry 28", 4),
            build_step("entry 29", 0),
        ]
    );
}

#[test]
fn the_select_action_past_the_oldest_card_keeps_the_selection() {
    let (mut client, mouse_frame) = build_center_client();
    post_own_entries(&mut client, 2);
    open_center(&mut client, &mouse_frame);

    for _ in 0..3 {
        client.apply_client_action(
            ClientActionKind::SelectOlderNotification,
            Some(&mouse_frame),
        );
    }

    assert_eq!(find_selected_text(&client), Some("entry 0".to_string()));
    assert_eq!(find_scroll_row_offset(&client), Some(0));
}

#[test]
fn the_select_action_with_no_painted_frame_moves_the_selection_and_keeps_the_scroll() {
    let (mut client, mouse_frame) = build_center_client();
    post_own_entries(&mut client, 30);
    open_center(&mut client, &mouse_frame);

    for _ in 0..5 {
        client.apply_client_action(ClientActionKind::SelectOlderNotification, None);
    }

    assert_eq!(find_selected_text(&client), Some("entry 24".to_string()));
    assert_eq!(find_scroll_row_offset(&client), Some(0));
}

#[test]
fn the_dismiss_action_removes_the_selected_card_and_selects_the_next_older_one() {
    let (mut client, mouse_frame) = build_center_client();
    post_own_entries(&mut client, 3);
    open_center(&mut client, &mouse_frame);

    client.apply_client_action(
        ClientActionKind::DismissSelectedNotification,
        Some(&mouse_frame),
    );
    let after_the_first_dismiss = (
        list_merged_entry_texts(&client),
        find_selected_text(&client),
    );
    client.apply_client_action(
        ClientActionKind::SelectOlderNotification,
        Some(&mouse_frame),
    );
    client.apply_client_action(
        ClientActionKind::DismissSelectedNotification,
        Some(&mouse_frame),
    );

    assert_eq!(
        after_the_first_dismiss,
        (
            vec!["entry 0".to_string(), "entry 1".to_string()],
            Some("entry 1".to_string())
        )
    );
    assert_eq!(list_merged_entry_texts(&client), vec!["entry 1"]);
    assert_eq!(find_selected_text(&client), Some("entry 1".to_string()));
    assert!(client
        .get_notification_history()
        .is_notification_center_open());
}

#[test]
fn the_dismiss_action_brings_the_card_selected_next_into_view() {
    let (mut client, mouse_frame) = build_center_client();
    post_own_entries(&mut client, 30);
    open_center(&mut client, &mouse_frame);
    for _ in 0..29 {
        client.apply_client_action(
            ClientActionKind::SelectOlderNotification,
            Some(&mouse_frame),
        );
    }
    let at_the_oldest = (find_selected_text(&client), find_scroll_row_offset(&client));

    client.apply_client_action(
        ClientActionKind::DismissSelectedNotification,
        Some(&mouse_frame),
    );

    // 29 cards take 116 rows: the oldest card, `entry 1`, takes rows 112 to
    // 115, and the body draws from row 99 at most.
    assert_eq!(at_the_oldest, (Some("entry 0".to_string()), Some(103)));
    assert_eq!(find_selected_text(&client), Some("entry 1".to_string()));
    assert_eq!(find_scroll_row_offset(&client), Some(99));
}

#[test]
fn a_wheel_tick_over_the_center_scrolls_it_by_the_scroll_line_count_and_stops_at_its_ends() {
    let (mut client, mouse_frame) = build_center_client();
    post_own_entries(&mut client, 30);
    open_center(&mut client, &mouse_frame);

    let up_answer = client.handle_notification_center_mouse(
        build_wheel_tick(ScrollDirection::Up, CENTER_BODY_CELL),
        &mouse_frame,
    );
    let above_the_top = find_scroll_row_offset(&client);
    let down_answer = client.handle_notification_center_mouse(
        build_wheel_tick(ScrollDirection::Down, CENTER_TITLE_CELL),
        &mouse_frame,
    );
    let after_the_down_tick = find_scroll_row_offset(&client);
    client.scroll_notification_center(false, 200, Some(&mouse_frame));
    let at_the_bottom = find_scroll_row_offset(&client);
    client.handle_notification_center_mouse(
        build_wheel_tick(ScrollDirection::Down, CENTER_BODY_CELL),
        &mouse_frame,
    );
    let below_the_bottom = find_scroll_row_offset(&client);
    let sideways_answer = client.handle_notification_center_mouse(
        build_wheel_tick(ScrollDirection::Right, CENTER_BODY_CELL),
        &mouse_frame,
    );

    assert_eq!(up_answer, Some(Vec::new()));
    assert_eq!(above_the_top, Some(0));
    assert_eq!(down_answer, Some(Vec::new()));
    assert_eq!(after_the_down_tick, Some(3));
    assert_eq!(at_the_bottom, Some(103));
    assert_eq!(below_the_bottom, Some(103));
    assert_eq!(sideways_answer, Some(Vec::new()));
    assert_eq!(find_scroll_row_offset(&client), Some(103));
}

#[test]
fn a_scroll_from_past_the_last_row_starts_from_the_row_the_center_draws() {
    let (mut client, mouse_frame) = build_center_client();
    post_own_entries(&mut client, 30);
    open_center(&mut client, &mouse_frame);

    client
        .notification_history
        .set_notification_center_scroll_row_offset(150);
    client.scroll_notification_center(true, 1, Some(&mouse_frame));
    let after_the_up_scroll = find_scroll_row_offset(&client);
    client
        .notification_history
        .set_notification_center_scroll_row_offset(150);
    client.scroll_notification_center(false, 1, Some(&mouse_frame));

    assert_eq!(after_the_up_scroll, Some(102));
    assert_eq!(find_scroll_row_offset(&client), Some(103));
}

#[test]
fn a_wheel_tick_off_the_open_center_takes_the_next_path_and_keeps_the_center_open() {
    let (mut client, mouse_frame) = build_center_client();
    post_own_entries(&mut client, 30);
    open_center(&mut client, &mouse_frame);

    let off_center_answer = client.handle_notification_center_mouse(
        build_wheel_tick(ScrollDirection::Down, PANE_CONTENT_CELL),
        &mouse_frame,
    );

    assert_eq!(off_center_answer, None);
    assert_eq!(find_scroll_row_offset(&client), Some(0));
    assert!(client
        .get_notification_history()
        .is_notification_center_open());
}

#[test]
fn a_press_off_the_open_center_closes_it_and_takes_the_next_path() {
    let (mut client, mouse_frame) = build_center_client();
    open_center(&mut client, &mouse_frame);

    let left_press_answer = client
        .handle_notification_center_mouse(build_left_mouse_press(PANE_CONTENT_CELL), &mouse_frame);
    let after_the_left_press = get_center(&client);
    open_center(&mut client, &mouse_frame);
    let right_press_answer = client
        .handle_notification_center_mouse(build_right_mouse_press(PANE_CONTENT_CELL), &mouse_frame);

    assert_eq!(left_press_answer, None);
    assert_eq!(after_the_left_press, None);
    assert_eq!(right_press_answer, None);
    assert_eq!(get_center(&client), None);
    assert_eq!(client.get_active_input_mode(), LockMode::Normal);
}

#[test]
fn a_press_off_the_open_center_drops_the_sequence_the_center_opened() {
    let (mut client, mouse_frame) = build_center_client();
    open_center(&mut client, &mouse_frame);
    let open_key_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::NONE,
        Key::Char('g'),
    ));
    client.pending_key_sequence = Some(PendingKeySequence {
        sequence: open_key_sequence,
        ambiguity_deadline: None,
        continuation_hint_hidden_until: None,
    });

    client
        .handle_notification_center_mouse(build_left_mouse_press(PANE_CONTENT_CELL), &mouse_frame);

    assert_eq!(client.get_pending_key_sequence(), None);
}

#[test]
fn a_press_on_the_center_keeps_an_open_sequence_of_its_mode() {
    let (mut client, mouse_frame) = build_center_client();
    open_center(&mut client, &mouse_frame);
    let open_key_sequence = KeySequence::from(KeyChord::from_parts(
        BindingModifierFlags::NONE,
        Key::Char('g'),
    ));
    client.pending_key_sequence = Some(PendingKeySequence {
        sequence: open_key_sequence.clone(),
        ambiguity_deadline: None,
        continuation_hint_hidden_until: None,
    });

    client.handle_notification_center_mouse(build_left_mouse_press(CENTER_BODY_CELL), &mouse_frame);

    assert_eq!(client.get_pending_key_sequence(), Some(&open_key_sequence));
}

#[test]
fn a_left_press_on_the_close_button_closes_the_center_and_a_right_press_does_nothing() {
    let (mut client, mouse_frame) = build_center_client();
    post_own_entries(&mut client, 1);
    open_center(&mut client, &mouse_frame);

    let right_press_answer = client.handle_notification_center_mouse(
        build_right_mouse_press(CENTER_CLOSE_BUTTON_CELL),
        &mouse_frame,
    );
    let is_open_after_the_right_press = client
        .get_notification_history()
        .is_notification_center_open();
    let left_press_answer = client.handle_notification_center_mouse(
        build_left_mouse_press(CENTER_CLOSE_BUTTON_CELL),
        &mouse_frame,
    );

    assert_eq!(right_press_answer, Some(Vec::new()));
    assert!(is_open_after_the_right_press);
    assert_eq!(left_press_answer, Some(Vec::new()));
    assert_eq!(get_center(&client), None);
    assert_eq!(list_merged_entry_texts(&client), vec!["entry 0"]);
}

#[test]
fn a_left_press_on_clear_all_clears_and_closes_the_center() {
    let (mut client, mouse_frame) = build_center_client();
    post_own_entries(&mut client, 2);
    open_center(&mut client, &mouse_frame);

    let clear_answer = client
        .handle_notification_center_mouse(build_left_mouse_press(CLEAR_ALL_CELL), &mouse_frame);

    assert_eq!(clear_answer, Some(Vec::new()));
    assert_eq!(get_center(&client), None);
    assert_eq!(list_merged_entry_texts(&client), Vec::<String>::new());
}

#[test]
fn a_left_press_on_a_card_selects_it_and_on_its_dismiss_button_removes_it() {
    let (mut client, mouse_frame) = build_center_client();
    post_own_entries(&mut client, 3);
    open_center(&mut client, &mouse_frame);
    let painted_mouse_frame = paint_mouse_frame(&client, &mouse_frame);

    let card_answer = client.handle_notification_center_mouse(
        build_left_mouse_press(SECOND_CARD_HEADER_CELL),
        &painted_mouse_frame,
    );
    let selected_after_the_card_press = find_selected_text(&client);
    let newest_dismiss_answer = client.handle_notification_center_mouse(
        build_left_mouse_press(NEWEST_CARD_DISMISS_BUTTON_CELL),
        &painted_mouse_frame,
    );
    let texts_after_the_newest_dismiss = list_merged_entry_texts(&client);
    let selected_after_the_newest_dismiss = find_selected_text(&client);
    let repainted_mouse_frame = paint_mouse_frame(&client, &mouse_frame);
    client.handle_notification_center_mouse(
        build_left_mouse_press(SECOND_CARD_DISMISS_BUTTON_CELL),
        &repainted_mouse_frame,
    );

    assert_eq!(card_answer, Some(Vec::new()));
    assert_eq!(selected_after_the_card_press, Some("entry 1".to_string()));
    assert_eq!(newest_dismiss_answer, Some(Vec::new()));
    assert_eq!(texts_after_the_newest_dismiss, vec!["entry 0", "entry 1"]);
    assert_eq!(
        selected_after_the_newest_dismiss,
        Some("entry 1".to_string())
    );
    assert_eq!(list_merged_entry_texts(&client), vec!["entry 1"]);
    assert_eq!(find_selected_text(&client), Some("entry 1".to_string()));
}

#[test]
fn a_press_on_a_drawn_card_acts_on_that_card_after_a_newer_entry_arrives() {
    let (mut client, mouse_frame) = build_center_client();
    post_own_entries(&mut client, 2);
    open_center(&mut client, &mouse_frame);
    let painted_mouse_frame = paint_mouse_frame(&client, &mouse_frame);
    client.post_core_notification(
        NotificationSeverity::Info,
        "entry 2",
        NonZeroU32::MIN,
        SystemTime::UNIX_EPOCH,
    );

    client.handle_notification_center_mouse(
        build_left_mouse_press(SECOND_CARD_HEADER_CELL),
        &painted_mouse_frame,
    );
    let selected_after_the_card_press = find_selected_text(&client);
    client.handle_notification_center_mouse(
        build_left_mouse_press(NEWEST_CARD_DISMISS_BUTTON_CELL),
        &painted_mouse_frame,
    );

    assert_eq!(selected_after_the_card_press, Some("entry 0".to_string()));
    assert_eq!(list_merged_entry_texts(&client), vec!["entry 0", "entry 2"]);
}

#[test]
fn a_press_on_the_body_of_a_frame_that_drew_no_center_does_nothing() {
    let (mut client, mouse_frame) = build_center_client();
    post_own_entries(&mut client, 2);
    open_center(&mut client, &mouse_frame);

    let dismiss_answer = client.handle_notification_center_mouse(
        build_left_mouse_press(NEWEST_CARD_DISMISS_BUTTON_CELL),
        &mouse_frame,
    );

    assert_eq!(dismiss_answer, Some(Vec::new()));
    assert_eq!(list_merged_entry_texts(&client), vec!["entry 0", "entry 1"]);
}

#[test]
fn a_right_press_on_a_dismiss_button_and_a_left_press_under_the_cards_do_nothing() {
    let (mut client, mouse_frame) = build_center_client();
    post_own_entries(&mut client, 2);
    open_center(&mut client, &mouse_frame);

    let right_press_answer = client.handle_notification_center_mouse(
        build_right_mouse_press(NEWEST_CARD_DISMISS_BUTTON_CELL),
        &mouse_frame,
    );
    let under_the_cards_answer = client
        .handle_notification_center_mouse(build_left_mouse_press(CENTER_BODY_CELL), &mouse_frame);

    assert_eq!(right_press_answer, Some(Vec::new()));
    assert_eq!(under_the_cards_answer, Some(Vec::new()));
    assert_eq!(list_merged_entry_texts(&client), vec!["entry 0", "entry 1"]);
    assert_eq!(find_selected_text(&client), Some("entry 1".to_string()));
}

#[test]
fn a_pointer_motion_over_the_center_clears_the_pane_hover_and_a_drag_takes_the_next_path() {
    let (mut client, mouse_frame) = build_center_client();
    open_center(&mut client, &mouse_frame);
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
        build_left_mouse_press(CENTER_CLOSE_BUTTON_CELL),
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
fn a_center_that_slides_out_takes_the_events_on_its_cells_and_acts_on_none() {
    let (mut client, mouse_frame) = build_sliding_center_client();
    post_own_entries(&mut client, 30);
    let start = Instant::now();
    open_center(&mut client, &mouse_frame);
    client.advance_notification_clocks(start, false);
    client.advance_notification_clocks(compute_instant_after_milliseconds(start, 160), false);
    client.apply_client_action(
        ClientActionKind::CloseNotificationCenter,
        Some(&mouse_frame),
    );
    client.advance_notification_clocks(compute_instant_after_milliseconds(start, 200), false);
    let sliding_out_center = get_center(&client);

    let answers: Vec<Option<Vec<MouseAction>>> = [
        build_left_mouse_press(CLEAR_ALL_CELL),
        build_left_mouse_press(NEWEST_CARD_DISMISS_BUTTON_CELL),
        build_left_mouse_press(CENTER_CLOSE_BUTTON_CELL),
        build_wheel_tick(ScrollDirection::Down, CENTER_BODY_CELL),
        build_left_mouse_press(PANE_CONTENT_CELL),
    ]
    .into_iter()
    .map(|mouse_input| client.handle_notification_center_mouse(mouse_input, &mouse_frame))
    .collect();

    assert_eq!(
        sliding_out_center.map(|notification_center| notification_center.hidden_per_mille),
        Some(0)
    );
    assert_eq!(
        answers,
        [
            Some(Vec::new()),
            Some(Vec::new()),
            Some(Vec::new()),
            Some(Vec::new()),
            None
        ]
    );
    assert_eq!(get_center(&client), sliding_out_center);
    assert_eq!(list_merged_entry_texts(&client).len(), 30);
}

#[test]
fn a_press_on_the_notification_button_opens_the_center_and_a_second_press_closes_it() {
    let (mut client, mouse_frame) = build_center_client();
    post_own_entries(&mut client, 1);
    let painted_mouse_frame = paint_mouse_frame(&client, &mouse_frame);
    let closed_center_button_cell = get_notification_button_cell(&painted_mouse_frame);

    let opening_actions = client.handle_mouse(
        build_left_mouse_press(closed_center_button_cell),
        &painted_mouse_frame,
        Instant::now(),
    );
    let is_open_after_the_first_press = client
        .get_notification_history()
        .is_notification_center_open();
    let repainted_mouse_frame = paint_mouse_frame(&client, &mouse_frame);
    let open_center_button_cell = get_notification_button_cell(&repainted_mouse_frame);
    let center_answer = client.handle_notification_center_mouse(
        build_left_mouse_press(open_center_button_cell),
        &repainted_mouse_frame,
    );

    assert_eq!(opening_actions, Vec::new());
    assert!(is_open_after_the_first_press);
    assert_eq!(center_answer, Some(Vec::new()));
    assert_eq!(get_center(&client), None);
}

#[test]
fn the_notification_button_counts_unread_entries_and_shows_with_no_entry() {
    let (mut client, mouse_frame) = build_center_client();
    let button_before_any_post = client.build_notification_button();
    post_own_entries(&mut client, 3);
    let with_three_unread = client.build_notification_button();

    open_center(&mut client, &mouse_frame);

    assert_eq!(
        button_before_any_post,
        NotificationButton {
            unread_entry_count: 0
        }
    );
    assert_eq!(
        with_three_unread,
        NotificationButton {
            unread_entry_count: 3
        }
    );
    assert_eq!(
        client.build_notification_button(),
        NotificationButton {
            unread_entry_count: 0
        }
    );
}

#[test]
fn opening_the_center_is_refused_while_another_local_mode_is_on() {
    let (mut client, mouse_frame) = build_center_client();
    client.pane_move_mode = Some(PaneMoveMode::Choice);

    client.open_notification_center(Some(&mouse_frame));

    assert_eq!(get_center(&client), None);
    assert_eq!(client.pane_move_mode, Some(PaneMoveMode::Choice));
}

#[test]
fn the_center_actions_do_nothing_while_the_tray_is_open_and_the_tray_actions_while_the_center_is_open(
) {
    let (mouse_frame, _floating_pane_ids) = build_tray_mouse_frame(&[true]);
    let mut tray_client = build_client_viewing(&mouse_frame);
    post_own_entries(&mut tray_client, 1);
    tray_client.apply_client_action(ClientActionKind::ToggleFloatingPaneTray, Some(&mouse_frame));
    let open_floating_pane_tray = tray_client.floating_pane_tray;
    let mut center_client = build_client_viewing(&mouse_frame);
    center_client.client_config.should_reduce_motion = true;
    open_center(&mut center_client, &mouse_frame);

    for client_action_kind in [
        ClientActionKind::ToggleNotificationCenter,
        ClientActionKind::ClearNotifications,
        ClientActionKind::DismissSelectedNotification,
    ] {
        assert_eq!(
            tray_client.apply_client_action(client_action_kind, Some(&mouse_frame)),
            PlacementInputAction::Consumed
        );
    }
    for client_action_kind in [
        ClientActionKind::ToggleFloatingPaneTray,
        ClientActionKind::CloseFloatingPaneTray,
        ClientActionKind::ActivateSelectedTrayTile,
    ] {
        assert_eq!(
            center_client.apply_client_action(client_action_kind, Some(&mouse_frame)),
            PlacementInputAction::Consumed
        );
    }

    assert_eq!(tray_client.floating_pane_tray, open_floating_pane_tray);
    assert_eq!(get_center(&tray_client), None);
    assert_eq!(list_merged_entry_texts(&tray_client), vec!["entry 0"]);
    assert_eq!(
        tray_client.get_active_input_mode(),
        LockMode::FloatingPaneTray
    );
    assert_eq!(center_client.floating_pane_tray, None);
    assert_eq!(
        center_client.get_active_input_mode(),
        LockMode::NotificationCenter
    );
}

#[test]
fn an_action_of_another_kind_closes_the_open_center() {
    let (mut client, mouse_frame) = build_center_client();
    open_center(&mut client, &mouse_frame);

    let cancel_action =
        client.apply_client_action(ClientActionKind::CancelPaneMoveChoice, Some(&mouse_frame));

    assert_eq!(cancel_action, PlacementInputAction::Consumed);
    assert_eq!(get_center(&client), None);
    assert_eq!(client.get_active_input_mode(), LockMode::Normal);
}

#[test]
fn opening_the_center_is_refused_with_no_painted_frame_and_on_a_frame_with_no_room() {
    let (mut client, mouse_frame) = build_center_client();
    let narrow_mouse_frame = resize_mouse_frame(
        &mouse_frame,
        Size {
            column_count: 9,
            row_count: 24,
        },
    );
    let short_mouse_frame = resize_mouse_frame(
        &mouse_frame,
        Size {
            column_count: 80,
            row_count: 7,
        },
    );
    let smallest_mouse_frame = resize_mouse_frame(
        &mouse_frame,
        Size {
            column_count: 10,
            row_count: 8,
        },
    );

    client.open_notification_center(None);
    let with_no_frame = get_center(&client);
    client.open_notification_center(Some(&narrow_mouse_frame));
    let on_a_narrow_frame = get_center(&client);
    client.open_notification_center(Some(&short_mouse_frame));
    let on_a_short_frame = get_center(&client);
    client.open_notification_center(Some(&smallest_mouse_frame));

    assert_eq!(with_no_frame, None);
    assert_eq!(on_a_narrow_frame, None);
    assert_eq!(on_a_short_frame, None);
    assert!(client
        .get_notification_history()
        .is_notification_center_open());
}

#[test]
fn a_frame_with_no_room_removes_the_center_at_once_and_a_larger_frame_leaves_it_closed() {
    let (mut client, mouse_frame) = build_sliding_center_client();
    open_center(&mut client, &mouse_frame);
    let narrow_viewport_size = Size {
        column_count: 9,
        row_count: 24,
    };
    let narrow_render_snapshot = build_render_snapshot_from_mouse_frame(&resize_mouse_frame(
        &mouse_frame,
        narrow_viewport_size,
    ));
    let render_snapshot = build_render_snapshot_from_mouse_frame(&mouse_frame);

    client.update_notification_center(&render_snapshot, &mouse_frame.committed_regions);
    let on_the_same_frame = client
        .get_notification_history()
        .is_notification_center_open();
    client.update_notification_center(
        &narrow_render_snapshot,
        &CommittedRegions::build_core(narrow_viewport_size, 0),
    );
    let on_the_narrow_frame = get_center(&client);
    client.update_notification_center(&render_snapshot, &mouse_frame.committed_regions);

    assert!(on_the_same_frame);
    assert_eq!(on_the_narrow_frame, None);
    assert_eq!(get_center(&client), None);
    assert_eq!(client.get_active_input_mode(), LockMode::Normal);
}

#[test]
fn the_center_does_not_open_while_the_viewer_has_no_link() {
    let (mut client, mouse_frame) = build_center_client();
    client.set_link_status(LinkStatus::Redialing(Reconnecting {
        attempt: 1,
        retry_in_seconds: Some(2),
    }));

    open_center(&mut client, &mouse_frame);

    assert_eq!(get_center(&client), None);
    assert_eq!(client.get_active_input_mode(), LockMode::Normal);
}

#[test]
fn a_lost_link_closes_the_open_center_and_the_base_mode_takes_the_keyboard() {
    for lost_link_status in [
        LinkStatus::Redialing(Reconnecting {
            attempt: 1,
            retry_in_seconds: Some(2),
        }),
        LinkStatus::AwaitingRestartedSession,
    ] {
        let (mut client, mouse_frame) = build_center_client();
        open_center(&mut client, &mouse_frame);
        assert_eq!(
            client.get_active_input_mode(),
            LockMode::NotificationCenter,
            "{lost_link_status:?}"
        );

        client.set_link_status(lost_link_status);

        assert_eq!(get_center(&client), None, "{lost_link_status:?}");
        assert_eq!(
            client.get_active_input_mode(),
            LockMode::Normal,
            "{lost_link_status:?}"
        );
    }
}

#[test]
fn a_lock_mode_change_closes_the_center() {
    let (mut client, mouse_frame) = build_center_client();
    open_center(&mut client, &mouse_frame);

    client.set_lock_mode(LockMode::Locked);

    assert_eq!(get_center(&client), None);
    assert_eq!(client.get_active_input_mode(), LockMode::Locked);
}

#[test]
fn an_opened_center_slides_in_and_a_closed_center_slides_out_with_the_keyboard_back_at_once() {
    let (mut client, mouse_frame) = build_sliding_center_client();
    let start = Instant::now();

    open_center(&mut client, &mouse_frame);
    let wakeup_before_the_first_advance = client.compute_next_notification_wakeup(start);
    client.advance_notification_clocks(start, false);
    client.advance_notification_clocks(compute_instant_after_milliseconds(start, 80), false);
    let halfway_in = get_center(&client);
    client.apply_client_action(
        ClientActionKind::CloseNotificationCenter,
        Some(&mouse_frame),
    );
    let mode_while_sliding_out = client.get_active_input_mode();
    client.advance_notification_clocks(compute_instant_after_milliseconds(start, 100), false);
    client.advance_notification_clocks(compute_instant_after_milliseconds(start, 260), false);

    assert_eq!(wakeup_before_the_first_advance, Some(Duration::ZERO));
    assert_eq!(
        halfway_in.map(|notification_center| notification_center.hidden_per_mille),
        Some(125)
    );
    assert_eq!(mode_while_sliding_out, LockMode::Normal);
    assert_eq!(get_center(&client), None);
    assert_eq!(
        client.compute_next_notification_wakeup(compute_instant_after_milliseconds(start, 260)),
        None
    );
}

#[test]
fn a_sliding_center_wakes_once_per_frame_interval_of_the_frame_rate() {
    let (mut client, mouse_frame) = build_sliding_center_client();
    client.client_config.max_frames_per_second = 30;
    let start = Instant::now();
    open_center(&mut client, &mouse_frame);

    client.advance_notification_clocks(start, false);

    assert_eq!(
        client.compute_next_notification_wakeup(compute_instant_after_milliseconds(start, 10)),
        Some(Duration::from_nanos(33_333_334))
    );
    assert_eq!(
        client.compute_next_notification_wakeup(compute_instant_after_milliseconds(start, 150)),
        Some(Duration::from_millis(10))
    );
}

#[test]
fn the_toast_takes_the_text_rows_of_its_preview_at_the_panel_width() {
    let (mut client, mouse_frame) = build_center_client();
    let toast_before_any_post = client.build_notification_toast(&mouse_frame.committed_regions);

    client.post_core_notification(
        NotificationSeverity::Error,
        "pane …a1b2 killed by signal 9 while it wrote to a full disk",
        NonZeroU32::MIN,
        SystemTime::UNIX_EPOCH,
    );

    assert_eq!(toast_before_any_post, None);
    assert_eq!(
        client.build_notification_toast(&mouse_frame.committed_regions),
        Some(NotificationToast {
            toast_position: ToastPosition::TopRight,
            text_row_count: 2,
            hidden_per_mille: 1000,
        })
    );
}

#[test]
fn the_toast_clock_slides_the_toast_in_and_names_its_next_wakeup() {
    let (mut client, mouse_frame) = build_sliding_center_client();
    post_own_entries(&mut client, 1);
    let start = Instant::now();
    let wakeup_before_the_clock = client.compute_next_notification_wakeup(start);

    client.advance_notification_clocks(start, false);
    client.advance_notification_clocks(compute_instant_after_milliseconds(start, 500), false);

    assert_eq!(wakeup_before_the_clock, Some(Duration::ZERO));
    assert_eq!(
        client.build_notification_toast(&mouse_frame.committed_regions),
        Some(NotificationToast {
            toast_position: ToastPosition::TopRight,
            text_row_count: 1,
            hidden_per_mille: 500,
        })
    );
    assert_eq!(
        client.compute_next_notification_wakeup(compute_instant_after_milliseconds(start, 520)),
        Some(Duration::from_millis(30))
    );
}

#[test]
fn reduced_motion_and_a_longer_toast_duration_set_the_toast_clock() {
    let (mut client, mouse_frame) = build_center_client();
    client.client_config.notifications.toast_duration_ms = 5000;
    post_own_entries(&mut client, 1);
    let start = Instant::now();

    client.advance_notification_clocks(start, false);
    let first_frame = client.build_notification_toast(&mouse_frame.committed_regions);
    client.advance_notification_clocks(compute_instant_after_milliseconds(start, 4999), false);
    let last_stay_frame = client.build_notification_toast(&mouse_frame.committed_regions);
    client.advance_notification_clocks(compute_instant_after_milliseconds(start, 5000), false);

    let shown_toast = Some(NotificationToast {
        toast_position: ToastPosition::TopRight,
        text_row_count: 1,
        hidden_per_mille: 0,
    });
    assert_eq!(first_frame, shown_toast);
    assert_eq!(last_stay_frame, shown_toast);
    assert_eq!(
        client.build_notification_toast(&mouse_frame.committed_regions),
        None
    );
}

#[test]
fn a_left_press_on_the_toast_close_control_withdraws_the_toast_and_keeps_its_entry() {
    let (mut client, mouse_frame) = build_sliding_center_client();
    post_own_entries(&mut client, 1);
    let start = Instant::now();
    client.advance_notification_clocks(start, false);
    client.advance_notification_clocks(compute_instant_after_milliseconds(start, 1000), false);
    let painted_mouse_frame = paint_mouse_frame(&client, &mouse_frame);

    let press_answer = client.handle_notification_toast_mouse(
        build_left_mouse_press(TOAST_CLOSE_CONTROL_CELL),
        &painted_mouse_frame,
    );
    client.advance_notification_clocks(compute_instant_after_milliseconds(start, 1000), false);
    client.advance_notification_clocks(compute_instant_after_milliseconds(start, 2000), false);

    assert_eq!(press_answer, Some(Vec::new()));
    assert_eq!(
        client.build_notification_toast(&mouse_frame.committed_regions),
        None
    );
    assert_eq!(list_merged_entry_texts(&client), vec!["entry 0"]);
}

#[test]
fn a_press_on_a_drawn_toast_close_control_leaves_the_toast_that_replaced_it() {
    let (mut client, mouse_frame) = build_sliding_center_client();
    post_own_entries(&mut client, 2);
    let start = Instant::now();
    client.advance_notification_clocks(start, false);
    client.advance_notification_clocks(compute_instant_after_milliseconds(start, 500), false);
    let painted_mouse_frame = paint_mouse_frame(&client, &mouse_frame);
    client.advance_notification_clocks(compute_instant_after_milliseconds(start, 1500), false);
    client.advance_notification_clocks(compute_instant_after_milliseconds(start, 2000), false);
    let shown_toast_number_at_the_press =
        client.get_notification_history().find_shown_toast_number();

    let press_answer = client.handle_notification_toast_mouse(
        build_left_mouse_press(TOAST_CLOSE_CONTROL_CELL),
        &painted_mouse_frame,
    );
    client.advance_notification_clocks(compute_instant_after_milliseconds(start, 3000), false);
    client.advance_notification_clocks(compute_instant_after_milliseconds(start, 3500), false);

    assert_eq!(
        painted_mouse_frame.drawn_notifications.toast_number,
        Some(0)
    );
    assert_eq!(shown_toast_number_at_the_press, Some(1));
    assert_eq!(press_answer, Some(Vec::new()));
    assert_eq!(
        client.build_notification_toast(&mouse_frame.committed_regions),
        Some(NotificationToast {
            toast_position: ToastPosition::TopRight,
            text_row_count: 1,
            hidden_per_mille: 0,
        })
    );
}

#[test]
fn a_right_press_on_the_toast_close_control_is_taken_and_keeps_the_toast() {
    let (mut client, mouse_frame) = build_sliding_center_client();
    post_own_entries(&mut client, 1);
    let start = Instant::now();
    client.advance_notification_clocks(start, false);
    client.advance_notification_clocks(compute_instant_after_milliseconds(start, 1000), false);
    let painted_mouse_frame = paint_mouse_frame(&client, &mouse_frame);

    let press_answer = client.handle_notification_toast_mouse(
        build_right_mouse_press(TOAST_CLOSE_CONTROL_CELL),
        &painted_mouse_frame,
    );
    client.advance_notification_clocks(compute_instant_after_milliseconds(start, 2000), false);

    assert_eq!(press_answer, Some(Vec::new()));
    assert_eq!(
        client.build_notification_toast(&mouse_frame.committed_regions),
        Some(NotificationToast {
            toast_position: ToastPosition::TopRight,
            text_row_count: 1,
            hidden_per_mille: 0,
        })
    );
}

#[test]
fn a_press_on_the_toast_body_and_a_motion_on_its_close_control_take_the_next_path() {
    let (mut client, mouse_frame) = build_sliding_center_client();
    post_own_entries(&mut client, 1);
    let start = Instant::now();
    client.advance_notification_clocks(start, false);
    client.advance_notification_clocks(compute_instant_after_milliseconds(start, 1000), false);
    let painted_mouse_frame = paint_mouse_frame(&client, &mouse_frame);

    let body_answer = client.handle_notification_toast_mouse(
        build_left_mouse_press(Point { column: 60, row: 2 }),
        &painted_mouse_frame,
    );
    let motion_answer = client.handle_notification_toast_mouse(
        build_mouse_event(
            MouseKind::Motion,
            TOAST_CLOSE_CONTROL_CELL,
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
    let (mut quiet_client, mouse_frame) = build_center_client();
    quiet_client.client_config.notifications.should_show_toasts = false;
    let (mut toasting_client, _) = build_center_client();
    for client in [&mut quiet_client, &mut toasting_client] {
        client
            .take_session_notification_queue(session_id, &session_queue)
            .expect("take the attach queue");
    }
    let posted_entry = session_queue
        .post_core_entry(
            NotificationSeverity::Error,
            "pane …a1b2 killed by signal 9",
            NonZeroU32::MIN,
            SystemTime::UNIX_EPOCH,
        )
        .clone();
    for client in [&mut quiet_client, &mut toasting_client] {
        client
            .take_session_notification_post(session_queue.get_revision(), posted_entry.clone())
            .expect("take the post");
    }

    toasting_client.open_notification_center(Some(&mouse_frame));
    toasting_client.clear_notifications();

    assert_eq!(
        quiet_client.build_notification_toast(&mouse_frame.committed_regions),
        None
    );
    assert_eq!(
        list_merged_entry_texts(&quiet_client),
        vec!["pane …a1b2 killed by signal 9"]
    );
    assert_eq!(
        list_merged_entry_texts(&toasting_client),
        Vec::<String>::new()
    );
}
