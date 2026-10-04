//! Which koshi a server runs, read from the program file the server wrote
//! beside its endpoint file, and the hint that follows a refusal of this
//! build's protocol version.
//!
//! Example: a session that runs `0.5.0` from `/usr/local/bin/koshi`, the file
//! this `0.6.0` koshi runs from, is [`RefusingServerBuild::Older`] with
//! [`ProgramFileMatch::Same`]: it restarts into `0.6.0` by itself, and the
//! caller waits for that restart.

use std::cmp::Ordering;
use std::path::Path;

use koshi_core::text::sanitize_reported_text;
use koshi_ipc::endpoint::{find_endpoint_process_record, EndpointFile, ServerProgramFile};
use koshi_ipc::error::IpcError;
use semver::Version;

#[cfg(test)]
mod tests;

/// This build's version.
const CURRENT_BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How the program file a server restarts into compares with another program
/// file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramFileMatch {
    /// Both paths name the same file once every symbolic link is followed.
    Same,
    /// Both paths name files, and they are different files.
    Other,
    /// At least one path names no file this process can follow.
    Undetermined,
}

/// Which koshi a server that refused this build's protocol version runs, as
/// the program file it wrote states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusingServerBuild {
    /// A koshi version newer than this one.
    Newer(ServerProgramFile),
    /// A koshi version older than this one, and how the server's program file
    /// compares with the file this koshi runs from.
    Older {
        server_program_file: ServerProgramFile,
        program_file_match: ProgramFileMatch,
    },
    /// The same version as this one, or a version that is not semver.
    Unordered(ServerProgramFile),
    /// No program file from the process the endpoint file names: a koshi
    /// built before servers wrote program files, a server between two process
    /// images, or a process that no longer runs.
    NotRecorded,
    /// A program file that cannot be read. Carries the failure.
    Unreadable(String),
}

impl RefusingServerBuild {
    /// Whether the server may still restart into this koshi by itself: an
    /// older server whose program file is this koshi's file, or cannot be
    /// compared with it, and a server whose program file is missing or
    /// cannot be read. `false` for a newer server, an unordered one, and an
    /// older one whose program file is another file.
    #[must_use]
    pub fn is_restart_expected(&self) -> bool {
        match self {
            RefusingServerBuild::Older {
                program_file_match: ProgramFileMatch::Same | ProgramFileMatch::Undetermined,
                ..
            }
            | RefusingServerBuild::NotRecorded
            | RefusingServerBuild::Unreadable(_) => true,
            RefusingServerBuild::Older {
                program_file_match: ProgramFileMatch::Other,
                ..
            }
            | RefusingServerBuild::Newer(_)
            | RefusingServerBuild::Unordered(_) => false,
        }
    }

    /// The clause that follows the server's refusal: which koshi the server
    /// runs, and what to do. `end_instruction` is the clause that ends the
    /// server, such as `end it with: koshi kill-session session-<uuid>`, and
    /// follows each hint that offers it. The server's version and path pass
    /// through [`sanitize_reported_text`].
    ///
    /// Example, on a `0.6.0` build: a newer server gives `it runs koshi 0.7.0
    /// from /usr/local/bin/koshi, which is newer than this koshi 0.6.0; use
    /// /usr/local/bin/koshi for it`.
    #[must_use]
    pub fn format_refusal_hint(&self, end_instruction: &str) -> String {
        match self {
            RefusingServerBuild::Newer(server_program_file) => {
                let program_path = sanitize_reported_text(&server_program_file.program_path);
                format!(
                    "it runs koshi {} from {program_path}, which is newer than this koshi \
                     {CURRENT_BUILD_VERSION}; use {program_path} for it",
                    sanitize_reported_text(&server_program_file.build_version)
                )
            }
            RefusingServerBuild::Older {
                server_program_file,
                program_file_match: ProgramFileMatch::Same,
            } => format!(
                "it runs koshi {} and has not restarted into this koshi {CURRENT_BUILD_VERSION} \
                 yet; it tries again at each command from this koshi, and its log says what \
                 stopped it",
                sanitize_reported_text(&server_program_file.build_version)
            ),
            RefusingServerBuild::Older {
                server_program_file,
                program_file_match: ProgramFileMatch::Undetermined,
            } if !Path::new(&server_program_file.program_path).exists() => format!(
                "it runs koshi {} from {}, which no longer exists; {end_instruction}",
                sanitize_reported_text(&server_program_file.build_version),
                sanitize_reported_text(&server_program_file.program_path)
            ),
            RefusingServerBuild::Older {
                server_program_file,
                program_file_match: ProgramFileMatch::Undetermined,
            } => format!(
                "it runs koshi {} from {} and has not restarted into this koshi \
                 {CURRENT_BUILD_VERSION} yet; it tries again at each command from this koshi, \
                 and its log says what stopped it",
                sanitize_reported_text(&server_program_file.build_version),
                sanitize_reported_text(&server_program_file.program_path)
            ),
            RefusingServerBuild::Older {
                server_program_file,
                program_file_match: ProgramFileMatch::Other,
            } => format!(
                "it runs koshi {} from {}, a program file this koshi does not replace; use that \
                 koshi for it, or {end_instruction}",
                sanitize_reported_text(&server_program_file.build_version),
                sanitize_reported_text(&server_program_file.program_path)
            ),
            RefusingServerBuild::Unordered(server_program_file) => format!(
                "it runs koshi {} from {}; use that koshi for it, or {end_instruction}",
                sanitize_reported_text(&server_program_file.build_version),
                sanitize_reported_text(&server_program_file.program_path)
            ),
            RefusingServerBuild::NotRecorded => format!(
                "it runs a koshi older than {CURRENT_BUILD_VERSION} that cannot restart into it; \
                 {end_instruction}"
            ),
            RefusingServerBuild::Unreadable(program_file_error) => format!(
                "koshi cannot tell which koshi it runs ({}); {end_instruction}",
                sanitize_reported_text(program_file_error)
            ),
        }
    }
}

/// Which koshi the server that wrote the program file at `program_file_path`
/// runs, compared with this koshi, when the process `refused_process_id`
/// refused this build's protocol version.
///
/// This koshi runs from the path
/// [`resolve_program_path`](koshi_host::program_path::resolve_program_path)
/// gives. A path it cannot give compares as [`ProgramFileMatch::Undetermined`].
#[must_use]
pub fn find_refusing_server_build(
    program_file_path: &Path,
    refused_process_id: u32,
) -> RefusingServerBuild {
    match find_server_program_file(program_file_path, refused_process_id) {
        Err(program_file_error) => RefusingServerBuild::Unreadable(program_file_error.to_string()),
        Ok(None) => RefusingServerBuild::NotRecorded,
        Ok(Some(server_program_file)) => {
            let current_program_path = koshi_host::program_path::resolve_program_path().ok();
            compare_server_build(
                server_program_file,
                CURRENT_BUILD_VERSION,
                current_program_path.as_deref(),
            )
        }
    }
}

/// The program file at `program_file_path`, when the server the endpoint
/// file at `endpoint_file_path` names wrote it, as [`find_server_program_file`]
/// states. `None` when either file is missing or cannot be read.
#[must_use]
pub fn find_advertised_server_program_file(
    program_file_path: &Path,
    endpoint_file_path: &Path,
) -> Option<ServerProgramFile> {
    let endpoint_file = EndpointFile::load_from_path(endpoint_file_path).ok()?;
    find_server_program_file(program_file_path, endpoint_file.process_id)
        .ok()
        .flatten()
}

/// The program file at `program_file_path`, when the running process
/// `process_id` wrote it: the file names that process, and that process
/// started in the whole second of the file's modification time, or earlier,
/// as [`find_endpoint_process_record`] reads it.
///
/// `Ok(None)` for no file, for a file that names another process, and when
/// [`find_endpoint_process_record`] finds no process: `process_id` no longer
/// runs, cannot be read, or started after that second.
///
/// # Errors
/// [`IpcError::ProgramFileUnreadable`] for a file that cannot be read or
/// decoded.
pub fn find_server_program_file(
    program_file_path: &Path,
    process_id: u32,
) -> Result<Option<ServerProgramFile>, IpcError> {
    let Some(server_program_file) = ServerProgramFile::load_from_path(program_file_path)? else {
        return Ok(None);
    };
    if server_program_file.process_id != process_id
        || find_endpoint_process_record(program_file_path, process_id).is_none()
    {
        return Ok(None);
    }
    Ok(Some(server_program_file))
}

/// `server_program_file` compared with the koshi build `current_build_version`
/// that runs from `current_program_path`. `None` for `current_program_path`
/// compares as [`ProgramFileMatch::Undetermined`].
///
/// Example: `0.5.0` against `0.6.0` is [`RefusingServerBuild::Older`],
/// `0.7.0` is [`RefusingServerBuild::Newer`], and `0.6.0` or `dev` is
/// [`RefusingServerBuild::Unordered`].
fn compare_server_build(
    server_program_file: ServerProgramFile,
    current_build_version: &str,
    current_program_path: Option<&Path>,
) -> RefusingServerBuild {
    let version_order = match (
        Version::parse(&server_program_file.build_version),
        Version::parse(current_build_version),
    ) {
        (Ok(server_version), Ok(current_version)) => server_version.cmp(&current_version),
        _ => return RefusingServerBuild::Unordered(server_program_file),
    };
    match version_order {
        Ordering::Greater => RefusingServerBuild::Newer(server_program_file),
        Ordering::Equal => RefusingServerBuild::Unordered(server_program_file),
        Ordering::Less => {
            let program_file_match = match current_program_path {
                Some(current_program_path) => compare_program_files(
                    Path::new(&server_program_file.program_path),
                    current_program_path,
                ),
                None => ProgramFileMatch::Undetermined,
            };
            RefusingServerBuild::Older {
                server_program_file,
                program_file_match,
            }
        }
    }
}

/// How `server_program_path` compares with `current_program_path` once every
/// symbolic link in each is followed.
///
/// Example: `/usr/local/bin/koshi`, a link to `/opt/koshi/0.6.0/koshi`,
/// against `/opt/koshi/0.6.0/koshi` is [`ProgramFileMatch::Same`].
#[must_use]
pub fn compare_program_files(
    server_program_path: &Path,
    current_program_path: &Path,
) -> ProgramFileMatch {
    match (
        std::fs::canonicalize(server_program_path),
        std::fs::canonicalize(current_program_path),
    ) {
        (Ok(server_file_path), Ok(current_file_path)) if server_file_path == current_file_path => {
            ProgramFileMatch::Same
        }
        (Ok(_), Ok(_)) => ProgramFileMatch::Other,
        _ => ProgramFileMatch::Undetermined,
    }
}
