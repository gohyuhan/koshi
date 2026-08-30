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

/// A variant with no fields also travels as a one-key object whose value is
/// `null`.
#[test]
fn a_variant_with_no_fields_decodes_from_a_one_key_object_with_null() {
    let decoded: MaybeKnown<Sample> = serde_json::from_str(r#"{"Bare":null}"#).unwrap();
    assert_eq!(decoded, MaybeKnown::Known(Sample::Bare));
}

/// A name this build has, spelled as a bare string while its variant carries
/// fields, keeps the decoder's refusal. That refusal carries no position.
#[test]
fn a_known_variant_spelled_without_its_fields_is_an_error() {
    let decoded: Result<MaybeKnown<Sample>, _> = serde_json::from_str(r#""Keep""#);
    let error = decoded.expect_err("a known name with the wrong shape is an error");
    assert_eq!(
        error.to_string(),
        "invalid type: unit variant, expected struct variant"
    );
}

#[test]
fn whitespace_around_a_value_does_not_change_what_it_names() {
    let known: MaybeKnown<Sample> =
        serde_json::from_str(" { \"Keep\" : { \"value\" : 7 } } ").unwrap();
    assert_eq!(known, MaybeKnown::Known(Sample::Keep { value: 7 }));

    for text in [" \"Added\" ", " { \"Added\" : 1 } "] {
        let unknown: MaybeKnown<Sample> = serde_json::from_str(text).unwrap();
        assert_eq!(
            unknown,
            MaybeKnown::Unknown {
                name: "Added".to_string()
            },
            "{text}"
        );
    }
}

#[test]
fn a_non_ascii_name_is_kept_as_the_peer_spelled_it() {
    let decoded: MaybeKnown<Sample> = serde_json::from_str(r#"{"Añadido":{"pane":3}}"#).unwrap();
    assert_eq!(
        decoded,
        MaybeKnown::Unknown {
            name: "Añadido".to_string()
        }
    );
}

#[test]
fn a_variant_this_build_has_but_cannot_read_is_an_error_not_an_unknown() {
    let decoded: Result<MaybeKnown<Sample>, _> = serde_json::from_str(r#"{"Keep":{"value":"x"}}"#);
    let error = decoded.expect_err("a known variant with an unreadable payload is an error");
    assert_eq!(
        error.to_string(),
        r#"invalid type: string "x", expected u32 at line 1 column 20"#
    );
}

#[test]
fn a_value_that_names_no_variant_is_an_error() {
    for text in [r#"{"Keep":1,"Bare":2}"#, "7", "[]", "null", "{}"] {
        let decoded: Result<MaybeKnown<Sample>, _> = serde_json::from_str(text);
        let error = decoded.expect_err(text);
        assert_eq!(
            error.to_string(),
            "a wire value is a variant name, or a one-key object naming one",
            "{text}"
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
/// variant, whichever of its keys this build has, and whichever key comes
/// first.
#[test]
fn an_object_with_a_second_key_names_no_variant() {
    for text in [
        r#"{"Keep":{"value":1},"Added":2}"#,
        r#"{"Added":2,"Keep":{"value":1}}"#,
        r#"{"Added":1,"AlsoAdded":2}"#,
    ] {
        let decoded: Result<MaybeKnown<Sample>, _> = serde_json::from_str(text);
        let error = decoded.expect_err(text);
        assert_eq!(
            error.to_string(),
            "a wire value is a variant name, or a one-key object naming one",
            "{text}"
        );
    }
}

#[test]
fn an_empty_name_is_unknown_with_an_empty_name() {
    let decoded: MaybeKnown<Sample> = serde_json::from_str(r#""""#).unwrap();
    assert_eq!(
        decoded,
        MaybeKnown::Unknown {
            name: String::new()
        }
    );
}

/// The name is the decoded JSON string: an escape in the text is read as the
/// character it stands for, as a bare name and as an object key.
#[test]
fn an_escaped_name_is_read_as_the_characters_it_stands_for() {
    for text in [r#""\u0041dded""#, r#"{"\u0041dded":1}"#] {
        let decoded: MaybeKnown<Sample> = serde_json::from_str(text).unwrap();
        assert_eq!(
            decoded,
            MaybeKnown::Unknown {
                name: "Added".to_string()
            },
            "{text}"
        );
    }
}

/// serde_json stops building a value 128 levels deep. Naming a variant walks
/// the payload without building it and has no depth limit: an unknown name
/// past the limit is still unknown, and a known name past it keeps the
/// decoder's own refusal.
#[test]
fn a_payload_nested_past_the_decoders_depth_limit_still_names_its_variant() {
    #[derive(Debug, PartialEq, Eq, Deserialize)]
    enum Holder {
        Tree(serde_json::Value),
    }

    impl WireVariants for Holder {
        const VARIANTS: &'static [&'static str] = &["Tree"];
    }

    let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));

    let unknown: MaybeKnown<Holder> =
        serde_json::from_str(&format!(r#"{{"Added":{deep}}}"#)).unwrap();
    assert_eq!(
        unknown,
        MaybeKnown::Unknown {
            name: "Added".to_string()
        }
    );

    let known: Result<MaybeKnown<Holder>, _> =
        serde_json::from_str(&format!(r#"{{"Tree":{deep}}}"#));
    let error = known.expect_err("a known name keeps the decoder's refusal");
    assert_eq!(
        error.to_string(),
        "recursion limit exceeded at line 1 column 135"
    );
}

/// The raw text is borrowed from the input, and a reader lends nothing.
#[test]
fn decoding_from_a_reader_that_lends_no_bytes_is_an_error() {
    let reader = std::io::Cursor::new(br#"{"Keep":{"value":7}}"#.to_vec());
    let decoded: Result<MaybeKnown<Sample>, _> = serde_json::from_reader(reader);
    let error = decoded.expect_err("a reader cannot lend its bytes to the raw text");
    assert_eq!(
        error.to_string(),
        r#"invalid type: string "{\"Keep\":{\"value\":7}}", expected raw value"#
    );
}

/// The refusal for an unreadable payload is positioned inside the kind's own
/// text, not inside the whole envelope: column 20 here is the `"x"` counted
/// from the start of `{"Keep":…}`, which sits at column 39 of the envelope.
#[test]
fn a_payload_fault_inside_an_envelope_keeps_the_kind_relative_position() {
    let decoded: Result<Envelope<MaybeKnown<Sample>>, _> =
        serde_json::from_str(r#"{"request_id":1,"kind":{"Keep":{"value":"x"}}}"#);
    let error = decoded.expect_err("a known variant with an unreadable payload is an error");
    assert_eq!(
        error.to_string(),
        r#"invalid type: string "x", expected u32 at line 1 column 20"#
    );
}

#[test]
fn an_envelope_carrying_a_kind_this_build_lacks_reads_as_unknown() {
    let decoded: Envelope<MaybeKnown<Sample>> =
        serde_json::from_str(r#"{"request_id":9,"kind":{"Added":{"pane":3}}}"#).unwrap();
    assert_eq!(
        decoded,
        Envelope {
            request_id: 9,
            kind: MaybeKnown::Unknown {
                name: "Added".to_string()
            },
        }
    );
}

#[test]
fn an_envelope_without_a_request_id_is_refused() {
    let decoded: Result<Envelope<Sample>, _> = serde_json::from_str(r#"{"kind":"Bare"}"#);
    let error = decoded.expect_err("the request id is not optional");
    assert_eq!(
        error.to_string(),
        "missing field `request_id` at line 1 column 15"
    );
}

#[test]
fn an_envelope_with_a_field_it_does_not_know_is_refused() {
    let decoded: Result<Envelope<Sample>, _> =
        serde_json::from_str(r#"{"request_id":1,"kind":"Bare","extra":true}"#);
    let error = decoded.expect_err("an envelope has exactly two fields");
    assert_eq!(
        error.to_string(),
        "unknown field `extra`, expected `request_id` or `kind` at line 1 column 37"
    );
}

#[test]
fn an_answer_with_a_field_it_does_not_know_is_refused() {
    let decoded: Result<Answer<Sample>, _> =
        serde_json::from_str(r#"{"request_id":1,"result":"Bare","extra":true}"#);
    let error = decoded.expect_err("an answer has exactly two fields");
    assert_eq!(
        error.to_string(),
        "unknown field `extra`, expected `request_id` or `result` at line 1 column 39"
    );
}

/// The JSON an envelope and an answer write: `request_id` first, then the
/// payload, and an absent answer id written as `null`.
#[test]
fn an_envelope_and_an_answer_write_their_fields_in_order() {
    let envelope = Envelope {
        request_id: 7,
        kind: Sample::Keep { value: 1 },
    };
    assert_eq!(
        serde_json::to_string(&envelope).unwrap(),
        r#"{"request_id":7,"kind":{"Keep":{"value":1}}}"#
    );

    let answer = Answer {
        request_id: None,
        result: Sample::Bare,
    };
    assert_eq!(
        serde_json::to_string(&answer).unwrap(),
        r#"{"request_id":null,"result":"Bare"}"#
    );
}

#[test]
fn an_answer_carrying_a_result_this_build_lacks_reads_as_unknown() {
    let decoded: Answer<MaybeKnown<Sample>> =
        serde_json::from_str(r#"{"request_id":9,"result":{"Added":{"pane":3}}}"#).unwrap();
    assert_eq!(
        decoded,
        Answer {
            request_id: Some(9),
            result: MaybeKnown::Unknown {
                name: "Added".to_string()
            },
        }
    );
}

/// An answer's `request_id` reads as `None` both when the field is absent and
/// when it is `null`.
#[test]
fn an_answer_with_no_request_id_reads_as_none() {
    for text in [
        r#"{"result":"Bare"}"#,
        r#"{"request_id":null,"result":"Bare"}"#,
    ] {
        let decoded: Answer<Sample> = serde_json::from_str(text).unwrap();
        assert_eq!(
            decoded,
            Answer {
                request_id: None,
                result: Sample::Bare,
            },
            "{text}"
        );
    }
}

/// The payload is not decoded while the variant is being named: an unknown
/// variant carrying bytes this build could not read reads as unknown.
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

/// The decode runs first, and the name is read only when it fails: a value
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

/// `null` and a number the type cannot hold both fall back to the default.
#[test]
fn or_default_falls_back_for_null_and_for_a_number_outside_the_type() {
    #[derive(Debug, Default, PartialEq, Eq, Deserialize)]
    struct Holder {
        #[serde(default, deserialize_with = "or_default")]
        gap: u16,
    }

    for (text, expected) in [
        (r#"{"gap":3}"#, 3),
        (r#"{"gap":null}"#, 0),
        (r#"{"gap":70000}"#, 0),
        (r#"{"gap":-1}"#, 0),
        (r#"{"gap":"3"}"#, 0),
    ] {
        let holder: Holder = serde_json::from_str(text).unwrap();
        assert_eq!(holder.gap, expected, "{text}");
    }
}

/// `or_default` borrows the raw text the same way `MaybeKnown` does, and a
/// reader lends nothing.
#[test]
fn or_default_from_a_reader_that_lends_no_bytes_is_an_error() {
    #[derive(Debug, Default, PartialEq, Eq, Deserialize)]
    struct Holder {
        #[serde(default, deserialize_with = "or_default")]
        gap: u16,
    }

    let reader = std::io::Cursor::new(br#"{"gap":3}"#.to_vec());
    let decoded: Result<Holder, _> = serde_json::from_reader(reader);
    let error = decoded.expect_err("a reader cannot lend its bytes to the raw text");
    assert_eq!(
        error.to_string(),
        r#"invalid type: string "3", expected raw value at line 1 column 9"#
    );
}

/// Every wire enum lists exactly the variants it can produce: each sample
/// value's name is in `VARIANTS`, the two lists have the same length, and the
/// JSON the value writes names that same variant.
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

/// Every wire enum's `VARIANTS` holds exactly the variants its type has. The
/// real list comes from the type's own decoder through [`variants_of`], and
/// follows the enum without being maintained.
///
/// `the_plane_a_remote_client_reaches_names_no_token_verb` in the protocol
/// tests reads `IpcRequestKind::VARIANTS` as the session plane's whole
/// vocabulary.
#[test]
fn every_wire_enum_lists_exactly_the_variants_its_type_has() {
    fn assert_matches<T: DeserializeOwned + WireVariants>(type_name: &str) {
        let mut listed: Vec<String> = T::VARIANTS.iter().map(|name| (*name).to_string()).collect();
        listed.sort();
        let mut real = variants_of::<T>();
        real.sort();
        assert_eq!(listed, real, "{type_name}");
    }

    assert_matches::<IpcRequestKind>("IpcRequestKind");
    assert_matches::<IpcResult>("IpcResult");
    assert_matches::<SessionEvent>("SessionEvent");
    assert_matches::<RouterRequestKind>("RouterRequestKind");
    assert_matches::<RouterResult>("RouterResult");
    assert_matches::<SupervisorRequestKind>("SupervisorRequestKind");
    assert_matches::<SupervisorResult>("SupervisorResult");
    assert_matches::<SupervisorEvent>("SupervisorEvent");
}

/// The variant names `T`'s decoder holds, read out of the refusal it writes for
/// a name that is not one of them.
///
/// Example — for a `SupervisorEvent` the refusal reads ``unknown variant
/// `koshi-no-such-variant`, expected `Output` or `Exited` at line 1 column 25``,
/// and the names in backticks after the first are `Output` and `Exited`.
fn variants_of<T: DeserializeOwned>() -> Vec<String> {
    let refusal = serde_json::from_str::<T>("\"koshi-no-such-variant\"")
        .err()
        .expect("a name no variant carries is refused")
        .to_string();
    assert!(
        refusal.starts_with("unknown variant `koshi-no-such-variant`, expected "),
        "the refusal no longer names the variants it knows: {refusal}"
    );
    // Odd-numbered pieces of a split on the backtick are what sat between two
    // of them. The first is the name that was refused; the rest are the names
    // the decoder holds.
    refusal
        .split('`')
        .skip(3)
        .step_by(2)
        .map(str::to_string)
        .collect()
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

/// One value per [`IpcRequestKind`] variant.
fn sample_request_kinds() -> Vec<IpcRequestKind> {
    use koshi_core::geometry::Size;

    vec![
        IpcRequestKind::Hello {
            min_protocol_version: 2,
            max_protocol_version: 2,
            token: ConnectionToken::new("t"),
            remote: false,
        },
        IpcRequestKind::Attach {
            viewport: Size { cols: 80, rows: 24 },
            filter: crate::protocol::EventFilterSpec::All,
            resume: None,
            resume_token: None,
            pane_area: None,
        },
        IpcRequestKind::KeyPress {
            chord: koshi_core::key::KeyChord::new(
                koshi_core::key::ModFlags::NONE,
                koshi_core::key::Key::Char('a'),
            ),
        },
        IpcRequestKind::Resize {
            viewport: Size { cols: 80, rows: 24 },
            pane_area: None,
        },
        IpcRequestKind::Paste {
            text: String::new(),
        },
        IpcRequestKind::Mouse(Vec::new()),
        IpcRequestKind::SubmitCommand(Box::new(koshi_core::command::CommandEnvelope::new(
            koshi_core::ids::CommandId::new(),
            koshi_core::command::CommandSource::ExternalCli {
                session_id: None,
                target_client: None,
            },
            std::time::UNIX_EPOCH,
            koshi_core::command::Command::ToggleLockMode(
                koshi_core::command::ToggleLockModeArgs::default(),
            ),
        ))),
        IpcRequestKind::Discovery,
        IpcRequestKind::Layout { tab: None },
        IpcRequestKind::RecentEvents,
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
            resume_token: None,
            pane_area: None,
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
        IpcResult::RecentEvents(Vec::new()),
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
            listening: false,
            fingerprint: None,
            remote_connections: Some(0),
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
                gap: 0,
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

#[test]
fn an_unknown_name_is_filtered_as_it_is_read() {
    // The name is quoted back in a refusal and written on a log line, and the
    // peer that chose it may be another local user or another machine.
    let decoded: MaybeKnown<Sample> =
        serde_json::from_str("{\"\\u001b[2JAdded\":{\"pane\":3}}").unwrap();
    assert_eq!(
        decoded,
        MaybeKnown::Unknown {
            name: "[2JAdded".to_string(),
        }
    );
}

#[test]
fn an_unknown_name_is_cut_to_the_reported_text_cap() {
    let long = "A".repeat(100_000);
    let decoded: MaybeKnown<Sample> =
        serde_json::from_str(&format!(r#"{{"{long}":{{"pane":3}}}}"#)).unwrap();
    let MaybeKnown::Unknown { name } = decoded else {
        panic!("a name this build does not have reads as unknown");
    };
    assert_eq!(name.len(), koshi_core::text::MAX_REPORTED_TEXT_BYTES);
}
