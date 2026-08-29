//! Tests for profile genesis: a `--profile` template opening its tabs and
//! panes, focusing the pane the profile marks, starting its first client
//! locked, and refusing a plugin pane.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{mpsc, Arc};
use std::time::SystemTime;

use koshi_config::layer::{PartialKoshiConfig, PartialLayoutDefaults};
use koshi_config::profile::parse_profile;
use koshi_core::event::{Event, InputMode, InputModeChanged};
use koshi_core::geometry::{Direction, Size, SplitDirection};
use koshi_core::ids::{ClientId, SessionId};
use koshi_core::lock::LockMode;
use koshi_layout::template::{ProfileTemplate, TemplateError};
use koshi_layout::tree::LayoutNode;
use koshi_pty::error::PtyError;
use koshi_session::session::lifecycle::SessionLifecycle;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{NullSnapshotProvider, NullStorage};
use crate::runtime::spawn_env::koshi_env;

use super::{ProfileLaunchError, Server};

/// A runtime backed by a fake PTY, with no session yet.
fn runtime() -> (Server, Arc<FakePtyBackend>) {
    let fake = Arc::new(FakePtyBackend::new());
    let (tx, rx) = mpsc::channel();
    let runtime = Server::new(
        fake.clone(),
        Arc::new(NullSnapshotProvider),
        Arc::new(NullStorage),
        rx,
        tx,
    );
    (runtime, fake)
}

/// Parse a profile from KDL text, panicking on error.
fn template(kdl: &str) -> ProfileTemplate {
    parse_profile(Path::new("profile/test.kdl"), kdl).expect("valid profile")
}

fn viewport() -> Size {
    Size { cols: 80, rows: 24 }
}

#[test]
fn a_profile_opens_its_tab_and_panes() {
    let (mut rt, _fake) = runtime();
    let tmpl = template("version 1\ntab {\n    horizontal {\n        pane\n        pane\n    }\n}");
    let client = ClientId::new();
    let () = rt
        .bootstrap_profile(
            SessionId::new(),
            tmpl,
            viewport(),
            SystemTime::UNIX_EPOCH,
            Some(client),
        )
        .expect("profile launches");

    assert_eq!(rt.sessions.len(), 1);
    let session = rt.sessions.values().next().expect("one session");
    assert_eq!(session.tabs.len(), 1);
    let tab = session.tabs.values().next().expect("one tab");
    assert_eq!(tab.layout().leaf_panes().len(), 2, "two panes in the tab");
    assert_eq!(rt.pty_handles.len(), 2, "both panes' PTYs are parked");
}

#[test]
fn a_profile_keeps_the_split_direction_it_declares() {
    // A profile states each split in the file — `vertical {}` here — so no
    // `layout.new-pane-direction` setting can turn it sideways. The session is
    // given a `koshi.kdl` naming the opposite direction to prove it.
    let (mut rt, _fake) = runtime();
    rt.load_startup_config(Some(PartialKoshiConfig {
        layout: Some(PartialLayoutDefaults {
            new_pane_direction: Some(Direction::Right),
        }),
        ..PartialKoshiConfig::default()
    }));

    let tmpl = template("version 1\ntab {\n    vertical {\n        pane\n        pane\n    }\n}");
    let client = ClientId::new();
    let () = rt
        .bootstrap_profile(
            SessionId::new(),
            tmpl,
            viewport(),
            SystemTime::UNIX_EPOCH,
            Some(client),
        )
        .expect("profile launches");

    let session = rt.sessions.values().next().expect("one session");
    let tab = session.tabs.values().next().expect("one tab");
    let LayoutNode::Split(split) = tab.layout() else {
        panic!("the tab's root is the profile's split");
    };
    assert_eq!(split.direction, SplitDirection::Vertical);
}

#[test]
fn a_profile_focuses_the_pane_it_marks() {
    let (mut rt, _fake) = runtime();
    // The second pane carries `focus`.
    let tmpl =
        template("version 1\ntab {\n    horizontal {\n        pane\n        pane {\n            focus\n        }\n    }\n}");
    let client = ClientId::new();
    let () = rt
        .bootstrap_profile(
            SessionId::new(),
            tmpl,
            viewport(),
            SystemTime::UNIX_EPOCH,
            Some(client),
        )
        .expect("profile launches");

    let session = rt.sessions.values().next().expect("one session");
    let (tab_id, tab) = session.tabs.iter().next().expect("one tab");
    let panes = tab.layout().leaf_panes();
    let focused = session
        .clients
        .get(client)
        .expect("client attached")
        .focused_pane(*tab_id);
    assert_eq!(
        focused,
        Some(panes[1]),
        "the marked (second) pane is focused"
    );
}

#[test]
fn a_multi_tab_profile_opens_every_tab() {
    let (mut rt, _fake) = runtime();
    let tmpl = template("version 1\ntab {\n    pane\n}\ntab {\n    pane\n}");
    let client = ClientId::new();
    let () = rt
        .bootstrap_profile(
            SessionId::new(),
            tmpl,
            viewport(),
            SystemTime::UNIX_EPOCH,
            Some(client),
        )
        .expect("profile launches");

    let session = rt.sessions.values().next().expect("one session");
    assert_eq!(session.tabs.len(), 2);
    assert_eq!(rt.pty_handles.len(), 2, "one PTY per tab's single pane");
}

#[test]
fn a_profile_with_a_plugin_pane_is_refused_and_commits_nothing() {
    let (mut rt, _fake) = runtime();
    let tmpl = template("version 1\ntab {\n    plugin \"sidebar\"\n}");
    let client = ClientId::new();
    let err = rt
        .bootstrap_profile(
            SessionId::new(),
            tmpl,
            viewport(),
            SystemTime::UNIX_EPOCH,
            Some(client),
        )
        .expect_err("a plugin pane has no host");

    assert!(matches!(err, ProfileLaunchError::PluginPane));
    // The plugin is caught before any spawn, so nothing is committed.
    assert!(rt.sessions.is_empty(), "no session committed");
    assert!(rt.pty_handles.is_empty(), "no PTY spawned");
}

#[test]
fn a_profile_sizes_its_focused_tab_panes_to_the_split() {
    // One pane fills the region; two side by side each get less than that, which
    // only holds if the focused tab was reflowed to its solved layout at genesis
    // (the panes spawn at the full-region placeholder size first).
    let (mut single, _fake) = runtime();
    single
        .bootstrap_profile(
            SessionId::new(),
            template("version 1\ntab {\n    pane\n}"),
            viewport(),
            SystemTime::UNIX_EPOCH,
            Some(ClientId::new()),
        )
        .expect("single-pane profile launches");
    let full = single.pty_sizes.values().next().expect("one pane").cols;

    let (mut split, _fake) = runtime();
    split
        .bootstrap_profile(
            SessionId::new(),
            template("version 1\ntab {\n    horizontal {\n        pane\n        pane\n    }\n}"),
            viewport(),
            SystemTime::UNIX_EPOCH,
            Some(ClientId::new()),
        )
        .expect("two-pane profile launches");
    let widths: Vec<u16> = split.pty_sizes.values().map(|size| size.cols).collect();
    assert_eq!(widths.len(), 2);
    assert!(
        widths.iter().all(|&w| w < full),
        "split panes {widths:?} should each be narrower than one full pane ({full})"
    );
}

#[test]
fn a_profile_pane_with_a_command_spawns_that_program() {
    // A `command` leaf takes the command arm of the pane-spec builder: the pane
    // spawns the named program rather than the default shell.
    let (mut rt, fake) = runtime();
    let tmpl = template("version 1\ntab {\n    pane {\n        command \"htop\"\n    }\n}");
    let client = ClientId::new();
    let () = rt
        .bootstrap_profile(
            SessionId::new(),
            tmpl,
            viewport(),
            SystemTime::UNIX_EPOCH,
            Some(client),
        )
        .expect("profile launches");

    let pane = fake.spawned_panes();
    assert_eq!(pane.len(), 1, "one pane spawned");
    let spec = fake.spawn_spec(pane[0]).expect("pane was spawned");
    assert_eq!(
        spec.program,
        Path::new("htop"),
        "the command's program is launched"
    );
    assert!(spec.args.is_empty(), "the command carried no arguments");
}

#[test]
fn a_profile_whose_pane_fails_to_spawn_is_refused_and_commits_nothing() {
    let (mut rt, fake) = runtime();
    fake.fail_spawns_with(PtyError::Spawn {
        detail: "no shell".to_string(),
    });
    let tmpl = template("version 1\ntab {\n    pane\n}");

    let client = ClientId::new();
    let err = rt
        .bootstrap_profile(
            SessionId::new(),
            tmpl,
            viewport(),
            SystemTime::UNIX_EPOCH,
            Some(client),
        )
        .expect_err("a failed spawn aborts the launch");

    let ProfileLaunchError::Spawn(inner) = err else {
        panic!("expected a Spawn error, got {err:?}");
    };
    assert_eq!(
        inner,
        PtyError::Spawn {
            detail: "no shell".to_string()
        }
    );
    // The failure happens before any commit, so nothing is left behind.
    assert!(rt.sessions.is_empty(), "no session committed");
    assert!(rt.pty_handles.is_empty(), "no PTY parked");
}

#[test]
fn profile_launch_error_display_names_each_cause() {
    assert_eq!(
        ProfileLaunchError::PluginPane.to_string(),
        "profile uses a plugin pane, which is not supported yet"
    );
    assert_eq!(
        ProfileLaunchError::Template(TemplateError::PaneCountMismatch {
            expected: 2,
            got: 1
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
    let (mut rt, fake) = runtime();
    let sid = SessionId::new();
    let client = rt
        .bootstrap_local(sid, viewport(), SystemTime::UNIX_EPOCH)
        .expect("bootstrap");

    // The root shell's spec carries the identity vars naming the session, the
    // genesis client, and the root pane.
    let session = rt.sessions.values().next().expect("one session");
    let tab = session.tabs.values().next().expect("one tab");
    let pane = tab.layout().leaf_panes()[0];
    let mut expected = rt.default_shell_spec(None, BTreeMap::new());
    expected.env.extend(koshi_env(
        sid,
        Some(client),
        pane,
        koshi_paths::runtime_dir().as_deref(),
    ));
    assert_eq!(fake.spawn_spec(pane).unwrap(), expected);
}

#[test]
fn bootstrap_local_named_uses_the_supplied_id_and_name() {
    let (mut rt, _fake) = runtime();
    let sid = SessionId::new();
    let _client = rt
        .bootstrap_local_named(
            sid,
            "S-example".to_string(),
            viewport(),
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");

    assert_eq!(rt.sessions.len(), 1);
    let session = rt.sessions.values().next().expect("one session");
    assert_eq!(session.id, sid);
    assert_eq!(session.name, "S-example");
}

#[test]
fn a_session_seeded_without_a_client_holds_none_and_still_reaches_running() {
    // The per-session server process seeds its session before anyone attaches,
    // so the session must run on its first tab alone.
    let (mut rt, _fake) = runtime();
    rt.bootstrap_session(
        SessionId::new(),
        "S-example".to_string(),
        viewport(),
        SystemTime::UNIX_EPOCH,
        None,
    )
    .expect("bootstrap");

    let session = rt.sessions.values().next().expect("one session");
    assert_eq!(session.clients.len(), 0, "no client is registered");
    assert_eq!(session.tabs.len(), 1, "the first tab is still seeded");
    assert_eq!(*session.lifecycle(), SessionLifecycle::Running);
}

#[test]
fn profile_panes_carry_the_in_session_identity_env() {
    let (mut rt, fake) = runtime();
    let tmpl = template("version 1\ntab {\n    horizontal {\n        pane\n        pane\n    }\n}");
    let sid = SessionId::new();
    let client = ClientId::new();
    let () = rt
        .bootstrap_profile(sid, tmpl, viewport(), SystemTime::UNIX_EPOCH, Some(client))
        .expect("profile launches");

    // Every pane's spec is the default shell plus the identity vars — the
    // same session and client for all, each pane's own id for itself.
    let session = rt.sessions.values().next().expect("one session");
    let tab = session.tabs.values().next().expect("one tab");
    for pane in tab.layout().leaf_panes() {
        let mut expected = rt.default_shell_spec(None, BTreeMap::new());
        expected.env.extend(koshi_env(
            sid,
            Some(client),
            pane,
            koshi_paths::runtime_dir().as_deref(),
        ));
        assert_eq!(fake.spawn_spec(pane).unwrap(), expected, "pane {pane}");
    }
}

#[test]
fn a_profile_records_focus_for_every_tab() {
    // Every tab — not just the starting one — records a focused pane on the
    // client, so keyboard input resolves after switching to a non-starting tab.
    let (mut rt, _fake) = runtime();
    let tmpl = template("version 1\ntab {\n    pane\n}\ntab {\n    pane\n}");
    let client = ClientId::new();
    let () = rt
        .bootstrap_profile(
            SessionId::new(),
            tmpl,
            viewport(),
            SystemTime::UNIX_EPOCH,
            Some(client),
        )
        .expect("profile launches");

    let session = rt.sessions.values().next().expect("one session");
    let client_ref = session.clients.get(client).expect("client attached");
    for tab_id in session.tabs.keys() {
        assert!(
            client_ref.focused_pane(*tab_id).is_some(),
            "tab {tab_id:?} has no focused pane recorded"
        );
    }
}

#[test]
fn a_profile_opens_on_the_tab_it_marks_focused() {
    let (mut rt, _fake) = runtime();
    let tmpl = template("version 1\ntab {\n    pane\n}\ntab {\n    focus\n    pane\n}");
    assert_eq!(tmpl.focused_tab, 1, "the second tab carries `focus`");
    let client = ClientId::new();
    let () = rt
        .bootstrap_profile(
            SessionId::new(),
            tmpl,
            viewport(),
            SystemTime::UNIX_EPOCH,
            Some(client),
        )
        .expect("profile launches");

    let session = rt.sessions.values().next().expect("one session");
    let second = session
        .tabs
        .values()
        .find(|tab| tab.index() == 1)
        .expect("a tab at bar position 1")
        .id();
    assert_eq!(
        session
            .clients
            .get(client)
            .expect("client attached")
            .active_tab(),
        second,
    );
}

#[test]
fn a_profile_focusing_a_tab_it_does_not_have_opens_on_its_last_tab() {
    let (mut rt, _fake) = runtime();
    let mut tmpl = template("version 1\ntab {\n    pane\n}\ntab {\n    pane\n}");
    tmpl.focused_tab = 5;
    let client = ClientId::new();
    let () = rt
        .bootstrap_profile(
            SessionId::new(),
            tmpl,
            viewport(),
            SystemTime::UNIX_EPOCH,
            Some(client),
        )
        .expect("profile launches");

    let session = rt.sessions.values().next().expect("one session");
    let last = session
        .tabs
        .values()
        .find(|tab| tab.index() == 1)
        .expect("a tab at bar position 1")
        .id();
    assert_eq!(
        session
            .clients
            .get(client)
            .expect("client attached")
            .active_tab(),
        last,
    );
}

#[test]
fn a_profile_with_the_lock_marker_starts_its_first_client_locked() {
    let (mut rt, _fake) = runtime();
    let tmpl = template("version 1\nlock\ntab { pane }");
    let client = ClientId::new();
    let () = rt
        .bootstrap_profile(
            SessionId::new(),
            tmpl,
            viewport(),
            SystemTime::UNIX_EPOCH,
            Some(client),
        )
        .expect("profile launches");

    let session = rt.sessions.values().next().expect("one session");
    assert_eq!(
        session
            .clients
            .get(client)
            .expect("the client attached")
            .lock_mode(),
        LockMode::Locked
    );
    assert!(
        !session.start_locked,
        "the first client spent the profile's starting lock"
    );
}

#[test]
fn a_profile_without_the_lock_marker_starts_its_first_client_unlocked() {
    let (mut rt, _fake) = runtime();
    let tmpl = template("version 1\ntab { pane }");
    let client = ClientId::new();
    let () = rt
        .bootstrap_profile(
            SessionId::new(),
            tmpl,
            viewport(),
            SystemTime::UNIX_EPOCH,
            Some(client),
        )
        .expect("profile launches");

    let session = rt.sessions.values().next().expect("one session");
    assert_eq!(
        session
            .clients
            .get(client)
            .expect("the client attached")
            .lock_mode(),
        LockMode::Normal
    );
}

#[test]
fn the_lock_marker_reaches_only_the_first_client_to_attach() {
    let (mut rt, _fake) = runtime();
    let tmpl = template("version 1\nlock\ntab { pane }");
    let session_id = SessionId::new();
    // The shape `koshi --profile` takes: the session server seeds the session
    // with no client, and every client arrives later over the control socket.
    let () = rt
        .bootstrap_profile(session_id, tmpl, viewport(), SystemTime::UNIX_EPOCH, None)
        .expect("profile launches");
    let tab_id = *rt
        .sessions
        .get(&session_id)
        .expect("the seeded session")
        .tabs
        .keys()
        .next()
        .expect("one tab");

    let first = ClientId::new();
    let events = rt.handle_client_attach(
        session_id,
        first,
        viewport(),
        None,
        tab_id,
        SystemTime::UNIX_EPOCH,
        false,
    );
    assert_eq!(
        rt.sessions
            .get(&session_id)
            .expect("the seeded session")
            .clients
            .get(first)
            .expect("the first client attached")
            .lock_mode(),
        LockMode::Locked
    );
    assert_eq!(
        mode_changes(&events),
        vec![InputModeChanged {
            client_id: first,
            mode: InputMode::Locked,
        }]
    );

    let second = ClientId::new();
    let events = rt.handle_client_attach(
        session_id,
        second,
        viewport(),
        None,
        tab_id,
        SystemTime::UNIX_EPOCH,
        false,
    );
    assert_eq!(
        rt.sessions
            .get(&session_id)
            .expect("the seeded session")
            .clients
            .get(second)
            .expect("the second client attached")
            .lock_mode(),
        LockMode::Normal
    );
    assert_eq!(mode_changes(&events), vec![]);
}

#[test]
fn a_locked_client_reattaching_keeps_its_mode_and_takes_no_second_lock() {
    let (mut rt, _fake) = runtime();
    let tmpl = template("version 1\nlock\ntab { pane }");
    let session_id = SessionId::new();
    let () = rt
        .bootstrap_profile(session_id, tmpl, viewport(), SystemTime::UNIX_EPOCH, None)
        .expect("profile launches");
    let tab_id = *rt
        .sessions
        .get(&session_id)
        .expect("the seeded session")
        .tabs
        .keys()
        .next()
        .expect("one tab");

    let client = ClientId::new();
    let _first = rt.handle_client_attach(
        session_id,
        client,
        viewport(),
        None,
        tab_id,
        SystemTime::UNIX_EPOCH,
        false,
    );
    // The same id arriving again is a re-attach: it updates the view in place
    // and leaves the mode alone.
    let events = rt.handle_client_attach(
        session_id,
        client,
        viewport(),
        None,
        tab_id,
        SystemTime::UNIX_EPOCH,
        false,
    );

    assert_eq!(
        rt.sessions
            .get(&session_id)
            .expect("the seeded session")
            .clients
            .get(client)
            .expect("the client is still attached")
            .lock_mode(),
        LockMode::Locked
    );
    assert_eq!(mode_changes(&events), vec![]);
}

/// Every [`Event::InputModeChanged`] in `events`, in order.
fn mode_changes(events: &[Event]) -> Vec<InputModeChanged> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::InputModeChanged(changed) => Some(*changed),
            _ => None,
        })
        .collect()
}
