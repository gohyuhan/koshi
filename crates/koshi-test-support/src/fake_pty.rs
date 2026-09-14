//! In-memory fake PTY (pseudo-terminal) backend.
//!
//! [`fake_pty::FakePtyBackend`] implements
//! [`koshi_pty::backend::state::PtyBackend`] in memory without starting a shell.
//! It records every pane spawn, input write, resize, and kill.
//!
//! Tests read records with `list_spawned_pane_ids`, `get_spawn_spec`, `list_pane_write_bytes`, `list_pane_sizes`,
//! and `list_pane_kill_policies`. They drive child output with `push_output` and child exit with
//! `trigger_child_exit`.
//!
//! A pane is live from `spawn_pane` until its first `kill_pane`. `kill_pane` returns
//! [`koshi_pty::error::PtyError::UnknownPane`] after that. `resize_pane` and
//! `write_pane_input`
//! return their configured failure when one names the pane, otherwise they
//! return [`koshi_pty::error::PtyError::UnknownPane`]. `spawn_pane` returns
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

/// Recorded calls and channels for one spawned pane.
struct PaneRecord {
    spawn_spec: SpawnSpec,
    resize_history: Vec<PtySize>,
    write_history: Vec<Vec<u8>>,
    kill_policies: Vec<KillPolicy>,
    /// `true` between [`spawn_pane`](FakePtyBackend::spawn_pane) and the pane's first
    /// [`kill_pane`](FakePtyBackend::kill_pane). A `false` record remains available to
    /// [`get_spawn_spec`](FakePtyBackend::get_spawn_spec),
    /// [`list_pane_write_bytes`](FakePtyBackend::list_pane_write_bytes),
    /// [`list_pane_sizes`](FakePtyBackend::list_pane_sizes), and
    /// [`list_pane_kill_policies`](FakePtyBackend::list_pane_kill_policies).
    /// It also accepts [`push_output`](FakePtyBackend::push_output) and
    /// [`trigger_child_exit`](FakePtyBackend::trigger_child_exit). `kill_pane` returns
    /// [`PtyError::UnknownPane`], while `resize_pane` and `write_pane_input` return a configured
    /// failure first and otherwise return [`PtyError::UnknownPane`].
    is_live: bool,
    /// The output channel's sending end. [`close_output`](FakePtyBackend::close_output)
    /// sets it to `None` and [`push_output`](FakePtyBackend::push_output) then
    /// discards its bytes.
    output_sender: Option<Sender<Vec<u8>>>,
    exit_sender: Sender<ExitStatus>,
}

/// Recorded backend calls protected by [`Mutex`]. Each backend method holds the
/// lock for the call.
#[derive(Default)]
struct FakePtyBackendState {
    pane_record_by_id: HashMap<PaneId, PaneRecord>,
    spawned_pane_ids: Vec<PaneId>,
    /// When set, every [`spawn_pane`](FakePtyBackend::spawn_pane) returns this error
    /// without registering a pane.
    spawn_error: Option<PtyError>,
    /// When set, [`resize_pane`](FakePtyBackend::resize_pane) returns this error for the
    /// named pane instead of recording.
    resize_error: Option<(PaneId, PtyError)>,
    /// When set, [`write_pane_input`](FakePtyBackend::write_pane_input) returns this error for the
    /// named pane instead of recording.
    write_error: Option<(PaneId, PtyError)>,
    /// Per-pane answers for
    /// [`find_live_working_directory`](FakePtyBackend::find_live_working_directory). An
    /// absent entry returns `None`.
    live_working_directories: HashMap<PaneId, PathBuf>,
}

/// An in-memory [`PtyBackend`] that records calls and lets tests drive output
/// and child exit.
#[derive(Default)]
pub struct FakePtyBackend {
    backend_state: Mutex<FakePtyBackendState>,
}

impl FakePtyBackend {
    /// Create a backend with no spawned panes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `spawn_error` as the result of every subsequent [`spawn_pane`](Self::spawn_pane).
    /// No pane is registered. Existing panes keep working, and a second call
    /// replaces the stored error.
    pub fn fail_spawns_with(&self, spawn_error: PtyError) {
        self.backend_state.lock().unwrap().spawn_error = Some(spawn_error);
    }

    /// Set [`resize_pane`](Self::resize_pane) to return `resize_error` for `pane_id` instead of
    /// recording. Other panes still record resizes. A second call replaces the
    /// stored pane and error.
    pub fn fail_resizes_on(&self, pane_id: PaneId, resize_error: PtyError) {
        self.backend_state.lock().unwrap().resize_error = Some((pane_id, resize_error));
    }

    /// Set [`write_pane_input`](Self::write_pane_input) to return `write_error` for `pane_id` instead of
    /// recording. Other panes still record writes. A second call replaces the
    /// stored pane and error.
    pub fn fail_writes_on(&self, pane_id: PaneId, write_error: PtyError) {
        self.backend_state.lock().unwrap().write_error = Some((pane_id, write_error));
    }

    /// Set [`find_live_working_directory`](Self::find_live_working_directory) to return
    /// `working_directory` for `pane_id`. The pane need not be spawned. A
    /// second call for the same pane replaces the directory.
    pub fn set_live_working_directory(
        &self,
        pane_id: PaneId,
        working_directory: impl Into<PathBuf>,
    ) {
        self.backend_state
            .lock()
            .unwrap()
            .live_working_directories
            .insert(pane_id, working_directory.into());
    }

    /// Deliver `output_bytes` as one child-output chunk on `pane_id`'s handle.
    ///
    /// Returns [`PtyError::UnknownPane`] if the pane was never spawned. If the
    /// handle was dropped or [`close_output`](Self::close_output) was called,
    /// the bytes are discarded and the call returns `Ok(())`.
    pub fn push_output(
        &self,
        pane_id: PaneId,
        output_bytes: impl Into<Vec<u8>>,
    ) -> Result<(), PtyError> {
        self.with_pane_record(pane_id, |pane_record| {
            if let Some(output_sender) = &pane_record.output_sender {
                let _ = output_sender.send(output_bytes.into());
            }
        })
    }

    /// Close a pane's output channel and make its handle report end of stream.
    ///
    /// [`push_output`](Self::push_output) calls after this discard their bytes.
    /// Repeated calls have no effect. Returns [`PtyError::UnknownPane`] if the
    /// pane was never spawned.
    pub fn close_output(&self, pane_id: PaneId) -> Result<(), PtyError> {
        let mut backend_state = self.backend_state.lock().unwrap();
        let pane_record = backend_state
            .pane_record_by_id
            .get_mut(&pane_id)
            .ok_or(PtyError::UnknownPane { pane_id })?;
        pane_record.output_sender = None;
        Ok(())
    }

    /// Deliver `exit_status` as a child-exit notification on `pane_id`'s handle.
    ///
    /// Each call queues one status, and the handle reads them in call order.
    /// Returns [`PtyError::UnknownPane`] if the pane was never spawned. If the
    /// handle was dropped, the status is discarded and the call returns
    /// `Ok(())`.
    pub fn trigger_child_exit(
        &self,
        pane_id: PaneId,
        exit_status: ExitStatus,
    ) -> Result<(), PtyError> {
        self.with_pane_record(pane_id, |pane_record| {
            let _ = pane_record.exit_sender.send(exit_status);
        })
    }

    /// Return pane IDs in spawn order, including killed panes. An ID reused
    /// after a kill appears once for each spawn.
    #[must_use]
    pub fn list_spawned_pane_ids(&self) -> Vec<PaneId> {
        self.backend_state.lock().unwrap().spawned_pane_ids.clone()
    }

    /// Run `record_reader` on a pane record while holding the backend-state lock.
    ///
    /// Returns [`PtyError::UnknownPane`] if the pane was never spawned;
    /// `record_reader` does not run in that case.
    fn with_pane_record<RecordValue>(
        &self,
        pane_id: PaneId,
        record_reader: impl FnOnce(&PaneRecord) -> RecordValue,
    ) -> Result<RecordValue, PtyError> {
        let backend_state = self.backend_state.lock().unwrap();
        backend_state
            .pane_record_by_id
            .get(&pane_id)
            .map(record_reader)
            .ok_or(PtyError::UnknownPane { pane_id })
    }

    /// Return the [`SpawnSpec`] used for a pane ID, or
    /// [`PtyError::UnknownPane`] if the pane was never spawned.
    pub fn get_spawn_spec(&self, pane_id: PaneId) -> Result<SpawnSpec, PtyError> {
        self.with_pane_record(pane_id, |pane_record| pane_record.spawn_spec.clone())
    }

    /// Return every byte write made to a pane in order, or
    /// [`PtyError::UnknownPane`] if the pane was never spawned.
    pub fn list_pane_write_bytes(&self, pane_id: PaneId) -> Result<Vec<Vec<u8>>, PtyError> {
        self.with_pane_record(pane_id, |pane_record| pane_record.write_history.clone())
    }

    /// Return every PTY size applied to a pane in order, with the spawn size
    /// first, or [`PtyError::UnknownPane`] if the pane was never spawned.
    pub fn list_pane_sizes(&self, pane_id: PaneId) -> Result<Vec<PtySize>, PtyError> {
        self.with_pane_record(pane_id, |pane_record| pane_record.resize_history.clone())
    }

    /// Return every kill policy requested for a pane in order, or
    /// [`PtyError::UnknownPane`] if the pane was never spawned.
    pub fn list_pane_kill_policies(&self, pane_id: PaneId) -> Result<Vec<KillPolicy>, PtyError> {
        self.with_pane_record(pane_id, |pane_record| pane_record.kill_policies.clone())
    }
}

impl PtyBackend for FakePtyBackend {
    /// Record a spawn under `pane_id` and return a handle.
    ///
    /// Stores `spawn_spec` and the initial `pty_size`, then appends `pane_id` to the spawn
    /// order. The handle carries the same `pane_id` and receives output and exit status
    /// sent by [`push_output`](Self::push_output) and
    /// [`trigger_child_exit`](Self::trigger_child_exit).
    ///
    /// A killed `pane_id` can be spawned again. The new spawn replaces the old record,
    /// replaces its spawn specification, clears its writes, resizes, and kills, and adds the pane ID
    /// to the spawn order again.
    ///
    /// # Errors
    ///
    /// Returns the error set by [`fail_spawns_with`](Self::fail_spawns_with)
    /// when one is set. Otherwise returns [`PtyError::Spawn`] with
    /// `pane <id> is already open` when `pane_id` is live. A refused spawn does
    /// not change the live record or handle.
    fn spawn_pane(
        &self,
        pane_id: PaneId,
        spawn_spec: SpawnSpec,
        pty_size: PtySize,
    ) -> Result<PtyHandle, PtyError> {
        let mut backend_state = self.backend_state.lock().unwrap();
        if let Some(spawn_error) = &backend_state.spawn_error {
            return Err(spawn_error.clone());
        }

        if backend_state
            .pane_record_by_id
            .get(&pane_id)
            .is_some_and(|pane_record| pane_record.is_live)
        {
            return Err(PtyError::Spawn {
                detail: format!("pane {pane_id} is already open"),
            });
        }

        let (pty_handle, output_sender, exit_sender) = PtyHandle::from_pane_id(pane_id);
        backend_state.pane_record_by_id.insert(
            pane_id,
            PaneRecord {
                spawn_spec,
                resize_history: vec![pty_size],
                write_history: Vec::new(),
                kill_policies: Vec::new(),
                is_live: true,
                output_sender: Some(output_sender),
                exit_sender,
            },
        );
        backend_state.spawned_pane_ids.push(pane_id);

        Ok(pty_handle)
    }

    /// Record a resize operation on a pane.
    ///
    /// Appends `pty_size` to the pane's resize history. The spawn size is already
    /// the first entry.
    ///
    /// Returns the error set by [`fail_resizes_on`](Self::fail_resizes_on) when
    /// it names `pane_id`, even if the pane was not spawned. Otherwise returns
    /// [`PtyError::UnknownPane`] when the pane was never spawned or was killed.
    fn resize_pane(&self, pane_id: PaneId, pty_size: PtySize) -> Result<(), PtyError> {
        let mut backend_state = self.backend_state.lock().unwrap();
        if let Some((failing_pane_id, resize_error)) = &backend_state.resize_error {
            if *failing_pane_id == pane_id {
                return Err(resize_error.clone());
            }
        }
        let pane_record = backend_state
            .pane_record_by_id
            .get_mut(&pane_id)
            .filter(|pane_record| pane_record.is_live)
            .ok_or(PtyError::UnknownPane { pane_id })?;
        pane_record.resize_history.push(pty_size);
        Ok(())
    }

    /// Record bytes written to a pane.
    ///
    /// Appends `write_bytes` to the pane's write history in call order; tests
    /// read it with [`list_pane_write_bytes`](Self::list_pane_write_bytes).
    ///
    /// Returns the error set by [`fail_writes_on`](Self::fail_writes_on) when it
    /// names `pane_id`, even if the pane was not spawned. Otherwise returns
    /// [`PtyError::UnknownPane`] when the pane was never spawned or was killed.
    fn write_pane_input(&self, pane_id: PaneId, write_bytes: &[u8]) -> Result<(), PtyError> {
        let mut backend_state = self.backend_state.lock().unwrap();
        if let Some((failing_pane_id, write_error)) = &backend_state.write_error {
            if *failing_pane_id == pane_id {
                return Err(write_error.clone());
            }
        }
        let pane_record = backend_state
            .pane_record_by_id
            .get_mut(&pane_id)
            .filter(|pane_record| pane_record.is_live)
            .ok_or(PtyError::UnknownPane { pane_id })?;
        pane_record.write_history.push(write_bytes.to_vec());
        Ok(())
    }

    /// Record a kill request and mark the pane as not live.
    ///
    /// Appends `kill_policy` to the kill history. Subsequent `kill_pane` calls return
    /// [`PtyError::UnknownPane`]. `resize_pane` and `write_pane_input` return a configured
    /// failure first and otherwise return [`PtyError::UnknownPane`].
    /// [`spawn_pane`](Self::spawn_pane) can reuse the id.
    /// The record remains available through [`get_spawn_spec`](Self::get_spawn_spec),
    /// [`list_pane_write_bytes`](Self::list_pane_write_bytes),
    /// [`list_pane_sizes`](Self::list_pane_sizes), and
    /// [`list_pane_kill_policies`](Self::list_pane_kill_policies). Its pane handle still receives data sent by
    /// [`push_output`](Self::push_output) and
    /// [`trigger_child_exit`](Self::trigger_child_exit).
    ///
    /// Returns [`PtyError::UnknownPane`] if the pane was never spawned or was
    /// already killed.
    fn kill_pane(&self, pane_id: PaneId, kill_policy: KillPolicy) -> Result<(), PtyError> {
        let mut backend_state = self.backend_state.lock().unwrap();
        let pane_record = backend_state
            .pane_record_by_id
            .get_mut(&pane_id)
            .filter(|pane_record| pane_record.is_live)
            .ok_or(PtyError::UnknownPane { pane_id })?;
        pane_record.kill_policies.push(kill_policy);
        pane_record.is_live = false;
        Ok(())
    }

    /// Return the directory set by
    /// [`set_live_working_directory`](Self::set_live_working_directory), or
    /// `None` when no directory was set for the pane.
    fn find_live_working_directory(&self, pane_id: PaneId) -> Option<PathBuf> {
        self.backend_state
            .lock()
            .unwrap()
            .live_working_directories
            .get(&pane_id)
            .cloned()
    }
}

#[cfg(test)]
mod tests;
