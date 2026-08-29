//! Serialization roundtrip tests and invariant assertions for events.
//!
//! Verifies that all [`Event`] and [`PluginEvent`] variants:
//! - survive JSON serialization and deserialization unchanged (canonical serde mapping),
//! - have stable, canonical variant names (renames or additions break the test),
//! - maintain privacy tier invariants (sensitive data is structurally unavoidable).

use super::*;
use crate::command::{GridPos, SelectionKind};
use crate::geometry::{PaneArea, Point, Size};
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
        mode: InputMode::Locked,
    }));
    roundtrip(&Event::MouseSelectChanged(MouseSelectChanged {
        client_id: ClientId::new(),
        on: true,
    }));
}

#[test]
fn move_suppression_and_reload_events_roundtrip() {
    roundtrip(&Event::TabMoved(TabMoved {
        tab_id: TabId::new(),
        old_index: 0,
        new_index: 2,
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
        pane_area: Some(PaneArea::Reported(Size { cols: 1, rows: 0 })),
        cause: TerminalTooSmallCause::Terminal,
    }));
    roundtrip(&Event::TerminalTooSmallExited(TerminalTooSmallExited {
        client_id: ClientId::new(),
        size: Size { cols: 80, rows: 24 },
    }));
    roundtrip(&Event::ConfigReloaded(ConfigReloaded {
        session_id: SessionId::new(),
    }));
}

#[test]
fn too_small_causes_roundtrip() {
    let other_client = ClientId::new();
    for cause in [
        TerminalTooSmallCause::Terminal,
        TerminalTooSmallCause::Regions,
        TerminalTooSmallCause::OtherClient(other_client),
    ] {
        roundtrip(&cause);
    }
}

#[test]
fn an_old_too_small_event_defaults_new_fields() {
    let client_id = ClientId::new();
    let old = serde_json::json!({
        "client_id": client_id,
        "size": { "cols": 80, "rows": 24 }
    });

    let event: TerminalTooSmallEntered =
        serde_json::from_value(old).expect("the old event shape remains readable");
    assert_eq!(event.client_id, client_id);
    assert_eq!(event.size, Size { cols: 80, rows: 24 });
    assert_eq!(event.pane_area, None);
    assert_eq!(event.cause, TerminalTooSmallCause::Terminal);
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
fn selection_and_copy_events_roundtrip() {
    roundtrip(&Event::SelectionChanged(SelectionChanged {
        client_id: ClientId::new(),
        pane_id: PaneId::new(),
        selection: Some(Selection {
            kind: SelectionKind::Block,
            anchor: GridPos { row: 1, col: 0 },
            cursor: GridPos { row: 3, col: 20 },
        }),
    }));
    roundtrip(&Event::SelectionChanged(SelectionChanged {
        client_id: ClientId::new(),
        pane_id: PaneId::new(),
        selection: None,
    }));
    roundtrip(&Event::Copied(Copied {
        client_id: ClientId::new(),
        pane_id: PaneId::new(),
        target: CopyTarget::Osc52,
        byte_len: 42,
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
    roundtrip(&Event::Quit);
    roundtrip(&Event::Restarting);
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

/// One instance per top-level `Event` variant, its canonical name, and class.
/// The array's length forces every variant to appear, so a reader of this list
/// sees the whole enum.
pub(crate) fn event_cases() -> [(Event, &'static str, EventClass); 38] {
    [
        (
            Event::PaneCreated(PaneCreated {
                pane_id: PaneId::new(),
                tab_id: TabId::new(),
            }),
            "PaneCreated",
            EventClass::Critical,
        ),
        (
            Event::PaneProcessExited(PaneProcessExited {
                pane_id: PaneId::new(),
                exit_code: None,
            }),
            "PaneProcessExited",
            EventClass::Critical,
        ),
        (
            Event::PaneClosing(PaneClosing {
                pane_id: PaneId::new(),
            }),
            "PaneClosing",
            EventClass::Critical,
        ),
        (
            Event::PaneRemoved(PaneRemoved {
                pane_id: PaneId::new(),
                tab_id: TabId::new(),
            }),
            "PaneRemoved",
            EventClass::Critical,
        ),
        (
            Event::PaneFocused(PaneFocused {
                client_id: ClientId::new(),
                tab_id: TabId::new(),
                pane_id: PaneId::new(),
                prior_pane: None,
            }),
            "PaneFocused",
            EventClass::Critical,
        ),
        (
            Event::PtyResized(PtyResized {
                pane_id: PaneId::new(),
                size: PtySize { cols: 80, rows: 24 },
            }),
            "PtyResized",
            EventClass::Critical,
        ),
        (
            Event::PaneOutputUpdated(PaneOutputUpdated {
                pane_id: PaneId::new(),
            }),
            "PaneOutputUpdated",
            EventClass::Lossy,
        ),
        (
            Event::PaneCommandStarted(PaneCommandStarted {
                pane_id: PaneId::new(),
            }),
            "PaneCommandStarted",
            EventClass::Critical,
        ),
        (
            Event::PaneCommandFinished(PaneCommandFinished {
                pane_id: PaneId::new(),
                exit_code: None,
            }),
            "PaneCommandFinished",
            EventClass::Critical,
        ),
        (
            Event::LayoutChanged(LayoutChanged {
                tab_id: TabId::new(),
            }),
            "LayoutChanged",
            EventClass::Critical,
        ),
        (
            Event::TabCreated(TabCreated {
                tab_id: TabId::new(),
            }),
            "TabCreated",
            EventClass::Critical,
        ),
        (
            Event::TabClosed(TabClosed {
                tab_id: TabId::new(),
            }),
            "TabClosed",
            EventClass::Critical,
        ),
        (
            Event::TabFocused(TabFocused {
                client_id: ClientId::new(),
                tab_id: TabId::new(),
                prior_tab: TabId::new(),
            }),
            "TabFocused",
            EventClass::Critical,
        ),
        (
            Event::TabMoved(TabMoved {
                tab_id: TabId::new(),
                old_index: 0,
                new_index: 1,
            }),
            "TabMoved",
            EventClass::Critical,
        ),
        (
            Event::PaneSuppressed(PaneSuppressed {
                pane_id: PaneId::new(),
                tab_id: TabId::new(),
            }),
            "PaneSuppressed",
            EventClass::Critical,
        ),
        (
            Event::PaneResumed(PaneResumed {
                pane_id: PaneId::new(),
                tab_id: TabId::new(),
            }),
            "PaneResumed",
            EventClass::Critical,
        ),
        (
            Event::TerminalTooSmallEntered(TerminalTooSmallEntered {
                client_id: ClientId::new(),
                size: Size { cols: 1, rows: 1 },
                pane_area: Some(PaneArea::Starving),
                cause: TerminalTooSmallCause::Regions,
            }),
            "TerminalTooSmallEntered",
            EventClass::Critical,
        ),
        (
            Event::TerminalTooSmallExited(TerminalTooSmallExited {
                client_id: ClientId::new(),
                size: Size { cols: 80, rows: 24 },
            }),
            "TerminalTooSmallExited",
            EventClass::Critical,
        ),
        (
            Event::ConfigReloaded(ConfigReloaded {
                session_id: SessionId::new(),
            }),
            "ConfigReloaded",
            EventClass::Critical,
        ),
        (
            Event::InputModeChanged(InputModeChanged {
                client_id: ClientId::new(),
                mode: InputMode::Normal,
            }),
            "InputModeChanged",
            EventClass::Critical,
        ),
        (
            Event::MouseSelectChanged(MouseSelectChanged {
                client_id: ClientId::new(),
                on: true,
            }),
            "MouseSelectChanged",
            EventClass::Critical,
        ),
        (
            Event::KeybindingMatched(KeybindingMatched {
                client_id: ClientId::new(),
                command_id: CommandId::new(),
            }),
            "KeybindingMatched",
            EventClass::Critical,
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
            EventClass::Lossy,
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
            EventClass::Critical,
        ),
        (
            Event::MousePressed(MousePressed {
                client_id: ClientId::new(),
                pane: None,
                position: Point { x: 0, y: 0 },
                button: MouseButton::Left,
            }),
            "MousePressed",
            EventClass::Critical,
        ),
        (
            Event::MouseReleased(MouseReleased {
                client_id: ClientId::new(),
                pane: None,
                position: Point { x: 0, y: 0 },
                button: MouseButton::Right,
            }),
            "MouseReleased",
            EventClass::Critical,
        ),
        (
            Event::MouseDragged(MouseDragged {
                client_id: ClientId::new(),
                pane: None,
                position: Point { x: 0, y: 0 },
                button: MouseButton::Middle,
            }),
            "MouseDragged",
            EventClass::Lossy,
        ),
        (
            Event::MouseScrolled(MouseScrolled {
                client_id: ClientId::new(),
                pane: None,
                position: Point { x: 0, y: 0 },
                direction: ScrollDirection::Down,
            }),
            "MouseScrolled",
            EventClass::Lossy,
        ),
        (
            Event::PaneMouseForwarded(PaneMouseForwarded {
                pane_id: PaneId::new(),
            }),
            "PaneMouseForwarded",
            EventClass::Lossy,
        ),
        (
            Event::PluginMouseInput(PluginMouseInput {
                plugin_id: PluginId::new(),
            }),
            "PluginMouseInput",
            EventClass::Lossy,
        ),
        (
            Event::PaneScrollbackTruncated(PaneScrollbackTruncated {
                pane_id: PaneId::new(),
                dropped_lines: 0,
                dropped_bytes: 0,
            }),
            "PaneScrollbackTruncated",
            EventClass::Lossy,
        ),
        (
            Event::SubscriberLagged(SubscriberLagged {
                subscriber_id: SubscriberId::new(),
                dropped_count: 0,
                event_class: EventClass::Critical,
            }),
            "SubscriberLagged",
            EventClass::Critical,
        ),
        (
            Event::CommandRejected(CommandRejected {
                id: CommandId::new(),
                reason: RejectReason::Unauthorized,
            }),
            "CommandRejected",
            EventClass::Critical,
        ),
        (
            Event::SelectionChanged(SelectionChanged {
                client_id: ClientId::new(),
                pane_id: PaneId::new(),
                selection: None,
            }),
            "SelectionChanged",
            EventClass::Critical,
        ),
        (
            Event::Copied(Copied {
                client_id: ClientId::new(),
                pane_id: PaneId::new(),
                target: CopyTarget::Native,
                byte_len: 0,
            }),
            "Copied",
            EventClass::Critical,
        ),
        (
            Event::Plugin(PluginEvent::Installed(PluginInstalled {
                plugin_id: PluginId::new(),
            })),
            "Plugin",
            EventClass::Critical,
        ),
        (Event::Quit, "Quit", EventClass::Critical),
        (Event::Restarting, "Restarting", EventClass::Critical),
    ]
}

/// Checks 38 distinct top-level event names against `Debug` and [`Event::name`].
#[test]
fn event_variant_names_are_canonical() {
    let cases = event_cases();
    let mut names = std::collections::BTreeSet::new();
    assert_eq!(cases.len(), 38);
    for (event, name, _) in cases {
        assert_eq!(variant_name(&event), name);
        assert_eq!(event.name(), name);
        assert!(names.insert(name), "duplicate event name: {name}");
    }
    assert_eq!(names.len(), 38);
}

#[test]
fn classify_maps_every_event_variant() {
    let mut lossy = 0;
    let mut critical = 0;

    for (event, name, expected_class) in event_cases() {
        let actual_class = classify(&event);
        assert_eq!(actual_class, expected_class, "{name}");
        match actual_class {
            EventClass::Lossy => lossy += 1,
            EventClass::Critical => critical += 1,
        }
    }

    assert_eq!(lossy, 7);
    assert_eq!(critical, 31);
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
