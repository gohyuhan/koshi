//! Tests for the process-wide beta-feature gate.

use super::*;

/// One test walks every state of the gate, which is one process-wide flag.
#[test]
fn the_gate_starts_closed_and_follows_what_it_is_set_to() {
    assert!(!allowed());

    set_allowed(true);
    assert!(allowed());

    set_allowed(false);
    assert!(!allowed());
}
