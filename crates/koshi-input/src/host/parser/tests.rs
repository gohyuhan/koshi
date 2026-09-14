//! Tests for host-terminal byte parsing and recovery.

use super::*;

fn drain_pending_events(parser: &mut Parser) -> Vec<Event> {
    let mut drained_events = Vec::new();
    while let Some(pending_event) = parser.remove_next_pending_event() {
        drained_events.push(pending_event);
    }
    drained_events
}

fn build_key_event(key_code: KeyCode, modifiers: Modifiers) -> Event {
    Event::Key(KeyEvent::from_key_code_and_modifiers(key_code, modifiers))
}

fn assert_event_at_every_split(terminal_input_bytes: &[u8], expected_event: Event) {
    for split_byte_count in 0..=terminal_input_bytes.len() {
        let mut parser = Parser::default();
        parser.process_input_bytes(&terminal_input_bytes[..split_byte_count]);
        parser.process_input_bytes(&terminal_input_bytes[split_byte_count..]);
        assert_eq!(
            drain_pending_events(&mut parser),
            std::slice::from_ref(&expected_event),
            "split_byte_count {split_byte_count}"
        );
    }
}

#[test]
fn cell_pixel_report_survives_every_byte_split() {
    assert_event_at_every_split(
        b"\x1b[6;20;10t",
        Event::CellSize(
            koshi_core::geometry::PixelCellSize::from_pixel_dimensions(10, 20).expect("nonzero"),
        ),
    );
}

#[test]
fn invalid_cell_pixel_reports_are_not_input() {
    for terminal_input_bytes in [
        b"\x1b[6;0;10t".as_slice(),
        b"\x1b[6;20;0t",
        b"\x1b[6;65536;10t",
        b"\x1b[6;20;65536t",
        b"\x1b[6;20;10;1t",
        b"\x1b[4;20;10t",
        b"\x1b[6;20t",
    ] {
        let mut parser = Parser::default();
        parser.process_input_bytes(terminal_input_bytes);
        assert_eq!(
            drain_pending_events(&mut parser),
            [],
            "{terminal_input_bytes:?}"
        );
        parser.process_input_bytes(b"a");
        assert_eq!(
            drain_pending_events(&mut parser),
            [build_key_event(KeyCode::Char('a'), Modifiers::NONE)]
        );
    }
}

#[test]
fn text_and_control_bytes_decode_exactly() {
    let mut parser = Parser::default();
    parser.process_input_bytes(b"aA\xc3\xa9\x01\x1c\r\t\x7f\0");
    assert_eq!(
        drain_pending_events(&mut parser),
        vec![
            build_key_event(KeyCode::Char('a'), Modifiers::NONE),
            build_key_event(KeyCode::Char('A'), Modifiers::SHIFT),
            build_key_event(KeyCode::Char('é'), Modifiers::NONE),
            build_key_event(KeyCode::Char('a'), Modifiers::CONTROL),
            build_key_event(KeyCode::Char('4'), Modifiers::CONTROL),
            build_key_event(KeyCode::Enter, Modifiers::NONE),
            build_key_event(KeyCode::Tab, Modifiers::NONE),
            build_key_event(KeyCode::Backspace, Modifiers::NONE),
            build_key_event(KeyCode::Char(' '), Modifiers::CONTROL),
        ]
    );
    assert!(!parser.has_pending_input());
}

#[test]
fn fragmented_utf8_and_escape_sequences_keep_their_bytes() {
    let mut parser = Parser::default();
    for byte in "🐈".as_bytes() {
        parser.process_input_bytes(&[*byte]);
    }
    for byte in b"\x1b[97;5u" {
        parser.process_input_bytes(&[*byte]);
    }
    assert_eq!(
        drain_pending_events(&mut parser),
        vec![
            build_key_event(KeyCode::Char('🐈'), Modifiers::NONE),
            build_key_event(KeyCode::Char('a'), Modifiers::CONTROL),
        ]
    );
}

#[test]
fn incomplete_utf8_does_not_use_the_escape_sequence_timeout() {
    let mut parser = Parser::default();
    parser.process_input_bytes(&[0xc3]);

    assert!(parser.has_pending_input());
    assert!(!parser.needs_input_sequence_timeout());

    parser.process_input_bytes(&[0xa9]);
    assert_eq!(
        drain_pending_events(&mut parser),
        vec![build_key_event(KeyCode::Char('é'), Modifiers::NONE)]
    );
    assert!(!parser.has_pending_input());
}

#[test]
fn escape_timeout_and_alt_input_are_distinct() {
    let mut parser = Parser::default();
    parser.process_input_bytes(b"\x1b");
    assert!(parser.has_pending_input());
    parser.finish_pending_input();
    parser.process_input_bytes(b"\x1bx\x1bH\x1b\xc3\xa9\x1b_");
    parser.finish_pending_input();
    assert_eq!(
        drain_pending_events(&mut parser),
        vec![
            build_key_event(KeyCode::Escape, Modifiers::NONE),
            build_key_event(KeyCode::Char('x'), Modifiers::ALT),
            build_key_event(KeyCode::Char('H'), Modifiers::ALT | Modifiers::SHIFT),
            build_key_event(KeyCode::Char('é'), Modifiers::ALT),
            build_key_event(KeyCode::Char('_'), Modifiers::ALT | Modifiers::SHIFT),
        ]
    );
}

#[test]
fn legacy_named_keys_and_modifiers_decode_exactly() {
    let legacy_key_cases = [
        (
            b"\x1bOA".as_slice(),
            build_key_event(KeyCode::Up, Modifiers::NONE),
        ),
        (
            b"\x1b[Z".as_slice(),
            build_key_event(KeyCode::BackTab, Modifiers::SHIFT),
        ),
        (
            b"\x1b[1;5C".as_slice(),
            build_key_event(KeyCode::Right, Modifiers::CONTROL),
        ),
        (
            b"\x1b[3;4~".as_slice(),
            build_key_event(KeyCode::Delete, Modifiers::SHIFT | Modifiers::ALT),
        ),
        (
            b"\x1b[24~".as_slice(),
            build_key_event(KeyCode::Function(12), Modifiers::NONE),
        ),
        (
            b"\x1b[1;3P".as_slice(),
            build_key_event(KeyCode::Function(1), Modifiers::ALT),
        ),
    ];
    for (terminal_input_bytes, expected_event) in legacy_key_cases {
        let mut parser = Parser::default();
        parser.process_input_bytes(terminal_input_bytes);
        assert_eq!(
            drain_pending_events(&mut parser),
            vec![expected_event],
            "{terminal_input_bytes:?}"
        );
    }
}

#[test]
fn kitty_keys_keep_event_kind_and_six_modifiers() {
    let mut parser = Parser::default();
    parser.process_input_bytes(b"\x1b[97;63:2u\x1b[97;5:3u");
    assert_eq!(
        drain_pending_events(&mut parser),
        vec![
            Event::Key(KeyEvent {
                code: KeyCode::Char('a'),
                key_event_kind: KeyEventKind::Repeat,
                modifiers: Modifiers::CONTROL
                    | Modifiers::ALT
                    | Modifiers::SUPER
                    | Modifiers::HYPER
                    | Modifiers::META,
            }),
            Event::Key(KeyEvent {
                code: KeyCode::Char('a'),
                key_event_kind: KeyEventKind::Release,
                modifiers: Modifiers::CONTROL,
            }),
        ]
    );
}

#[test]
fn kitty_shifted_and_functional_keys_decode_exactly() {
    let mut parser = Parser::default();
    parser.process_input_bytes(b"\x1b[49:33;2u\x1b[57376u\x1b[57387;3u\x1b[57388u\x1b[57414u");
    assert_eq!(
        drain_pending_events(&mut parser),
        vec![
            build_key_event(KeyCode::Char('!'), Modifiers::NONE),
            build_key_event(KeyCode::Function(13), Modifiers::NONE),
            build_key_event(KeyCode::Function(24), Modifiers::ALT),
            build_key_event(KeyCode::Unsupported, Modifiers::NONE),
            build_key_event(KeyCode::Enter, Modifiers::NONE),
        ]
    );
}

#[test]
fn kitty_shifted_keys_survive_every_byte_split() {
    assert_event_at_every_split(
        b"\x1b[49:33;2u",
        build_key_event(KeyCode::Char('!'), Modifiers::NONE),
    );
}

#[test]
fn malformed_keyboard_sequences_are_dropped_and_parsing_recovers() {
    let mut parser = Parser::default();
    parser.process_input_bytes(b"\x1b[97;5:9u\x1b[999999999999999999999u\x1b[1;5Ax");
    assert_eq!(
        drain_pending_events(&mut parser),
        vec![
            build_key_event(KeyCode::Up, Modifiers::CONTROL),
            build_key_event(KeyCode::Char('x'), Modifiers::NONE),
        ]
    );
}

#[test]
fn every_sgr_mouse_action_has_zero_based_coordinates() {
    let sgr_mouse_action_cases = [
        (0, MouseEventKind::Down(MouseButton::Left)),
        (1, MouseEventKind::Down(MouseButton::Middle)),
        (2, MouseEventKind::Down(MouseButton::Right)),
        (32, MouseEventKind::Drag(MouseButton::Left)),
        (35, MouseEventKind::Moved),
        (64, MouseEventKind::ScrollUp),
        (65, MouseEventKind::ScrollDown),
        (66, MouseEventKind::ScrollLeft),
        (67, MouseEventKind::ScrollRight),
    ];
    for (mouse_button_code, mouse_event_kind) in sgr_mouse_action_cases {
        let mut parser = Parser::default();
        parser.process_input_bytes(format!("\x1b[<{mouse_button_code};11;4M").as_bytes());
        assert_eq!(
            drain_pending_events(&mut parser),
            vec![Event::Mouse(MouseEvent {
                mouse_event_kind,
                column: 10,
                row: 3,
                modifiers: Modifiers::NONE,
            })],
            "button code {mouse_button_code}"
        );
    }
}

#[test]
fn sgr_release_modifiers_and_full_coordinates_decode_exactly() {
    let mut parser = Parser::default();
    parser.process_input_bytes(b"\x1b[<29;65535;65535m");
    assert_eq!(
        drain_pending_events(&mut parser),
        vec![Event::Mouse(MouseEvent {
            mouse_event_kind: MouseEventKind::Up(MouseButton::Middle),
            column: 65_534,
            row: 65_534,
            modifiers: Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL,
        })]
    );
}

#[test]
fn x10_and_rxvt_mouse_forms_decode_exactly() {
    let mut parser = Parser::default();
    parser.process_input_bytes(b"\x1b[M *%\x1b[32;11;4M");
    assert_eq!(
        drain_pending_events(&mut parser),
        vec![
            Event::Mouse(MouseEvent {
                mouse_event_kind: MouseEventKind::Down(MouseButton::Left),
                column: 9,
                row: 4,
                modifiers: Modifiers::NONE,
            }),
            Event::Mouse(MouseEvent {
                mouse_event_kind: MouseEventKind::Down(MouseButton::Left),
                column: 10,
                row: 3,
                modifiers: Modifiers::NONE,
            }),
        ]
    );
}

#[test]
fn mouse_forms_reject_coordinates_before_the_first_cell() {
    let malformed_mouse_sequence_cases = [
        b"\x1b[<0;0;1Mx".as_slice(),
        b"\x1b[32;0;1Mx".as_slice(),
        b"\x1b[M   x".as_slice(),
    ];
    for terminal_input_bytes in malformed_mouse_sequence_cases {
        let mut parser = Parser::default();
        parser.process_input_bytes(terminal_input_bytes);
        assert_eq!(
            drain_pending_events(&mut parser),
            vec![build_key_event(KeyCode::Char('x'), Modifiers::NONE)],
            "{terminal_input_bytes:?}"
        );
    }
}

#[test]
fn bracketed_paste_keeps_partial_markers_and_invalid_utf8() {
    let mut parser = Parser::default();
    parser.process_input_bytes(b"\x1b[200~one\x1b[20");
    parser.process_input_bytes(b"x\xff\x1b[201~");
    assert_eq!(
        drain_pending_events(&mut parser),
        vec![Event::Paste("one\x1b[20x�".to_string())]
    );
}

#[test]
fn oversized_paste_is_discarded_and_the_next_key_survives() {
    let mut parser = Parser::default();
    parser.process_input_bytes(b"\x1b[200~");
    parser.process_input_bytes(&vec![b'x'; MAX_PASTE_BYTE_COUNT + 1]);
    parser.process_input_bytes(b"\x1b[201~z");
    assert_eq!(
        drain_pending_events(&mut parser),
        vec![build_key_event(KeyCode::Char('z'), Modifiers::NONE)]
    );
}

#[test]
fn device_and_kitty_answers_decode_exactly() {
    let mut parser = Parser::default();
    parser.process_input_bytes(b"\x1b[?1;2c\x1b_Gi=31;OK\x1b\\\x1b_Gi=32;EINVAL:bad size\x1b\\");
    assert_eq!(
        drain_pending_events(&mut parser),
        vec![
            Event::PrimaryDeviceAttributes(vec![1, 2]),
            Event::KittyGraphicsReply(KittyGraphicsReply {
                image_id: 31,
                is_successful: true,
            }),
            Event::KittyGraphicsReply(KittyGraphicsReply {
                image_id: 32,
                is_successful: false,
            }),
        ]
    );
}

#[test]
fn primary_device_attributes_preserve_all_parameters_at_every_byte_split() {
    assert_event_at_every_split(
        b"\x1b[?1;2;4c",
        Event::PrimaryDeviceAttributes(vec![1, 2, 4]),
    );
}

#[test]
fn malformed_primary_device_attributes_are_dropped_and_next_key_survives() {
    for terminal_input_bytes in [
        b"\x1b[?c".as_slice(),
        b"\x1b[?1;;2c",
        b"\x1b[?;1c",
        b"\x1b[?1;2;c",
        b"\x1b[?4294967296c",
        b"\x1b[?1;2:c",
    ] {
        let mut parser = Parser::default();
        parser.process_input_bytes(terminal_input_bytes);
        parser.process_input_bytes(b"x");
        assert_eq!(
            drain_pending_events(&mut parser),
            vec![build_key_event(KeyCode::Char('x'), Modifiers::NONE)],
            "{terminal_input_bytes:?}"
        );
    }
}

#[test]
fn sixel_attribute_success_replies_keep_exact_nonzero_values_at_every_split() {
    assert_event_at_every_split(
        b"\x1b[?1;0;256S",
        Event::SixelGraphicsAttributeReply(GraphicAttributeReply::Palette(Ok(256))),
    );
    assert_event_at_every_split(
        b"\x1b[?2;0;640;480S",
        Event::SixelGraphicsAttributeReply(GraphicAttributeReply::Geometry(Ok((640, 480)))),
    );
    assert_event_at_every_split(
        b"\x1b[?2;0;0;0S",
        Event::SixelGraphicsAttributeReply(GraphicAttributeReply::Geometry(Ok((0, 0)))),
    );
}

#[test]
fn sixel_attribute_status_replies_keep_item_and_error_without_values() {
    let sixel_attribute_status_cases = [
        (
            b"\x1b[?1;1S".as_slice(),
            GraphicAttributeReply::Palette(Err(GraphicAttributeError::InvalidItem)),
        ),
        (
            b"\x1b[?1;2S".as_slice(),
            GraphicAttributeReply::Palette(Err(GraphicAttributeError::InvalidAction)),
        ),
        (
            b"\x1b[?2;3S".as_slice(),
            GraphicAttributeReply::Geometry(Err(GraphicAttributeError::Failure)),
        ),
    ];
    for (terminal_input_bytes, reply) in sixel_attribute_status_cases {
        assert_event_at_every_split(
            terminal_input_bytes,
            Event::SixelGraphicsAttributeReply(reply),
        );
    }
}

#[test]
fn malformed_sixel_attribute_replies_are_dropped_and_next_key_survives() {
    for terminal_input_bytes in [
        b"\x1b[?3;0;256S".as_slice(),
        b"\x1b[?1;0;0S",
        b"\x1b[?1;0;1;2S",
        b"\x1b[?2;0;640S",
        b"\x1b[?1;4;1S",
        b"\x1b[?1;0;4294967296S",
    ] {
        let mut parser = Parser::default();
        parser.process_input_bytes(terminal_input_bytes);
        parser.process_input_bytes(b"x");
        assert_eq!(
            drain_pending_events(&mut parser),
            vec![build_key_event(KeyCode::Char('x'), Modifiers::NONE)],
            "{terminal_input_bytes:?}"
        );
    }
}

#[test]
fn iterm_capability_replies_support_bel_st_and_c1_st_at_every_split() {
    for terminator in [b"\x07".as_slice(), b"\x1b\\", b"\x9c"] {
        let mut terminal_input_bytes = b"\x1b]1337;Capabilities=FSx".to_vec();
        terminal_input_bytes.extend_from_slice(terminator);
        assert_event_at_every_split(
            &terminal_input_bytes,
            Event::TerminalFeatures(b"FSx".to_vec()),
        );
    }
}

#[test]
fn iterm_capability_replies_do_not_leak_into_keys_or_paste() {
    let mut parser = Parser::default();
    parser.process_input_bytes(b"x\x1b]1337;Capabilities=F\x07y\x1b[200~paste\x1b[201~z");
    assert_eq!(
        drain_pending_events(&mut parser),
        vec![
            build_key_event(KeyCode::Char('x'), Modifiers::NONE),
            Event::TerminalFeatures(b"F".to_vec()),
            build_key_event(KeyCode::Char('y'), Modifiers::NONE),
            Event::Paste("paste".to_string()),
            build_key_event(KeyCode::Char('z'), Modifiers::NONE),
        ]
    );
}

#[test]
fn cancelled_and_unrelated_osc_sequences_are_discarded_and_recover() {
    for cancellation in [0x18, 0x1a] {
        let mut parser = Parser::default();
        parser.process_input_bytes(b"\x1b]1337;Capabilities=F");
        parser.process_input_bytes(&[cancellation, b'x']);
        assert_eq!(
            drain_pending_events(&mut parser),
            vec![build_key_event(KeyCode::Char('x'), Modifiers::NONE)],
            "cancellation {cancellation:#x}"
        );
    }
    for terminator in [b"\x07".as_slice(), b"\x1b\\", b"\x9c"] {
        let mut parser = Parser::default();
        parser.process_input_bytes(b"\x1b]0;window title");
        parser.process_input_bytes(terminator);
        parser.process_input_bytes(b"x");
        assert_eq!(
            drain_pending_events(&mut parser),
            vec![build_key_event(KeyCode::Char('x'), Modifiers::NONE)],
            "terminator {terminator:?}"
        );
    }
}

#[test]
fn oversized_osc_is_discarded_until_terminator_and_next_key_survives() {
    let mut terminal_input_bytes = b"\x1b]1337;Capabilities=".to_vec();
    terminal_input_bytes.extend(std::iter::repeat_n(b'x', MAX_CONTROL_STRING_BYTE_COUNT + 1));
    let mut parser = Parser::default();
    parser.process_input_bytes(&terminal_input_bytes);
    assert!(parser.has_pending_input());
    assert!(!parser.needs_input_sequence_timeout());
    parser.finish_pending_input();
    assert!(parser.has_pending_input());
    parser.process_input_bytes(b"\x07z");
    assert_eq!(
        drain_pending_events(&mut parser),
        vec![build_key_event(KeyCode::Char('z'), Modifiers::NONE)]
    );
}

#[test]
fn oversized_apc_and_dcs_stay_pending_until_their_string_terminator() {
    for prefix in [b"\x1b_G".as_slice(), b"\x1bP"] {
        let mut terminal_input_bytes = prefix.to_vec();
        terminal_input_bytes.extend(std::iter::repeat_n(b'x', MAX_CONTROL_STRING_BYTE_COUNT + 1));
        let mut parser = Parser::default();
        parser.process_input_bytes(&terminal_input_bytes);
        assert!(parser.has_pending_input(), "prefix {prefix:?}");
        assert!(!parser.needs_input_sequence_timeout(), "prefix {prefix:?}");
        parser.finish_pending_input();
        assert!(parser.has_pending_input(), "prefix {prefix:?}");
        parser.process_input_bytes(b"\x1b\\z");
        assert_eq!(
            drain_pending_events(&mut parser),
            vec![build_key_event(KeyCode::Char('z'), Modifiers::NONE)],
            "prefix {prefix:?}"
        );
    }
}

#[test]
fn malformed_and_oversized_control_strings_recover_at_their_terminator() {
    let mut parser = Parser::default();
    parser.process_input_bytes(b"\x1b]0;ignored\x07\x1bPignored\x1b\\");
    parser.process_input_bytes(b"\x1b_Gi=31;");
    parser.process_input_bytes(&vec![b'x'; MAX_CONTROL_STRING_BYTE_COUNT]);
    parser.process_input_bytes(b"\x1b\\q");
    assert_eq!(
        drain_pending_events(&mut parser),
        vec![build_key_event(KeyCode::Char('q'), Modifiers::NONE)]
    );
}

#[test]
fn identified_control_strings_survive_finish_pending_until_their_terminator() {
    let mut oversized_csi = b"\x1b[".to_vec();
    oversized_csi.extend(std::iter::repeat_n(b'1', MAX_CSI_BYTE_COUNT));
    let identified_control_sequence_cases = [
        (b"\x1b]0;title".to_vec(), b"\x1b\\".as_slice()),
        (b"\x1bPignored".to_vec(), b"\x1b\\".as_slice()),
        (b"\x1b_Gi=31;OK".to_vec(), b"\x1b\\".as_slice()),
        (oversized_csi, b"A".as_slice()),
    ];
    for (terminal_input_bytes, terminator) in identified_control_sequence_cases {
        let mut parser = Parser::default();
        parser.process_input_bytes(&terminal_input_bytes);

        assert!(parser.has_pending_input(), "{terminal_input_bytes:?}");
        assert!(
            !parser.needs_input_sequence_timeout(),
            "{terminal_input_bytes:?}"
        );
        parser.finish_pending_input();
        assert!(parser.has_pending_input(), "{terminal_input_bytes:?}");
        parser.process_input_bytes(terminator);
        parser.process_input_bytes(b"x");

        let expected_event = if terminal_input_bytes.starts_with(b"\x1b_G") {
            vec![
                Event::KittyGraphicsReply(KittyGraphicsReply {
                    image_id: 31,
                    is_successful: true,
                }),
                build_key_event(KeyCode::Char('x'), Modifiers::NONE),
            ]
        } else {
            vec![build_key_event(KeyCode::Char('x'), Modifiers::NONE)]
        };
        assert_eq!(
            drain_pending_events(&mut parser),
            expected_event,
            "{terminal_input_bytes:?}"
        );
        assert!(!parser.has_pending_input(), "{terminal_input_bytes:?}");
    }
}

#[test]
fn ambiguous_key_prefixes_still_use_the_sequence_timeout() {
    for terminal_input_bytes in [b"\x1b".as_slice(), b"\x1bO", b"\x1b[", b"\x1b[M", b"\x1b_"] {
        let mut parser = Parser::default();
        parser.process_input_bytes(terminal_input_bytes);
        assert!(parser.has_pending_input(), "{terminal_input_bytes:?}");
        assert!(
            parser.needs_input_sequence_timeout(),
            "{terminal_input_bytes:?}"
        );
        parser.finish_pending_input();
        assert!(!parser.has_pending_input(), "{terminal_input_bytes:?}");
        parser.process_input_bytes(b"x");
        let expected_event = match terminal_input_bytes {
            b"\x1b" => vec![
                build_key_event(KeyCode::Escape, Modifiers::NONE),
                build_key_event(KeyCode::Char('x'), Modifiers::NONE),
            ],
            b"\x1b_" => vec![
                build_key_event(KeyCode::Char('_'), Modifiers::ALT | Modifiers::SHIFT),
                build_key_event(KeyCode::Char('x'), Modifiers::NONE),
            ],
            _ => vec![build_key_event(KeyCode::Char('x'), Modifiers::NONE)],
        };
        assert_eq!(
            drain_pending_events(&mut parser),
            expected_event,
            "{terminal_input_bytes:?}"
        );
    }
}

#[test]
fn capability_reply_survives_timeout_and_cancellation_without_key_leak() {
    let prefix = b"\x1b]1337;Capabilities=F";
    for split_byte_count in 0..=prefix.len() {
        let mut parser = Parser::default();
        parser.process_input_bytes(&prefix[..split_byte_count]);
        parser.process_input_bytes(&prefix[split_byte_count..]);
        assert!(
            parser.has_pending_input(),
            "split_byte_count {split_byte_count}"
        );
        assert!(
            !parser.needs_input_sequence_timeout(),
            "split_byte_count {split_byte_count}"
        );
        parser.finish_pending_input();
        assert!(
            parser.has_pending_input(),
            "split_byte_count {split_byte_count}"
        );
        parser.process_input_bytes(b"\x1b\\x");
        assert_eq!(
            drain_pending_events(&mut parser),
            vec![
                Event::TerminalFeatures(b"F".to_vec()),
                build_key_event(KeyCode::Char('x'), Modifiers::NONE),
            ],
            "split_byte_count {split_byte_count}"
        );
    }

    for cancellation in [0x18, 0x1a] {
        for split_byte_count in 0..=prefix.len() {
            let mut parser = Parser::default();
            parser.process_input_bytes(&prefix[..split_byte_count]);
            parser.process_input_bytes(&prefix[split_byte_count..]);
            parser.finish_pending_input();
            parser.process_input_bytes(&[cancellation, b'x']);
            assert_eq!(
                drain_pending_events(&mut parser),
                vec![build_key_event(KeyCode::Char('x'), Modifiers::NONE)],
                "cancellation {cancellation:#x}, split_byte_count {split_byte_count}"
            );
        }
    }
}

#[test]
fn cancelled_apc_and_dcs_strings_stay_silent_and_recover() {
    for prefix in [b"\x1b_Gi=31;OK".as_slice(), b"\x1bPignored"] {
        for cancellation in [0x18, 0x1a] {
            let mut parser = Parser::default();
            parser.process_input_bytes(prefix);
            assert!(parser.has_pending_input(), "prefix {prefix:?}");
            assert!(!parser.needs_input_sequence_timeout(), "prefix {prefix:?}");
            parser.finish_pending_input();
            assert!(parser.has_pending_input(), "prefix {prefix:?}");
            parser.process_input_bytes(&[cancellation, b'x']);
            assert_eq!(
                drain_pending_events(&mut parser),
                vec![build_key_event(KeyCode::Char('x'), Modifiers::NONE)],
                "prefix {prefix:?}, cancellation {cancellation:#x}"
            );
        }
    }
}

#[test]
fn private_csi_reply_survives_timeout_until_its_final_byte() {
    let prefix = b"\x1b[?2;0;";
    for split_byte_count in 0..=prefix.len() {
        let mut parser = Parser::default();
        parser.process_input_bytes(&prefix[..split_byte_count]);
        parser.process_input_bytes(&prefix[split_byte_count..]);
        assert!(
            parser.has_pending_input(),
            "split_byte_count {split_byte_count}"
        );
        assert!(
            !parser.needs_input_sequence_timeout(),
            "split_byte_count {split_byte_count}"
        );
        parser.finish_pending_input();
        assert!(
            parser.has_pending_input(),
            "split_byte_count {split_byte_count}"
        );
        parser.process_input_bytes(b"0;0Sx");
        assert_eq!(
            drain_pending_events(&mut parser),
            vec![
                Event::SixelGraphicsAttributeReply(GraphicAttributeReply::Geometry(Ok((0, 0)))),
                build_key_event(KeyCode::Char('x'), Modifiers::NONE),
            ],
            "split_byte_count {split_byte_count}"
        );
    }
}

#[test]
fn private_csi_reply_cancellation_discards_its_suffix() {
    for cancellation in [0x18, 0x1a] {
        let mut parser = Parser::default();
        parser.process_input_bytes(b"\x1b[?2;0;");
        assert!(!parser.needs_input_sequence_timeout());
        parser.finish_pending_input();
        parser.process_input_bytes(&[cancellation, b'x']);
        assert_eq!(
            drain_pending_events(&mut parser),
            vec![build_key_event(KeyCode::Char('x'), Modifiers::NONE)],
            "cancellation {cancellation:#x}"
        );
    }
}

#[test]
fn private_csi_reply_escape_starts_the_next_control_string() {
    let mut parser = Parser::default();
    parser.process_input_bytes(b"\x1b[?2;0;");
    parser.finish_pending_input();
    parser.process_input_bytes(b"\x1b]0;title\x07x");

    assert_eq!(
        drain_pending_events(&mut parser),
        vec![build_key_event(KeyCode::Char('x'), Modifiers::NONE)]
    );
}

#[test]
fn oversized_csi_recovers_after_its_final_byte() {
    let mut parser = Parser::default();
    parser.process_input_bytes(b"\x1b[");
    parser.process_input_bytes(&[b'1'; MAX_CSI_BYTE_COUNT]);
    parser.process_input_bytes(b"Ax");
    assert_eq!(
        drain_pending_events(&mut parser),
        vec![build_key_event(KeyCode::Char('x'), Modifiers::NONE)]
    );
}

#[test]
fn focus_and_non_kitty_apc_input_decode_without_cross_talk() {
    let mut parser = Parser::default();
    parser.process_input_bytes(b"\x1b[I\x1b[O\x1b_x");
    assert_eq!(
        drain_pending_events(&mut parser),
        vec![
            Event::FocusIn,
            Event::FocusOut,
            build_key_event(KeyCode::Char('_'), Modifiers::ALT | Modifiers::SHIFT),
            build_key_event(KeyCode::Char('x'), Modifiers::NONE),
        ]
    );
}

#[test]
fn invalid_utf8_does_not_consume_the_next_ascii_key() {
    let mut parser = Parser::default();
    parser.process_input_bytes(&[0xf0, b'x']);
    parser.finish_pending_input();
    assert_eq!(
        drain_pending_events(&mut parser),
        vec![build_key_event(KeyCode::Char('x'), Modifiers::NONE)]
    );
}
