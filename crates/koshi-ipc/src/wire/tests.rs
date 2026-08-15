//! Tests for reading a message whose variant this build may not have.

use koshi_core::ids::PaneId;
use serde::Serialize;

use super::*;
use crate::event::SessionEvent;
use crate::protocol::{ConnectionToken, IpcRequestKind, IpcResult};
use crate::router::{RouterRequestKind, RouterResult};
use crate::supervisor::{SupervisorEvent, SupervisorRequestKind, SupervisorResult};

/// A stand-in for a build that has fewer variants than its peer: it knows
/// `Keep` and `Bare`, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Sample {
    Keep { value: u32 },
    Bare,
}

impl WireVariants for Sample {
    const VARIANTS: &'static [&'static str] = &["Keep", "Bare"];
}

impl WireName for Sample {
    fn wire_name(&self) -> &'static str {
        match self {
            Sample::Keep { .. } => "Keep",
            Sample::Bare => "Bare",
        }
    }
}

#[test]
fn a_variant_this_build_has_decodes_as_itself() {
    let decoded: MaybeKnown<Sample> = serde_json::from_str(r#"{"Keep":{"value":7}}"#).unwrap();
    assert_eq!(decoded, MaybeKnown::Known(Sample::Keep { value: 7 }));
}

#[test]
fn a_variant_with_no_fields_decodes_from_its_bare_name() {
    let decoded: MaybeKnown<Sample> = serde_json::from_str(r#""Bare""#).unwrap();
    assert_eq!(decoded, MaybeKnown::Known(Sample::Bare));
}

#[test]
fn a_variant_this_build_lacks_decodes_as_unknown_and_keeps_its_name() {
    let decoded: MaybeKnown<Sample> = serde_json::from_str(r#"{"Added":{"pane":3}}"#).unwrap();
    assert_eq!(
        decoded,
        MaybeKnown::Unknown {
            name: "Added".to_string()
        }
    );
}

#[test]
fn a_variant_this_build_lacks_and_that_carries_no_fields_decodes_as_unknown() {
    let decoded: MaybeKnown<Sample> = serde_json::from_str(r#""Added""#).unwrap();
    assert_eq!(
        decoded,
        MaybeKnown::Unknown {
            name: "Added".to_string()
        }
    );
}

#[test]
fn a_variant_this_build_has_but_cannot_read_is_an_error_not_an_unknown() {
    let decoded: Result<MaybeKnown<Sample>, _> = serde_json::from_str(r#"{"Keep":{"value":"x"}}"#);
    let error = decoded.expect_err("a known variant with an unreadable payload is an error");
    assert!(
        error.to_string().contains("invalid type"),
        "the error names the payload fault, got: {error}"
    );
}

#[test]
fn a_value_that_names_no_variant_is_an_error() {
    for text in [r#"{"Keep":1,"Bare":2}"#, "7", "[]", "null"] {
        let decoded: Result<MaybeKnown<Sample>, _> = serde_json::from_str(text);
        assert!(
            decoded.is_err(),
            "{text} names no single variant, so it must not decode"
        );
    }
}

#[test]
fn an_unknown_field_inside_a_known_variant_is_ignored() {
    let decoded: MaybeKnown<Sample> =
        serde_json::from_str(r#"{"Keep":{"value":7,"added_later":true}}"#).unwrap();
    assert_eq!(decoded, MaybeKnown::Known(Sample::Keep { value: 7 }));
}

/// A variant travels as a one-key object. An object with a second key names no
/// variant, whichever of its keys this build has, so it is refused rather than
/// read as the key that happens to come first.
#[test]
fn an_object_with_a_second_key_names_no_variant() {
    for text in [
        r#"{"Keep":{"value":1},"Added":2}"#,
        r#"{"Added":2,"Keep":{"value":1}}"#,
        r#"{"Added":1,"AlsoAdded":2}"#,
    ] {
        let decoded: Result<MaybeKnown<Sample>, _> = serde_json::from_str(text);
        assert!(
            decoded.is_err(),
            "{text} carries two keys, so it names no variant"
        );
    }
}

/// A payload is never decoded while the variant is being named, so an unknown
/// variant carrying bytes this build could not read still reads as unknown
/// rather than as an error.
#[test]
fn naming_an_unknown_variant_never_reads_its_payload() {
    let decoded: MaybeKnown<Sample> =
        serde_json::from_str(r#"{"Added":{"value":{"deeply":["nested",1,true,null]}}}"#).unwrap();
    assert_eq!(
        decoded,
        MaybeKnown::Unknown {
            name: "Added".to_string()
        }
    );
}

/// The decode runs first, and the name is read only when it fails, so a value
/// that decodes never reaches the `VARIANTS` list.
/// [`every_wire_enum_lists_the_variants_it_writes`] keeps the two in step for
/// the real wire enums.
#[test]
fn a_value_that_decodes_is_known_even_when_variants_omits_its_name() {
    #[derive(Debug, PartialEq, Eq, Deserialize)]
    enum Partial {
        Listed { value: u32 },
        Unlisted { value: u32 },
    }

    impl WireVariants for Partial {
        const VARIANTS: &'static [&'static str] = &["Listed"];
    }

    let decoded: MaybeKnown<Partial> = serde_json::from_str(r#"{"Unlisted":{"value":3}}"#).unwrap();
    assert_eq!(decoded, MaybeKnown::Known(Partial::Unlisted { value: 3 }));

    let listed: MaybeKnown<Partial> = serde_json::from_str(r#"{"Listed":{"value":4}}"#).unwrap();
    assert_eq!(listed, MaybeKnown::Known(Partial::Listed { value: 4 }));

    let absent: MaybeKnown<Partial> = serde_json::from_str(r#"{"Added":{"value":5}}"#).unwrap();
    assert_eq!(
        absent,
        MaybeKnown::Unknown {
            name: "Added".to_string()
        }
    );
}

#[test]
fn or_default_falls_back_for_a_value_this_build_cannot_read() {
    #[derive(Debug, Default, PartialEq, Eq, Deserialize)]
    struct Holder {
        #[serde(default, deserialize_with = "or_default")]
        shade: Shade,
    }

    #[derive(Debug, Default, PartialEq, Eq, Deserialize)]
    enum Shade {
        #[default]
        Plain,
        Deep(u8),
    }

    let known: Holder = serde_json::from_str(r#"{"shade":{"Deep":4}}"#).unwrap();
    assert_eq!(
        known.shade,
        Shade::Deep(4),
        "a value it has reads as itself"
    );

    let unknown: Holder = serde_json::from_str(r#"{"shade":"Neon"}"#).unwrap();
    assert_eq!(
        unknown.shade,
        Shade::Plain,
        "a value it has no name for falls back to the default"
    );

    let unreadable: Holder = serde_json::from_str(r#"{"shade":{"Deep":"x"}}"#).unwrap();
    assert_eq!(
        unreadable.shade,
        Shade::Plain,
        "a value it cannot read falls back to the default"
    );

    let absent: Holder = serde_json::from_str("{}").unwrap();
    assert_eq!(
        absent.shade,
        Shade::Plain,
        "an absent value takes the default"
    );
}

/// Every wire enum lists exactly the variants it can produce. A variant added
/// without its `VARIANTS` entry would arrive as `Unknown` on the far side, so
/// the two are pinned together here.
#[test]
fn every_wire_enum_lists_the_variants_it_writes() {
    fn assert_listed<T>(values: Vec<T>)
    where
        T: Serialize + WireName + WireVariants + std::fmt::Debug,
    {
        assert_eq!(
            T::VARIANTS.len(),
            values.len(),
            "the sample list and VARIANTS must cover the same variants: {:?}",
            T::VARIANTS
        );
        for value in values {
            let name = value.wire_name();
            assert!(
                T::VARIANTS.contains(&name),
                "{name} is written but missing from VARIANTS"
            );
            let encoded = serde_json::to_string(&value).unwrap();
            assert_eq!(
                variant_name(&encoded).as_deref(),
                Some(name),
                "{value:?} writes a tag that does not match its name"
            );
        }
    }

    assert_listed(sample_request_kinds());
    assert_listed(sample_results());
    assert_listed(sample_events());
    assert_listed(sample_router_kinds());
    assert_listed(sample_router_results());
    assert_listed(sample_supervisor_kinds());
    assert_listed(sample_supervisor_results());
    assert_listed(sample_supervisor_events());
}

/// One value per [`SupervisorRequestKind`] variant.
fn sample_supervisor_kinds() -> Vec<SupervisorRequestKind> {
    use koshi_core::process::{KillPolicy, PtySize, ShellKind, SpawnSpec};

    let size = PtySize { cols: 80, rows: 24 };

    vec![
        SupervisorRequestKind::Hello {
            min_protocol_version: 1,
            max_protocol_version: 1,
            token: ConnectionToken::new("t"),
        },
        SupervisorRequestKind::Spawn {
            pane_id: PaneId::new(),
            spec: SpawnSpec {
                program: std::path::PathBuf::from("/bin/sh"),
                args: Vec::new(),
                cwd: None,
                env: std::collections::BTreeMap::new(),
                shell_kind: ShellKind::Bash,
            },
            size,
        },
        SupervisorRequestKind::Resize {
            pane_id: PaneId::new(),
            size,
        },
        SupervisorRequestKind::Write {
            pane_id: PaneId::new(),
            bytes: Vec::new(),
        },
        SupervisorRequestKind::Kill {
            pane_id: PaneId::new(),
            kill_policy: KillPolicy::Force,
        },
        SupervisorRequestKind::LiveCwd {
            pane_id: PaneId::new(),
        },
        SupervisorRequestKind::ListPanes,
        SupervisorRequestKind::PauseOutput,
        SupervisorRequestKind::ResumeOutput,
        SupervisorRequestKind::Shutdown,
    ]
}

/// One value per [`SupervisorResult`] variant.
fn sample_supervisor_results() -> Vec<SupervisorResult> {
    vec![
        SupervisorResult::Hello {
            protocol_version: 1,
        },
        SupervisorResult::Spawned { pid: 1 },
        SupervisorResult::Panes(Vec::new()),
        SupervisorResult::Cwd(None),
        SupervisorResult::Done,
        SupervisorResult::Error(crate::protocol::IpcErrorPayload {
            code: crate::protocol::IpcErrorCode::BadToken,
            message: String::new(),
        }),
    ]
}

/// One value per [`SupervisorEvent`] variant.
fn sample_supervisor_events() -> Vec<SupervisorEvent> {
    use koshi_core::process::ExitStatus;

    vec![
        SupervisorEvent::Output {
            pane_id: PaneId::new(),
            bytes: Vec::new(),
        },
        SupervisorEvent::Exited {
            pane_id: PaneId::new(),
            status: ExitStatus::Signaled(9),
        },
    ]
}

/// One value per [`IpcRequestKind`] variant. The match in
/// [`IpcRequestKind::name`] is exhaustive, so a new variant fails the build
/// there; this list is what pins the count.
fn sample_request_kinds() -> Vec<IpcRequestKind> {
    use koshi_core::geometry::Size;

    vec![
        IpcRequestKind::Hello {
            min_protocol_version: 2,
            max_protocol_version: 2,
            token: ConnectionToken::new("t"),
        },
        IpcRequestKind::Attach {
            viewport: Size { cols: 80, rows: 24 },
            filter: crate::protocol::EventFilterSpec::All,
            resume: None,
        },
        IpcRequestKind::KeyPress {
            chord: koshi_core::key::KeyChord::new(
                koshi_core::key::ModFlags::NONE,
                koshi_core::key::Key::Char('a'),
            ),
        },
        IpcRequestKind::Resize {
            viewport: Size { cols: 80, rows: 24 },
        },
        IpcRequestKind::Paste {
            text: String::new(),
        },
        IpcRequestKind::Mouse(Vec::new()),
        IpcRequestKind::SubmitCommand(Box::new(koshi_core::command::CommandEnvelope::new(
            koshi_core::ids::CommandId::new(),
            koshi_core::command::CommandSource::ExternalCli { session_id: None },
            std::time::UNIX_EPOCH,
            koshi_core::command::Command::ToggleLockMode(
                koshi_core::command::ToggleLockModeArgs::default(),
            ),
        ))),
        IpcRequestKind::Discovery,
        IpcRequestKind::Layout { tab: None },
        IpcRequestKind::Restart,
        IpcRequestKind::Leaving,
    ]
}

/// One value per [`IpcResult`] variant.
fn sample_results() -> Vec<IpcResult> {
    vec![
        IpcResult::Hello {
            protocol_version: 2,
            version: String::new(),
        },
        IpcResult::Attached {
            client_id: koshi_core::ids::ClientId::new(),
            session_id: koshi_core::ids::SessionId::new(),
            structure: crate::attach::AttachedSessionStructureSnapshot {
                id: koshi_core::ids::SessionId::new(),
                name: String::new(),
                tabs: Vec::new(),
                panes: Vec::new(),
            },
        },
        IpcResult::CommandResult(koshi_core::command::CommandResult::Ok {
            command_id: koshi_core::ids::CommandId::new(),
            emitted_events: Vec::new(),
        }),
        IpcResult::Overview(koshi_core::discovery::SessionOverview {
            session: session_info(),
            tabs: Vec::new(),
            panes: Vec::new(),
            clients: Vec::new(),
        }),
        IpcResult::Layout(crate::layout::SessionLayout {
            id: koshi_core::ids::SessionId::new(),
            name: String::new(),
            tabs: Vec::new(),
            clients: Vec::new(),
        }),
        IpcResult::Restarting,
        IpcResult::Error(crate::protocol::IpcErrorPayload {
            code: crate::protocol::IpcErrorCode::BadToken,
            message: String::new(),
        }),
    ]
}

/// The smallest session record a discovery answer can carry.
fn session_info() -> koshi_core::discovery::SessionInfo {
    koshi_core::discovery::SessionInfo {
        id: koshi_core::ids::SessionId::new(),
        name: String::new(),
        created_at: std::time::UNIX_EPOCH,
        attached_clients: Vec::new(),
        pane_count: 0,
    }
}

/// One value per [`SessionEvent`] variant.
fn sample_events() -> Vec<SessionEvent> {
    use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};

    vec![
        SessionEvent::Painted {
            frame: Box::new(painted_frame()),
        },
        SessionEvent::PaneCreated {
            pane_id: PaneId::new(),
            tab_id: TabId::new(),
        },
        SessionEvent::PaneProcessExited {
            pane_id: PaneId::new(),
            exit_code: None,
        },
        SessionEvent::PaneClosing {
            pane_id: PaneId::new(),
        },
        SessionEvent::PaneRemoved {
            pane_id: PaneId::new(),
            tab_id: TabId::new(),
        },
        SessionEvent::PaneFocused {
            client_id: ClientId::new(),
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
            prior_pane: None,
        },
        SessionEvent::LayoutChanged {
            tab_id: TabId::new(),
        },
        SessionEvent::TabCreated {
            tab_id: TabId::new(),
        },
        SessionEvent::TabClosed {
            tab_id: TabId::new(),
        },
        SessionEvent::TabFocused {
            client_id: ClientId::new(),
            tab_id: TabId::new(),
            prior_tab: TabId::new(),
        },
        SessionEvent::TabMoved {
            tab_id: TabId::new(),
            old_index: 0,
            new_index: 1,
        },
        SessionEvent::Quit,
        SessionEvent::Restarting,
        SessionEvent::Detached,
        SessionEvent::Resync { dropped_count: 1 },
        SessionEvent::MouseAnswer {
            request_id: 1,
            answers: Vec::new(),
        },
        SessionEvent::HostWrite { bytes: Vec::new() },
        SessionEvent::SwitchTo {
            session_id: SessionId::new(),
        },
    ]
}

/// One value per [`RouterRequestKind`] variant.
fn sample_router_kinds() -> Vec<RouterRequestKind> {
    vec![
        RouterRequestKind::Hello {
            min_protocol_version: 1,
            max_protocol_version: 1,
            token: ConnectionToken::new("t"),
        },
        RouterRequestKind::CreateSession {
            profile: None,
            cwd: None,
            allow_other_users: None,
        },
        RouterRequestKind::AttachLookup {
            selector: crate::router::SessionSelector::Name("quiet-lake".to_string()),
        },
        RouterRequestKind::ListSessions,
        RouterRequestKind::Restart,
        RouterRequestKind::GrantToken {
            identity: String::new(),
            scope: crate::remote_tokens::TokenScope::HostWide,
            expires_in: None,
        },
        RouterRequestKind::RevokeToken {
            identity: String::new(),
            scope: None,
        },
        RouterRequestKind::ListTokens { scope: None },
        RouterRequestKind::RemoteStatus,
        RouterRequestKind::EnableRemote,
    ]
}

/// One value per [`RouterResult`] variant.
fn sample_router_results() -> Vec<RouterResult> {
    let address = crate::router::SessionAddress {
        id: koshi_core::ids::SessionId::new(),
        name: String::new(),
        socket: String::new(),
        pid: 1,
    };
    vec![
        RouterResult::Hello {
            protocol_version: 1,
            version: String::new(),
        },
        RouterResult::Created(address.clone()),
        RouterResult::Found(address),
        RouterResult::Sessions(Vec::new()),
        RouterResult::Restarting,
        RouterResult::Granted {
            token: ConnectionToken::new("t"),
            replaced: false,
        },
        RouterResult::Revoked(Vec::new()),
        RouterResult::Tokens(Vec::new()),
        RouterResult::RemoteStatus {
            address: None,
            enabled: false,
            fingerprint: None,
        },
        RouterResult::RemoteEnabled {
            address: String::new(),
            fingerprint: String::new(),
        },
        RouterResult::Error(crate::protocol::IpcErrorPayload {
            code: crate::protocol::IpcErrorCode::BadToken,
            message: String::new(),
        }),
    ]
}

/// The smallest frame that still holds every record a painted frame needs.
fn painted_frame() -> crate::frame::PaintedFrame {
    use koshi_core::geometry::Size;
    use koshi_core::ids::{ClientId, SessionId, TabId};

    crate::frame::PaintedFrame {
        session: crate::frame::FrameSession {
            id: SessionId::new(),
            name: String::new(),
            active_tab: crate::frame::FrameTab {
                id: TabId::new(),
                name: String::new(),
                slots: Vec::new(),
                effective_size: Size { cols: 80, rows: 24 },
                stack_headers: Vec::new(),
                layout_mode: koshi_layout::mode::LayoutMode::Tiled,
                all_suppressed: false,
            },
            tabs: Vec::new(),
        },
        panes: Vec::new(),
        client: crate::frame::FrameClient {
            id: ClientId::new(),
            viewport: Size { cols: 80, rows: 24 },
            active_tab: TabId::new(),
            focused_pane: None,
            lock_mode: koshi_core::lock::LockMode::default(),
            mouse_select: false,
        },
    }
}
