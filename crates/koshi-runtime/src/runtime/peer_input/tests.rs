//! Tests for [`PeerInputView`]: which input makes a mark, which panes a frame
//! names, how a mark moves from queued to taken at the take of the first
//! frame that named it, when a taken mark ends, and which marks a missing
//! client or pane removes.

use super::*;

/// One nanosecond, the smallest step between two `Instant`s these tests take.
const ONE_NANOSECOND: Duration = Duration::from_nanos(1);

/// Three seconds: how long a slow client keeps a frame waiting in these tests.
const SLOW_PAINT_DURATION: Duration = Duration::from_secs(3);

#[test]
fn input_before_the_first_frame_makes_a_mark_that_the_next_frame_names() {
    let mut peer_input_view = PeerInputView::default();
    let bob_client_id = ClientId::new();
    let pane_id = PaneId::new();

    assert!(peer_input_view.record_input(bob_client_id, pane_id, Instant::now()));
    assert_eq!(
        peer_input_view.list_mark_pane_ids(bob_client_id, None),
        vec![pane_id]
    );
    assert_eq!(
        peer_input_view.list_mark_pane_ids(ClientId::new(), None),
        Vec::<PaneId>::new()
    );
}

#[test]
fn input_in_the_typed_pane_the_newest_queued_frame_named_makes_no_mark() {
    let mut peer_input_view = PeerInputView::default();
    let bob_client_id = ClientId::new();
    let typed_pane_id = PaneId::new();
    peer_input_view.record_frame_queued(1, [(bob_client_id, Some(typed_pane_id))]);

    assert!(!peer_input_view.record_input(bob_client_id, typed_pane_id, Instant::now()));
    assert!(!peer_input_view.has_marks());
}

#[test]
fn input_in_two_panes_the_newest_queued_frame_did_not_name_makes_one_mark_each_oldest_first() {
    let mut peer_input_view = PeerInputView::default();
    let bob_client_id = ClientId::new();
    let [typed_pane_id, first_pane_id, second_pane_id] =
        [PaneId::new(), PaneId::new(), PaneId::new()];
    peer_input_view.record_frame_queued(1, [(bob_client_id, None)]);
    let input_at = Instant::now();

    assert!(peer_input_view.record_input(bob_client_id, first_pane_id, input_at));
    assert!(peer_input_view.record_input(bob_client_id, second_pane_id, input_at));
    assert!(!peer_input_view.record_input(bob_client_id, first_pane_id, input_at));

    assert_eq!(
        peer_input_view.list_mark_pane_ids(bob_client_id, Some(typed_pane_id)),
        vec![first_pane_id, second_pane_id]
    );
    assert_eq!(
        peer_input_view.list_mark_pane_ids(bob_client_id, Some(first_pane_id)),
        vec![second_pane_id]
    );
}

#[test]
fn a_mark_no_taken_frame_named_never_expires_whether_queued_or_not() {
    let mut peer_input_view = PeerInputView::default();
    let bob_client_id = ClientId::new();
    let [unqueued_pane_id, queued_pane_id] = [PaneId::new(), PaneId::new()];
    let input_at = Instant::now();
    let _ = peer_input_view.record_input(bob_client_id, queued_pane_id, input_at);
    peer_input_view.record_frame_queued(1, [(bob_client_id, None)]);
    let _ = peer_input_view.record_input(bob_client_id, unqueued_pane_id, input_at);
    let long_after = input_at + PEER_INPUT_MARK_DURATION * 10;

    assert_eq!(peer_input_view.find_first_queued_frame_number(), Some(1));
    assert!(!peer_input_view.remove_expired_marks(long_after));
    assert_eq!(
        peer_input_view.list_mark_pane_ids(bob_client_id, None),
        vec![queued_pane_id, unqueued_pane_id]
    );
    assert_eq!(peer_input_view.find_next_mark_expiry(), None);
}

#[test]
fn a_mark_taken_after_a_slow_paint_ends_its_duration_after_the_take() {
    let mut peer_input_view = PeerInputView::default();
    let bob_client_id = ClientId::new();
    let pane_id = PaneId::new();
    let input_at = Instant::now();
    let _ = peer_input_view.record_input(bob_client_id, pane_id, input_at);
    peer_input_view.record_frame_queued(1, [(bob_client_id, None)]);
    let taken_at = input_at + SLOW_PAINT_DURATION;

    peer_input_view.record_frame_taken(1, taken_at);

    let mark_expiry = taken_at + PEER_INPUT_MARK_DURATION;
    assert_eq!(peer_input_view.find_first_queued_frame_number(), None);
    assert_eq!(peer_input_view.find_next_mark_expiry(), Some(mark_expiry));
    assert!(!peer_input_view.remove_expired_marks(mark_expiry - ONE_NANOSECOND));
    assert_eq!(
        peer_input_view.list_mark_pane_ids(bob_client_id, None),
        vec![pane_id]
    );
    assert!(peer_input_view.remove_expired_marks(mark_expiry));
    assert!(!peer_input_view.has_marks());
    assert_eq!(peer_input_view.find_next_mark_expiry(), None);
}

#[test]
fn new_input_in_a_taken_mark_moves_its_end_and_makes_no_mark() {
    let mut peer_input_view = PeerInputView::default();
    let bob_client_id = ClientId::new();
    let pane_id = PaneId::new();
    let input_at = Instant::now();
    let _ = peer_input_view.record_input(bob_client_id, pane_id, input_at);
    peer_input_view.record_frame_queued(1, [(bob_client_id, None)]);
    peer_input_view.record_frame_taken(1, input_at);
    let later_input_at = input_at + PEER_INPUT_MARK_DURATION;

    assert!(!peer_input_view.record_input(bob_client_id, pane_id, later_input_at));

    assert_eq!(
        peer_input_view.find_next_mark_expiry(),
        Some(later_input_at + PEER_INPUT_MARK_DURATION)
    );
}

#[test]
fn an_overdue_mark_keeps_its_past_expiry_until_it_is_removed() {
    let mut peer_input_view = PeerInputView::default();
    let bob_client_id = ClientId::new();
    let input_at = Instant::now();
    let _ = peer_input_view.record_input(bob_client_id, PaneId::new(), input_at);
    peer_input_view.record_frame_queued(1, [(bob_client_id, None)]);
    peer_input_view.record_frame_taken(1, input_at);

    assert_eq!(
        peer_input_view.find_next_mark_expiry(),
        Some(input_at + PEER_INPUT_MARK_DURATION)
    );
    assert!(peer_input_view.remove_expired_marks(input_at + PEER_INPUT_MARK_DURATION * 3));
    assert_eq!(peer_input_view.find_next_mark_expiry(), None);
}

#[test]
fn a_queued_frame_queues_only_the_marks_no_frame_named_and_a_take_leaves_earlier_takes_alone() {
    let mut peer_input_view = PeerInputView::default();
    let bob_client_id = ClientId::new();
    let [first_pane_id, second_pane_id] = [PaneId::new(), PaneId::new()];
    let first_input_at = Instant::now();
    let _ = peer_input_view.record_input(bob_client_id, first_pane_id, first_input_at);
    peer_input_view.record_frame_queued(1, [(bob_client_id, None)]);
    peer_input_view.record_frame_taken(1, first_input_at);
    let second_input_at = first_input_at + Duration::from_secs(1);
    let _ = peer_input_view.record_input(bob_client_id, second_pane_id, second_input_at);
    assert_eq!(peer_input_view.find_first_queued_frame_number(), None);

    peer_input_view.record_frame_queued(2, [(bob_client_id, None)]);
    assert_eq!(peer_input_view.find_first_queued_frame_number(), Some(2));
    let second_taken_at = second_input_at + SLOW_PAINT_DURATION;
    peer_input_view.record_frame_taken(2, second_taken_at);

    assert!(peer_input_view.remove_expired_marks(first_input_at + PEER_INPUT_MARK_DURATION));
    assert_eq!(
        peer_input_view.list_mark_pane_ids(bob_client_id, None),
        vec![second_pane_id]
    );
    assert_eq!(
        peer_input_view.find_next_mark_expiry(),
        Some(second_taken_at + PEER_INPUT_MARK_DURATION)
    );
}

#[test]
fn an_absent_client_or_pane_removes_its_marks_in_every_state() {
    let mut peer_input_view = PeerInputView::default();
    let [bob_client_id, carol_client_id] = [ClientId::new(), ClientId::new()];
    let [kept_pane_id, closed_pane_id] = [PaneId::new(), PaneId::new()];
    let input_at = Instant::now();
    let _ = peer_input_view.record_input(bob_client_id, kept_pane_id, input_at);
    peer_input_view.record_frame_queued(1, []);
    peer_input_view.record_frame_taken(1, input_at);
    let _ = peer_input_view.record_input(bob_client_id, closed_pane_id, input_at);
    peer_input_view.record_frame_queued(2, []);
    let _ = peer_input_view.record_input(carol_client_id, kept_pane_id, input_at);

    peer_input_view.remove_absent_marks(|client_id, pane_id| {
        client_id == bob_client_id && pane_id == kept_pane_id
    });

    assert_eq!(
        peer_input_view.list_mark_pane_ids(bob_client_id, None),
        vec![kept_pane_id]
    );
    assert_eq!(
        peer_input_view.list_mark_pane_ids(carol_client_id, None),
        Vec::<PaneId>::new()
    );
    assert_eq!(peer_input_view.find_first_queued_frame_number(), None);
}

/// Bob's mark in pane 1 is queued at frame 1, his mark in pane 2 at frame 2,
/// and frame 3 names both. The take of frame 1 counts only the first mark as
/// taken, and the take of frame 2 counts the second.
#[test]
fn a_take_counts_only_the_marks_whose_first_frame_is_the_taken_frame_or_an_earlier_one() {
    let mut peer_input_view = PeerInputView::default();
    let bob_client_id = ClientId::new();
    let [first_pane_id, second_pane_id] = [PaneId::new(), PaneId::new()];
    let input_at = Instant::now();
    let _ = peer_input_view.record_input(bob_client_id, first_pane_id, input_at);
    peer_input_view.record_frame_queued(1, [(bob_client_id, None)]);
    let _ = peer_input_view.record_input(bob_client_id, second_pane_id, input_at);
    peer_input_view.record_frame_queued(2, [(bob_client_id, None)]);
    peer_input_view.record_frame_queued(3, [(bob_client_id, None)]);
    assert_eq!(peer_input_view.find_first_queued_frame_number(), Some(1));

    let first_taken_at = input_at + Duration::from_secs(1);
    peer_input_view.record_frame_taken(1, first_taken_at);

    assert_eq!(peer_input_view.find_first_queued_frame_number(), Some(2));
    assert_eq!(
        peer_input_view.find_next_mark_expiry(),
        Some(first_taken_at + PEER_INPUT_MARK_DURATION)
    );
    assert!(peer_input_view.remove_expired_marks(first_taken_at + PEER_INPUT_MARK_DURATION));
    assert_eq!(
        peer_input_view.list_mark_pane_ids(bob_client_id, None),
        vec![second_pane_id]
    );

    let second_taken_at = input_at + SLOW_PAINT_DURATION * 2;
    peer_input_view.record_frame_taken(2, second_taken_at);

    assert_eq!(peer_input_view.find_first_queued_frame_number(), None);
    assert_eq!(
        peer_input_view.find_next_mark_expiry(),
        Some(second_taken_at + PEER_INPUT_MARK_DURATION)
    );
}
