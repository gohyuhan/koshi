//! `--remote <address> --certificate-fingerprint <fingerprint>` with a server
//! this machine has not saved, and `attach --remote <address> --save-as <name>`
//! with a server this machine saved under no name.
//!
//! Each test starts the real `koshi` binary under a temporary home directory
//! against a TLS server that the test runs on a loopback port, or against a
//! loopback address that no test dials. A dial to a server this machine has
//! not saved has `KOSHI_REMOTE_SECRET` set. No connection leaves this machine.
//! A secret given at `--certificate-fingerprint`, at `--save-as`, at
//! `--remote`, or as the server of a `koshi remote` verb is refused before any
//! dial, and the refusal does not repeat it.
//! A session argument that names a folder of this machine's shared directory
//! is refused before any dial.

mod common;

use common::{
    build_koshi_command_under_home, build_saved_server, build_short_test_directory,
    list_saved_server_records, spawn_stand_in_remote_server, write_saved_server_store,
};
use koshi_core::command::CliExitCode;
use koshi_core::ids::SessionId;
use koshi_ipc::remote_servers::SavedServer;
use koshi_ipc::remote_wire::{RemoteServerFrame, RemoteSessionRow, REMOTE_PROTOCOL_VERSION};
use koshi_link::discovery::SessionRow;

#[test]
fn list_sessions_lists_an_unsaved_server_and_saves_it_holding_the_fingerprint_given() {
    let remote_session_rows = vec![RemoteSessionRow {
        session_id: SessionId::new(),
        session_name: "quiet-lake".to_string(),
    }];
    let (server_address, certificate_fingerprint) = spawn_stand_in_remote_server(vec![
        RemoteServerFrame::Welcome {
            remote_protocol_version: REMOTE_PROTOCOL_VERSION,
        },
        RemoteServerFrame::Sessions {
            session_rows: remote_session_rows.clone(),
        },
    ]);
    let home_directory = build_short_test_directory();

    let listing_output = build_koshi_command_under_home(home_directory.path())
        .args([
            "list-sessions",
            "--remote",
            &server_address,
            "--certificate-fingerprint",
            &format!("sha256:{certificate_fingerprint}"),
        ])
        .env("KOSHI_REMOTE_SECRET", "test-secret")
        .output()
        .expect("the koshi binary runs");

    let expected_session_rows: Vec<SessionRow> = remote_session_rows
        .iter()
        .map(|remote_session_row| {
            SessionRow::from_session(
                remote_session_row.session_id,
                &remote_session_row.session_name,
                Some(server_address.clone()),
            )
        })
        .collect();
    assert_eq!(String::from_utf8_lossy(&listing_output.stderr), "");
    assert_eq!(
        String::from_utf8_lossy(&listing_output.stdout),
        koshi::output::render_sessions(&expected_session_rows, koshi::cli::OutputFormat::Table)
    );
    assert_eq!(
        listing_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(
        list_saved_server_records(home_directory.path()),
        vec![(None, server_address, Some(certificate_fingerprint))]
    );
}

#[test]
fn an_action_verb_reaches_an_unsaved_server_and_saves_it_holding_the_fingerprint_given() {
    // The stand-in server lists no session: `new-pane` finds no session to
    // open a pane in.
    let (server_address, certificate_fingerprint) = spawn_stand_in_remote_server(vec![
        RemoteServerFrame::Welcome {
            remote_protocol_version: REMOTE_PROTOCOL_VERSION,
        },
        RemoteServerFrame::Sessions {
            session_rows: Vec::new(),
        },
    ]);
    let home_directory = build_short_test_directory();

    let verb_output = build_koshi_command_under_home(home_directory.path())
        .args([
            "new-pane",
            "--remote",
            &server_address,
            "--certificate-fingerprint",
            &format!("sha256:{certificate_fingerprint}"),
        ])
        .env("KOSHI_REMOTE_SECRET", "test-secret")
        .output()
        .expect("the koshi binary runs");

    assert_eq!(
        String::from_utf8_lossy(&verb_output.stderr),
        "koshi: no koshi session is running\n"
    );
    assert_eq!(String::from_utf8_lossy(&verb_output.stdout), "");
    assert_eq!(
        verb_output.status.code(),
        Some(CliExitCode::SessionNotFound.get_exit_code())
    );
    assert_eq!(
        list_saved_server_records(home_directory.path()),
        vec![(None, server_address, Some(certificate_fingerprint))]
    );
}

#[test]
fn an_unsaved_server_whose_certificate_is_not_the_one_given_is_refused_and_not_saved() {
    let (server_address, presented_certificate_fingerprint) =
        spawn_stand_in_remote_server(vec![RemoteServerFrame::Welcome {
            remote_protocol_version: REMOTE_PROTOCOL_VERSION,
        }]);
    let given_certificate_fingerprint = "0".repeat(64);
    let home_directory = build_short_test_directory();

    let listing_output = build_koshi_command_under_home(home_directory.path())
        .args([
            "list-sessions",
            "--remote",
            &server_address,
            "--certificate-fingerprint",
            &format!("sha256:{given_certificate_fingerprint}"),
        ])
        .env("KOSHI_REMOTE_SECRET", "test-secret")
        .output()
        .expect("the koshi binary runs");

    assert_eq!(
        String::from_utf8_lossy(&listing_output.stderr),
        format!(
            "koshi: IPC unavailable: the certificate {server_address} presented is not the \
             expected one: expected sha256:{given_certificate_fingerprint}, presented \
             sha256:{presented_certificate_fingerprint}. the secret was not sent. if that \
             machine's certificate changed on purpose, `koshi share open` there prints the new \
             fingerprint: give it with --certificate-fingerprint, or with `koshi remote edit` \
             for a saved server\n"
        )
    );
    assert_eq!(String::from_utf8_lossy(&listing_output.stdout), "");
    assert_eq!(
        listing_output.status.code(),
        Some(CliExitCode::IpcUnavailable.get_exit_code())
    );
    assert_eq!(list_saved_server_records(home_directory.path()), Vec::new());
}

#[test]
fn a_secret_at_the_fingerprint_or_save_as_flag_is_refused_before_any_dial_and_not_printed() {
    let secret_text = "f00d".repeat(16);
    let (server_address, certificate_fingerprint) =
        spawn_stand_in_remote_server(vec![RemoteServerFrame::Welcome {
            remote_protocol_version: REMOTE_PROTOCOL_VERSION,
        }]);
    let certificate_fingerprint_text = format!("sha256:{certificate_fingerprint}");
    let home_directory = build_short_test_directory();
    let refused_argument_cases: [(Vec<&str>, &str); 2] = [
        (
            vec![
                "attach",
                "--remote",
                &server_address,
                "--certificate-fingerprint",
                &secret_text,
            ],
            "koshi: invalid arguments: a certificate fingerprint is sha256: followed by 64 \
             lowercase hex characters, as `koshi share grant` prints it on the machine that \
             serves. 64 hex characters alone is the shape of a secret, and a secret is never \
             given here\n",
        ),
        (
            vec![
                "attach",
                "--remote",
                &server_address,
                "--certificate-fingerprint",
                &certificate_fingerprint_text,
                "--save-as",
                &secret_text,
            ],
            "koshi: invalid arguments: a saved name must not be 64 hex characters, the shape of \
             a secret. Pick a plain name.\n",
        ),
    ];

    for (command_arguments, expected_stderr) in refused_argument_cases {
        let refused_output = build_koshi_command_under_home(home_directory.path())
            .args(&command_arguments)
            .env("KOSHI_REMOTE_SECRET", "test-secret")
            .output()
            .expect("the koshi binary runs");

        assert_eq!(
            String::from_utf8_lossy(&refused_output.stderr),
            expected_stderr,
            "{command_arguments:?}"
        );
        assert_eq!(String::from_utf8_lossy(&refused_output.stdout), "");
        assert_eq!(
            refused_output.status.code(),
            Some(CliExitCode::UsageOrConfig.get_exit_code()),
            "{command_arguments:?}"
        );
        assert_eq!(list_saved_server_records(home_directory.path()), Vec::new());
    }
}

#[test]
fn a_secret_where_a_saved_server_or_its_name_goes_is_refused_before_any_dial_and_not_printed() {
    let secret_text = "f00d".repeat(16);
    let padded_secret_text = format!(" {secret_text}");
    // Nothing listens on this address: a dial would end in a refusal of its
    // own.
    let saved_server_address = "127.0.0.1:1".to_string();
    let saved_certificate_fingerprint = "0".repeat(64);
    let secret_shaped_name_stderr = "koshi: invalid arguments: a saved name must not be 64 hex \
                                     characters, the shape of a secret. Pick a plain name.\n";
    let secret_shaped_server_stderr = "koshi: invalid arguments: no saved server is named by \
                                       the 64 hex characters given, the shape of a secret, and \
                                       a secret is never given here; run `koshi remote list`\n";
    let refused_argument_cases: [(Vec<&str>, &str); 7] = [
        (
            vec!["attach", "--remote", "work", "--save-as", &secret_text],
            secret_shaped_name_stderr,
        ),
        (
            vec![
                "attach",
                "--remote",
                "work",
                "--save-as",
                &padded_secret_text,
            ],
            secret_shaped_name_stderr,
        ),
        (
            vec!["attach", "--remote", &secret_text],
            secret_shaped_server_stderr,
        ),
        (
            vec!["list-sessions", "--remote", &padded_secret_text],
            secret_shaped_server_stderr,
        ),
        (
            vec!["remote", "edit", &secret_text],
            secret_shaped_server_stderr,
        ),
        (
            vec!["remote", "forget", &secret_text],
            secret_shaped_server_stderr,
        ),
        (
            vec!["remote", "set-secret", &secret_text],
            secret_shaped_server_stderr,
        ),
    ];

    for (command_arguments, expected_stderr) in refused_argument_cases {
        let home_directory = build_short_test_directory();
        write_saved_server_store(
            home_directory.path(),
            vec![build_saved_server(
                "work",
                saved_server_address.clone(),
                Some(saved_certificate_fingerprint.clone()),
            )],
        );

        let refused_output = build_koshi_command_under_home(home_directory.path())
            .args(&command_arguments)
            .output()
            .expect("the koshi binary runs");

        let refused_stderr = String::from_utf8_lossy(&refused_output.stderr);
        assert_eq!(refused_stderr, expected_stderr, "{command_arguments:?}");
        assert!(
            !refused_stderr.contains(&secret_text),
            "{command_arguments:?}"
        );
        assert_eq!(String::from_utf8_lossy(&refused_output.stdout), "");
        assert_eq!(
            refused_output.status.code(),
            Some(CliExitCode::UsageOrConfig.get_exit_code()),
            "{command_arguments:?}"
        );
        assert_eq!(
            list_saved_server_records(home_directory.path()),
            vec![(
                Some("work".to_string()),
                saved_server_address.clone(),
                Some(saved_certificate_fingerprint.clone())
            )],
            "{command_arguments:?}"
        );
    }
}

#[test]
fn a_session_in_this_machines_shared_directory_is_refused_with_the_remote_flag() {
    let owned_session_text = format!("1001/{}", SessionId::new());
    let home_directory = build_short_test_directory();
    let refused_argument_cases: [(Vec<&str>, String); 3] = [
        (
            vec!["attach", "--remote", "some-other-box", &owned_session_text],
            format!(
                "koshi: invalid arguments: --remote takes a session id or name, not \
                 `{owned_session_text}`, which names a folder of this machine's shared directory\n"
            ),
        ),
        (
            vec![
                "close-tab",
                "--remote",
                "some-other-box",
                "--session",
                &owned_session_text,
            ],
            format!(
                "koshi: invalid arguments: --remote takes a session id or name, not \
                 `{owned_session_text}`, which names a folder of this machine's shared directory\n"
            ),
        ),
        (
            vec!["attach", "--remote", "some-other-box", "1001/quiet-lake"],
            "koshi: invalid arguments: expected `<user id>/session-<uuid>`, such as \
             `1001/session-<uuid>`\n"
                .to_string(),
        ),
    ];

    for (command_arguments, expected_stderr) in refused_argument_cases {
        let refused_output = build_koshi_command_under_home(home_directory.path())
            .args(&command_arguments)
            .output()
            .expect("the koshi binary runs");

        assert_eq!(
            String::from_utf8_lossy(&refused_output.stderr),
            expected_stderr,
            "{command_arguments:?}"
        );
        assert_eq!(String::from_utf8_lossy(&refused_output.stdout), "");
        assert_eq!(
            refused_output.status.code(),
            Some(CliExitCode::UsageOrConfig.get_exit_code()),
            "{command_arguments:?}"
        );
        assert_eq!(list_saved_server_records(home_directory.path()), Vec::new());
    }
}

/// A saved server at `server_address` holding `certificate_fingerprint`, the
/// secret `test-secret` and no name.
fn build_unnamed_saved_server(
    server_address: String,
    certificate_fingerprint: String,
) -> SavedServer {
    SavedServer {
        server_name: None,
        ..build_saved_server("work", server_address, Some(certificate_fingerprint))
    }
}

#[test]
fn attach_save_as_names_a_saved_server_that_has_no_name_once_it_admits_the_connection() {
    // The stand-in server lists no session: the attach finds none to join
    // after the name is saved.
    let (server_address, certificate_fingerprint) = spawn_stand_in_remote_server(vec![
        RemoteServerFrame::Welcome {
            remote_protocol_version: REMOTE_PROTOCOL_VERSION,
        },
        RemoteServerFrame::Sessions {
            session_rows: Vec::new(),
        },
    ]);
    let home_directory = build_short_test_directory();
    write_saved_server_store(
        home_directory.path(),
        vec![build_unnamed_saved_server(
            server_address.clone(),
            certificate_fingerprint.clone(),
        )],
    );

    let attach_output = build_koshi_command_under_home(home_directory.path())
        .args(["attach", "--remote", &server_address, "--save-as", "work"])
        .output()
        .expect("the koshi binary runs");

    assert_eq!(
        String::from_utf8_lossy(&attach_output.stderr),
        format!("koshi: no session is reachable on {server_address}\n")
    );
    assert_eq!(String::from_utf8_lossy(&attach_output.stdout), "");
    assert_eq!(
        attach_output.status.code(),
        Some(CliExitCode::RuntimeAction.get_exit_code())
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
fn attach_save_as_leaves_a_saved_server_unnamed_when_its_certificate_is_not_the_expected_one() {
    let (server_address, presented_certificate_fingerprint) =
        spawn_stand_in_remote_server(vec![RemoteServerFrame::Welcome {
            remote_protocol_version: REMOTE_PROTOCOL_VERSION,
        }]);
    let saved_certificate_fingerprint = "0".repeat(64);
    let home_directory = build_short_test_directory();
    write_saved_server_store(
        home_directory.path(),
        vec![build_unnamed_saved_server(
            server_address.clone(),
            saved_certificate_fingerprint.clone(),
        )],
    );

    let attach_output = build_koshi_command_under_home(home_directory.path())
        .args(["attach", "--remote", &server_address, "--save-as", "work"])
        .output()
        .expect("the koshi binary runs");

    assert_eq!(
        String::from_utf8_lossy(&attach_output.stderr),
        format!(
            "koshi: IPC unavailable: the certificate {server_address} presented is not the \
             expected one: expected sha256:{saved_certificate_fingerprint}, presented \
             sha256:{presented_certificate_fingerprint}. the secret was not sent. if that \
             machine's certificate changed on purpose, `koshi share open` there prints the new \
             fingerprint: give it with `koshi remote edit {server_address}`\n"
        )
    );
    assert_eq!(String::from_utf8_lossy(&attach_output.stdout), "");
    assert_eq!(
        attach_output.status.code(),
        Some(CliExitCode::IpcUnavailable.get_exit_code())
    );
    assert_eq!(
        list_saved_server_records(home_directory.path()),
        vec![(None, server_address, Some(saved_certificate_fingerprint))]
    );
}

#[test]
fn attach_save_as_refuses_a_name_a_saved_server_cannot_take_before_any_dial() {
    // Nothing listens on either address: a dial would end in a refusal of
    // its own.
    let unnamed_server_address = "127.0.0.1:1".to_string();
    let named_server_address = "127.0.0.1:2".to_string();
    let saved_certificate_fingerprint = "0".repeat(64);
    let saved_servers = vec![
        build_unnamed_saved_server(
            unnamed_server_address.clone(),
            saved_certificate_fingerprint.clone(),
        ),
        build_saved_server(
            "work",
            named_server_address.clone(),
            Some(saved_certificate_fingerprint.clone()),
        ),
    ];
    let refused_name_cases = [
        (
            "work",
            format!(
                "koshi: invalid arguments: the name work already belongs to \
                 {named_server_address}; run `koshi remote forget work` first, or pick another \
                 name\n"
            ),
        ),
        (
            "127.0.0.1:3",
            "koshi: invalid arguments: 127.0.0.1:3 is the shape of an address, and a saved \
             name must not be: a lookup would take it for the server listening there. Pick a \
             plain name.\n"
                .to_string(),
        ),
    ];

    for (refused_name, expected_stderr) in refused_name_cases {
        let home_directory = build_short_test_directory();
        write_saved_server_store(home_directory.path(), saved_servers.clone());

        let attach_output = build_koshi_command_under_home(home_directory.path())
            .args([
                "attach",
                "--remote",
                &unnamed_server_address,
                "--save-as",
                refused_name,
            ])
            .output()
            .expect("the koshi binary runs");

        assert_eq!(
            String::from_utf8_lossy(&attach_output.stderr),
            expected_stderr,
            "{refused_name}"
        );
        assert_eq!(String::from_utf8_lossy(&attach_output.stdout), "");
        assert_eq!(
            attach_output.status.code(),
            Some(CliExitCode::UsageOrConfig.get_exit_code()),
            "{refused_name}"
        );
        assert_eq!(
            list_saved_server_records(home_directory.path()),
            vec![
                (
                    None,
                    unnamed_server_address.clone(),
                    Some(saved_certificate_fingerprint.clone())
                ),
                (
                    Some("work".to_string()),
                    named_server_address.clone(),
                    Some(saved_certificate_fingerprint.clone())
                ),
            ],
            "{refused_name}"
        );
    }
}

#[test]
fn the_connect_command_a_grant_prints_runs_twice_with_the_same_answer_and_one_saved_server() {
    // The stand-in server lists no session: each attach finds none to join
    // after the dial.
    let (server_address, certificate_fingerprint) = spawn_stand_in_remote_server(vec![
        RemoteServerFrame::Welcome {
            remote_protocol_version: REMOTE_PROTOCOL_VERSION,
        },
        RemoteServerFrame::Sessions {
            session_rows: Vec::new(),
        },
    ]);
    let home_directory = build_short_test_directory();
    let grant_output = koshi::output::render_remote_ready(
        Some("alice"),
        &koshi::output::RemoteReady::On {
            remote_listen_address: server_address
                .parse()
                .expect("the stand-in address is a socket address"),
            host_addresses: Vec::new(),
            host_name: Some("desk".to_string()),
            certificate_fingerprint: certificate_fingerprint.clone(),
            config_mismatch: None,
        },
    );
    let certificate_fingerprint_text = format!("sha256:{certificate_fingerprint}");
    let connect_command_line = grant_output
        .lines()
        .find(|grant_line| grant_line.trim_start().starts_with("koshi attach"))
        .expect("the grant prints a connect command");
    let connect_arguments: Vec<&str> = connect_command_line
        .trim()
        .strip_prefix("koshi ")
        .and_then(|connect_command| connect_command.strip_suffix(" [SESSION]"))
        .expect("the connect command starts with `koshi` and ends with `[SESSION]`")
        .split(' ')
        .collect();
    assert_eq!(
        connect_arguments,
        vec![
            "attach",
            "--remote",
            &server_address,
            "--certificate-fingerprint",
            &certificate_fingerprint_text,
            "--save-as",
            "desk"
        ]
    );

    for attach_run_number in 1..=2 {
        let attach_output = build_koshi_command_under_home(home_directory.path())
            .args(&connect_arguments)
            .env("KOSHI_REMOTE_SECRET", "test-secret")
            .output()
            .expect("the koshi binary runs");

        assert_eq!(
            String::from_utf8_lossy(&attach_output.stderr),
            format!("koshi: no session is reachable on {server_address}\n"),
            "run {attach_run_number}"
        );
        assert_eq!(
            String::from_utf8_lossy(&attach_output.stdout),
            "",
            "run {attach_run_number}"
        );
        assert_eq!(
            attach_output.status.code(),
            Some(CliExitCode::RuntimeAction.get_exit_code()),
            "run {attach_run_number}"
        );
        assert_eq!(
            list_saved_server_records(home_directory.path()),
            vec![(
                Some("desk".to_string()),
                server_address.clone(),
                Some(certificate_fingerprint.clone())
            )],
            "run {attach_run_number}"
        );
    }
}
