//! Tests for [`PeerInputView`]: which input makes a mark, which panes a frame
//! names, and when a mark ends.

use super::*;

/// One nanosecond, the smallest step between two `Instant`s these tests take.
const ONE_NANOSECOND: Duration = Duration::from_nanos(1);

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
fn input_in_the_typed_pane_the_last_frame_named_makes_no_mark() {
    let mut peer_input_view = PeerInputView::default();
    let bob_client_id = ClientId::new();
    let typed_pane_id = PaneId::new();
    peer_input_view.record_frame_sent([(bob_client_id, Some(typed_pane_id))]);

    assert!(!peer_input_view.record_input(bob_client_id, typed_pane_id, Instant::now()));
    assert_eq!(
        peer_input_view.list_mark_pane_ids(bob_client_id, Some(typed_pane_id)),
        Vec::<PaneId>::new()
    );
}

#[test]
fn input_in_two_panes_the_last_frame_did_not_name_makes_one_mark_each_oldest_first() {
    let mut peer_input_view = PeerInputView::default();
    let bob_client_id = ClientId::new();
    let [typed_pane_id, first_pane_id, second_pane_id] =
        [PaneId::new(), PaneId::new(), PaneId::new()];
    peer_input_view.record_frame_sent([(bob_client_id, None)]);
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
fn a_mark_no_sent_frame_named_stays_past_its_duration_and_sets_no_expiry() {
    let mut peer_input_view = PeerInputView::default();
    let bob_client_id = ClientId::new();
    let pane_id = PaneId::new();
    let input_at = Instant::now();
    let _ = peer_input_view.record_input(bob_client_id, pane_id, input_at);
    let long_after = input_at + PEER_INPUT_MARK_DURATION * 10;

    peer_input_view.remove_expired_marks(long_after);

    assert_eq!(
        peer_input_view.list_mark_pane_ids(bob_client_id, None),
        vec![pane_id]
    );
    assert!(!peer_input_view.has_expired_mark(long_after));
    assert_eq!(peer_input_view.find_next_mark_expiry(input_at), None);
}

#[test]
fn a_sent_mark_ends_its_duration_after_its_last_input() {
    let mut peer_input_view = PeerInputView::default();
    let bob_client_id = ClientId::new();
    let pane_id = PaneId::new();
    let input_at = Instant::now();
    let _ = peer_input_view.record_input(bob_client_id, pane_id, input_at);
    peer_input_view.record_frame_sent([(bob_client_id, None)]);
    let mark_expiry = input_at + PEER_INPUT_MARK_DURATION;

    assert_eq!(
        peer_input_view.find_next_mark_expiry(input_at),
        Some(mark_expiry)
    );
    assert!(!peer_input_view.has_expired_mark(mark_expiry - ONE_NANOSECOND));
    peer_input_view.remove_expired_marks(mark_expiry - ONE_NANOSECOND);
    assert_eq!(
        peer_input_view.list_mark_pane_ids(bob_client_id, None),
        vec![pane_id]
    );

    assert!(peer_input_view.has_expired_mark(mark_expiry));
    assert_eq!(peer_input_view.find_next_mark_expiry(mark_expiry), None);
    peer_input_view.remove_expired_marks(mark_expiry);
    assert_eq!(
        peer_input_view.list_mark_pane_ids(bob_client_id, None),
        Vec::<PaneId>::new()
    );
}

#[test]
fn new_input_in_a_sent_mark_moves_its_end_and_owes_no_frame() {
    let mut peer_input_view = PeerInputView::default();
    let bob_client_id = ClientId::new();
    let pane_id = PaneId::new();
    let input_at = Instant::now();
    let _ = peer_input_view.record_input(bob_client_id, pane_id, input_at);
    peer_input_view.record_frame_sent([(bob_client_id, None)]);
    let later_input_at = input_at + PEER_INPUT_MARK_DURATION;

    assert!(!peer_input_view.record_input(bob_client_id, pane_id, later_input_at));

    assert!(!peer_input_view.has_expired_mark(later_input_at));
    assert_eq!(
        peer_input_view.find_next_mark_expiry(later_input_at),
        Some(later_input_at + PEER_INPUT_MARK_DURATION)
    );
}
