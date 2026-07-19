//! Tests for the self-update helpers: version comparison, check scheduling,
//! archive URL construction, and state serialization.

use super::*;

#[test]
fn strip_v_drops_a_leading_v_only() {
    assert_eq!(strip_v("v1.2.3"), "1.2.3");
    assert_eq!(strip_v("1.2.3"), "1.2.3");
    assert_eq!(strip_v("version"), "ersion");
}

#[test]
fn a_far_higher_tag_is_newer() {
    assert!(is_newer("v9999.0.0"));
    assert!(is_newer("9999.0.0"));
}

#[test]
fn a_zero_tag_is_not_newer() {
    assert!(!is_newer("v0.0.0"));
}

#[test]
fn the_current_build_is_not_newer_than_itself() {
    assert!(!is_newer(APP_VERSION));
}

#[test]
fn a_malformed_tag_is_not_newer() {
    assert!(!is_newer("not-a-version"));
    assert!(!is_newer("v"));
}

#[test]
fn a_first_ever_check_is_due() {
    let state = UpdateState::default();
    assert!(is_due(&state, 14));
}

#[test]
fn a_check_within_the_interval_is_not_due() {
    let state = UpdateState {
        last_check: Some(now_secs()),
        allow_prerelease: false,
    };
    assert!(!is_due(&state, 14));
}

#[test]
fn a_check_older_than_the_interval_is_due() {
    let fifteen_days_ago = now_secs().saturating_sub(15 * SECONDS_PER_DAY);
    let state = UpdateState {
        last_check: Some(fifteen_days_ago),
        allow_prerelease: false,
    };
    assert!(is_due(&state, 14));
}

#[test]
fn a_zero_interval_is_always_due() {
    let state = UpdateState {
        last_check: Some(now_secs()),
        allow_prerelease: false,
    };
    assert!(is_due(&state, 0));
}

#[test]
fn binary_url_matches_the_release_naming_on_supported_platforms() {
    // The exact archive name is platform-specific; assert the invariant parts
    // for whichever platform the test runs on.
    let url = binary_url("v0.2.0").expect("dev + CI platforms are all supported");
    assert!(
        url.starts_with("https://github.com/gohyuhan/koshi/releases/download/v0.2.0/koshi-v0.2.0-"),
        "unexpected url: {url}"
    );
    let ext = if cfg!(windows) { ".zip" } else { ".tar.gz" };
    assert!(url.ends_with(ext), "unexpected extension in {url}");
}

#[test]
fn binary_name_is_platform_specific() {
    if cfg!(windows) {
        assert_eq!(binary_name(), "koshi.exe");
    } else {
        assert_eq!(binary_name(), "koshi");
    }
}

#[test]
fn state_defaults_when_deserialized_from_empty_object() {
    let state: UpdateState = serde_json::from_str("{}").expect("empty object is valid state");
    assert_eq!(state.last_check, None);
    assert!(!state.allow_prerelease);
}

#[test]
fn state_survives_a_serialize_deserialize_round_trip() {
    let original = UpdateState {
        last_check: Some(1_700_000_000),
        allow_prerelease: true,
    };
    let text = serde_json::to_string(&original).expect("serializable");
    let restored: UpdateState = serde_json::from_str(&text).expect("deserializable");
    assert_eq!(restored.last_check, original.last_check);
    assert_eq!(restored.allow_prerelease, original.allow_prerelease);
}
