//! Tests for genesis: the first session seeded with one shell pane under a
//! caller-chosen id, and a `--profile` template opening its tabs and panes,
//! focusing the pane the profile marks, starting its first client locked, and
//! refusing a plugin pane.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::time::SystemTime;

use koshi_config::layer::{PartialKoshiConfig, PartialLayoutDefaults};
use koshi_config::profile::parse_profile;
use koshi_core::event::{Event, InputModeChanged};
use koshi_core::geometry::{Direction, Size, SplitDirection};
use koshi_core::ids::{ClientId, SessionId};
use koshi_core::lock::LockMode;
use koshi_core::process::PtySize;
use koshi_layout::template::{ProfileTemplate, TemplateError};
use koshi_layout::tree::LayoutNode;
use koshi_pty::error::PtyError;
use koshi_session::client::ClientOrigin;
use koshi_session::session::lifecycle::SessionLifecycle;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::runtime::spawn_env::build_koshi_environment;

use super::{ProfileLaunchError, Server};

/// A runtime backed by a fake PTY, with no session yet.
fn build_test_runtime() -> (Server, Arc<FakePtyBackend>) {
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (event_sender, event_receiver) = mpsc::channel();
    let server = Server::from_runtime_parts(fake_pty_backend.clone(), event_receiver, event_sender);
    (server, fake_pty_backend)
}

/// Parse a profile from KDL text, panicking on error.
fn parse_test_profile_template(kdl: &str) -> ProfileTemplate {
    parse_profile(Path::new("profile/test.kdl"), kdl).expect("valid profile")
}

fn build_test_viewport_size() -> Size {
    Size {
        column_count: 80,
        row_count: 24,
    }
}

#[test]
fn a_profile_opens_its_tab_and_panes() {
    let (mut server, _fake_pty_backend) = build_test_runtime();
    let profile_template = parse_test_profile_template(
        "version 1\ntab {\n    horizontal {\n        pane\n        pane\n    }\n}",
    );
    let client_id = ClientId::new();
    let () = server
        .bootstrap_profile(
            SessionId::new(),
            profile_template,
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            Some(client_id),
        )
        .expect("profile launches");

    assert_eq!(server.session_by_id.len(), 1);
    let session = server.session_by_id.values().next().expect("one session");
    assert_eq!(session.tabs.len(), 1);
    let tab = session.tabs.values().next().expect("one tab");
    assert_eq!(
        tab.get_layout_tree().list_leaf_pane_ids().len(),
        2,
        "two panes in the tab"
    );
    assert_eq!(
        server.pty_handle_by_pane_id.len(),
        2,
        "both panes' PTYs are parked"
    );
}

#[test]
fn a_profile_keeps_the_split_direction_it_declares() {
    // A profile states each split in the file — `vertical {}` here — so no
    // `layout.new-pane-direction` setting can turn it sideways. The session is
    // given a `koshi.kdl` naming the opposite direction to prove it.
    let (mut server, _fake_pty_backend) = build_test_runtime();
    server.load_startup_config(Some(PartialKoshiConfig {
        layout: Some(PartialLayoutDefaults {
            new_pane_direction: Some(Direction::Right),
        }),
        ..PartialKoshiConfig::default()
    }));

    let profile_template = parse_test_profile_template(
        "version 1\ntab {\n    vertical {\n        pane\n        pane\n    }\n}",
    );
    let client_id = ClientId::new();
    let () = server
        .bootstrap_profile(
            SessionId::new(),
            profile_template,
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            Some(client_id),
        )
        .expect("profile launches");

    let session = server.session_by_id.values().next().expect("one session");
    let tab = session.tabs.values().next().expect("one tab");
    let LayoutNode::Split(split) = tab.get_layout_tree() else {
        panic!("the tab's root is the profile's split");
    };
    assert_eq!(split.direction, SplitDirection::Vertical);
}

#[test]
fn a_profile_focuses_the_pane_it_marks() {
    let (mut server, _fake_pty_backend) = build_test_runtime();
    // The second pane carries `focus`.
    let profile_template =
        parse_test_profile_template("version 1\ntab {\n    horizontal {\n        pane\n        pane {\n            focus\n        }\n    }\n}");
    let client_id = ClientId::new();
    let () = server
        .bootstrap_profile(
            SessionId::new(),
            profile_template,
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            Some(client_id),
        )
        .expect("profile launches");

    let session = server.session_by_id.values().next().expect("one session");
    let (tab_id, tab) = session.tabs.iter().next().expect("one tab");
    let pane_ids = tab.get_layout_tree().list_leaf_pane_ids();
    let focused_pane_id = session
        .clients
        .get_client_by_id(client_id)
        .expect("client attached")
        .get_focused_pane(*tab_id);
    assert_eq!(
        focused_pane_id,
        Some(pane_ids[1]),
        "the marked (second) pane is focused"
    );
}

#[test]
fn a_multi_tab_profile_opens_every_tab() {
    let (mut server, _fake_pty_backend) = build_test_runtime();
    let profile_template =
        parse_test_profile_template("version 1\ntab {\n    pane\n}\ntab {\n    pane\n}");
    let client_id = ClientId::new();
    let () = server
        .bootstrap_profile(
            SessionId::new(),
            profile_template,
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            Some(client_id),
        )
        .expect("profile launches");

    let session = server.session_by_id.values().next().expect("one session");
    assert_eq!(session.tabs.len(), 2);
    assert_eq!(
        server.pty_handle_by_pane_id.len(),
        2,
        "one PTY per tab's single pane"
    );
}

#[test]
fn a_profile_with_a_plugin_pane_is_refused_and_commits_nothing() {
    let (mut server, _fake_pty_backend) = build_test_runtime();
    let profile_template =
        parse_test_profile_template("version 1\ntab {\n    plugin \"sidebar\"\n}");
    let client_id = ClientId::new();
    let launch_error = server
        .bootstrap_profile(
            SessionId::new(),
            profile_template,
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            Some(client_id),
        )
        .expect_err("a plugin pane has no host");

    assert!(matches!(launch_error, ProfileLaunchError::PluginPane));
    // The plugin is caught before any spawn, so nothing is committed.
    assert!(server.session_by_id.is_empty(), "no session committed");
    assert!(server.pty_handle_by_pane_id.is_empty(), "no PTY spawned");
}

#[test]
fn a_profile_sizes_its_focused_tab_panes_to_the_split() {
    // One pane fills the pane region of an 80x24 viewport — 80x22 once the two
    // chrome rows are removed, 78x20 of content inside its border. Two side by
    // side take 40 columns each, 38 of them content.
    let (mut single_server, _single_fake_pty_backend) = build_test_runtime();
    single_server
        .bootstrap_profile(
            SessionId::new(),
            parse_test_profile_template("version 1\ntab {\n    pane\n}"),
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            Some(ClientId::new()),
        )
        .expect("single-pane profile launches");
    let single_pane_pty_size = *single_server
        .pty_size_by_pane_id
        .values()
        .next()
        .expect("one pane");
    assert_eq!(
        single_pane_pty_size,
        PtySize {
            column_count: 78,
            row_count: 20
        }
    );

    let (mut split_server, _split_fake_pty_backend) = build_test_runtime();
    split_server
        .bootstrap_profile(
            SessionId::new(),
            parse_test_profile_template(
                "version 1\ntab {\n    horizontal {\n        pane\n        pane\n    }\n}",
            ),
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            Some(ClientId::new()),
        )
        .expect("two-pane profile launches");
    let session = split_server
        .session_by_id
        .values()
        .next()
        .expect("one session");
    let tab = session.tabs.values().next().expect("one tab");
    let pane_pty_sizes: Vec<PtySize> = tab
        .get_layout_tree()
        .list_leaf_pane_ids()
        .iter()
        .map(|pane_id| split_server.pty_size_by_pane_id[pane_id])
        .collect();
    assert_eq!(
        pane_pty_sizes,
        vec![
            PtySize {
                column_count: 38,
                row_count: 20
            },
            PtySize {
                column_count: 38,
                row_count: 20
            }
        ]
    );
}

#[test]
fn a_profile_pane_with_a_command_spawns_that_program() {
    // A `command` leaf takes the command arm of the pane-spec builder: the pane
    // spawns the named program rather than the default shell.
    let (mut server, fake_pty_backend) = build_test_runtime();
    let profile_template = parse_test_profile_template(
        "version 1\ntab {\n    pane {\n        command \"htop\"\n    }\n}",
    );
    let client_id = ClientId::new();
    let () = server
        .bootstrap_profile(
            SessionId::new(),
            profile_template,
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            Some(client_id),
        )
        .expect("profile launches");

    let spawned_pane_ids = fake_pty_backend.list_spawned_pane_ids();
    assert_eq!(spawned_pane_ids.len(), 1, "one pane spawned");
    let spawn_spec = fake_pty_backend
        .get_spawn_spec(spawned_pane_ids[0])
        .expect("pane was spawned");
    assert_eq!(
        spawn_spec.program,
        Path::new("htop"),
        "the command's program is launched"
    );
    assert!(
        spawn_spec.arguments.is_empty(),
        "the command carried no arguments"
    );
}

#[test]
fn a_profile_whose_pane_fails_to_spawn_is_refused_and_commits_nothing() {
    let (mut server, fake_pty_backend) = build_test_runtime();
    fake_pty_backend.fail_spawns_with(PtyError::Spawn {
        detail: "no shell".to_string(),
    });
    let profile_template = parse_test_profile_template("version 1\ntab {\n    pane\n}");

    let client_id = ClientId::new();
    let spawn_error = server
        .bootstrap_profile(
            SessionId::new(),
            profile_template,
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            Some(client_id),
        )
        .expect_err("a failed spawn aborts the launch");

    let ProfileLaunchError::Spawn(spawn_error) = spawn_error else {
        panic!("expected a Spawn error");
    };
    assert_eq!(
        spawn_error,
        PtyError::Spawn {
            detail: "no shell".to_string()
        }
    );
    // The failure happens before any commit, so nothing is left behind.
    assert!(server.session_by_id.is_empty(), "no session committed");
    assert!(server.pty_handle_by_pane_id.is_empty(), "no PTY parked");
}

#[test]
fn profile_launch_error_display_names_each_cause() {
    assert_eq!(
        ProfileLaunchError::PluginPane.to_string(),
        "profile uses a plugin pane, which is not supported yet"
    );
    assert_eq!(
        ProfileLaunchError::Template(TemplateError::PaneCountMismatch {
            expected_leaf_count: 2,
            provided_pane_id_count: 1
        })
        .to_string(),
        "profile layout could not be built: template has 2 pane slots but 1 pane ids were supplied"
    );
    assert_eq!(
        ProfileLaunchError::Spawn(PtyError::Spawn {
            detail: "boom".to_string()
        })
        .to_string(),
        "a profile pane failed to start: failed to spawn pty: boom"
    );
}

#[test]
fn bootstrap_local_injects_the_in_session_identity_env() {
    let (mut server, fake_pty_backend) = build_test_runtime();
    let session_id = SessionId::new();
    let client_id = server
        .bootstrap_local(
            session_id,
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");

    // The root shell's spec carries the identity vars naming the session, the
    // genesis client, and the root pane.
    let session = server.session_by_id.values().next().expect("one session");
    let tab = session.tabs.values().next().expect("one tab");
    let pane_id = tab.get_layout_tree().list_leaf_pane_ids()[0];
    let mut expected_shell_spec = server.build_default_shell_spec(None, BTreeMap::new());
    expected_shell_spec
        .environment_variables
        .extend(build_koshi_environment(
            session_id,
            Some(client_id),
            pane_id,
            koshi_paths::resolve_runtime_directory().as_deref(),
        ));
    assert_eq!(
        fake_pty_backend.get_spawn_spec(pane_id).unwrap(),
        expected_shell_spec
    );
}

#[test]
fn bootstrap_local_named_uses_the_supplied_id_and_name() {
    let (mut server, _fake_pty_backend) = build_test_runtime();
    let session_id = SessionId::new();
    let _client_id = server
        .bootstrap_local_named(
            session_id,
            "S-example".to_string(),
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");

    assert_eq!(server.session_by_id.len(), 1);
    let session = server.session_by_id.values().next().expect("one session");
    assert_eq!(session.session_id, session_id);
    assert_eq!(session.session_name, "S-example");
}

#[test]
fn a_session_seeded_without_a_client_holds_none_and_still_reaches_running() {
    // The per-session server process seeds its session before anyone attaches,
    // so the session must run on its first tab alone.
    let (mut server, _fake_pty_backend) = build_test_runtime();
    server
        .bootstrap_session(
            SessionId::new(),
            "S-example".to_string(),
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            None,
        )
        .expect("bootstrap");

    let session = server.session_by_id.values().next().expect("one session");
    assert_eq!(session.clients.client_count(), 0, "no client is registered");
    assert_eq!(session.tabs.len(), 1, "the first tab is still seeded");
    assert_eq!(*session.get_lifecycle(), SessionLifecycle::Running);
}

#[test]
fn profile_panes_carry_the_in_session_identity_env() {
    let (mut server, fake_pty_backend) = build_test_runtime();
    let profile_template = parse_test_profile_template(
        "version 1\ntab {\n    horizontal {\n        pane\n        pane\n    }\n}",
    );
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let () = server
        .bootstrap_profile(
            session_id,
            profile_template,
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            Some(client_id),
        )
        .expect("profile launches");

    // Every pane's spec is the default shell plus the identity vars — the
    // same session and client for all, each pane's own id for itself.
    let session = server.session_by_id.values().next().expect("one session");
    let tab = session.tabs.values().next().expect("one tab");
    for pane_id in tab.get_layout_tree().list_leaf_pane_ids() {
        let mut expected_pane_shell_spec = server.build_default_shell_spec(None, BTreeMap::new());
        expected_pane_shell_spec
            .environment_variables
            .extend(build_koshi_environment(
                session_id,
                Some(client_id),
                pane_id,
                koshi_paths::resolve_runtime_directory().as_deref(),
            ));
        assert_eq!(
            fake_pty_backend.get_spawn_spec(pane_id).unwrap(),
            expected_pane_shell_spec,
            "pane {pane_id}"
        );
    }
}

#[test]
fn a_profile_records_focus_for_every_tab() {
    // Every tab — not just the starting one — records a focused pane on the
    // client, so keyboard input resolves after switching to a non-starting tab.
    let (mut server, _fake_pty_backend) = build_test_runtime();
    let profile_template =
        parse_test_profile_template("version 1\ntab {\n    pane\n}\ntab {\n    pane\n}");
    let client_id = ClientId::new();
    let () = server
        .bootstrap_profile(
            SessionId::new(),
            profile_template,
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            Some(client_id),
        )
        .expect("profile launches");

    let session = server.session_by_id.values().next().expect("one session");
    let attached_client = session
        .clients
        .get_client_by_id(client_id)
        .expect("client attached");
    for (tab_id, tab) in &session.tabs {
        let pane_ids = tab.get_layout_tree().list_leaf_pane_ids();
        assert_eq!(pane_ids.len(), 1, "tab {tab_id:?} holds one pane");
        assert_eq!(
            attached_client.get_focused_pane(*tab_id),
            Some(pane_ids[0]),
            "tab {tab_id:?} focuses its own pane"
        );
    }
}

#[test]
fn a_profile_opens_on_the_tab_it_marks_focused() {
    let (mut server, _fake_pty_backend) = build_test_runtime();
    let profile_template =
        parse_test_profile_template("version 1\ntab {\n    pane\n}\ntab {\n    focus\n    pane\n}");
    assert_eq!(
        profile_template.focused_tab_index, 1,
        "the second tab carries `focus`"
    );
    let client_id = ClientId::new();
    let () = server
        .bootstrap_profile(
            SessionId::new(),
            profile_template,
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            Some(client_id),
        )
        .expect("profile launches");

    let session = server.session_by_id.values().next().expect("one session");
    let focused_tab_id = session
        .tabs
        .values()
        .find(|tab| tab.get_tab_index() == 1)
        .expect("a tab at bar position 1")
        .get_tab_id();
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client attached")
            .get_active_tab(),
        focused_tab_id,
    );
}

#[test]
fn a_profile_focusing_a_tab_it_does_not_have_opens_on_its_last_tab() {
    let (mut server, _fake_pty_backend) = build_test_runtime();
    let mut profile_template =
        parse_test_profile_template("version 1\ntab {\n    pane\n}\ntab {\n    pane\n}");
    profile_template.focused_tab_index = 5;
    let client_id = ClientId::new();
    let () = server
        .bootstrap_profile(
            SessionId::new(),
            profile_template,
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            Some(client_id),
        )
        .expect("profile launches");

    let session = server.session_by_id.values().next().expect("one session");
    let last_tab_id = session
        .tabs
        .values()
        .find(|tab| tab.get_tab_index() == 1)
        .expect("a tab at bar position 1")
        .get_tab_id();
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("client attached")
            .get_active_tab(),
        last_tab_id,
    );
}

#[test]
fn a_profile_with_the_lock_marker_starts_its_first_client_locked() {
    let (mut server, _fake_pty_backend) = build_test_runtime();
    let profile_template = parse_test_profile_template("version 1\nlock\ntab { pane }");
    let client_id = ClientId::new();
    let () = server
        .bootstrap_profile(
            SessionId::new(),
            profile_template,
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            Some(client_id),
        )
        .expect("profile launches");

    let session = server.session_by_id.values().next().expect("one session");
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("the client attached")
            .get_lock_mode(),
        LockMode::Locked
    );
    assert!(
        !session.start_locked,
        "the first client spent the profile's starting lock"
    );
}

#[test]
fn a_profile_without_the_lock_marker_starts_its_first_client_unlocked() {
    let (mut server, _fake_pty_backend) = build_test_runtime();
    let profile_template = parse_test_profile_template("version 1\ntab { pane }");
    let client_id = ClientId::new();
    let () = server
        .bootstrap_profile(
            SessionId::new(),
            profile_template,
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            Some(client_id),
        )
        .expect("profile launches");

    let session = server.session_by_id.values().next().expect("one session");
    assert_eq!(
        session
            .clients
            .get_client_by_id(client_id)
            .expect("the client attached")
            .get_lock_mode(),
        LockMode::Normal
    );
}

#[test]
fn the_lock_marker_reaches_only_the_first_client_to_attach() {
    let (mut server, _fake_pty_backend) = build_test_runtime();
    let profile_template = parse_test_profile_template("version 1\nlock\ntab { pane }");
    let session_id = SessionId::new();
    // The shape `koshi --profile` takes: the session server seeds the session
    // with no client, and every client arrives afterward over the control socket.
    let () = server
        .bootstrap_profile(
            session_id,
            profile_template,
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            None,
        )
        .expect("profile launches");
    let tab_id = *server
        .session_by_id
        .get(&session_id)
        .expect("the seeded session")
        .tabs
        .keys()
        .next()
        .expect("one tab");

    let first_client_id = ClientId::new();
    let emitted_events = server.handle_client_attach(
        session_id,
        first_client_id,
        build_test_viewport_size(),
        None,
        tab_id,
        SystemTime::UNIX_EPOCH,
        false,
    );
    assert_eq!(
        server
            .session_by_id
            .get(&session_id)
            .expect("the seeded session")
            .clients
            .get_client_by_id(first_client_id)
            .expect("the first client attached")
            .get_lock_mode(),
        LockMode::Locked
    );
    assert_eq!(
        mode_changes(&emitted_events),
        vec![InputModeChanged {
            client_id: first_client_id,
            lock_mode: LockMode::Locked,
        }]
    );

    let second_client_id = ClientId::new();
    let emitted_events = server.handle_client_attach(
        session_id,
        second_client_id,
        build_test_viewport_size(),
        None,
        tab_id,
        SystemTime::UNIX_EPOCH,
        false,
    );
    assert_eq!(
        server
            .session_by_id
            .get(&session_id)
            .expect("the seeded session")
            .clients
            .get_client_by_id(second_client_id)
            .expect("the second client attached")
            .get_lock_mode(),
        LockMode::Normal
    );
    assert_eq!(mode_changes(&emitted_events), vec![]);
}

#[test]
fn a_locked_client_reattaching_keeps_its_mode_and_takes_no_second_lock() {
    let (mut server, _fake_pty_backend) = build_test_runtime();
    let profile_template = parse_test_profile_template("version 1\nlock\ntab { pane }");
    let session_id = SessionId::new();
    let () = server
        .bootstrap_profile(
            session_id,
            profile_template,
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            None,
        )
        .expect("profile launches");
    let tab_id = *server
        .session_by_id
        .get(&session_id)
        .expect("the seeded session")
        .tabs
        .keys()
        .next()
        .expect("one tab");

    let client_id = ClientId::new();
    let _first_attach_events = server.handle_client_attach(
        session_id,
        client_id,
        build_test_viewport_size(),
        None,
        tab_id,
        SystemTime::UNIX_EPOCH,
        false,
    );
    // The same id arriving again is a re-attach: it updates the view in place
    // and leaves the mode alone.
    let emitted_events = server.handle_client_attach(
        session_id,
        client_id,
        build_test_viewport_size(),
        None,
        tab_id,
        SystemTime::UNIX_EPOCH,
        false,
    );

    assert_eq!(
        server
            .session_by_id
            .get(&session_id)
            .expect("the seeded session")
            .clients
            .get_client_by_id(client_id)
            .expect("the client is still attached")
            .get_lock_mode(),
        LockMode::Locked
    );
    assert_eq!(mode_changes(&emitted_events), vec![]);
}

#[test]
fn bootstrap_local_attaches_its_client_to_the_seeded_tab_and_root_pane() {
    let (mut server, _fake_pty_backend) = build_test_runtime();
    let client_id = server
        .bootstrap_local(
            SessionId::new(),
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");

    let session = server.session_by_id.values().next().expect("one session");
    let (tab_id, tab) = session.tabs.iter().next().expect("one tab");
    let root_pane_id = tab.get_layout_tree().list_leaf_pane_ids()[0];
    assert_eq!(
        session.clients.client_count(),
        1,
        "the genesis client alone"
    );
    let attached_client = session
        .clients
        .get_client_by_id(client_id)
        .expect("the genesis client");
    assert_eq!(attached_client.get_active_tab(), *tab_id);
    assert_eq!(
        attached_client.get_focused_pane(*tab_id),
        Some(root_pane_id)
    );
    assert_eq!(attached_client.get_origin(), ClientOrigin::Local);
    assert_eq!(attached_client.get_color(), 0);
    assert_eq!(attached_client.get_lock_mode(), LockMode::Normal);
    assert_eq!(
        attached_client.get_viewport_size(),
        build_test_viewport_size()
    );
}

#[test]
fn bootstrap_local_sizes_the_root_pane_to_the_pane_region() {
    // The chrome takes one row above and one below the pane region, and the
    // pane's border one cell on each side, so an 80x24 viewport gives the root
    // pane 78x20 of content. Genesis resizes it no further.
    let (mut server, fake_pty_backend) = build_test_runtime();
    let _client_id = server
        .bootstrap_local(
            SessionId::new(),
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");

    let session = server.session_by_id.values().next().expect("one session");
    let tab = session.tabs.values().next().expect("one tab");
    let root_pane_id = tab.get_layout_tree().list_leaf_pane_ids()[0];
    assert_eq!(
        server.pty_size_by_pane_id[&root_pane_id],
        PtySize {
            column_count: 78,
            row_count: 20
        }
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(root_pane_id)
            .expect("the root pane was spawned"),
        vec![PtySize {
            column_count: 78,
            row_count: 20
        }]
    );
}

#[test]
fn a_one_row_viewport_seeds_the_root_pane_at_the_minimum_row_count() {
    // The pane region of a one-row viewport saturates to zero rows, and the PTY
    // size floors each axis at 2x1.
    let (mut server, _fake_pty_backend) = build_test_runtime();
    let _client_id = server
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 1,
            },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");

    let root_pane_pty_size = *server
        .pty_size_by_pane_id
        .values()
        .next()
        .expect("one pane");
    assert_eq!(
        root_pane_pty_size,
        PtySize {
            column_count: 80,
            row_count: 1
        }
    );
}

#[test]
fn a_genesis_shell_that_fails_to_spawn_commits_nothing_and_surfaces_the_error() {
    let (mut server, fake_pty_backend) = build_test_runtime();
    fake_pty_backend.fail_spawns_with(PtyError::Spawn {
        detail: "no shell".to_string(),
    });

    let spawn_error = server
        .bootstrap_local(
            SessionId::new(),
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
        )
        .expect_err("a failed spawn aborts genesis");

    assert_eq!(
        spawn_error,
        PtyError::Spawn {
            detail: "no shell".to_string()
        }
    );
    assert!(server.session_by_id.is_empty(), "no session committed");
    assert!(server.pty_handle_by_pane_id.is_empty(), "no PTY parked");
    assert!(
        server.pty_size_by_pane_id.is_empty(),
        "no PTY size recorded"
    );
}

#[test]
fn a_profile_pane_cannot_replace_the_identity_env_it_is_given() {
    let (mut server, fake_pty_backend) = build_test_runtime();
    let profile_template = parse_test_profile_template(
        "version 1\ntab {\n    pane {\n        env \"KOSHI\" \"0\"\n        \
         env \"KOSHI_SESSION_ID\" \"session-someone-elses\"\n    }\n}",
    );
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let () = server
        .bootstrap_profile(
            session_id,
            profile_template,
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            Some(client_id),
        )
        .expect("profile launches");

    // The identity overlay is merged last, so koshi's own values win over the
    // ones the profile file names.
    let session = server.session_by_id.values().next().expect("one session");
    let tab = session.tabs.values().next().expect("one tab");
    let pane_id = tab.get_layout_tree().list_leaf_pane_ids()[0];
    let spawn_spec = fake_pty_backend
        .get_spawn_spec(pane_id)
        .expect("pane was spawned");
    assert_eq!(
        spawn_spec.environment_variables.get("KOSHI"),
        Some(&"1".to_string())
    );
    assert_eq!(
        spawn_spec.environment_variables.get("KOSHI_SESSION_ID"),
        Some(&session_id.to_string())
    );
    assert_eq!(
        spawn_spec.environment_variables.get("KOSHI_CLIENT_ID"),
        Some(&client_id.to_string())
    );
    assert_eq!(
        spawn_spec.environment_variables.get("KOSHI_PANE_ID"),
        Some(&pane_id.to_string())
    );
}

#[test]
fn a_profile_pane_keeps_its_own_terminal_identity_over_the_configured_one() {
    let (mut server, fake_pty_backend) = build_test_runtime();
    server.config.terminal.term = "xterm-kitty".to_string();
    server.config.terminal.colorterm = "24bit".to_string();
    let profile_template = parse_test_profile_template(
        "version 1\ntab {\n    pane {\n        env \"TERM\" \"screen-256color\"\n    }\n}",
    );
    let () = server
        .bootstrap_profile(
            SessionId::new(),
            profile_template,
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            Some(ClientId::new()),
        )
        .expect("profile launches");

    let session = server.session_by_id.values().next().expect("one session");
    let tab = session.tabs.values().next().expect("one tab");
    let pane_id = tab.get_layout_tree().list_leaf_pane_ids()[0];
    let spawn_spec = fake_pty_backend
        .get_spawn_spec(pane_id)
        .expect("pane was spawned");
    assert_eq!(
        spawn_spec.environment_variables.get("TERM"),
        Some(&"screen-256color".to_string())
    );
    assert_eq!(
        spawn_spec.environment_variables.get("COLORTERM"),
        Some(&"24bit".to_string())
    );
}

#[test]
fn a_profile_command_pane_records_the_command_and_cwd_it_declares() {
    let (mut server, _fake_pty_backend) = build_test_runtime();
    let profile_template = parse_test_profile_template(
        "version 1\ntab {\n    pane {\n        command \"htop\" \"-d\" \"5\"\n        \
         cwd \"/tmp\"\n    }\n}",
    );
    let () = server
        .bootstrap_profile(
            SessionId::new(),
            profile_template,
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            Some(ClientId::new()),
        )
        .expect("profile launches");

    let session = server.session_by_id.values().next().expect("one session");
    let tab = session.tabs.values().next().expect("one tab");
    let pane_id = tab.get_layout_tree().list_leaf_pane_ids()[0];
    let pane_record = session
        .panes
        .get_pane_record_by_id(pane_id)
        .expect("the pane record");
    assert_eq!(pane_record.working_directory, Some(PathBuf::from("/tmp")));
    let spawn_spec = pane_record
        .spawn_spec
        .as_ref()
        .expect("the command is recorded");
    assert_eq!(spawn_spec.program, PathBuf::from("htop"));
    assert_eq!(
        spawn_spec.arguments,
        vec!["-d".to_string(), "5".to_string()]
    );
    assert_eq!(spawn_spec.working_directory, Some(PathBuf::from("/tmp")));
}

#[test]
fn a_profile_default_shell_pane_records_no_command() {
    let (mut server, _fake_pty_backend) = build_test_runtime();
    let profile_template =
        parse_test_profile_template("version 1\ntab {\n    pane {\n        cwd \"/tmp\"\n    }\n}");
    let () = server
        .bootstrap_profile(
            SessionId::new(),
            profile_template,
            build_test_viewport_size(),
            SystemTime::UNIX_EPOCH,
            Some(ClientId::new()),
        )
        .expect("profile launches");

    let session = server.session_by_id.values().next().expect("one session");
    let tab = session.tabs.values().next().expect("one tab");
    let pane_id = tab.get_layout_tree().list_leaf_pane_ids()[0];
    let pane_record = session
        .panes
        .get_pane_record_by_id(pane_id)
        .expect("the pane record");
    assert_eq!(pane_record.working_directory, Some(PathBuf::from("/tmp")));
    assert_eq!(
        pane_record.spawn_spec, None,
        "the default shell is not a command"
    );
}

/// Every [`Event::InputModeChanged`] in `emitted_events`, in order.
fn mode_changes(emitted_events: &[Event]) -> Vec<InputModeChanged> {
    emitted_events
        .iter()
        .filter_map(|runtime_event| match runtime_event {
            Event::InputModeChanged(input_mode_change) => Some(*input_mode_change),
            _ => None,
        })
        .collect()
}
