//! Tests for the runtime-directory fixture, the key-event fixture, and the
//! fixture that closes a connection once its peer hangs up.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use koshi_core::key::{Key, NamedKey};
use koshi_ipc::transport::Listener;

use super::*;

#[test]
fn a_built_key_input_projects_back_to_the_chord_it_was_built_from() {
    for modifier_bits in 0..=0b1111 {
        let modifier_flags =
            BindingModifierFlags::try_from(modifier_bits).expect("four modifier bits");
        for key in [Key::Char('p'), Key::Named(NamedKey::Enter)] {
            let chord = KeyChord::from_parts(modifier_flags, key);

            assert_eq!(
                build_key_input_for_chord(chord).to_binding_chord(),
                Some(chord),
                "the event built for {chord:?} projects back to it"
            );
        }
    }
}

#[test]
fn a_built_key_input_reports_the_modifiers_the_chord_names() {
    let chord = KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('p'));

    assert_eq!(
        build_key_input_for_chord(chord).modifier_flags,
        KeyModifierFlags::CTRL
    );
}

#[test]
fn runtime_directory_exists_until_its_handle_drops() {
    let runtime_directory_path = {
        let directory = build_test_runtime_directory();
        assert!(directory.path().is_dir());
        directory.path().to_owned()
    };

    assert!(!runtime_directory_path.exists());
}

#[test]
fn close_connection_after_peer_hangs_up_returns_once_the_peer_closed_its_end() {
    let runtime_directory = build_test_runtime_directory();
    let socket_address = compute_socket_address(runtime_directory.path(), SessionId::new());
    let listener = Listener::bind(&socket_address).expect("bind the stand-in socket");
    let has_peer_hung_up = Arc::new(AtomicBool::new(false));
    let peer_hang_up_flag = Arc::clone(&has_peer_hung_up);
    let peer_thread = std::thread::spawn(move || {
        let peer_connection = Connection::connect(&socket_address).expect("the peer connects");
        std::thread::sleep(Duration::from_millis(200));
        peer_hang_up_flag.store(true, Ordering::SeqCst);
        drop(peer_connection);
    });
    let mut stand_in_connection = listener.accept().expect("accept the peer");
    stand_in_connection
        .send(&"a frame the peer never reads")
        .expect("send to the peer");

    close_connection_after_peer_hangs_up(stand_in_connection);

    assert!(has_peer_hung_up.load(Ordering::SeqCst));
    peer_thread.join().expect("the peer thread ends");
}
