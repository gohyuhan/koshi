//! Unit tests for the Windows pane spawn: the command line, the environment
//! block, the program search, the working directory, and a pane program that
//! starts inside its pane's two jobs.

use super::*;

use std::io::Write;

use koshi_core::process::ShellKind;
use windows_sys::{core::BOOL, Win32::System::JobObjects::IsProcessInJob};

use crate::{kill::PtyChildKillControl, portable::CURSOR_POSITION_AT_HOME_RESPONSE_BYTES};

/// The command line [`build_command_line`] builds for `program_path` and
/// `arguments`, without its closing NUL.
fn build_command_line_text(program_path: &str, arguments: &[&str]) -> String {
    let arguments: Vec<String> = arguments
        .iter()
        .map(|argument| argument.to_string())
        .collect();
    let command_line_units =
        build_command_line(Path::new(program_path), &arguments).expect("no argument holds a NUL");
    assert_eq!(command_line_units.last(), Some(&0));
    String::from_utf16(&command_line_units[..command_line_units.len() - 1]).expect("valid UTF-16")
}

/// `variable_pairs` as environment variables the way the process or the registry
/// lists them.
fn build_variable_list(variable_pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    variable_pairs
        .iter()
        .map(|(name, variable_value)| (OsString::from(name), OsString::from(variable_value)))
        .collect()
}

/// Each variable's name and value, in [`EnvironmentVariableKey`] order.
fn list_names_and_values(
    environment_variables: &EnvironmentVariableByKey,
) -> Vec<(String, String)> {
    environment_variables
        .values()
        .map(|environment_variable| {
            (
                environment_variable.name.to_string_lossy().into_owned(),
                environment_variable
                    .variable_value
                    .to_string_lossy()
                    .into_owned(),
            )
        })
        .collect()
}

/// `true` when the process behind `process_handle` belongs to the job behind
/// `job_handle`.
fn is_process_in_job(process_handle: &OwnedHandle, job_handle: HANDLE) -> bool {
    let mut is_in_job: BOOL = 0;
    assert_ne!(
        // SAFETY: `process_handle` and the caller's job keep both handles open,
        // and `is_in_job` is a writable `BOOL`.
        unsafe { IsProcessInJob(process_handle.as_raw_handle(), job_handle, &mut is_in_job) },
        0,
        "IsProcessInJob failed"
    );
    is_in_job != 0
}

#[test]
fn a_plain_argument_is_written_as_it_is_and_one_with_a_space_a_quote_or_nothing_is_quoted() {
    assert_eq!(
        build_command_line_text(
            r"C:\Windows\System32\cmd.exe",
            &["/C", "echo", "a b", r#"a"b"#, "", "tab\there"]
        ),
        "C:\\Windows\\System32\\cmd.exe /C echo \"a b\" \"a\\\"b\" \"\" \"tab\there\"",
    );
}

#[test]
fn backslashes_double_only_before_a_quote_and_before_the_closing_quote() {
    assert_eq!(
        build_command_line_text(
            "p",
            &[r"a\\\b", r#"a\"b"#, r"C:\dir with space\", r"a\\b c"]
        ),
        r#"p a\\\b "a\\\"b" "C:\dir with space\\" "a\\b c""#,
    );
}

#[test]
fn a_program_path_with_a_space_is_quoted() {
    assert_eq!(
        build_command_line_text(r"C:\Program Files\PowerShell\7\pwsh.exe", &["-NoLogo"]),
        r#""C:\Program Files\PowerShell\7\pwsh.exe" -NoLogo"#,
    );
}

#[test]
fn an_argument_that_holds_a_nul_character_is_refused() {
    assert_eq!(
        build_command_line(Path::new("p"), &["a\0b".to_string()]),
        Err(PtyError::Spawn {
            detail: "an argument holds a NUL character".to_string(),
        }),
    );
}

#[test]
fn the_user_path_follows_the_system_path_and_the_system_username_is_left_out() {
    let environment_variables = merge_environment_variables(
        build_variable_list(&[
            ("Path", r"C:\process"),
            ("HOME", r"C:\home"),
            ("USERNAME", "user"),
        ]),
        build_variable_list(&[("Path", r"C:\system"), ("UserName", "SYSTEM")]),
        build_variable_list(&[("PATH", r"C:\user")]),
        &BTreeMap::new(),
    );

    assert_eq!(
        list_names_and_values(&environment_variables),
        vec![
            ("HOME".to_string(), r"C:\home".to_string()),
            ("PATH".to_string(), r"C:\system;C:\user".to_string()),
            ("USERNAME".to_string(), "user".to_string()),
        ],
    );
}

#[test]
fn a_user_path_with_no_earlier_path_is_the_user_value_alone() {
    let environment_variables = merge_environment_variables(
        Vec::new(),
        Vec::new(),
        build_variable_list(&[("Path", r"C:\user")]),
        &BTreeMap::new(),
    );

    assert_eq!(
        list_names_and_values(&environment_variables),
        vec![("Path".to_string(), r"C:\user".to_string())],
    );
}

#[test]
fn a_user_variable_other_than_path_replaces_the_system_value() {
    let environment_variables = merge_environment_variables(
        Vec::new(),
        build_variable_list(&[("TEMP", r"C:\Windows\Temp")]),
        build_variable_list(&[("Temp", r"C:\user\Temp")]),
        &BTreeMap::new(),
    );

    assert_eq!(
        list_names_and_values(&environment_variables),
        vec![("Temp".to_string(), r"C:\user\Temp".to_string())],
    );
}

#[test]
fn the_overlay_replaces_a_variable_whose_name_differs_only_in_case() {
    let environment_variables = merge_environment_variables(
        build_variable_list(&[("Term", "dumb")]),
        Vec::new(),
        Vec::new(),
        &BTreeMap::from([("TERM".to_string(), "xterm-256color".to_string())]),
    );

    assert_eq!(
        list_names_and_values(&environment_variables),
        vec![("TERM".to_string(), "xterm-256color".to_string())],
    );
}

#[test]
fn the_environment_block_lists_each_variable_in_windows_name_order_and_ends_with_an_empty_entry() {
    let environment_variables = merge_environment_variables(
        build_variable_list(&[("TERM", "xterm"), ("Path", r"C:\s"), ("=C:", r"C:\work")]),
        Vec::new(),
        Vec::new(),
        &BTreeMap::new(),
    );

    let expected_block_units: Vec<u16> = "=C:=C:\\work\0Path=C:\\s\0TERM=xterm\0\0"
        .encode_utf16()
        .collect();
    assert_eq!(
        build_environment_block(&environment_variables),
        Ok(expected_block_units),
    );
}

#[test]
fn names_sort_by_their_uppercase_units_so_an_underscore_follows_every_letter() {
    let environment_variables = merge_environment_variables(
        build_variable_list(&[
            ("term_program", "koshi"),
            ("Terminal", "x"),
            ("TERM", "xterm"),
        ]),
        Vec::new(),
        Vec::new(),
        &BTreeMap::new(),
    );

    assert_eq!(
        list_names_and_values(&environment_variables),
        vec![
            ("TERM".to_string(), "xterm".to_string()),
            ("Terminal".to_string(), "x".to_string()),
            ("term_program".to_string(), "koshi".to_string()),
        ],
    );
}

#[test]
fn an_environment_value_that_holds_a_nul_character_is_refused() {
    let environment_variables = merge_environment_variables(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        &BTreeMap::from([("KOSHI_TEST".to_string(), "a\0b".to_string())]),
    );

    assert_eq!(
        build_environment_block(&environment_variables),
        Err(PtyError::Spawn {
            detail: "the environment variable KOSHI_TEST holds a NUL character".to_string(),
        }),
    );
}

#[test]
fn a_program_is_found_with_an_extension_appended_in_the_first_folder_that_holds_it() {
    let empty_folder = tempfile::tempdir().expect("create a folder");
    let program_folder = tempfile::tempdir().expect("create a folder");
    std::fs::write(program_folder.path().join("tool.EXE"), b"").expect("write the program");
    let path_value =
        std::env::join_paths([empty_folder.path(), program_folder.path()]).expect("join folders");

    assert_eq!(
        find_program_path(
            OsStr::new("tool"),
            Some(&path_value),
            Some(OsStr::new(".COM;.EXE"))
        ),
        program_folder.path().join("tool.EXE"),
    );
}

#[test]
fn an_extension_is_appended_to_a_name_that_already_holds_a_dot() {
    let program_folder = tempfile::tempdir().expect("create a folder");
    std::fs::write(program_folder.path().join("python3.EXE"), b"").expect("write a program");
    std::fs::write(program_folder.path().join("python3.11.EXE"), b"").expect("write a program");

    assert_eq!(
        find_program_path(
            OsStr::new("python3.11"),
            Some(program_folder.path().as_os_str()),
            Some(OsStr::new(".EXE"))
        ),
        program_folder.path().join("python3.11.EXE"),
    );
}

#[test]
fn a_folder_that_carries_the_program_name_is_not_a_match() {
    let program_folder = tempfile::tempdir().expect("create a folder");
    std::fs::create_dir(program_folder.path().join("tool")).expect("create the decoy folder");
    std::fs::write(program_folder.path().join("tool.EXE"), b"").expect("write the program");

    assert_eq!(
        find_program_path(
            OsStr::new("tool"),
            Some(program_folder.path().as_os_str()),
            None
        ),
        program_folder.path().join("tool.EXE"),
    );
}

#[test]
fn empty_folders_and_empty_extensions_leave_the_search_going() {
    let program_folder = tempfile::tempdir().expect("create a folder");
    std::fs::write(program_folder.path().join("tool.EXE"), b"").expect("write the program");
    let mut path_value = OsString::from(";");
    path_value.push(program_folder.path());

    assert_eq!(
        find_program_path(
            OsStr::new("tool"),
            Some(&path_value),
            Some(OsStr::new(";;.EXE"))
        ),
        program_folder.path().join("tool.EXE"),
    );
}

/// Cargo runs these tests in the package folder, which holds `Cargo.toml`.
#[test]
fn an_empty_folder_never_finds_a_program_in_the_working_folder() {
    let program_folder = tempfile::tempdir().expect("create a folder");
    std::fs::write(program_folder.path().join("Cargo.toml"), b"").expect("write the program");
    let mut path_value = OsString::from(";");
    path_value.push(program_folder.path());

    assert_eq!(
        find_program_path(OsStr::new("Cargo.toml"), Some(&path_value), None),
        program_folder.path().join("Cargo.toml"),
    );
}

#[test]
fn a_program_no_folder_holds_and_a_program_with_no_path_come_back_as_given() {
    let empty_folder = tempfile::tempdir().expect("create a folder");

    assert_eq!(
        find_program_path(
            OsStr::new("missing-tool"),
            Some(empty_folder.path().as_os_str()),
            None
        ),
        PathBuf::from("missing-tool"),
    );
    assert_eq!(
        find_program_path(OsStr::new("missing-tool"), None, None),
        PathBuf::from("missing-tool"),
    );
}

#[test]
fn a_working_directory_that_is_no_folder_falls_back_to_the_user_profile() {
    let user_profile = tempfile::tempdir().expect("create a folder");
    let missing_folder = user_profile.path().join("missing");

    assert_eq!(
        resolve_working_directory(Some(&missing_folder), Some(user_profile.path().as_os_str())),
        Some(user_profile.path().to_path_buf()),
    );
    assert_eq!(
        resolve_working_directory(Some(&missing_folder), Some(missing_folder.as_os_str())),
        None,
    );
}

#[test]
fn a_relative_working_directory_is_joined_to_the_process_working_directory() {
    assert_eq!(
        resolve_working_directory(Some(Path::new(".")), None),
        Some(
            std::env::current_dir()
                .expect("read the working directory")
                .join(".")
        ),
    );
}

#[test]
fn a_size_past_32767_cells_on_a_side_is_refused() {
    let Err(size_error) = build_console_size(PtySize {
        row_count: 24,
        column_count: 40_000,
    }) else {
        panic!("40000 columns is past the console's limit");
    };
    assert_eq!(size_error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
        size_error.to_string(),
        "40000x24 is past the console's 32767 cells on a side"
    );
}

#[test]
fn a_pane_program_starts_inside_the_shared_job_and_its_pane_job() {
    let pane_job = PaneJob::create().expect("create the pane's jobs");
    let spawn_spec = SpawnSpec {
        program: PathBuf::from("cmd.exe"),
        arguments: vec!["/C".to_string(), "ping -n 30 127.0.0.1 >NUL".to_string()],
        working_directory: None,
        environment_variables: BTreeMap::new(),
        shell_kind: ShellKind::Other("cmd".to_string()),
    };
    let SpawnedPaneProgram {
        pane_console,
        output_reader,
        mut input_writer,
        pane_process,
    } = spawn_pane_program(
        &spawn_spec,
        PtySize {
            row_count: 24,
            column_count: 80,
        },
        &pane_job,
    )
    .expect("start the pane program");
    input_writer
        .write_all(CURSOR_POSITION_AT_HOME_RESPONSE_BYTES)
        .expect("answer the cursor-position request");

    let process_handle = pane_process.share_process_handle();
    for job_handle in pane_job.list_job_handles() {
        assert!(is_process_in_job(&process_handle, job_handle));
    }

    let kill_control = PtyChildKillControl::from_pane_job(
        pane_process.get_process_id(),
        Arc::clone(&process_handle),
        pane_job,
    );
    kill_control
        .force_kill_process_tree()
        .expect("terminate the pane's job");
    assert_eq!(
        pane_process
            .wait_for_exit_code()
            .expect("read the exit code"),
        137
    );

    drop(output_reader);
    drop(input_writer);
    drop(pane_console);
}
