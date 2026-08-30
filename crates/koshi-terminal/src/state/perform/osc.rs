//! OSC parsing for shell reports and working-directory updates.

use std::path::PathBuf;

use percent_encoding::percent_decode;

use crate::state::ReportedCwd;

/// A semantic shell marker carried by OSC 133.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Osc133 {
    /// The shell started a prompt.
    Prompt,
    /// The shell started receiving command input.
    Input,
    /// The shell started executing the command.
    CommandStart,
    /// The shell finished the command and may have reported an exit code.
    CommandFinished(Option<i32>),
}

/// Parse an OSC 133 payload, split on `;` by `vte`, into its marker.
///
/// Accepts exactly `133;A`, `133;B`, `133;C`, `133;D`, and `133;D;<code>`
/// where `<code>` parses as an `i32` (`0`, `137`, `-1`, `+3`). Anything else —
/// another command number, a marker other than `A`–`D`, an extra parameter, a
/// `D` code that is empty or not a decimal `i32` — yields `None`.
pub(super) fn parse_osc133(params: &[&[u8]]) -> Option<Osc133> {
    let [command, marker, rest @ ..] = params else {
        return None;
    };
    if *command != b"133" {
        return None;
    }
    match (*marker, rest) {
        (b"A", []) => Some(Osc133::Prompt),
        (b"B", []) => Some(Osc133::Input),
        (b"C", []) => Some(Osc133::CommandStart),
        (b"D", []) => Some(Osc133::CommandFinished(None)),
        (b"D", [exit_code]) => {
            let exit_code = std::str::from_utf8(exit_code).ok()?.parse().ok()?;
            Some(Osc133::CommandFinished(Some(exit_code)))
        }
        _ => None,
    }
}

/// The longest OSC 7 URI [`parse_osc7_cwd`] accepts. A longer one yields
/// `None` and leaves the last reported working directory in place.
pub(super) const MAX_OSC7_URI_BYTES: usize = 4 * 1024;

/// The bytes an OSC 7 payload carries ahead of the URI on the wire: the command
/// number `7` and the `;` after it.
const OSC7_PAYLOAD_PREFIX: usize = 2;

// A URI of exactly `MAX_OSC7_URI_BYTES` fits the parser's OSC buffer whole. A
// URI the parser cut short is longer than the limit and yields `None`.
const _: () = assert!(MAX_OSC7_URI_BYTES + OSC7_PAYLOAD_PREFIX <= crate::engine::OSC_CAPACITY);

/// Parse an OSC 7 cwd URI (`file://host/path`) into a [`ReportedCwd`], or
/// `None` when it is not a `file://` URI, carries no `/` after the authority,
/// is longer than [`MAX_OSC7_URI_BYTES`], or decodes to a path holding a NUL
/// byte.
///
/// The scheme `file` compares case-insensitively (RFC 3986 §3.1); `://`
/// compares exactly. The `host` is the authority between `//` and the
/// first `/`, decoded lossily (a non-UTF-8 byte becomes U+FFFD) and filtered
/// by [`sanitize_reported_text`](koshi_core::text::sanitize_reported_text);
/// an empty authority (`file:///path`) gives `host: None`. The path keeps its
/// leading `/`, is percent-decoded (`%20` → space, `%C3%A9` → `é`; a `%` not
/// followed by two hex digits stays literal), and becomes a [`PathBuf`] via
/// [`bytes_to_path`]. `?` and `#` are ordinary path bytes.
pub(super) fn parse_osc7_cwd(uri: &[u8]) -> Option<ReportedCwd> {
    if uri.len() < 7 || uri.len() > MAX_OSC7_URI_BYTES {
        return None;
    }
    if !uri[..4].eq_ignore_ascii_case(b"file") || &uri[4..7] != b"://" {
        return None;
    }
    let rest = &uri[7..];
    let slash = rest.iter().position(|&b| b == b'/')?;
    let host = match &rest[..slash] {
        [] => None,
        bytes => Some(koshi_core::text::sanitize_reported_text(
            &String::from_utf8_lossy(bytes),
        )),
    };
    let decoded = percent_decode(&rest[slash..]).collect::<Vec<u8>>();
    if decoded.contains(&0) {
        return None;
    }
    let path = bytes_to_path(decoded)?;
    Some(ReportedCwd { host, path })
}

/// Turn percent-decoded path bytes into a [`PathBuf`]. The bytes become an
/// `OsString` unchanged; a path that is not valid UTF-8 survives intact.
#[cfg(unix)]
fn bytes_to_path(decoded: Vec<u8>) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(decoded)))
}

/// Turn percent-decoded path bytes into a [`PathBuf`], or `None` when they are
/// not valid UTF-8. A leading `/` before a drive letter is dropped:
/// `/C:/Users` → `C:/Users`.
#[cfg(windows)]
fn bytes_to_path(mut decoded: Vec<u8>) -> Option<PathBuf> {
    let drive_prefixed =
        matches!(decoded.as_slice(), [b'/', drive, b':', ..] if drive.is_ascii_alphabetic());
    if drive_prefixed {
        decoded.remove(0);
    }
    String::from_utf8(decoded).ok().map(PathBuf::from)
}

#[cfg(test)]
mod tests;
