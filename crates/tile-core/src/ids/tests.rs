//! Tests for typed identifiers (FND-004).

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
fn generated_ids_are_unique() {
    const N: usize = 10_000;
    let ids: HashSet<PaneId> = (0..N).map(|_| PaneId::new()).collect();
    assert_eq!(ids.len(), N);
}
