//! The gate's untouched initial value.
//!
//! This binary holds one test, and nothing in it calls
//! `set_beta_features_allowed`.

/// Beta features are off in a process that never enables them.
#[test]
fn beta_feature_gate_starts_disabled_without_setting() {
    assert!(!koshi_beta::should_allow_beta_features());
}
