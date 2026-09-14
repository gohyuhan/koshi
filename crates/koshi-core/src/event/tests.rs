//! Tests for the event vocabulary.
//!
//! Every [`Event`] and [`PluginEvent`] variant survives a JSON round trip
//! unchanged, in the externally tagged shape, and reports its canonical
//! variant name from `Debug`. Every [`Event`] variant also reports that name
//! from [`Event::get_event_name`] and maps to its delivery class. Each input payload
//! maps to its privacy tier, and no `SensitiveBlocked` variant holds content.

use super::*;
use crate::command::{GridPosition, SelectionKind};
use crate::geometry::{PaneArea, Point, Size};
use crate::ids::{ClientId, CommandId, PaneId, PluginId, SessionId, SubscriberId, TabId};
use crate::process::PtySize;
use std::time::{Duration, UNIX_EPOCH};

/// Roundtrip a value through JSON and assert it survives unchanged.
fn assert_json_roundtrip<Roundtrippable>(roundtrippable_value: &Roundtrippable)
where
    Roundtrippable: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let serialized_json = serde_json::to_string(roundtrippable_value).expect("serialize");
    let decoded_roundtrippable_value: Roundtrippable =
        serde_json::from_str(&serialized_json).expect("deserialize");
    assert_eq!(*roundtrippable_value, decoded_roundtrippable_value);
}

/// A fixed timestamp: `1_700_000_000` seconds after the Unix epoch.
fn build_fixed_timestamp() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

#[test]
fn lifecycle_events_roundtrip() {
    assert_json_roundtrip(&Event::PaneCreated(PaneCreated {
        pane_id: PaneId::new(),
        tab_id: TabId::new(),
    }));
    assert_json_roundtrip(&Event::PaneProcessExited(PaneProcessExited {
        pane_id: PaneId::new(),
        exit_code: Some(0),
    }));
    assert_json_roundtrip(&Event::PaneRemoved(PaneRemoved {
        pane_id: PaneId::new(),
        tab_id: TabId::new(),
    }));
    assert_json_roundtrip(&Event::PtyResized(PtyResized {
        pane_id: PaneId::new(),
        pty_size: PtySize {
            column_count: 80,
            row_count: 24,
        },
    }));
    assert_json_roundtrip(&Event::PaneOutputUpdated(PaneOutputUpdated {
        pane_id: PaneId::new(),
    }));
    assert_json_roundtrip(&Event::PaneCommandStarted(PaneCommandStarted {
        pane_id: PaneId::new(),
    }));
    assert_json_roundtrip(&Event::PaneCommandFinished(PaneCommandFinished {
        pane_id: PaneId::new(),
        exit_code: Some(1),
    }));
    assert_json_roundtrip(&Event::InputModeChanged(InputModeChanged {
        client_id: ClientId::new(),
        lock_mode: LockMode::Locked,
    }));
    assert_json_roundtrip(&Event::MouseSelectChanged(MouseSelectChanged {
        client_id: ClientId::new(),
        is_enabled: true,
    }));
}

#[test]
fn move_suppression_and_reload_events_roundtrip() {
    assert_json_roundtrip(&Event::TabMoved(TabMoved {
        tab_id: TabId::new(),
        previous_tab_index: 0,
        new_tab_index: 2,
    }));
    assert_json_roundtrip(&Event::PaneSuppressed(PaneSuppressed {
        pane_id: PaneId::new(),
        tab_id: TabId::new(),
    }));
    assert_json_roundtrip(&Event::PaneResumed(PaneResumed {
        pane_id: PaneId::new(),
        tab_id: TabId::new(),
    }));
    assert_json_roundtrip(&Event::TerminalTooSmallEntered(TerminalTooSmallEntered {
        client_id: ClientId::new(),
        viewport_size: Size {
            column_count: 1,
            row_count: 1,
        },
        pane_area: Some(PaneArea::Reported(Size {
            column_count: 1,
            row_count: 0,
        })),
        cause: TerminalTooSmallCause::Terminal,
    }));
    assert_json_roundtrip(&Event::TerminalTooSmallExited(TerminalTooSmallExited {
        client_id: ClientId::new(),
        viewport_size: Size {
            column_count: 80,
            row_count: 24,
        },
    }));
    assert_json_roundtrip(&Event::ConfigReloaded(ConfigReloaded {
        session_id: SessionId::new(),
    }));
}

#[test]
fn too_small_causes_roundtrip() {
    let other_client_id = ClientId::new();
    for cause in [
        TerminalTooSmallCause::Terminal,
        TerminalTooSmallCause::Regions,
        TerminalTooSmallCause::OtherClient(other_client_id),
    ] {
        assert_json_roundtrip(&cause);
    }
}

#[test]
fn an_old_too_small_event_defaults_new_fields() {
    let client_id = ClientId::new();
    let old_event_json = serde_json::json!({
        "client_id": client_id,
        "size": { "cols": 80, "rows": 24 }
    });

    let entered_event: TerminalTooSmallEntered =
        serde_json::from_value(old_event_json).expect("the old event shape remains readable");
    assert_eq!(entered_event.client_id, client_id);
    assert_eq!(
        entered_event.viewport_size,
        Size {
            column_count: 80,
            row_count: 24,
        }
    );
    assert_eq!(entered_event.pane_area, None);
    assert_eq!(entered_event.cause, TerminalTooSmallCause::Terminal);
}

#[test]
fn input_privacy_events_roundtrip() {
    assert_json_roundtrip(&Event::PaneTyped(PaneTyped {
        pane_id: PaneId::new(),
        tab_id: TabId::new(),
        session_id: SessionId::new(),
        client_id: ClientId::new(),
        typed_payload: TypedPayload::SafePublic('a'),
        accepted_at: build_fixed_timestamp(),
    }));
    assert_json_roundtrip(&Event::PaneEnterPressed(PaneEnterPressed {
        pane_id: PaneId::new(),
        tab_id: TabId::new(),
        session_id: SessionId::new(),
        client_id: ClientId::new(),
        submitted_line: SubmittedLinePayload::SensitiveRedacted,
        accepted_at: build_fixed_timestamp(),
    }));
}

#[test]
fn mouse_events_roundtrip() {
    assert_json_roundtrip(&Event::MousePressed(MousePressed {
        client_id: ClientId::new(),
        pane_id: Some(PaneId::new()),
        position: Point { column: 4, row: 9 },
        button: MouseButton::Left,
    }));
    assert_json_roundtrip(&Event::MouseScrolled(MouseScrolled {
        client_id: ClientId::new(),
        pane_id: None,
        position: Point { column: 0, row: 0 },
        direction: ScrollDirection::Up,
    }));
    assert_json_roundtrip(&Event::MouseScrolled(MouseScrolled {
        client_id: ClientId::new(),
        pane_id: None,
        position: Point { column: 0, row: 0 },
        direction: ScrollDirection::Left,
    }));
    assert_json_roundtrip(&Event::PluginMouseInput(PluginMouseInput {
        plugin_id: PluginId::new(),
    }));
}

#[test]
fn delivery_and_rejection_events_roundtrip() {
    assert_json_roundtrip(&Event::SubscriberLagged(SubscriberLagged {
        subscriber_id: SubscriberId::new(),
        dropped_event_count: 12,
        event_class: EventClass::Lossy,
    }));
    assert_json_roundtrip(&Event::PaneScrollbackTruncated(PaneScrollbackTruncated {
        pane_id: PaneId::new(),
        dropped_lines: 500,
        dropped_bytes: 8192,
    }));
    assert_json_roundtrip(&Event::CommandRejected(CommandRejected {
        command_id: CommandId::new(),
        rejection_reason: RejectReason::TargetGone,
    }));
}

#[test]
fn selection_and_copy_events_roundtrip() {
    assert_json_roundtrip(&Event::SelectionChanged(SelectionChanged {
        client_id: ClientId::new(),
        pane_id: PaneId::new(),
        selection: Some(Selection {
            selection_kind: SelectionKind::Block,
            anchor: GridPosition {
                row_index: 1,
                column_index: 0,
            },
            cursor: GridPosition {
                row_index: 3,
                column_index: 20,
            },
        }),
    }));
    assert_json_roundtrip(&Event::SelectionChanged(SelectionChanged {
        client_id: ClientId::new(),
        pane_id: PaneId::new(),
        selection: None,
    }));
    assert_json_roundtrip(&Event::Copied(Copied {
        client_id: ClientId::new(),
        pane_id: PaneId::new(),
        clipboard_target: CopyTarget::Osc52,
        byte_count: 42,
    }));
}

#[test]
fn plugin_events_roundtrip() {
    assert_json_roundtrip(&Event::Plugin(PluginEvent::Installed(PluginInstalled {
        plugin_id: PluginId::new(),
    })));
    assert_json_roundtrip(&Event::Plugin(PluginEvent::LoadFailed(PluginLoadFailed {
        plugin_id: PluginId::new(),
        failure_reason: "missing export".to_string(),
    })));
}

/// Round-trips the variants the named round-trip tests above leave out, with
/// both `Some` and `None` for `PaneFocused::previous_pane_id`.
#[test]
fn remaining_event_variants_survive_a_json_round_trip() {
    assert_json_roundtrip(&Event::PaneClosing(PaneClosing {
        pane_id: PaneId::new(),
    }));
    assert_json_roundtrip(&Event::PaneFocused(PaneFocused {
        client_id: ClientId::new(),
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
        previous_pane_id: Some(PaneId::new()),
    }));
    assert_json_roundtrip(&Event::PaneFocused(PaneFocused {
        client_id: ClientId::new(),
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
        previous_pane_id: None,
    }));
    assert_json_roundtrip(&Event::LayoutChanged(LayoutChanged {
        tab_id: TabId::new(),
    }));
    assert_json_roundtrip(&Event::TabCreated(TabCreated {
        tab_id: TabId::new(),
    }));
    assert_json_roundtrip(&Event::TabClosed(TabClosed {
        tab_id: TabId::new(),
    }));
    assert_json_roundtrip(&Event::TabFocused(TabFocused {
        client_id: ClientId::new(),
        tab_id: TabId::new(),
        previous_tab_id: TabId::new(),
    }));
    assert_json_roundtrip(&Event::KeybindingMatched(KeybindingMatched {
        client_id: ClientId::new(),
        command_id: CommandId::new(),
    }));
    assert_json_roundtrip(&Event::MouseReleased(MouseReleased {
        client_id: ClientId::new(),
        pane_id: Some(PaneId::new()),
        position: Point { column: 1, row: 2 },
        button: MouseButton::Right,
    }));
    assert_json_roundtrip(&Event::MouseDragged(MouseDragged {
        client_id: ClientId::new(),
        pane_id: None,
        position: Point { column: 0, row: 0 },
        button: MouseButton::Middle,
    }));
    assert_json_roundtrip(&Event::PaneMouseForwarded(PaneMouseForwarded {
        pane_id: PaneId::new(),
    }));
    assert_json_roundtrip(&Event::Quit);
    assert_json_roundtrip(&Event::Restarting);
}

/// The tier of an input event is its payload variant; there is no separate
/// `tier` field. Each `SensitiveBlocked` variant — on [`PrivacyTier`] and on
/// both input payloads — is unit-shaped: its `Debug` output is the bare name
/// with no `(`.
#[test]
fn sensitive_blocked_tier_carries_no_content() {
    let blocked_debug_representations = [
        format!("{:?}", PrivacyTier::SensitiveBlocked),
        format!("{:?}", TypedPayload::SensitiveBlocked),
        format!("{:?}", SubmittedLinePayload::SensitiveBlocked),
    ];
    for debug_representation in &blocked_debug_representations {
        assert_eq!(debug_representation, "SensitiveBlocked");
        assert!(
            !debug_representation.contains('('),
            "{debug_representation} must hold no payload"
        );
    }
}

/// The `get_privacy_tier()` accessor maps each payload variant to its privacy tier, and
/// `Unknown` lines fail closed to `MetadataOnly`.
#[test]
fn payload_tier_accessors_map_to_privacy_tier() {
    assert_eq!(
        TypedPayload::SafePublic('x').get_privacy_tier(),
        PrivacyTier::Public
    );
    assert_eq!(
        TypedPayload::SensitiveRedacted.get_privacy_tier(),
        PrivacyTier::Redacted
    );
    assert_eq!(
        TypedPayload::AlternateScreenMetadataOnly.get_privacy_tier(),
        PrivacyTier::MetadataOnly
    );
    assert_eq!(
        TypedPayload::RawModeMetadataOnly.get_privacy_tier(),
        PrivacyTier::MetadataOnly
    );
    assert_eq!(
        TypedPayload::UnknownMetadataOnly.get_privacy_tier(),
        PrivacyTier::MetadataOnly
    );
    assert_eq!(
        TypedPayload::SensitiveBlocked.get_privacy_tier(),
        PrivacyTier::SensitiveBlocked
    );

    assert_eq!(
        SubmittedLinePayload::SafePublic("ls".to_string()).get_privacy_tier(),
        PrivacyTier::Public
    );
    assert_eq!(
        SubmittedLinePayload::SensitiveRedacted.get_privacy_tier(),
        PrivacyTier::Redacted
    );
    assert_eq!(
        SubmittedLinePayload::UnknownMetadataOnly.get_privacy_tier(),
        PrivacyTier::MetadataOnly
    );
    assert_eq!(
        SubmittedLinePayload::SensitiveBlocked.get_privacy_tier(),
        PrivacyTier::SensitiveBlocked
    );
}

/// The variant name in a value's `Debug` output: the text before the first
/// `(`, or the whole string for a unit variant.
/// `PaneCreated(PaneCreated { .. })` → `"PaneCreated"`; `Quit` → `"Quit"`.
fn get_variant_name<DebugValue: std::fmt::Debug>(debug_value: &DebugValue) -> String {
    let debug_text = format!("{debug_value:?}");
    debug_text
        .split('(')
        .next()
        .unwrap_or(&debug_text)
        .to_string()
}

/// One instance per top-level `Event` variant with its canonical name and
/// delivery class. The array length is the variant count.
pub(crate) fn list_event_cases() -> [(Event, &'static str, EventClass); 38] {
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
                previous_pane_id: None,
            }),
            "PaneFocused",
            EventClass::Critical,
        ),
        (
            Event::PtyResized(PtyResized {
                pane_id: PaneId::new(),
                pty_size: PtySize {
                    column_count: 80,
                    row_count: 24,
                },
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
                previous_tab_id: TabId::new(),
            }),
            "TabFocused",
            EventClass::Critical,
        ),
        (
            Event::TabMoved(TabMoved {
                tab_id: TabId::new(),
                previous_tab_index: 0,
                new_tab_index: 1,
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
                viewport_size: Size {
                    column_count: 1,
                    row_count: 1,
                },
                pane_area: Some(PaneArea::Starving),
                cause: TerminalTooSmallCause::Regions,
            }),
            "TerminalTooSmallEntered",
            EventClass::Critical,
        ),
        (
            Event::TerminalTooSmallExited(TerminalTooSmallExited {
                client_id: ClientId::new(),
                viewport_size: Size {
                    column_count: 80,
                    row_count: 24,
                },
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
                lock_mode: LockMode::Normal,
            }),
            "InputModeChanged",
            EventClass::Critical,
        ),
        (
            Event::MouseSelectChanged(MouseSelectChanged {
                client_id: ClientId::new(),
                is_enabled: true,
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
                typed_payload: TypedPayload::SensitiveRedacted,
                accepted_at: build_fixed_timestamp(),
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
                submitted_line: SubmittedLinePayload::UnknownMetadataOnly,
                accepted_at: build_fixed_timestamp(),
            }),
            "PaneEnterPressed",
            EventClass::Critical,
        ),
        (
            Event::MousePressed(MousePressed {
                client_id: ClientId::new(),
                pane_id: None,
                position: Point { column: 0, row: 0 },
                button: MouseButton::Left,
            }),
            "MousePressed",
            EventClass::Critical,
        ),
        (
            Event::MouseReleased(MouseReleased {
                client_id: ClientId::new(),
                pane_id: None,
                position: Point { column: 0, row: 0 },
                button: MouseButton::Right,
            }),
            "MouseReleased",
            EventClass::Critical,
        ),
        (
            Event::MouseDragged(MouseDragged {
                client_id: ClientId::new(),
                pane_id: None,
                position: Point { column: 0, row: 0 },
                button: MouseButton::Middle,
            }),
            "MouseDragged",
            EventClass::Lossy,
        ),
        (
            Event::MouseScrolled(MouseScrolled {
                client_id: ClientId::new(),
                pane_id: None,
                position: Point { column: 0, row: 0 },
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
                dropped_event_count: 0,
                event_class: EventClass::Critical,
            }),
            "SubscriberLagged",
            EventClass::Critical,
        ),
        (
            Event::CommandRejected(CommandRejected {
                command_id: CommandId::new(),
                rejection_reason: RejectReason::Unauthorized,
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
                clipboard_target: CopyTarget::Native,
                byte_count: 0,
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

/// Checks 38 distinct top-level event names against `Debug` and
/// [`Event::get_event_name`].
#[test]
fn event_variant_names_are_canonical() {
    let event_cases = list_event_cases();
    let mut event_names = std::collections::BTreeSet::new();
    assert_eq!(event_cases.len(), 38);
    for (event, event_name, _) in event_cases {
        assert_eq!(get_variant_name(&event), event_name);
        assert_eq!(event.get_event_name(), event_name);
        assert!(
            event_names.insert(event_name),
            "duplicate event name: {event_name}"
        );
    }
    assert_eq!(event_names.len(), 38);
}

#[test]
fn classify_maps_every_event_variant() {
    let mut lossy_event_count = 0;
    let mut critical_event_count = 0;

    for (event, event_name, expected_event_class) in list_event_cases() {
        let actual_class = classify_event(&event);
        assert_eq!(actual_class, expected_event_class, "{event_name}");
        match actual_class {
            EventClass::Lossy => lossy_event_count += 1,
            EventClass::Critical => critical_event_count += 1,
        }
    }

    assert_eq!(lossy_event_count, 7);
    assert_eq!(critical_event_count, 31);
}

/// One instance per [`PluginEvent`] variant with its canonical name. The array
/// length is the variant count.
fn list_plugin_event_cases() -> [(PluginEvent, &'static str); 10] {
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
                failure_reason: "x".to_string(),
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
                failure_reason: "x".to_string(),
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
    let plugin_event_cases = list_plugin_event_cases();
    assert_eq!(plugin_event_cases.len(), 10);
    for (plugin_event, event_name) in &plugin_event_cases {
        assert_eq!(&get_variant_name(plugin_event), event_name);
    }
}

#[test]
fn every_plugin_event_survives_a_json_round_trip() {
    for (plugin_event, event_name) in list_plugin_event_cases() {
        let event = Event::Plugin(plugin_event);
        let event_json = serde_json::to_string(&event).expect("serialize");
        let decoded_event: Event = serde_json::from_str(&event_json).expect("deserialize");
        assert_eq!(decoded_event, event, "{event_name}");
    }
}

#[test]
fn every_event_case_survives_a_json_round_trip() {
    for (event, event_name, _event_class) in list_event_cases() {
        let event_json = serde_json::to_string(&event).expect("serialize");
        let decoded_event: Event = serde_json::from_str(&event_json).expect("deserialize");
        assert_eq!(decoded_event, event, "{event_name}");
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
        assert_json_roundtrip(&tier);
    }
    for typed_payload in [
        TypedPayload::SafePublic('a'),
        TypedPayload::SafePublic('🦀'),
        TypedPayload::SafePublic('\u{0}'),
        TypedPayload::SensitiveRedacted,
        TypedPayload::AlternateScreenMetadataOnly,
        TypedPayload::RawModeMetadataOnly,
        TypedPayload::UnknownMetadataOnly,
        TypedPayload::SensitiveBlocked,
    ] {
        assert_json_roundtrip(&typed_payload);
    }
    for submitted_line_payload in [
        SubmittedLinePayload::SafePublic(String::new()),
        SubmittedLinePayload::SafePublic("echo \"日本語\" \\ \t \u{1b}[0m 🦀".to_string()),
        SubmittedLinePayload::SensitiveRedacted,
        SubmittedLinePayload::UnknownMetadataOnly,
        SubmittedLinePayload::SensitiveBlocked,
    ] {
        assert_json_roundtrip(&submitted_line_payload);
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
    let event_json =
        serde_json::to_string(&Event::PaneClosing(PaneClosing { pane_id })).expect("serialize");
    assert_eq!(
        event_json,
        format!(
            r#"{{"PaneClosing":{{"pane_id":"{}"}}}}"#,
            pane_id.get_uuid()
        )
    );

    assert_eq!(
        serde_json::to_string(&TypedPayload::SafePublic('a')).expect("serialize"),
        r#"{"SafePublic":"a"}"#
    );
    assert_eq!(
        serde_json::to_string(&TypedPayload::SensitiveBlocked).expect("serialize"),
        r#""SensitiveBlocked""#
    );

    let other_client_id = ClientId::new();
    assert_eq!(
        serde_json::to_string(&TerminalTooSmallCause::OtherClient(other_client_id))
            .expect("serialize"),
        format!(r#"{{"OtherClient":"{}"}}"#, other_client_id.get_uuid())
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
    let too_small_event_json = serde_json::json!({
        "client_id": client_id,
        "size": { "cols": 80, "rows": 24 },
        "pane_area": null,
        "cause": "Regions"
    });

    let event: TerminalTooSmallEntered =
        serde_json::from_value(too_small_event_json).expect("deserialize");
    assert_eq!(event.client_id, client_id);
    assert_eq!(
        event.viewport_size,
        Size {
            column_count: 80,
            row_count: 24,
        }
    );
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
        typed_payload: TypedPayload::SensitiveBlocked,
        accepted_at: UNIX_EPOCH - Duration::from_secs(1),
    });

    let serialization_error = serde_json::to_string(&event).expect_err("pre-epoch timestamp");
    assert_eq!(
        serialization_error.to_string(),
        "SystemTime must be later than UNIX_EPOCH"
    );
}
