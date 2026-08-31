//! Genesis: seed the first session, tab, root pane, and client in code.
//!
//! The single-process local start assembles the first session with one tab
//! holding one shell pane, viewed by one client, directly through the
//! session-layer ops, then hands the pane's PTY to a forwarder like any other.
//!
//! The per-session server process seeds the same session and tab with no
//! client at all; the first attach adds one.
//!
//! A `--profile` start seeds one session holding every tab the profile file
//! defines, each with its own tree of panes.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;

use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::naming::{generate_name, NameKind};
use koshi_core::process::{KillPolicy, PtySize, ShellKind, SpawnSpec};
use koshi_layout::template::{LeafTemplate, ProfileTemplate, TemplateError, TerminalTemplate};
use koshi_layout::tree::LayoutNode;
use koshi_pty::backend::state::PtyHandle;
use koshi_pty::error::PtyError;
use koshi_session::client::{pane_viewport, Client, ClientOrigin, ClientRegistry};
use koshi_session::session::pane_ops::NewPaneSpec;
use koshi_session::session::state::Session;
use koshi_session::session::tab_ops;

use crate::runtime::command::{pane_spawn_sizes, size_root_pane};
use crate::runtime::spawn_env::koshi_env;
use crate::server::Server;

#[cfg(test)]
mod tests;

impl Server {
    /// Seed the first session/tab/root-pane/client for a local single-process
    /// start and return the client's id. Same start as
    /// [`bootstrap_local_named`](Self::bootstrap_local_named), with the
    /// session's display name generated here.
    pub fn bootstrap_local(
        &mut self,
        session_id: SessionId,
        viewport: Size,
        now: SystemTime,
    ) -> Result<ClientId, PtyError> {
        // This is the first session, so no existing name can collide.
        let session_name = generate_name(NameKind::Session, |_| false);
        self.bootstrap_local_named(session_id, session_name, viewport, now)
    }

    /// Seed the first session/tab/root-pane/client under a caller-chosen id and
    /// display name, and return the client's id. The session is registered
    /// under `session_id` (the caller mints it so the log file can be named for
    /// the session before genesis) and carries `session_name`. The root pane
    /// runs the default shell, sized to the middle pane region of `viewport`;
    /// `now` stamps attach/create.
    ///
    /// The child is spawned before any state is committed, so a failed launch
    /// leaves no session behind and surfaces as `Err`.
    pub fn bootstrap_local_named(
        &mut self,
        session_id: SessionId,
        session_name: String,
        viewport: Size,
        now: SystemTime,
    ) -> Result<ClientId, PtyError> {
        let client_id = ClientId::new();
        self.bootstrap_session(session_id, session_name, viewport, now, Some(client_id))?;
        Ok(client_id)
    }

    /// Seed the first session/tab/root-pane under a caller-chosen id and
    /// display name, optionally viewed by `client_id`. `Some` attaches that
    /// client to the new tab, focuses it on the root pane, and names it in the
    /// pane's identity vars; `None` seeds the session with no client, the
    /// headless start a later attach joins. Every other argument means what it
    /// does in [`bootstrap_local_named`](Self::bootstrap_local_named).
    ///
    /// The child is spawned before any state is committed, so a failed launch
    /// leaves no session behind and surfaces as `Err`.
    pub fn bootstrap_session(
        &mut self,
        session_id: SessionId,
        session_name: String,
        viewport: Size,
        now: SystemTime,
        client_id: Option<ClientId>,
    ) -> Result<(), PtyError> {
        let backend = Arc::clone(self.pty_backend());

        let tab_id = TabId::new();
        let pane_id = PaneId::new();

        // Chrome owns one row above and below the pane region.
        let spawn_size = size_root_pane(pane_id, pane_viewport(viewport), self.pane_sizing());

        // Launch the shell first: on failure nothing is registered. The spec
        // carries the pane's in-session identity vars in its env overlay.
        let mut spawn_spec = self.default_shell_spec(None, BTreeMap::new());
        spawn_spec.env.extend(koshi_env(
            session_id,
            client_id,
            pane_id,
            koshi_paths::runtime_dir().as_deref(),
        ));
        let handle = backend.spawn(pane_id, spawn_spec, spawn_size)?;

        // Assemble the session with its client, if any, viewing the tab we are
        // about to create, then commit the tab + root pane and focus the client
        // on it.
        let mut session = Session::new(session_id, session_name, now, ClientRegistry::new());
        attach_first_client(&mut session, client_id, viewport, tab_id, now);

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
            client_id,
            spec,
            now,
        );

        self.sessions.insert(session_id, session);
        self.park_pane_pty(pane_id, handle, spawn_size);
        self.render_scheduler.invalidate();

        Ok(())
    }

    /// Seed the first session from a `--profile` template: one session holding
    /// every tab the profile defines, each with its own tree of panes. The
    /// session is registered under the caller-supplied `session_id`, as in
    /// [`bootstrap_local`](Self::bootstrap_local).
    ///
    /// `client_id` names the one client viewing it, focused on the profile's
    /// starting tab and pane. `None` seeds the session with no client at all,
    /// which is what a session server started with nothing attached holds; the
    /// tabs still record which pane a client attaching later lands on.
    ///
    /// Every child is spawned before any state is committed, so a failed launch
    /// commits nothing and kills whatever it already spawned — the caller then
    /// falls back to a plain single-pane start. A profile that asks for a plugin
    /// pane cannot launch: there is no plugin host to fill it yet.
    ///
    /// # Panics
    ///
    /// Panics when `template` holds no tab, or when one of its tabs holds no
    /// leaf. [`parse_profile`](koshi_config::profile::parse_profile) rejects
    /// both.
    pub fn bootstrap_profile(
        &mut self,
        session_id: SessionId,
        template: ProfileTemplate,
        viewport: Size,
        now: SystemTime,
        client_id: Option<ClientId>,
    ) -> Result<(), ProfileLaunchError> {
        // This is the first session, so no existing name can collide.
        let session_name = generate_name(NameKind::Session, |_| false);
        self.bootstrap_profile_named(session_id, session_name, template, viewport, now, client_id)
    }

    /// [`bootstrap_profile`](Self::bootstrap_profile) under a caller-chosen
    /// display name, for a session server whose name was picked by the router
    /// that started it.
    ///
    /// # Panics
    ///
    /// Panics when `template` holds no tab, or when one of its tabs holds no
    /// leaf. [`parse_profile`](koshi_config::profile::parse_profile) rejects
    /// both.
    pub fn bootstrap_profile_named(
        &mut self,
        session_id: SessionId,
        session_name: String,
        template: ProfileTemplate,
        viewport: Size,
        now: SystemTime,
        client_id: Option<ClientId>,
    ) -> Result<(), ProfileLaunchError> {
        let backend = Arc::clone(self.pty_backend());
        let region = pane_viewport(viewport);

        // Plan every tab: a pane id per leaf, the spawn spec and the record
        // spec for each, and the live tree the ids fill. A plugin leaf has no
        // host, so the whole profile is refused before anything is spawned.
        let mut plans: Vec<TabPlan> = Vec::with_capacity(template.tabs.len());
        for tab in &template.tabs {
            let leaves = tab.root.leaves();
            let mut pane_ids = Vec::with_capacity(leaves.len());
            let mut spawns = Vec::with_capacity(leaves.len());
            let mut records = Vec::with_capacity(leaves.len());
            for leaf in leaves {
                let terminal = match leaf {
                    LeafTemplate::Terminal(terminal) => terminal,
                    LeafTemplate::Plugin(_) => return Err(ProfileLaunchError::PluginPane),
                };
                let (spawn, record) = self.profile_pane_specs(terminal);
                pane_ids.push(PaneId::new());
                spawns.push(spawn);
                records.push(record);
            }
            let layout = tab
                .root
                .to_layout_node(&pane_ids)
                .map_err(ProfileLaunchError::Template)?;
            plans.push(TabPlan {
                tab_id: TabId::new(),
                pane_ids,
                layout,
                spawns,
                records,
                focus_leaf: tab.focused_leaf,
            });
        }

        // Spawn every pane before committing anything. On any failure, kill
        // what was already spawned so no orphan child outlives the launch.
        let runtime_dir = koshi_paths::runtime_dir();
        let mut handles: Vec<(PaneId, PtyHandle, PtySize)> = Vec::new();
        let sizing = self.pane_sizing();
        for plan in &plans {
            // Size every pane against the tab's whole tree, so a multi-pane tab
            // spawns each child at its tiled slice rather than the full tab.
            let sizes = pane_spawn_sizes(&plan.layout, region, sizing);
            for (pane_id, spawn) in plan.pane_ids.iter().zip(&plan.spawns) {
                let spawn_size = sizes
                    .iter()
                    .find(|(id, _)| id == pane_id)
                    .map(|(_, size)| *size)
                    .expect("every planned pane id is a leaf of its own tab tree");
                let mut spawn_spec = spawn.clone();
                spawn_spec.env.extend(koshi_env(
                    session_id,
                    client_id,
                    *pane_id,
                    runtime_dir.as_deref(),
                ));
                match backend.spawn(*pane_id, spawn_spec, spawn_size) {
                    Ok(handle) => handles.push((*pane_id, handle, spawn_size)),
                    Err(err) => {
                        // Group-kill each already-spawned pane so a profile
                        // command that forked or backgrounded a child leaves no
                        // orphaned grandchild behind when the launch aborts.
                        for (spawned, _, _) in &handles {
                            let _ = backend.kill(*spawned, KillPolicy::Tree);
                        }
                        return Err(ProfileLaunchError::Spawn(err));
                    }
                }
            }
        }

        // Assemble the session and its client, if any, viewing the tab the
        // profile starts focused on.
        let focused_tab = template.focused_tab.min(plans.len().saturating_sub(1));
        let focused_tab_id = plans[focused_tab].tab_id;
        let mut session = Session::new(session_id, session_name, now, ClientRegistry::new());
        session.start_locked = template.locked;
        attach_first_client(&mut session, client_id, viewport, focused_tab_id, now);

        // Commit each tab; only the focused one moves the client onto it.
        for (index, plan) in plans.into_iter().enumerate() {
            let tab_name = generate_name(NameKind::Tab, |candidate| {
                session.tabs.values().any(|tab| tab.name() == candidate)
            });
            let _ = tab_ops::commit_profile_tab(
                &mut session,
                plan.tab_id,
                tab_ops::ProfileTab {
                    pane_ids: plan.pane_ids,
                    layout: plan.layout,
                    specs: plan.records,
                    focus_leaf: plan.focus_leaf,
                },
                tab_name,
                client_id,
                index == focused_tab,
                now,
            );
        }

        self.sessions.insert(session_id, session);
        for (pane_id, handle, size) in handles {
            self.park_pane_pty(pane_id, handle, size);
        }

        // Resize the focused tab's panes to the rects a client viewing it
        // solves; a tab no client views keeps the sizes its panes spawned at.
        // The resize events are dropped.
        let mut events = Vec::new();
        self.reflow_tab_if_viewed(backend.as_ref(), session_id, focused_tab_id, &mut events);
        self.render_scheduler.invalidate();

        Ok(())
    }
}

/// Attach `client_id` to a freshly seeded `session` as its only client,
/// viewing `tab_id`, sized to `viewport`, stamped `now`, with a generated
/// client label, colour `0`, and origin [`ClientOrigin::Local`]. A `None`
/// `client_id` attaches nobody and leaves `session` untouched.
///
/// The client is recorded with no pane area report; its pane area resolves to
/// the viewport minus two rows.
///
/// The client takes the session's starting lock: a session seeded from a
/// profile carrying `lock` attaches it in
/// [`LockMode::Locked`](koshi_core::lock::LockMode::Locked), and the flag is
/// spent, so no later attach is locked. Emits no event; the first frame this
/// client is painted into carries the mode.
fn attach_first_client(
    session: &mut Session,
    client_id: Option<ClientId>,
    viewport: Size,
    tab_id: TabId,
    now: SystemTime,
) {
    let Some(client_id) = client_id else {
        return;
    };
    // The session holds no other client, so no existing label can collide.
    let client_label = generate_name(NameKind::Client, |_| false);
    let mut client = Client::new(
        client_id,
        session.id,
        now,
        viewport,
        None,
        tab_id,
        ClientOrigin::Local,
        client_label,
        0,
    );
    if session.take_start_lock() {
        client.update_lock_mode(LockMode::Locked);
    }
    session.attach_client(client);
}

/// One tab's fully-planned genesis: the ids, tree, and specs its panes need.
struct TabPlan {
    /// The tab's id.
    tab_id: TabId,
    /// One pane id per leaf, in layout order.
    pane_ids: Vec<PaneId>,
    /// The live tree the ids fill.
    layout: LayoutNode,
    /// The spawn request for each pane, parallel to `pane_ids`.
    spawns: Vec<SpawnSpec>,
    /// The record spec for each pane, parallel to `pane_ids`.
    records: Vec<NewPaneSpec>,
    /// Index into `pane_ids` of the pane that starts focused.
    focus_leaf: usize,
}

impl Server {
    /// The spawn spec (what to launch) and record spec (what to remember) for
    /// one terminal leaf of a profile. A leaf with no command runs the default
    /// shell (honoring `terminal.default_shell`); either way the spec carries
    /// koshi's configured terminal identity, with the leaf's own `env` winning.
    fn profile_pane_specs(&self, terminal: &TerminalTemplate) -> (SpawnSpec, NewPaneSpec) {
        let cwd = terminal.cwd.clone();
        let env = self.terminal_identity_env(terminal.env.clone());
        match &terminal.command {
            Some(command) => {
                let spawn = SpawnSpec {
                    program: command.program.clone(),
                    args: command.args.clone(),
                    cwd: cwd.clone(),
                    env,
                    shell_kind: ShellKind::from_program(&command.program),
                };
                let record = NewPaneSpec {
                    cwd,
                    command: Some(spawn.clone()),
                };
                (spawn, record)
            }
            None => {
                let spawn = self.default_shell_spec(cwd.clone(), env);
                let record = NewPaneSpec { cwd, command: None };
                (spawn, record)
            }
        }
    }
}

/// Why a `--profile` launch could not be instantiated. The caller falls back to
/// a plain single-pane start and surfaces the reason.
#[derive(Debug)]
pub enum ProfileLaunchError {
    /// The profile asks for a plugin pane, which has no host to fill it yet.
    PluginPane,
    /// A tab's tree could not be built from its pane ids: the tree's leaf count
    /// and the pane-id count disagree.
    Template(TemplateError),
    /// A pane's child process failed to spawn.
    Spawn(PtyError),
}

impl std::fmt::Display for ProfileLaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PluginPane => {
                write!(f, "profile uses a plugin pane, which is not supported yet")
            }
            Self::Template(err) => write!(f, "profile layout could not be built: {err}"),
            Self::Spawn(err) => write!(f, "a profile pane failed to start: {err}"),
        }
    }
}
