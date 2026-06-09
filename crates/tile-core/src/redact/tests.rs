use super::*;

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn sensitive_keys_are_hidden() {
    // Each fragment, as a substring and in mixed case.
    let cases = [
        "GITHUB_TOKEN",
        "AWS_SECRET_ACCESS",
        "DB_PASSWORD",
        "API_KEY",
        "AUTH_HEADER",
        "Authorization",
        "my_secret_thing",
        "session-token",
    ];
    for key in cases {
        let map = env(&[(key, "leak-me")]);
        let out = redact_env_map(&map);
        assert_eq!(out[key], RedactedValue::Hidden, "key `{key}` should redact");
    }
}

#[test]
fn nonsensitive_keys_pass_through() {
    let map = env(&[("PATH", "/usr/bin"), ("HOME", "/root"), ("TERM", "xterm")]);
    let out = redact_env_map(&map);
    assert_eq!(out["PATH"], RedactedValue::Visible("/usr/bin".into()));
    assert_eq!(out["HOME"], RedactedValue::Visible("/root".into()));
    assert_eq!(out["TERM"], RedactedValue::Visible("xterm".into()));
}

#[test]
fn context_token_always_hidden_regardless_of_casing() {
    for key in [
        "TILE_CONTEXT_TOKEN",
        "tile_context_token",
        "Tile_Context_Token",
    ] {
        let map = env(&[(key, "ctx-secret")]);
        let out = redact_env_map(&map);
        assert_eq!(out[key], RedactedValue::Hidden, "`{key}` must redact");
    }
}

#[test]
fn hidden_prints_stars_in_display_and_debug() {
    let hidden = RedactedValue::Hidden;
    assert_eq!(format!("{hidden}"), "***");
    assert_eq!(format!("{hidden:?}"), "***");
}

#[test]
fn visible_shows_value_in_display() {
    let visible = RedactedValue::Visible("plain".into());
    assert_eq!(format!("{visible}"), "plain");
}

#[test]
fn redact_string_scrubs_every_marker_occurrence() {
    let line = "run --token abc123 then reuse abc123";
    let out = redact_string(line, &[Marker::literal("abc123")]);
    assert_eq!(out, "run --token *** then reuse ***");
}

#[test]
fn redact_string_applies_multiple_markers() {
    let out = redact_string(
        "user=root pass=hunter2",
        &[Marker::literal("root"), Marker::literal("hunter2")],
    );
    assert_eq!(out, "user=*** pass=***");
}

#[test]
fn redact_string_is_noop_without_markers() {
    assert_eq!(redact_string("nothing here", &[]), "nothing here");
}

#[test]
fn redact_string_ignores_empty_marker() {
    assert_eq!(redact_string("keep me", &[Marker::literal("")]), "keep me");
}
