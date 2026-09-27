//! The gate's untouched initial value.
//!
//! The gate is one process-wide flag, so this answer holds only before any
//! call to `set_beta_features_allowed`. This binary holds one test and nothing
//! else calls it, which keeps the answer free of test order.

/// Beta features are off in a process that never enables them.
#[test]
fn beta_feature_gate_starts_disabled_without_setting() {
    assert!(!koshi_beta::should_allow_beta_features());
}
