//! In-memory fake PTY (pseudo-terminal, the virtual terminal a shell process
//! runs inside) backend.
//!
//! [`fake_pty::FakePtyBackend`] implements the whole
//! [`koshi_pty::backend::state::PtyBackend`] trait in memory, without launching
//! a real shell. It records every spawn, write, resize, and kill.
//!
//! A test reads those records back with `spawned_panes`, `spawn_spec`,
//! `writes`, `resizes`, and `kills`. A test drives child output with
//! `push_output` and child exit with `trigger_child_exit`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Mutex;

pub use koshi_core::ids::PaneId;
pub use koshi_core::process::{ExitStatus, KillPolicy, PtySize, SpawnSpec};
pub use koshi_pty::backend::state::{PtyBackend, PtyHandle};
pub use koshi_pty::error::{PtyError, Result};

/// Everything the backend records and drives for a single spawned pane.
struct PaneRecord {
    spec: SpawnSpec,
    resizes: Vec<PtySize>,
    writes: Vec<Vec<u8>>,
    kills: Vec<KillPolicy>,
    /// The output channel's sending end. [`close_output`](FakePtyBackend::close_output)
    /// sets it to `None`, which models the child's PTY reaching EOF.
    /// [`push_output`](FakePtyBackend::push_output) then discards its bytes.
    output_tx: Option<Sender<Vec<u8>>>,
    exit_tx: Sender<ExitStatus>,
}

/// Backend state behind the [`Mutex`]. The trait methods take `&self`, so every
/// mutation goes through interior mutability.
#[derive(Default)]
struct State {
    panes: HashMap<PaneId, PaneRecord>,
    spawn_order: Vec<PaneId>,
    /// When set, every [`spawn`](FakePtyBackend::spawn) fails with this error
    /// instead of registering a pane.
    spawn_error: Option<PtyError>,
    /// When set, [`resize`](FakePtyBackend::resize) fails for this pane with this
    /// error instead of recording.
    resize_error: Option<(PaneId, PtyError)>,
    /// When set, [`write`](FakePtyBackend::write) fails for this pane with this
    /// error instead of recording.
    write_error: Option<(PaneId, PtyError)>,
    /// Per-pane answers for [`live_cwd`](FakePtyBackend::live_cwd). A pane with
    /// no entry answers `None`.
    live_cwds: HashMap<PaneId, PathBuf>,
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
    /// registering a pane.
    pub fn fail_spawns_with(&self, error: PtyError) {
        self.state.lock().unwrap().spawn_error = Some(error);
    }

    /// Make [`resize`](Self::resize) fail for `pane` with `error` instead of
    /// recording. Other panes still record their resizes.
    pub fn fail_resizes_on(&self, pane: PaneId, error: PtyError) {
        self.state.lock().unwrap().resize_error = Some((pane, error));
    }

    /// Make [`write`](Self::write) fail for `pane` with `error` instead of
    /// recording.
    pub fn fail_writes_on(&self, pane: PaneId, error: PtyError) {
        self.state.lock().unwrap().write_error = Some((pane, error));
    }

    /// Make [`live_cwd`](Self::live_cwd) answer `cwd` for `pane`, as the OS
    /// reports the child's current directory.
    pub fn set_live_cwd(&self, pane: PaneId, cwd: impl Into<PathBuf>) {
        self.state
            .lock()
            .unwrap()
            .live_cwds
            .insert(pane, cwd.into());
    }

    /// Deliver `bytes` as a chunk of child output on `pane`'s handle.
    ///
    /// Returns [`PtyError::UnknownPane`] if the pane was never spawned. If the
    /// handle has been dropped the bytes are discarded and the call still
    /// returns `Ok(())`.
    pub fn push_output(&self, pane: PaneId, bytes: impl Into<Vec<u8>>) -> Result<()> {
        self.with_record(pane, |record| {
            if let Some(output_tx) = &record.output_tx {
                let _ = output_tx.send(bytes.into());
            }
        })
    }

    /// Close a pane's output channel, which models its PTY reaching EOF once the
    /// child is gone.
    ///
    /// The handle's output receiver then reports the channel closed. Later
    /// [`push_output`](Self::push_output) calls discard their bytes. Returns
    /// [`PtyError::UnknownPane`] if the pane was never spawned.
    pub fn close_output(&self, pane: PaneId) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let record = state
            .panes
            .get_mut(&pane)
            .ok_or(PtyError::UnknownPane { pane })?;
        record.output_tx = None;
        Ok(())
    }

    /// Fire `pane`'s child-exit with the given status on its handle.
    ///
    /// Returns [`PtyError::UnknownPane`] if the pane was never spawned.
    pub fn trigger_child_exit(&self, pane: PaneId, status: ExitStatus) -> Result<()> {
        self.with_record(pane, |record| {
            let _ = record.exit_tx.send(status);
        })
    }

    /// The panes spawned so far, in spawn order.
    #[must_use]
    pub fn spawned_panes(&self) -> Vec<PaneId> {
        self.state.lock().unwrap().spawn_order.clone()
    }

    /// Run `read` on a pane's record while the state lock is held, and return
    /// its value.
    ///
    /// Returns [`PtyError::UnknownPane`] if the pane was never spawned; `read`
    /// then does not run.
    fn with_record<T>(&self, pane: PaneId, read: impl FnOnce(&PaneRecord) -> T) -> Result<T> {
        let state = self.state.lock().unwrap();
        state
            .panes
            .get(&pane)
            .map(read)
            .ok_or(PtyError::UnknownPane { pane })
    }

    /// The [`SpawnSpec`] a pane was spawned with, or
    /// [`PtyError::UnknownPane`] if the pane was never spawned.
    pub fn spawn_spec(&self, pane: PaneId) -> Result<SpawnSpec> {
        self.with_record(pane, |record| record.spec.clone())
    }

    /// Every write made to a pane, in order, or [`PtyError::UnknownPane`] if
    /// the pane was never spawned.
    pub fn writes(&self, pane: PaneId) -> Result<Vec<Vec<u8>>> {
        self.with_record(pane, |record| record.writes.clone())
    }

    /// Every resize applied to a pane, in order — the spawn size first — or
    /// [`PtyError::UnknownPane`] if the pane was never spawned.
    pub fn resizes(&self, pane: PaneId) -> Result<Vec<PtySize>> {
        self.with_record(pane, |record| record.resizes.clone())
    }

    /// Every kill requested for a pane, in order, or [`PtyError::UnknownPane`]
    /// if the pane was never spawned.
    pub fn kills(&self, pane: PaneId) -> Result<Vec<KillPolicy>> {
        self.with_record(pane, |record| record.kills.clone())
    }
}

impl PtyBackend for FakePtyBackend {
    /// Record a pane spawn under the caller's `pane_id` and return a handle.
    ///
    /// Stores the spawn spec and the initial size in the pane's record, then
    /// appends `pane_id` to the spawn order. The handle carries the same id. It
    /// receives the output and exit status a test drives with
    /// [`push_output`](Self::push_output) and [`trigger_child_exit`](Self::trigger_child_exit).
    ///
    /// Returns the error set by [`fail_spawns_with`](Self::fail_spawns_with),
    /// when one is set.
    ///
    /// # Panics
    ///
    /// In a debug build, if `pane_id` is already live and no spawn error is set.
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
                output_tx: Some(output_tx),
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
    ///
    /// Returns the error set by [`fail_resizes_on`](Self::fail_resizes_on)
    /// when it names `pane`, else [`PtyError::UnknownPane`] if the pane was
    /// never spawned.
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
    ///
    /// Returns the error set by [`fail_writes_on`](Self::fail_writes_on) when
    /// it names `pane`, else [`PtyError::UnknownPane`] if the pane was never
    /// spawned.
    fn write(&self, pane: PaneId, bytes: &[u8]) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if let Some((failing, error)) = &state.write_error {
            if *failing == pane {
                return Err(error.clone());
            }
        }
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
    /// The pane's record stays, so a write or resize after a kill still
    /// records.
    ///
    /// Returns [`PtyError::UnknownPane`] if the pane was never spawned.
    fn kill(&self, pane: PaneId, kill_policy: KillPolicy) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let record = state
            .panes
            .get_mut(&pane)
            .ok_or(PtyError::UnknownPane { pane })?;
        record.kills.push(kill_policy);
        Ok(())
    }

    /// Answer the directory set via [`set_live_cwd`](Self::set_live_cwd), or
    /// `None` when the test set nothing for the pane.
    fn live_cwd(&self, pane: PaneId) -> Option<PathBuf> {
        self.state.lock().unwrap().live_cwds.get(&pane).cloned()
    }
}

#[cfg(test)]
mod tests;
