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
fn spawning_into_a_live_pane_id_is_refused() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    let handle = pty.spawn(pane, spec(), size(80, 24)).unwrap();

    assert_eq!(
        pty.spawn(pane, spec(), size(100, 30)).err(),
        Some(PtyError::Spawn {
            detail: format!("pane {pane} is already open"),
        })
    );

    // The refused spawn changed nothing: the live pane keeps its record, its
    // handle, and its single place in the spawn order.
    assert_eq!(pty.resizes(pane).unwrap(), vec![size(80, 24)]);
    assert_eq!(pty.spawned_panes(), vec![pane]);
    pty.push_output(pane, b"still mine".to_vec()).unwrap();
    assert_eq!(handle.try_read_output(), Some(b"still mine".to_vec()));
}

#[test]
fn a_killed_pane_id_can_be_spawned_again() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    pty.spawn(pane, spec(), size(80, 24)).unwrap();
    pty.write(pane, b"first\n").unwrap();
    pty.kill(pane, KillPolicy::Force).unwrap();

    let respawned = pty.spawn(pane, spec(), size(100, 30)).unwrap();

    // The record starts over at the new spawn's size, and the spawn order
    // names the id once per spawn.
    assert_eq!(respawned.pane_id(), pane);
    assert_eq!(pty.resizes(pane).unwrap(), vec![size(100, 30)]);
    assert_eq!(pty.writes(pane).unwrap(), Vec::<Vec<u8>>::new());
    assert_eq!(pty.kills(pane).unwrap(), Vec::<KillPolicy>::new());
    assert_eq!(pty.spawned_panes(), vec![pane, pane]);

    // The pane is live again.
    pty.write(pane, b"second\n").unwrap();
    assert_eq!(pty.writes(pane).unwrap(), vec![b"second\n".to_vec()]);
}

#[test]
fn output_is_delivered_in_order() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    let handle = pty.spawn(pane, spec(), size(80, 24)).unwrap();

    assert_eq!(handle.try_read_output(), None);
    pty.push_output(pane, b"hello".to_vec()).unwrap();
    pty.push_output(pane, b" world".to_vec()).unwrap();

    assert_eq!(handle.try_read_output(), Some(b"hello".to_vec()));
    assert_eq!(handle.try_read_output(), Some(b" world".to_vec()));
    assert_eq!(handle.try_read_output(), None);
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
    let (forced, graceful) = (PaneId::new(), PaneId::new());
    pty.spawn(forced, spec(), size(80, 24)).unwrap();
    pty.spawn(graceful, spec(), size(80, 24)).unwrap();

    pty.kill(forced, KillPolicy::Force).unwrap();
    pty.kill(
        graceful,
        KillPolicy::Graceful {
            timeout: Duration::from_secs(5),
        },
    )
    .unwrap();

    assert_eq!(pty.kills(forced).unwrap(), vec![KillPolicy::Force]);
    assert_eq!(
        pty.kills(graceful).unwrap(),
        vec![KillPolicy::Graceful {
            timeout: Duration::from_secs(5)
        }]
    );
}

#[test]
fn child_exit_fires_once() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    let handle = pty.spawn(pane, spec(), size(80, 24)).unwrap();

    assert_eq!(handle.try_exit_status(), None);
    pty.trigger_child_exit(pane, ExitStatus::ExitCode(0))
        .unwrap();

    assert_eq!(handle.try_exit_status(), Some(ExitStatus::ExitCode(0)));
    assert_eq!(handle.try_exit_status(), None);
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
    assert_eq!(pty.writes(b.pane_id()).unwrap(), Vec::<Vec<u8>>::new());
    assert_eq!(a.try_read_output(), None);
    assert_eq!(b.try_read_output(), Some(b"b".to_vec()));
    assert_eq!(pty.spawned_panes(), vec![a.pane_id(), b.pane_id()]);
}

#[test]
fn a_killed_pane_is_unknown_to_every_later_backend_call() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    pty.spawn(pane, spec(), size(80, 24)).unwrap();
    pty.write(pane, b"before\n").unwrap();

    pty.kill(pane, KillPolicy::Force).unwrap();

    assert_eq!(
        pty.write(pane, b"after\n"),
        Err(PtyError::UnknownPane { pane })
    );
    assert_eq!(
        pty.resize(pane, size(100, 30)),
        Err(PtyError::UnknownPane { pane })
    );
    assert_eq!(
        pty.kill(pane, KillPolicy::Tree),
        Err(PtyError::UnknownPane { pane })
    );

    // The record the pane built while it was live stays readable, and the
    // refused calls added nothing to it.
    assert_eq!(pty.writes(pane).unwrap(), vec![b"before\n".to_vec()]);
    assert_eq!(pty.resizes(pane).unwrap(), vec![size(80, 24)]);
    assert_eq!(pty.kills(pane).unwrap(), vec![KillPolicy::Force]);
    assert_eq!(pty.spawn_spec(pane).unwrap(), spec());
}

#[test]
fn resize_to_zero_size_is_recorded_without_validation() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    pty.spawn(pane, spec(), size(80, 24)).unwrap();

    pty.resize(pane, size(0, 0)).unwrap();

    assert_eq!(pty.resizes(pane).unwrap(), vec![size(80, 24), size(0, 0)]);
}

#[test]
fn close_output_then_push_output_is_silently_dropped() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    let handle = pty.spawn(pane, spec(), size(80, 24)).unwrap();

    pty.close_output(pane).unwrap();
    // Push after close must still return Ok (mirrors a real child writing to
    // a closed reader), but the bytes go nowhere.
    pty.push_output(pane, b"lost".to_vec()).unwrap();

    assert_eq!(handle.try_read_output(), None);
}

#[test]
fn close_output_on_unknown_pane_errors() {
    let pty = FakePtyBackend::new();
    let ghost = PaneId::new();

    assert_eq!(
        pty.close_output(ghost),
        Err(PtyError::UnknownPane { pane: ghost })
    );
}

#[test]
fn trigger_child_exit_twice_queues_both_statuses_in_order() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    let handle = pty.spawn(pane, spec(), size(80, 24)).unwrap();

    pty.trigger_child_exit(pane, ExitStatus::ExitCode(0))
        .unwrap();
    pty.trigger_child_exit(pane, ExitStatus::Signaled(9))
        .unwrap();

    assert_eq!(handle.try_exit_status(), Some(ExitStatus::ExitCode(0)));
    assert_eq!(handle.try_exit_status(), Some(ExitStatus::Signaled(9)));
    assert_eq!(handle.try_exit_status(), None);
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

#[test]
fn an_armed_spawn_failure_is_returned_and_registers_no_pane() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    pty.fail_spawns_with(PtyError::Spawn {
        detail: "no such file".to_string(),
    });

    assert_eq!(
        pty.spawn(pane, spec(), size(80, 24)).err(),
        Some(PtyError::Spawn {
            detail: "no such file".to_string()
        })
    );
    // The failed spawn left nothing behind: the pane is unknown to every query.
    assert_eq!(pty.spawned_panes(), Vec::<PaneId>::new());
    assert_eq!(pty.spawn_spec(pane), Err(PtyError::UnknownPane { pane }));

    // The failure stays armed for the next spawn too.
    let other = PaneId::new();
    assert_eq!(
        pty.spawn(other, spec(), size(80, 24)).err(),
        Some(PtyError::Spawn {
            detail: "no such file".to_string()
        })
    );
}

#[test]
fn an_armed_resize_failure_hits_only_the_pane_it_names() {
    let pty = FakePtyBackend::new();
    let failing = PaneId::new();
    let healthy = PaneId::new();
    pty.spawn(failing, spec(), size(80, 24)).unwrap();
    pty.spawn(healthy, spec(), size(80, 24)).unwrap();
    pty.fail_resizes_on(
        failing,
        PtyError::Io {
            detail: "ioctl refused".to_string(),
        },
    );

    assert_eq!(
        pty.resize(failing, size(100, 30)),
        Err(PtyError::Io {
            detail: "ioctl refused".to_string()
        })
    );
    pty.resize(healthy, size(100, 30)).unwrap();

    // The refused resize is not recorded; the other pane's is.
    assert_eq!(pty.resizes(failing).unwrap(), vec![size(80, 24)]);
    assert_eq!(
        pty.resizes(healthy).unwrap(),
        vec![size(80, 24), size(100, 30)]
    );
}

#[test]
fn an_armed_write_failure_hits_only_the_pane_it_names() {
    let pty = FakePtyBackend::new();
    let failing = PaneId::new();
    let healthy = PaneId::new();
    pty.spawn(failing, spec(), size(80, 24)).unwrap();
    pty.spawn(healthy, spec(), size(80, 24)).unwrap();
    pty.fail_writes_on(
        failing,
        PtyError::Io {
            detail: "broken pipe".to_string(),
        },
    );

    assert_eq!(
        pty.write(failing, b"ls\n"),
        Err(PtyError::Io {
            detail: "broken pipe".to_string()
        })
    );
    pty.write(healthy, b"ls\n").unwrap();

    // The refused write is not recorded; the other pane's is.
    assert_eq!(pty.writes(failing).unwrap(), Vec::<Vec<u8>>::new());
    assert_eq!(pty.writes(healthy).unwrap(), vec![b"ls\n".to_vec()]);
}

#[test]
fn live_cwd_answers_the_directory_set_for_a_pane_and_none_for_every_other() {
    let pty = FakePtyBackend::new();
    let told = PaneId::new();
    let untold = PaneId::new();
    pty.spawn(told, spec(), size(80, 24)).unwrap();
    pty.spawn(untold, spec(), size(80, 24)).unwrap();

    pty.set_live_cwd(told, "/home/dev/work");

    assert_eq!(pty.live_cwd(told), Some(PathBuf::from("/home/dev/work")));
    assert_eq!(pty.live_cwd(untold), None);
    // A pane that was never spawned answers `None` rather than erroring.
    assert_eq!(pty.live_cwd(PaneId::new()), None);

    // The latest directory set for a pane replaces the earlier one.
    pty.set_live_cwd(told, "/tmp");
    assert_eq!(pty.live_cwd(told), Some(PathBuf::from("/tmp")));
}

#[test]
fn push_output_after_the_handle_is_dropped_returns_ok_and_discards_the_bytes() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    let handle = pty.spawn(pane, spec(), size(80, 24)).unwrap();
    drop(handle);

    assert_eq!(pty.push_output(pane, b"gone".to_vec()), Ok(()));
    assert_eq!(pty.spawned_panes(), vec![pane]);
}

#[test]
fn trigger_child_exit_after_the_handle_is_dropped_returns_ok() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    let handle = pty.spawn(pane, spec(), size(80, 24)).unwrap();
    drop(handle);

    assert_eq!(
        pty.trigger_child_exit(pane, ExitStatus::ExitCode(1)),
        Ok(())
    );
}

#[test]
fn push_output_of_an_empty_chunk_delivers_an_empty_chunk() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    let handle = pty.spawn(pane, spec(), size(80, 24)).unwrap();

    pty.push_output(pane, Vec::new()).unwrap();

    assert_eq!(handle.try_read_output(), Some(Vec::new()));
    assert_eq!(handle.try_read_output(), None);
}

#[test]
fn an_empty_write_is_recorded_as_an_empty_chunk() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    pty.spawn(pane, spec(), size(80, 24)).unwrap();

    pty.write(pane, b"").unwrap();

    assert_eq!(pty.writes(pane).unwrap(), vec![Vec::<u8>::new()]);
}

#[test]
fn close_output_twice_on_the_same_pane_returns_ok_both_times() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    let handle = pty.spawn(pane, spec(), size(80, 24)).unwrap();

    assert_eq!(pty.close_output(pane), Ok(()));
    assert_eq!(pty.close_output(pane), Ok(()));
    pty.push_output(pane, b"lost".to_vec()).unwrap();

    assert_eq!(handle.try_read_output(), None);
}

#[test]
fn close_output_leaves_the_exit_channel_open() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    let handle = pty.spawn(pane, spec(), size(80, 24)).unwrap();

    pty.close_output(pane).unwrap();
    pty.trigger_child_exit(pane, ExitStatus::ExitCode(0))
        .unwrap();

    assert_eq!(handle.try_exit_status(), Some(ExitStatus::ExitCode(0)));
}

#[test]
fn output_and_exit_still_reach_the_handle_after_a_kill() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    let handle = pty.spawn(pane, spec(), size(80, 24)).unwrap();

    pty.kill(pane, KillPolicy::Force).unwrap();
    pty.push_output(pane, b"late".to_vec()).unwrap();
    pty.trigger_child_exit(pane, ExitStatus::Signaled(9))
        .unwrap();

    assert_eq!(handle.try_read_output(), Some(b"late".to_vec()));
    assert_eq!(handle.try_exit_status(), Some(ExitStatus::Signaled(9)));
}

#[test]
fn an_armed_spawn_failure_leaves_panes_spawned_earlier_working() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    let handle = pty.spawn(pane, spec(), size(80, 24)).unwrap();
    pty.fail_spawns_with(PtyError::Spawn {
        detail: "no such file".to_string(),
    });

    pty.write(pane, b"ls\n").unwrap();
    pty.resize(pane, size(100, 30)).unwrap();
    pty.kill(pane, KillPolicy::Force).unwrap();
    pty.push_output(pane, b"out".to_vec()).unwrap();

    assert_eq!(pty.writes(pane).unwrap(), vec![b"ls\n".to_vec()]);
    assert_eq!(
        pty.resizes(pane).unwrap(),
        vec![size(80, 24), size(100, 30)]
    );
    assert_eq!(pty.kills(pane).unwrap(), vec![KillPolicy::Force]);
    assert_eq!(handle.try_read_output(), Some(b"out".to_vec()));
}

#[test]
fn a_second_fail_spawns_with_replaces_the_armed_error() {
    let pty = FakePtyBackend::new();
    pty.fail_spawns_with(PtyError::Spawn {
        detail: "first".to_string(),
    });
    pty.fail_spawns_with(PtyError::Io {
        detail: "second".to_string(),
    });

    assert_eq!(
        pty.spawn(PaneId::new(), spec(), size(80, 24)).err(),
        Some(PtyError::Io {
            detail: "second".to_string()
        })
    );
}

#[cfg(debug_assertions)]
#[test]
fn spawning_into_a_live_pane_id_with_a_spawn_failure_armed_returns_the_error_without_panicking() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();
    pty.spawn(pane, spec(), size(80, 24)).unwrap();
    pty.fail_spawns_with(PtyError::Spawn {
        detail: "refused".to_string(),
    });

    assert_eq!(
        pty.spawn(pane, spec(), size(80, 24)).err(),
        Some(PtyError::Spawn {
            detail: "refused".to_string()
        })
    );
    assert_eq!(pty.spawned_panes(), vec![pane]);
}

#[test]
fn an_armed_resize_failure_on_an_unspawned_pane_returns_the_armed_error_not_unknown_pane() {
    let pty = FakePtyBackend::new();
    let ghost = PaneId::new();
    pty.fail_resizes_on(
        ghost,
        PtyError::Io {
            detail: "ioctl refused".to_string(),
        },
    );

    assert_eq!(
        pty.resize(ghost, size(100, 30)),
        Err(PtyError::Io {
            detail: "ioctl refused".to_string()
        })
    );
}

#[test]
fn an_armed_write_failure_on_an_unspawned_pane_returns_the_armed_error_not_unknown_pane() {
    let pty = FakePtyBackend::new();
    let ghost = PaneId::new();
    pty.fail_writes_on(
        ghost,
        PtyError::Io {
            detail: "broken pipe".to_string(),
        },
    );

    assert_eq!(
        pty.write(ghost, b"x"),
        Err(PtyError::Io {
            detail: "broken pipe".to_string()
        })
    );
}

#[test]
fn a_second_fail_resizes_on_moves_the_failure_to_the_new_pane() {
    let pty = FakePtyBackend::new();
    let first = PaneId::new();
    let second = PaneId::new();
    pty.spawn(first, spec(), size(80, 24)).unwrap();
    pty.spawn(second, spec(), size(80, 24)).unwrap();
    pty.fail_resizes_on(
        first,
        PtyError::Io {
            detail: "first".to_string(),
        },
    );
    pty.fail_resizes_on(
        second,
        PtyError::Io {
            detail: "second".to_string(),
        },
    );

    pty.resize(first, size(100, 30)).unwrap();
    assert_eq!(
        pty.resize(second, size(100, 30)),
        Err(PtyError::Io {
            detail: "second".to_string()
        })
    );
    assert_eq!(
        pty.resizes(first).unwrap(),
        vec![size(80, 24), size(100, 30)]
    );
    assert_eq!(pty.resizes(second).unwrap(), vec![size(80, 24)]);
}

#[test]
fn a_second_fail_writes_on_moves_the_failure_to_the_new_pane() {
    let pty = FakePtyBackend::new();
    let first = PaneId::new();
    let second = PaneId::new();
    pty.spawn(first, spec(), size(80, 24)).unwrap();
    pty.spawn(second, spec(), size(80, 24)).unwrap();
    pty.fail_writes_on(
        first,
        PtyError::Io {
            detail: "first".to_string(),
        },
    );
    pty.fail_writes_on(
        second,
        PtyError::Io {
            detail: "second".to_string(),
        },
    );

    pty.write(first, b"ok").unwrap();
    assert_eq!(
        pty.write(second, b"refused"),
        Err(PtyError::Io {
            detail: "second".to_string()
        })
    );
    assert_eq!(pty.writes(first).unwrap(), vec![b"ok".to_vec()]);
    assert_eq!(pty.writes(second).unwrap(), Vec::<Vec<u8>>::new());
}

#[test]
fn set_live_cwd_answers_for_a_pane_that_was_never_spawned() {
    let pty = FakePtyBackend::new();
    let pane = PaneId::new();

    pty.set_live_cwd(pane, "/srv");

    assert_eq!(pty.live_cwd(pane), Some(PathBuf::from("/srv")));
    assert_eq!(pty.spawn_spec(pane), Err(PtyError::UnknownPane { pane }));
}

#[test]
fn each_pane_keeps_its_own_spawn_spec() {
    let pty = FakePtyBackend::new();
    let zsh = PaneId::new();
    let bash = PaneId::new();
    let bash_spec = SpawnSpec {
        program: PathBuf::from("/bin/bash"),
        args: vec!["-l".to_string()],
        cwd: Some(PathBuf::from("/home/dev")),
        env: BTreeMap::from([("TERM".to_string(), "xterm-256color".to_string())]),
        shell_kind: ShellKind::Bash,
    };
    pty.spawn(zsh, spec(), size(80, 24)).unwrap();
    pty.spawn(bash, bash_spec.clone(), size(132, 43)).unwrap();

    assert_eq!(pty.spawn_spec(zsh).unwrap(), spec());
    assert_eq!(pty.spawn_spec(bash).unwrap(), bash_spec);
    assert_eq!(pty.resizes(zsh).unwrap(), vec![size(80, 24)]);
    assert_eq!(pty.resizes(bash).unwrap(), vec![size(132, 43)]);
}
