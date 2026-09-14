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

use crate::runtime::command::{compute_pane_spawn_sizes, size_root_pane};
use crate::runtime::spawn_env::build_koshi_environment;
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
    /// pane's identity vars; `None` seeds the session with no client, and a
    /// subsequent attach joins the headless start. Every other argument means what it
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
        let backend = Arc::clone(self.get_pty_backend());

        let tab_id = TabId::new();
        let pane_id = PaneId::new();

        // Chrome owns one row above and below the pane region.
        let spawn_size = size_root_pane(pane_id, pane_viewport(viewport), self.get_pane_sizing());

        // Launch the shell first: on failure nothing is registered. The spec
        // carries the pane's in-session identity vars in its env overlay.
        let mut spawn_spec = self.build_default_shell_spec(None, BTreeMap::new());
        spawn_spec
            .environment_variables
            .extend(build_koshi_environment(
                session_id,
                client_id,
                pane_id,
                koshi_paths::resolve_runtime_directory().as_deref(),
            ));
        let handle = backend.spawn_pane(pane_id, spawn_spec, spawn_size)?;

        // Assemble the session with its client, if any, viewing the tab we are
        // about to create, then commit the tab + root pane and focus the client
        // on it.
        let mut session = Session::from_identity_and_client_registry(
            session_id,
            session_name,
            now,
            ClientRegistry::new(),
        );
        attach_first_client(&mut session, client_id, viewport, tab_id, now);

        let tab_name = generate_name(NameKind::Tab, |candidate| {
            session
                .tabs
                .values()
                .any(|tab| tab.get_tab_name() == candidate)
        });
        let spec = NewPaneSpec {
            working_directory: None,
            spawn_spec: None,
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

        self.session_by_id.insert(session_id, session);
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
        let backend = Arc::clone(self.get_pty_backend());
        let region = pane_viewport(viewport);

        // Plan every tab: a pane id per leaf, the spawn spec and the record
        // spec for each, and the live tree the ids fill. A plugin leaf has no
        // host, so the whole profile is refused before anything is spawned.
        let mut profile_tab_plans: Vec<ProfileTabPlan> = Vec::with_capacity(template.tabs.len());
        for tab_template in &template.tabs {
            let leaf_templates = tab_template.root.list_leaf_templates();
            let mut pane_ids = Vec::with_capacity(leaf_templates.len());
            let mut spawn_specs = Vec::with_capacity(leaf_templates.len());
            let mut pane_specs = Vec::with_capacity(leaf_templates.len());
            for leaf_template in leaf_templates {
                let terminal_template = match leaf_template {
                    LeafTemplate::Terminal(terminal_template) => terminal_template,
                    LeafTemplate::Plugin(_) => return Err(ProfileLaunchError::PluginPane),
                };
                let (spawn_spec, pane_spec) = self.profile_pane_specs(terminal_template);
                pane_ids.push(PaneId::new());
                spawn_specs.push(spawn_spec);
                pane_specs.push(pane_spec);
            }
            let layout_tree = tab_template
                .root
                .build_layout_node(&pane_ids)
                .map_err(ProfileLaunchError::Template)?;
            profile_tab_plans.push(ProfileTabPlan {
                tab_id: TabId::new(),
                pane_ids,
                layout_tree,
                spawn_specs,
                pane_specs,
                focused_leaf_index: tab_template.focused_leaf_index,
            });
        }

        // Spawn every pane before committing anything. On any failure, kill
        // what was already spawned so no orphan child outlives the launch.
        let runtime_directory = koshi_paths::resolve_runtime_directory();
        let mut spawned_pty_handles: Vec<(PaneId, PtyHandle, PtySize)> = Vec::new();
        let sizing = self.get_pane_sizing();
        for tab_plan in &profile_tab_plans {
            // Size every pane against the tab's whole tree, so a multi-pane tab
            // spawns each child at its tiled slice rather than the full tab.
            let pane_spawn_sizes = compute_pane_spawn_sizes(&tab_plan.layout_tree, region, sizing);
            for (pane_id, spawn_spec) in tab_plan.pane_ids.iter().zip(&tab_plan.spawn_specs) {
                let pane_size = pane_spawn_sizes
                    .iter()
                    .find(|(candidate_pane_id, _)| candidate_pane_id == pane_id)
                    .map(|(_, size)| *size)
                    .expect("every planned pane id is a leaf of its own tab tree");
                let mut pane_spawn_spec = spawn_spec.clone();
                pane_spawn_spec
                    .environment_variables
                    .extend(build_koshi_environment(
                        session_id,
                        client_id,
                        *pane_id,
                        runtime_directory.as_deref(),
                    ));
                match backend.spawn_pane(*pane_id, pane_spawn_spec, pane_size) {
                    Ok(pty_handle) => spawned_pty_handles.push((*pane_id, pty_handle, pane_size)),
                    Err(spawn_error) => {
                        // Group-kill each already-spawned pane so a profile
                        // command that forked or backgrounded a child leaves no
                        // orphaned grandchild behind when the launch aborts.
                        for (spawned_pane_id, _, _) in &spawned_pty_handles {
                            let _ = backend.kill_pane(*spawned_pane_id, KillPolicy::Tree);
                        }
                        return Err(ProfileLaunchError::Spawn(spawn_error));
                    }
                }
            }
        }

        // Assemble the session and its client, if any, viewing the tab the
        // profile starts focused on.
        let focused_tab_index = template
            .focused_tab_index
            .min(profile_tab_plans.len().saturating_sub(1));
        let focused_tab_id = profile_tab_plans[focused_tab_index].tab_id;
        let mut session = Session::from_identity_and_client_registry(
            session_id,
            session_name,
            now,
            ClientRegistry::new(),
        );
        session.start_locked = template.is_locked;
        attach_first_client(&mut session, client_id, viewport, focused_tab_id, now);

        // Commit each tab; only the focused one moves the client onto it.
        for (tab_index, tab_plan) in profile_tab_plans.into_iter().enumerate() {
            let tab_name = generate_name(NameKind::Tab, |candidate| {
                session
                    .tabs
                    .values()
                    .any(|tab| tab.get_tab_name() == candidate)
            });
            let _ = tab_ops::commit_profile_tab(
                &mut session,
                tab_plan.tab_id,
                tab_ops::ProfileTab {
                    pane_ids: tab_plan.pane_ids,
                    layout: tab_plan.layout_tree,
                    specs: tab_plan.pane_specs,
                    focused_leaf_index: tab_plan.focused_leaf_index,
                },
                tab_name,
                client_id,
                tab_index == focused_tab_index,
                now,
            );
        }

        self.session_by_id.insert(session_id, session);
        for (pane_id, pty_handle, pane_size) in spawned_pty_handles {
            self.park_pane_pty(pane_id, pty_handle, pane_size);
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
/// [`LockMode::Locked`], and the flag is
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
    let mut client = Client::from_attachment(
        client_id,
        session.session_id,
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
struct ProfileTabPlan {
    /// The tab's id.
    tab_id: TabId,
    /// One pane id per leaf, in layout order.
    pane_ids: Vec<PaneId>,
    /// The live tree the ids fill.
    layout_tree: LayoutNode,
    /// The spawn request for each pane, parallel to `pane_ids`.
    spawn_specs: Vec<SpawnSpec>,
    /// The record spec for each pane, parallel to `pane_ids`.
    pane_specs: Vec<NewPaneSpec>,
    /// Index into `pane_ids` of the pane that starts focused.
    focused_leaf_index: usize,
}

impl Server {
    /// The spawn spec (what to launch) and record spec (what to remember) for
    /// one terminal leaf of a profile. A leaf with no command runs the default
    /// shell (honoring `terminal.default_shell`); either way the spec carries
    /// koshi's configured terminal identity, with the leaf's own `env` winning.
    fn profile_pane_specs(&self, terminal_template: &TerminalTemplate) -> (SpawnSpec, NewPaneSpec) {
        let working_directory = terminal_template.working_directory.clone();
        let environment_variables = self.apply_terminal_identity_environment_variables(
            terminal_template.environment_variables.clone(),
        );
        match &terminal_template.command {
            Some(command_template) => {
                let spawn_spec = SpawnSpec {
                    program: command_template.program.clone(),
                    arguments: command_template.arguments.clone(),
                    working_directory: working_directory.clone(),
                    environment_variables,
                    shell_kind: ShellKind::from_program(&command_template.program),
                };
                let pane_spec = NewPaneSpec {
                    working_directory,
                    spawn_spec: Some(spawn_spec.clone()),
                };
                (spawn_spec, pane_spec)
            }
            None => {
                let spawn_spec =
                    self.build_default_shell_spec(working_directory.clone(), environment_variables);
                let pane_spec = NewPaneSpec {
                    working_directory,
                    spawn_spec: None,
                };
                (spawn_spec, pane_spec)
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
            Self::Template(template_error) => {
                write!(f, "profile layout could not be built: {template_error}")
            }
            Self::Spawn(spawn_error) => {
                write!(f, "a profile pane failed to start: {spawn_error}")
            }
        }
    }
}
