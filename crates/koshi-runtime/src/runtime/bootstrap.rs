//! Genesis: seed the first session, tab, root pane, and client in code.
//!
//! The command layer can't bootstrap from nothing — `NewTab`/`NewPane` reject
//! unless a client is already attached, and a client can't be built without a
//! tab id. So the single-process local start assembles the first session with
//! one tab holding one shell pane, viewed by one client, directly through the
//! session-layer ops, then hands the pane's PTY to a forwarder like any other.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;

use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::naming::{generate_name, NameKind};
use koshi_core::process::SpawnSpec;
use koshi_pty::error::PtyError;
use koshi_session::client::{Client, ClientRegistry};
use koshi_session::session::pane_ops::NewPaneSpec;
use koshi_session::session::state::Session;
use koshi_session::session::tab_ops;

use crate::runtime::command::size_root_pane;
use crate::runtime::render_schedule::InvalidationReason;
use crate::runtime::state::Runtime;

impl Runtime {
    /// Seed the first session/tab/root-pane/client for a local single-process
    /// start and return the client's id. The root pane runs the default shell,
    /// sized to `viewport`; `now` stamps the client attach and tab creation.
    ///
    /// The child is spawned before any state is committed, so a failed launch
    /// leaves no session behind and surfaces as `Err`.
    pub fn bootstrap_local(
        &mut self,
        viewport: Size,
        now: SystemTime,
    ) -> Result<ClientId, PtyError> {
        let backend = Arc::clone(self.pty_backend());

        let session_id = SessionId::new();
        let tab_id = TabId::new();
        let pane_id = PaneId::new();
        let client_id = ClientId::new();

        // Size the sole root pane against the whole viewport.
        let spawn_size = size_root_pane(pane_id, viewport);

        // Launch the shell first: on failure nothing is registered.
        let spawn_spec = SpawnSpec::default_shell(None, BTreeMap::new());
        let handle = backend.spawn(pane_id, spawn_spec, spawn_size)?;

        // Assemble the session with one client viewing the tab we are about to
        // create, then commit the tab + root pane and focus the client on it.
        // This is the first session, so no existing name can collide.
        let session_name = generate_name(NameKind::Session, |_| false);
        let mut session = Session::new(session_id, session_name, now, ClientRegistry::new());
        let client = Client::new(client_id, session_id, now, viewport, tab_id);
        session.attach_client(client);

        let tab_name = generate_name(NameKind::Tab, |candidate| {
            session.tabs.values().any(|tab| tab.name() == candidate)
        });
        let spec = NewPaneSpec {
            cwd: None,
            command: None,
        };
        let _ = tab_ops::commit_new_tab(
            &mut session,
            tab_id,
            pane_id,
            tab_name,
            Some(client_id),
            spec,
            now,
        );

        self.sessions.insert(session_id, session);
        Self::park_pane_pty(
            &mut self.pty_handles,
            &mut self.pty_sizes,
            &mut self.terminal_engines,
            &self.inbox_tx,
            pane_id,
            handle,
            spawn_size,
        );
        self.render_scheduler
            .invalidate(InvalidationReason::LayoutChanged);

        Ok(client_id)
    }
}
