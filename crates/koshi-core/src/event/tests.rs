//! Tests for the event vocabulary.
//!
//! Every [`Event`] and [`PluginEvent`] variant survives a JSON round trip
//! unchanged, in the externally tagged shape, and reports its canonical
//! variant name from `Debug`. Every [`Event`] variant also reports that name
//! from [`Event::name`] and maps to its delivery class. Each input payload
//! maps to its privacy tier, and no `SensitiveBlocked` variant holds content.

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

/// A fixed timestamp: `1_700_000_000` seconds after the Unix epoch.
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
        mode: LockMode::Locked,
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

/// Round-trips the variants the named round-trip tests above leave out, with
/// both `Some` and `None` for `PaneFocused::prior_pane`.
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

/// The tier of an input event is its payload variant; there is no separate
/// `tier` field. Each `SensitiveBlocked` variant — on [`PrivacyTier`] and on
/// both input payloads — is unit-shaped: its `Debug` output is the bare name
/// with no `(`.
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

/// The variant name in a value's `Debug` output: the text before the first
/// `(`, or the whole string for a unit variant.
/// `PaneCreated(PaneCreated { .. })` → `"PaneCreated"`; `Quit` → `"Quit"`.
fn variant_name<T: std::fmt::Debug>(value: &T) -> String {
    let repr = format!("{value:?}");
    repr.split('(').next().unwrap_or(&repr).to_string()
}

/// One instance per top-level `Event` variant with its canonical name and
/// delivery class. The array length is the variant count.
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
                mode: LockMode::Normal,
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

/// One instance per [`PluginEvent`] variant with its canonical name. The array
/// length is the variant count.
fn plugin_event_cases() -> [(PluginEvent, &'static str); 10] {
    [
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
    ]
}

#[test]
fn plugin_event_variant_names_are_canonical() {
    let cases = plugin_event_cases();
    assert_eq!(cases.len(), 10);
    for (value, name) in &cases {
        assert_eq!(&variant_name(value), name);
    }
}

#[test]
fn every_plugin_event_survives_a_json_round_trip() {
    for (plugin_event, name) in plugin_event_cases() {
        let event = Event::Plugin(plugin_event);
        let json = serde_json::to_string(&event).expect("serialize");
        let back: Event = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, event, "{name}");
    }
}

#[test]
fn every_event_case_survives_a_json_round_trip() {
    for (event, name, _class) in event_cases() {
        let json = serde_json::to_string(&event).expect("serialize");
        let back: Event = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, event, "{name}");
    }
}

#[test]
fn privacy_tiers_and_input_payloads_roundtrip() {
    for tier in [
        PrivacyTier::Public,
        PrivacyTier::MetadataOnly,
        PrivacyTier::Redacted,
        PrivacyTier::SensitiveBlocked,
    ] {
        roundtrip(&tier);
    }
    for payload in [
        TypedPayload::SafePublic('a'),
        TypedPayload::SafePublic('🦀'),
        TypedPayload::SafePublic('\u{0}'),
        TypedPayload::SensitiveRedacted,
        TypedPayload::AlternateScreenMetadataOnly,
        TypedPayload::RawModeMetadataOnly,
        TypedPayload::UnknownMetadataOnly,
        TypedPayload::SensitiveBlocked,
    ] {
        roundtrip(&payload);
    }
    for line in [
        SubmittedLinePayload::SafePublic(String::new()),
        SubmittedLinePayload::SafePublic("echo \"日本語\" \\ \t \u{1b}[0m 🦀".to_string()),
        SubmittedLinePayload::SensitiveRedacted,
        SubmittedLinePayload::UnknownMetadataOnly,
        SubmittedLinePayload::SensitiveBlocked,
    ] {
        roundtrip(&line);
    }
}

#[test]
fn events_encode_externally_tagged() {
    assert_eq!(
        serde_json::to_string(&Event::Quit).expect("serialize"),
        r#""Quit""#
    );
    assert_eq!(
        serde_json::to_string(&Event::Restarting).expect("serialize"),
        r#""Restarting""#
    );

    let pane_id = PaneId::new();
    let json =
        serde_json::to_string(&Event::PaneClosing(PaneClosing { pane_id })).expect("serialize");
    assert_eq!(
        json,
        format!(r#"{{"PaneClosing":{{"pane_id":"{}"}}}}"#, pane_id.as_uuid())
    );

    assert_eq!(
        serde_json::to_string(&TypedPayload::SafePublic('a')).expect("serialize"),
        r#"{"SafePublic":"a"}"#
    );
    assert_eq!(
        serde_json::to_string(&TypedPayload::SensitiveBlocked).expect("serialize"),
        r#""SensitiveBlocked""#
    );

    let other = ClientId::new();
    assert_eq!(
        serde_json::to_string(&TerminalTooSmallCause::OtherClient(other)).expect("serialize"),
        format!(r#"{{"OtherClient":"{}"}}"#, other.as_uuid())
    );
}

#[test]
fn too_small_cause_defaults_to_terminal() {
    assert_eq!(
        TerminalTooSmallCause::default(),
        TerminalTooSmallCause::Terminal
    );
}

#[test]
fn a_too_small_event_reads_an_explicit_null_pane_area_as_none() {
    let client_id = ClientId::new();
    let json = serde_json::json!({
        "client_id": client_id,
        "size": { "cols": 80, "rows": 24 },
        "pane_area": null,
        "cause": "Regions"
    });

    let event: TerminalTooSmallEntered = serde_json::from_value(json).expect("deserialize");
    assert_eq!(event.client_id, client_id);
    assert_eq!(event.size, Size { cols: 80, rows: 24 });
    assert_eq!(event.pane_area, None);
    assert_eq!(event.cause, TerminalTooSmallCause::Regions);
}

#[test]
fn a_timestamp_before_the_unix_epoch_cannot_be_serialized() {
    let event = Event::PaneTyped(PaneTyped {
        pane_id: PaneId::new(),
        tab_id: TabId::new(),
        session_id: SessionId::new(),
        client_id: ClientId::new(),
        payload: TypedPayload::SensitiveBlocked,
        timestamp: UNIX_EPOCH - Duration::from_secs(1),
    });

    let err = serde_json::to_string(&event).expect_err("pre-epoch timestamp");
    assert_eq!(err.to_string(), "SystemTime must be later than UNIX_EPOCH");
}
