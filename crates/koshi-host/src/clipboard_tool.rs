//! The clipboard tools that koshi stands in for inside a pane, and the folder
//! of stand-ins that one session puts first on the `PATH` of its panes.
//!
//! On Linux and macOS, `refresh_clipboard_tool_directory` fills that folder
//! with one symbolic link to the koshi program per tool in
//! `CLIPBOARD_TOOL_NAMES` that `find_real_clipboard_tool` finds on
//! `PATH`. A pane that runs `xclip` then runs koshi under the name `xclip`,
//! and koshi runs the real `xclip` that `find_real_clipboard_tool` finds
//! whenever the copy stays on this machine.
//!
//! Example: with `PATH` `/usr/bin:/bin` and `/usr/bin/xclip` the only tool
//! installed, the folder holds one entry, `xclip`, a link to the koshi program.

#[cfg(unix)]
use std::ffi::OsStr;
#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::path::{Path, PathBuf};

#[cfg(unix)]
use crate::execute_permission::can_user_execute;
#[cfg(unix)]
use crate::program_path::resolve_program_path;

#[cfg(all(test, unix))]
mod tests;

/// The end of the name of the folder that holds the stand-in clipboard tools
/// of one session. The folder is `session-<uuid>.clipboard-tools`, directly
/// inside the runtime directory.
pub const CLIPBOARD_TOOL_DIRECTORY_SUFFIX: &str = ".clipboard-tools";

/// The clipboard tools koshi stands in for, each by the name it runs under.
#[cfg(unix)]
pub const CLIPBOARD_TOOL_NAMES: &[&str] = &["pbcopy", "xclip", "xsel", "wl-copy"];

/// The real `tool_name`: the first regular file named `tool_name` that this
/// user may run, in the order of `path_entries`, a `PATH` value. The path
/// handed back is absolute.
///
/// Two kinds of match are passed over:
///
/// - every file in a directory whose name ends with
///   [`CLIPBOARD_TOOL_DIRECTORY_SUFFIX`];
/// - a file that is the program this process runs, reached through any
///   symbolic link.
///
/// An empty entry names the working directory, and a relative entry is read
/// from the working directory. `None` when no entry holds a match, and when
/// `path_entries` is `None`.
///
/// Example: `path_entries`
/// `/tmp/koshi-1000/session-<uuid>.clipboard-tools:/usr/local/bin:/usr/bin`,
/// with an `xclip` in the first and the last directory, gives
/// `/usr/bin/xclip`.
#[cfg(unix)]
#[must_use]
pub fn find_real_clipboard_tool(tool_name: &str, path_entries: Option<&OsStr>) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt as _;

    let program_file_metadata = resolve_program_path().and_then(std::fs::metadata).ok();
    for path_directory in std::env::split_paths(path_entries?) {
        let is_clipboard_tool_directory =
            path_directory.file_name().is_some_and(|directory_name| {
                directory_name
                    .as_encoded_bytes()
                    .ends_with(CLIPBOARD_TOOL_DIRECTORY_SUFFIX.as_bytes())
            });
        if is_clipboard_tool_directory {
            continue;
        }
        let Ok(tool_path) = std::path::absolute(path_directory.join(tool_name)) else {
            continue;
        };
        let Ok(tool_file_metadata) = std::fs::metadata(&tool_path) else {
            continue;
        };
        if !tool_file_metadata.is_file() || !can_user_execute(&tool_path) {
            continue;
        }
        let is_program_file = program_file_metadata
            .as_ref()
            .is_some_and(|program_file_metadata| {
                program_file_metadata.dev() == tool_file_metadata.dev()
                    && program_file_metadata.ino() == tool_file_metadata.ino()
            });
        if !is_program_file {
            return Some(tool_path);
        }
    }
    None
}

/// Make `clipboard_tool_directory` hold one entry for each tool in
/// [`CLIPBOARD_TOOL_NAMES`] that [`find_real_clipboard_tool`] finds in
/// `path_entries`, and nothing else. Each entry is a symbolic link named after
/// its tool, and its target is `program_path`.
///
/// 1. The folder is created with mode `0700` when it is not there. A folder
///    already there keeps its mode.
/// 2. Every other entry is removed: the link of a tool that is not found, a
///    link to another target, and any other file.
/// 3. Each link that is not there is created.
///
/// `program_path` is the absolute path of the koshi program. A link holds it
/// as written: a new program file renamed over that path runs through the
/// same link.
///
/// Example: `path_entries` that hold `xclip` alone, and a folder that holds
/// the links `xclip` and `xsel`, leave the folder holding the link `xclip`.
///
/// # Errors
/// The first failure to create the folder, read it, remove an entry, or
/// create a link. Each step done before that failure stays done.
#[cfg(unix)]
pub fn refresh_clipboard_tool_directory(
    clipboard_tool_directory: &Path,
    program_path: &Path,
    path_entries: Option<&OsStr>,
) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    match std::fs::DirBuilder::new()
        .mode(0o700)
        .create(clipboard_tool_directory)
    {
        Ok(()) => {}
        Err(create_error) if create_error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(create_error) => return Err(create_error),
    }
    let installed_tool_names: Vec<&str> = CLIPBOARD_TOOL_NAMES
        .iter()
        .copied()
        .filter(|tool_name| find_real_clipboard_tool(tool_name, path_entries).is_some())
        .collect();
    for directory_entry in std::fs::read_dir(clipboard_tool_directory)? {
        let entry_path = directory_entry?.path();
        let is_installed_tool_link = entry_path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|entry_name| installed_tool_names.contains(&entry_name))
            && std::fs::read_link(&entry_path).is_ok_and(|link_target| link_target == program_path);
        if !is_installed_tool_link {
            std::fs::remove_file(&entry_path)?;
        }
    }
    for tool_name in installed_tool_names {
        match std::os::unix::fs::symlink(program_path, clipboard_tool_directory.join(tool_name)) {
            Ok(()) => {}
            Err(link_error) if link_error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(link_error) => return Err(link_error),
        }
    }
    Ok(())
}
