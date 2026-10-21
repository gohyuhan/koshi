//! Tests for typed identifiers.

use super::*;
use std::collections::HashSet;

#[test]
fn serde_roundtrip_preserves_pane_id() {
    let pane_id = PaneId::new();
    let serialized_pane_id = serde_json::to_string(&pane_id).expect("serialize");
    let deserialized_pane_id: PaneId =
        serde_json::from_str(&serialized_pane_id).expect("deserialize");
    assert_eq!(pane_id, deserialized_pane_id);
}

#[test]
fn serde_uses_bare_uuid_wire_form() {
    // Wire form is the bare UUID string, not the prefixed Display form.
    let uuid = Uuid::nil();
    let pane_id = PaneId::from_uuid(uuid);
    let serialized_pane_id = serde_json::to_string(&pane_id).expect("serialize");
    assert_eq!(serialized_pane_id, format!("\"{uuid}\""));
}

#[test]
fn id_display_includes_each_type_prefix() {
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
        CommandId::from_uuid(uuid).to_string(),
        "command-00000000-0000-0000-0000-000000000000"
    );
    assert_eq!(
        SubscriberId::from_uuid(uuid).to_string(),
        "subscriber-00000000-0000-0000-0000-000000000000"
    );
}

#[test]
fn pane_id_debug_includes_type_and_uuid() {
    let pane_id = PaneId::from_uuid(Uuid::nil());
    assert_eq!(
        format!("{pane_id:?}"),
        "PaneId(00000000-0000-0000-0000-000000000000)"
    );
}

#[test]
fn from_uuid_preserves_uuid() {
    let uuid = Uuid::now_v7();
    assert_eq!(PaneId::from_uuid(uuid).get_uuid(), &uuid);
}

#[test]
fn pane_ids_are_orderable() {
    // The nil UUID is all-zero, so it sorts below any v7 id (which carries a
    // non-zero timestamp). Proves the `Ord` derive directly, not just via a
    // `BTreeMap` key elsewhere.
    let lowest_pane_id = PaneId::from_uuid(Uuid::nil());
    let highest_pane_id = PaneId::new();
    assert!(lowest_pane_id < highest_pane_id);
}

#[test]
fn default_mints_a_fresh_id_not_a_fixed_one() {
    // `Default` delegates to `new()`, not to a fixed/nil id: two calls yield
    // distinct ids, and neither is the nil UUID.
    let first_pane_id = PaneId::default();
    let second_pane_id = PaneId::default();
    assert_ne!(first_pane_id, second_pane_id);
    assert_ne!(first_pane_id, PaneId::from_uuid(Uuid::nil()));
    assert_ne!(second_pane_id, PaneId::from_uuid(Uuid::nil()));
}

#[test]
fn generated_pane_ids_are_unique() {
    const GENERATED_PANE_ID_COUNT: usize = 10_000;
    let generated_pane_ids: HashSet<PaneId> = (0..GENERATED_PANE_ID_COUNT)
        .map(|_| PaneId::new())
        .collect();
    assert_eq!(generated_pane_ids.len(), GENERATED_PANE_ID_COUNT);
}

#[test]
fn get_uuid_returns_the_wrapped_value_for_every_id_type() {
    // Each type wraps the same nil UUID and hands it back unchanged, proving
    // the per-type `get_uuid` accessor on all six.
    let uuid = Uuid::nil();
    assert_eq!(SessionId::from_uuid(uuid).get_uuid(), &uuid);
    assert_eq!(ClientId::from_uuid(uuid).get_uuid(), &uuid);
    assert_eq!(TabId::from_uuid(uuid).get_uuid(), &uuid);
    assert_eq!(PaneId::from_uuid(uuid).get_uuid(), &uuid);
    assert_eq!(CommandId::from_uuid(uuid).get_uuid(), &uuid);
    assert_eq!(SubscriberId::from_uuid(uuid).get_uuid(), &uuid);
}

#[test]
fn default_mints_a_fresh_non_nil_id_for_every_id_type() {
    // `Default` delegates to `new()` on every type: two calls differ, and
    // neither is the fixed nil UUID.
    let nil_uuid = Uuid::nil();
    assert_ne!(SessionId::default(), SessionId::default());
    assert_ne!(SessionId::default().get_uuid(), &nil_uuid);
    assert_ne!(ClientId::default(), ClientId::default());
    assert_ne!(ClientId::default().get_uuid(), &nil_uuid);
    assert_ne!(TabId::default(), TabId::default());
    assert_ne!(TabId::default().get_uuid(), &nil_uuid);
    assert_ne!(PaneId::default(), PaneId::default());
    assert_ne!(PaneId::default().get_uuid(), &nil_uuid);
    assert_ne!(CommandId::default(), CommandId::default());
    assert_ne!(CommandId::default().get_uuid(), &nil_uuid);
    assert_ne!(SubscriberId::default(), SubscriberId::default());
    assert_ne!(SubscriberId::default().get_uuid(), &nil_uuid);
}

#[test]
fn session_id_accepts_prefixed_and_bare_uuid_text() {
    let session_id = SessionId::new();
    let prefixed_session_id_text = session_id.to_string();
    let bare_session_uuid_text = prefixed_session_id_text
        .strip_prefix("session-")
        .expect("a session id prints with its prefix");

    assert_eq!(
        parse_prefixed_uuid(&prefixed_session_id_text, "session"),
        Ok(*session_id.get_uuid())
    );
    assert_eq!(
        parse_prefixed_uuid(bare_session_uuid_text, "session"),
        Ok(*session_id.get_uuid())
    );
}

#[test]
fn parse_prefixed_uuid_accepts_hyphenless_uuid_with_and_without_prefix() {
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
fn empty_session_id_text_returns_uuid_format_error() {
    assert_eq!(
        parse_prefixed_uuid("", "session"),
        Err("expected `session-<uuid>` or a bare UUID".to_string())
    );
}

#[test]
fn a_session_prefix_without_uuid_returns_uuid_format_error() {
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
fn a_session_prefix_without_hyphen_returns_uuid_format_error() {
    assert_eq!(
        parse_prefixed_uuid("session00000000-0000-0000-0000-000000000000", "session"),
        Err("expected `session-<uuid>` or a bare UUID".to_string())
    );
}

#[test]
fn uppercase_session_prefix_is_rejected() {
    assert_eq!(
        parse_prefixed_uuid("SESSION-00000000-0000-0000-0000-000000000000", "session"),
        Err("expected `session-<uuid>` or a bare UUID".to_string())
    );
}

#[test]
fn ids_minted_one_after_another_sort_by_creation() {
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    assert!(first_pane_id < second_pane_id);
}

#[test]
fn deserializing_text_that_is_no_uuid_is_refused() {
    let deserialization_error =
        serde_json::from_str::<PaneId>("\"quiet-lake\"").expect_err("not a uuid");
    assert_eq!(
        deserialization_error.to_string(),
        "UUID parsing failed: invalid character: found `q` at 0 at line 1 column 12"
    );
}

#[test]
fn deserializing_the_prefixed_display_form_is_refused() {
    let deserialization_error =
        serde_json::from_str::<PaneId>("\"pane-00000000-0000-0000-0000-000000000000\"")
            .expect_err("the wire form is the bare uuid");
    assert_eq!(
        deserialization_error.to_string(),
        "UUID parsing failed: invalid character: found `p` at 0 at line 1 column 43"
    );
}

#[test]
fn pane_id_prefix_is_rejected_when_parsing_session_id() {
    let pane_id_text = PaneId::new().to_string();

    assert_eq!(
        parse_prefixed_uuid(&pane_id_text, "session"),
        Err("expected `session-<uuid>` or a bare UUID".to_string())
    );
}

#[test]
fn non_uuid_text_is_rejected_for_a_session_id() {
    assert_eq!(
        parse_prefixed_uuid("quiet-lake", "session"),
        Err("expected `session-<uuid>` or a bare UUID".to_string())
    );
}

#[test]
fn an_owned_session_id_reads_a_user_id_then_a_session_id() {
    let session_uuid = Uuid::parse_str("0192f0c1-0000-7000-8000-000000000001").expect("uuid");
    let owned_session_id = OwnedSessionId {
        owner_user_id: 1001,
        session_id: SessionId::from_uuid(session_uuid),
    };

    assert_eq!(
        parse_owned_session_id(&format!("1001/session-{session_uuid}")),
        Ok(Some(owned_session_id))
    );
    assert_eq!(
        parse_owned_session_id(&format!("1001/{session_uuid}")),
        Ok(Some(owned_session_id))
    );
    assert_eq!(
        parse_owned_session_id(&format!("0/session-{session_uuid}")),
        Ok(Some(OwnedSessionId {
            owner_user_id: 0,
            session_id: SessionId::from_uuid(session_uuid),
        }))
    );
    assert_eq!(
        owned_session_id.to_string(),
        format!("1001/session-{session_uuid}")
    );
}

#[test]
fn a_session_target_carries_the_owner_user_id_only_when_one_is_set() {
    let session_id = SessionId::from_uuid(
        Uuid::parse_str("0192f0c1-0000-7000-8000-000000000001").expect("uuid"),
    );

    assert_eq!(
        format_session_target(session_id, Some(1001)),
        "1001/session-0192f0c1-0000-7000-8000-000000000001"
    );
    assert_eq!(
        format_session_target(session_id, None),
        "session-0192f0c1-0000-7000-8000-000000000001"
    );
}

#[test]
fn text_with_no_slash_is_not_an_owned_session_id() {
    assert_eq!(parse_owned_session_id("quiet-lake"), Ok(None));
    assert_eq!(
        parse_owned_session_id("session-0192f0c1-0000-7000-8000-000000000001"),
        Ok(None)
    );
    assert_eq!(parse_owned_session_id(""), Ok(None));
}

#[test]
fn an_owned_session_id_refuses_any_other_text_around_the_slash() {
    let expected_error = Err(String::from(
        "expected `<user id>/session-<uuid>`, such as `1001/session-<uuid>`",
    ));
    for refused_text in [
        "01001/session-0192f0c1-0000-7000-8000-000000000001",
        "+1001/session-0192f0c1-0000-7000-8000-000000000001",
        "-1/session-0192f0c1-0000-7000-8000-000000000001",
        "4294967296/session-0192f0c1-0000-7000-8000-000000000001",
        "alice/session-0192f0c1-0000-7000-8000-000000000001",
        "/session-0192f0c1-0000-7000-8000-000000000001",
        "1001/",
        "1001/quiet-lake",
        "1001/pane-0192f0c1-0000-7000-8000-000000000001",
        "1001/session-0192f0c1-0000-7000-8000-000000000001/extra",
    ] {
        assert_eq!(
            parse_owned_session_id(refused_text),
            expected_error,
            "{refused_text}"
        );
    }
}
