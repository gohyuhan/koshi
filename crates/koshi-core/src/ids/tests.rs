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
        EventId::from_uuid(uuid).to_string(),
        "event-00000000-0000-0000-0000-000000000000"
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
    // the per-type `as_uuid` accessor on all eight.
    let uuid = Uuid::nil();
    assert_eq!(SessionId::from_uuid(uuid).as_uuid(), &uuid);
    assert_eq!(ClientId::from_uuid(uuid).as_uuid(), &uuid);
    assert_eq!(TabId::from_uuid(uuid).as_uuid(), &uuid);
    assert_eq!(PaneId::from_uuid(uuid).as_uuid(), &uuid);
    assert_eq!(PluginId::from_uuid(uuid).as_uuid(), &uuid);
    assert_eq!(CommandId::from_uuid(uuid).as_uuid(), &uuid);
    assert_eq!(EventId::from_uuid(uuid).as_uuid(), &uuid);
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
    assert_ne!(EventId::default(), EventId::default());
    assert_ne!(EventId::default().as_uuid(), &nil);
    assert_ne!(SubscriberId::default(), SubscriberId::default());
    assert_ne!(SubscriberId::default().as_uuid(), &nil);
}
