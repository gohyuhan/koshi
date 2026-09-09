//! In-memory fake PTY (pseudo-terminal) backend.
//!
//! [`fake_pty::FakePtyBackend`] implements
//! [`koshi_pty::backend::state::PtyBackend`] in memory without starting a shell.
//! It records every spawn, write, resize, and kill.
//!
//! Tests read records with `spawned_panes`, `spawn_spec`, `writes`, `resizes`,
//! and `kills`. They drive child output with `push_output` and child exit with
//! `trigger_child_exit`.
//!
//! A pane is live from spawn until its first `kill`. `kill` returns
//! [`koshi_pty::error::PtyError::UnknownPane`] after that. `resize` and `write`
//! return their configured failure when one names the pane, otherwise they
//! return [`koshi_pty::error::PtyError::UnknownPane`]. `spawn` returns
//! [`koshi_pty::error::PtyError::Spawn`] for a live id and accepts an id that is
//! not live. Killed records remain readable.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Mutex;

pub use koshi_core::ids::PaneId;
pub use koshi_core::process::{ExitStatus, KillPolicy, PtySize, SpawnSpec};
pub use koshi_pty::backend::state::{PtyBackend, PtyHandle};
pub use koshi_pty::error::PtyError;

/// Recorded state and channels for one spawned pane.
struct PaneRecord {
    spec: SpawnSpec,
    resizes: Vec<PtySize>,
    writes: Vec<Vec<u8>>,
    kills: Vec<KillPolicy>,
    /// `true` between [`spawn`](FakePtyBackend::spawn) and the pane's first
    /// [`kill`](FakePtyBackend::kill). A `false` record remains available to
    /// [`spawn_spec`](FakePtyBackend::spawn_spec),
    /// [`writes`](FakePtyBackend::writes), [`resizes`](FakePtyBackend::resizes),
    /// and [`kills`](FakePtyBackend::kills).
    /// It also accepts [`push_output`](FakePtyBackend::push_output) and
    /// [`trigger_child_exit`](FakePtyBackend::trigger_child_exit). `kill` returns
    /// [`PtyError::UnknownPane`], while `resize` and `write` return a configured
    /// failure first and otherwise return [`PtyError::UnknownPane`].
    live: bool,
    /// The output channel's sending end. [`close_output`](FakePtyBackend::close_output)
    /// sets it to `None` and [`push_output`](FakePtyBackend::push_output) then
    /// discards its bytes.
    output_tx: Option<Sender<Vec<u8>>>,
    exit_tx: Sender<ExitStatus>,
}

/// Backend state protected by [`Mutex`]. Each backend method holds the lock
/// for the call.
#[derive(Default)]
struct State {
    panes: HashMap<PaneId, PaneRecord>,
    spawn_order: Vec<PaneId>,
    /// When set, every [`spawn`](FakePtyBackend::spawn) returns this error
    /// without registering a pane.
    spawn_error: Option<PtyError>,
    /// When set, [`resize`](FakePtyBackend::resize) returns this error for the
    /// named pane instead of recording.
    resize_error: Option<(PaneId, PtyError)>,
    /// When set, [`write`](FakePtyBackend::write) returns this error for the
    /// named pane instead of recording.
    write_error: Option<(PaneId, PtyError)>,
    /// Per-pane answers for [`live_cwd`](FakePtyBackend::live_cwd). An absent
    /// entry returns `None`.
    live_cwds: HashMap<PaneId, PathBuf>,
}

/// An in-memory [`PtyBackend`] that records calls and lets tests drive output
/// and child exit.
#[derive(Default)]
pub struct FakePtyBackend {
    state: Mutex<State>,
}

impl FakePtyBackend {
    /// Create a backend with no spawned panes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `error` as the result of every subsequent [`spawn`](Self::spawn).
    /// No pane is registered. Existing panes keep working, and a second call
    /// replaces the stored error.
    pub fn fail_spawns_with(&self, error: PtyError) {
        self.state.lock().unwrap().spawn_error = Some(error);
    }

    /// Set [`resize`](Self::resize) to return `error` for `pane` instead of
    /// recording. Other panes still record resizes. A second call replaces the
    /// stored pane and error.
    pub fn fail_resizes_on(&self, pane: PaneId, error: PtyError) {
        self.state.lock().unwrap().resize_error = Some((pane, error));
    }

    /// Set [`write`](Self::write) to return `error` for `pane` instead of
    /// recording. Other panes still record writes. A second call replaces the
    /// stored pane and error.
    pub fn fail_writes_on(&self, pane: PaneId, error: PtyError) {
        self.state.lock().unwrap().write_error = Some((pane, error));
    }

    /// Set [`live_cwd`](Self::live_cwd) to return `cwd` for `pane`. The pane
    /// need not be spawned. A second call for the same pane replaces `cwd`.
    pub fn set_live_cwd(&self, pane: PaneId, cwd: impl Into<PathBuf>) {
        self.state
            .lock()
            .unwrap()
            .live_cwds
            .insert(pane, cwd.into());
    }

    /// Deliver `bytes` as one child-output chunk on `pane`'s handle.
    ///
    /// Returns [`PtyError::UnknownPane`] if the pane was never spawned. If the
    /// handle was dropped or [`close_output`](Self::close_output) was called,
    /// the bytes are discarded and the call returns `Ok(())`.
    pub fn push_output(&self, pane: PaneId, bytes: impl Into<Vec<u8>>) -> Result<(), PtyError> {
        self.with_record(pane, |record| {
            if let Some(output_tx) = &record.output_tx {
                let _ = output_tx.send(bytes.into());
            }
        })
    }

    /// Close a pane's output channel and make its handle report end of stream.
    ///
    /// [`push_output`](Self::push_output) calls after this discard their bytes.
    /// Repeated calls have no effect. Returns [`PtyError::UnknownPane`] if the
    /// pane was never spawned.
    pub fn close_output(&self, pane: PaneId) -> Result<(), PtyError> {
        let mut state = self.state.lock().unwrap();
        let record = state
            .panes
            .get_mut(&pane)
            .ok_or(PtyError::UnknownPane { pane })?;
        record.output_tx = None;
        Ok(())
    }

    /// Deliver `status` as a child-exit notification on `pane`'s handle.
    ///
    /// Each call queues one status, and the handle reads them in call order.
    /// Returns [`PtyError::UnknownPane`] if the pane was never spawned. If the
    /// handle was dropped, the status is discarded and the call returns
    /// `Ok(())`.
    pub fn trigger_child_exit(&self, pane: PaneId, status: ExitStatus) -> Result<(), PtyError> {
        self.with_record(pane, |record| {
            let _ = record.exit_tx.send(status);
        })
    }

    /// Return pane ids in spawn order, including killed panes. An id reused
    /// after a kill appears once for each spawn.
    #[must_use]
    pub fn spawned_panes(&self) -> Vec<PaneId> {
        self.state.lock().unwrap().spawn_order.clone()
    }

    /// Run `read` on a pane record while holding the state lock.
    ///
    /// Returns [`PtyError::UnknownPane`] if the pane was never spawned; `read`
    /// does not run in that case.
    fn with_record<T>(
        &self,
        pane: PaneId,
        read: impl FnOnce(&PaneRecord) -> T,
    ) -> Result<T, PtyError> {
        let state = self.state.lock().unwrap();
        state
            .panes
            .get(&pane)
            .map(read)
            .ok_or(PtyError::UnknownPane { pane })
    }

    /// Return the [`SpawnSpec`] used for a pane, or
    /// [`PtyError::UnknownPane`] if the pane was never spawned.
    pub fn spawn_spec(&self, pane: PaneId) -> Result<SpawnSpec, PtyError> {
        self.with_record(pane, |record| record.spec.clone())
    }

    /// Return every write made to a pane in order, or
    /// [`PtyError::UnknownPane`] if the pane was never spawned.
    pub fn writes(&self, pane: PaneId) -> Result<Vec<Vec<u8>>, PtyError> {
        self.with_record(pane, |record| record.writes.clone())
    }

    /// Return every resize applied to a pane in order, with the spawn size
    /// first, or [`PtyError::UnknownPane`] if the pane was never spawned.
    pub fn resizes(&self, pane: PaneId) -> Result<Vec<PtySize>, PtyError> {
        self.with_record(pane, |record| record.resizes.clone())
    }

    /// Return every kill requested for a pane in order, or
    /// [`PtyError::UnknownPane`] if the pane was never spawned.
    pub fn kills(&self, pane: PaneId) -> Result<Vec<KillPolicy>, PtyError> {
        self.with_record(pane, |record| record.kills.clone())
    }
}

impl PtyBackend for FakePtyBackend {
    /// Record a spawn under `pane_id` and return a handle.
    ///
    /// Stores `spec` and the initial `size`, then appends `pane_id` to the spawn
    /// order. The handle carries the same id and receives output and exit status
    /// sent by [`push_output`](Self::push_output) and
    /// [`trigger_child_exit`](Self::trigger_child_exit).
    ///
    /// A killed id can be spawned again. The new spawn replaces the old record,
    /// replaces its spec, clears its writes, resizes, and kills, and adds the id
    /// to the spawn order again.
    ///
    /// # Errors
    ///
    /// Returns the error set by [`fail_spawns_with`](Self::fail_spawns_with)
    /// when one is set. Otherwise returns [`PtyError::Spawn`] with
    /// `pane <id> is already open` when `pane_id` is live. A refused spawn does
    /// not change the live record or handle.
    fn spawn(
        &self,
        pane_id: PaneId,
        spec: SpawnSpec,
        size: PtySize,
    ) -> Result<PtyHandle, PtyError> {
        let mut state = self.state.lock().unwrap();
        if let Some(error) = &state.spawn_error {
            return Err(error.clone());
        }

        if state.panes.get(&pane_id).is_some_and(|record| record.live) {
            return Err(PtyError::Spawn {
                detail: format!("pane {pane_id} is already open"),
            });
        }

        let (handle, output_tx, exit_tx) = PtyHandle::new(pane_id);
        state.panes.insert(
            pane_id,
            PaneRecord {
                spec,
                resizes: vec![size],
                writes: Vec::new(),
                kills: Vec::new(),
                live: true,
                output_tx: Some(output_tx),
                exit_tx,
            },
        );
        state.spawn_order.push(pane_id);

        Ok(handle)
    }

    /// Record a resize operation on a pane.
    ///
    /// Appends `size` to the pane's resize history. The spawn size is already
    /// the first entry.
    ///
    /// Returns the error set by [`fail_resizes_on`](Self::fail_resizes_on) when
    /// it names `pane`, even if the pane was not spawned. Otherwise returns
    /// [`PtyError::UnknownPane`] when the pane was never spawned or was killed.
    fn resize(&self, pane: PaneId, size: PtySize) -> Result<(), PtyError> {
        let mut state = self.state.lock().unwrap();
        if let Some((failing, error)) = &state.resize_error {
            if *failing == pane {
                return Err(error.clone());
            }
        }
        let record = state
            .panes
            .get_mut(&pane)
            .filter(|record| record.live)
            .ok_or(PtyError::UnknownPane { pane })?;
        record.resizes.push(size);
        Ok(())
    }

    /// Record bytes written to a pane.
    ///
    /// Appends `bytes` to the pane's write history in call order; tests read it
    /// with [`writes`](Self::writes).
    ///
    /// Returns the error set by [`fail_writes_on`](Self::fail_writes_on) when it
    /// names `pane`, even if the pane was not spawned. Otherwise returns
    /// [`PtyError::UnknownPane`] when the pane was never spawned or was killed.
    fn write(&self, pane: PaneId, bytes: &[u8]) -> Result<(), PtyError> {
        let mut state = self.state.lock().unwrap();
        if let Some((failing, error)) = &state.write_error {
            if *failing == pane {
                return Err(error.clone());
            }
        }
        let record = state
            .panes
            .get_mut(&pane)
            .filter(|record| record.live)
            .ok_or(PtyError::UnknownPane { pane })?;
        record.writes.push(bytes.to_vec());
        Ok(())
    }

    /// Record a kill request and mark the pane as not live.
    ///
    /// Appends `kill_policy` to the kill history. Subsequent `kill` calls return
    /// [`PtyError::UnknownPane`]. `resize` and `write` return a configured
    /// failure first and otherwise return [`PtyError::UnknownPane`].
    /// [`spawn`](Self::spawn) can reuse the id.
    /// The record remains available through [`spawn_spec`](Self::spawn_spec),
    /// [`writes`](Self::writes), [`resizes`](Self::resizes), and
    /// [`kills`](Self::kills). Its handle still receives data sent by
    /// [`push_output`](Self::push_output) and
    /// [`trigger_child_exit`](Self::trigger_child_exit).
    ///
    /// Returns [`PtyError::UnknownPane`] if the pane was never spawned or was
    /// already killed.
    fn kill(&self, pane: PaneId, kill_policy: KillPolicy) -> Result<(), PtyError> {
        let mut state = self.state.lock().unwrap();
        let record = state
            .panes
            .get_mut(&pane)
            .filter(|record| record.live)
            .ok_or(PtyError::UnknownPane { pane })?;
        record.kills.push(kill_policy);
        record.live = false;
        Ok(())
    }

    /// Return the directory set by [`set_live_cwd`](Self::set_live_cwd), or
    /// `None` when no directory was set for the pane.
    fn live_cwd(&self, pane: PaneId) -> Option<PathBuf> {
        self.state.lock().unwrap().live_cwds.get(&pane).cloned()
    }
}

#[cfg(test)]
mod tests;
