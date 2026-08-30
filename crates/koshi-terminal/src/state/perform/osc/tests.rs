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
    assert_invalid(&[b"133", b"A", b"extra"]);
    assert_invalid(&[b"133", b"D", b""]);
    assert_invalid(&[b"133", b"D", b"not-a-number"]);
    assert_invalid(&[b"133", b"D", b"0", b"extra"]);
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
    assert_invalid(&[b"133", b"A", b""]);
    assert_invalid(&[b"133", b"D", b"0", b""]);
}

fn assert_invalid(params: &[&[u8]]) {
    assert_eq!(parse_osc133(params), None, "payload {params:?}");
}

fn parsed(uri: &[u8]) -> ReportedCwd {
    parse_osc7_cwd(uri).unwrap_or_else(|| panic!("a valid cwd URI: {uri:?}"))
}

#[test]
fn osc7_splits_the_host_from_the_path() {
    let cwd = parsed(b"file://host/tmp");
    assert_eq!(cwd.host(), Some("host"));
    assert_eq!(cwd.path(), Path::new("/tmp"));
}

#[test]
fn osc7_empty_authority_gives_no_host() {
    let cwd = parsed(b"file:///tmp");
    assert_eq!(cwd.host(), None);
    assert_eq!(cwd.path(), Path::new("/tmp"));
}

#[test]
fn osc7_root_path_after_a_host_is_a_lone_slash() {
    let cwd = parsed(b"file://host/");
    assert_eq!(cwd.host(), Some("host"));
    assert_eq!(cwd.path(), Path::new("/"));
}

#[test]
fn osc7_scheme_is_case_insensitive_but_the_separator_is_not() {
    assert_eq!(parsed(b"FILE://host/tmp").path(), Path::new("/tmp"));
    assert_eq!(parsed(b"File:///tmp").path(), Path::new("/tmp"));
    assert_eq!(parse_osc7_cwd(b"file:/tmp"), None);
    assert_eq!(parse_osc7_cwd(b"file:\\\\host/tmp"), None);
}

#[test]
fn osc7_rejects_a_uri_shorter_than_the_scheme_prefix() {
    assert_eq!(parse_osc7_cwd(b""), None);
    assert_eq!(parse_osc7_cwd(b"file:/"), None);
}

#[test]
fn osc7_rejects_a_non_file_scheme() {
    assert_eq!(parse_osc7_cwd(b"http://host/tmp"), None);
    assert_eq!(parse_osc7_cwd(b"files://host/tmp"), None);
}

#[test]
fn osc7_rejects_an_authority_with_no_path() {
    assert_eq!(parse_osc7_cwd(b"file://"), None);
    assert_eq!(parse_osc7_cwd(b"file://host"), None);
    assert_eq!(parse_osc7_cwd(b"file://host:22"), None);
}

#[test]
fn osc7_percent_decodes_the_path() {
    assert_eq!(parsed(b"file:///a%20b").path(), Path::new("/a b"));
    assert_eq!(parsed(b"file:///%C3%A9").path(), Path::new("/é"));
    assert_eq!(parsed(b"file:///a%2Fb").path(), Path::new("/a/b"));
}

#[test]
fn osc7_keeps_a_percent_without_two_hex_digits_literal() {
    assert_eq!(parsed(b"file:///100%").path(), Path::new("/100%"));
    assert_eq!(parsed(b"file:///a%zzb").path(), Path::new("/a%zzb"));
    assert_eq!(parsed(b"file:///a%4").path(), Path::new("/a%4"));
}

#[test]
fn osc7_keeps_query_and_fragment_characters_in_the_path() {
    assert_eq!(parsed(b"file:///a?b#c").path(), Path::new("/a?b#c"));
}

#[test]
fn osc7_keeps_a_double_slash_path_with_no_host() {
    let cwd = parsed(b"file:////srv/share");
    assert_eq!(cwd.host(), None);
    assert_eq!(cwd.path(), Path::new("//srv/share"));
}

#[test]
fn osc7_rejects_a_decoded_nul_anywhere_in_the_path() {
    assert_eq!(parse_osc7_cwd(b"file:///a%00b"), None);
    assert_eq!(parse_osc7_cwd(b"file:///%00"), None);
    assert_eq!(parse_osc7_cwd(b"file:///tmp\x00"), None);
}

#[test]
fn osc7_does_not_percent_decode_the_host() {
    let cwd = parsed(b"file://h%2Fost/x");
    assert_eq!(cwd.host(), Some("h%2Fost"));
    assert_eq!(cwd.path(), Path::new("/x"));
}

#[test]
fn osc7_decodes_a_non_utf8_host_byte_to_the_replacement_character() {
    assert_eq!(parsed(b"file://h\xffst/x").host(), Some("h\u{FFFD}st"));
}

#[test]
fn osc7_filters_control_characters_out_of_the_host() {
    assert_eq!(parsed(b"file://ho\x7fst/x").host(), Some("host"));
    assert_eq!(
        parsed("file://ho\u{202E}st/x".as_bytes()).host(),
        Some("host")
    );
}

#[test]
fn osc7_host_of_only_control_characters_is_an_empty_host_not_no_host() {
    assert_eq!(parsed(b"file://\x7f/x").host(), Some(""));
}

#[test]
fn osc7_accepts_a_uri_at_the_byte_limit_and_rejects_one_past_it() {
    let mut uri = b"file:///".to_vec();
    uri.resize(MAX_OSC7_URI_BYTES, b'a');
    let mut expected = String::from("/");
    expected.push_str(&"a".repeat(MAX_OSC7_URI_BYTES - "file:///".len()));
    assert_eq!(parsed(&uri).path(), Path::new(&expected));

    uri.push(b'a');
    assert_eq!(parse_osc7_cwd(&uri), None);
}

#[cfg(unix)]
#[test]
fn unix_path_bytes_keep_a_non_utf8_byte() {
    use std::os::unix::ffi::OsStringExt;
    let expected = PathBuf::from(std::ffi::OsString::from_vec(b"/p/\xff".to_vec()));
    assert_eq!(bytes_to_path(b"/p/\xff".to_vec()), Some(expected));
}

#[cfg(unix)]
#[test]
fn unix_path_bytes_keep_a_leading_slash_before_a_drive_letter() {
    assert_eq!(
        bytes_to_path(b"/C:/Users".to_vec()),
        Some(PathBuf::from("/C:/Users"))
    );
}

#[cfg(windows)]
#[test]
fn windows_path_bytes_drop_the_slash_before_a_drive_letter() {
    assert_eq!(
        bytes_to_path(b"/C:/Users".to_vec()),
        Some(PathBuf::from("C:/Users"))
    );
    assert_eq!(
        bytes_to_path(b"/c:/x".to_vec()),
        Some(PathBuf::from("c:/x"))
    );
    assert_eq!(bytes_to_path(b"/C:".to_vec()), Some(PathBuf::from("C:")));
}

#[cfg(windows)]
#[test]
fn windows_path_bytes_keep_a_slash_that_precedes_no_drive_letter() {
    assert_eq!(
        bytes_to_path(b"/1:/x".to_vec()),
        Some(PathBuf::from("/1:/x"))
    );
    assert_eq!(bytes_to_path(b"/C/x".to_vec()), Some(PathBuf::from("/C/x")));
    assert_eq!(bytes_to_path(b"C:/x".to_vec()), Some(PathBuf::from("C:/x")));
    assert_eq!(bytes_to_path(b"/".to_vec()), Some(PathBuf::from("/")));
}

#[cfg(windows)]
#[test]
fn windows_path_bytes_reject_non_utf8() {
    assert_eq!(bytes_to_path(b"/p/\xff".to_vec()), None);
}
