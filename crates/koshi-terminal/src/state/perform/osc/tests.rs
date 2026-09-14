//! Tests for OSC 133 marker parsing and OSC 7 working-directory parsing.

use std::path::Path;

use super::*;

#[test]
fn osc133_parses_each_marker_and_exit_code() {
    assert_eq!(parse_osc133(&[b"133", b"A"]), Some(Osc133::Prompt));
    assert_eq!(parse_osc133(&[b"133", b"B"]), Some(Osc133::Input));
    assert_eq!(parse_osc133(&[b"133", b"C"]), Some(Osc133::CommandStart));
    assert_eq!(
        parse_osc133(&[b"133", b"D"]),
        Some(Osc133::CommandFinished(None))
    );
    assert_eq!(
        parse_osc133(&[b"133", b"D", b"0"]),
        Some(Osc133::CommandFinished(Some(0)))
    );
    assert_eq!(
        parse_osc133(&[b"133", b"D", b"137"]),
        Some(Osc133::CommandFinished(Some(137)))
    );
}

#[test]
fn osc133_exit_code_accepts_every_decimal_i32_form() {
    assert_eq!(
        parse_osc133(&[b"133", b"D", b"-1"]),
        Some(Osc133::CommandFinished(Some(-1)))
    );
    assert_eq!(
        parse_osc133(&[b"133", b"D", b"+3"]),
        Some(Osc133::CommandFinished(Some(3)))
    );
    assert_eq!(
        parse_osc133(&[b"133", b"D", b"007"]),
        Some(Osc133::CommandFinished(Some(7)))
    );
    assert_eq!(
        parse_osc133(&[b"133", b"D", b"2147483647"]),
        Some(Osc133::CommandFinished(Some(i32::MAX)))
    );
    assert_eq!(
        parse_osc133(&[b"133", b"D", b"-2147483648"]),
        Some(Osc133::CommandFinished(Some(i32::MIN)))
    );
}

#[test]
fn osc133_exit_code_rejects_anything_that_is_not_a_decimal_i32() {
    assert_invalid(&[b"133", b"D", b"2147483648"]);
    assert_invalid(&[b"133", b"D", b" 0"]);
    assert_invalid(&[b"133", b"D", b"0 "]);
    assert_invalid(&[b"133", b"D", b"0x1"]);
    assert_invalid(&[b"133", b"D", b"1.0"]);
    assert_invalid(&[b"133", b"D", b"\xff"]);
    assert_invalid(&[b"133", b"D", "１".as_bytes()]);
}

#[test]
fn osc133_rejects_unrelated_and_malformed_payloads() {
    assert_invalid(&[b"7", b"A"]);
    assert_invalid(&[b"133"]);
    assert_invalid(&[b"133", b"E"]);
    assert_invalid(&[b"133", b"D", b"not-a-number"]);
}

#[test]
fn osc133_markers_carrying_shell_options_are_still_recognized() {
    // A shell integration appends `key=value` options after the marker, and
    // after the exit code on `D`.
    assert_eq!(
        parse_osc133(&[b"133", b"A", b"cl=m", b"aid=1"]),
        Some(Osc133::Prompt)
    );
    assert_eq!(parse_osc133(&[b"133", b"B", b"aid=1"]), Some(Osc133::Input));
    assert_eq!(
        parse_osc133(&[b"133", b"C", b""]),
        Some(Osc133::CommandStart)
    );
    assert_eq!(
        parse_osc133(&[b"133", b"D", b"0", b"aid=1"]),
        Some(Osc133::CommandFinished(Some(0)))
    );
    assert_eq!(
        parse_osc133(&[b"133", b"D", b""]),
        Some(Osc133::CommandFinished(None)),
        "an empty first parameter carries no code"
    );
}

#[test]
fn osc133_rejects_a_malformed_command_number_or_marker() {
    assert_invalid(&[]);
    assert_invalid(&[b"133", b""]);
    assert_invalid(&[b"133", b"a"]);
    assert_invalid(&[b"133", b"AB"]);
    assert_invalid(&[b"0133", b"A"]);
    assert_invalid(&[b"1330", b"A"]);
    assert_invalid(&[b"", b"A"]);
}

fn assert_invalid(osc_parameters: &[&[u8]]) {
    assert_eq!(
        parse_osc133(osc_parameters),
        None,
        "payload {osc_parameters:?}"
    );
}

fn parse_reported_working_directory(working_directory_uri: &[u8]) -> ReportedWorkingDirectory {
    parse_osc7_working_directory(working_directory_uri).unwrap_or_else(|| {
        panic!("a valid reported working-directory URI: {working_directory_uri:?}")
    })
}

#[test]
fn osc7_splits_the_host_from_the_path() {
    let reported_working_directory = parse_reported_working_directory(b"file://host/tmp");
    assert_eq!(reported_working_directory.get_host(), Some("host"));
    assert_eq!(
        reported_working_directory.get_working_directory_path(),
        Path::new("/tmp")
    );
}

#[test]
fn osc7_empty_authority_gives_no_host() {
    let reported_working_directory = parse_reported_working_directory(b"file:///tmp");
    assert_eq!(reported_working_directory.get_host(), None);
    assert_eq!(
        reported_working_directory.get_working_directory_path(),
        Path::new("/tmp")
    );
}

#[test]
fn osc7_root_path_after_a_host_is_a_lone_slash() {
    let reported_working_directory = parse_reported_working_directory(b"file://host/");
    assert_eq!(reported_working_directory.get_host(), Some("host"));
    assert_eq!(
        reported_working_directory.get_working_directory_path(),
        Path::new("/")
    );
}

#[test]
fn osc7_scheme_is_case_insensitive_but_the_separator_is_not() {
    assert_eq!(
        parse_reported_working_directory(b"FILE://host/tmp").get_working_directory_path(),
        Path::new("/tmp")
    );
    assert_eq!(
        parse_reported_working_directory(b"File:///tmp").get_working_directory_path(),
        Path::new("/tmp")
    );
    assert_eq!(parse_osc7_working_directory(b"file:/tmp"), None);
    assert_eq!(parse_osc7_working_directory(b"file:\\\\host/tmp"), None);
}

#[test]
fn osc7_rejects_a_uri_shorter_than_the_scheme_prefix() {
    assert_eq!(parse_osc7_working_directory(b""), None);
    assert_eq!(parse_osc7_working_directory(b"file:/"), None);
}

#[test]
fn osc7_rejects_a_non_file_scheme() {
    assert_eq!(parse_osc7_working_directory(b"http://host/tmp"), None);
    assert_eq!(parse_osc7_working_directory(b"files://host/tmp"), None);
}

#[test]
fn osc7_rejects_an_authority_with_no_path() {
    assert_eq!(parse_osc7_working_directory(b"file://"), None);
    assert_eq!(parse_osc7_working_directory(b"file://host"), None);
    assert_eq!(parse_osc7_working_directory(b"file://host:22"), None);
}

#[test]
fn osc7_percent_decodes_the_path() {
    assert_eq!(
        parse_reported_working_directory(b"file:///a%20b").get_working_directory_path(),
        Path::new("/a b")
    );
    assert_eq!(
        parse_reported_working_directory(b"file:///%C3%A9").get_working_directory_path(),
        Path::new("/é")
    );
    assert_eq!(
        parse_reported_working_directory(b"file:///a%2Fb").get_working_directory_path(),
        Path::new("/a/b")
    );
}

#[test]
fn osc7_keeps_a_percent_without_two_hex_digits_literal() {
    assert_eq!(
        parse_reported_working_directory(b"file:///100%").get_working_directory_path(),
        Path::new("/100%")
    );
    assert_eq!(
        parse_reported_working_directory(b"file:///a%zzb").get_working_directory_path(),
        Path::new("/a%zzb")
    );
    assert_eq!(
        parse_reported_working_directory(b"file:///a%4").get_working_directory_path(),
        Path::new("/a%4")
    );
}

#[test]
fn osc7_keeps_query_and_fragment_characters_in_the_path() {
    assert_eq!(
        parse_reported_working_directory(b"file:///a?b#c").get_working_directory_path(),
        Path::new("/a?b#c")
    );
}

#[test]
fn osc7_keeps_a_double_slash_path_with_no_host() {
    let reported_working_directory = parse_reported_working_directory(b"file:////srv/share");
    assert_eq!(reported_working_directory.get_host(), None);
    assert_eq!(
        reported_working_directory.get_working_directory_path(),
        Path::new("//srv/share")
    );
}

#[test]
fn osc7_rejects_a_decoded_nul_anywhere_in_the_path() {
    assert_eq!(parse_osc7_working_directory(b"file:///a%00b"), None);
    assert_eq!(parse_osc7_working_directory(b"file:///%00"), None);
    assert_eq!(parse_osc7_working_directory(b"file:///tmp\x00"), None);
}

#[test]
fn osc7_does_not_percent_decode_the_host() {
    let reported_working_directory = parse_reported_working_directory(b"file://h%2Fost/x");
    assert_eq!(reported_working_directory.get_host(), Some("h%2Fost"));
    assert_eq!(
        reported_working_directory.get_working_directory_path(),
        Path::new("/x")
    );
}

#[test]
fn osc7_decodes_a_non_utf8_host_byte_to_the_replacement_character() {
    assert_eq!(
        parse_reported_working_directory(b"file://h\xffst/x").get_host(),
        Some("h\u{FFFD}st")
    );
}

#[test]
fn osc7_filters_control_characters_out_of_the_host() {
    assert_eq!(
        parse_reported_working_directory(b"file://ho\x7fst/x").get_host(),
        Some("host")
    );
    assert_eq!(
        parse_reported_working_directory("file://ho\u{202E}st/x".as_bytes()).get_host(),
        Some("host")
    );
}

#[test]
fn osc7_host_of_only_control_characters_is_an_empty_host_not_no_host() {
    assert_eq!(
        parse_reported_working_directory(b"file://\x7f/x").get_host(),
        Some("")
    );
}

#[test]
fn osc7_accepts_a_uri_at_the_byte_limit_and_rejects_one_past_it() {
    let mut working_directory_uri_bytes = b"file:///".to_vec();
    working_directory_uri_bytes.resize(MAX_OSC7_URI_BYTE_COUNT, b'a');
    let mut expected_working_directory_path = String::from("/");
    expected_working_directory_path
        .push_str(&"a".repeat(MAX_OSC7_URI_BYTE_COUNT - "file:///".len()));
    assert_eq!(
        parse_reported_working_directory(&working_directory_uri_bytes).get_working_directory_path(),
        Path::new(&expected_working_directory_path)
    );

    working_directory_uri_bytes.push(b'a');
    assert_eq!(
        parse_osc7_working_directory(&working_directory_uri_bytes),
        None
    );
}

#[cfg(unix)]
#[test]
fn unix_path_bytes_keep_a_non_utf8_byte() {
    use std::os::unix::ffi::OsStringExt;
    let expected_working_directory_path =
        PathBuf::from(std::ffi::OsString::from_vec(b"/p/\xff".to_vec()));
    assert_eq!(
        decode_working_directory_path(b"/p/\xff".to_vec()),
        Some(expected_working_directory_path)
    );
}

#[cfg(unix)]
#[test]
fn unix_path_bytes_keep_a_leading_slash_before_a_drive_letter() {
    assert_eq!(
        decode_working_directory_path(b"/C:/Users".to_vec()),
        Some(PathBuf::from("/C:/Users"))
    );
}

#[cfg(windows)]
#[test]
fn windows_path_bytes_drop_the_slash_before_a_drive_letter() {
    assert_eq!(
        decode_working_directory_path(b"/C:/Users".to_vec()),
        Some(PathBuf::from("C:/Users"))
    );
    assert_eq!(
        decode_working_directory_path(b"/c:/x".to_vec()),
        Some(PathBuf::from("c:/x"))
    );
    assert_eq!(
        decode_working_directory_path(b"/C:".to_vec()),
        Some(PathBuf::from("C:"))
    );
}

#[cfg(windows)]
#[test]
fn windows_path_bytes_keep_a_slash_that_precedes_no_drive_letter() {
    assert_eq!(
        decode_working_directory_path(b"/1:/x".to_vec()),
        Some(PathBuf::from("/1:/x"))
    );
    assert_eq!(
        decode_working_directory_path(b"/C/x".to_vec()),
        Some(PathBuf::from("/C/x"))
    );
    assert_eq!(
        decode_working_directory_path(b"C:/x".to_vec()),
        Some(PathBuf::from("C:/x"))
    );
    assert_eq!(
        decode_working_directory_path(b"/".to_vec()),
        Some(PathBuf::from("/"))
    );
}

#[cfg(windows)]
#[test]
fn windows_path_bytes_reject_non_utf8() {
    assert_eq!(decode_working_directory_path(b"/p/\xff".to_vec()), None);
}
