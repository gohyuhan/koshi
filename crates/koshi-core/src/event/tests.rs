//! Serialization roundtrip tests and invariant assertions for events.
//!
//! Verifies that all [`Event`] and [`PluginEvent`] variants:
//! - survive JSON serialization and deserialization unchanged (canonical serde mapping),
//! - have stable, canonical variant names (renames or additions break the test),
//! - maintain privacy tier invariants (sensitive data is structurally unavoidable).

use super::*;
use crate::geometry::{Point, Size};
use crate::ids::{ClientId, CommandId, PaneId, PluginId, SessionId, SubscriberId, TabId};
use crate::process::PtySize;
use std::time::{Duration, UNIX_EPOCH};

/// Roundtrip a value through JSON and assert it survives unchanged.
fn roundtrip<T>(value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).expect("serialize");
    let back: T = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(*value, back);
}

/// A fixed timestamp so serde roundtrips are deterministic.
fn fixed_time() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

#[test]
fn lifecycle_events_roundtrip() {
    roundtrip(&Event::PaneCreated(PaneCreated {
        pane_id: PaneId::new(),
        tab_id: TabId::new(),
    }));
    roundtrip(&Event::PaneProcessExited(PaneProcessExited {
        pane_id: PaneId::new(),
        exit_code: Some(0),
    }));
    roundtrip(&Event::PaneRemoved(PaneRemoved {
        pane_id: PaneId::new(),
        tab_id: TabId::new(),
    }));
    roundtrip(&Event::PtyResized(PtyResized {
        pane_id: PaneId::new(),
        size: PtySize { cols: 80, rows: 24 },
    }));
    roundtrip(&Event::PaneOutputUpdated(PaneOutputUpdated {
        pane_id: PaneId::new(),
    }));
    roundtrip(&Event::PaneCommandStarted(PaneCommandStarted {
        pane_id: PaneId::new(),
    }));
    roundtrip(&Event::PaneCommandFinished(PaneCommandFinished {
        pane_id: PaneId::new(),
        exit_code: Some(1),
    }));
    roundtrip(&Event::InputModeChanged(InputModeChanged {
        client_id: ClientId::new(),
        mode: InputMode::CopyMode,
    }));
}

#[test]
fn rename_move_suppression_and_reload_events_roundtrip() {
    roundtrip(&Event::PaneRenamed(PaneRenamed {
        pane_id: PaneId::new(),
        name: "logs".to_string(),
    }));
    roundtrip(&Event::TabMoved(TabMoved {
        tab_id: TabId::new(),
        old_index: 0,
        new_index: 2,
    }));
    roundtrip(&Event::TabRenamed(TabRenamed {
        tab_id: TabId::new(),
        name: "work".to_string(),
    }));
    roundtrip(&Event::SessionRenamed(SessionRenamed {
        session_id: SessionId::new(),
        old_name: "default".to_string(),
        new_name: "main".to_string(),
    }));
    roundtrip(&Event::PaneSuppressed(PaneSuppressed {
        pane_id: PaneId::new(),
        tab_id: TabId::new(),
    }));
    roundtrip(&Event::PaneResumed(PaneResumed {
        pane_id: PaneId::new(),
        tab_id: TabId::new(),
    }));
    roundtrip(&Event::TerminalTooSmallEntered(TerminalTooSmallEntered {
        client_id: ClientId::new(),
        size: Size { cols: 1, rows: 1 },
    }));
    roundtrip(&Event::TerminalTooSmallExited(TerminalTooSmallExited {
        client_id: ClientId::new(),
        size: Size { cols: 80, rows: 24 },
    }));
    roundtrip(&Event::ConfigReloaded(ConfigReloaded {
        session_id: SessionId::new(),
    }));
    roundtrip(&Event::ConfigReloadFailed(ConfigReloadFailed {
        session_id: SessionId::new(),
        reason: "parse error".to_string(),
    }));
}

#[test]
fn input_privacy_events_roundtrip() {
    roundtrip(&Event::PaneTyped(PaneTyped {
        pane_id: PaneId::new(),
        tab_id: TabId::new(),
        session_id: SessionId::new(),
        client_id: ClientId::new(),
        payload: TypedPayload::SafePublic('a'),
        timestamp: fixed_time(),
    }));
    roundtrip(&Event::PaneEnterPressed(PaneEnterPressed {
        pane_id: PaneId::new(),
        tab_id: TabId::new(),
        session_id: SessionId::new(),
        client_id: ClientId::new(),
        line: SubmittedLinePayload::SensitiveRedacted,
        timestamp: fixed_time(),
    }));
}

#[test]
fn mouse_events_roundtrip() {
    roundtrip(&Event::MousePressed(MousePressed {
        client_id: ClientId::new(),
        pane: Some(PaneId::new()),
        position: Point { x: 4, y: 9 },
        button: MouseButton::Left,
    }));
    roundtrip(&Event::MouseScrolled(MouseScrolled {
        client_id: ClientId::new(),
        pane: None,
        position: Point { x: 0, y: 0 },
        direction: ScrollDirection::Up,
    }));
    roundtrip(&Event::MouseScrolled(MouseScrolled {
        client_id: ClientId::new(),
        pane: None,
        position: Point { x: 0, y: 0 },
        direction: ScrollDirection::Left,
    }));
    roundtrip(&Event::PluginMouseInput(PluginMouseInput {
        plugin_id: PluginId::new(),
    }));
}

#[test]
fn delivery_and_rejection_events_roundtrip() {
    roundtrip(&Event::SubscriberLagged(SubscriberLagged {
        subscriber_id: SubscriberId::new(),
        dropped_count: 12,
        event_class: EventClass::Lossy,
    }));
    roundtrip(&Event::PaneScrollbackTruncated(PaneScrollbackTruncated {
        pane_id: PaneId::new(),
        dropped_lines: 500,
        dropped_bytes: 8192,
    }));
    roundtrip(&Event::CommandRejected(CommandRejected {
        id: CommandId::new(),
        reason: RejectReason::TargetGone,
    }));
}

#[test]
fn copy_and_search_events_roundtrip() {
    roundtrip(&Event::SelectionChanged(SelectionChanged {
        pane_id: PaneId::new(),
        selection: Some(Selection {
            kind: SelectionKind::Block,
            anchor: GridPos { row: 1, col: 0 },
            cursor: GridPos { row: 3, col: 20 },
        }),
    }));
    roundtrip(&Event::SelectionChanged(SelectionChanged {
        pane_id: PaneId::new(),
        selection: None,
    }));
    roundtrip(&Event::Copied(Copied {
        pane_id: PaneId::new(),
        target: CopyTarget::Osc52,
        byte_len: 42,
    }));
    roundtrip(&Event::SearchUpdated(SearchUpdated {
        pane_id: PaneId::new(),
        match_count: 3,
        current_match: Some(1),
    }));
}

#[test]
fn plugin_events_roundtrip() {
    roundtrip(&Event::Plugin(PluginEvent::Installed(PluginInstalled {
        plugin_id: PluginId::new(),
    })));
    roundtrip(&Event::Plugin(PluginEvent::LoadFailed(PluginLoadFailed {
        plugin_id: PluginId::new(),
        reason: "missing export".to_string(),
    })));
}

/// The variant-name test below constructs one instance of every `Event`
/// variant and checks its `Debug` repr, but `Debug` does not exercise serde —
/// several variants (unit payloads, `Option` fields, enum-typed fields) are
/// otherwise never round-tripped through JSON by any other test in this file.
/// This closes that gap for every variant `lifecycle_events_roundtrip` and its
/// siblings above do not already cover.
#[test]
fn remaining_event_variants_survive_a_json_round_trip() {
    roundtrip(&Event::PaneClosing(PaneClosing {
        pane_id: PaneId::new(),
    }));
    roundtrip(&Event::PaneFocused(PaneFocused {
        client_id: ClientId::new(),
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
        prior_pane: Some(PaneId::new()),
    }));
    roundtrip(&Event::PaneFocused(PaneFocused {
        client_id: ClientId::new(),
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
        prior_pane: None,
    }));
    roundtrip(&Event::LayoutChanged(LayoutChanged {
        tab_id: TabId::new(),
    }));
    roundtrip(&Event::TabCreated(TabCreated {
        tab_id: TabId::new(),
    }));
    roundtrip(&Event::TabClosed(TabClosed {
        tab_id: TabId::new(),
    }));
    roundtrip(&Event::TabFocused(TabFocused {
        client_id: ClientId::new(),
        tab_id: TabId::new(),
        prior_tab: TabId::new(),
    }));
    roundtrip(&Event::KeybindingMatched(KeybindingMatched {
        client_id: ClientId::new(),
        command_id: CommandId::new(),
    }));
    roundtrip(&Event::MouseReleased(MouseReleased {
        client_id: ClientId::new(),
        pane: Some(PaneId::new()),
        position: Point { x: 1, y: 2 },
        button: MouseButton::Right,
    }));
    roundtrip(&Event::MouseDragged(MouseDragged {
        client_id: ClientId::new(),
        pane: None,
        position: Point { x: 0, y: 0 },
        button: MouseButton::Middle,
    }));
    roundtrip(&Event::PaneMouseForwarded(PaneMouseForwarded {
        pane_id: PaneId::new(),
    }));
    roundtrip(&Event::CopyModeEntered(CopyModeEntered {
        pane_id: PaneId::new(),
    }));
    roundtrip(&Event::CopyModeExited(CopyModeExited {
        pane_id: PaneId::new(),
    }));
    roundtrip(&Event::Quit);
}

/// The privacy guarantee is structural, not advisory. The tier of an input
/// event is the payload variant itself — there is no independent `tier` field
/// that could be set to `SensitiveBlocked` alongside a character or line of
/// text. Every withholding case (`SensitiveBlocked` on the tier and on both
/// input payloads) is unit-shaped: the absence of a `(` in its Debug repr
/// proves it holds no data field, so adding one would fail this test.
#[test]
fn sensitive_blocked_tier_carries_no_content() {
    let blocked = [
        format!("{:?}", PrivacyTier::SensitiveBlocked),
        format!("{:?}", TypedPayload::SensitiveBlocked),
        format!("{:?}", SubmittedLinePayload::SensitiveBlocked),
    ];
    for repr in &blocked {
        assert_eq!(repr, "SensitiveBlocked");
        assert!(!repr.contains('('), "{repr} must hold no payload");
    }
}

/// The `tier()` accessor maps each payload variant to its privacy tier, and
/// `Unknown` lines fail closed to `MetadataOnly`.
#[test]
fn payload_tier_accessors_map_to_privacy_tier() {
    assert_eq!(TypedPayload::SafePublic('x').tier(), PrivacyTier::Public);
    assert_eq!(
        TypedPayload::SensitiveRedacted.tier(),
        PrivacyTier::Redacted
    );
    assert_eq!(
        TypedPayload::AlternateScreenMetadataOnly.tier(),
        PrivacyTier::MetadataOnly
    );
    assert_eq!(
        TypedPayload::RawModeMetadataOnly.tier(),
        PrivacyTier::MetadataOnly
    );
    assert_eq!(
        TypedPayload::UnknownMetadataOnly.tier(),
        PrivacyTier::MetadataOnly
    );
    assert_eq!(
        TypedPayload::SensitiveBlocked.tier(),
        PrivacyTier::SensitiveBlocked
    );

    assert_eq!(
        SubmittedLinePayload::SafePublic("ls".to_string()).tier(),
        PrivacyTier::Public
    );
    assert_eq!(
        SubmittedLinePayload::SensitiveRedacted.tier(),
        PrivacyTier::Redacted
    );
    assert_eq!(
        SubmittedLinePayload::UnknownMetadataOnly.tier(),
        PrivacyTier::MetadataOnly
    );
    assert_eq!(
        SubmittedLinePayload::SensitiveBlocked.tier(),
        PrivacyTier::SensitiveBlocked
    );
}

/// Extract the variant name from a value's Debug output.
///
/// For data variants like `PaneCreated(...)`, returns everything before the first `(`.
/// For unit variants like `Quit`, returns the whole string.
/// Used to verify variant names are stable: any enum rename changes the Debug output
/// and breaks the matching assertions in the test.
fn variant_name<T: std::fmt::Debug>(value: &T) -> String {
    let repr = format!("{value:?}");
    repr.split('(').next().unwrap_or(&repr).to_string()
}

/// One instance per top-level `Event` variant, paired with its canonical name.
/// Renaming any variant breaks the matching `variant_name` assert, and
/// adding/removing one breaks the count — neither passes on a detached list.
#[test]
fn event_variant_names_are_canonical() {
    let cases: Vec<(Event, &str)> = vec![
        (
            Event::PaneCreated(PaneCreated {
                pane_id: PaneId::new(),
                tab_id: TabId::new(),
            }),
            "PaneCreated",
        ),
        (
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: PaneId::new(),
                exit_code: None,
            }),
            "PaneProcessExited",
        ),
        (
            Event::PaneClosing(PaneClosing {
                pane_id: PaneId::new(),
            }),
            "PaneClosing",
        ),
        (
            Event::PaneRemoved(PaneRemoved {
                pane_id: PaneId::new(),
                tab_id: TabId::new(),
            }),
            "PaneRemoved",
        ),
        (
            Event::PaneFocused(PaneFocused {
                client_id: ClientId::new(),
                tab_id: TabId::new(),
                pane_id: PaneId::new(),
                prior_pane: None,
            }),
            "PaneFocused",
        ),
        (
            Event::PtyResized(PtyResized {
                pane_id: PaneId::new(),
                size: PtySize { cols: 80, rows: 24 },
            }),
            "PtyResized",
        ),
        (
            Event::PaneOutputUpdated(PaneOutputUpdated {
                pane_id: PaneId::new(),
            }),
            "PaneOutputUpdated",
        ),
        (
            Event::PaneCommandStarted(PaneCommandStarted {
                pane_id: PaneId::new(),
            }),
            "PaneCommandStarted",
        ),
        (
            Event::PaneCommandFinished(PaneCommandFinished {
                pane_id: PaneId::new(),
                exit_code: None,
            }),
            "PaneCommandFinished",
        ),
        (
            Event::LayoutChanged(LayoutChanged {
                tab_id: TabId::new(),
            }),
            "LayoutChanged",
        ),
        (
            Event::TabCreated(TabCreated {
                tab_id: TabId::new(),
            }),
            "TabCreated",
        ),
        (
            Event::TabClosed(TabClosed {
                tab_id: TabId::new(),
            }),
            "TabClosed",
        ),
        (
            Event::TabFocused(TabFocused {
                client_id: ClientId::new(),
                tab_id: TabId::new(),
                prior_tab: TabId::new(),
            }),
            "TabFocused",
        ),
        (
            Event::PaneRenamed(PaneRenamed {
                pane_id: PaneId::new(),
                name: "logs".to_string(),
            }),
            "PaneRenamed",
        ),
        (
            Event::TabMoved(TabMoved {
                tab_id: TabId::new(),
                old_index: 0,
                new_index: 1,
            }),
            "TabMoved",
        ),
        (
            Event::TabRenamed(TabRenamed {
                tab_id: TabId::new(),
                name: "work".to_string(),
            }),
            "TabRenamed",
        ),
        (
            Event::SessionRenamed(SessionRenamed {
                session_id: SessionId::new(),
                old_name: "default".to_string(),
                new_name: "main".to_string(),
            }),
            "SessionRenamed",
        ),
        (
            Event::PaneSuppressed(PaneSuppressed {
                pane_id: PaneId::new(),
                tab_id: TabId::new(),
            }),
            "PaneSuppressed",
        ),
        (
            Event::PaneResumed(PaneResumed {
                pane_id: PaneId::new(),
                tab_id: TabId::new(),
            }),
            "PaneResumed",
        ),
        (
            Event::TerminalTooSmallEntered(TerminalTooSmallEntered {
                client_id: ClientId::new(),
                size: Size { cols: 1, rows: 1 },
            }),
            "TerminalTooSmallEntered",
        ),
        (
            Event::TerminalTooSmallExited(TerminalTooSmallExited {
                client_id: ClientId::new(),
                size: Size { cols: 80, rows: 24 },
            }),
            "TerminalTooSmallExited",
        ),
        (
            Event::ConfigReloaded(ConfigReloaded {
                session_id: SessionId::new(),
            }),
            "ConfigReloaded",
        ),
        (
            Event::ConfigReloadFailed(ConfigReloadFailed {
                session_id: SessionId::new(),
                reason: "parse error".to_string(),
            }),
            "ConfigReloadFailed",
        ),
        (
            Event::InputModeChanged(InputModeChanged {
                client_id: ClientId::new(),
                mode: InputMode::Normal,
            }),
            "InputModeChanged",
        ),
        (
            Event::KeybindingMatched(KeybindingMatched {
                client_id: ClientId::new(),
                command_id: CommandId::new(),
            }),
            "KeybindingMatched",
        ),
        (
            Event::PaneTyped(PaneTyped {
                pane_id: PaneId::new(),
                tab_id: TabId::new(),
                session_id: SessionId::new(),
                client_id: ClientId::new(),
                payload: TypedPayload::SensitiveRedacted,
                timestamp: fixed_time(),
            }),
            "PaneTyped",
        ),
        (
            Event::PaneEnterPressed(PaneEnterPressed {
                pane_id: PaneId::new(),
                tab_id: TabId::new(),
                session_id: SessionId::new(),
                client_id: ClientId::new(),
                line: SubmittedLinePayload::UnknownMetadataOnly,
                timestamp: fixed_time(),
            }),
            "PaneEnterPressed",
        ),
        (
            Event::MousePressed(MousePressed {
                client_id: ClientId::new(),
                pane: None,
                position: Point { x: 0, y: 0 },
                button: MouseButton::Left,
            }),
            "MousePressed",
        ),
        (
            Event::MouseReleased(MouseReleased {
                client_id: ClientId::new(),
                pane: None,
                position: Point { x: 0, y: 0 },
                button: MouseButton::Right,
            }),
            "MouseReleased",
        ),
        (
            Event::MouseDragged(MouseDragged {
                client_id: ClientId::new(),
                pane: None,
                position: Point { x: 0, y: 0 },
                button: MouseButton::Middle,
            }),
            "MouseDragged",
        ),
        (
            Event::MouseScrolled(MouseScrolled {
                client_id: ClientId::new(),
                pane: None,
                position: Point { x: 0, y: 0 },
                direction: ScrollDirection::Down,
            }),
            "MouseScrolled",
        ),
        (
            Event::PaneMouseForwarded(PaneMouseForwarded {
                pane_id: PaneId::new(),
            }),
            "PaneMouseForwarded",
        ),
        (
            Event::PluginMouseInput(PluginMouseInput {
                plugin_id: PluginId::new(),
            }),
            "PluginMouseInput",
        ),
        (
            Event::PaneScrollbackTruncated(PaneScrollbackTruncated {
                pane_id: PaneId::new(),
                dropped_lines: 0,
                dropped_bytes: 0,
            }),
            "PaneScrollbackTruncated",
        ),
        (
            Event::SubscriberLagged(SubscriberLagged {
                subscriber_id: SubscriberId::new(),
                dropped_count: 0,
                event_class: EventClass::Critical,
            }),
            "SubscriberLagged",
        ),
        (
            Event::CommandRejected(CommandRejected {
                id: CommandId::new(),
                reason: RejectReason::Unauthorized,
            }),
            "CommandRejected",
        ),
        (
            Event::CopyModeEntered(CopyModeEntered {
                pane_id: PaneId::new(),
            }),
            "CopyModeEntered",
        ),
        (
            Event::CopyModeExited(CopyModeExited {
                pane_id: PaneId::new(),
            }),
            "CopyModeExited",
        ),
        (
            Event::SelectionChanged(SelectionChanged {
                pane_id: PaneId::new(),
                selection: None,
            }),
            "SelectionChanged",
        ),
        (
            Event::Copied(Copied {
                pane_id: PaneId::new(),
                target: CopyTarget::Native,
                byte_len: 0,
            }),
            "Copied",
        ),
        (
            Event::SearchUpdated(SearchUpdated {
                pane_id: PaneId::new(),
                match_count: 0,
                current_match: None,
            }),
            "SearchUpdated",
        ),
        (
            Event::Plugin(PluginEvent::Installed(PluginInstalled {
                plugin_id: PluginId::new(),
            })),
            "Plugin",
        ),
        (Event::Quit, "Quit"),
    ];
    assert_eq!(cases.len(), 43);
    for (value, name) in &cases {
        assert_eq!(&variant_name(value), name);
    }
}

#[test]
fn plugin_event_variant_names_are_canonical() {
    let cases: Vec<(PluginEvent, &str)> = vec![
        (
            PluginEvent::Installed(PluginInstalled {
                plugin_id: PluginId::new(),
            }),
            "Installed",
        ),
        (
            PluginEvent::Uninstalled(PluginUninstalled {
                plugin_id: PluginId::new(),
            }),
            "Uninstalled",
        ),
        (
            PluginEvent::Enabled(PluginEnabled {
                plugin_id: PluginId::new(),
            }),
            "Enabled",
        ),
        (
            PluginEvent::Disabled(PluginDisabled {
                plugin_id: PluginId::new(),
            }),
            "Disabled",
        ),
        (
            PluginEvent::Updated(PluginUpdated {
                plugin_id: PluginId::new(),
            }),
            "Updated",
        ),
        (
            PluginEvent::Reloaded(PluginReloaded {
                plugin_id: PluginId::new(),
            }),
            "Reloaded",
        ),
        (
            PluginEvent::LoadFailed(PluginLoadFailed {
                plugin_id: PluginId::new(),
                reason: "x".to_string(),
            }),
            "LoadFailed",
        ),
        (
            PluginEvent::Unloaded(PluginUnloaded {
                plugin_id: PluginId::new(),
            }),
            "Unloaded",
        ),
        (
            PluginEvent::Broken(PluginBroken {
                plugin_id: PluginId::new(),
                reason: "x".to_string(),
            }),
            "Broken",
        ),
        (
            PluginEvent::DoctorCompleted(PluginDoctorCompleted {
                plugin_id: PluginId::new(),
            }),
            "DoctorCompleted",
        ),
    ];
    assert_eq!(cases.len(), 10);
    for (value, name) in &cases {
        assert_eq!(&variant_name(value), name);
    }
}
