//! A terminal viewing a session on another machine while the saved server it
//! joined through changes in the saved-server store.
//!
//! A router runs as its own process, serves remote clients on a loopback port,
//! and starts one session. `koshi attach --remote <address> --save-as work`
//! joins that session with a secret granted to `viewer`, and saves the server
//! as `work`. The test then changes the saved server in the store, as `koshi
//! remote edit`, `koshi remote set-secret` and `koshi remote forget` do, and
//! revokes the grant to `viewer`, which ends the client's connection. Each
//! dial the client makes after that, and the way back it prints when it stops,
//! reads the saved server at the address it joined through from the store. One
//! test overwrites the store with bytes that are not a store.
//!
//! Every wait is bounded by [`WAIT_DURATION`](common::WAIT_DURATION).

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::time::Instant;

use koshi_core::command::CliExitCode;
use koshi_core::ids::{ClientId, SessionId};
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::remote_migration::migrate_saved_server_file;
use koshi_ipc::remote_servers::{resolve_server_store_path, SavedServer, ServerStore};
use koshi_ipc::remote_tokens::TokenScope;
use koshi_ipc::router::{RouterRequestKind, RouterResult};
use koshi_ipc::transport::Connection;

use common::{
    build_koshi_command_under_home, build_saved_server, build_short_test_directory, create_session,
    format_client_exit, resolve_data_directory_under_home, resolve_runtime_directory_under_home,
    send_router_request, start_router_with_remote_access, wait_for_attached_client_count,
    wait_for_client_exit, write_saved_server_store, RunningClient, RunningRouter, RunningSessions,
    POLL_INTERVAL_DURATION, WAIT_DURATION,
};

/// The `koshi.kdl` lines after `remote-listen` for a client that dials again
/// when its link drops.
const RECONNECT_CONFIG_TEXT: &str = "remote-reconnect #true\n";

/// The `koshi.kdl` lines after `remote-listen` for a client that ends when its
/// link drops.
const NO_RECONNECT_CONFIG_TEXT: &str = "remote-reconnect #false\n";

/// One router with remote access on, one session, and one client attached to
/// that session through the server saved as `work`.
struct AttachedViewer {
    /// The test home every process runs under.
    home_directory: tempfile::TempDir,
    /// The runtime directory under `home_directory`.
    runtime_directory: std::path::PathBuf,
    /// The router serving the session and the remote listener.
    _router: RunningRouter,
    /// A connection to the router, for grants and revokes.
    router_connection: Connection,
    /// The session's processes, ended when the test ends.
    _session_processes: RunningSessions,
    /// Where the remote listener serves.
    listener_address: SocketAddr,
    /// The session the client joined.
    session_id: SessionId,
    /// The client the session lists once the attach joined.
    first_client_id: ClientId,
    /// The `koshi attach --remote` client.
    remote_client: RunningClient,
}

/// Start a router whose `koshi.kdl` ends with `remote_config_text`, start one
/// session, grant `viewer` a host-wide token, and attach a client to the
/// session with `koshi attach --remote <listener> --certificate-fingerprint
/// sha256:<fingerprint> --save-as work <session id>`, presenting that token
/// in `KOSHI_REMOTE_SECRET`. Returns once the session lists the client.
fn attach_viewer_saved_as_work(remote_config_text: &str) -> AttachedViewer {
    let home_directory = build_short_test_directory();
    let runtime_directory = resolve_runtime_directory_under_home(home_directory.path());
    let (router, mut router_connection, listener_address, certificate_fingerprint) =
        start_router_with_remote_access(
            home_directory.path(),
            &runtime_directory,
            remote_config_text,
        );
    let created_session = create_session(&mut router_connection);
    let session_processes = RunningSessions {
        runtime_directory: runtime_directory.clone(),
        session_addresses: vec![created_session.clone()],
    };
    let session_id = created_session.session_id;
    let connection_token = grant_host_wide_token(&mut router_connection, "viewer");
    let child_process = build_koshi_command_under_home(home_directory.path())
        .arg("attach")
        .arg("--remote")
        .arg(listener_address.to_string())
        .arg("--certificate-fingerprint")
        .arg(format!("sha256:{certificate_fingerprint}"))
        .arg("--save-as")
        .arg("work")
        .arg(session_id.to_string())
        .env("KOSHI_REMOTE_SECRET", connection_token.expose_secret())
        .spawn()
        .expect("the koshi binary starts");
    let mut remote_client = RunningClient { child_process };
    let mut client_output_stream = remote_client
        .child_process
        .stdout
        .take()
        .expect("the client's output is a pipe");
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut client_output_stream, &mut std::io::sink());
    });
    wait_for_attached_client_count(&runtime_directory, session_id, 1, &mut remote_client);
    let first_client_id = list_attached_client_ids(&runtime_directory, session_id)[0];
    AttachedViewer {
        home_directory,
        runtime_directory,
        _router: router,
        router_connection,
        _session_processes: session_processes,
        listener_address,
        session_id,
        first_client_id,
        remote_client,
    }
}

/// Ask the router for a host-wide grant to `identity`, and hand back its
/// secret.
///
/// # Panics
/// When the router answers anything else than `Granted`.
fn grant_host_wide_token(router_connection: &mut Connection, identity: &str) -> ConnectionToken {
    match send_router_request(
        router_connection,
        RouterRequestKind::GrantToken {
            identity: identity.to_string(),
            scope: TokenScope::HostWide,
            expires_in: None,
        },
    ) {
        RouterResult::Granted {
            connection_token, ..
        } => connection_token,
        unexpected_result => panic!("the grant was answered with {unexpected_result:?}"),
    }
}

/// Ask the router to stop every grant `identity` holds. The router ends every
/// connection those grants admitted.
///
/// # Panics
/// When the router answers anything else than `Revoked` naming the host-wide
/// grant.
fn revoke_every_grant(router_connection: &mut Connection, identity: &str) {
    assert_eq!(
        send_router_request(
            router_connection,
            RouterRequestKind::RevokeToken {
                identity: identity.to_string(),
                scope: None,
            },
        ),
        RouterResult::Revoked(vec![TokenScope::HostWide])
    );
}

/// Every server the saved-server store under `home_directory` holds, in store
/// order.
///
/// # Panics
/// When the store cannot be read.
fn load_saved_servers(home_directory: &Path) -> Vec<SavedServer> {
    ServerStore::load_server_store_from_path(&resolve_server_store_path(
        &resolve_data_directory_under_home(home_directory),
    ))
    .expect("the saved-server store is read")
    .saved_servers
}

/// The ids of the clients the overview of session `session_id` lists.
///
/// # Panics
/// When the overview cannot be read.
fn list_attached_client_ids(runtime_directory: &Path, session_id: SessionId) -> Vec<ClientId> {
    koshi_link::discovery::fetch_session_overview(runtime_directory, None, session_id, None, None)
        .expect("the session server describes itself")
        .clients
        .into_iter()
        .map(|client_discovery| client_discovery.client_id)
        .collect()
}

/// Poll the overview of session `session_id` every [`POLL_INTERVAL_DURATION`]
/// until it lists one client, and that client is not `replaced_client_id`.
///
/// # Panics
/// When the overview cannot be read, or lists another set of clients after
/// [`WAIT_DURATION`]. The message names how `running_client` exited, when it
/// has.
fn wait_for_one_client_other_than(
    runtime_directory: &Path,
    session_id: SessionId,
    replaced_client_id: ClientId,
    running_client: &mut RunningClient,
) {
    let wait_deadline = Instant::now() + WAIT_DURATION;
    loop {
        let attached_client_ids = list_attached_client_ids(runtime_directory, session_id);
        if attached_client_ids.len() == 1 && attached_client_ids[0] != replaced_client_id {
            return;
        }
        assert!(
            Instant::now() < wait_deadline,
            "session {session_id} lists the clients {attached_client_ids:?}; {}",
            format_client_exit(running_client)
                .unwrap_or_else(|| "the client is still running".to_string())
        );
        std::thread::sleep(POLL_INTERVAL_DURATION);
    }
}

/// `saved_server` renamed `server_name`, as `koshi remote edit` leaves it.
fn rename_saved_server(saved_server: &SavedServer, server_name: &str) -> SavedServer {
    SavedServer {
        server_name: Some(server_name.to_string()),
        ..saved_server.clone()
    }
}

#[test]
fn a_viewer_whose_saved_server_was_renamed_names_the_new_name_when_its_link_ends() {
    let mut attached_viewer = attach_viewer_saved_as_work(NO_RECONNECT_CONFIG_TEXT);
    let home_directory = attached_viewer.home_directory.path().to_path_buf();
    let listener_address = attached_viewer.listener_address;
    let session_id = attached_viewer.session_id;
    let saved_servers = load_saved_servers(&home_directory);
    assert_eq!(
        saved_servers
            .iter()
            .map(|saved_server| (
                saved_server.server_name.clone(),
                saved_server.server_address.clone()
            ))
            .collect::<Vec<_>>(),
        vec![(Some("work".to_string()), listener_address.to_string())]
    );

    // `work` becomes `lab`, and another machine is then saved as `work`.
    write_saved_server_store(
        &home_directory,
        vec![
            rename_saved_server(&saved_servers[0], "lab"),
            build_saved_server("work", "192.0.2.30:7654".to_string(), None),
        ],
    );
    revoke_every_grant(&mut attached_viewer.router_connection, "viewer");

    // With `remote-reconnect` off, the ended link ends the client.
    let (exit_status, client_error_text) = wait_for_client_exit(&mut attached_viewer.remote_client);
    assert_eq!(
        exit_status.code(),
        Some(CliExitCode::RuntimeAction.get_exit_code())
    );
    assert_eq!(
        client_error_text,
        format!(
            "koshi: the session ended unexpectedly\n  \
             run `koshi attach --remote lab` to see that server's sessions; \
             if session {session_id} is among them, reattach with \
             `koshi attach --remote lab {session_id}`\n"
        )
    );
}

#[test]
fn a_viewer_whose_saved_server_was_renamed_names_the_new_name_in_a_refused_dial() {
    let mut attached_viewer = attach_viewer_saved_as_work(RECONNECT_CONFIG_TEXT);
    let home_directory = attached_viewer.home_directory.path().to_path_buf();
    let listener_address = attached_viewer.listener_address;
    let session_id = attached_viewer.session_id;
    let saved_servers = load_saved_servers(&home_directory);

    write_saved_server_store(
        &home_directory,
        vec![
            rename_saved_server(&saved_servers[0], "lab"),
            build_saved_server("work", "192.0.2.30:7654".to_string(), None),
        ],
    );
    revoke_every_grant(&mut attached_viewer.router_connection, "viewer");

    // The client dials `lab` again with the revoked secret, and the server
    // refuses it. No dial follows a refusal.
    let (exit_status, client_error_text) = wait_for_client_exit(&mut attached_viewer.remote_client);
    assert_eq!(
        exit_status.code(),
        Some(CliExitCode::RuntimeAction.get_exit_code())
    );
    assert_eq!(
        client_error_text,
        format!(
            "koshi: the server {listener_address} did not admit the connection: the token \
             expired, was revoked, or was never granted on that machine, or that machine already \
             serves as many remote connections as it admits. `koshi share list` on that machine \
             shows when each grant expires or was revoked. to replace the token, run `koshi share \
             grant` there, then store the new secret with `koshi remote set-secret lab`\n  \
             the session continues without you\n  \
             run `koshi attach --remote lab` to see that server's sessions; \
             if session {session_id} is among them, reattach with \
             `koshi attach --remote lab {session_id}`\n"
        )
    );
}

#[test]
fn a_viewer_dials_again_with_the_secret_its_saved_server_holds_now() {
    let mut attached_viewer = attach_viewer_saved_as_work(RECONNECT_CONFIG_TEXT);
    let home_directory = attached_viewer.home_directory.path().to_path_buf();
    let saved_servers = load_saved_servers(&home_directory);
    let replacement_token =
        grant_host_wide_token(&mut attached_viewer.router_connection, "viewer-2");

    // `koshi remote set-secret work` stores the secret granted to `viewer-2`.
    write_saved_server_store(
        &home_directory,
        vec![SavedServer {
            connection_token: replacement_token.clone(),
            ..saved_servers[0].clone()
        }],
    );
    revoke_every_grant(&mut attached_viewer.router_connection, "viewer");

    // The client dials again with the secret the store holds, and the session
    // lists it under a fresh client id.
    wait_for_one_client_other_than(
        &attached_viewer.runtime_directory,
        attached_viewer.session_id,
        attached_viewer.first_client_id,
        &mut attached_viewer.remote_client,
    );
    assert_eq!(
        load_saved_servers(&home_directory)[0].connection_token,
        replacement_token
    );
}

/// What a client prints on its error stream when it stops dialing because the
/// saved-server store holds no server at `listener_address`, the address it
/// joined session `session_id` through.
fn build_not_saved_ending_text(listener_address: SocketAddr, session_id: SessionId) -> String {
    format!(
        "koshi: no saved server is at {listener_address} any more: it was forgotten, or saved \
         under another address\n  \
         the session continues without you\n  \
         run `koshi remote list` to find the name that server is saved under now, then reattach \
         with `koshi attach --remote <server> {session_id}`; if it is not saved, run `koshi share \
         grant` on that machine, then reattach with the command it prints, giving {session_id} \
         as the session\n"
    )
}

#[test]
fn a_viewer_whose_saved_server_was_forgotten_stops_dialing_and_names_the_next_step() {
    let mut attached_viewer = attach_viewer_saved_as_work(RECONNECT_CONFIG_TEXT);
    let home_directory = attached_viewer.home_directory.path().to_path_buf();

    // `koshi remote forget work` leaves the store empty.
    write_saved_server_store(&home_directory, Vec::new());
    revoke_every_grant(&mut attached_viewer.router_connection, "viewer");

    let (exit_status, client_error_text) = wait_for_client_exit(&mut attached_viewer.remote_client);
    assert_eq!(
        exit_status.code(),
        Some(CliExitCode::RuntimeAction.get_exit_code())
    );
    assert_eq!(
        client_error_text,
        build_not_saved_ending_text(attached_viewer.listener_address, attached_viewer.session_id)
    );
}

#[test]
fn a_viewer_whose_saved_server_moved_to_another_address_stops_dialing_and_names_the_next_step() {
    let mut attached_viewer = attach_viewer_saved_as_work(RECONNECT_CONFIG_TEXT);
    let home_directory = attached_viewer.home_directory.path().to_path_buf();
    let saved_servers = load_saved_servers(&home_directory);

    // `koshi remote edit work` keeps the name and gives another address. The
    // store holds no server at the address the client joined through.
    write_saved_server_store(
        &home_directory,
        vec![SavedServer {
            server_address: "192.0.2.30:7654".to_string(),
            ..saved_servers[0].clone()
        }],
    );
    revoke_every_grant(&mut attached_viewer.router_connection, "viewer");

    let (exit_status, client_error_text) = wait_for_client_exit(&mut attached_viewer.remote_client);
    assert_eq!(
        exit_status.code(),
        Some(CliExitCode::RuntimeAction.get_exit_code())
    );
    assert_eq!(
        client_error_text,
        build_not_saved_ending_text(attached_viewer.listener_address, attached_viewer.session_id)
    );
}

#[test]
fn a_viewer_whose_address_two_saved_servers_hold_stops_dialing_and_names_the_next_step() {
    let mut attached_viewer = attach_viewer_saved_as_work(RECONNECT_CONFIG_TEXT);
    let home_directory = attached_viewer.home_directory.path().to_path_buf();
    let listener_address = attached_viewer.listener_address;
    let session_id = attached_viewer.session_id;
    let saved_servers = load_saved_servers(&home_directory);

    // A store written by hand holds a second server at the same address.
    write_saved_server_store(
        &home_directory,
        vec![
            saved_servers[0].clone(),
            rename_saved_server(&saved_servers[0], "lab"),
        ],
    );
    revoke_every_grant(&mut attached_viewer.router_connection, "viewer");

    let (exit_status, client_error_text) = wait_for_client_exit(&mut attached_viewer.remote_client);
    assert_eq!(
        exit_status.code(),
        Some(CliExitCode::RuntimeAction.get_exit_code())
    );
    assert_eq!(
        client_error_text,
        format!(
            "koshi: more than one saved server is at {listener_address}\n  \
             the session continues without you\n  \
             run `koshi remote list` to see the saved servers at {listener_address}, keep one of \
             them, then reattach with `koshi attach --remote {listener_address} {session_id}`\n"
        )
    );
}

#[test]
fn a_viewer_whose_saved_servers_cannot_be_read_stops_dialing_and_names_the_next_step() {
    let mut attached_viewer = attach_viewer_saved_as_work(RECONNECT_CONFIG_TEXT);
    let listener_address = attached_viewer.listener_address;
    let session_id = attached_viewer.session_id;
    let server_store_path = resolve_server_store_path(&resolve_data_directory_under_home(
        attached_viewer.home_directory.path(),
    ));

    std::fs::write(&server_store_path, "not a saved-server store")
        .expect("the saved-server store is overwritten");
    revoke_every_grant(&mut attached_viewer.router_connection, "viewer");

    let (exit_status, client_error_text) = wait_for_client_exit(&mut attached_viewer.remote_client);
    // The client reads the store as `migrate_saved_server_file` and then
    // `ServerStore::load_server_store_from_path` read it.
    let store_read_error = migrate_saved_server_file(&server_store_path)
        .and_then(|()| ServerStore::load_server_store_from_path(&server_store_path).map(drop))
        .expect_err("the saved-server store cannot be read");
    assert_eq!(
        exit_status.code(),
        Some(CliExitCode::RuntimeAction.get_exit_code())
    );
    assert_eq!(
        client_error_text,
        format!(
            "koshi: the saved servers could not be read: {store_read_error}\n  \
             the session continues without you\n  \
             once the saved servers can be read, run `koshi attach --remote {listener_address}` \
             to see that server's sessions; if session {session_id} is among them, reattach with \
             `koshi attach --remote {listener_address} {session_id}`\n"
        )
    );
}
