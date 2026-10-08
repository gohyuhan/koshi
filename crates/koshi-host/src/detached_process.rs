//! Starting a process that outlives the process that starts it.

use std::process::{Command, Stdio};

/// Set `command` to start a process that outlives this one: a process group of
/// its own, and input and output going nowhere. On Windows the process also
/// gets no console and does not inherit the caller's: the creation flags are
/// `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`.
///
/// Hands back the same `command` for the caller to spawn.
pub fn configure_detached_process(command: &mut Command) -> &mut Command {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command
}
