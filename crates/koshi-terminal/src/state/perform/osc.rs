//! OSC 7 working-directory parsing: turn a shell's `file://host/path` report
//! into a [`ReportedCwd`], honoring each platform's path encoding.

use std::path::PathBuf;

use percent_encoding::percent_decode;

use crate::state::ReportedCwd;

/// Parse an OSC 7 cwd URI (`file://host/path`) into a [`ReportedCwd`], or
/// `None` when it is not a `file://` URI or carries no path.
///
/// The `host` component (between `//` and the next `/`) is kept on the result —
/// the spawn layer needs it to tell a local report from a remote one. An empty
/// authority (`file:///path`) yields no host. The path keeps its leading `/`
/// and is percent-decoded (`%20` → space, `%C3%A9` → `é`) before being turned
/// into a [`PathBuf`] by [`bytes_to_path`]; a decoded NUL byte (which cannot
/// occur in a real path) rejects the whole report.
pub(super) fn parse_osc7_cwd(uri: &[u8]) -> Option<ReportedCwd> {
    // The `file` scheme is case-insensitive (RFC 3986 §3.1: schemes compare
    // case-insensitively); the `//` authority separator and the path are not.
    if uri.len() < 7 || !uri[..4].eq_ignore_ascii_case(b"file") || &uri[4..7] != b"://" {
        return None;
    }
    let rest = &uri[7..];
    let slash = rest.iter().position(|&b| b == b'/')?;
    // The authority between `//` and the first `/` is the host; an empty
    // authority means no host. Hosts are ASCII in practice, so decode lossily.
    let host = match &rest[..slash] {
        [] => None,
        bytes => Some(String::from_utf8_lossy(bytes).into_owned()),
    };
    let decoded = percent_decode(&rest[slash..]).collect::<Vec<u8>>();
    // A NUL cannot appear in a real path; reject so a malformed report never
    // stores an unusable directory (and never silently truncates at a later
    // filesystem call).
    if decoded.contains(&0) {
        return None;
    }
    let path = bytes_to_path(decoded)?;
    Some(ReportedCwd { host, path })
}

/// Turn percent-decoded path bytes into a [`PathBuf`], honoring each platform's
/// path encoding. On Unix the bytes become an `OsString` directly, so a path
/// that is not valid UTF-8 survives intact. On Windows the bytes are required
/// UTF-8 (invalid → `None`) and a `file:///C:/…` URI's leading slash before the
/// drive letter is stripped so the resulting path is well-formed.
#[cfg(unix)]
fn bytes_to_path(decoded: Vec<u8>) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(decoded)))
}

#[cfg(windows)]
fn bytes_to_path(mut decoded: Vec<u8>) -> Option<PathBuf> {
    // `file:///C:/Users` gives path bytes `/C:/Users`; drop the leading slash
    // before the drive letter so the drive-rooted path is well-formed.
    let drive_prefixed =
        matches!(decoded.as_slice(), [b'/', drive, b':', ..] if drive.is_ascii_alphabetic());
    if drive_prefixed {
        decoded.remove(0);
    }
    String::from_utf8(decoded).ok().map(PathBuf::from)
}
