//! Tests for the stand-in clipboard tools: how the arguments of `pbcopy`,
//! `xclip`, `xsel` and `wl-copy` read as a copy or a pass-through, the `xclip`
//! option prefixes, the `xsel` option letters, the plain text types, how the
//! input of a copy is read part by part, and how the real tool reads every
//! byte of it.

use std::sync::mpsc;

use koshi_core::ids::{PaneId, SessionId};
use koshi_ipc::protocol::{IpcRequestKind, IpcResult};
use koshi_test_support::fixtures::{
    build_restarting_refusal, build_test_runtime_directory, spawn_session_replacing_its_image,
};

use super::*;

/// A copy of standard input to `clipboard_targets`.
fn build_standard_input_copy(
    clipboard_targets: &'static str,
    should_trim_last_newline: bool,
) -> ClipboardToolCall {
    ClipboardToolCall::Copy {
        clipboard_targets,
        copy_source: CopySource::StandardInput {
            should_trim_last_newline,
        },
    }
}

/// A copy of `argument_text` to `clipboard_targets`.
fn build_argument_text_copy(
    clipboard_targets: &'static str,
    argument_text: &str,
) -> ClipboardToolCall {
    ClipboardToolCall::Copy {
        clipboard_targets,
        copy_source: CopySource::ArgumentText(argument_text.to_string()),
    }
}

/// The call `tool_name` makes with `tool_arguments`, standard input from a
/// pipe, and standard output on a terminal: the shape of `printf hi | xclip`
/// typed at a shell prompt.
fn parse_piped_call(tool_name: &str, tool_arguments: &[&str]) -> ClipboardToolCall {
    parse_clipboard_tool_call(tool_name, tool_arguments, false, true)
}

#[test]
fn pbcopy_with_no_argument_or_the_general_pasteboard_copies_input_to_the_clipboard() {
    for tool_arguments in [&[][..], &["-pboard", "general"][..]] {
        assert_eq!(
            parse_piped_call("pbcopy", tool_arguments),
            build_standard_input_copy("c", false),
            "pbcopy {tool_arguments:?}"
        );
    }
}

#[test]
fn pbcopy_with_another_pasteboard_or_argument_passes_through() {
    for tool_arguments in [
        &["-pboard", "find"][..],
        &["-pboard"][..],
        &["-pboard", "general", "-pboard", "font"][..],
        &["-help"][..],
    ] {
        assert_eq!(
            parse_piped_call("pbcopy", tool_arguments),
            ClipboardToolCall::PassThrough,
            "pbcopy {tool_arguments:?}"
        );
    }
}

#[test]
fn xclip_with_no_argument_copies_piped_input_to_the_primary_selection() {
    assert_eq!(
        parse_piped_call("xclip", &[]),
        build_standard_input_copy("p", false)
    );
}

#[test]
fn xclip_with_no_argument_and_input_on_a_terminal_passes_through() {
    assert_eq!(
        parse_clipboard_tool_call("xclip", &[], true, true),
        ClipboardToolCall::PassThrough
    );
}

#[test]
fn xclip_in_copies_input_on_a_terminal() {
    for tool_arguments in [&["-in"][..], &["-i"][..]] {
        assert_eq!(
            parse_clipboard_tool_call("xclip", tool_arguments, true, true),
            build_standard_input_copy("p", false),
            "xclip {tool_arguments:?}"
        );
    }
}

#[test]
fn the_last_of_xclip_in_and_out_decides() {
    assert_eq!(
        parse_piped_call("xclip", &["-in", "-out"]),
        ClipboardToolCall::PassThrough
    );
    assert_eq!(
        parse_piped_call("xclip", &["-out", "-in"]),
        build_standard_input_copy("p", false)
    );
    assert_eq!(
        parse_piped_call("xclip", &["-o"]),
        ClipboardToolCall::PassThrough
    );
}

#[test]
fn xclip_selection_reads_the_first_letter_in_any_case() {
    for (selection_name, clipboard_targets) in [
        ("clipboard", "c"),
        ("CLIP", "c"),
        ("c", "c"),
        ("secondary", "q"),
        ("Sec", "q"),
        ("primary", "p"),
        ("xyz", "p"),
        ("", "p"),
    ] {
        assert_eq!(
            parse_piped_call("xclip", &["-selection", selection_name]),
            build_standard_input_copy(clipboard_targets, false),
            "xclip -selection {selection_name:?}"
        );
    }
}

#[test]
fn xclip_selection_of_a_cut_buffer_passes_through() {
    for selection_name in ["buffer-cut", "B"] {
        assert_eq!(
            parse_piped_call("xclip", &["-selection", selection_name]),
            ClipboardToolCall::PassThrough,
            "xclip -selection {selection_name:?}"
        );
    }
}

#[test]
fn xclip_rmlastnl_trims_the_last_newline() {
    assert_eq!(
        parse_piped_call("xclip", &["-selection", "clipboard", "-rmlastnl"]),
        build_standard_input_copy("c", true)
    );
}

#[test]
fn xclip_unique_option_prefixes_read_as_the_option() {
    assert_eq!(
        parse_piped_call("xclip", &["-sel", "clip", "-rmlast", "-i"]),
        build_standard_input_copy("c", true)
    );
}

#[test]
fn xclip_plain_text_target_copies_and_other_targets_pass_through() {
    for content_type in ["UTF8_STRING", "STRING", "TEXT", "text/plain"] {
        assert_eq!(
            parse_piped_call("xclip", &["-target", content_type]),
            build_standard_input_copy("p", false),
            "xclip -target {content_type:?}"
        );
    }
    for content_type in ["image/png", "text/html", "utf8_string"] {
        assert_eq!(
            parse_piped_call("xclip", &["-t", content_type]),
            ClipboardToolCall::PassThrough,
            "xclip -t {content_type:?}"
        );
    }
}

#[test]
fn xclip_options_that_change_only_messages_or_the_display_still_copy() {
    assert_eq!(
        parse_piped_call(
            "xclip",
            &[
                "-loops", "1", "-display", ":0", "-silent", "-quiet", "-verbose", "-debug",
                "-noutf8",
            ]
        ),
        build_standard_input_copy("p", false)
    );
}

#[test]
fn xclip_option_with_its_value_missing_passes_through() {
    for tool_arguments in [
        &["-selection"][..],
        &["-loops"][..],
        &["-display"][..],
        &["-target"][..],
    ] {
        assert_eq!(
            parse_piped_call("xclip", tool_arguments),
            ClipboardToolCall::PassThrough,
            "xclip {tool_arguments:?}"
        );
    }
}

#[test]
fn xclip_arguments_the_stand_in_does_not_read_pass_through() {
    for tool_arguments in [
        &["-filter"][..],
        &["-f"][..],
        &["-version"][..],
        &["-h"][..],
        &["-sensitive"][..],
        &["-alt-text", "secret"][..],
        &["-wait", "100"][..],
        &["notes.txt"][..],
        &["-input"][..],
        &["-s"][..],
        &["-se", "clipboard"][..],
        &["--"][..],
        &["-"][..],
    ] {
        assert_eq!(
            parse_piped_call("xclip", tool_arguments),
            ClipboardToolCall::PassThrough,
            "xclip {tool_arguments:?}"
        );
    }
}

#[test]
fn an_xclip_argument_names_the_option_it_spells_whole_or_alone_starts() {
    for (xclip_argument, option_name) in [
        ("-in", "-in"),
        ("-i", "-in"),
        ("-sel", "-selection"),
        ("-t", "-target"),
        ("-de", "-debug"),
        ("-verb", "-verbose"),
        ("-vers", "-version"),
        ("-rmlastnl", "-rmlastnl"),
    ] {
        assert_eq!(
            find_xclip_option(xclip_argument),
            Some(option_name),
            "{xclip_argument:?}"
        );
    }
}

#[test]
fn an_xclip_argument_that_starts_several_options_or_none_names_no_option() {
    for xclip_argument in ["-s", "-d", "-v", "-input", "-", "", "selection", "--in"] {
        assert_eq!(
            find_xclip_option(xclip_argument),
            None,
            "{xclip_argument:?}"
        );
    }
}

#[test]
fn xsel_with_piped_input_and_output_on_a_terminal_copies_to_the_primary_selection() {
    assert_eq!(
        parse_clipboard_tool_call("xsel", &[], false, true),
        build_standard_input_copy("p", false)
    );
}

#[test]
fn xsel_with_no_mode_passes_through_unless_only_output_is_a_terminal() {
    for (is_stdin_terminal, is_stdout_terminal) in [(true, true), (false, false), (true, false)] {
        assert_eq!(
            parse_clipboard_tool_call("xsel", &[], is_stdin_terminal, is_stdout_terminal),
            ClipboardToolCall::PassThrough,
            "stdin terminal {is_stdin_terminal}, stdout terminal {is_stdout_terminal}"
        );
    }
}

#[test]
fn xsel_input_option_copies_whatever_the_terminals() {
    for tool_arguments in [&["-i"][..], &["--input"][..]] {
        for (is_stdin_terminal, is_stdout_terminal) in
            [(true, true), (false, false), (true, false), (false, true)]
        {
            assert_eq!(
                parse_clipboard_tool_call(
                    "xsel",
                    tool_arguments,
                    is_stdin_terminal,
                    is_stdout_terminal
                ),
                build_standard_input_copy("p", false),
                "xsel {tool_arguments:?}, stdin terminal {is_stdin_terminal}, \
                 stdout terminal {is_stdout_terminal}"
            );
        }
    }
}

#[test]
fn xsel_output_option_passes_through_even_with_input() {
    for tool_arguments in [
        &["-o"][..],
        &["--output"][..],
        &["-i", "-o"][..],
        &["-o", "-i"][..],
    ] {
        assert_eq!(
            parse_piped_call("xsel", tool_arguments),
            ClipboardToolCall::PassThrough,
            "xsel {tool_arguments:?}"
        );
    }
}

#[test]
fn xsel_clipboard_option_wins_over_primary_and_secondary() {
    for tool_arguments in [&["-i", "-b", "-p"][..], &["-i", "-s", "--clipboard"][..]] {
        assert_eq!(
            parse_piped_call("xsel", tool_arguments),
            build_standard_input_copy("c", false),
            "xsel {tool_arguments:?}"
        );
    }
}

#[test]
fn the_last_of_xsel_primary_and_secondary_decides() {
    assert_eq!(
        parse_piped_call("xsel", &["-i", "-p", "-s"]),
        build_standard_input_copy("q", false)
    );
    assert_eq!(
        parse_piped_call("xsel", &["-i", "--secondary", "--primary"]),
        build_standard_input_copy("p", false)
    );
}

#[test]
fn xsel_option_letters_in_one_argument_read_one_by_one() {
    assert_eq!(
        parse_piped_call("xsel", &["-ib"]),
        build_standard_input_copy("c", false)
    );
    assert_eq!(
        parse_piped_call("xsel", &["-is", "--trim"]),
        build_standard_input_copy("q", true)
    );
    assert_eq!(
        parse_piped_call("xsel", &["-io"]),
        ClipboardToolCall::PassThrough
    );
}

#[test]
fn xsel_options_that_change_only_messages_or_the_display_still_copy() {
    assert_eq!(
        parse_piped_call(
            "xsel",
            &[
                "-i",
                "-n",
                "-v",
                "--nodetach",
                "--verbose",
                "-t",
                "500",
                "--selectionTimeout",
                "500",
                "--display",
                ":0",
                "-l",
                "xsel.log",
                "--logfile",
                "xsel.log",
                "-m",
                "koshi",
                "--name",
                "koshi",
                "--windowName",
                "koshi",
            ]
        ),
        build_standard_input_copy("p", false)
    );
}

#[test]
fn xsel_option_with_its_value_missing_passes_through() {
    for option_name in [
        "--display",
        "-t",
        "--selectionTimeout",
        "-l",
        "--logfile",
        "-m",
        "--name",
        "--windowName",
    ] {
        assert_eq!(
            parse_piped_call("xsel", &["-i", option_name]),
            ClipboardToolCall::PassThrough,
            "xsel -i {option_name}"
        );
    }
}

#[test]
fn xsel_arguments_the_stand_in_does_not_read_pass_through() {
    for tool_arguments in [
        &["-i", "-a"][..],
        &["-i", "--append"][..],
        &["-c"][..],
        &["-i", "-f"][..],
        &["-i", "-z"][..],
        &["-k"][..],
        &["-x"][..],
        &["-d"][..],
        &["-h"][..],
        &["--version"][..],
        &["-i", "-"][..],
        &["-i", "notes.txt"][..],
        &["-i", "--trim=yes"][..],
    ] {
        assert_eq!(
            parse_piped_call("xsel", tool_arguments),
            ClipboardToolCall::PassThrough,
            "xsel {tool_arguments:?}"
        );
    }
}

#[test]
fn xsel_splits_each_argument_of_one_dash_and_two_or_more_letters() {
    assert_eq!(
        expand_xsel_short_options(&["-ib", "--input", "-t", "-", "x", "-xyz"]),
        vec!["-i", "-b", "--input", "-t", "-", "x", "-x", "-y", "-z"]
    );
}

#[test]
fn wl_copy_with_no_argument_copies_input_to_the_clipboard() {
    assert_eq!(
        parse_piped_call("wl-copy", &[]),
        build_standard_input_copy("c", false)
    );
}

#[test]
fn wl_copy_primary_and_trim_newline_set_the_target_and_the_trim() {
    for tool_arguments in [&["-p", "-n"][..], &["--primary", "--trim-newline"][..]] {
        assert_eq!(
            parse_piped_call("wl-copy", tool_arguments),
            build_standard_input_copy("p", true),
            "wl-copy {tool_arguments:?}"
        );
    }
}

#[test]
fn wl_copy_words_are_copied_joined_by_one_space() {
    assert_eq!(
        parse_piped_call("wl-copy", &["hi", "there"]),
        build_argument_text_copy("c", "hi there")
    );
    assert_eq!(
        parse_piped_call("wl-copy", &["-p", "hi"]),
        build_argument_text_copy("p", "hi")
    );
}

#[test]
fn wl_copy_trim_newline_leaves_word_text_as_it_is() {
    assert_eq!(
        parse_piped_call("wl-copy", &["-n", "hi\n"]),
        build_argument_text_copy("c", "hi\n")
    );
}

#[test]
fn wl_copy_double_dash_ends_the_options() {
    assert_eq!(
        parse_piped_call("wl-copy", &["--", "-n"]),
        build_argument_text_copy("c", "-n")
    );
    assert_eq!(
        parse_piped_call("wl-copy", &["-p", "--", "a", "-b"]),
        build_argument_text_copy("p", "a -b")
    );
    assert_eq!(
        parse_piped_call("wl-copy", &["--"]),
        build_standard_input_copy("c", false)
    );
}

#[test]
fn a_lone_dash_is_a_word_for_wl_copy() {
    assert_eq!(
        parse_piped_call("wl-copy", &["-"]),
        build_argument_text_copy("c", "-")
    );
    assert_eq!(
        parse_piped_call("wl-copy", &["hi", "-"]),
        build_argument_text_copy("c", "hi -")
    );
}

#[test]
fn wl_copy_option_after_a_word_passes_through() {
    for tool_arguments in [&["hi", "-n"][..], &["hi", "--", "there"][..]] {
        assert_eq!(
            parse_piped_call("wl-copy", tool_arguments),
            ClipboardToolCall::PassThrough,
            "wl-copy {tool_arguments:?}"
        );
    }
}

#[test]
fn wl_copy_plain_text_type_copies_and_other_types_pass_through() {
    for tool_arguments in [
        &["-t", "text/plain"][..],
        &["--type", "UTF8_STRING"][..],
        &["--type=text/plain;charset=utf-8"][..],
    ] {
        assert_eq!(
            parse_piped_call("wl-copy", tool_arguments),
            build_standard_input_copy("c", false),
            "wl-copy {tool_arguments:?}"
        );
    }
    for tool_arguments in [
        &["-t", "image/png"][..],
        &["--type=image/png"][..],
        &["--type=TEXT/PLAIN"][..],
        &["-t"][..],
        &["--type"][..],
    ] {
        assert_eq!(
            parse_piped_call("wl-copy", tool_arguments),
            ClipboardToolCall::PassThrough,
            "wl-copy {tool_arguments:?}"
        );
    }
}

#[test]
fn wl_copy_seat_paste_once_and_foreground_still_copy() {
    for tool_arguments in [
        &["-s", "seat0", "-o", "-f"][..],
        &["--seat", "seat0", "--paste-once", "--foreground"][..],
        &["--seat=seat0"][..],
    ] {
        assert_eq!(
            parse_piped_call("wl-copy", tool_arguments),
            build_standard_input_copy("c", false),
            "wl-copy {tool_arguments:?}"
        );
    }
}

#[test]
fn wl_copy_option_with_its_value_missing_passes_through() {
    for tool_arguments in [&["-s"][..], &["--seat"][..]] {
        assert_eq!(
            parse_piped_call("wl-copy", tool_arguments),
            ClipboardToolCall::PassThrough,
            "wl-copy {tool_arguments:?}"
        );
    }
}

#[test]
fn wl_copy_arguments_the_stand_in_does_not_read_pass_through() {
    for tool_arguments in [
        &["-c"][..],
        &["--clear"][..],
        &["-pn"][..],
        &["--sensitive"][..],
        &["--version"][..],
        &["-h"][..],
        &["--prim"][..],
    ] {
        assert_eq!(
            parse_piped_call("wl-copy", tool_arguments),
            ClipboardToolCall::PassThrough,
            "wl-copy {tool_arguments:?}"
        );
    }
}

#[test]
fn a_tool_name_koshi_does_not_stand_in_for_passes_through() {
    assert_eq!(
        parse_piped_call("xdg-open", &[]),
        ClipboardToolCall::PassThrough
    );
}

#[test]
fn only_the_exact_plain_text_type_names_count_as_plain_text() {
    for content_type in [
        "text/plain",
        "text/plain;charset=utf-8",
        "UTF8_STRING",
        "STRING",
        "TEXT",
    ] {
        assert!(is_plain_text_type(content_type), "{content_type:?}");
    }
    for content_type in [
        "text/plain; charset=utf-8",
        "TEXT/PLAIN",
        "text/html",
        "text",
        "",
    ] {
        assert!(!is_plain_text_type(content_type), "{content_type:?}");
    }
}

/// A reader whose every read fails with `the pipe broke`.
struct BrokenPipeReader;

impl Read for BrokenPipeReader {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("the pipe broke"))
    }
}

/// A reader whose first read is interrupted, and whose later reads give
/// `rest_bytes`.
struct InterruptedOnceReader {
    /// Whether the interrupted read happened.
    is_interrupted: bool,
    /// The bytes the reads after the interrupted one give.
    rest_bytes: &'static [u8],
}

impl Read for InterruptedOnceReader {
    fn read(&mut self, read_bytes: &mut [u8]) -> std::io::Result<usize> {
        if !self.is_interrupted {
            self.is_interrupted = true;
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        self.rest_bytes.read(read_bytes)
    }
}

/// The bytes of `byte_count` letters `a`.
fn build_letter_bytes(byte_count: usize) -> Vec<u8> {
    vec![b'a'; byte_count]
}

#[test]
fn an_empty_input_is_one_empty_last_part() {
    let mut copy_input = CopyInput::from_input_reader(&b""[..], false);

    assert_eq!(
        copy_input.read_next_copy_part().expect("a slice reads"),
        Some((&b""[..], true))
    );
}

#[test]
fn the_newline_that_ends_the_input_is_left_out_of_the_last_part_only_when_trimmed() {
    let mut trimmed_copy_input = CopyInput::from_input_reader(&b"hi\n"[..], true);
    let mut kept_copy_input = CopyInput::from_input_reader(&b"hi\n"[..], false);

    assert_eq!(
        trimmed_copy_input
            .read_next_copy_part()
            .expect("a slice reads"),
        Some((&b"hi"[..], true))
    );
    assert_eq!(
        kept_copy_input
            .read_next_copy_part()
            .expect("a slice reads"),
        Some((&b"hi\n"[..], true))
    );
}

#[test]
fn an_input_of_one_part_is_one_last_part() {
    let input_bytes = build_letter_bytes(MAX_CLIPBOARD_COPY_PART_BYTE_COUNT);
    let mut copy_input = CopyInput::from_input_reader(input_bytes.as_slice(), false);

    let (copy_part, is_last_part) = copy_input
        .read_next_copy_part()
        .expect("a slice reads")
        .expect("the input is UTF-8");

    assert!(
        copy_part == input_bytes.as_slice(),
        "the part holds the input"
    );
    assert!(is_last_part);
}

#[test]
fn a_rewound_input_that_has_ended_gives_its_parts_again_from_the_first() {
    let input_bytes = build_letter_bytes(MAX_CLIPBOARD_COPY_PART_BYTE_COUNT + 1);
    let mut copy_input = CopyInput::from_input_reader(input_bytes.as_slice(), false);
    let _ = copy_input.read_next_copy_part().expect("a slice reads");
    assert_eq!(
        copy_input.read_next_copy_part().expect("a slice reads"),
        Some((&b"a"[..], true))
    );

    copy_input.rewind_copy_parts();

    let (first_copy_part, is_first_part_last) = copy_input
        .read_next_copy_part()
        .expect("a slice reads")
        .expect("the input is UTF-8");
    assert!(
        first_copy_part == &input_bytes[..MAX_CLIPBOARD_COPY_PART_BYTE_COUNT],
        "the first part holds the first 12582774 bytes again"
    );
    assert!(!is_first_part_last);
    assert_eq!(
        copy_input.read_next_copy_part().expect("a slice reads"),
        Some((&b"a"[..], true))
    );
}

#[test]
fn a_rewound_single_part_input_gives_its_trimmed_last_part_again() {
    let mut copy_input = CopyInput::from_input_reader(&b"hi\n"[..], true);
    assert_eq!(
        copy_input.read_next_copy_part().expect("a slice reads"),
        Some((&b"hi"[..], true))
    );

    copy_input.rewind_copy_parts();

    assert_eq!(
        copy_input.read_next_copy_part().expect("a slice reads"),
        Some((&b"hi"[..], true))
    );
}

#[test]
fn an_input_one_byte_past_a_part_gives_a_part_then_a_last_part_of_one_byte() {
    let input_bytes = build_letter_bytes(MAX_CLIPBOARD_COPY_PART_BYTE_COUNT + 1);
    let mut copy_input = CopyInput::from_input_reader(input_bytes.as_slice(), false);

    let (first_copy_part, is_first_part_last) = copy_input
        .read_next_copy_part()
        .expect("a slice reads")
        .expect("the input is UTF-8");
    assert!(
        first_copy_part == &input_bytes[..MAX_CLIPBOARD_COPY_PART_BYTE_COUNT],
        "the first part holds the first 12582774 bytes"
    );
    assert!(!is_first_part_last);
    assert_eq!(
        copy_input.read_next_copy_part().expect("a slice reads"),
        Some((&b"a"[..], true))
    );
}

#[test]
fn a_newline_that_ends_a_part_but_not_the_input_stays_in_the_part() {
    let mut input_bytes = build_letter_bytes(MAX_CLIPBOARD_COPY_PART_BYTE_COUNT - 1);
    input_bytes.extend_from_slice(b"\nx");
    let mut copy_input = CopyInput::from_input_reader(input_bytes.as_slice(), true);

    let (first_copy_part, is_first_part_last) = copy_input
        .read_next_copy_part()
        .expect("a slice reads")
        .expect("the input is UTF-8");
    assert!(
        first_copy_part == &input_bytes[..MAX_CLIPBOARD_COPY_PART_BYTE_COUNT],
        "the first part ends with the newline"
    );
    assert!(!is_first_part_last);
    assert_eq!(
        copy_input.read_next_copy_part().expect("a slice reads"),
        Some((&b"x"[..], true))
    );
}

#[test]
fn a_trimmed_newline_alone_behind_the_last_full_part_leaves_an_empty_last_part() {
    let mut input_bytes = build_letter_bytes(MAX_CLIPBOARD_COPY_PART_BYTE_COUNT);
    input_bytes.push(b'\n');
    let mut copy_input = CopyInput::from_input_reader(input_bytes.as_slice(), true);

    let (first_copy_part, is_first_part_last) = copy_input
        .read_next_copy_part()
        .expect("a slice reads")
        .expect("the input is UTF-8");
    assert!(
        first_copy_part == &input_bytes[..MAX_CLIPBOARD_COPY_PART_BYTE_COUNT],
        "the first part holds every letter"
    );
    assert!(!is_first_part_last);
    assert_eq!(
        copy_input.read_next_copy_part().expect("a slice reads"),
        Some((&b""[..], true))
    );
}

#[test]
fn a_character_cut_by_the_end_of_a_part_and_of_a_read_is_whole_with_the_next_bytes() {
    // `€` is `e2 82 ac`. The first part ends after `e2`, and the first read
    // after `82`.
    let mut input_bytes = build_letter_bytes(MAX_CLIPBOARD_COPY_PART_BYTE_COUNT - 1);
    input_bytes.extend_from_slice("€".as_bytes());
    let mut copy_input = CopyInput::from_input_reader(input_bytes.as_slice(), false);

    let (first_copy_part, is_first_part_last) = copy_input
        .read_next_copy_part()
        .expect("a slice reads")
        .expect("the cut character is whole once the next byte is read");
    assert_eq!(first_copy_part.len(), MAX_CLIPBOARD_COPY_PART_BYTE_COUNT);
    assert_eq!(first_copy_part.last(), Some(&0xe2));
    assert!(!is_first_part_last);
    assert_eq!(
        copy_input.read_next_copy_part().expect("a slice reads"),
        Some((&[0x82, 0xac][..], true))
    );
}

#[test]
fn an_input_that_is_not_utf8_gives_no_part() {
    for input_bytes in [&b"hi\xff"[..], &b"h\xc3"[..], &b"\xa9"[..]] {
        let mut copy_input = CopyInput::from_input_reader(input_bytes, false);

        assert_eq!(
            copy_input.read_next_copy_part().expect("a slice reads"),
            None,
            "{input_bytes:?}"
        );
    }
}

#[test]
fn a_failed_read_of_the_input_is_handed_back() {
    let mut copy_input = CopyInput::from_input_reader(BrokenPipeReader, false);

    let read_error = copy_input
        .read_next_copy_part()
        .expect_err("the read fails");

    assert_eq!(read_error.to_string(), "the pipe broke");
}

#[test]
fn the_whole_input_is_every_byte_read_untrimmed_then_the_rest_of_the_input() {
    let mut trimmed_copy_input = CopyInput::from_input_reader(&b"hi\n"[..], true);
    trimmed_copy_input
        .read_next_copy_part()
        .expect("a slice reads");
    let mut trimmed_whole_input = Vec::new();
    trimmed_copy_input
        .write_whole_input(&mut trimmed_whole_input)
        .expect("a slice reads");
    assert_eq!(trimmed_whole_input, b"hi\n");

    let input_bytes = build_letter_bytes(MAX_CLIPBOARD_COPY_PART_BYTE_COUNT + 4097);
    let mut copy_input = CopyInput::from_input_reader(input_bytes.as_slice(), false);
    copy_input.read_next_copy_part().expect("a slice reads");
    let mut whole_input = Vec::new();
    copy_input
        .write_whole_input(&mut whole_input)
        .expect("a slice reads");
    assert!(
        whole_input == input_bytes,
        "the real tool reads every byte of the input"
    );
}

#[test]
fn an_interrupted_read_of_the_rest_of_the_input_is_read_again() {
    let read_input_bytes = build_letter_bytes(MAX_CLIPBOARD_COPY_PART_BYTE_COUNT + 1);
    let input_reader = read_input_bytes.as_slice().chain(InterruptedOnceReader {
        is_interrupted: false,
        rest_bytes: b"bc",
    });
    let mut copy_input = CopyInput::from_input_reader(input_reader, false);
    copy_input
        .read_next_copy_part()
        .expect("the first part reads");

    let mut whole_input = Vec::new();
    copy_input
        .write_whole_input(&mut whole_input)
        .expect("the interrupted read is read again");

    let mut expected_whole_input = read_input_bytes.clone();
    expected_whole_input.extend_from_slice(b"bc");
    assert!(
        whole_input == expected_whole_input,
        "the real tool reads every byte of the input"
    );
}

#[test]
fn a_failed_read_of_the_rest_of_the_input_is_handed_back_after_every_byte_read() {
    let read_input_bytes = build_letter_bytes(MAX_CLIPBOARD_COPY_PART_BYTE_COUNT + 1);
    let input_reader = read_input_bytes.as_slice().chain(BrokenPipeReader);
    let mut copy_input = CopyInput::from_input_reader(input_reader, false);
    copy_input
        .read_next_copy_part()
        .expect("the first part reads");

    let mut whole_input = Vec::new();
    let read_error = copy_input
        .write_whole_input(&mut whole_input)
        .expect_err("the read of the rest fails");

    assert_eq!(read_error.to_string(), "the pipe broke");
    assert!(
        whole_input == read_input_bytes,
        "the real tool reads every byte read before the failure"
    );
}

#[test]
fn a_failed_write_to_the_real_tool_ends_the_writing_with_no_error() {
    let mut copy_input = CopyInput::from_input_reader(&b"abc"[..], false);
    copy_input.read_next_copy_part().expect("a slice reads");
    let mut real_tool_input_bytes = [0; 2];

    copy_input
        .write_whole_input(&mut &mut real_tool_input_bytes[..])
        .expect("a failed write is no error");

    assert_eq!(&real_tool_input_bytes, b"ab");
}

#[test]
fn a_copy_whose_second_part_meets_the_session_replacing_its_image_goes_again_from_its_first_part() {
    let runtime_directory = build_test_runtime_directory();
    let in_session_context = InSessionContext {
        session_id: SessionId::new(),
        client_id: None,
        pane_id: PaneId::new(),
    };
    let (new_image_part_sender, new_image_part_receiver) = mpsc::channel();
    let session_thread = spawn_session_replacing_its_image(
        runtime_directory.path(),
        in_session_context.session_id,
        "the token of the image replaced",
        "the token of the new image",
        |request_kind| match request_kind {
            IpcRequestKind::ClipboardCopyPart { .. } => IpcResult::ClipboardCopyPartTaken,
            _ => build_restarting_refusal(),
        },
        move |request_kind| match request_kind {
            IpcRequestKind::ClipboardCopyPart { copied_bytes } => {
                let _ = new_image_part_sender.send(copied_bytes.len());
                IpcResult::ClipboardCopyPartTaken
            }
            IpcRequestKind::ClipboardCopy { copied_bytes, .. } => {
                let _ = new_image_part_sender.send(copied_bytes.len());
                IpcResult::ClipboardCopyRouted {
                    should_run_clipboard_tool: false,
                }
            }
            unexpected_request_kind => panic!("unexpected {unexpected_request_kind:?}"),
        },
    );
    let input_bytes = build_letter_bytes(MAX_CLIPBOARD_COPY_PART_BYTE_COUNT + 1);
    let mut copy_input = CopyInput::from_input_reader(input_bytes.as_slice(), false);

    let copy_routing = route_copy_input_to_session(
        &mut copy_input,
        "c",
        runtime_directory.path(),
        &in_session_context,
    )
    .expect("the input reads");

    session_thread.join().expect("the stand-in session exits");
    assert!(
        matches!(copy_routing, CopyRouting::Routed),
        "expected Routed, got {copy_routing:?}"
    );
    assert_eq!(
        new_image_part_receiver.try_iter().collect::<Vec<usize>>(),
        vec![MAX_CLIPBOARD_COPY_PART_BYTE_COUNT, 1]
    );
}
