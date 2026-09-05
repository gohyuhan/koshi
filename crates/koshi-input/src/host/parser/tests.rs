//! Tests for host-terminal byte parsing and recovery.

use super::*;

fn events(parser: &mut Parser) -> Vec<Event> {
    let mut events = Vec::new();
    while let Some(event) = parser.pop() {
        events.push(event);
    }
    events
}

fn key(code: KeyCode, modifiers: Modifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

#[test]
fn text_and_control_bytes_decode_exactly() {
    let mut parser = Parser::default();
    parser.push(b"aA\xc3\xa9\x01\x1c\r\t\x7f\0");
    assert_eq!(
        events(&mut parser),
        vec![
            key(KeyCode::Char('a'), Modifiers::NONE),
            key(KeyCode::Char('A'), Modifiers::SHIFT),
            key(KeyCode::Char('é'), Modifiers::NONE),
            key(KeyCode::Char('a'), Modifiers::CONTROL),
            key(KeyCode::Char('4'), Modifiers::CONTROL),
            key(KeyCode::Enter, Modifiers::NONE),
            key(KeyCode::Tab, Modifiers::NONE),
            key(KeyCode::Backspace, Modifiers::NONE),
            key(KeyCode::Char(' '), Modifiers::CONTROL),
        ]
    );
    assert!(!parser.has_pending());
}

#[test]
fn fragmented_utf8_and_escape_sequences_keep_their_bytes() {
    let mut parser = Parser::default();
    for byte in "🐈".as_bytes() {
        parser.push(&[*byte]);
    }
    for byte in b"\x1b[97;5u" {
        parser.push(&[*byte]);
    }
    assert_eq!(
        events(&mut parser),
        vec![
            key(KeyCode::Char('🐈'), Modifiers::NONE),
            key(KeyCode::Char('a'), Modifiers::CONTROL),
        ]
    );
}

#[test]
fn incomplete_utf8_does_not_use_the_escape_sequence_timeout() {
    let mut parser = Parser::default();
    parser.push(&[0xc3]);

    assert!(parser.has_pending());
    assert!(!parser.needs_sequence_timeout());

    parser.push(&[0xa9]);
    assert_eq!(
        events(&mut parser),
        vec![key(KeyCode::Char('é'), Modifiers::NONE)]
    );
    assert!(!parser.has_pending());
}

#[test]
fn escape_timeout_and_alt_input_are_distinct() {
    let mut parser = Parser::default();
    parser.push(b"\x1b");
    assert!(parser.has_pending());
    parser.finish_pending();
    parser.push(b"\x1bx\x1bH\x1b\xc3\xa9\x1b_");
    parser.finish_pending();
    assert_eq!(
        events(&mut parser),
        vec![
            key(KeyCode::Escape, Modifiers::NONE),
            key(KeyCode::Char('x'), Modifiers::ALT),
            key(KeyCode::Char('H'), Modifiers::ALT | Modifiers::SHIFT),
            key(KeyCode::Char('é'), Modifiers::ALT),
            key(KeyCode::Char('_'), Modifiers::ALT | Modifiers::SHIFT),
        ]
    );
}

#[test]
fn legacy_named_keys_and_modifiers_decode_exactly() {
    let cases = [
        (b"\x1bOA".as_slice(), key(KeyCode::Up, Modifiers::NONE)),
        (
            b"\x1b[Z".as_slice(),
            key(KeyCode::BackTab, Modifiers::SHIFT),
        ),
        (
            b"\x1b[1;5C".as_slice(),
            key(KeyCode::Right, Modifiers::CONTROL),
        ),
        (
            b"\x1b[3;4~".as_slice(),
            key(KeyCode::Delete, Modifiers::SHIFT | Modifiers::ALT),
        ),
        (
            b"\x1b[24~".as_slice(),
            key(KeyCode::Function(12), Modifiers::NONE),
        ),
        (
            b"\x1b[1;3P".as_slice(),
            key(KeyCode::Function(1), Modifiers::ALT),
        ),
    ];
    for (bytes, expected) in cases {
        let mut parser = Parser::default();
        parser.push(bytes);
        assert_eq!(events(&mut parser), vec![expected], "{bytes:?}");
    }
}

#[test]
fn kitty_keys_keep_event_kind_and_six_modifiers() {
    let mut parser = Parser::default();
    parser.push(b"\x1b[97;63:2u\x1b[97;5:3u");
    assert_eq!(
        events(&mut parser),
        vec![
            Event::Key(KeyEvent {
                code: KeyCode::Char('a'),
                kind: KeyEventKind::Repeat,
                modifiers: Modifiers::CONTROL
                    | Modifiers::ALT
                    | Modifiers::SUPER
                    | Modifiers::HYPER
                    | Modifiers::META,
            }),
            Event::Key(KeyEvent {
                code: KeyCode::Char('a'),
                kind: KeyEventKind::Release,
                modifiers: Modifiers::CONTROL,
            }),
        ]
    );
}

#[test]
fn kitty_shifted_and_functional_keys_decode_exactly() {
    let mut parser = Parser::default();
    parser.push(b"\x1b[49:33;2u\x1b[57376u\x1b[57387;3u\x1b[57388u\x1b[57414u");
    assert_eq!(
        events(&mut parser),
        vec![
            key(KeyCode::Char('!'), Modifiers::NONE),
            key(KeyCode::Function(13), Modifiers::NONE),
            key(KeyCode::Function(24), Modifiers::ALT),
            key(KeyCode::Unsupported, Modifiers::NONE),
            key(KeyCode::Enter, Modifiers::NONE),
        ]
    );
}

#[test]
fn malformed_keyboard_sequences_are_dropped_and_parsing_recovers() {
    let mut parser = Parser::default();
    parser.push(b"\x1b[97;5:9u\x1b[999999999999999999999u\x1b[1;5Ax");
    assert_eq!(
        events(&mut parser),
        vec![
            key(KeyCode::Up, Modifiers::CONTROL),
            key(KeyCode::Char('x'), Modifiers::NONE),
        ]
    );
}

#[test]
fn every_sgr_mouse_action_has_zero_based_coordinates() {
    let cases = [
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
    for (code, kind) in cases {
        let mut parser = Parser::default();
        parser.push(format!("\x1b[<{code};11;4M").as_bytes());
        assert_eq!(
            events(&mut parser),
            vec![Event::Mouse(MouseEvent {
                kind,
                column: 10,
                row: 3,
                modifiers: Modifiers::NONE,
            })],
            "button code {code}"
        );
    }
}

#[test]
fn sgr_release_modifiers_and_full_coordinates_decode_exactly() {
    let mut parser = Parser::default();
    parser.push(b"\x1b[<29;65535;65535m");
    assert_eq!(
        events(&mut parser),
        vec![Event::Mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Middle),
            column: 65_534,
            row: 65_534,
            modifiers: Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL,
        })]
    );
}

#[test]
fn x10_and_rxvt_mouse_forms_decode_exactly() {
    let mut parser = Parser::default();
    parser.push(b"\x1b[M *%\x1b[32;11;4M");
    assert_eq!(
        events(&mut parser),
        vec![
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 9,
                row: 4,
                modifiers: Modifiers::NONE,
            }),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 10,
                row: 3,
                modifiers: Modifiers::NONE,
            }),
        ]
    );
}

#[test]
fn mouse_forms_reject_coordinates_before_the_first_cell() {
    let cases = [
        b"\x1b[<0;0;1Mx".as_slice(),
        b"\x1b[32;0;1Mx".as_slice(),
        b"\x1b[M   x".as_slice(),
    ];
    for bytes in cases {
        let mut parser = Parser::default();
        parser.push(bytes);
        assert_eq!(
            events(&mut parser),
            vec![key(KeyCode::Char('x'), Modifiers::NONE)],
            "{bytes:?}"
        );
    }
}

#[test]
fn bracketed_paste_keeps_partial_markers_and_invalid_utf8() {
    let mut parser = Parser::default();
    parser.push(b"\x1b[200~one\x1b[20");
    parser.push(b"x\xff\x1b[201~");
    assert_eq!(
        events(&mut parser),
        vec![Event::Paste("one\x1b[20x�".to_string())]
    );
}

#[test]
fn oversized_paste_is_discarded_and_the_next_key_survives() {
    let mut parser = Parser::default();
    parser.push(b"\x1b[200~");
    parser.push(&vec![b'x'; PASTE_LIMIT + 1]);
    parser.push(b"\x1b[201~z");
    assert_eq!(
        events(&mut parser),
        vec![key(KeyCode::Char('z'), Modifiers::NONE)]
    );
}

#[test]
fn device_and_kitty_answers_decode_exactly() {
    let mut parser = Parser::default();
    parser.push(b"\x1b[?1;2c\x1b_Gi=31;OK\x1b\\\x1b_Gi=32;EINVAL:bad size\x1b\\");
    assert_eq!(
        events(&mut parser),
        vec![
            Event::PrimaryDeviceAttributes,
            Event::KittyGraphicsReply(KittyGraphicsReply {
                image_id: 31,
                ok: true,
            }),
            Event::KittyGraphicsReply(KittyGraphicsReply {
                image_id: 32,
                ok: false,
            }),
        ]
    );
}

#[test]
fn malformed_and_oversized_control_strings_recover_at_their_terminator() {
    let mut parser = Parser::default();
    parser.push(b"\x1b]0;ignored\x07\x1bPignored\x1b\\");
    parser.push(b"\x1b_Gi=31;");
    parser.push(&vec![b'x'; CONTROL_STRING_LIMIT]);
    parser.push(b"\x1b\\q");
    assert_eq!(
        events(&mut parser),
        vec![key(KeyCode::Char('q'), Modifiers::NONE)]
    );
}

#[test]
fn unterminated_and_discarded_sequences_use_the_timeout_and_release_the_next_key() {
    let mut oversized_csi = b"\x1b[".to_vec();
    oversized_csi.extend(std::iter::repeat_n(b'1', CSI_LIMIT));
    for bytes in [
        b"\x1b]0;title".to_vec(),
        b"\x1bPignored".to_vec(),
        b"\x1b_Gi=31;OK".to_vec(),
        oversized_csi,
    ] {
        let mut parser = Parser::default();
        parser.push(&bytes);

        assert!(parser.has_pending(), "{bytes:?}");
        assert!(parser.needs_sequence_timeout(), "{bytes:?}");
        parser.finish_pending();
        parser.push(b"x");

        assert_eq!(
            events(&mut parser),
            vec![key(KeyCode::Char('x'), Modifiers::NONE)],
            "{bytes:?}"
        );
        assert!(!parser.has_pending(), "{bytes:?}");
    }
}

#[test]
fn oversized_csi_recovers_after_its_final_byte() {
    let mut parser = Parser::default();
    parser.push(b"\x1b[");
    parser.push(&[b'1'; CSI_LIMIT]);
    parser.push(b"Ax");
    assert_eq!(
        events(&mut parser),
        vec![key(KeyCode::Char('x'), Modifiers::NONE)]
    );
}

#[test]
fn focus_and_non_kitty_apc_input_decode_without_cross_talk() {
    let mut parser = Parser::default();
    parser.push(b"\x1b[I\x1b[O\x1b_x");
    assert_eq!(
        events(&mut parser),
        vec![
            Event::FocusIn,
            Event::FocusOut,
            key(KeyCode::Char('_'), Modifiers::ALT | Modifiers::SHIFT),
            key(KeyCode::Char('x'), Modifiers::NONE),
        ]
    );
}

#[test]
fn invalid_utf8_does_not_consume_the_next_ascii_key() {
    let mut parser = Parser::default();
    parser.push(&[0xf0, b'x']);
    parser.finish_pending();
    assert_eq!(
        events(&mut parser),
        vec![key(KeyCode::Char('x'), Modifiers::NONE)]
    );
}
