//! `koshi remote new` and `koshi remote edit`, answered on standard input.
//! Each command is a wizard: it asks its questions one at a time.
//!
//! Each test starts the real `koshi` binary under a temporary home directory,
//! with no controlling terminal on Unix and no console on Windows: every
//! question, the secret included, reads one line of standard input. The check
//! each wizard makes dials a TLS server that the test runs on a loopback port.
//! No connection leaves this machine.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Output, Stdio};

use common::{
    build_koshi_command_under_home, build_saved_server, build_short_test_directory,
    list_saved_server_records, spawn_stand_in_remote_server, write_saved_server_store,
};
use koshi_core::command::CliExitCode;
use koshi_ipc::remote_wire::{RemoteServerFrame, REMOTE_PROTOCOL_VERSION};

/// The first line `koshi remote new` prints on standard output.
const NEW_OPENING_LINE: &str = "every answer is needed. Ctrl-C stops without saving.\n";

/// The first line `koshi remote edit` prints on standard output.
const EDIT_OPENING_LINE: &str = "press Enter to keep the value in brackets. An empty secret \
                                 keeps the saved one. Ctrl-C stops without saving.\n";

/// The line both wizards print on standard error for a fingerprint answer
/// that is not `sha256:` followed by 64 lowercase hex characters.
const MALFORMED_FINGERPRINT_LINE: &str = "koshi: invalid arguments: a certificate fingerprint \
                                          is sha256: followed by 64 lowercase hex characters, \
                                          as `koshi share grant` prints it on the machine that \
                                          serves\n";

/// A secret with the shape `koshi share grant` prints: 64 lowercase hex
/// characters.
const SECRET_TEXT: &str = "f00df00df00df00df00df00df00df00df00df00df00df00df00df00df00df00d";

/// A stand-in server that admits the one check dial a wizard makes. Returns
/// its address and the fingerprint of its certificate.
fn spawn_admitting_remote_server() -> (String, String) {
    spawn_stand_in_remote_server(vec![RemoteServerFrame::Welcome {
        remote_protocol_version: REMOTE_PROTOCOL_VERSION,
    }])
}

/// Run `koshi` with `wizard_arguments` under `home_directory`, with
/// `answer_text` on standard input, and return what it printed and its exit
/// status.
///
/// On Unix the binary starts in a session of its own, with no controlling
/// terminal. On Windows it starts with no console.
fn run_wizard_with_answers(
    home_directory: &Path,
    wizard_arguments: &[&str],
    answer_text: &str,
) -> Output {
    let mut wizard_command = build_koshi_command_under_home(home_directory);
    wizard_command.args(wizard_arguments).stdin(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: the closure runs in the child between `fork` and `exec`. It
        // calls `setsid` and, when that fails, reads `errno`. Both are
        // async-signal-safe, and neither allocates.
        unsafe {
            wizard_command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        wizard_command.creation_flags(windows_sys::Win32::System::Threading::DETACHED_PROCESS);
    }
    let mut wizard_process = wizard_command.spawn().expect("the koshi binary starts");
    wizard_process
        .stdin
        .take()
        .expect("the wizard's input is a pipe")
        .write_all(answer_text.as_bytes())
        .expect("the answers are written");
    wizard_process
        .wait_with_output()
        .expect("the koshi binary runs")
}

#[test]
fn remote_new_asks_again_for_a_malformed_fingerprint_and_saves_the_one_given() {
    let (server_address, certificate_fingerprint) = spawn_admitting_remote_server();
    let home_directory = build_short_test_directory();

    let wizard_output = run_wizard_with_answers(
        home_directory.path(),
        &["remote", "new"],
        &format!("work\n{server_address}\nABC\nsha256:{certificate_fingerprint}\ntest-secret\n"),
    );

    assert_eq!(
        String::from_utf8_lossy(&wizard_output.stdout),
        format!("{NEW_OPENING_LINE}checking {server_address} …\nsaved work at {server_address}.\n")
    );
    assert_eq!(
        String::from_utf8_lossy(&wizard_output.stderr),
        format!("name: address: fingerprint: {MALFORMED_FINGERPRINT_LINE}fingerprint: secret: ")
    );
    assert_eq!(
        wizard_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(
        list_saved_server_records(home_directory.path()),
        vec![(
            Some("work".to_string()),
            server_address,
            Some(certificate_fingerprint)
        )]
    );
}

#[test]
fn remote_new_refuses_a_secret_at_the_name_address_and_fingerprint_questions_and_never_prints_it() {
    let (server_address, certificate_fingerprint) = spawn_admitting_remote_server();
    let home_directory = build_short_test_directory();

    let wizard_output = run_wizard_with_answers(
        home_directory.path(),
        &["remote", "new"],
        &format!(
            "{SECRET_TEXT}\nwork\n{SECRET_TEXT}\n{server_address}\n{SECRET_TEXT}\n\
             sha256:{certificate_fingerprint}\n{SECRET_TEXT}\n"
        ),
    );

    assert_eq!(
        String::from_utf8_lossy(&wizard_output.stdout),
        format!("{NEW_OPENING_LINE}checking {server_address} …\nsaved work at {server_address}.\n")
    );
    assert_eq!(
        String::from_utf8_lossy(&wizard_output.stderr),
        "name: koshi: invalid arguments: a saved name must not be 64 hex characters, the shape \
         of a secret. Pick a plain name.\n\
         name: address: koshi: invalid arguments: an address is host:port, such as \
         laptop.local:7654\n\
         address: fingerprint: koshi: invalid arguments: a certificate fingerprint is sha256: \
         followed by 64 lowercase hex characters, as `koshi share grant` prints it on the \
         machine that serves. 64 hex characters alone is the shape of a secret, and a secret is \
         never given here\n\
         fingerprint: secret: "
    );
    assert_eq!(
        wizard_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(
        list_saved_server_records(home_directory.path()),
        vec![(
            Some("work".to_string()),
            server_address,
            Some(certificate_fingerprint)
        )]
    );
}

#[test]
fn remote_new_saves_the_fingerprint_given_when_the_check_fails_and_the_answer_is_yes() {
    let (server_address, presented_certificate_fingerprint) = spawn_admitting_remote_server();
    let given_certificate_fingerprint = "0".repeat(64);
    let home_directory = build_short_test_directory();

    let wizard_output = run_wizard_with_answers(
        home_directory.path(),
        &["remote", "new"],
        &format!(
            "work\n{server_address}\nsha256:{given_certificate_fingerprint}\ntest-secret\ny\n"
        ),
    );

    assert_eq!(
        String::from_utf8_lossy(&wizard_output.stdout),
        format!("{NEW_OPENING_LINE}checking {server_address} …\nsaved work at {server_address}.\n")
    );
    assert_eq!(
        String::from_utf8_lossy(&wizard_output.stderr),
        format!(
            "name: address: fingerprint: secret: koshi: IPC unavailable: the certificate \
             {server_address} presented is not the expected one: expected \
             sha256:{given_certificate_fingerprint}, presented \
             sha256:{presented_certificate_fingerprint}. the secret was not sent. if that \
             machine's certificate changed on purpose, `koshi share open` there prints the new \
             fingerprint: give it with --certificate-fingerprint, or with `koshi remote edit` \
             for a saved server\nsave it anyway? [y/N]: "
        )
    );
    assert_eq!(
        wizard_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(
        list_saved_server_records(home_directory.path()),
        vec![(
            Some("work".to_string()),
            server_address,
            Some(given_certificate_fingerprint)
        )]
    );
}

#[test]
fn remote_edit_asks_again_for_an_empty_fingerprint_when_the_saved_server_holds_none() {
    let (server_address, certificate_fingerprint) = spawn_admitting_remote_server();
    let home_directory = build_short_test_directory();
    write_saved_server_store(
        home_directory.path(),
        vec![build_saved_server("work", server_address.clone(), None)],
    );

    let wizard_output = run_wizard_with_answers(
        home_directory.path(),
        &["remote", "edit", "work"],
        &format!("\n\n\nsha256:{certificate_fingerprint}\n\n"),
    );

    assert_eq!(
        String::from_utf8_lossy(&wizard_output.stdout),
        format!(
            "{EDIT_OPENING_LINE}checking {server_address} …\nupdated work at {server_address}.\n"
        )
    );
    assert_eq!(
        String::from_utf8_lossy(&wizard_output.stderr),
        format!(
            "name [work]: address [{server_address}]: fingerprint: \
             {MALFORMED_FINGERPRINT_LINE}fingerprint: secret: "
        )
    );
    assert_eq!(
        wizard_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(
        list_saved_server_records(home_directory.path()),
        vec![(
            Some("work".to_string()),
            server_address,
            Some(certificate_fingerprint)
        )]
    );
}

#[test]
fn remote_edit_keeps_the_saved_fingerprint_on_an_empty_answer() {
    let (server_address, certificate_fingerprint) = spawn_admitting_remote_server();
    let home_directory = build_short_test_directory();
    write_saved_server_store(
        home_directory.path(),
        vec![build_saved_server(
            "work",
            server_address.clone(),
            Some(certificate_fingerprint.clone()),
        )],
    );

    let wizard_output = run_wizard_with_answers(
        home_directory.path(),
        &["remote", "edit", "work"],
        "\n\n\n\n",
    );

    assert_eq!(
        String::from_utf8_lossy(&wizard_output.stdout),
        format!(
            "{EDIT_OPENING_LINE}checking {server_address} …\nupdated work at {server_address}.\n"
        )
    );
    assert_eq!(
        String::from_utf8_lossy(&wizard_output.stderr),
        format!(
            "name [work]: address [{server_address}]: fingerprint \
             [sha256:{certificate_fingerprint}]: secret: "
        )
    );
    assert_eq!(
        wizard_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(
        list_saved_server_records(home_directory.path()),
        vec![(
            Some("work".to_string()),
            server_address,
            Some(certificate_fingerprint)
        )]
    );
}

#[test]
fn remote_edit_replaces_the_saved_fingerprint_with_the_one_given() {
    let (server_address, certificate_fingerprint) = spawn_admitting_remote_server();
    let saved_certificate_fingerprint = "0".repeat(64);
    let home_directory = build_short_test_directory();
    write_saved_server_store(
        home_directory.path(),
        vec![build_saved_server(
            "work",
            server_address.clone(),
            Some(saved_certificate_fingerprint.clone()),
        )],
    );

    let wizard_output = run_wizard_with_answers(
        home_directory.path(),
        &["remote", "edit", "work"],
        &format!("\n\nsha256:{certificate_fingerprint}\n\n"),
    );

    assert_eq!(
        String::from_utf8_lossy(&wizard_output.stdout),
        format!(
            "{EDIT_OPENING_LINE}checking {server_address} …\nupdated work at {server_address}.\n"
        )
    );
    assert_eq!(
        String::from_utf8_lossy(&wizard_output.stderr),
        format!(
            "name [work]: address [{server_address}]: fingerprint \
             [sha256:{saved_certificate_fingerprint}]: secret: "
        )
    );
    assert_eq!(
        wizard_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(
        list_saved_server_records(home_directory.path()),
        vec![(
            Some("work".to_string()),
            server_address,
            Some(certificate_fingerprint)
        )]
    );
}
