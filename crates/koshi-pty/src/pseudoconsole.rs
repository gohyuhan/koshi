//! A Windows pane's pseudoconsole and the program that runs in it.
//!
//! [`spawn_pane_program`] opens a pseudoconsole on two anonymous pipes and
//! starts the pane program attached to it with `CreateProcessW`. The program
//! starts inside the pane's two jobs, in the order
//! [`PaneJob::list_job_handles`] gives them (`PROC_THREAD_ATTRIBUTE_JOB_LIST`):
//! the shared job that ends every pane when this process ends, then the pane's
//! own job. Every process the program starts belongs to both jobs from its
//! first instruction.
//!
//! The pseudoconsole is created with `PSEUDOCONSOLE_INHERIT_CURSOR`,
//! `PSEUDOCONSOLE_RESIZE_QUIRK` and `PSEUDOCONSOLE_WIN32_INPUT_MODE`. It writes
//! a cursor-position request (`ESC [ 6 n`) to its output and holds the
//! program's output until a reply arrives on its input. `CreatePseudoConsole`,
//! `ResizePseudoConsole` and `ClosePseudoConsole` are looked up in
//! `kernel32.dll` at run time. A Windows that lacks them (before Windows 10
//! version 1809) opens no pane.
//!
//! The pipe ends the pseudoconsole holds are closed once the program has
//! started. A spawn that fails after the pseudoconsole exists closes the output
//! pipe's read end and the input pipe's write end before `ClosePseudoConsole`
//! runs.
//!
//! The program's environment is this process's environment, then the system
//! environment in the registry without `USERNAME`, then the user environment in
//! the registry, then koshi's overlay. Names match and sort as Windows compares
//! them ([`EnvironmentVariableKey`]). A user `Path` is appended to the `Path`
//! before it, after a `;`. The program is found in that environment's `PATH`:
//! in each folder, the name as given, then the name with each `PATHEXT`
//! extension appended. Example: `nu` with `PATH=C:\tools` and
//! `PATHEXT=.COM;.EXE` starts `C:\tools\nu.EXE` when that file exists.

use std::{
    cmp::Ordering,
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::File,
    io,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use koshi_core::process::{PtySize, SpawnSpec};
use windows_sys::{
    core::HRESULT,
    s, w,
    Win32::{
        Foundation::{
            ERROR_MORE_DATA, ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE, S_OK, TRUE, WAIT_FAILED,
        },
        Globalization::{CompareStringOrdinal, CSTR_EQUAL, CSTR_GREATER_THAN, CSTR_LESS_THAN},
        System::{
            Console::{COORD, HPCON, PSEUDOCONSOLE_INHERIT_CURSOR},
            Environment::ExpandEnvironmentStringsW,
            LibraryLoader::{GetModuleHandleW, GetProcAddress},
            Pipes::CreatePipe,
            Registry::{
                RegCloseKey, RegEnumValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER,
                HKEY_LOCAL_MACHINE, KEY_READ, REG_EXPAND_SZ, REG_SZ,
            },
            Threading::{
                CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
                InitializeProcThreadAttributeList, TerminateProcess, UpdateProcThreadAttribute,
                WaitForSingleObject, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
                INFINITE, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_JOB_LIST,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESTDHANDLES, STARTUPINFOEXW,
            },
        },
    },
};

use crate::{env::build_environment_overlay, error::PtyError, kill::PaneJob};

/// `PSEUDOCONSOLE_RESIZE_QUIRK` in `src/winconpty/winconpty.h` of
/// microsoft/terminal `v1.18.3181.0`.
const PSEUDOCONSOLE_RESIZE_QUIRK: u32 = 0x2;

/// `PSEUDOCONSOLE_WIN32_INPUT_MODE` in `src/winconpty/winconpty.h` of
/// microsoft/terminal `v1.18.3181.0`.
const PSEUDOCONSOLE_WIN32_INPUT_MODE: u32 = 0x4;

/// The flags every pane's pseudoconsole is created with.
const PANE_PSEUDOCONSOLE_FLAGS: u32 =
    PSEUDOCONSOLE_INHERIT_CURSOR | PSEUDOCONSOLE_RESIZE_QUIRK | PSEUDOCONSOLE_WIN32_INPUT_MODE;

/// The registry key under `HKEY_LOCAL_MACHINE` that holds the system
/// environment.
const SYSTEM_ENVIRONMENT_KEY_PATH: &str =
    r"System\CurrentControlSet\Control\Session Manager\Environment";

/// The registry key under `HKEY_CURRENT_USER` that holds the user environment.
const USER_ENVIRONMENT_KEY_PATH: &str = "Environment";

/// The extensions a program search appends when the environment has no
/// `PATHEXT`.
const DEFAULT_PROGRAM_EXTENSIONS: &str = ".EXE";

/// UTF-16 units in the longest registry value name, 16,383 characters, and its
/// terminating NUL.
const REGISTRY_VALUE_NAME_UNIT_COUNT: usize = 16_384;

/// UTF-16 units a registry value read starts with. A longer value is read again
/// at the size the registry reports.
const INITIAL_REGISTRY_VALUE_UNIT_COUNT: usize = 1_024;

/// The two attributes a pane program starts with: its pseudoconsole and its
/// jobs.
const PANE_PROCESS_ATTRIBUTE_COUNT: u32 = 2;

/// `CreatePseudoConsole` as `kernel32.dll` exports it.
type CreatePseudoConsoleFunction =
    unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> HRESULT;

/// `ResizePseudoConsole` as `kernel32.dll` exports it.
type ResizePseudoConsoleFunction = unsafe extern "system" fn(HPCON, COORD) -> HRESULT;

/// `ClosePseudoConsole` as `kernel32.dll` exports it.
type ClosePseudoConsoleFunction = unsafe extern "system" fn(HPCON);

/// What `GetProcAddress` hands back for a function it found.
type ExportedFunction = unsafe extern "system" fn() -> isize;

/// The three pseudoconsole functions of `kernel32.dll`.
struct PseudoConsoleFunctions {
    create_pseudo_console: CreatePseudoConsoleFunction,
    resize_pseudo_console: ResizePseudoConsoleFunction,
    close_pseudo_console: ClosePseudoConsoleFunction,
}

/// The pseudoconsole functions, looked up on first use. `None` when
/// `kernel32.dll` exports one of the three under no name.
static PSEUDO_CONSOLE_FUNCTIONS: OnceLock<Option<PseudoConsoleFunctions>> = OnceLock::new();

/// Look up `CreatePseudoConsole`, `ResizePseudoConsole` and
/// `ClosePseudoConsole` in `kernel32.dll`, once per process.
///
/// # Errors
/// Returns [`PtyError::Spawn`] carrying `this Windows has no pseudoconsole:
/// panes need Windows 10 version 1809 or newer` when one of the three is
/// missing.
fn load_pseudo_console_functions() -> Result<&'static PseudoConsoleFunctions, PtyError> {
    PSEUDO_CONSOLE_FUNCTIONS
        // SAFETY: `w!` and `s!` give NUL-terminated names. `GetModuleHandleW`
        // hands back the loaded `kernel32.dll` with no reference to release,
        // and `kernel32.dll` exports the three functions with the signatures
        // of `CreatePseudoConsoleFunction`, `ResizePseudoConsoleFunction` and
        // `ClosePseudoConsoleFunction`.
        .get_or_init(|| unsafe {
            let kernel32_module = GetModuleHandleW(w!("kernel32.dll"));
            if kernel32_module.is_null() {
                return None;
            }
            let create_pseudo_console = GetProcAddress(kernel32_module, s!("CreatePseudoConsole"))?;
            let resize_pseudo_console = GetProcAddress(kernel32_module, s!("ResizePseudoConsole"))?;
            let close_pseudo_console = GetProcAddress(kernel32_module, s!("ClosePseudoConsole"))?;
            Some(PseudoConsoleFunctions {
                create_pseudo_console: std::mem::transmute::<
                    ExportedFunction,
                    CreatePseudoConsoleFunction,
                >(create_pseudo_console),
                resize_pseudo_console: std::mem::transmute::<
                    ExportedFunction,
                    ResizePseudoConsoleFunction,
                >(resize_pseudo_console),
                close_pseudo_console: std::mem::transmute::<
                    ExportedFunction,
                    ClosePseudoConsoleFunction,
                >(close_pseudo_console),
            })
        })
        .as_ref()
        .ok_or_else(|| PtyError::Spawn {
            detail: "this Windows has no pseudoconsole: panes need Windows 10 version 1809 or \
                     newer"
                .to_string(),
        })
}

/// A pane's open pseudoconsole. Dropping it runs `ClosePseudoConsole`.
///
/// Before Windows 11 24H2 the close waits until every program attached to the
/// pseudoconsole has let it go and its output has been read out or its output
/// pipe's read end is closed.
pub(crate) struct PaneConsole {
    pseudo_console_handle: HPCON,
    pseudo_console_functions: &'static PseudoConsoleFunctions,
}

impl PaneConsole {
    /// Tell the pseudoconsole, and the program in it, that the pane is now
    /// `pty_size`.
    ///
    /// # Errors
    /// Returns [`io::ErrorKind::InvalidInput`] when a side of `pty_size` is past
    /// 32767 cells, and an error carrying `ResizePseudoConsole to
    /// <columns>x<rows> failed: HRESULT <code>` when the call fails.
    pub(crate) fn resize(&self, pty_size: PtySize) -> io::Result<()> {
        let console_size = build_console_size(pty_size)?;
        // SAFETY: `pseudo_console_handle` stays open until `self` drops.
        let resize_result = unsafe {
            (self.pseudo_console_functions.resize_pseudo_console)(
                self.pseudo_console_handle,
                console_size,
            )
        };
        if resize_result != S_OK {
            return Err(io::Error::other(format!(
                "ResizePseudoConsole to {}x{} failed: HRESULT {resize_result:#010x}",
                pty_size.column_count, pty_size.row_count
            )));
        }
        Ok(())
    }
}

impl Drop for PaneConsole {
    fn drop(&mut self) {
        // SAFETY: `pseudo_console_handle` is open, and this drop closes it
        // once.
        unsafe { (self.pseudo_console_functions.close_pseudo_console)(self.pseudo_console_handle) };
    }
}

/// The program that runs in a pane's pseudoconsole.
pub(crate) struct PaneProcess {
    process_id: u32,
    /// The process handle `CreateProcessW` returned, shared with the pane's
    /// kill control.
    process_handle: Arc<OwnedHandle>,
}

impl PaneProcess {
    /// The program's process ID.
    pub(crate) fn get_process_id(&self) -> u32 {
        self.process_id
    }

    /// A second owner of the program's process handle.
    pub(crate) fn share_process_handle(&self) -> Arc<OwnedHandle> {
        Arc::clone(&self.process_handle)
    }

    /// End the program with exit code 1 (`TerminateProcess`). The processes it
    /// started keep running.
    ///
    /// # Errors
    /// Returns the OS error when `TerminateProcess` fails.
    pub(crate) fn kill(&mut self) -> io::Result<()> {
        // SAFETY: `self.process_handle` keeps the process handle open.
        if unsafe { TerminateProcess(self.process_handle.as_raw_handle(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Wait until the program has ended, and return its exit code.
    ///
    /// # Errors
    /// Returns the OS error when the wait or `GetExitCodeProcess` fails.
    pub(crate) fn wait_for_exit_code(&self) -> io::Result<u32> {
        let process_handle = self.process_handle.as_raw_handle();
        // SAFETY: `self.process_handle` keeps the process handle open.
        if unsafe { WaitForSingleObject(process_handle, INFINITE) } == WAIT_FAILED {
            return Err(io::Error::last_os_error());
        }
        let mut exit_code: u32 = 0;
        // SAFETY: `self.process_handle` keeps the process handle open, and
        // `exit_code` is a writable `u32`.
        if unsafe { GetExitCodeProcess(process_handle, &mut exit_code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(exit_code)
    }
}

/// A pane program that has started in its pseudoconsole, and the pane's ends
/// of the two pipes.
pub(crate) struct SpawnedPaneProgram {
    pub(crate) pane_console: PaneConsole,
    /// The read end of the pseudoconsole's output pipe.
    pub(crate) output_reader: File,
    /// The write end of the pseudoconsole's input pipe.
    pub(crate) input_writer: File,
    pub(crate) pane_process: PaneProcess,
}

/// Start `spawn_spec`'s program in a new pseudoconsole of `pty_size`, inside
/// `pane_job`'s jobs.
///
/// The program, its arguments, its working directory and its environment
/// follow the module documentation. A working directory that is not a folder
/// falls back to the environment's `USERPROFILE` when that is a folder, and to
/// this process's working directory otherwise. `None` uses this process's
/// working directory.
///
/// # Errors
/// Returns [`PtyError::Spawn`] when the pseudoconsole functions are missing,
/// when a side of `pty_size` is past 32767 cells, when the program, an
/// argument, the working directory or an environment entry holds a NUL
/// character, when a pipe, the pseudoconsole or the attribute list cannot be
/// created, or when `CreateProcessW` fails. Nothing keeps running after an
/// error.
pub(crate) fn spawn_pane_program(
    spawn_spec: &SpawnSpec,
    pty_size: PtySize,
    pane_job: &PaneJob,
) -> Result<SpawnedPaneProgram, PtyError> {
    let build_spawn_error = |detail: String| PtyError::Spawn { detail };

    let pseudo_console_functions = load_pseudo_console_functions()?;
    let console_size = build_console_size(pty_size)
        .map_err(|size_error| build_spawn_error(size_error.to_string()))?;

    let environment_variables = merge_environment_variables(
        std::env::vars_os(),
        list_registry_environment_variables(HKEY_LOCAL_MACHINE, SYSTEM_ENVIRONMENT_KEY_PATH),
        list_registry_environment_variables(HKEY_CURRENT_USER, USER_ENVIRONMENT_KEY_PATH),
        &build_environment_overlay(spawn_spec),
    );
    let mut environment_block = build_environment_block(&environment_variables)?;
    let program_path = find_program_path(
        spawn_spec.program.as_os_str(),
        find_environment_variable(&environment_variables, "PATH"),
        find_environment_variable(&environment_variables, "PATHEXT"),
    );
    let program_path_units = encode_wide_with_nul(program_path.as_os_str(), "the program")?;
    let mut command_line_units = build_command_line(&program_path, &spawn_spec.arguments)?;
    let requested_working_directory = match &spawn_spec.working_directory {
        Some(working_directory) => Some(working_directory.clone()),
        None => std::env::current_dir().ok(),
    };
    let working_directory = resolve_working_directory(
        requested_working_directory.as_deref(),
        find_environment_variable(&environment_variables, "USERPROFILE"),
    );
    let working_directory_units = match &working_directory {
        Some(working_directory) => Some(encode_wide_with_nul(
            working_directory.as_os_str(),
            "the working directory",
        )?),
        None => None,
    };

    let (console_input_reader, input_writer) = create_pipe()
        .map_err(|pipe_error| build_spawn_error(format!("CreatePipe failed: {pipe_error}")))?;
    let (output_reader, console_output_writer) = create_pipe()
        .map_err(|pipe_error| build_spawn_error(format!("CreatePipe failed: {pipe_error}")))?;

    let mut pseudo_console_handle: HPCON = 0;
    // SAFETY: both pipe handles are open, and `pseudo_console_handle` is a
    // writable `HPCON`.
    let create_result = unsafe {
        (pseudo_console_functions.create_pseudo_console)(
            console_size,
            console_input_reader.as_raw_handle(),
            console_output_writer.as_raw_handle(),
            PANE_PSEUDOCONSOLE_FLAGS,
            &mut pseudo_console_handle,
        )
    };
    if create_result != S_OK {
        return Err(build_spawn_error(format!(
            "CreatePseudoConsole failed: HRESULT {create_result:#010x}"
        )));
    }
    let pane_console = PaneConsole {
        pseudo_console_handle,
        pseudo_console_functions,
    };
    // Bound after `pane_console`: an early return below drops both pipe ends
    // before the pseudoconsole closes.
    let output_reader = File::from(output_reader);
    let input_writer = File::from(input_writer);

    let job_handles = pane_job.list_job_handles();
    let mut process_attributes = ProcessAttributeList::with_attribute_count(
        PANE_PROCESS_ATTRIBUTE_COUNT,
    )
    .map_err(|attribute_error| {
        build_spawn_error(format!(
            "InitializeProcThreadAttributeList failed: {attribute_error}"
        ))
    })?;
    // SAFETY: `job_handles` and `pane_console` outlive `process_attributes`,
    // which points at both until it is dropped.
    unsafe {
        process_attributes
            .set_attribute(
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                pane_console.pseudo_console_handle as *const std::ffi::c_void,
                std::mem::size_of::<HPCON>(),
            )
            .and_then(|()| {
                process_attributes.set_attribute(
                    PROC_THREAD_ATTRIBUTE_JOB_LIST,
                    job_handles.as_ptr().cast(),
                    std::mem::size_of_val(&job_handles),
                )
            })
    }
    .map_err(|attribute_error| {
        build_spawn_error(format!(
            "UpdateProcThreadAttribute failed: {attribute_error}"
        ))
    })?;

    let mut startup_information = STARTUPINFOEXW::default();
    startup_information.StartupInfo.cb = u32::try_from(std::mem::size_of::<STARTUPINFOEXW>())
        .expect("STARTUPINFOEXW is far below 4 GiB");
    startup_information.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup_information.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
    startup_information.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
    startup_information.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
    startup_information.lpAttributeList = process_attributes.as_attribute_list_pointer();

    let mut process_information = PROCESS_INFORMATION::default();
    // SAFETY: `program_path_units`, `working_directory_units` and
    // `environment_block` end as `CreateProcessW` reads them, and
    // `command_line_units` is a writable NUL-terminated buffer.
    // `startup_information` and its attribute list live for the call.
    let create_process_result = unsafe {
        CreateProcessW(
            program_path_units.as_ptr(),
            command_line_units.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            environment_block.as_mut_ptr().cast(),
            working_directory_units
                .as_ref()
                .map_or(std::ptr::null(), |working_directory_units| {
                    working_directory_units.as_ptr()
                }),
            &startup_information.StartupInfo,
            &mut process_information,
        )
    };
    if create_process_result == 0 {
        let create_process_error = io::Error::last_os_error();
        return Err(build_spawn_error(format!(
            "CreateProcessW `{}` in `{}` failed: {create_process_error}",
            OsString::from_wide(&command_line_units[..command_line_units.len() - 1]).display(),
            working_directory
                .as_deref()
                .unwrap_or(Path::new(""))
                .display(),
        )));
    }
    // SAFETY: `CreateProcessW` succeeded, so `hProcess` is an open handle that
    // nothing else owns.
    let process_handle = unsafe { OwnedHandle::from_raw_handle(process_information.hProcess) };
    // SAFETY: `CreateProcessW` succeeded, so `hThread` is an open handle that
    // nothing else owns.
    drop(unsafe { OwnedHandle::from_raw_handle(process_information.hThread) });
    drop(process_attributes);
    drop(console_input_reader);
    drop(console_output_writer);

    Ok(SpawnedPaneProgram {
        pane_console,
        output_reader,
        input_writer,
        pane_process: PaneProcess {
            process_id: process_information.dwProcessId,
            process_handle: Arc::new(process_handle),
        },
    })
}

/// Convert `pty_size` into a console size.
///
/// # Errors
/// Returns [`io::ErrorKind::InvalidInput`] carrying `<columns>x<rows> is past
/// the console's 32767 cells on a side` when either side is above 32767.
fn build_console_size(pty_size: PtySize) -> io::Result<COORD> {
    match (
        i16::try_from(pty_size.column_count),
        i16::try_from(pty_size.row_count),
    ) {
        (Ok(column_count), Ok(row_count)) => Ok(COORD {
            X: column_count,
            Y: row_count,
        }),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{}x{} is past the console's 32767 cells on a side",
                pty_size.column_count, pty_size.row_count
            ),
        )),
    }
}

/// Create an anonymous pipe whose handles no child inherits, and return its
/// read end and its write end.
///
/// # Errors
/// Returns the OS error when `CreatePipe` fails.
fn create_pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read_handle: HANDLE = std::ptr::null_mut();
    let mut write_handle: HANDLE = std::ptr::null_mut();
    // SAFETY: both handle slots are writable, and NULL attributes give
    // handles that no child inherits.
    if unsafe { CreatePipe(&mut read_handle, &mut write_handle, std::ptr::null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `CreatePipe` succeeded, so both handles are open and nothing
    // else owns them.
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(read_handle),
            OwnedHandle::from_raw_handle(write_handle),
        )
    })
}

/// An initialized process attribute list. Dropping it runs
/// `DeleteProcThreadAttributeList`.
struct ProcessAttributeList {
    /// The list's memory, in `usize` units: it starts on a pointer-aligned
    /// address.
    attribute_list_units: Vec<usize>,
}

impl ProcessAttributeList {
    /// Initialize a list that holds `attribute_count` attributes.
    ///
    /// # Errors
    /// Returns the OS error when `InitializeProcThreadAttributeList` fails.
    fn with_attribute_count(attribute_count: u32) -> io::Result<Self> {
        let mut attribute_list_byte_count: usize = 0;
        // SAFETY: a NULL list asks only for the size, which the call writes to
        // `attribute_list_byte_count`.
        unsafe {
            InitializeProcThreadAttributeList(
                std::ptr::null_mut(),
                attribute_count,
                0,
                &mut attribute_list_byte_count,
            )
        };
        let mut attribute_list_units =
            vec![0_usize; attribute_list_byte_count.div_ceil(std::mem::size_of::<usize>())];
        // SAFETY: `attribute_list_units` holds at least
        // `attribute_list_byte_count` pointer-aligned bytes.
        if unsafe {
            InitializeProcThreadAttributeList(
                attribute_list_units.as_mut_ptr().cast(),
                attribute_count,
                0,
                &mut attribute_list_byte_count,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            attribute_list_units,
        })
    }

    /// The list as `STARTUPINFOEXW.lpAttributeList` takes it.
    fn as_attribute_list_pointer(&mut self) -> *mut std::ffi::c_void {
        self.attribute_list_units.as_mut_ptr().cast()
    }

    /// Set `attribute` to the `value_byte_count` bytes at `value_pointer`.
    ///
    /// # Safety
    /// `value_pointer` stays valid until this list is dropped.
    ///
    /// # Errors
    /// Returns the OS error when `UpdateProcThreadAttribute` fails.
    unsafe fn set_attribute(
        &mut self,
        attribute: u32,
        value_pointer: *const std::ffi::c_void,
        value_byte_count: usize,
    ) -> io::Result<()> {
        // SAFETY: the list is initialized, and the caller keeps
        // `value_pointer` valid until the list drops.
        if unsafe {
            UpdateProcThreadAttribute(
                self.as_attribute_list_pointer(),
                0,
                attribute as usize,
                value_pointer,
                value_byte_count,
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for ProcessAttributeList {
    fn drop(&mut self) {
        // SAFETY: `with_attribute_count` initialized the list, and this drop
        // deletes it once.
        unsafe { DeleteProcThreadAttributeList(self.as_attribute_list_pointer()) };
    }
}

/// One environment variable: the name it was last set under, and its value.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EnvironmentVariable {
    name: OsString,
    variable_value: OsString,
}

/// An environment variable name, compared as Windows compares names:
/// `CompareStringOrdinal` with `bIgnoreCase` set, which uppercases each UTF-16
/// unit with the system's table and compares the units by value. Example:
/// `Path` equals `PATH`, and `TERMINAL` sorts before `TERM_PROGRAM`.
#[derive(Debug, Clone)]
struct EnvironmentVariableKey {
    name_units: Vec<u16>,
}

impl EnvironmentVariableKey {
    /// The key of the variable name `name`.
    fn from_name(name: &OsStr) -> Self {
        Self {
            name_units: name.encode_wide().collect(),
        }
    }
}

impl Ord for EnvironmentVariableKey {
    /// # Panics
    /// Panics when a name holds 2^31 or more UTF-16 units, or when
    /// `CompareStringOrdinal` fails.
    fn cmp(&self, other_variable_key: &Self) -> Ordering {
        // SAFETY: each pointer and length pair describes one of the two
        // `name_units` vectors.
        let compare_result = unsafe {
            CompareStringOrdinal(
                self.name_units.as_ptr(),
                i32::try_from(self.name_units.len()).expect("a name holds under 2^31 units"),
                other_variable_key.name_units.as_ptr(),
                i32::try_from(other_variable_key.name_units.len())
                    .expect("a name holds under 2^31 units"),
                TRUE,
            )
        };
        match compare_result {
            CSTR_LESS_THAN => Ordering::Less,
            CSTR_EQUAL => Ordering::Equal,
            CSTR_GREATER_THAN => Ordering::Greater,
            _ => panic!(
                "CompareStringOrdinal failed: {}",
                io::Error::last_os_error()
            ),
        }
    }
}

impl PartialOrd for EnvironmentVariableKey {
    fn partial_cmp(&self, other_variable_key: &Self) -> Option<Ordering> {
        Some(self.cmp(other_variable_key))
    }
}

impl PartialEq for EnvironmentVariableKey {
    fn eq(&self, other_variable_key: &Self) -> bool {
        self.cmp(other_variable_key) == Ordering::Equal
    }
}

impl Eq for EnvironmentVariableKey {}

/// Environment variables keyed by the [`EnvironmentVariableKey`] of their
/// name. Inserting a variable replaces the one whose name it equals.
type EnvironmentVariableByKey = BTreeMap<EnvironmentVariableKey, EnvironmentVariable>;

/// The value of the variable whose name equals `name`, or `None` when there is
/// none.
fn find_environment_variable<'variables>(
    environment_variables: &'variables EnvironmentVariableByKey,
    name: &str,
) -> Option<&'variables OsStr> {
    environment_variables
        .get(&EnvironmentVariableKey::from_name(OsStr::new(name)))
        .map(|environment_variable| environment_variable.variable_value.as_os_str())
}

/// Merge the environment a pane program starts with.
///
/// Sets `process_variables`, then each of `system_variables` except
/// `USERNAME`, then each of `user_variables`, then each entry of
/// `environment_overlay`, each replacing the variable whose name it equals and
/// keeping its own casing. A user variable named `PATH` in any casing takes the
/// value set before it, `;`, then its own value.
///
/// Example: process `Path=C:\a`, system `Path=C:\s`, user `PATH=C:\u` →
/// `PATH=C:\s;C:\u`.
fn merge_environment_variables(
    process_variables: impl IntoIterator<Item = (OsString, OsString)>,
    system_variables: Vec<(OsString, OsString)>,
    user_variables: Vec<(OsString, OsString)>,
    environment_overlay: &BTreeMap<String, String>,
) -> EnvironmentVariableByKey {
    let username_key = EnvironmentVariableKey::from_name(OsStr::new("USERNAME"));
    let path_key = EnvironmentVariableKey::from_name(OsStr::new("PATH"));
    let mut environment_variables = EnvironmentVariableByKey::new();
    for (name, variable_value) in process_variables {
        environment_variables.insert(
            EnvironmentVariableKey::from_name(&name),
            EnvironmentVariable {
                name,
                variable_value,
            },
        );
    }
    for (name, variable_value) in system_variables {
        let name_key = EnvironmentVariableKey::from_name(&name);
        if name_key == username_key {
            continue;
        }
        environment_variables.insert(
            name_key,
            EnvironmentVariable {
                name,
                variable_value,
            },
        );
    }
    for (name, variable_value) in user_variables {
        let name_key = EnvironmentVariableKey::from_name(&name);
        let variable_value = match environment_variables.get(&name_key) {
            Some(earlier_variable) if name_key == path_key => {
                let mut merged_value = earlier_variable.variable_value.clone();
                merged_value.push(";");
                merged_value.push(&variable_value);
                merged_value
            }
            _ => variable_value,
        };
        environment_variables.insert(
            name_key,
            EnvironmentVariable {
                name,
                variable_value,
            },
        );
    }
    for (name, variable_value) in environment_overlay {
        environment_variables.insert(
            EnvironmentVariableKey::from_name(OsStr::new(name)),
            EnvironmentVariable {
                name: name.into(),
                variable_value: variable_value.into(),
            },
        );
    }
    environment_variables
}

/// Encode `environment_variables` as a `CreateProcessW` environment block:
/// `name=value` and a NUL for each variable, in [`EnvironmentVariableKey`]
/// order, then one more NUL.
///
/// Example: `TERM=xterm` and `Path=C:\s` encode as
/// `Path=C:\s\0TERM=xterm\0\0`.
///
/// # Errors
/// Returns [`PtyError::Spawn`] carrying `the environment variable <name>
/// holds a NUL character` when a name or a value holds one.
fn build_environment_block(
    environment_variables: &EnvironmentVariableByKey,
) -> Result<Vec<u16>, PtyError> {
    let mut environment_block = Vec::new();
    for environment_variable in environment_variables.values() {
        let entry_start = environment_block.len();
        environment_block.extend(environment_variable.name.encode_wide());
        environment_block.push(u16::from(b'='));
        environment_block.extend(environment_variable.variable_value.encode_wide());
        if environment_block[entry_start..].contains(&0) {
            return Err(PtyError::Spawn {
                detail: format!(
                    "the environment variable {} holds a NUL character",
                    environment_variable.name.display()
                ),
            });
        }
        environment_block.push(0);
    }
    environment_block.push(0);
    Ok(environment_block)
}

/// An open registry key. Dropping it runs `RegCloseKey`.
struct OpenRegistryKey(HKEY);

impl Drop for OpenRegistryKey {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the open key that `RegOpenKeyExW` returned, and
        // this drop closes it once.
        unsafe { RegCloseKey(self.0) };
    }
}

/// List the string values of `key_path` under `root_key` as environment
/// variables, in the order the registry returns them.
///
/// A `REG_SZ` value reads up to its first NUL. A `REG_EXPAND_SZ` value reads
/// up to its first NUL and expands with this process's environment
/// ([`expand_environment_strings`]). Values of other types are left out. A key
/// that cannot be opened lists nothing, and a read that fails ends the list. A
/// value whose data outgrows the buffer is read again at the size the registry
/// reports.
fn list_registry_environment_variables(
    root_key: HKEY,
    key_path: &str,
) -> Vec<(OsString, OsString)> {
    let key_path_units: Vec<u16> = OsStr::new(key_path).encode_wide().chain([0]).collect();
    let mut key_handle: HKEY = std::ptr::null_mut();
    // SAFETY: `key_path_units` is NUL-terminated, and `key_handle` is a
    // writable `HKEY`.
    if unsafe {
        RegOpenKeyExW(
            root_key,
            key_path_units.as_ptr(),
            0,
            KEY_READ,
            &mut key_handle,
        )
    } != ERROR_SUCCESS
    {
        return Vec::new();
    }
    let open_key = OpenRegistryKey(key_handle);

    let mut environment_variables = Vec::new();
    let mut value_name_units = vec![0_u16; REGISTRY_VALUE_NAME_UNIT_COUNT];
    let mut value_units = vec![0_u16; INITIAL_REGISTRY_VALUE_UNIT_COUNT];
    let mut value_index: u32 = 0;
    loop {
        let mut value_name_unit_count = u32::try_from(value_name_units.len())
            .expect("the value name buffer is far below 4 GiB");
        let value_buffer_byte_count = value_units.len() * 2;
        let mut value_byte_count = u32::try_from(value_buffer_byte_count).unwrap_or(u32::MAX);
        let mut value_type: u32 = 0;
        // SAFETY: `value_name_units` holds `value_name_unit_count` units,
        // `value_units` holds at least `value_byte_count` bytes, and every
        // other pointer is a writable local or NULL.
        let enumerate_result = unsafe {
            RegEnumValueW(
                open_key.0,
                value_index,
                value_name_units.as_mut_ptr(),
                &mut value_name_unit_count,
                std::ptr::null(),
                &mut value_type,
                value_units.as_mut_ptr().cast(),
                &mut value_byte_count,
            )
        };
        match enumerate_result {
            ERROR_SUCCESS => {}
            ERROR_MORE_DATA if value_byte_count as usize > value_buffer_byte_count => {
                value_units.resize((value_byte_count as usize).div_ceil(2), 0);
                continue;
            }
            _ => break,
        }
        value_index += 1;
        let read_units = read_until_nul(&value_units[..value_byte_count as usize / 2]);
        let variable_value = match value_type {
            REG_SZ => OsString::from_wide(read_units),
            REG_EXPAND_SZ => expand_environment_strings(read_units),
            _ => continue,
        };
        let name = OsString::from_wide(&value_name_units[..value_name_unit_count as usize]);
        environment_variables.push((name, variable_value));
    }
    environment_variables
}

/// The units of `text_units` before its first NUL, or all of them when it
/// holds none.
fn read_until_nul(text_units: &[u16]) -> &[u16] {
    let text_unit_count = text_units
        .iter()
        .position(|text_unit| *text_unit == 0)
        .unwrap_or(text_units.len());
    &text_units[..text_unit_count]
}

/// Replace each `%NAME%` in `unexpanded_units` with the value of `NAME` in
/// this process's environment (`ExpandEnvironmentStringsW`). A `%NAME%` that
/// names no variable stays as it is, and text that cannot expand comes back
/// unexpanded.
///
/// Example: `%SystemRoot%\system32` → `C:\Windows\system32`.
fn expand_environment_strings(unexpanded_units: &[u16]) -> OsString {
    let unexpanded_units_with_nul: Vec<u16> = unexpanded_units.iter().copied().chain([0]).collect();
    let mut expanded_units = vec![0_u16; unexpanded_units_with_nul.len()];
    loop {
        let expanded_buffer_unit_count = u32::try_from(expanded_units.len()).unwrap_or(u32::MAX);
        // SAFETY: `unexpanded_units_with_nul` is NUL-terminated, and
        // `expanded_units` holds at least `expanded_buffer_unit_count` units.
        let expanded_unit_count = unsafe {
            ExpandEnvironmentStringsW(
                unexpanded_units_with_nul.as_ptr(),
                expanded_units.as_mut_ptr(),
                expanded_buffer_unit_count,
            )
        } as usize;
        if expanded_unit_count == 0 {
            return OsString::from_wide(unexpanded_units);
        }
        if expanded_unit_count <= expanded_units.len() {
            return OsString::from_wide(read_until_nul(&expanded_units[..expanded_unit_count]));
        }
        expanded_units.resize(expanded_unit_count, 0);
    }
}

/// Find `program` in the folders of `path_value`.
///
/// In each folder, in order, the first file found is `program` as given, then
/// `program` with each extension of `program_extensions` appended. Without
/// `program_extensions` the one extension is `.EXE`. An empty folder is
/// skipped: the working folder is never searched. Without `path_value`, or
/// when no folder holds a match, `program` comes back as given. An absolute
/// `program` that is a file comes back as given.
///
/// Example: `nu` with `path_value` `C:\a;C:\tools` and `program_extensions`
/// `.COM;.EXE` → `C:\tools\nu.EXE` when only that file exists.
fn find_program_path(
    program: &OsStr,
    path_value: Option<&OsStr>,
    program_extensions: Option<&OsStr>,
) -> PathBuf {
    let Some(path_value) = path_value else {
        return PathBuf::from(program);
    };
    let program_extensions =
        program_extensions.unwrap_or_else(|| OsStr::new(DEFAULT_PROGRAM_EXTENSIONS));
    for folder in std::env::split_paths(path_value) {
        if folder.as_os_str().is_empty() {
            continue;
        }
        let candidate_path = folder.join(program);
        if candidate_path.is_file() {
            return candidate_path;
        }
        for program_extension in std::env::split_paths(program_extensions) {
            let mut extended_path = candidate_path.clone().into_os_string();
            extended_path.push(program_extension.as_os_str());
            let extended_path = PathBuf::from(extended_path);
            if extended_path.is_file() {
                return extended_path;
            }
        }
    }
    PathBuf::from(program)
}

/// The working directory a pane program starts in: `requested_directory`
/// when it is a folder, else `user_profile` when that is a folder, else `None`.
/// A relative folder is joined to this process's working directory.
fn resolve_working_directory(
    requested_directory: Option<&Path>,
    user_profile: Option<&OsStr>,
) -> Option<PathBuf> {
    let working_directory = requested_directory
        .filter(|requested_directory| requested_directory.is_dir())
        .or_else(|| {
            user_profile
                .map(Path::new)
                .filter(|user_profile| user_profile.is_dir())
        })?;
    if working_directory.is_relative() {
        if let Ok(process_working_directory) = std::env::current_dir() {
            return Some(process_working_directory.join(working_directory));
        }
    }
    Some(working_directory.to_path_buf())
}

/// Encode `text` as UTF-16 followed by one NUL.
///
/// # Errors
/// Returns [`PtyError::Spawn`] carrying `<subject> holds a NUL character`
/// when `text` holds one.
fn encode_wide_with_nul(text: &OsStr, subject: &str) -> Result<Vec<u16>, PtyError> {
    let mut text_units: Vec<u16> = text.encode_wide().collect();
    if text_units.contains(&0) {
        return Err(PtyError::Spawn {
            detail: format!("{subject} holds a NUL character"),
        });
    }
    text_units.push(0);
    Ok(text_units)
}

/// Build the command line `CreateProcessW` takes: `program_path`, then each
/// of `arguments`, each quoted by [`append_quoted_argument`] and separated by
/// one space, then a NUL.
///
/// Example: `C:\Windows\System32\cmd.exe` with `/C` and `echo "hi"` →
/// `C:\Windows\System32\cmd.exe /C "echo \"hi\""`.
///
/// # Errors
/// Returns [`PtyError::Spawn`] carrying `an argument holds a NUL character`
/// when an argument holds one.
fn build_command_line(program_path: &Path, arguments: &[String]) -> Result<Vec<u16>, PtyError> {
    let mut command_line_units = Vec::new();
    append_quoted_argument(program_path.as_os_str(), &mut command_line_units);
    for argument in arguments {
        if argument.contains('\0') {
            return Err(PtyError::Spawn {
                detail: "an argument holds a NUL character".to_string(),
            });
        }
        command_line_units.push(u16::from(b' '));
        append_quoted_argument(OsStr::new(argument), &mut command_line_units);
    }
    command_line_units.push(0);
    Ok(command_line_units)
}

/// Append `argument` to `command_line_units` in the form the Microsoft C
/// runtime reads back as one argument.
///
/// An argument that is not empty and holds no space, tab, newline, vertical
/// tab or `"` is appended as it is. Any other argument is wrapped in `"`: each
/// `"` in it follows twice its run of backslashes plus one, and the
/// backslashes before the closing `"` are doubled.
///
/// Before → after: `a b` → `"a b"`, `a"b` → `"a\"b"`, `a\"b` → `"a\\\"b"`,
/// `C:\dir with space\` → `"C:\dir with space\\"`, the empty argument → `""`.
fn append_quoted_argument(argument: &OsStr, command_line_units: &mut Vec<u16>) {
    const BACKSLASH: u16 = b'\\' as u16;
    const QUOTE: u16 = b'"' as u16;

    let argument_units: Vec<u16> = argument.encode_wide().collect();
    let needs_quotes = argument_units.is_empty()
        || argument_units
            .iter()
            .any(|argument_unit| matches!(*argument_unit, 0x20 | 0x09 | 0x0a | 0x0b | QUOTE));
    if !needs_quotes {
        command_line_units.extend(&argument_units);
        return;
    }

    command_line_units.push(QUOTE);
    let mut backslash_count = 0;
    for argument_unit in argument_units {
        match argument_unit {
            BACKSLASH => backslash_count += 1,
            QUOTE => {
                command_line_units.extend(std::iter::repeat_n(BACKSLASH, backslash_count * 2 + 1));
                command_line_units.push(QUOTE);
                backslash_count = 0;
            }
            _ => {
                command_line_units.extend(std::iter::repeat_n(BACKSLASH, backslash_count));
                command_line_units.push(argument_unit);
                backslash_count = 0;
            }
        }
    }
    command_line_units.extend(std::iter::repeat_n(BACKSLASH, backslash_count * 2));
    command_line_units.push(QUOTE);
}

#[cfg(test)]
mod tests;
