//! Tests for the process-wide beta-feature gate.

use super::*;

/// One test walks every state, because the gate is one process-wide flag and
/// separate tests would race each other over it.
#[test]
fn the_gate_starts_closed_and_follows_what_it_is_set_to() {
    assert!(!allowed());

    set_allowed(true);
    assert!(allowed());

    set_allowed(false);
    assert!(!allowed());
}
