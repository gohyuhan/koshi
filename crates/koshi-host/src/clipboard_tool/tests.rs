//! Tests for the stand-in clipboard tools: the search for the real tool on
//! `PATH`, and the refresh of one session's folder of stand-ins.

use super::*;

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;

/// The koshi program the links in a test folder point at. No file is there.
const TEST_PROGRAM_PATH: &str = "/opt/koshi/bin/koshi";

/// Write a shell script named `tool_name` into `directory_path`, with the
/// permission bits `permission_mode`, and hand back its path. The folder is
/// created first when it is not there.
fn write_tool_file(directory_path: &Path, tool_name: &str, permission_mode: u32) -> PathBuf {
    std::fs::create_dir_all(directory_path).expect("the tool folder is made");
    let tool_path = directory_path.join(tool_name);
    std::fs::write(&tool_path, b"#!/bin/sh\n").expect("the tool file is written");
    std::fs::set_permissions(&tool_path, std::fs::Permissions::from_mode(permission_mode))
        .expect("the mode is set");
    tool_path
}

/// The `PATH` value that lists `path_directories` in order.
fn build_path_entries(path_directories: &[PathBuf]) -> OsString {
    std::env::join_paths(path_directories).expect("no folder holds the separator")
}

/// Each entry of `clipboard_tool_directory`, as its name and the target of the
/// link it is, in name order.
fn list_tool_links(clipboard_tool_directory: &Path) -> Vec<(String, PathBuf)> {
    let mut tool_links: Vec<(String, PathBuf)> = std::fs::read_dir(clipboard_tool_directory)
        .expect("the folder reads")
        .map(|directory_entry| {
            let entry_path = directory_entry.expect("the entry reads").path();
            let entry_name = entry_path
                .file_name()
                .and_then(OsStr::to_str)
                .expect("the entry has a name")
                .to_string();
            let link_target = std::fs::read_link(&entry_path).expect("the entry is a link");
            (entry_name, link_target)
        })
        .collect();
    tool_links.sort();
    tool_links
}

#[test]
fn the_real_tool_is_the_first_runnable_file_in_path_order() {
    let test_root = tempfile::tempdir().expect("a test folder");
    let plain_directory = test_root.path().join("plain");
    write_tool_file(&plain_directory, "xclip", 0o644);
    let folder_directory = test_root.path().join("folder");
    std::fs::create_dir_all(folder_directory.join("xclip")).expect("a folder named xclip");
    let first_runnable_directory = test_root.path().join("first");
    let first_runnable_path = write_tool_file(&first_runnable_directory, "xclip", 0o755);
    let second_runnable_directory = test_root.path().join("second");
    write_tool_file(&second_runnable_directory, "xclip", 0o755);
    let path_entries = build_path_entries(&[
        test_root.path().join("missing"),
        plain_directory,
        folder_directory,
        first_runnable_directory,
        second_runnable_directory,
    ]);

    assert_eq!(
        find_real_clipboard_tool("xclip", Some(&path_entries)),
        Some(first_runnable_path)
    );
}

#[test]
fn the_real_tool_is_never_in_a_folder_of_stand_ins() {
    let test_root = tempfile::tempdir().expect("a test folder");
    let stand_in_directory = test_root
        .path()
        .join("session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.clipboard-tools");
    write_tool_file(&stand_in_directory, "pbcopy", 0o755);
    let nested_stand_in_directory = test_root.path().join("outer.clipboard-tools");
    write_tool_file(&nested_stand_in_directory, "pbcopy", 0o755);
    let real_directory = test_root.path().join("bin");
    let real_tool_path = write_tool_file(&real_directory, "pbcopy", 0o755);

    let path_entries = build_path_entries(&[
        stand_in_directory.clone(),
        nested_stand_in_directory,
        real_directory,
    ]);
    assert_eq!(
        find_real_clipboard_tool("pbcopy", Some(&path_entries)),
        Some(real_tool_path)
    );

    // A folder of stand-ins written with a trailing separator is passed over
    // the same way.
    let mut stand_in_entry = stand_in_directory.into_os_string();
    stand_in_entry.push("/");
    assert_eq!(
        find_real_clipboard_tool("pbcopy", Some(&stand_in_entry)),
        None
    );
}

#[test]
fn the_real_tool_is_never_the_program_this_process_runs() {
    let test_root = tempfile::tempdir().expect("a test folder");
    let alias_directory = test_root.path().join("alias");
    std::fs::create_dir_all(&alias_directory).expect("the alias folder is made");
    std::os::unix::fs::symlink(
        std::env::current_exe().expect("the test binary has a path"),
        alias_directory.join("wl-copy"),
    )
    .expect("the link is made");
    let real_directory = test_root.path().join("bin");
    let real_tool_path = write_tool_file(&real_directory, "wl-copy", 0o755);

    let path_entries = build_path_entries(&[alias_directory.clone(), real_directory]);
    assert_eq!(
        find_real_clipboard_tool("wl-copy", Some(&path_entries)),
        Some(real_tool_path)
    );
    assert_eq!(
        find_real_clipboard_tool("wl-copy", Some(alias_directory.as_os_str())),
        None
    );
}

#[test]
fn no_path_and_a_path_without_the_tool_find_no_real_tool() {
    let test_root = tempfile::tempdir().expect("a test folder");
    let real_directory = test_root.path().join("bin");
    write_tool_file(&real_directory, "xsel", 0o755);

    assert_eq!(find_real_clipboard_tool("xsel", None), None);
    assert_eq!(
        find_real_clipboard_tool("xclip", Some(real_directory.as_os_str())),
        None
    );
}

#[test]
fn a_refresh_makes_a_private_folder_holding_one_link_per_tool_found() {
    let test_root = tempfile::tempdir().expect("a test folder");
    let real_directory = test_root.path().join("bin");
    write_tool_file(&real_directory, "xclip", 0o755);
    write_tool_file(&real_directory, "wl-copy", 0o755);
    write_tool_file(&real_directory, "xsel", 0o644);
    let clipboard_tool_directory = test_root.path().join("session-5000.clipboard-tools");

    refresh_clipboard_tool_directory(
        &clipboard_tool_directory,
        Path::new(TEST_PROGRAM_PATH),
        Some(real_directory.as_os_str()),
    )
    .expect("the folder is refreshed");

    let folder_mode = std::fs::metadata(&clipboard_tool_directory)
        .expect("the folder is there")
        .permissions()
        .mode();
    assert_eq!(folder_mode & 0o777, 0o700);
    assert_eq!(
        list_tool_links(&clipboard_tool_directory),
        vec![
            ("wl-copy".to_string(), PathBuf::from(TEST_PROGRAM_PATH)),
            ("xclip".to_string(), PathBuf::from(TEST_PROGRAM_PATH)),
        ]
    );
}

#[test]
fn a_refresh_keeps_the_mode_of_a_folder_already_there() {
    let test_root = tempfile::tempdir().expect("a test folder");
    let clipboard_tool_directory = test_root.path().join("session-5000.clipboard-tools");
    std::fs::create_dir(&clipboard_tool_directory).expect("the folder is made");
    std::fs::set_permissions(
        &clipboard_tool_directory,
        std::fs::Permissions::from_mode(0o750),
    )
    .expect("the mode is set");

    refresh_clipboard_tool_directory(
        &clipboard_tool_directory,
        Path::new(TEST_PROGRAM_PATH),
        None,
    )
    .expect("the folder is refreshed");

    let folder_mode = std::fs::metadata(&clipboard_tool_directory)
        .expect("the folder is there")
        .permissions()
        .mode();
    assert_eq!(folder_mode & 0o777, 0o750);
}

#[test]
fn a_refresh_removes_every_entry_but_the_links_of_the_tools_found() {
    let test_root = tempfile::tempdir().expect("a test folder");
    let real_directory = test_root.path().join("bin");
    write_tool_file(&real_directory, "pbcopy", 0o755);
    write_tool_file(&real_directory, "xclip", 0o755);
    let clipboard_tool_directory = test_root.path().join("session-5000.clipboard-tools");
    std::fs::create_dir(&clipboard_tool_directory).expect("the folder is made");
    // A link to the program, a link to another target, the link of a tool
    // that is not installed, and an entry no koshi writes.
    std::os::unix::fs::symlink(TEST_PROGRAM_PATH, clipboard_tool_directory.join("pbcopy"))
        .expect("the pbcopy link is made");
    std::os::unix::fs::symlink(
        "/opt/koshi/0.4.0/koshi",
        clipboard_tool_directory.join("xclip"),
    )
    .expect("the xclip link is made");
    std::os::unix::fs::symlink(TEST_PROGRAM_PATH, clipboard_tool_directory.join("xsel"))
        .expect("the xsel link is made");
    std::fs::write(clipboard_tool_directory.join("notes.txt"), b"notes")
        .expect("the stray file is written");

    refresh_clipboard_tool_directory(
        &clipboard_tool_directory,
        Path::new(TEST_PROGRAM_PATH),
        Some(real_directory.as_os_str()),
    )
    .expect("the folder is refreshed");

    assert_eq!(
        list_tool_links(&clipboard_tool_directory),
        vec![
            ("pbcopy".to_string(), PathBuf::from(TEST_PROGRAM_PATH)),
            ("xclip".to_string(), PathBuf::from(TEST_PROGRAM_PATH)),
        ]
    );
}

#[test]
fn a_refresh_with_no_path_leaves_the_folder_empty() {
    let test_root = tempfile::tempdir().expect("a test folder");
    let clipboard_tool_directory = test_root.path().join("session-5000.clipboard-tools");
    std::fs::create_dir(&clipboard_tool_directory).expect("the folder is made");
    std::os::unix::fs::symlink(TEST_PROGRAM_PATH, clipboard_tool_directory.join("pbcopy"))
        .expect("the pbcopy link is made");

    refresh_clipboard_tool_directory(
        &clipboard_tool_directory,
        Path::new(TEST_PROGRAM_PATH),
        None,
    )
    .expect("the folder is refreshed");

    assert_eq!(list_tool_links(&clipboard_tool_directory), Vec::new());
}

#[test]
fn a_refresh_over_a_file_in_the_folders_place_fails() {
    let test_root = tempfile::tempdir().expect("a test folder");
    let clipboard_tool_directory = test_root.path().join("session-5000.clipboard-tools");
    std::fs::write(&clipboard_tool_directory, b"").expect("a file stands in the folder's place");

    let refresh_error = refresh_clipboard_tool_directory(
        &clipboard_tool_directory,
        Path::new(TEST_PROGRAM_PATH),
        None,
    )
    .expect_err("a file is no folder");

    assert_eq!(
        refresh_error.raw_os_error(),
        Some(libc::ENOTDIR),
        "{refresh_error}"
    );
}
