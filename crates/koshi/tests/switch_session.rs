//! What an attached client reads when its session moves it to another one.
//!
//! A real `koshi` binary runs as the router, the router starts two real
//! session servers, and the test joins the first the way the attached client
//! does — Hello then Attach on one connection. It then submits
//! `SwitchSession` naming the second session on that same connection and reads
//! the event stream until a frame ends it.
//!
//! The test checks the command crossing a real socket and the exact frame the
//! session server writes back. The command carries a key-binding source from
//! the attached client.
//!
//! Every test serves its own temporary runtime directory under a short base.
//! Every router runs under its own temporary home, and so does every session
//! server it starts. Every process a test starts is held in a guard that ends
//! it when the test drops it.

use koshi_core::command::{Command, CommandEnvelope, CommandSource, SwitchSessionArgs};
use koshi_core::ids::{ClientId, CommandId};
use koshi_ipc::event::SessionEvent;
use koshi_ipc::protocol::{IpcRequest, IpcRequestKind};
use koshi_ipc::transport::Connection;

mod common;

use common::session_connection::{
    attach_client_on_connection, read_session_ending, wait_for_session_connection,
};
use common::{
    build_short_test_directory, connect_to_router, create_session, start_router_process,
    RunningSessions,
};
use koshi_test_support::fixtures::build_test_runtime_directory;

/// Send `command` as request `3` on the attached client's own connection,
/// from a key binding of `client_id`. The session writes no reply to it; what
/// the command does arrives on the event stream.
fn submit_session_command(connection: &mut Connection, client_id: ClientId, command: Command) {
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client_id),
        command,
    );
    let submit_request = IpcRequest {
        request_id: 3,
        request_kind: IpcRequestKind::SubmitCommand(Box::new(command_envelope)),
    };
    connection
        .send(&submit_request)
        .expect("the server reads the command");
}

#[test]
fn a_switch_ends_the_stream_with_the_session_to_join_next() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_test_runtime_directory();
    let _router_process =
        start_router_process(test_home_directory.path(), runtime_directory.path());
    let mut router_connection = connect_to_router(runtime_directory.path());

    let source_session = create_session(&mut router_connection);
    let target_session = create_session(&mut router_connection);
    let _session_processes = RunningSessions {
        session_server_process_ids: vec![source_session.process_id, target_session.process_id],
    };

    let (mut source_connection, _) =
        wait_for_session_connection(runtime_directory.path(), source_session.session_id);
    let client_id = attach_client_on_connection(&mut source_connection, source_session.session_id);

    submit_session_command(
        &mut source_connection,
        client_id,
        Command::SwitchSession(SwitchSessionArgs {
            client_id: None,
            session_id: target_session.session_id,
        }),
    );

    assert_eq!(
        read_session_ending(source_connection).expect("the stream ends with a frame"),
        SessionEvent::SwitchTo {
            session_id: target_session.session_id
        }
    );
}
