//! In-memory fake PTY backend.
//!
//! Layout, session, and runtime tests need to exercise pane spawning, writing,
//! resizing, and child-exit handling without launching real shells: real
//! processes make tests slow, platform-dependent, and non-deterministic.
//! [`FakePtyBackend`] satisfies the full PTY backend surface entirely in
//! memory, capturing every call so a test can assert on it and driving output
//! and child-exit on demand.
//!
//! ## Locally declared trait
//!
//! The canonical `PtyBackend` trait is owned by `tile-pty`, which lands in a
//! later task. Depending on `tile-pty` from here would invert the layering
//! (test-support sits below the PTY crate), so this module declares the trait
//! locally against the same `tile-core` types. When `tile-pty` lands, the local
//! declaration gives way to implementing the canonical trait — but
//! [`FakePtyBackend`] itself stays: it is the permanent test double, and its
//! capture/drive surface does not change.

use std::collections::HashMap;
use std::fmt;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;

pub use tile_core::ids::PaneId;
pub use tile_core::process::{KillPolicy, PtySize, SpawnSpec};

/// Exit status reported for a spawned child.
///
/// Temporary stand-in until the PTY layer lands its canonical type; it carries
/// the single fact tests assert on — the process exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus {
    /// The child's exit code (`0` is success by convention).
    pub code: i32,
}

/// Errors a [`PtyBackend`] operation can return.
///
/// Temporary stand-in for the PTY layer's error type. The fake only fails one
/// way: addressing a pane that was never spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyError {
    /// No pane with this id is known to the backend.
    UnknownPane(PaneId),
}

impl fmt::Display for PtyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PtyError::UnknownPane(pane) => write!(f, "unknown pane: {pane}"),
        }
    }
}

impl std::error::Error for PtyError {}

/// Result alias for backend operations.
pub type Result<T> = std::result::Result<T, PtyError>;

/// The PTY backend surface, declared locally until `tile-pty` owns the
/// canonical trait (see module docs).
pub trait PtyBackend {
    /// Spawn a child in a new PTY of the given size, returning a handle that
    /// streams its output and exit status.
    fn spawn(&self, spec: SpawnSpec, size: PtySize) -> Result<PtyHandle>;
    /// Resize an existing pane's PTY.
    fn resize(&self, pane: PaneId, size: PtySize) -> Result<()>;
    /// Write bytes to a pane's child stdin.
    fn write(&self, pane: PaneId, bytes: &[u8]) -> Result<()>;
    /// Terminate a pane's child according to `policy`.
    fn kill(&self, pane: PaneId, policy: KillPolicy) -> Result<()>;
}

/// The caller's view of one spawned pane: its id plus the streams the backend
/// delivers child output and exit status on.
///
/// Output and exit are non-blocking reads so a single-threaded test can spawn,
/// drive, and assert without scheduling. Dropping the handle does not forget the
/// pane: the backend keeps its capture; the handle only owns the receiving ends
/// of the two streams.
pub struct PtyHandle {
    pane_id: PaneId,
    output: Receiver<Vec<u8>>,
    exit: Receiver<ExitStatus>,
}

impl PtyHandle {
    /// The pane this handle addresses.
    #[must_use]
    pub fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    /// The next chunk of child output, or `None` if none is pending.
    ///
    /// Each chunk corresponds to one [`FakePtyBackend::push_output`] call, in
    /// order.
    pub fn try_read_output(&self) -> Option<Vec<u8>> {
        self.output.try_recv().ok()
    }

    /// The child's exit status, or `None` if it has not exited yet.
    ///
    /// Set by [`FakePtyBackend::trigger_child_exit`].
    pub fn try_exit_status(&self) -> Option<ExitStatus> {
        self.exit.try_recv().ok()
    }
}

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

    /// Deliver `bytes` as a chunk of child output on `pane`'s handle.
    ///
    /// Returns [`PtyError::UnknownPane`] if the pane was never spawned. If the
    /// handle has been dropped the bytes are discarded silently, mirroring a
    /// real child writing to a closed reader.
    pub fn push_output(&self, pane: PaneId, bytes: impl Into<Vec<u8>>) -> Result<()> {
        let state = self.state.lock().unwrap();
        let record = state.panes.get(&pane).ok_or(PtyError::UnknownPane(pane))?;
        let _ = record.output_tx.send(bytes.into());
        Ok(())
    }

    /// Fire `pane`'s child-exit with the given status on its handle.
    ///
    /// Returns [`PtyError::UnknownPane`] if the pane was never spawned.
    pub fn trigger_child_exit(&self, pane: PaneId, status: ExitStatus) -> Result<()> {
        let state = self.state.lock().unwrap();
        let record = state.panes.get(&pane).ok_or(PtyError::UnknownPane(pane))?;
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
            .ok_or(PtyError::UnknownPane(pane))
    }

    /// Every write made to a pane, in order.
    pub fn writes(&self, pane: PaneId) -> Result<Vec<Vec<u8>>> {
        let state = self.state.lock().unwrap();
        state
            .panes
            .get(&pane)
            .map(|r| r.writes.clone())
            .ok_or(PtyError::UnknownPane(pane))
    }

    /// Every resize applied to a pane, in order.
    pub fn resizes(&self, pane: PaneId) -> Result<Vec<PtySize>> {
        let state = self.state.lock().unwrap();
        state
            .panes
            .get(&pane)
            .map(|r| r.resizes.clone())
            .ok_or(PtyError::UnknownPane(pane))
    }

    /// Every kill requested for a pane, in order.
    pub fn kills(&self, pane: PaneId) -> Result<Vec<KillPolicy>> {
        let state = self.state.lock().unwrap();
        state
            .panes
            .get(&pane)
            .map(|r| r.kills.clone())
            .ok_or(PtyError::UnknownPane(pane))
    }
}

impl PtyBackend for FakePtyBackend {
    fn spawn(&self, spec: SpawnSpec, size: PtySize) -> Result<PtyHandle> {
        let pane_id = PaneId::new();
        let (output_tx, output) = mpsc::channel();
        let (exit_tx, exit) = mpsc::channel();

        let mut state = self.state.lock().unwrap();
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

        Ok(PtyHandle {
            pane_id,
            output,
            exit,
        })
    }

    fn resize(&self, pane: PaneId, size: PtySize) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let record = state
            .panes
            .get_mut(&pane)
            .ok_or(PtyError::UnknownPane(pane))?;
        record.resizes.push(size);
        Ok(())
    }

    fn write(&self, pane: PaneId, bytes: &[u8]) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let record = state
            .panes
            .get_mut(&pane)
            .ok_or(PtyError::UnknownPane(pane))?;
        record.writes.push(bytes.to_vec());
        Ok(())
    }

    fn kill(&self, pane: PaneId, policy: KillPolicy) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        let record = state
            .panes
            .get_mut(&pane)
            .ok_or(PtyError::UnknownPane(pane))?;
        record.kills.push(policy);
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
        let handle = pty.spawn(spec(), size(80, 24)).unwrap();
        let pane = handle.pane_id();

        assert_eq!(pty.spawned_panes(), vec![pane]);
        assert_eq!(pty.spawn_spec(pane).unwrap(), spec());
        assert_eq!(pty.resizes(pane).unwrap(), vec![size(80, 24)]);
    }

    #[test]
    fn output_is_delivered_in_order() {
        let pty = FakePtyBackend::new();
        let handle = pty.spawn(spec(), size(80, 24)).unwrap();
        let pane = handle.pane_id();

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
        let handle = pty.spawn(spec(), size(80, 24)).unwrap();
        let pane = handle.pane_id();

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
        let handle = pty.spawn(spec(), size(80, 24)).unwrap();
        let pane = handle.pane_id();

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
        let handle = pty.spawn(spec(), size(80, 24)).unwrap();
        let pane = handle.pane_id();

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
        let handle = pty.spawn(spec(), size(80, 24)).unwrap();
        let pane = handle.pane_id();

        assert!(handle.try_exit_status().is_none());
        pty.trigger_child_exit(pane, ExitStatus { code: 0 })
            .unwrap();

        assert_eq!(handle.try_exit_status(), Some(ExitStatus { code: 0 }));
        assert!(handle.try_exit_status().is_none());
    }

    #[test]
    fn operations_on_unknown_pane_error() {
        let pty = FakePtyBackend::new();
        let ghost = PaneId::new();

        assert_eq!(
            pty.resize(ghost, size(80, 24)),
            Err(PtyError::UnknownPane(ghost))
        );
        assert_eq!(pty.write(ghost, b"x"), Err(PtyError::UnknownPane(ghost)));
        assert_eq!(
            pty.kill(ghost, KillPolicy::Force),
            Err(PtyError::UnknownPane(ghost))
        );
        assert_eq!(
            pty.push_output(ghost, b"x".to_vec()),
            Err(PtyError::UnknownPane(ghost))
        );
        assert_eq!(
            pty.trigger_child_exit(ghost, ExitStatus { code: 0 }),
            Err(PtyError::UnknownPane(ghost))
        );
    }

    #[test]
    fn multiple_panes_are_isolated() {
        let pty = FakePtyBackend::new();
        let a = pty.spawn(spec(), size(80, 24)).unwrap();
        let b = pty.spawn(spec(), size(80, 24)).unwrap();

        pty.write(a.pane_id(), b"a").unwrap();
        pty.push_output(b.pane_id(), b"b".to_vec()).unwrap();

        assert_eq!(pty.writes(a.pane_id()).unwrap(), vec![b"a".to_vec()]);
        assert!(pty.writes(b.pane_id()).unwrap().is_empty());
        assert!(a.try_read_output().is_none());
        assert_eq!(b.try_read_output(), Some(b"b".to_vec()));
        assert_eq!(pty.spawned_panes(), vec![a.pane_id(), b.pane_id()]);
    }
}
