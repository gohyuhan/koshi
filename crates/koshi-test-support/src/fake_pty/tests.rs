//! Tests for the in-memory fake PTY backend.

use super::*;
use koshi_core::process::ShellKind;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

fn build_spawn_spec() -> SpawnSpec {
    SpawnSpec {
        program: PathBuf::from("/bin/zsh"),
        arguments: Vec::new(),
        working_directory: None,
        environment_variables: BTreeMap::new(),
        shell_kind: ShellKind::Zsh,
    }
}

fn build_pty_size(column_count: u16, row_count: u16) -> PtySize {
    PtySize {
        column_count,
        row_count,
    }
}

#[test]
fn spawn_records_spawn_spec_and_initial_pty_size() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();

    assert_eq!(fake_pty_backend.list_spawned_pane_ids(), vec![pane_id]);
    assert_eq!(
        fake_pty_backend.get_spawn_spec(pane_id).unwrap(),
        build_spawn_spec()
    );
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id).unwrap(),
        vec![build_pty_size(80, 24)]
    );
}

#[test]
fn spawning_into_a_live_pane_id_is_refused() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    let pane_handle = fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();

    assert_eq!(
        fake_pty_backend
            .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(100, 30))
            .err(),
        Some(PtyError::Spawn {
            detail: format!("pane {pane_id} is already open"),
        })
    );

    // The refused spawn changed nothing: the live pane keeps its record, its
    // pane handle, and its single place in the spawn order.
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id).unwrap(),
        vec![build_pty_size(80, 24)]
    );
    assert_eq!(fake_pty_backend.list_spawned_pane_ids(), vec![pane_id]);
    fake_pty_backend
        .push_output(pane_id, b"still mine".to_vec())
        .unwrap();
    assert_eq!(
        pane_handle.try_receive_output_chunk(),
        Some(b"still mine".to_vec())
    );
}

#[test]
fn a_killed_pane_id_can_be_spawned_again() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();
    fake_pty_backend
        .write_pane_input(pane_id, b"first\n")
        .unwrap();
    fake_pty_backend
        .kill_pane(pane_id, KillPolicy::Force)
        .unwrap();

    let respawned_pane_handle = fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(100, 30))
        .unwrap();

    // The record starts over at the new spawn's size, and the spawn order
    // names the id once per spawn.
    assert_eq!(respawned_pane_handle.get_pane_id(), pane_id);
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id).unwrap(),
        vec![build_pty_size(100, 30)]
    );
    assert_eq!(
        fake_pty_backend.list_pane_write_bytes(pane_id).unwrap(),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(
        fake_pty_backend.list_pane_kill_policies(pane_id).unwrap(),
        Vec::<KillPolicy>::new()
    );
    assert_eq!(
        fake_pty_backend.list_spawned_pane_ids(),
        vec![pane_id, pane_id]
    );

    // The pane is live again.
    fake_pty_backend
        .write_pane_input(pane_id, b"second\n")
        .unwrap();
    assert_eq!(
        fake_pty_backend.list_pane_write_bytes(pane_id).unwrap(),
        vec![b"second\n".to_vec()]
    );
}

#[test]
fn output_chunks_are_delivered_in_order() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    let pane_handle = fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();

    assert_eq!(pane_handle.try_receive_output_chunk(), None);
    fake_pty_backend
        .push_output(pane_id, b"hello".to_vec())
        .unwrap();
    fake_pty_backend
        .push_output(pane_id, b" world".to_vec())
        .unwrap();

    assert_eq!(
        pane_handle.try_receive_output_chunk(),
        Some(b"hello".to_vec())
    );
    assert_eq!(
        pane_handle.try_receive_output_chunk(),
        Some(b" world".to_vec())
    );
    assert_eq!(pane_handle.try_receive_output_chunk(), None);
}

#[test]
fn pane_input_writes_are_captured() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();

    fake_pty_backend.write_pane_input(pane_id, b"ls\n").unwrap();
    fake_pty_backend
        .write_pane_input(pane_id, b"exit\n")
        .unwrap();

    assert_eq!(
        fake_pty_backend.list_pane_write_bytes(pane_id).unwrap(),
        vec![b"ls\n".to_vec(), b"exit\n".to_vec()]
    );
}

#[test]
fn resizes_are_captured_after_initial_spawn() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();

    fake_pty_backend
        .resize_pane(pane_id, build_pty_size(100, 30))
        .unwrap();
    fake_pty_backend
        .resize_pane(pane_id, build_pty_size(120, 40))
        .unwrap();

    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id).unwrap(),
        vec![
            build_pty_size(80, 24),
            build_pty_size(100, 30),
            build_pty_size(120, 40)
        ]
    );
}

#[test]
fn kill_policies_are_captured() {
    let fake_pty_backend = FakePtyBackend::new();
    let (forced_pane_id, graceful_pane_id) = (PaneId::new(), PaneId::new());
    fake_pty_backend
        .spawn_pane(forced_pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();
    fake_pty_backend
        .spawn_pane(graceful_pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();

    fake_pty_backend
        .kill_pane(forced_pane_id, KillPolicy::Force)
        .unwrap();
    fake_pty_backend
        .kill_pane(
            graceful_pane_id,
            KillPolicy::Graceful {
                timeout_duration: Duration::from_secs(5),
            },
        )
        .unwrap();

    assert_eq!(
        fake_pty_backend
            .list_pane_kill_policies(forced_pane_id)
            .unwrap(),
        vec![KillPolicy::Force]
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_kill_policies(graceful_pane_id)
            .unwrap(),
        vec![KillPolicy::Graceful {
            timeout_duration: Duration::from_secs(5)
        }]
    );
}

#[test]
fn child_exit_status_is_delivered_once() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    let pane_handle = fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();

    assert_eq!(pane_handle.try_receive_exit_status(), None);
    fake_pty_backend
        .trigger_child_exit(pane_id, ExitStatus::ExitCode(0))
        .unwrap();

    assert_eq!(
        pane_handle.try_receive_exit_status(),
        Some(ExitStatus::ExitCode(0))
    );
    assert_eq!(pane_handle.try_receive_exit_status(), None);
}

#[test]
fn operations_on_unknown_pane_return_unknown_pane_errors() {
    let fake_pty_backend = FakePtyBackend::new();
    let unknown_pane_id = PaneId::new();

    assert_eq!(
        fake_pty_backend.resize_pane(unknown_pane_id, build_pty_size(80, 24)),
        Err(PtyError::UnknownPane {
            pane_id: unknown_pane_id,
        })
    );
    assert_eq!(
        fake_pty_backend.write_pane_input(unknown_pane_id, b"x"),
        Err(PtyError::UnknownPane {
            pane_id: unknown_pane_id,
        })
    );
    assert_eq!(
        fake_pty_backend.kill_pane(unknown_pane_id, KillPolicy::Force),
        Err(PtyError::UnknownPane {
            pane_id: unknown_pane_id,
        })
    );
    assert_eq!(
        fake_pty_backend.push_output(unknown_pane_id, b"x".to_vec()),
        Err(PtyError::UnknownPane {
            pane_id: unknown_pane_id,
        })
    );
    assert_eq!(
        fake_pty_backend.trigger_child_exit(unknown_pane_id, ExitStatus::ExitCode(0)),
        Err(PtyError::UnknownPane {
            pane_id: unknown_pane_id,
        })
    );
}

#[test]
fn multiple_panes_keep_output_and_input_isolated() {
    let fake_pty_backend = FakePtyBackend::new();
    let (first_pane_id, second_pane_id) = (PaneId::new(), PaneId::new());
    let first_pane_handle = fake_pty_backend
        .spawn_pane(first_pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();
    let second_pane_handle = fake_pty_backend
        .spawn_pane(second_pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();

    fake_pty_backend
        .write_pane_input(first_pane_handle.get_pane_id(), b"a")
        .unwrap();
    fake_pty_backend
        .push_output(second_pane_handle.get_pane_id(), b"b".to_vec())
        .unwrap();

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(first_pane_handle.get_pane_id())
            .unwrap(),
        vec![b"a".to_vec()]
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(second_pane_handle.get_pane_id())
            .unwrap(),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(first_pane_handle.try_receive_output_chunk(), None);
    assert_eq!(
        second_pane_handle.try_receive_output_chunk(),
        Some(b"b".to_vec())
    );
    assert_eq!(
        fake_pty_backend.list_spawned_pane_ids(),
        vec![
            first_pane_handle.get_pane_id(),
            second_pane_handle.get_pane_id()
        ]
    );
}

#[test]
fn killed_pane_rejects_each_subsequent_backend_call() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();
    fake_pty_backend
        .write_pane_input(pane_id, b"before\n")
        .unwrap();

    fake_pty_backend
        .kill_pane(pane_id, KillPolicy::Force)
        .unwrap();

    assert_eq!(
        fake_pty_backend.write_pane_input(pane_id, b"after\n"),
        Err(PtyError::UnknownPane { pane_id })
    );
    assert_eq!(
        fake_pty_backend.resize_pane(pane_id, build_pty_size(100, 30)),
        Err(PtyError::UnknownPane { pane_id })
    );
    assert_eq!(
        fake_pty_backend.kill_pane(pane_id, KillPolicy::Tree),
        Err(PtyError::UnknownPane { pane_id })
    );

    // The record the pane built while it was live stays readable, and the
    // refused calls added nothing to it.
    assert_eq!(
        fake_pty_backend.list_pane_write_bytes(pane_id).unwrap(),
        vec![b"before\n".to_vec()]
    );
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id).unwrap(),
        vec![build_pty_size(80, 24)]
    );
    assert_eq!(
        fake_pty_backend.list_pane_kill_policies(pane_id).unwrap(),
        vec![KillPolicy::Force]
    );
    assert_eq!(
        fake_pty_backend.get_spawn_spec(pane_id).unwrap(),
        build_spawn_spec()
    );
}

#[test]
fn resize_to_zero_size_is_recorded_without_backend_validation() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();

    fake_pty_backend
        .resize_pane(pane_id, build_pty_size(0, 0))
        .unwrap();

    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id).unwrap(),
        vec![build_pty_size(80, 24), build_pty_size(0, 0)]
    );
}

#[test]
fn pushing_output_after_close_discards_the_output() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    let pane_handle = fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();

    fake_pty_backend.close_output(pane_id).unwrap();
    // Push after close must still return Ok (mirrors a real child writing to
    // a closed reader), but the bytes go nowhere.
    fake_pty_backend
        .push_output(pane_id, b"lost".to_vec())
        .unwrap();

    assert_eq!(pane_handle.try_receive_output_chunk(), None);
}

#[test]
fn closing_output_for_unknown_pane_returns_unknown_pane_error() {
    let fake_pty_backend = FakePtyBackend::new();
    let unknown_pane_id = PaneId::new();

    assert_eq!(
        fake_pty_backend.close_output(unknown_pane_id),
        Err(PtyError::UnknownPane {
            pane_id: unknown_pane_id,
        })
    );
}

#[test]
fn triggering_child_exit_twice_queues_both_statuses_in_order() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    let pane_handle = fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();

    fake_pty_backend
        .trigger_child_exit(pane_id, ExitStatus::ExitCode(0))
        .unwrap();
    fake_pty_backend
        .trigger_child_exit(pane_id, ExitStatus::Signaled(9))
        .unwrap();

    assert_eq!(
        pane_handle.try_receive_exit_status(),
        Some(ExitStatus::ExitCode(0))
    );
    assert_eq!(
        pane_handle.try_receive_exit_status(),
        Some(ExitStatus::Signaled(9))
    );
    assert_eq!(pane_handle.try_receive_exit_status(), None);
}

#[test]
fn fake_pty_backend_is_usable_as_a_pty_backend_trait_object() {
    // The fake stands in for any `PtyBackend`, so it must work behind a trait
    // object the way the real backend will. Drive a full spawn/resize/write/
    // kill/exit cycle through `&dyn PtyBackend` plus the inherent queries.
    let fake_pty_backend = FakePtyBackend::new();
    let pty_backend: &dyn PtyBackend = &fake_pty_backend;

    let pane_id = PaneId::new();
    let pane_handle = pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();
    pty_backend
        .resize_pane(pane_id, build_pty_size(100, 30))
        .unwrap();
    pty_backend.write_pane_input(pane_id, b"ls\n").unwrap();
    pty_backend.kill_pane(pane_id, KillPolicy::Force).unwrap();

    // Calls made through the trait object are captured like inherent ones.
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id).unwrap(),
        vec![build_pty_size(80, 24), build_pty_size(100, 30)]
    );
    assert_eq!(
        fake_pty_backend.list_pane_write_bytes(pane_id).unwrap(),
        vec![b"ls\n".to_vec()]
    );
    assert_eq!(
        fake_pty_backend.list_pane_kill_policies(pane_id).unwrap(),
        vec![KillPolicy::Force]
    );

    // The pane handle the trait object returned streams exit status canonically.
    fake_pty_backend
        .trigger_child_exit(pane_id, ExitStatus::ExitCode(0))
        .unwrap();
    assert_eq!(
        pane_handle.try_receive_exit_status(),
        Some(ExitStatus::ExitCode(0))
    );
}

#[test]
fn armed_spawn_failure_is_returned_and_registers_no_pane() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    fake_pty_backend.fail_spawns_with(PtyError::Spawn {
        detail: "no such file".to_string(),
    });

    assert_eq!(
        fake_pty_backend
            .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
            .err(),
        Some(PtyError::Spawn {
            detail: "no such file".to_string()
        })
    );
    // The failed spawn left nothing behind: the pane is unknown to every query.
    assert_eq!(
        fake_pty_backend.list_spawned_pane_ids(),
        Vec::<PaneId>::new()
    );
    assert_eq!(
        fake_pty_backend.get_spawn_spec(pane_id),
        Err(PtyError::UnknownPane { pane_id })
    );

    // The failure stays armed for the next spawn too.
    let other_pane_id = PaneId::new();
    assert_eq!(
        fake_pty_backend
            .spawn_pane(other_pane_id, build_spawn_spec(), build_pty_size(80, 24),)
            .err(),
        Some(PtyError::Spawn {
            detail: "no such file".to_string()
        })
    );
}

#[test]
fn armed_resize_failure_hits_only_the_named_pane() {
    let fake_pty_backend = FakePtyBackend::new();
    let failing_pane_id = PaneId::new();
    let healthy_pane_id = PaneId::new();
    fake_pty_backend
        .spawn_pane(failing_pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();
    fake_pty_backend
        .spawn_pane(healthy_pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();
    fake_pty_backend.fail_resizes_on(
        failing_pane_id,
        PtyError::Io {
            detail: "ioctl refused".to_string(),
        },
    );

    assert_eq!(
        fake_pty_backend.resize_pane(failing_pane_id, build_pty_size(100, 30)),
        Err(PtyError::Io {
            detail: "ioctl refused".to_string()
        })
    );
    fake_pty_backend
        .resize_pane(healthy_pane_id, build_pty_size(100, 30))
        .unwrap();

    // The refused resize is not recorded; the other pane's is.
    assert_eq!(
        fake_pty_backend.list_pane_sizes(failing_pane_id).unwrap(),
        vec![build_pty_size(80, 24)]
    );
    assert_eq!(
        fake_pty_backend.list_pane_sizes(healthy_pane_id).unwrap(),
        vec![build_pty_size(80, 24), build_pty_size(100, 30)]
    );
}

#[test]
fn armed_write_failure_hits_only_the_named_pane() {
    let fake_pty_backend = FakePtyBackend::new();
    let failing_pane_id = PaneId::new();
    let healthy_pane_id = PaneId::new();
    fake_pty_backend
        .spawn_pane(failing_pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();
    fake_pty_backend
        .spawn_pane(healthy_pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();
    fake_pty_backend.fail_writes_on(
        failing_pane_id,
        PtyError::Io {
            detail: "broken pipe".to_string(),
        },
    );

    assert_eq!(
        fake_pty_backend.write_pane_input(failing_pane_id, b"ls\n"),
        Err(PtyError::Io {
            detail: "broken pipe".to_string()
        })
    );
    fake_pty_backend
        .write_pane_input(healthy_pane_id, b"ls\n")
        .unwrap();

    // The refused write is not recorded; the other pane's is.
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(failing_pane_id)
            .unwrap(),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(healthy_pane_id)
            .unwrap(),
        vec![b"ls\n".to_vec()]
    );
}

#[test]
fn live_working_directory_returns_the_set_directory_only_for_the_named_pane() {
    let fake_pty_backend = FakePtyBackend::new();
    let directory_pane_id = PaneId::new();
    let other_pane_id = PaneId::new();
    fake_pty_backend
        .spawn_pane(
            directory_pane_id,
            build_spawn_spec(),
            build_pty_size(80, 24),
        )
        .unwrap();
    fake_pty_backend
        .spawn_pane(other_pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();

    fake_pty_backend.set_live_working_directory(directory_pane_id, "/home/dev/work");

    assert_eq!(
        fake_pty_backend.find_live_working_directory(directory_pane_id),
        Some(PathBuf::from("/home/dev/work"))
    );
    assert_eq!(
        fake_pty_backend.find_live_working_directory(other_pane_id),
        None
    );
    // A pane that was never spawned answers `None` rather than erroring.
    assert_eq!(
        fake_pty_backend.find_live_working_directory(PaneId::new()),
        None
    );

    // The latest directory set for a pane replaces the earlier one.
    fake_pty_backend.set_live_working_directory(directory_pane_id, "/tmp");
    assert_eq!(
        fake_pty_backend.find_live_working_directory(directory_pane_id),
        Some(PathBuf::from("/tmp"))
    );
}

#[test]
fn pushing_output_after_handle_drop_returns_ok_and_discards_bytes() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    let pane_handle = fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();
    drop(pane_handle);

    assert_eq!(
        fake_pty_backend.push_output(pane_id, b"gone".to_vec()),
        Ok(())
    );
    assert_eq!(fake_pty_backend.list_spawned_pane_ids(), vec![pane_id]);
}

#[test]
fn triggering_child_exit_after_handle_drop_returns_ok() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    let pane_handle = fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();
    drop(pane_handle);

    assert_eq!(
        fake_pty_backend.trigger_child_exit(pane_id, ExitStatus::ExitCode(1)),
        Ok(())
    );
}

#[test]
fn pushing_empty_output_delivers_an_empty_chunk() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    let pane_handle = fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();

    fake_pty_backend.push_output(pane_id, Vec::new()).unwrap();

    assert_eq!(pane_handle.try_receive_output_chunk(), Some(Vec::new()));
    assert_eq!(pane_handle.try_receive_output_chunk(), None);
}

#[test]
fn writing_empty_input_records_an_empty_chunk() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();

    fake_pty_backend.write_pane_input(pane_id, b"").unwrap();

    assert_eq!(
        fake_pty_backend.list_pane_write_bytes(pane_id).unwrap(),
        vec![Vec::<u8>::new()]
    );
}

#[test]
fn closing_output_twice_on_one_pane_returns_ok_both_times() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    let pane_handle = fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();

    assert_eq!(fake_pty_backend.close_output(pane_id), Ok(()));
    assert_eq!(fake_pty_backend.close_output(pane_id), Ok(()));
    fake_pty_backend
        .push_output(pane_id, b"lost".to_vec())
        .unwrap();

    assert_eq!(pane_handle.try_receive_output_chunk(), None);
}

#[test]
fn closing_output_leaves_exit_status_delivery_open() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    let pane_handle = fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();

    fake_pty_backend.close_output(pane_id).unwrap();
    fake_pty_backend
        .trigger_child_exit(pane_id, ExitStatus::ExitCode(0))
        .unwrap();

    assert_eq!(
        pane_handle.try_receive_exit_status(),
        Some(ExitStatus::ExitCode(0))
    );
}

#[test]
fn output_and_exit_status_reach_handle_after_kill() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    let pane_handle = fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();

    fake_pty_backend
        .kill_pane(pane_id, KillPolicy::Force)
        .unwrap();
    fake_pty_backend
        .push_output(pane_id, b"late".to_vec())
        .unwrap();
    fake_pty_backend
        .trigger_child_exit(pane_id, ExitStatus::Signaled(9))
        .unwrap();

    assert_eq!(
        pane_handle.try_receive_output_chunk(),
        Some(b"late".to_vec())
    );
    assert_eq!(
        pane_handle.try_receive_exit_status(),
        Some(ExitStatus::Signaled(9))
    );
}

#[test]
fn armed_spawn_failure_leaves_earlier_panes_working() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    let pane_handle = fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();
    fake_pty_backend.fail_spawns_with(PtyError::Spawn {
        detail: "no such file".to_string(),
    });

    fake_pty_backend.write_pane_input(pane_id, b"ls\n").unwrap();
    fake_pty_backend
        .resize_pane(pane_id, build_pty_size(100, 30))
        .unwrap();
    fake_pty_backend
        .kill_pane(pane_id, KillPolicy::Force)
        .unwrap();
    fake_pty_backend
        .push_output(pane_id, b"out".to_vec())
        .unwrap();

    assert_eq!(
        fake_pty_backend.list_pane_write_bytes(pane_id).unwrap(),
        vec![b"ls\n".to_vec()]
    );
    assert_eq!(
        fake_pty_backend.list_pane_sizes(pane_id).unwrap(),
        vec![build_pty_size(80, 24), build_pty_size(100, 30)]
    );
    assert_eq!(
        fake_pty_backend.list_pane_kill_policies(pane_id).unwrap(),
        vec![KillPolicy::Force]
    );
    assert_eq!(
        pane_handle.try_receive_output_chunk(),
        Some(b"out".to_vec())
    );
}

#[test]
fn second_spawn_failure_replaces_the_armed_error() {
    let fake_pty_backend = FakePtyBackend::new();
    fake_pty_backend.fail_spawns_with(PtyError::Spawn {
        detail: "first".to_string(),
    });
    fake_pty_backend.fail_spawns_with(PtyError::Io {
        detail: "second".to_string(),
    });

    assert_eq!(
        fake_pty_backend
            .spawn_pane(PaneId::new(), build_spawn_spec(), build_pty_size(80, 24))
            .err(),
        Some(PtyError::Io {
            detail: "second".to_string()
        })
    );
}

#[test]
fn live_pane_spawn_with_armed_failure_returns_error_without_panicking() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();
    fake_pty_backend
        .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();
    fake_pty_backend.fail_spawns_with(PtyError::Spawn {
        detail: "refused".to_string(),
    });

    assert_eq!(
        fake_pty_backend
            .spawn_pane(pane_id, build_spawn_spec(), build_pty_size(80, 24))
            .err(),
        Some(PtyError::Spawn {
            detail: "refused".to_string()
        })
    );
    assert_eq!(fake_pty_backend.list_spawned_pane_ids(), vec![pane_id]);
}

#[test]
fn armed_resize_failure_on_unspawned_pane_returns_armed_error() {
    let fake_pty_backend = FakePtyBackend::new();
    let unknown_pane_id = PaneId::new();
    fake_pty_backend.fail_resizes_on(
        unknown_pane_id,
        PtyError::Io {
            detail: "ioctl refused".to_string(),
        },
    );

    assert_eq!(
        fake_pty_backend.resize_pane(unknown_pane_id, build_pty_size(100, 30)),
        Err(PtyError::Io {
            detail: "ioctl refused".to_string()
        })
    );
}

#[test]
fn armed_write_failure_on_unspawned_pane_returns_armed_error() {
    let fake_pty_backend = FakePtyBackend::new();
    let unknown_pane_id = PaneId::new();
    fake_pty_backend.fail_writes_on(
        unknown_pane_id,
        PtyError::Io {
            detail: "broken pipe".to_string(),
        },
    );

    assert_eq!(
        fake_pty_backend.write_pane_input(unknown_pane_id, b"x"),
        Err(PtyError::Io {
            detail: "broken pipe".to_string()
        })
    );
}

#[test]
fn second_resize_failure_moves_to_the_new_pane() {
    let fake_pty_backend = FakePtyBackend::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    fake_pty_backend
        .spawn_pane(first_pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();
    fake_pty_backend
        .spawn_pane(second_pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();
    fake_pty_backend.fail_resizes_on(
        first_pane_id,
        PtyError::Io {
            detail: "first".to_string(),
        },
    );
    fake_pty_backend.fail_resizes_on(
        second_pane_id,
        PtyError::Io {
            detail: "second".to_string(),
        },
    );

    fake_pty_backend
        .resize_pane(first_pane_id, build_pty_size(100, 30))
        .unwrap();
    assert_eq!(
        fake_pty_backend.resize_pane(second_pane_id, build_pty_size(100, 30)),
        Err(PtyError::Io {
            detail: "second".to_string()
        })
    );
    assert_eq!(
        fake_pty_backend.list_pane_sizes(first_pane_id).unwrap(),
        vec![build_pty_size(80, 24), build_pty_size(100, 30)]
    );
    assert_eq!(
        fake_pty_backend.list_pane_sizes(second_pane_id).unwrap(),
        vec![build_pty_size(80, 24)]
    );
}

#[test]
fn second_write_failure_moves_to_the_new_pane() {
    let fake_pty_backend = FakePtyBackend::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    fake_pty_backend
        .spawn_pane(first_pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();
    fake_pty_backend
        .spawn_pane(second_pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();
    fake_pty_backend.fail_writes_on(
        first_pane_id,
        PtyError::Io {
            detail: "first".to_string(),
        },
    );
    fake_pty_backend.fail_writes_on(
        second_pane_id,
        PtyError::Io {
            detail: "second".to_string(),
        },
    );

    fake_pty_backend
        .write_pane_input(first_pane_id, b"ok")
        .unwrap();
    assert_eq!(
        fake_pty_backend.write_pane_input(second_pane_id, b"refused"),
        Err(PtyError::Io {
            detail: "second".to_string()
        })
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(first_pane_id)
            .unwrap(),
        vec![b"ok".to_vec()]
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(second_pane_id)
            .unwrap(),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn setting_live_working_directory_supports_unspawned_pane() {
    let fake_pty_backend = FakePtyBackend::new();
    let pane_id = PaneId::new();

    fake_pty_backend.set_live_working_directory(pane_id, "/srv");

    assert_eq!(
        fake_pty_backend.find_live_working_directory(pane_id),
        Some(PathBuf::from("/srv"))
    );
    assert_eq!(
        fake_pty_backend.get_spawn_spec(pane_id),
        Err(PtyError::UnknownPane { pane_id })
    );
}

#[test]
fn each_pane_keeps_its_own_spawn_spec() {
    let fake_pty_backend = FakePtyBackend::new();
    let zsh_pane_id = PaneId::new();
    let bash_pane_id = PaneId::new();
    let bash_spec = SpawnSpec {
        program: PathBuf::from("/bin/bash"),
        arguments: vec!["-l".to_string()],
        working_directory: Some(PathBuf::from("/home/dev")),
        environment_variables: BTreeMap::from([("TERM".to_string(), "xterm-256color".to_string())]),
        shell_kind: ShellKind::Bash,
    };
    fake_pty_backend
        .spawn_pane(zsh_pane_id, build_spawn_spec(), build_pty_size(80, 24))
        .unwrap();
    fake_pty_backend
        .spawn_pane(bash_pane_id, bash_spec.clone(), build_pty_size(132, 43))
        .unwrap();

    assert_eq!(
        fake_pty_backend.get_spawn_spec(zsh_pane_id).unwrap(),
        build_spawn_spec()
    );
    assert_eq!(
        fake_pty_backend.get_spawn_spec(bash_pane_id).unwrap(),
        bash_spec
    );
    assert_eq!(
        fake_pty_backend.list_pane_sizes(zsh_pane_id).unwrap(),
        vec![build_pty_size(80, 24)]
    );
    assert_eq!(
        fake_pty_backend.list_pane_sizes(bash_pane_id).unwrap(),
        vec![build_pty_size(132, 43)]
    );
}
