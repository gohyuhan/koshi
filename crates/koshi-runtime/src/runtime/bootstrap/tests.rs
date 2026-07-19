//! Tests for profile genesis: a `--profile` template opening its tabs and
//! panes, focusing the pane the profile marks, and refusing a plugin pane.

use std::path::Path;
use std::sync::{mpsc, Arc};
use std::time::SystemTime;

use koshi_config::profile::parse_profile;
use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::SessionId;
use koshi_layout::template::ProfileTemplate;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{NullSnapshotProvider, NullStorage};

use super::{ProfileLaunchError, Runtime};

/// A runtime backed by a fake PTY, with no session yet.
fn runtime() -> (Runtime, Arc<FakePtyBackend>) {
    let fake = Arc::new(FakePtyBackend::new());
    let (tx, rx) = mpsc::channel();
    let runtime = Runtime::new(
        fake.clone(),
        Arc::new(NullSnapshotProvider),
        Arc::new(NullStorage),
        rx,
        tx,
        TerminalCleanupGuard::new(),
        Direction::Right,
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
    let _client = rt
        .bootstrap_profile(SessionId::new(), tmpl, viewport(), SystemTime::UNIX_EPOCH)
        .expect("profile launches");

    assert_eq!(rt.sessions.len(), 1);
    let session = rt.sessions.values().next().expect("one session");
    assert_eq!(session.tabs.len(), 1);
    let tab = session.tabs.values().next().expect("one tab");
    assert_eq!(tab.layout().leaf_panes().len(), 2, "two panes in the tab");
    assert_eq!(rt.pty_handles.len(), 2, "both panes' PTYs are parked");
}

#[test]
fn a_profile_focuses_the_pane_it_marks() {
    let (mut rt, _fake) = runtime();
    // The second pane carries `focus`.
    let tmpl =
        template("version 1\ntab {\n    horizontal {\n        pane\n        pane {\n            focus\n        }\n    }\n}");
    let client = rt
        .bootstrap_profile(SessionId::new(), tmpl, viewport(), SystemTime::UNIX_EPOCH)
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
    let _client = rt
        .bootstrap_profile(SessionId::new(), tmpl, viewport(), SystemTime::UNIX_EPOCH)
        .expect("profile launches");

    let session = rt.sessions.values().next().expect("one session");
    assert_eq!(session.tabs.len(), 2);
    assert_eq!(rt.pty_handles.len(), 2, "one PTY per tab's single pane");
}

#[test]
fn a_profile_with_a_plugin_pane_is_refused_and_commits_nothing() {
    let (mut rt, _fake) = runtime();
    let tmpl = template("version 1\ntab {\n    plugin \"sidebar\"\n}");
    let err = rt
        .bootstrap_profile(SessionId::new(), tmpl, viewport(), SystemTime::UNIX_EPOCH)
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
fn a_profile_records_focus_for_every_tab() {
    // Every tab — not just the starting one — records a focused pane on the
    // client, so keyboard input resolves after switching to a non-starting tab.
    let (mut rt, _fake) = runtime();
    let tmpl = template("version 1\ntab {\n    pane\n}\ntab {\n    pane\n}");
    let client = rt
        .bootstrap_profile(SessionId::new(), tmpl, viewport(), SystemTime::UNIX_EPOCH)
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
