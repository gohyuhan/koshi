//! The gate's untouched initial value.
//!
//! The gate is one process-wide flag, so this answer holds only before any
//! call to `set_beta_features_allowed`. This binary holds one test and nothing
//! else calls it, which keeps the answer free of test order.

/// Beta features are off in a process that never enables them.
#[test]
fn the_gate_starts_closed() {
    assert!(!koshi_beta::are_beta_features_allowed());
}
