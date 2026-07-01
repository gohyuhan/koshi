//! In-memory fake PTY backend.
//!
//! Layout, session, and runtime tests need to exercise pane spawning, writing,
//! resizing, and child-exit handling without launching real shells: real
//! processes make tests slow, platform-dependent, and non-deterministic.
//! [`fake_pty::FakePtyBackend`] satisfies the full [`tile_pty::backend::state::PtyBackend`] surface entirely in
//! memory, capturing every call so a test can assert on it and driving output
//! and child-exit on demand.
//!
//! It implements the canonical [`tile_pty`] trait, so a test can drive it
//! through the same interface the real backend exposes. [`fake_pty::FakePtyBackend`] is
//! the permanent test double: its capture/drive surface — `push_output`,
//! `trigger_child_exit`, and the `*s` query methods — is what tests assert on.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::Mutex;

pub use tile_core::ids::PaneId;
pub use tile_core::process::{ExitStatus, KillPolicy, PtySize, SpawnSpec};
pub use tile_pty::backend::state::{PtyBackend, PtyHandle};
pub use tile_pty::error::{PtyError, Result};

/// Everything the backend records and drives for a single spawned pane.
struct PaneRecord {
    spec: SpawnSpec,
    resizes: Vec<PtySize>,
    writes: Vec<Vec<u8>>,
    kills: Vec<KillPolicy>,
    output_tx: Sender<Vec<u8>>,
    exit_tx: Sender<ExitStatus>,
}

/// Backend state behind the [`Mutex`]; the trait takes `&self`, so all mutation
/// goes through interior mutability.
#[derive(Default)]
struct State {
    panes: HashMap<PaneId, PaneRecord>,
    spawn_order: Vec<PaneId>,
    /// When set, every [`spawn`](FakePtyBackend::spawn) fails with this error
    /// instead of registering a pane — drives the spawn-failure path.
    spawn_error: Option<PtyError>,
    /// When set, [`resize`](FakePtyBackend::resize) fails for this pane with this
    /// error instead of recording — drives the best-effort partial-failure reflow
    /// path (one sibling's resize failing must not drop the others').
    resize_error: Option<(PaneId, PtyError)>,
}

/// An in-memory [`PtyBackend`] that records every call and lets the test drive
/// output and child-exit by hand.
#[derive(Default)]
pub struct FakePtyBackend {
    state: Mutex<State>,
}

impl FakePtyBackend {
    /// Create an empty backend with no spawned panes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Make every subsequent [`spawn`](Self::spawn) fail with `error` instead of
    /// registering a pane, so a test can drive the spawn-failure path.
    pub fn fail_spawns_with(&self, error: PtyError) {
        self.state.lock().unwrap().spawn_error = Some(error);
    }

    /// Make [`resize`](Self::resize) fail for `pane` with `error` instead of
    /// recording, so a test can drive the best-effort partial-failure reflow path
    /// (one sibling failing to resize must not drop the others').
    pub fn fail_resizes_on(&self, pane: PaneId, error: PtyError) {
        self.state.lock().unwrap().resize_error = Some((pane, error));
    }

    /// Deliver `bytes` as a chunk of child output on `pane`'s handle.
    ///
    /// Returns [`PtyError::UnknownPane`] if the pane was never spawned. If the
    /// handle has been dropped the bytes are discarded silently, mirroring a
    /// real child writing to a closed reader.
    pub fn push_output(&self, pane: PaneId, bytes: impl Into<Vec<u8>>) -> Result<()> {
        let state = self.state.lock().unwrap();
        let record = state
            .panes
            .get(&pane)
            .ok_or(PtyError::UnknownPane { pane })?;
        let _ = record.output_tx.send(bytes.into());
        Ok(())
    }

    /// Fire `pane`'s child-exit with the given status on its handle.
    ///
    /// Returns [`PtyError::UnknownPane`] if the pane was never spawned.
    pub fn trigger_child_exit(&self, pane: PaneId, status: ExitStatus) -> Result<()> {
        let state = self.state.lock().unwrap();
        let record = state
            .panes
            .get(&pane)
            .ok_or(PtyError::UnknownPane { pane })?;
        let _ = record.exit_tx.send(status);
        Ok(())
    }

    /// The panes spawned so far, in spawn order.
    #[must_use]
    pub fn spawned_panes(&self) -> Vec<PaneId> {
        self.state.lock().unwrap().spawn_order.clone()
    }

    /// The [`SpawnSpec`] a pane was spawned with.
    pub fn spawn_spec(&self, pane: PaneId) -> Result<SpawnSpec> {
        let state = self.state.lock().unwrap();
        state
            .panes
            .get(&pane)
            .map(|r| r.spec.clone())
            .ok_or(PtyError::UnknownPane { pane })
    }

    /// Every write made to a pane, in order.
    pub fn writes(&self, pane: PaneId) -> Result<Vec<Vec<u8>>> {
        let state = self.state.lock().unwrap();
        state
            .panes
            .get(&pane)
            .map(|r| r.writes.clone())
            .ok_or(PtyError::UnknownPane { pane })
    }

    /// Every resize applied to a pane, in order.
    pub fn resizes(&self, pane: PaneId) -> Result<Vec<PtySize>> {
        let state = self.state.lock().unwrap();
        state
            .panes
            .get(&pane)
            .map(|r| r.resizes.clone())
            .ok_or(PtyError::UnknownPane { pane })
    }

    /// Every kill requested for a pane, in order.
    pub fn kills(&self, pane: PaneId) -> Result<Vec<KillPolicy>> {
        let state = self.state.lock().unwrap();
        state
            .panes
            .get(&pane)
            .map(|r| r.kills.clone())
            .ok_or(PtyError::UnknownPane { pane })
    }
}

impl PtyBackend for FakePtyBackend {
    /// Record a pane spawn under the caller's `pane_id` and return a handle.
    ///
    /// Stores the spawn spec and initial size in the pane's record keyed by
    /// `pane_id`, and appends that id to the spawn order. The returned handle
    /// is addressed by the same id and can be used to receive output and exit
    /// status driven by the test via
    /// [`push_output`](Self::push_output) and [`trigger_child_exit`](Self::trigger_child_exit).
    fn spawn(&self, pane_id: PaneId, spec: SpawnSpec, size: PtySize) -> Result<PtyHandle> {
        let mut state = self.state.lock().unwrap();
        if let Some(error) = &state.spawn_error {
            return Err(error.clone());
        }

        debug_assert!(
            !state.panes.contains_key(&pane_id),
            "spawn into an already-live pane id {pane_id}; kill it before respawning"
        );
        let (handle, output_tx, exit_tx) = PtyHandle::new(pane_id);
        state.panes.insert(
            pane_id,
            PaneRecord {
                spec,
                resizes: vec![size],
                writes: Vec::new(),
                kills: Vec::new(),
                output_tx,
                exit_tx,
            },
        );
        state.spawn_order.push(pane_id);

        Ok(handle)
    }

    /// Record a resize operation on a pane.
    ///
    /// Appends the new size to the pane's resize history. The initial size
    /// from spawn is already recorded; subsequent resizes are added in order.
    fn resize(&self, pane: PaneId, size: PtySize) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if let Some((failing, error)) = &state.resize_error {
            if *failing == pane {
                return Err(error.clone());
            }
        }
        let record = state
            .panes
            .get_mut(&pane)
            .ok_or(PtyError::UnknownPane { pane })?;
        record.resizes.push(size);
        Ok(())
    }

    /// Record bytes written to a pane.
    ///
    /// Appends the byte slice to the pane's write history. Calls are
    /// captured in order; a test asserts on them via [`writes`](Self::writes).
    fn write(&self, pane: PaneId, bytes: &[u8]) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let record = state
            .panes
            .get_mut(&pane)
            .ok_or(PtyError::UnknownPane { pane })?;
        record.writes.push(bytes.to_vec());
        Ok(())
    }

    /// Record a kill request for a pane.
    ///
    /// Appends the kill policy to the pane's kill history. Calls are
    /// captured in order; a test asserts on them via [`kills`](Self::kills).
    fn kill(&self, pane: PaneId, kill_policy: KillPolicy) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let record = state
            .panes
            .get_mut(&pane)
            .ok_or(PtyError::UnknownPane { pane })?;
        record.kills.push(kill_policy);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;
    use tile_core::process::ShellKind;

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
}
