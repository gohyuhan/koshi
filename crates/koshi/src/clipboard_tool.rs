//! The stand-in clipboard tools: the koshi program run under the name of a
//! clipboard tool, through a link in the folder of stand-ins that a session
//! puts first on the `PATH` of its panes.
//!
//! A program in a pane that runs `pbcopy`, `xclip`, `xsel` or `wl-copy` runs
//! the stand-in. The stand-in reads the arguments as the real tool reads them
//! ([`parse_clipboard_tool_call`](crate::clipboard_tool::parse_clipboard_tool_call)):
//!
//! - A copy of text goes to the pane's session
//!   ([`submit_clipboard_copy`](koshi_link::ipc_client::submit_clipboard_copy)).
//!   Each client on another machine that focuses the pane gets the copy as an
//!   `OSC 52` host write. When the session answers that the real tool runs,
//!   the real tool runs with the same arguments and the same input, and the
//!   stand-in exits with the real tool's exit code. The bytes of a copy pick
//!   no type: `pbcopy` input that starts with `{\rtf`, which the real `pbcopy`
//!   keeps as rich text, goes to the session as its RTF source.
//! - Every other call runs the real tool in place of the stand-in, with
//!   standard input untouched: a read such as `xclip -o`, a clear, a copy
//!   whose arguments name another type than plain text, such as
//!   `xclip -target image/png`, and every argument the stand-in does not
//!   read.
//!
//! The real tool is the one
//! [`find_real_clipboard_tool`](koshi_host::clipboard_tool::find_real_clipboard_tool)
//! finds on `PATH`. A stand-in that finds none exits with code 127.
//!
//! Example: `printf hi | xclip -selection clipboard` in a pane that only a
//! client on another machine focuses puts `hi` on that client's clipboard,
//! and the real `xclip` does not run.

use std::ffi::{OsStr, OsString};
use std::io::{IsTerminal, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use koshi_host::clipboard_tool::{find_real_clipboard_tool, CLIPBOARD_TOOL_NAMES};
use koshi_host::program_path::resolve_program_path;
use koshi_link::in_session::InSessionContext;
use koshi_link::ipc_client::{submit_clipboard_copy, SESSION_ANSWER_TIMEOUT_DURATION};
use koshi_link::process::exec_and_keep_ignoring_sigpipe;

#[cfg(test)]
mod tests;

/// The exit code of a stand-in that finds no real tool on `PATH`: 127, the
/// code a shell gives for a command it does not find.
const MISSING_CLIPBOARD_TOOL_EXIT_CODE: u8 = 127;

/// The exit code of a stand-in whose real tool does not start: 126, the code a
/// shell gives for a command it finds and cannot run.
const UNSTARTABLE_CLIPBOARD_TOOL_EXIT_CODE: u8 = 126;

/// Every option `xclip` reads, as the option table of `xclip.c` lists them.
const XCLIP_OPTION_NAMES: [&str; 18] = [
    "-loops",
    "-display",
    "-selection",
    "-filter",
    "-in",
    "-out",
    "-version",
    "-help",
    "-silent",
    "-quiet",
    "-verbose",
    "-debug",
    "-noutf8",
    "-target",
    "-alt-text",
    "-rmlastnl",
    "-sensitive",
    "-wait",
];

/// What one run of a stand-in clipboard tool does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardToolCall {
    /// Copy text to the selections `clipboard_targets` names, in `OSC 52`
    /// spelling: `c` for the clipboard, `p` for the primary selection, and `q`
    /// for the secondary selection.
    Copy {
        /// The `OSC 52` targets of the selection the real tool sets.
        clipboard_targets: &'static str,
        /// Where the text of the copy comes from.
        copy_source: CopySource,
    },
    /// Run the real tool with the same arguments, and with standard input
    /// untouched.
    PassThrough,
}

/// Where the text of a copy comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopySource {
    /// Every byte of standard input.
    StandardInput {
        /// One `\n` at the very end of the input stays out of the copy.
        should_trim_last_newline: bool,
    },
    /// The words given as arguments, joined by one space each: `wl-copy hi
    /// there` copies `hi there`.
    ArgumentText(String),
}

/// The clipboard tool this process runs as: the name in
/// [`CLIPBOARD_TOOL_NAMES`] that is the file name of the program path
/// ([`resolve_program_path`]). `None` for every other file name, and when the
/// program path cannot be read.
///
/// Example: the program path `/run/koshi/session-<uuid>.clipboard-tools/xclip`
/// gives `xclip`, and `/usr/local/bin/koshi` gives `None`.
#[must_use]
pub fn find_invoked_clipboard_tool() -> Option<&'static str> {
    let program_path = resolve_program_path().ok()?;
    let program_file_name = program_path.file_name()?;
    CLIPBOARD_TOOL_NAMES
        .iter()
        .copied()
        .find(|tool_name| program_file_name == OsStr::new(tool_name))
}

/// Run as the stand-in for `tool_name`, a name in [`CLIPBOARD_TOOL_NAMES`],
/// with this process's arguments and standard input, and hand back the exit
/// code.
///
/// 1. The arguments are read by [`parse_clipboard_tool_call`]. An argument
///    that is not UTF-8 makes the call a pass-through.
/// 2. A pass-through runs the real tool in place of this process, with
///    standard input untouched.
/// 3. A copy reads its text: every byte of standard input, or the argument
///    text. A failed read of standard input prints the failure and exits with
///    code 1.
/// 4. The copy goes to the session that the pane's `KOSHI_*` variables name
///    ([`submit_clipboard_copy`], within [`SESSION_ANSWER_TIMEOUT_DURATION`]).
///    The real tool runs with the copy when the copy is not UTF-8, when the
///    variables are missing or broken, when the runtime directory is unknown,
///    when asking the session fails, and when the session answers so. It then
///    reads the same bytes as the stand-in read, before any newline was
///    trimmed, and the stand-in exits with its exit code, or with 128 plus the
///    number of the signal that ended it. Otherwise the stand-in exits with
///    code 0.
///
/// The real tool is the first `tool_name` that [`find_real_clipboard_tool`]
/// finds on this process's `PATH`. When it finds none, the stand-in prints a
/// message and exits with code 127. A real tool that does not start prints
/// the failure, and the stand-in exits with code 126.
#[must_use]
pub fn run_clipboard_tool(tool_name: &str) -> ExitCode {
    let mut invocation_arguments = std::env::args_os();
    let invoked_name = invocation_arguments
        .next()
        .unwrap_or_else(|| OsString::from(tool_name));
    let tool_arguments: Vec<OsString> = invocation_arguments.collect();
    let tool_argument_texts: Option<Vec<&str>> = tool_arguments
        .iter()
        .map(|tool_argument| tool_argument.to_str())
        .collect();
    let clipboard_tool_call = match tool_argument_texts {
        Some(tool_argument_texts) => parse_clipboard_tool_call(
            tool_name,
            &tool_argument_texts,
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal(),
        ),
        None => ClipboardToolCall::PassThrough,
    };
    let ClipboardToolCall::Copy {
        clipboard_targets,
        copy_source,
    } = clipboard_tool_call
    else {
        return run_real_clipboard_tool(tool_name, &invoked_name, &tool_arguments, None);
    };
    match copy_source {
        CopySource::ArgumentText(argument_text) => {
            if should_run_real_clipboard_tool(clipboard_targets, argument_text.as_bytes()) {
                return run_real_clipboard_tool(tool_name, &invoked_name, &tool_arguments, None);
            }
        }
        CopySource::StandardInput {
            should_trim_last_newline,
        } => {
            let mut standard_input_bytes = Vec::new();
            if let Err(read_error) = std::io::stdin().read_to_end(&mut standard_input_bytes) {
                eprintln!("koshi: {tool_name}: reading standard input failed: {read_error}");
                return ExitCode::FAILURE;
            }
            let mut copied_bytes = standard_input_bytes.as_slice();
            if should_trim_last_newline {
                copied_bytes = copied_bytes.strip_suffix(b"\n").unwrap_or(copied_bytes);
            }
            if should_run_real_clipboard_tool(clipboard_targets, copied_bytes) {
                return run_real_clipboard_tool(
                    tool_name,
                    &invoked_name,
                    &tool_arguments,
                    Some(&standard_input_bytes),
                );
            }
        }
    }
    ExitCode::SUCCESS
}

/// What the stand-in for `tool_name` does with `tool_arguments`, the
/// arguments after the program name, read as the real tool reads them.
/// `is_stdin_terminal` and `is_stdout_terminal` say whether standard input and
/// standard output are terminals: `xclip` and `xsel` choose between a copy and
/// a read with them.
///
/// Each tool makes a [`ClipboardToolCall::Copy`] only for these calls, and a
/// [`ClipboardToolCall::PassThrough`] for every other call. A plain text type
/// is `text/plain`, `text/plain;charset=utf-8`, `UTF8_STRING`, `STRING` or
/// `TEXT`, spelled exactly so.
///
/// - `pbcopy`: no argument, or `-pboard general`. The target is `c`.
/// - `xclip`: each argument names the `xclip` option it spells whole, else the
///   one option it is the start of: `-sel` is `-selection`, and `-s`, the
///   start of three options, names none. The call reads standard input:
///   `-in`, or no `-out` and standard input is not a terminal. The last of
///   `-in` and `-out` counts. The other options are `-selection` (its first
///   letter, in any case: `c` gives `c`, `s` gives `q`, and every other
///   letter but `b` gives `p`, the target with no `-selection`), `-rmlastnl`
///   (trims the last newline), `-target` with a plain text type, `-loops`,
///   `-display`, `-silent`, `-quiet`, `-verbose`, `-debug` and `-noutf8`.
/// - `xsel`: each argument of 3 or more characters that starts with one `-`
///   splits into one option per letter first, so `-ib` reads as `-i -b`. The
///   call reads standard input and writes nothing: `-i`/`--input` and no
///   `-o`/`--output`, or neither and only standard output is a terminal. The
///   target is `c` with `-b`/`--clipboard`, else `q` when the last of
///   `-p`/`--primary` and `-s`/`--secondary` is `-s`, else `p`. The other
///   options are `--trim` (trims the last newline), `-n`/`--nodetach`,
///   `-v`/`--verbose`, and `--display`, `-t`/`--selectionTimeout`,
///   `-l`/`--logfile`, `-m`/`--name` and `--windowName` with their value.
/// - `wl-copy`: options come first, then the words to copy. Words make the
///   argument text, and no words read standard input. The target is `p` with
///   `-p`/`--primary`, else `c`. The other options are `-n`/`--trim-newline`
///   (trims the last newline of standard input), `-o`/`--paste-once`,
///   `-f`/`--foreground`, `-s`/`--seat` with its value, and `-t`/`--type`
///   with a plain text type. `--` ends the options, and `--seat=<seat>` and
///   `--type=<type>` give the value in the same argument. An argument that
///   starts with `-` after the first word passes through.
///
/// Example: `xclip` with `["-sel", "clip"]` and standard input from a pipe
/// copies standard input to `c`, and `xclip` with `["-o"]` passes through.
#[must_use]
pub fn parse_clipboard_tool_call(
    tool_name: &str,
    tool_arguments: &[&str],
    is_stdin_terminal: bool,
    is_stdout_terminal: bool,
) -> ClipboardToolCall {
    match tool_name {
        "pbcopy" => parse_pbcopy_call(tool_arguments),
        "xclip" => parse_xclip_call(tool_arguments, is_stdin_terminal),
        "xsel" => parse_xsel_call(tool_arguments, is_stdin_terminal, is_stdout_terminal),
        "wl-copy" => parse_wl_copy_call(tool_arguments),
        _ => ClipboardToolCall::PassThrough,
    }
}

/// The `pbcopy` call `tool_arguments` makes, as [`parse_clipboard_tool_call`]
/// states.
fn parse_pbcopy_call(tool_arguments: &[&str]) -> ClipboardToolCall {
    match tool_arguments {
        [] | ["-pboard", "general"] => ClipboardToolCall::Copy {
            clipboard_targets: "c",
            copy_source: CopySource::StandardInput {
                should_trim_last_newline: false,
            },
        },
        _ => ClipboardToolCall::PassThrough,
    }
}

/// The `xclip` call `tool_arguments` makes, as [`parse_clipboard_tool_call`]
/// states.
fn parse_xclip_call(tool_arguments: &[&str], is_stdin_terminal: bool) -> ClipboardToolCall {
    let mut is_reading_input = !is_stdin_terminal;
    let mut selection_name = "";
    let mut should_trim_last_newline = false;
    let mut remaining_arguments = tool_arguments.iter();
    while let Some(tool_argument) = remaining_arguments.next() {
        match find_xclip_option(tool_argument) {
            Some("-in") => is_reading_input = true,
            Some("-out") => is_reading_input = false,
            Some("-rmlastnl") => should_trim_last_newline = true,
            Some("-silent" | "-quiet" | "-verbose" | "-debug" | "-noutf8") => {}
            Some("-selection") => match remaining_arguments.next() {
                Some(selection_value) => selection_name = selection_value,
                None => return ClipboardToolCall::PassThrough,
            },
            Some("-loops" | "-display") => {
                if remaining_arguments.next().is_none() {
                    return ClipboardToolCall::PassThrough;
                }
            }
            Some("-target") => match remaining_arguments.next() {
                Some(content_type) if is_plain_text_type(content_type) => {}
                _ => return ClipboardToolCall::PassThrough,
            },
            _ => return ClipboardToolCall::PassThrough,
        }
    }
    if !is_reading_input {
        return ClipboardToolCall::PassThrough;
    }
    let first_selection_letter = selection_name
        .bytes()
        .next()
        .map(|selection_letter| selection_letter.to_ascii_lowercase());
    let clipboard_targets = match first_selection_letter {
        Some(b'c') => "c",
        Some(b's') => "q",
        Some(b'b') => return ClipboardToolCall::PassThrough,
        _ => "p",
    };
    ClipboardToolCall::Copy {
        clipboard_targets,
        copy_source: CopySource::StandardInput {
            should_trim_last_newline,
        },
    }
}

/// The option in [`XCLIP_OPTION_NAMES`] that `xclip_argument` names, read as
/// `XrmParseCommand` reads it: the option it spells whole, else the one option
/// it is the start of. `None` when it names no option, or starts more than
/// one: `xclip` reads such an argument as a file name.
///
/// Example: `-sel` gives `-selection` and `-i` gives `-in`. `-s` gives `None`,
/// since it starts `-selection`, `-silent` and `-sensitive`, and so does
/// `-input`.
fn find_xclip_option(xclip_argument: &str) -> Option<&'static str> {
    if let Some(option_name) = XCLIP_OPTION_NAMES
        .iter()
        .copied()
        .find(|option_name| *option_name == xclip_argument)
    {
        return Some(option_name);
    }
    let mut started_option_names = XCLIP_OPTION_NAMES
        .iter()
        .copied()
        .filter(|option_name| option_name.starts_with(xclip_argument));
    match (started_option_names.next(), started_option_names.next()) {
        (Some(option_name), None) => Some(option_name),
        _ => None,
    }
}

/// The `xsel` call `tool_arguments` makes, as [`parse_clipboard_tool_call`]
/// states.
fn parse_xsel_call(
    tool_arguments: &[&str],
    is_stdin_terminal: bool,
    is_stdout_terminal: bool,
) -> ClipboardToolCall {
    let mut has_input_option = false;
    let mut has_output_option = false;
    let mut has_clipboard_option = false;
    let mut is_secondary_selection = false;
    let mut should_trim_last_newline = false;
    let expanded_arguments = expand_xsel_short_options(tool_arguments);
    let mut remaining_arguments = expanded_arguments.iter().map(String::as_str);
    while let Some(tool_argument) = remaining_arguments.next() {
        match tool_argument {
            "-i" | "--input" => has_input_option = true,
            "-o" | "--output" => has_output_option = true,
            "-b" | "--clipboard" => has_clipboard_option = true,
            "-p" | "--primary" => is_secondary_selection = false,
            "-s" | "--secondary" => is_secondary_selection = true,
            "--trim" => should_trim_last_newline = true,
            "-n" | "--nodetach" | "-v" | "--verbose" => {}
            "--display" | "-t" | "--selectionTimeout" | "-l" | "--logfile" | "-m" | "--name"
            | "--windowName" => {
                if remaining_arguments.next().is_none() {
                    return ClipboardToolCall::PassThrough;
                }
            }
            _ => return ClipboardToolCall::PassThrough,
        }
    }
    let is_copy_from_input = has_input_option || (!is_stdin_terminal && is_stdout_terminal);
    if has_output_option || !is_copy_from_input {
        return ClipboardToolCall::PassThrough;
    }
    let clipboard_targets = if has_clipboard_option {
        "c"
    } else if is_secondary_selection {
        "q"
    } else {
        "p"
    };
    ClipboardToolCall::Copy {
        clipboard_targets,
        copy_source: CopySource::StandardInput {
            should_trim_last_newline,
        },
    }
}

/// `tool_arguments` with each argument of 3 or more characters that starts
/// with one `-` split into one option per character, as `xsel` splits its
/// arguments before it reads them.
///
/// Example: `["-ib", "--input", "-t"]` gives `["-i", "-b", "--input", "-t"]`.
fn expand_xsel_short_options(tool_arguments: &[&str]) -> Vec<String> {
    let mut expanded_arguments = Vec::with_capacity(tool_arguments.len());
    for tool_argument in tool_arguments {
        match tool_argument.strip_prefix('-') {
            Some(option_letters)
                if option_letters.chars().count() > 1 && !option_letters.starts_with('-') =>
            {
                expanded_arguments.extend(
                    option_letters
                        .chars()
                        .map(|option_letter| format!("-{option_letter}")),
                );
            }
            _ => expanded_arguments.push((*tool_argument).to_string()),
        }
    }
    expanded_arguments
}

/// The `wl-copy` call `tool_arguments` makes, as [`parse_clipboard_tool_call`]
/// states.
fn parse_wl_copy_call(tool_arguments: &[&str]) -> ClipboardToolCall {
    let mut is_primary_selection = false;
    let mut should_trim_last_newline = false;
    let mut has_option_end = false;
    let mut argument_index = 0;
    while let Some(&tool_argument) = tool_arguments.get(argument_index) {
        if tool_argument == "--" {
            has_option_end = true;
            argument_index += 1;
            break;
        }
        if !tool_argument.starts_with('-') || tool_argument == "-" {
            break;
        }
        argument_index += 1;
        match tool_argument {
            "-p" | "--primary" => is_primary_selection = true,
            "-n" | "--trim-newline" => should_trim_last_newline = true,
            "-o" | "--paste-once" | "-f" | "--foreground" => {}
            "-s" | "--seat" => {
                if tool_arguments.get(argument_index).is_none() {
                    return ClipboardToolCall::PassThrough;
                }
                argument_index += 1;
            }
            "-t" | "--type" => match tool_arguments.get(argument_index) {
                Some(content_type) if is_plain_text_type(content_type) => argument_index += 1,
                _ => return ClipboardToolCall::PassThrough,
            },
            seat_option if seat_option.starts_with("--seat=") => {}
            other_option => match other_option.strip_prefix("--type=") {
                Some(content_type) if is_plain_text_type(content_type) => {}
                _ => return ClipboardToolCall::PassThrough,
            },
        }
    }
    let words = &tool_arguments[argument_index..];
    let has_option_after_word = !has_option_end
        && words
            .iter()
            .any(|word| word.starts_with('-') && *word != "-");
    if has_option_after_word {
        return ClipboardToolCall::PassThrough;
    }
    let clipboard_targets = if is_primary_selection { "p" } else { "c" };
    let copy_source = if words.is_empty() {
        CopySource::StandardInput {
            should_trim_last_newline,
        }
    } else {
        CopySource::ArgumentText(words.join(" "))
    };
    ClipboardToolCall::Copy {
        clipboard_targets,
        copy_source,
    }
}

/// Whether `content_type`, the value of `xclip -target` or `wl-copy --type`,
/// names plain text: `text/plain`, `text/plain;charset=utf-8`, `UTF8_STRING`,
/// `STRING` or `TEXT`, spelled exactly so.
fn is_plain_text_type(content_type: &str) -> bool {
    matches!(
        content_type,
        "text/plain" | "text/plain;charset=utf-8" | "UTF8_STRING" | "STRING" | "TEXT"
    )
}

/// Whether the real tool runs with `copied_bytes`, a copy for
/// `clipboard_targets`.
///
/// `true` when `copied_bytes` is not UTF-8, when the `KOSHI_*` variables of a
/// pane are missing or broken ([`InSessionContext::from_env`]), when the
/// runtime directory is unknown, when asking the session fails, and when the
/// session answers so ([`submit_clipboard_copy`], within
/// [`SESSION_ANSWER_TIMEOUT_DURATION`]). `false` when the session answers that
/// each client that focuses the pane got the copy.
fn should_run_real_clipboard_tool(clipboard_targets: &str, copied_bytes: &[u8]) -> bool {
    if std::str::from_utf8(copied_bytes).is_err() {
        return true;
    }
    let Ok(Some(in_session_context)) = InSessionContext::from_env() else {
        return true;
    };
    let Some(runtime_directory) = koshi_paths::resolve_runtime_directory() else {
        return true;
    };
    submit_clipboard_copy(
        &runtime_directory,
        &in_session_context,
        clipboard_targets,
        copied_bytes,
        Instant::now() + SESSION_ANSWER_TIMEOUT_DURATION,
    )
    .unwrap_or(true)
}

/// Run the real `tool_name`, found by [`find_real_clipboard_tool`] on this
/// process's `PATH`, with `tool_arguments` and the program name
/// `invoked_name`, and hand back the exit code.
///
/// - With `standard_input_bytes`, the real tool reads those bytes on its
///   standard input. The stand-in waits for it and exits with its exit code,
///   or with 128 plus the number of the signal that ended it.
/// - With `None`, the real tool replaces this process, with standard input
///   untouched.
///
/// No real tool found prints a message and gives
/// [`MISSING_CLIPBOARD_TOOL_EXIT_CODE`]. A real tool that does not start
/// prints the failure and gives [`UNSTARTABLE_CLIPBOARD_TOOL_EXIT_CODE`]. A
/// failed wait prints the failure and gives 1.
fn run_real_clipboard_tool(
    tool_name: &str,
    invoked_name: &OsStr,
    tool_arguments: &[OsString],
    standard_input_bytes: Option<&[u8]>,
) -> ExitCode {
    let Some(real_tool_path) =
        find_real_clipboard_tool(tool_name, std::env::var_os("PATH").as_deref())
    else {
        eprintln!("koshi: no {tool_name} found on PATH outside koshi's stand-in folder");
        return ExitCode::from(MISSING_CLIPBOARD_TOOL_EXIT_CODE);
    };
    let mut real_tool_command = Command::new(&real_tool_path);
    real_tool_command.arg0(invoked_name).args(tool_arguments);
    let Some(standard_input_bytes) = standard_input_bytes else {
        let exec_error = exec_and_keep_ignoring_sigpipe(&mut real_tool_command);
        eprintln!("koshi: {}: {exec_error}", real_tool_path.display());
        return ExitCode::from(UNSTARTABLE_CLIPBOARD_TOOL_EXIT_CODE);
    };
    let mut real_tool_process = match real_tool_command.stdin(Stdio::piped()).spawn() {
        Ok(real_tool_process) => real_tool_process,
        Err(spawn_error) => {
            eprintln!("koshi: {}: {spawn_error}", real_tool_path.display());
            return ExitCode::from(UNSTARTABLE_CLIPBOARD_TOOL_EXIT_CODE);
        }
    };
    if let Some(mut real_tool_input) = real_tool_process.stdin.take() {
        // A real tool that stops reading leaves the rest of the bytes
        // unwritten, and its exit code still stands.
        let _ = real_tool_input.write_all(standard_input_bytes);
    }
    match real_tool_process.wait() {
        Ok(exit_status) => {
            let exit_code = match exit_status.code() {
                Some(exit_code) => exit_code,
                None => 128 + exit_status.signal().unwrap_or_default(),
            };
            ExitCode::from(u8::try_from(exit_code).unwrap_or(u8::MAX))
        }
        Err(wait_error) => {
            eprintln!("koshi: {}: {wait_error}", real_tool_path.display());
            ExitCode::FAILURE
        }
    }
}
