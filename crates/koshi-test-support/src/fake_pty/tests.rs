//! Tests for the in-memory fake PTY backend.

use super::*;
use koshi_core::process::ShellKind;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

fn spec() -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from("/bin/zsh"),
        args: Vec::new(),
        cwd: None,
        env: BTreeMap::new(),
        shell_kind: ShellKind::Zsh,
    }
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize { cols, rows }
}

#[test]
fn spawn_records_spec_and_initial_size() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    pty.spawn(pane, spec(), size(80, 24)).unwrap();

    assert_eq!(pty.spawned_panes(), vec![pane]);
    assert_eq!(pty.spawn_spec(pane).unwrap(), spec());
    assert_eq!(pty.resizes(pane).unwrap(), vec![size(80, 24)]);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "already-live pane id")]
fn spawning_into_a_live_pane_id_panics() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    pty.spawn(pane, spec(), size(80, 24)).unwrap();
    // Reusing a live id (a caller bug: respawn without kill first) trips the
    // debug-build precondition.
    let _ = pty.spawn(pane, spec(), size(80, 24));
}

#[test]
fn output_is_delivered_in_order() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    let handle = pty.spawn(pane, spec(), size(80, 24)).unwrap();

    assert!(handle.try_read_output().is_none());
    pty.push_output(pane, b"hello".to_vec()).unwrap();
    pty.push_output(pane, b" world".to_vec()).unwrap();

    assert_eq!(handle.try_read_output(), Some(b"hello".to_vec()));
    assert_eq!(handle.try_read_output(), Some(b" world".to_vec()));
    assert!(handle.try_read_output().is_none());
}

#[test]
fn writes_are_captured() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    pty.spawn(pane, spec(), size(80, 24)).unwrap();

    pty.write(pane, b"ls\n").unwrap();
    pty.write(pane, b"exit\n").unwrap();

    assert_eq!(
        pty.writes(pane).unwrap(),
        vec![b"ls\n".to_vec(), b"exit\n".to_vec()]
    );
}

#[test]
fn resizes_are_captured_after_initial() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    pty.spawn(pane, spec(), size(80, 24)).unwrap();

    pty.resize(pane, size(100, 30)).unwrap();
    pty.resize(pane, size(120, 40)).unwrap();

    assert_eq!(
        pty.resizes(pane).unwrap(),
        vec![size(80, 24), size(100, 30), size(120, 40)]
    );
}

#[test]
fn kills_are_captured() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    pty.spawn(pane, spec(), size(80, 24)).unwrap();

    pty.kill(pane, KillPolicy::Force).unwrap();
    pty.kill(
        pane,
        KillPolicy::Graceful {
            timeout: Duration::from_secs(5),
        },
    )
    .unwrap();

    assert_eq!(
        pty.kills(pane).unwrap(),
        vec![
            KillPolicy::Force,
            KillPolicy::Graceful {
                timeout: Duration::from_secs(5)
            }
        ]
    );
}

#[test]
fn child_exit_fires_once() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    let handle = pty.spawn(pane, spec(), size(80, 24)).unwrap();

    assert!(handle.try_exit_status().is_none());
    pty.trigger_child_exit(pane, ExitStatus::ExitCode(0))
        .unwrap();

    assert_eq!(handle.try_exit_status(), Some(ExitStatus::ExitCode(0)));
    assert!(handle.try_exit_status().is_none());
}

#[test]
fn operations_on_unknown_pane_error() {
    let pty = FakePtyBackend::new();
    let ghost = PaneId::new();

    assert_eq!(
        pty.resize(ghost, size(80, 24)),
        Err(PtyError::UnknownPane { pane: ghost })
    );
    assert_eq!(
        pty.write(ghost, b"x"),
        Err(PtyError::UnknownPane { pane: ghost })
    );
    assert_eq!(
        pty.kill(ghost, KillPolicy::Force),
        Err(PtyError::UnknownPane { pane: ghost })
    );
    assert_eq!(
        pty.push_output(ghost, b"x".to_vec()),
        Err(PtyError::UnknownPane { pane: ghost })
    );
    assert_eq!(
        pty.trigger_child_exit(ghost, ExitStatus::ExitCode(0)),
        Err(PtyError::UnknownPane { pane: ghost })
    );
}

#[test]
fn multiple_panes_are_isolated() {
    let pty = FakePtyBackend::new();
    let (a_id, b_id) = (PaneId::new(), PaneId::new());
    let a = pty.spawn(a_id, spec(), size(80, 24)).unwrap();
    let b = pty.spawn(b_id, spec(), size(80, 24)).unwrap();

    pty.write(a.pane_id(), b"a").unwrap();
    pty.push_output(b.pane_id(), b"b".to_vec()).unwrap();

    assert_eq!(pty.writes(a.pane_id()).unwrap(), vec![b"a".to_vec()]);
    assert!(pty.writes(b.pane_id()).unwrap().is_empty());
    assert!(a.try_read_output().is_none());
    assert_eq!(b.try_read_output(), Some(b"b".to_vec()));
    assert_eq!(pty.spawned_panes(), vec![a.pane_id(), b.pane_id()]);
}

#[test]
fn the_fake_is_usable_as_a_pty_backend_trait_object() {
    // The fake stands in for any `PtyBackend`, so it must work behind a trait
    // object the way the real backend will. Drive a full spawn/resize/write/
    // kill/exit cycle through `&dyn PtyBackend` plus the inherent queries.
    let pty = FakePtyBackend::new();
    let backend: &dyn PtyBackend = &pty;

    let pane = PaneId::new();
    let handle = backend.spawn(pane, spec(), size(80, 24)).unwrap();
    backend.resize(pane, size(100, 30)).unwrap();
    backend.write(pane, b"ls\n").unwrap();
    backend.kill(pane, KillPolicy::Force).unwrap();

    // Calls made through the trait object are captured like inherent ones.
    assert_eq!(
        pty.resizes(pane).unwrap(),
        vec![size(80, 24), size(100, 30)]
    );
    assert_eq!(pty.writes(pane).unwrap(), vec![b"ls\n".to_vec()]);
    assert_eq!(pty.kills(pane).unwrap(), vec![KillPolicy::Force]);

    // The handle the trait object returned streams exit status canonically.
    pty.trigger_child_exit(pane, ExitStatus::ExitCode(0))
        .unwrap();
    assert_eq!(handle.try_exit_status(), Some(ExitStatus::ExitCode(0)));
}
