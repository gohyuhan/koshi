//! Genesis: seed the first session of a per-session server process in code.
//!
//! A plain start seeds one session with one tab holding one shell pane,
//! directly through the session-layer ops, then parks the pane's PTY like any
//! other. The session holds no client; the first attach adds one.
//!
//! A `--profile` start seeds one session holding every tab the profile file
//! defines, each with its own tree of panes.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;

use koshi_core::geometry::Size;
use koshi_core::ids::{PaneId, SessionId, TabId};
use koshi_core::notification::NotificationSeverity;
use koshi_core::process::{KillPolicy, PtySize, ShellKind, SpawnSpec};
use koshi_layout::template::{ProfileTemplate, TemplateError, TerminalTemplate};
use koshi_layout::tree::LayoutNode;
use koshi_pty::error::PtyError;
use koshi_session::client::{compute_default_pane_area_size, ClientRegistry};
use koshi_session::session::pane_ops::NewPaneSpec;
use koshi_session::session::state::Session;
use koshi_session::session::tab_ops;

use crate::resume::{LAYOUT_NOT_RESTORED_NOTICE_BYTES, LAYOUT_NOT_RESTORED_NOTIFICATION_TEXT};
use crate::runtime::command::{
    compute_pane_spawn_sizes, compute_root_pane_pty_size, generate_tab_name,
};
use crate::runtime::spawn_env::add_koshi_environment;
use crate::server::Server;

#[cfg(test)]
mod tests;

impl Server {
    /// Seed one session under `session_id` and `session_name`, holding one tab
    /// with one pane that runs the default shell and no client. The pane is
    /// sized to the pane region of `viewport_size`; `session_started_at`
    /// stamps the session. The first attach adds a client.
    ///
    /// The child is spawned before any state is committed, so a failed launch
    /// leaves no session behind and surfaces as `Err`.
    pub fn bootstrap_session(
        &mut self,
        session_id: SessionId,
        session_name: String,
        viewport_size: Size,
        session_started_at: SystemTime,
    ) -> Result<(), PtyError> {
        let pty_backend = Arc::clone(self.get_pty_backend());

        let tab_id = TabId::new();
        let pane_id = PaneId::new();

        // Chrome owns one row above and below the pane region.
        let pane_area_size = compute_default_pane_area_size(viewport_size);
        let root_pane_pty_size =
            compute_root_pane_pty_size(pane_id, pane_area_size, self.get_pane_sizing());

        // Launch the shell first: on failure nothing is registered. The spawn spec
        // carries the pane's in-session identity vars in its env overlay.
        let mut spawn_spec = self.build_default_shell_spec(None, BTreeMap::new());
        add_koshi_environment(
            &mut spawn_spec.environment_variables,
            session_id,
            None,
            pane_id,
            koshi_paths::resolve_runtime_directory().as_deref(),
            self.config.pane_clipboard_writes.should_allow_writes,
        );
        pty_backend.spawn_pane(pane_id, spawn_spec, root_pane_pty_size)?;

        // Assemble the session, then commit the tab + root pane.
        let mut session = Session::from_identity_and_client_registry(
            session_id,
            session_name,
            session_started_at,
            ClientRegistry::new(),
        );
        let _ = session.update_pane_sizing(self.get_pane_sizing());

        let tab_name = generate_tab_name(&session);
        let new_pane_spec = NewPaneSpec {
            working_directory: None,
            spawn_spec: None,
        };
        let _ = tab_ops::commit_new_tab(
            &mut session,
            tab_id,
            pane_id,
            tab_name,
            None,
            new_pane_spec,
            pane_area_size,
        );

        self.session_by_id.insert(session_id, session);
        self.park_pane_pty(pane_id, root_pane_pty_size);

        Ok(())
    }

    /// Seed one session under `session_id` and `session_name` over panes whose
    /// children are already running, one tab per pane, in `carried_pane_sizes`
    /// order.
    ///
    /// Each pane is recorded live at the size its entry names, gets a blank
    /// screen, and shows [`LAYOUT_NOT_RESTORED_NOTICE_BYTES`]. The session
    /// posts the `Warning` notification [`LAYOUT_NOT_RESTORED_NOTIFICATION_TEXT`].
    /// Each tab is solved at `0x0` until a client views it. The session holds
    /// no client.
    /// A client that attaches goes to the tab its saved view names while that
    /// tab exists, else to the tab with the lowest index.
    ///
    /// Before → after: panes `A` (80×24) and `B` (120×40) → a session with two
    /// tabs, `A` alone in the first and `B` alone in the second, each showing
    /// the notice line.
    pub fn restore_panes_without_layout(
        &mut self,
        session_id: SessionId,
        session_name: String,
        carried_pane_sizes: &[(PaneId, PtySize)],
        session_started_at: SystemTime,
    ) {
        let mut session = Session::from_identity_and_client_registry(
            session_id,
            session_name,
            session_started_at,
            ClientRegistry::new(),
        );
        let _ = session.update_pane_sizing(self.get_pane_sizing());
        for (pane_id, pty_size) in carried_pane_sizes {
            let tab_name = generate_tab_name(&session);
            let new_pane_spec = NewPaneSpec {
                working_directory: None,
                spawn_spec: None,
            };
            let _ = tab_ops::commit_new_tab(
                &mut session,
                TabId::new(),
                *pane_id,
                tab_name,
                None,
                new_pane_spec,
                Size {
                    column_count: 0,
                    row_count: 0,
                },
            );
            self.park_pane_pty(*pane_id, *pty_size);
        }
        self.session_by_id.insert(session_id, session);
        for (pane_id, _) in carried_pane_sizes {
            self.handle_pty_output(*pane_id, LAYOUT_NOT_RESTORED_NOTICE_BYTES);
        }
        self.post_session_notification(
            session_id,
            NotificationSeverity::Warning,
            LAYOUT_NOT_RESTORED_NOTIFICATION_TEXT,
        );
    }

    /// Seed one session under `session_id` and `session_name` from a
    /// `--profile` template: every tab the profile defines, each with its own
    /// tree of panes. The session holds no client.
    ///
    /// Each tab records the pane its profile marks `focus` as the newest entry
    /// of its focus history. The session records the tab the profile marks
    /// `focus`, else the profile's first tab, as its starting tab
    /// ([`Session::starting_tab_id`]), and the `lock` marker as its starting
    /// lock ([`Session::should_start_locked`]). The first client to attach
    /// opens on the starting tab, locked when the marker is set, focused in
    /// each tab on the pane that tab records.
    ///
    /// Every child is spawned before any state is committed, so a failed launch
    /// commits nothing and kills whatever it already spawned — the caller then
    /// falls back to a plain single-pane start.
    ///
    /// # Panics
    ///
    /// Panics when `template` holds no tab, or when one of its tabs holds no
    /// leaf. [`parse_profile`](koshi_config::profile::parse_profile) rejects
    /// both.
    pub fn bootstrap_profile(
        &mut self,
        session_id: SessionId,
        session_name: String,
        template: ProfileTemplate,
        viewport_size: Size,
        session_started_at: SystemTime,
    ) -> Result<(), ProfileLaunchError> {
        let pty_backend = Arc::clone(self.get_pty_backend());
        let tab_pane_area_size = compute_default_pane_area_size(viewport_size);

        // Plan every tab: a pane id per leaf, the spawn spec and the record
        // spec for each, and the live tree the ids fill.
        let mut profile_tab_plans: Vec<ProfileTabPlan> = Vec::with_capacity(template.tabs.len());
        for tab_template in &template.tabs {
            let leaf_templates = tab_template.root.list_leaf_templates();
            let mut pane_ids = Vec::with_capacity(leaf_templates.len());
            let mut spawn_specs = Vec::with_capacity(leaf_templates.len());
            let mut new_pane_specs = Vec::with_capacity(leaf_templates.len());
            for terminal_template in leaf_templates {
                let (spawn_spec, new_pane_spec) = self.build_profile_pane_specs(terminal_template);
                pane_ids.push(PaneId::new());
                spawn_specs.push(spawn_spec);
                new_pane_specs.push(new_pane_spec);
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
                new_pane_specs,
                focused_leaf_index: tab_template.focused_leaf_index,
            });
        }

        // Spawn every pane before committing anything. On any failure, kill
        // every pane already spawned: no child outlives the launch.
        let runtime_directory_path = koshi_paths::resolve_runtime_directory();
        let should_add_clipboard_tools = self.config.pane_clipboard_writes.should_allow_writes;
        let mut spawned_pane_pty_sizes: Vec<(PaneId, PtySize)> = Vec::new();
        let pane_sizing = self.get_pane_sizing();
        for tab_plan in &profile_tab_plans {
            // Size every pane against the tab's whole tree: each child of a
            // multi-pane tab spawns at its tiled slice of the tab.
            let pane_spawn_pty_sizes =
                compute_pane_spawn_sizes(&tab_plan.layout_tree, tab_pane_area_size, pane_sizing);
            for (pane_id, spawn_spec) in tab_plan.pane_ids.iter().zip(&tab_plan.spawn_specs) {
                let pane_pty_size = pane_spawn_pty_sizes
                    .iter()
                    .find(|(candidate_pane_id, _)| candidate_pane_id == pane_id)
                    .map(|(_, pane_pty_size)| *pane_pty_size)
                    .expect("every planned pane id is a leaf of its own tab tree");
                let mut pane_spawn_spec = spawn_spec.clone();
                add_koshi_environment(
                    &mut pane_spawn_spec.environment_variables,
                    session_id,
                    None,
                    *pane_id,
                    runtime_directory_path.as_deref(),
                    should_add_clipboard_tools,
                );
                match pty_backend.spawn_pane(*pane_id, pane_spawn_spec, pane_pty_size) {
                    Ok(()) => spawned_pane_pty_sizes.push((*pane_id, pane_pty_size)),
                    Err(spawn_error) => {
                        // Group-kill each already-spawned pane: a child that a
                        // profile command forked or put in the background dies
                        // with its pane when the launch aborts.
                        for (spawned_pane_id, _) in &spawned_pane_pty_sizes {
                            let _ = pty_backend.kill_pane(*spawned_pane_id, KillPolicy::Tree);
                        }
                        return Err(ProfileLaunchError::Spawn(spawn_error));
                    }
                }
            }
        }

        let focused_tab_index = template
            .focused_tab_index
            .min(profile_tab_plans.len().saturating_sub(1));
        let mut session = Session::from_identity_and_client_registry(
            session_id,
            session_name,
            session_started_at,
            ClientRegistry::new(),
        );
        session.should_start_locked = template.is_locked;
        session.starting_tab_id = Some(profile_tab_plans[focused_tab_index].tab_id);
        let _ = session.update_pane_sizing(pane_sizing);

        for tab_plan in profile_tab_plans {
            let tab_name = generate_tab_name(&session);
            let _ = tab_ops::commit_profile_tab(
                &mut session,
                tab_plan.tab_id,
                tab_ops::ProfileTab {
                    pane_ids: tab_plan.pane_ids,
                    layout_tree: tab_plan.layout_tree,
                    new_pane_specs: tab_plan.new_pane_specs,
                    focused_leaf_index: tab_plan.focused_leaf_index,
                },
                tab_name,
                tab_pane_area_size,
            );
        }

        self.session_by_id.insert(session_id, session);
        for (pane_id, pane_pty_size) in spawned_pane_pty_sizes {
            self.park_pane_pty(pane_id, pane_pty_size);
        }

        Ok(())
    }
}

/// One profile tab's pane ids, layout tree, spawn requests, pane records, and focus.
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
    new_pane_specs: Vec<NewPaneSpec>,
    /// Index into `pane_ids` of the pane that starts focused.
    focused_leaf_index: usize,
}

impl Server {
    /// The spawn spec (what to launch) and record spec (what to remember) for
    /// one terminal leaf of a profile. A leaf with no command runs the default
    /// shell (honoring `terminal.default_shell`); either way the spec carries
    /// koshi's configured terminal identity, with the leaf's own `env` winning.
    fn build_profile_pane_specs(
        &self,
        terminal_template: &TerminalTemplate,
    ) -> (SpawnSpec, NewPaneSpec) {
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
                let new_pane_spec = NewPaneSpec {
                    working_directory,
                    spawn_spec: Some(spawn_spec.clone()),
                };
                (spawn_spec, new_pane_spec)
            }
            None => {
                let spawn_spec =
                    self.build_default_shell_spec(working_directory.clone(), environment_variables);
                let new_pane_spec = NewPaneSpec {
                    working_directory,
                    spawn_spec: None,
                };
                (spawn_spec, new_pane_spec)
            }
        }
    }
}

/// Why a `--profile` launch could not be instantiated. The caller falls back to
/// a plain single-pane start and surfaces the reason.
#[derive(Debug)]
pub enum ProfileLaunchError {
    /// A tab's tree could not be built from its pane ids: the tree's leaf count
    /// and the pane-id count disagree.
    Template(TemplateError),
    /// A pane's child process failed to spawn.
    Spawn(PtyError),
}

impl std::fmt::Display for ProfileLaunchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Template(template_error) => {
                write!(
                    formatter,
                    "profile layout could not be built: {template_error}"
                )
            }
            Self::Spawn(spawn_error) => {
                write!(formatter, "a profile pane failed to start: {spawn_error}")
            }
        }
    }
}
