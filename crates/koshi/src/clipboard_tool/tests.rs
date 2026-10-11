//! Tests for the stand-in clipboard tools: how the arguments of `pbcopy`,
//! `xclip`, `xsel` and `wl-copy` read as a copy or a pass-through, the `xclip`
//! option prefixes, the `xsel` option letters, and the plain text types.

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
