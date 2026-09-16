//! Unit tests for one screen's Kitty keyboard flag stack: push, pop, set,
//! the reported flags, the depth bound, and the restored entry list.

use super::*;

#[test]
fn empty_stack_reports_no_flags() {
    let keyboard_stack = KeyboardStack::default();

    assert_eq!(keyboard_stack.get_current_flags(), 0);
    assert_eq!(keyboard_stack.flag_entries, Vec::<u8>::new());
}

#[test]
fn push_of_zero_adds_an_all_off_entry() {
    let mut keyboard_stack = KeyboardStack::default();

    keyboard_stack.push_flags(0);

    assert_eq!(keyboard_stack.flag_entries, vec![0]);
    assert_eq!(keyboard_stack.get_current_flags(), 0);
}

#[test]
fn push_keeps_every_known_flag_bit() {
    let mut keyboard_stack = KeyboardStack::default();

    keyboard_stack.push_flags(17);

    assert_eq!(keyboard_stack.get_current_flags(), 17);
}

#[test]
fn push_drops_the_bits_above_the_known_flags() {
    let mut keyboard_stack = KeyboardStack::default();

    keyboard_stack.push_flags(256);
    keyboard_stack.push_flags(33);

    assert_eq!(keyboard_stack.flag_entries, vec![0, 1]);
}

#[test]
fn push_past_the_depth_bound_drops_the_oldest_entry() {
    let mut keyboard_stack = KeyboardStack::default();

    for entry_flags in 1..=9 {
        keyboard_stack.push_flags(entry_flags);
    }

    assert_eq!(keyboard_stack.flag_entries, vec![2, 3, 4, 5, 6, 7, 8, 9]);
}

#[test]
fn pop_removes_exactly_the_requested_entries() {
    let mut keyboard_stack = KeyboardStack::default();
    keyboard_stack.push_flags(1);
    keyboard_stack.push_flags(2);
    keyboard_stack.push_flags(4);

    keyboard_stack.pop_entries(2);

    assert_eq!(keyboard_stack.flag_entries, vec![1]);
    assert_eq!(keyboard_stack.get_current_flags(), 1);
}

#[test]
fn pop_past_the_entries_held_empties_the_stack() {
    let mut keyboard_stack = KeyboardStack::default();
    keyboard_stack.push_flags(1);

    keyboard_stack.pop_entries(3);

    assert_eq!(keyboard_stack.flag_entries, Vec::<u8>::new());
    assert_eq!(keyboard_stack.get_current_flags(), 0);
}

#[test]
fn pop_on_an_empty_stack_leaves_it_empty() {
    let mut keyboard_stack = KeyboardStack::default();

    keyboard_stack.pop_entries(1);

    assert_eq!(keyboard_stack.flag_entries, Vec::<u8>::new());
    assert_eq!(keyboard_stack.get_current_flags(), 0);
}

#[test]
fn set_then_push_then_pop_restores_the_flags_the_push_covered() {
    let mut keyboard_stack = KeyboardStack::default();

    keyboard_stack.set_current_flags(1, 1);
    keyboard_stack.push_flags(8);
    keyboard_stack.pop_entries(1);

    assert_eq!(keyboard_stack.flag_entries, vec![1]);
    assert_eq!(keyboard_stack.get_current_flags(), 1);

    keyboard_stack.pop_entries(1);

    assert_eq!(keyboard_stack.get_current_flags(), 0);
}

#[test]
fn set_on_an_empty_stack_creates_the_entry() {
    let mut keyboard_stack = KeyboardStack::default();

    keyboard_stack.set_current_flags(4, 1);

    assert_eq!(keyboard_stack.flag_entries, vec![4]);
}

#[test]
fn set_mode_one_replaces_the_last_entry() {
    let mut keyboard_stack = KeyboardStack::default();
    keyboard_stack.push_flags(1);
    keyboard_stack.push_flags(9);

    keyboard_stack.set_current_flags(4, 1);

    assert_eq!(keyboard_stack.flag_entries, vec![1, 4]);
}

#[test]
fn set_mode_two_adds_to_the_last_entry() {
    let mut keyboard_stack = KeyboardStack::default();
    keyboard_stack.push_flags(1);
    keyboard_stack.push_flags(9);

    keyboard_stack.set_current_flags(4, 2);

    assert_eq!(keyboard_stack.flag_entries, vec![1, 13]);
}

#[test]
fn set_mode_three_clears_from_the_last_entry() {
    let mut keyboard_stack = KeyboardStack::default();
    keyboard_stack.push_flags(1);
    keyboard_stack.push_flags(13);

    keyboard_stack.set_current_flags(4, 3);

    assert_eq!(keyboard_stack.flag_entries, vec![1, 9]);
}

#[test]
fn set_with_an_unknown_mode_changes_nothing() {
    let mut keyboard_stack = KeyboardStack::default();
    keyboard_stack.push_flags(9);

    keyboard_stack.set_current_flags(4, 7);

    assert_eq!(keyboard_stack.flag_entries, vec![9]);
}

#[test]
fn clear_empties_the_stack() {
    let mut keyboard_stack = KeyboardStack::default();
    keyboard_stack.push_flags(1);
    keyboard_stack.push_flags(8);

    keyboard_stack.clear_entries();

    assert_eq!(keyboard_stack.flag_entries, Vec::<u8>::new());
    assert_eq!(keyboard_stack.get_current_flags(), 0);
}

#[test]
fn stack_round_trips_through_its_entry_list() {
    let mut keyboard_stack = KeyboardStack::default();
    keyboard_stack.push_flags(1);
    keyboard_stack.push_flags(24);

    let serialized_stack = serde_json::to_value(&keyboard_stack).expect("stack serializes");

    assert_eq!(serialized_stack, serde_json::json!([1, 24]));

    let restored_keyboard_stack: KeyboardStack =
        serde_json::from_value(serialized_stack).expect("stack deserializes");

    assert_eq!(restored_keyboard_stack, keyboard_stack);
}

#[test]
fn restore_keeps_the_newest_entries_within_the_depth_bound() {
    let restored_keyboard_stack: KeyboardStack =
        serde_json::from_value(serde_json::json!([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]))
            .expect("stack deserializes");

    assert_eq!(
        restored_keyboard_stack.flag_entries,
        vec![3, 4, 5, 6, 7, 8, 9, 10]
    );
}

#[test]
fn restore_cuts_and_masks_the_documented_entry_list() {
    let restored_keyboard_stack: KeyboardStack =
        serde_json::from_value(serde_json::json!([1, 2, 3, 4, 5, 6, 7, 8, 9, 64]))
            .expect("stack deserializes");

    assert_eq!(
        restored_keyboard_stack.flag_entries,
        vec![3, 4, 5, 6, 7, 8, 9, 0]
    );
}

#[test]
fn restore_drops_the_bits_above_the_known_flags() {
    let restored_keyboard_stack: KeyboardStack =
        serde_json::from_value(serde_json::json!([255, 32])).expect("stack deserializes");

    assert_eq!(restored_keyboard_stack.flag_entries, vec![31, 0]);
}
