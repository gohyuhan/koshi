//! Tests for typed identifiers.

use super::*;
use std::collections::HashSet;

#[test]
fn serde_roundtrip() {
    let id = PaneId::new();
    let json = serde_json::to_string(&id).expect("serialize");
    let back: PaneId = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(id, back);
}

#[test]
fn serde_uses_bare_uuid() {
    // Wire form is the bare UUID string, not the prefixed Display form.
    let uuid = Uuid::nil();
    let id = PaneId::from_uuid(uuid);
    let json = serde_json::to_string(&id).expect("serialize");
    assert_eq!(json, format!("\"{uuid}\""));
}

#[test]
fn display_is_prefixed() {
    let uuid = Uuid::nil();
    assert_eq!(
        SessionId::from_uuid(uuid).to_string(),
        "session-00000000-0000-0000-0000-000000000000"
    );
    assert_eq!(
        ClientId::from_uuid(uuid).to_string(),
        "client-00000000-0000-0000-0000-000000000000"
    );
    assert_eq!(
        TabId::from_uuid(uuid).to_string(),
        "tab-00000000-0000-0000-0000-000000000000"
    );
    assert_eq!(
        PaneId::from_uuid(uuid).to_string(),
        "pane-00000000-0000-0000-0000-000000000000"
    );
    assert_eq!(
        PluginId::from_uuid(uuid).to_string(),
        "plugin-00000000-0000-0000-0000-000000000000"
    );
    assert_eq!(
        CommandId::from_uuid(uuid).to_string(),
        "command-00000000-0000-0000-0000-000000000000"
    );
    assert_eq!(
        SubscriberId::from_uuid(uuid).to_string(),
        "subscriber-00000000-0000-0000-0000-000000000000"
    );
}

#[test]
fn debug_shows_type_and_uuid() {
    let id = PaneId::from_uuid(Uuid::nil());
    assert_eq!(
        format!("{id:?}"),
        "PaneId(00000000-0000-0000-0000-000000000000)"
    );
}

#[test]
fn from_uuid_preserves_value() {
    let uuid = Uuid::now_v7();
    assert_eq!(PaneId::from_uuid(uuid).as_uuid(), &uuid);
}

#[test]
fn ids_are_orderable() {
    // The nil UUID is all-zero, so it sorts below any v7 id (which carries a
    // non-zero timestamp). Proves the `Ord` derive directly, not just via a
    // `BTreeMap` key elsewhere.
    let low = PaneId::from_uuid(Uuid::nil());
    let high = PaneId::new();
    assert!(low < high);
}

#[test]
fn default_mints_a_fresh_id_not_a_fixed_one() {
    // `Default` delegates to `new()`, not to a fixed/nil id: two calls yield
    // distinct ids, and neither is the nil UUID.
    let a = PaneId::default();
    let b = PaneId::default();
    assert_ne!(a, b);
    assert_ne!(a, PaneId::from_uuid(Uuid::nil()));
    assert_ne!(b, PaneId::from_uuid(Uuid::nil()));
}

#[test]
fn generated_ids_are_unique() {
    const N: usize = 10_000;
    let ids: HashSet<PaneId> = (0..N).map(|_| PaneId::new()).collect();
    assert_eq!(ids.len(), N);
}

#[test]
fn as_uuid_returns_the_wrapped_value_for_every_id_type() {
    // Each type wraps the same nil UUID and hands it back unchanged, proving
    // the per-type `as_uuid` accessor on all seven.
    let uuid = Uuid::nil();
    assert_eq!(SessionId::from_uuid(uuid).as_uuid(), &uuid);
    assert_eq!(ClientId::from_uuid(uuid).as_uuid(), &uuid);
    assert_eq!(TabId::from_uuid(uuid).as_uuid(), &uuid);
    assert_eq!(PaneId::from_uuid(uuid).as_uuid(), &uuid);
    assert_eq!(PluginId::from_uuid(uuid).as_uuid(), &uuid);
    assert_eq!(CommandId::from_uuid(uuid).as_uuid(), &uuid);
    assert_eq!(SubscriberId::from_uuid(uuid).as_uuid(), &uuid);
}

#[test]
fn default_mints_a_fresh_non_nil_id_for_every_id_type() {
    // `Default` delegates to `new()` on every type: two calls differ, and
    // neither is the fixed nil UUID.
    let nil = Uuid::nil();
    assert_ne!(SessionId::default(), SessionId::default());
    assert_ne!(SessionId::default().as_uuid(), &nil);
    assert_ne!(ClientId::default(), ClientId::default());
    assert_ne!(ClientId::default().as_uuid(), &nil);
    assert_ne!(TabId::default(), TabId::default());
    assert_ne!(TabId::default().as_uuid(), &nil);
    assert_ne!(PaneId::default(), PaneId::default());
    assert_ne!(PaneId::default().as_uuid(), &nil);
    assert_ne!(PluginId::default(), PluginId::default());
    assert_ne!(PluginId::default().as_uuid(), &nil);
    assert_ne!(CommandId::default(), CommandId::default());
    assert_ne!(CommandId::default().as_uuid(), &nil);
    assert_ne!(SubscriberId::default(), SubscriberId::default());
    assert_ne!(SubscriberId::default().as_uuid(), &nil);
}

#[test]
fn an_id_reads_in_both_spellings_koshi_accepts() {
    let id = SessionId::new();
    let printed = id.to_string();
    let bare = printed
        .strip_prefix("session-")
        .expect("a session id prints with its prefix");

    assert_eq!(parse_prefixed_uuid(&printed, "session"), Ok(*id.as_uuid()));
    assert_eq!(parse_prefixed_uuid(bare, "session"), Ok(*id.as_uuid()));
}

#[test]
fn a_uuid_written_without_hyphens_reads_in_both_spellings() {
    assert_eq!(
        parse_prefixed_uuid("00000000000000000000000000000000", "session"),
        Ok(Uuid::nil())
    );
    assert_eq!(
        parse_prefixed_uuid("session-00000000000000000000000000000000", "session"),
        Ok(Uuid::nil())
    );
}

#[test]
fn empty_text_names_both_spellings() {
    assert_eq!(
        parse_prefixed_uuid("", "session"),
        Err("expected `session-<uuid>` or a bare UUID".to_string())
    );
}

#[test]
fn a_prefix_with_no_uuid_after_it_names_both_spellings() {
    assert_eq!(
        parse_prefixed_uuid("session-", "session"),
        Err("expected `session-<uuid>` or a bare UUID".to_string())
    );
    assert_eq!(
        parse_prefixed_uuid("session", "session"),
        Err("expected `session-<uuid>` or a bare UUID".to_string())
    );
}

#[test]
fn a_prefix_missing_its_hyphen_names_both_spellings() {
    assert_eq!(
        parse_prefixed_uuid("session00000000-0000-0000-0000-000000000000", "session"),
        Err("expected `session-<uuid>` or a bare UUID".to_string())
    );
}

#[test]
fn the_prefix_is_matched_case_sensitively() {
    assert_eq!(
        parse_prefixed_uuid("SESSION-00000000-0000-0000-0000-000000000000", "session"),
        Err("expected `session-<uuid>` or a bare UUID".to_string())
    );
}

#[test]
fn ids_minted_one_after_another_sort_by_creation() {
    let first = PaneId::new();
    let second = PaneId::new();
    assert!(first < second);
}

#[test]
fn deserializing_text_that_is_no_uuid_is_refused() {
    let refused = serde_json::from_str::<PaneId>("\"quiet-lake\"").expect_err("not a uuid");
    assert_eq!(
        refused.to_string(),
        "UUID parsing failed: invalid character: found `q` at 0 at line 1 column 12"
    );
}

#[test]
fn deserializing_the_prefixed_display_form_is_refused() {
    let refused = serde_json::from_str::<PaneId>("\"pane-00000000-0000-0000-0000-000000000000\"")
        .expect_err("the wire form is the bare uuid");
    assert_eq!(
        refused.to_string(),
        "UUID parsing failed: invalid character: found `p` at 0 at line 1 column 43"
    );
}

#[test]
fn an_id_carrying_another_kinds_prefix_names_both_spellings() {
    let pane = PaneId::new().to_string();

    assert_eq!(
        parse_prefixed_uuid(&pane, "session"),
        Err("expected `session-<uuid>` or a bare UUID".to_string())
    );
}

#[test]
fn text_that_is_no_uuid_at_all_names_both_spellings() {
    assert_eq!(
        parse_prefixed_uuid("quiet-lake", "session"),
        Err("expected `session-<uuid>` or a bare UUID".to_string())
    );
}
