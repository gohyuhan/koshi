//! Tests for the OSC 9, OSC 777 and OSC 99 desktop notification parsers.

use super::*;

fn build_notify(title: Option<&str>, body: &str, notification_id: Option<&str>) -> ProgramFact {
    ProgramFact::Notify {
        title: title.map(str::to_string),
        body: body.to_string(),
        notification_id: notification_id.map(str::to_string),
    }
}

fn apply_osc99_payloads(
    open_osc99_notification: &mut Option<Osc99Notification>,
    osc99_metadata_bytes: &[u8],
    notification_payload_bytes: &[u8],
) -> Option<ProgramFact> {
    let mut argument_bytes = osc99_metadata_bytes.to_vec();
    argument_bytes.push(b';');
    argument_bytes.extend_from_slice(notification_payload_bytes);
    apply_osc99_sequence(open_osc99_notification, &argument_bytes)
}

#[test]
fn osc9_text_is_a_notification_body_with_its_semicolons_kept() {
    assert_eq!(
        parse_osc9_notification(b"hello"),
        Some(build_notify(None, "hello", None))
    );
    assert_eq!(
        parse_osc9_notification(b"a;b"),
        Some(build_notify(None, "a;b", None))
    );
    assert_eq!(
        parse_osc9_notification(b""),
        Some(build_notify(None, "", None))
    );
    assert_eq!(
        parse_osc9_notification(b"13"),
        Some(build_notify(None, "13", None))
    );
    assert_eq!(
        parse_osc9_notification(b"04"),
        Some(build_notify(None, "04", None))
    );
}

#[test]
fn osc9_conemu_subcommands_one_to_twelve_are_not_notifications() {
    for subcommand_number in 1..=12 {
        let subcommand_text = subcommand_number.to_string();
        assert_eq!(
            parse_osc9_notification(subcommand_text.as_bytes()),
            None,
            "subcommand {subcommand_number}"
        );
    }
    assert_eq!(parse_osc9_notification(b"4;1;40"), None);
    assert_eq!(
        parse_osc9_notification(b"9;\"C:\\Users\\user\\project\""),
        None
    );
}

#[test]
fn osc777_notify_splits_the_title_from_the_body() {
    assert_eq!(
        parse_osc777_notification(b"notify;Build;done;3 s"),
        Some(build_notify(Some("Build"), "done;3 s", None))
    );
    assert_eq!(
        parse_osc777_notification(b"notify;Build"),
        Some(build_notify(Some("Build"), "", None))
    );
    assert_eq!(
        parse_osc777_notification(b"notify;;body"),
        Some(build_notify(None, "body", None))
    );
    assert_eq!(
        parse_osc777_notification(b"notify"),
        Some(build_notify(None, "", None))
    );
}

#[test]
fn osc777_without_the_notify_subcommand_is_not_a_notification() {
    assert_eq!(parse_osc777_notification(b"preexec;ls"), None);
    assert_eq!(parse_osc777_notification(b"Notify;T"), None);
}

#[test]
fn notification_text_is_sanitized_and_cut_to_512_bytes() {
    let long_body = "b".repeat(5000);
    assert_eq!(
        parse_osc777_notification(format!("notify;Build\u{202E}gpj;{long_body}").as_bytes()),
        Some(build_notify(Some("Buildgpj"), &"b".repeat(512), None))
    );
    assert_eq!(
        parse_osc777_notification("notify;\u{202E};x".as_bytes()),
        Some(build_notify(None, "x", None))
    );
    assert_eq!(
        parse_osc9_notification(b"a\xffb"),
        Some(build_notify(None, "a\u{FFFD}b", None))
    );
}

#[test]
fn osc99_single_sequence_with_no_keys_is_a_title() {
    let mut open_osc99_notification = None;
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"", b"Hello world"),
        Some(build_notify(Some("Hello world"), "", None))
    );
    assert_eq!(open_osc99_notification, None);
}

#[test]
fn osc99_parts_join_by_id_until_a_done_part() {
    let mut open_osc99_notification = None;
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"i=1:d=0", b"Hello world"),
        None
    );
    assert_eq!(
        open_osc99_notification,
        Some(Osc99Notification {
            notification_id: Some("1".to_string()),
            title: "Hello world".to_string(),
            body: String::new(),
        })
    );
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"i=1:p=body", b"This is cool"),
        Some(build_notify(Some("Hello world"), "This is cool", Some("1")))
    );
    assert_eq!(open_osc99_notification, None);
}

#[test]
fn osc99_repeated_title_parts_are_joined() {
    let mut open_osc99_notification = None;
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"i=a:d=0", b"Hel"),
        None
    );
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"i=a:d=0", b"lo"),
        None
    );
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"i=a", b""),
        Some(build_notify(Some("Hello"), "", Some("a")))
    );
}

#[test]
fn osc99_parts_with_no_id_join_each_other() {
    let mut open_osc99_notification = None;
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"d=0", b"Hello"),
        None
    );
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"p=body", b"World"),
        Some(build_notify(Some("Hello"), "World", None))
    );
}

#[test]
fn osc99_part_with_another_id_drops_the_open_notification() {
    let mut open_osc99_notification = None;
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"i=1:d=0", b"A"),
        None
    );
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"i=2:p=body", b"B"),
        Some(build_notify(None, "B", Some("2")))
    );
    assert_eq!(open_osc99_notification, None);

    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"i=1:d=0", b"A"),
        None
    );
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"i=2:d=0", b"B"),
        None
    );
    assert_eq!(
        open_osc99_notification,
        Some(Osc99Notification {
            notification_id: Some("2".to_string()),
            title: "B".to_string(),
            body: String::new(),
        })
    );
}

#[test]
fn osc99_queries_return_nothing_and_leave_the_open_notification() {
    let mut open_osc99_notification = None;
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"i=1:d=0", b"T"),
        None
    );
    let open_before_queries = open_osc99_notification.clone();

    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"i=1:p=?", b""),
        None
    );
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"i=1:p=alive", b""),
        None
    );
    assert_eq!(
        apply_osc99_sequence(&mut open_osc99_notification, b"i=1:p=?"),
        None
    );

    assert_eq!(open_osc99_notification, open_before_queries);
}

#[test]
fn osc99_close_names_its_id_and_leaves_the_open_notification() {
    let mut open_osc99_notification = None;
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"i=2:d=0", b"T"),
        None
    );
    let open_before_close = open_osc99_notification.clone();

    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"i=1:p=close", b""),
        Some(ProgramFact::NotificationWithdrawn {
            notification_id: "1".to_string()
        })
    );
    assert_eq!(
        apply_osc99_sequence(&mut open_osc99_notification, b"i=1:p=close"),
        Some(ProgramFact::NotificationWithdrawn {
            notification_id: "1".to_string()
        })
    );
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"p=close", b""),
        None
    );
    assert_eq!(open_osc99_notification, open_before_close);
}

#[test]
fn osc99_base64_payloads_decode_with_or_without_padding() {
    let mut open_osc99_notification = None;
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"e=1", b"SGVsbG8="),
        Some(build_notify(Some("Hello"), "", None))
    );
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"e=1:p=body", b"SGVsbG8"),
        Some(build_notify(None, "Hello", None))
    );
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"e=1", b"!!!"),
        None
    );
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"e=0", b"SGVsbG8="),
        Some(build_notify(Some("SGVsbG8="), "", None))
    );
}

#[test]
fn osc99_untexted_part_completes_the_notification() {
    let mut open_osc99_notification = None;
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"i=1:d=0", b"Title"),
        None
    );
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"i=1:p=icon:e=1", b"AAAA"),
        Some(build_notify(Some("Title"), "", Some("1")))
    );
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"p=buttons", b"Yes"),
        None
    );
}

#[test]
fn osc99_with_no_title_and_no_body_is_dropped() {
    let mut open_osc99_notification = None;
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"", b""),
        None
    );
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"i=1", b"\x07\x1b"),
        None
    );
    assert_eq!(
        apply_osc99_sequence(&mut open_osc99_notification, b""),
        None
    );
    assert_eq!(open_osc99_notification, None);
}

#[test]
fn osc99_metadata_skips_unknown_keys_and_entries_without_a_value() {
    let mut open_osc99_notification = None;
    assert_eq!(
        apply_osc99_payloads(
            &mut open_osc99_notification,
            b"i=1:zz:x=5:u=2:p=title",
            b"T"
        ),
        Some(build_notify(Some("T"), "", Some("1")))
    );
    assert_eq!(
        apply_osc99_payloads(&mut open_osc99_notification, b"p=body:p=title", b"T"),
        Some(build_notify(Some("T"), "", None))
    );
}

#[test]
fn osc99_ids_lose_refused_bytes_and_keep_at_most_512() {
    assert_eq!(
        sanitize_osc99_notification_id(b"a%b!c"),
        Some("abc".to_string())
    );
    assert_eq!(
        sanitize_osc99_notification_id(b"A-z_0+9."),
        Some("A-z_0+9.".to_string())
    );
    assert_eq!(sanitize_osc99_notification_id(b"%%"), None);
    assert_eq!(sanitize_osc99_notification_id(b""), None);
    assert_eq!(
        sanitize_osc99_notification_id("x".repeat(600).as_bytes()),
        Some("x".repeat(512))
    );
}

#[test]
fn osc99_joined_text_keeps_at_most_512_bytes() {
    let mut open_osc99_notification = None;
    let body_part = "b".repeat(300);
    assert_eq!(
        apply_osc99_payloads(
            &mut open_osc99_notification,
            b"p=body:d=0",
            body_part.as_bytes()
        ),
        None
    );
    assert_eq!(
        apply_osc99_payloads(
            &mut open_osc99_notification,
            b"p=body:d=0",
            body_part.as_bytes()
        ),
        None
    );
    assert_eq!(
        apply_osc99_payloads(
            &mut open_osc99_notification,
            b"p=body",
            body_part.as_bytes()
        ),
        Some(build_notify(None, &"b".repeat(512), None))
    );
}
