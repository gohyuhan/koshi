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
use koshi_core::process::{KillPolicy, PtySize, ShellKind, SpawnSpec};
use koshi_layout::template::{LeafTemplate, ProfileTemplate, TemplateError, TerminalTemplate};
use koshi_layout::tree::LayoutNode;
use koshi_pty::backend::state::PtyHandle;
use koshi_pty::error::PtyError;
use koshi_session::client::{pane_viewport, Client, ClientRegistry};
use koshi_session::session::pane_ops::NewPaneSpec;
use koshi_session::session::state::Session;
use koshi_session::session::tab_ops;

use crate::runtime::command::{pane_spawn_sizes, size_root_pane};
use crate::runtime::render_schedule::InvalidationReason;
use crate::runtime::state::Runtime;

#[cfg(test)]
mod tests;

impl Runtime {
    /// Seed the first session/tab/root-pane/client for a local single-process
    /// start and return the client's id. The root pane runs the default shell,
    /// sized to the middle pane region of `viewport`; `now` stamps attach/create.
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

        // Chrome owns one row above and below the pane region.
        let spawn_size =
            size_root_pane(pane_id, pane_viewport(viewport), self.effective_pane_min());

        // Launch the shell first: on failure nothing is registered.
        let spawn_spec = self.default_shell_spec(None, BTreeMap::new());
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
        self.park_pane_pty(pane_id, handle, spawn_size);
        self.render_scheduler
            .invalidate(InvalidationReason::LayoutChanged);

        Ok(client_id)
    }

    /// Seed the first session from a `--profile` template: one session holding
    /// every tab the profile defines, each with its own tree of panes, viewed
    /// by one client focused on the profile's starting tab and pane.
    ///
    /// Every child is spawned before any state is committed, so a failed launch
    /// commits nothing and kills whatever it already spawned — the caller then
    /// falls back to a plain single-pane start. A profile that asks for a plugin
    /// pane cannot launch: there is no plugin host to fill it yet.
    pub fn bootstrap_profile(
        &mut self,
        template: ProfileTemplate,
        viewport: Size,
        now: SystemTime,
    ) -> Result<ClientId, ProfileLaunchError> {
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
        let mut handles: Vec<(PaneId, PtyHandle, PtySize)> = Vec::new();
        let pane_min = self.effective_pane_min();
        for plan in &plans {
            // Size every pane against the tab's whole tree, so a multi-pane tab
            // spawns each child at its tiled slice rather than the full tab.
            let sizes = pane_spawn_sizes(&plan.layout, region, pane_min);
            for (pane_id, spawn) in plan.pane_ids.iter().zip(&plan.spawns) {
                let spawn_size = sizes
                    .iter()
                    .find(|(id, _)| id == pane_id)
                    .map(|(_, size)| *size)
                    .expect("every planned pane id is a leaf of its own tab tree");
                match backend.spawn(*pane_id, spawn.clone(), spawn_size) {
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

        // Assemble the session and its one client, viewing the tab the profile
        // starts focused on.
        let session_id = SessionId::new();
        let focused_tab = template.focused_tab.min(plans.len().saturating_sub(1));
        let focused_tab_id = plans[focused_tab].tab_id;
        let client_id = ClientId::new();
        let session_name = generate_name(NameKind::Session, |_| false);
        let mut session = Session::new(session_id, session_name, now, ClientRegistry::new());
        let client = Client::new(client_id, session_id, now, viewport, focused_tab_id);
        session.attach_client(client);

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
                Some(client_id),
                index == focused_tab,
                now,
            );
        }

        self.sessions.insert(session_id, session);
        for (pane_id, handle, size) in handles {
            self.park_pane_pty(pane_id, handle, size);
        }

        // Size the focused tab's panes to their solved rects; other tabs reflow
        // the first time they are viewed.
        let mut events = Vec::new();
        self.reflow_tab_if_viewed(backend.as_ref(), session_id, focused_tab_id, &mut events);
        self.render_scheduler
            .invalidate(InvalidationReason::LayoutChanged);

        Ok(client_id)
    }
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

impl Runtime {
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
    /// A tab's tree could not be built from its pane ids — an internal count
    /// mismatch that should not happen once the profile parsed.
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
