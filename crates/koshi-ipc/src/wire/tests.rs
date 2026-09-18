//! Tests for reading a message whose variant this build may not have.

use koshi_core::ids::PaneId;
use serde::Serialize;

use super::*;
use crate::event::SessionEvent;
use crate::frame::{
    FrameGraphicsProtocol, FrameImageAction, FrameImageChunk, FrameImageDisplay,
    FrameImageRecordHeader, FrameImageTransfer,
};
use crate::protocol::{ConnectionToken, IpcRequestKind, IpcResult};
use crate::router::{RouterRequestKind, RouterResult};
use crate::supervisor::{SupervisorEvent, SupervisorRequestKind, SupervisorResult};

/// A stand-in for a build that has fewer variants than its peer: it knows
/// `Keep` and `Bare`, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Sample {
    Keep {
        #[serde(rename = "value")]
        payload_number: u32,
    },
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
    assert_eq!(
        decoded,
        MaybeKnown::Known(Sample::Keep { payload_number: 7 })
    );
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
            variant_name: "Added".to_string()
        }
    );
}

#[test]
fn a_variant_this_build_lacks_and_that_carries_no_fields_decodes_as_unknown() {
    let decoded: MaybeKnown<Sample> = serde_json::from_str(r#""Added""#).unwrap();
    assert_eq!(
        decoded,
        MaybeKnown::Unknown {
            variant_name: "Added".to_string()
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
    assert_eq!(known, MaybeKnown::Known(Sample::Keep { payload_number: 7 }));

    for text in [" \"Added\" ", " { \"Added\" : 1 } "] {
        let unknown: MaybeKnown<Sample> = serde_json::from_str(text).unwrap();
        assert_eq!(
            unknown,
            MaybeKnown::Unknown {
                variant_name: "Added".to_string()
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
            variant_name: "Añadido".to_string()
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
    assert_eq!(
        decoded,
        MaybeKnown::Known(Sample::Keep { payload_number: 7 })
    );
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
            variant_name: String::new()
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
                variant_name: "Added".to_string()
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
            variant_name: "Added".to_string()
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
            request_kind: MaybeKnown::Unknown {
                variant_name: "Added".to_string()
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
        request_kind: Sample::Keep { payload_number: 1 },
    };
    assert_eq!(
        serde_json::to_string(&envelope).unwrap(),
        r#"{"request_id":7,"kind":{"Keep":{"value":1}}}"#
    );

    let response = Answer {
        request_id: None,
        answer_result: Sample::Bare,
    };
    assert_eq!(
        serde_json::to_string(&response).unwrap(),
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
            answer_result: MaybeKnown::Unknown {
                variant_name: "Added".to_string()
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
                answer_result: Sample::Bare,
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
            variant_name: "Added".to_string()
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
        Listed {
            #[serde(rename = "value")]
            payload_number: u32,
        },
        Unlisted {
            #[serde(rename = "value")]
            payload_number: u32,
        },
    }

    impl WireVariants for Partial {
        const VARIANTS: &'static [&'static str] = &["Listed"];
    }

    let decoded: MaybeKnown<Partial> = serde_json::from_str(r#"{"Unlisted":{"value":3}}"#).unwrap();
    assert_eq!(
        decoded,
        MaybeKnown::Known(Partial::Unlisted { payload_number: 3 })
    );

    let listed: MaybeKnown<Partial> = serde_json::from_str(r#"{"Listed":{"value":4}}"#).unwrap();
    assert_eq!(
        listed,
        MaybeKnown::Known(Partial::Listed { payload_number: 4 })
    );

    let absent: MaybeKnown<Partial> = serde_json::from_str(r#"{"Added":{"value":5}}"#).unwrap();
    assert_eq!(
        absent,
        MaybeKnown::Unknown {
            variant_name: "Added".to_string()
        }
    );
}

#[test]
fn or_default_falls_back_for_a_value_this_build_cannot_read() {
    #[derive(Debug, Default, PartialEq, Eq, Deserialize)]
    struct Holder {
        #[serde(default, deserialize_with = "deserialize_or_default")]
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
        #[serde(default, deserialize_with = "deserialize_or_default")]
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

/// `deserialize_or_default` borrows the raw text the same way `MaybeKnown` does, and a
/// reader lends nothing.
#[test]
fn or_default_from_a_reader_that_lends_no_bytes_is_an_error() {
    #[derive(Debug, Default, PartialEq, Eq, Deserialize)]
    struct Holder {
        #[serde(default, deserialize_with = "deserialize_or_default")]
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
    fn assert_listed<T>(wire_variants: Vec<T>)
    where
        T: Serialize + WireName + WireVariants + std::fmt::Debug,
    {
        assert_eq!(
            T::VARIANTS.len(),
            wire_variants.len(),
            "the sample list and VARIANTS must cover the same variants: {:?}",
            T::VARIANTS
        );
        for wire_variant in wire_variants {
            let wire_name = wire_variant.wire_name();
            assert!(
                T::VARIANTS.contains(&wire_name),
                "{wire_name} is written but missing from VARIANTS"
            );
            let encoded_json = serde_json::to_string(&wire_variant).unwrap();
            assert_eq!(
                parse_wire_variant_name(&encoded_json).as_deref(),
                Some(wire_name),
                "{wire_variant:?} writes a tag that does not match its name"
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
        let mut listed: Vec<String> = T::VARIANTS
            .iter()
            .map(|wire_variant_name| (*wire_variant_name).to_string())
            .collect();
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

    let sample_pty_size = PtySize {
        column_count: 80,
        row_count: 24,
    };

    vec![
        SupervisorRequestKind::Hello {
            min_protocol_version: 1,
            max_protocol_version: 1,
            connection_token: ConnectionToken::from_secret("t"),
        },
        SupervisorRequestKind::Spawn {
            pane_id: PaneId::new(),
            spawn_spec: SpawnSpec {
                program: std::path::PathBuf::from("/bin/sh"),
                arguments: Vec::new(),
                working_directory: None,
                environment_variables: std::collections::BTreeMap::new(),
                shell_kind: ShellKind::Bash,
            },
            pty_size: sample_pty_size,
        },
        SupervisorRequestKind::Resize {
            pane_id: PaneId::new(),
            pty_size: sample_pty_size,
        },
        SupervisorRequestKind::Write {
            pane_id: PaneId::new(),
            input_bytes: Vec::new(),
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
        SupervisorResult::Spawned { process_id: 1 },
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
            output_bytes: Vec::new(),
        },
        SupervisorEvent::Exited {
            pane_id: PaneId::new(),
            exit_status: ExitStatus::Signaled(9),
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
            connection_token: ConnectionToken::from_secret("t"),
            is_remote: false,
        },
        IpcRequestKind::Attach {
            viewport: Size {
                column_count: 80,
                row_count: 24,
            },
            event_filter: crate::protocol::EventFilterSpec::All,
            resume_client_id: None,
            resume_token: None,
            pane_area: None,
            graphics_capabilities: crate::protocol::GraphicsCapabilities::default(),
            cell_size: None,
        },
        IpcRequestKind::Keyboard {
            key_input: koshi_core::key::KeyInput {
                key: koshi_core::key::KeyIdentity::Key(koshi_core::key::Key::Char('a')),
                key_event_kind: koshi_core::key::KeyEventKind::Press,
                shifted_key: None,
                base_layout_key: None,
                associated_text: "a".to_string(),
                modifier_flags: koshi_core::key::KeyModifierFlags::NONE,
            },
        },
        IpcRequestKind::Resize {
            viewport: Size {
                column_count: 80,
                row_count: 24,
            },
            pane_area: None,
            cell_size: None,
        },
        IpcRequestKind::CellSize {
            cell_size: koshi_core::geometry::PixelCellSize::from_pixel_dimensions(10, 20)
                .expect("nonzero cell size"),
        },
        IpcRequestKind::Paste {
            pasted_text: String::new(),
        },
        IpcRequestKind::Mouse(Vec::new()),
        IpcRequestKind::SubmitCommand(Box::new(koshi_core::command::CommandEnvelope::from_parts(
            koshi_core::ids::CommandId::new(),
            koshi_core::command::CommandSource::ExternalCli {
                session_id: None,
                target_client_id: None,
            },
            std::time::UNIX_EPOCH,
            koshi_core::command::Command::ToggleLockMode(
                koshi_core::command::ToggleLockModeArgs::default(),
            ),
        ))),
        IpcRequestKind::Discovery,
        IpcRequestKind::Layout { tab_id: None },
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
            build_version: String::new(),
        },
        IpcResult::Attached {
            client_id: koshi_core::ids::ClientId::new(),
            session_id: koshi_core::ids::SessionId::new(),
            session_structure: crate::attach::AttachedSessionStructureSnapshot {
                session_id: koshi_core::ids::SessionId::new(),
                session_name: String::new(),
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
            session: build_test_session_discovery(),
            tabs: Vec::new(),
            panes: Vec::new(),
            clients: Vec::new(),
        }),
        IpcResult::Layout(crate::layout::SessionLayout {
            session_id: koshi_core::ids::SessionId::new(),
            session_name: String::new(),
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
fn build_test_session_discovery() -> koshi_core::discovery::SessionDiscovery {
    koshi_core::discovery::SessionDiscovery {
        session_id: koshi_core::ids::SessionId::new(),
        session_name: String::new(),
        created_at: std::time::UNIX_EPOCH,
        attached_client_ids: Vec::new(),
        pane_count: 0,
    }
}

/// One value per [`SessionEvent`] variant.
fn sample_events() -> Vec<SessionEvent> {
    use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};

    vec![
        SessionEvent::Painted {
            frame: Box::new(build_test_painted_frame()),
        },
        SessionEvent::ImageCacheReset,
        SessionEvent::ImageContentStart {
            image_transfer: FrameImageTransfer {
                image_content_id: 1,
                image_record: FrameImageRecordHeader {
                    protocol: FrameGraphicsProtocol::Kitty,
                    pixel_width: 1,
                    pixel_height: 1,
                    image_action: FrameImageAction::Display,
                    display: FrameImageDisplay::default(),
                    anchor_cell: (0, 0),
                },
                image_byte_count: 4,
            },
        },
        SessionEvent::ImageContentChunk {
            image_chunk: FrameImageChunk {
                image_transfer_id: 1,
                byte_offset: 0,
                is_last: true,
                chunk_bytes: vec![0],
            },
        },
        SessionEvent::PaneCreated {
            pane_id: PaneId::new(),
            tab_id: TabId::new(),
        },
        SessionEvent::PaneProcessExited {
            pane_id: PaneId::new(),
            exit_code: None,
            signal: None,
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
            previous_pane_id: None,
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
            previous_tab_id: TabId::new(),
        },
        SessionEvent::TabMoved {
            tab_id: TabId::new(),
            previous_tab_index: 0,
            new_tab_index: 1,
        },
        SessionEvent::Quit,
        SessionEvent::Restarting,
        SessionEvent::Detached,
        SessionEvent::Resync {
            dropped_event_count: 1,
        },
        SessionEvent::MouseAnswer {
            request_id: 1,
            mouse_answers: Vec::new(),
        },
        SessionEvent::HostWrite {
            host_output_bytes: Vec::new(),
        },
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
            connection_token: ConnectionToken::from_secret("t"),
        },
        RouterRequestKind::CreateSession {
            profile: None,
            working_directory: None,
            is_other_user_access_allowed: None,
        },
        RouterRequestKind::AttachLookup {
            session_selector: crate::router::SessionSelector::SessionName("quiet-lake".to_string()),
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
    let session_address = crate::router::SessionAddress {
        session_id: koshi_core::ids::SessionId::new(),
        session_name: String::new(),
        socket_address: String::new(),
        process_id: 1,
    };
    vec![
        RouterResult::Hello {
            protocol_version: 1,
            build_version: String::new(),
        },
        RouterResult::Created(session_address.clone()),
        RouterResult::Found(session_address),
        RouterResult::Sessions(Vec::new()),
        RouterResult::Restarting,
        RouterResult::Granted {
            connection_token: ConnectionToken::from_secret("t"),
            did_replace_active_grant: false,
        },
        RouterResult::Revoked(Vec::new()),
        RouterResult::Tokens(Vec::new()),
        RouterResult::RemoteStatus {
            remote_listen_address: None,
            is_remote_access_enabled: false,
            is_listening: false,
            certificate_fingerprint: None,
            remote_connection_count: Some(0),
        },
        RouterResult::RemoteEnabled {
            remote_listen_address: String::new(),
            certificate_fingerprint: String::new(),
        },
        RouterResult::Error(crate::protocol::IpcErrorPayload {
            code: crate::protocol::IpcErrorCode::BadToken,
            message: String::new(),
        }),
    ]
}

/// The smallest frame that still holds every record a painted frame needs.
fn build_test_painted_frame() -> crate::frame::PaintedFrame {
    use koshi_core::geometry::Size;
    use koshi_core::ids::{ClientId, SessionId, TabId};

    crate::frame::PaintedFrame {
        session_snapshot: crate::frame::FrameSession {
            session_id: SessionId::new(),
            session_name: String::new(),
            active_tab_snapshot: crate::frame::FrameTab {
                tab_id: TabId::new(),
                tab_name: String::new(),
                pane_slots: Vec::new(),
                effective_cell_size: Size {
                    column_count: 80,
                    row_count: 24,
                },
                stack_headers: Vec::new(),
                layout_mode: koshi_layout::mode::LayoutMode::Tiled,
                is_every_pane_suppressed: false,
                gap_cell_count: 0,
            },
            tab_snapshots: Vec::new(),
        },
        pane_snapshots: Vec::new(),
        client_snapshot: crate::frame::FrameClient {
            client_id: ClientId::new(),
            viewport_size: Size {
                column_count: 80,
                row_count: 24,
            },
            active_tab_id: TabId::new(),
            focused_pane_id: None,
            lock_mode: koshi_core::lock::LockMode::default(),
            is_mouse_selection_enabled: false,
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
            variant_name: "[2JAdded".to_string(),
        }
    );
}

#[test]
fn an_unknown_name_is_cut_to_the_reported_text_cap() {
    let long = "A".repeat(100_000);
    let decoded: MaybeKnown<Sample> =
        serde_json::from_str(&format!(r#"{{"{long}":{{"pane":3}}}}"#)).unwrap();
    let MaybeKnown::Unknown { variant_name } = decoded else {
        panic!("a name this build does not have reads as unknown");
    };
    assert_eq!(
        variant_name.len(),
        koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT
    );
}
