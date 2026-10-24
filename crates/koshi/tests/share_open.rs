//! `koshi share open` opens the remote listener on the address the router read
//! from `koshi.kdl`, mints no token, and prints the commands that connect.
//!
//! Each test runs the real binary under a fresh test home, against a router
//! the test starts or the one the command starts. The listen address is a
//! loopback port found by binding port `0` and releasing it, and another
//! program can take it before the router binds it: a test that meets the
//! refusal naming its port tries a new one.

mod common;

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Output;

use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::router::{resolve_router_endpoint_path, RouterRequestKind, RouterResult};
use koshi_pty::working_directory::get_local_hostname;

use common::{
    build_koshi_command_under_home, build_short_test_directory, connect_to_router,
    resolve_config_directory_under_home, resolve_runtime_directory_under_home, send_router_request,
    start_router_process, terminate_process, write_test_config,
};

/// How many free ports a test tries before it gives up.
const MAX_ADDRESS_ATTEMPT_COUNT: usize = 8;

/// The exit code a failed `koshi share open` ends with:
/// `CliExitCode::RuntimeAction`.
const RUNTIME_ACTION_EXIT_CODE: i32 = 1;

/// A loopback address whose port was free a moment ago.
fn find_free_loopback_address() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .expect("a free loopback port")
        .local_addr()
        .expect("the free port's address can be read")
}

/// Write a `koshi.kdl` under `home_directory` whose `remote-listen` names
/// `remote_listen_address`, or that names none for `None`.
fn write_remote_listen_config(home_directory: &Path, remote_listen_address: Option<SocketAddr>) {
    let config_text = match remote_listen_address {
        Some(remote_listen_address) => {
            format!("version 2\nremote-listen \"{remote_listen_address}\"\n")
        }
        None => "version 2\n".to_string(),
    };
    write_test_config(home_directory, &config_text);
}

/// The `koshi.kdl` a koshi run under `home_directory` reads.
fn resolve_config_file_path(home_directory: &Path) -> PathBuf {
    resolve_config_directory_under_home(home_directory).join("koshi.kdl")
}

/// Run `koshi <arguments>` under `home_directory` and hand back its output.
fn run_koshi(home_directory: &Path, arguments: &[&str]) -> Output {
    build_koshi_command_under_home(home_directory)
        .args(arguments)
        .output()
        .expect("the koshi binary runs")
}

/// The fingerprint of the certificate the router serving `runtime_directory`
/// presents, as its status names it.
fn read_served_certificate_fingerprint(runtime_directory: &Path) -> String {
    let mut router_connection = connect_to_router(runtime_directory);
    match send_router_request(&mut router_connection, RouterRequestKind::RemoteStatus) {
        RouterResult::RemoteStatus {
            certificate_fingerprint: Some(certificate_fingerprint),
            ..
        } => certificate_fingerprint,
        unexpected_router_result => {
            panic!("the router names the certificate it serves, got {unexpected_router_result:?}")
        }
    }
}

/// End the router `runtime_directory` advertises, when one does.
fn end_advertised_router(runtime_directory: &Path) {
    if let Ok(router_endpoint) =
        EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory))
    {
        terminate_process(router_endpoint.process_id);
    }
}

/// What `koshi share open` prints on standard output once the router serves
/// `remote_listen_address`, a loopback address, with the certificate
/// `certificate_fingerprint`: the block
/// [`render_remote_ready`](koshi::output::render_remote_ready) renders after
/// no grant, for no other address of this machine and the host name
/// [`get_local_hostname`] reads. The text of that block is pinned in
/// `crates/koshi/src/share/tests.rs`.
fn build_expected_loopback_block(
    remote_listen_address: SocketAddr,
    certificate_fingerprint: &str,
) -> String {
    koshi::output::render_remote_ready(
        None,
        &koshi::output::RemoteReady::On {
            remote_listen_address,
            host_addresses: Vec::new(),
            host_name: get_local_hostname(),
            certificate_fingerprint: certificate_fingerprint.to_string(),
            config_mismatch: None,
        },
    )
}

/// Whether `share_open_output` is the refusal of a port another program took
/// between the test finding it free and the router binding it.
fn is_port_taken_refusal(share_open_output: &Output, remote_listen_address: SocketAddr) -> bool {
    share_open_output.status.code() == Some(RUNTIME_ACTION_EXIT_CODE)
        && String::from_utf8_lossy(&share_open_output.stderr).contains(&format!(
            "the port did not open: the remote listener could not open {remote_listen_address}: "
        ))
}

#[test]
fn a_remote_listen_added_after_the_router_started_is_named_and_opens_after_a_restart() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = resolve_runtime_directory_under_home(test_home_directory.path());
    let config_file_path = resolve_config_file_path(test_home_directory.path());
    write_remote_listen_config(test_home_directory.path(), None);
    let _router = start_router_process(test_home_directory.path(), &runtime_directory);
    drop(connect_to_router(&runtime_directory));

    let mut router_listen_description = "no remote listen address".to_string();
    for _ in 0..MAX_ADDRESS_ATTEMPT_COUNT {
        let remote_listen_address = find_free_loopback_address();
        write_remote_listen_config(test_home_directory.path(), Some(remote_listen_address));

        let mismatch_output = run_koshi(test_home_directory.path(), &["share", "open"]);
        assert_eq!(
            String::from_utf8_lossy(&mismatch_output.stderr),
            format!(
                "koshi: {} names the remote listen address {remote_listen_address}, and the \
                 running router read {router_listen_description} from it when it started. Run \
                 `koshi restart-servers` so the router reads it again, then run `koshi share \
                 open`.\n",
                config_file_path.display()
            )
        );
        assert_eq!(String::from_utf8_lossy(&mismatch_output.stdout), "");
        assert_eq!(
            mismatch_output.status.code(),
            Some(RUNTIME_ACTION_EXIT_CODE)
        );

        let restart_output = run_koshi(test_home_directory.path(), &["restart-servers"]);
        assert_eq!(
            restart_output.status.code(),
            Some(0),
            "the router restarts: {}",
            String::from_utf8_lossy(&restart_output.stderr)
        );

        let open_output = run_koshi(test_home_directory.path(), &["share", "open"]);
        if is_port_taken_refusal(&open_output, remote_listen_address) {
            router_listen_description =
                format!("the remote listen address {remote_listen_address}");
            continue;
        }
        let certificate_fingerprint = read_served_certificate_fingerprint(&runtime_directory);
        end_advertised_router(&runtime_directory);
        assert_eq!(String::from_utf8_lossy(&open_output.stderr), "");
        assert_eq!(
            String::from_utf8_lossy(&open_output.stdout),
            build_expected_loopback_block(remote_listen_address, &certificate_fingerprint)
        );
        assert_eq!(open_output.status.code(), Some(0));
        return;
    }
    end_advertised_router(&runtime_directory);
    panic!("no free port opened in {MAX_ADDRESS_ATTEMPT_COUNT} tries");
}

#[test]
fn share_open_with_no_router_running_starts_one_opens_the_port_and_prints_it_again() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = resolve_runtime_directory_under_home(test_home_directory.path());

    for _ in 0..MAX_ADDRESS_ATTEMPT_COUNT {
        let remote_listen_address = find_free_loopback_address();
        write_remote_listen_config(test_home_directory.path(), Some(remote_listen_address));

        let first_open_output = run_koshi(test_home_directory.path(), &["share", "open"]);
        if is_port_taken_refusal(&first_open_output, remote_listen_address) {
            end_advertised_router(&runtime_directory);
            continue;
        }
        let second_open_output = run_koshi(test_home_directory.path(), &["share", "open"]);
        let certificate_fingerprint = read_served_certificate_fingerprint(&runtime_directory);
        end_advertised_router(&runtime_directory);

        let expected_block =
            build_expected_loopback_block(remote_listen_address, &certificate_fingerprint);
        assert_eq!(String::from_utf8_lossy(&first_open_output.stderr), "");
        assert_eq!(
            String::from_utf8_lossy(&first_open_output.stdout),
            expected_block
        );
        assert_eq!(first_open_output.status.code(), Some(0));
        assert_eq!(
            String::from_utf8_lossy(&second_open_output.stdout),
            expected_block,
            "a router already serving prints the same block"
        );
        assert_eq!(second_open_output.status.code(), Some(0));
        return;
    }
    panic!("no free port opened in {MAX_ADDRESS_ATTEMPT_COUNT} tries");
}

#[test]
fn share_open_with_no_address_names_the_line_to_set_and_opens_nothing() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = resolve_runtime_directory_under_home(test_home_directory.path());
    write_remote_listen_config(test_home_directory.path(), None);

    let open_output = run_koshi(test_home_directory.path(), &["share", "open"]);
    end_advertised_router(&runtime_directory);

    assert_eq!(
        String::from_utf8_lossy(&open_output.stderr),
        "koshi: koshi.kdl names no remote listen address. Set `remote-listen \"<ip>:<port>\"` in \
         koshi.kdl, run `koshi restart-servers`, then run `koshi share open` again.\n"
    );
    assert_eq!(String::from_utf8_lossy(&open_output.stdout), "");
    assert_eq!(open_output.status.code(), Some(RUNTIME_ACTION_EXIT_CODE));
}

#[test]
fn share_open_on_a_port_another_program_holds_prints_the_routers_reason() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = resolve_runtime_directory_under_home(test_home_directory.path());
    let holding_listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port to hold");
    let held_address = holding_listener
        .local_addr()
        .expect("the held port's address can be read");
    let bind_error = TcpListener::bind(held_address)
        .expect_err("a second bind of the held port is refused")
        .to_string();
    write_remote_listen_config(test_home_directory.path(), Some(held_address));

    let open_output = run_koshi(test_home_directory.path(), &["share", "open"]);
    end_advertised_router(&runtime_directory);
    drop(holding_listener);

    assert_eq!(
        String::from_utf8_lossy(&open_output.stderr),
        format!(
            "koshi: remote access stays off, and the port did not open: the remote listener could \
             not open {held_address}: {bind_error}. Run `koshi share open` once that is fixed.\n"
        )
    );
    assert_eq!(String::from_utf8_lossy(&open_output.stdout), "");
    assert_eq!(open_output.status.code(), Some(RUNTIME_ACTION_EXIT_CODE));
}
