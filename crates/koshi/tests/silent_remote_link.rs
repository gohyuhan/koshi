//! A terminal viewing a session on another machine whose link goes silent: no
//! byte arrives over the link, and nothing closes it.
//!
//! A router runs as its own process, serves remote clients on a loopback port,
//! and starts one session. A `koshi attach --remote` client reaches that port
//! through a relay in this process. The test home's `koshi.kdl` sets
//! `remote-link-timeout-ms 1000` and `remote-reconnect #false`. Once the client
//! is attached, the relay keeps both of its connections open and drops every
//! byte it reads from either one, as a network that loses every packet does.
//! Then the session removes the client and keeps running, and the client ends
//! with the message of a link that broke.
//!
//! Every wait is bounded by [`WAIT_DURATION`](common::WAIT_DURATION).

mod common;

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use koshi_core::command::CliExitCode;
use koshi_core::ids::SessionId;
use koshi_ipc::remote_tokens::TokenScope;
use koshi_ipc::router::{RouterRequestKind, RouterResult};
use koshi_ipc::transport::Connection;

use common::{
    build_koshi_command_under_home, build_short_test_directory, connect_to_router, create_session,
    resolve_runtime_directory_under_home, send_router_request, start_router_process,
    wait_for_attached_client_count, wait_for_client_exit, write_test_config, RunningClient,
    RunningRouter, RunningSessions,
};

/// The most bytes one read of a relay pump takes: 16 KiB.
const RELAY_READ_BYTE_COUNT: usize = 16 * 1024;

/// How many loopback ports the test tries before it gives up switching remote
/// access on.
const MAX_ADDRESS_ATTEMPT_COUNT: usize = 8;

/// A relay between `koshi attach --remote` and the router's remote listener.
///
/// Each connection the relay accepts gets its own connection to the listener,
/// and one pump thread per direction. Until [`SilentRelay::drop_every_byte`],
/// the pumps pass every byte on. After it, they drop every byte they read, and
/// the end of one side leaves the other side open. Dropping the relay shuts
/// every connection it holds and ends its accept thread.
struct SilentRelay {
    /// Where a client connects to reach the listener.
    relay_address: SocketAddr,
    /// Set by [`SilentRelay::drop_every_byte`].
    is_dropping_bytes: Arc<AtomicBool>,
    /// Set when the relay is dropped. The accept thread ends at the next
    /// connection it accepts.
    is_stopped: Arc<AtomicBool>,
    /// One handle of each connection the relay accepted or opened.
    relayed_streams: Arc<Mutex<Vec<TcpStream>>>,
}

impl SilentRelay {
    /// Drop every byte read from here on, in both directions, and keep every
    /// connection open.
    fn drop_every_byte(&self) {
        self.is_dropping_bytes.store(true, Ordering::SeqCst);
    }
}

impl Drop for SilentRelay {
    fn drop(&mut self) {
        self.is_stopped.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.relay_address);
        for relayed_stream in self
            .relayed_streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            let _ = relayed_stream.shutdown(Shutdown::Both);
        }
    }
}

/// Start a [`SilentRelay`] on a loopback port, in front of the listener at
/// `listener_address`.
fn start_silent_relay(listener_address: SocketAddr) -> SilentRelay {
    let relay_listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port for the relay");
    let relay_address = relay_listener
        .local_addr()
        .expect("the relay's address can be read");
    let is_dropping_bytes = Arc::new(AtomicBool::new(false));
    let is_stopped = Arc::new(AtomicBool::new(false));
    let relayed_streams = Arc::new(Mutex::new(Vec::new()));
    let accept_is_dropping_bytes = Arc::clone(&is_dropping_bytes);
    let accept_is_stopped = Arc::clone(&is_stopped);
    let accept_relayed_streams = Arc::clone(&relayed_streams);
    thread::spawn(move || {
        for accepted_stream in relay_listener.incoming() {
            if accept_is_stopped.load(Ordering::SeqCst) {
                return;
            }
            let Ok(client_stream) = accepted_stream else {
                return;
            };
            let Ok(listener_stream) = TcpStream::connect(listener_address) else {
                return;
            };
            let (Ok(client_source), Ok(listener_destination)) =
                (client_stream.try_clone(), listener_stream.try_clone())
            else {
                return;
            };
            let (Ok(listener_source), Ok(client_destination)) =
                (listener_stream.try_clone(), client_stream.try_clone())
            else {
                return;
            };
            accept_relayed_streams
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend([client_stream, listener_stream]);
            let client_pump_is_dropping_bytes = Arc::clone(&accept_is_dropping_bytes);
            thread::spawn(move || {
                pump_relayed_bytes(
                    client_source,
                    listener_destination,
                    &client_pump_is_dropping_bytes,
                );
            });
            let listener_pump_is_dropping_bytes = Arc::clone(&accept_is_dropping_bytes);
            thread::spawn(move || {
                pump_relayed_bytes(
                    listener_source,
                    client_destination,
                    &listener_pump_is_dropping_bytes,
                );
            });
        }
    });
    SilentRelay {
        relay_address,
        is_dropping_bytes,
        is_stopped,
        relayed_streams,
    }
}

/// Copy the bytes read from `source_stream` to `destination_stream` until a
/// read or a write fails, or `source_stream` ends.
///
/// While `is_dropping_bytes` is set, each byte read is dropped, and the end of
/// `source_stream` leaves `destination_stream` open. While it is clear, the end
/// of `source_stream` shuts the writing side of `destination_stream`.
fn pump_relayed_bytes(
    mut source_stream: TcpStream,
    mut destination_stream: TcpStream,
    is_dropping_bytes: &AtomicBool,
) {
    let mut relayed_bytes = vec![0_u8; RELAY_READ_BYTE_COUNT];
    loop {
        let read_byte_count = match source_stream.read(&mut relayed_bytes) {
            Ok(0) | Err(_) => break,
            Ok(read_byte_count) => read_byte_count,
        };
        if is_dropping_bytes.load(Ordering::SeqCst) {
            continue;
        }
        if destination_stream
            .write_all(&relayed_bytes[..read_byte_count])
            .is_err()
        {
            return;
        }
    }
    if !is_dropping_bytes.load(Ordering::SeqCst) {
        let _ = destination_stream.shutdown(Shutdown::Write);
    }
}

/// Write the `koshi.kdl` of `home_directory`, start a router under it serving
/// `runtime_directory`, and switch its remote access on. Hands back the
/// router, a connection to it, and the address its remote listener serves.
///
/// `koshi.kdl` sets `remote-link-timeout-ms 1000`, `remote-reconnect #false`,
/// and a free loopback port as `remote-listen`. The port is found by binding
/// port `0` and releasing it, and another program can take it before the
/// router binds it. A router that answers the switch with anything else than
/// `RemoteEnabled` is ended, and the next try writes a new port.
///
/// # Panics
/// When no try out of [`MAX_ADDRESS_ATTEMPT_COUNT`] switches remote access on.
/// The message holds the last answer.
fn start_router_with_remote_access(
    home_directory: &Path,
    runtime_directory: &Path,
) -> (RunningRouter, Connection, SocketAddr) {
    let mut last_router_result = None;
    for _ in 0..MAX_ADDRESS_ATTEMPT_COUNT {
        let remote_listen_port = TcpListener::bind("127.0.0.1:0")
            .expect("a free loopback port")
            .local_addr()
            .expect("the free port's address can be read")
            .port();
        write_test_config(
            home_directory,
            &format!(
                "version 2\n\
                 remote-listen \"127.0.0.1:{remote_listen_port}\"\n\
                 remote-link-timeout-ms 1000\n\
                 remote-reconnect #false\n"
            ),
        );
        let router = start_router_process(home_directory, runtime_directory);
        let mut router_connection = connect_to_router(runtime_directory);
        match send_router_request(&mut router_connection, RouterRequestKind::EnableRemote) {
            RouterResult::RemoteEnabled {
                remote_listen_address,
                ..
            } => return (router, router_connection, remote_listen_address),
            router_result => last_router_result = Some(router_result),
        }
    }
    panic!(
        "remote access was not switched on in {MAX_ADDRESS_ATTEMPT_COUNT} tries; \
         the last answer was {last_router_result:?}"
    );
}

/// Start `koshi attach --remote <relay_address> <session_id>` under
/// `home_directory`, with `remote_secret` in `KOSHI_REMOTE_SECRET`. A thread
/// reads the client's standard output to its end and drops it.
fn start_remote_client(
    home_directory: &Path,
    relay_address: SocketAddr,
    session_id: SessionId,
    remote_secret: &str,
) -> RunningClient {
    let mut child_process = build_koshi_command_under_home(home_directory)
        .arg("attach")
        .arg("--remote")
        .arg(relay_address.to_string())
        .arg(session_id.to_string())
        .env("KOSHI_REMOTE_SECRET", remote_secret)
        .spawn()
        .expect("the koshi binary starts");
    let mut client_output_stream = child_process
        .stdout
        .take()
        .expect("the client's output is a pipe");
    thread::spawn(move || {
        let _ = std::io::copy(&mut client_output_stream, &mut std::io::sink());
    });
    RunningClient { child_process }
}

#[test]
fn a_silent_link_ends_the_remote_viewer_and_its_session_removes_it() {
    let home_directory = build_short_test_directory();
    let runtime_directory = resolve_runtime_directory_under_home(home_directory.path());
    let (_router, mut router_connection, listener_address) =
        start_router_with_remote_access(home_directory.path(), &runtime_directory);
    let created_session = create_session(&mut router_connection);
    let _session_processes = RunningSessions {
        session_server_process_ids: vec![created_session.process_id],
    };
    let session_id = created_session.session_id;
    let connection_token = match send_router_request(
        &mut router_connection,
        RouterRequestKind::GrantToken {
            identity: "viewer".to_string(),
            scope: TokenScope::HostWide,
            expires_in: None,
        },
    ) {
        RouterResult::Granted {
            connection_token, ..
        } => connection_token,
        unexpected_result => panic!("the grant was answered with {unexpected_result:?}"),
    };
    let silent_relay = start_silent_relay(listener_address);
    let relay_address = silent_relay.relay_address;
    let mut remote_client = start_remote_client(
        home_directory.path(),
        relay_address,
        session_id,
        connection_token.expose_secret(),
    );
    wait_for_attached_client_count(&runtime_directory, session_id, 1, &mut remote_client);

    silent_relay.drop_every_byte();

    // The session reads no byte from the client for 1 second and removes it.
    // The overview still answers: the session keeps running.
    wait_for_attached_client_count(&runtime_directory, session_id, 0, &mut remote_client);
    // With `remote-reconnect` off, the client that reads no byte for 1 second
    // counts the link as broken and ends.
    let (exit_status, client_error_text) = wait_for_client_exit(&mut remote_client);
    assert_eq!(
        exit_status.code(),
        Some(CliExitCode::RuntimeAction.get_exit_code())
    );
    assert_eq!(
        client_error_text,
        format!(
            "koshi: the session ended unexpectedly\n  \
             run `koshi attach --remote {relay_address}` to see that server's sessions; \
             if session {session_id} is among them, reattach with \
             `koshi attach --remote {relay_address} {session_id}`\n"
        )
    );
}
