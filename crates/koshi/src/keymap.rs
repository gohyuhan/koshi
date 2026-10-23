//! The offline keymap view served by the `koshi keys` queries.
//!
//! `keys list`, `keys describe`, `keys conflicts`, and `keys validate` answer
//! without a running session: the view folds the user's keybinding file (when
//! one exists in the koshi config directory) onto the built-in defaults, runs
//! conflict detection against the core action table, and merges the layers
//! into the per-mode lookup the renderers read. The running session's layers
//! (`session`, `layout`) are not visible here.
//!
//! The view reads, parses, folds, and checks the file through the functions a
//! client runs on its own `keybinding.kdl` when it attaches, and
//! [`KeymapView::keymap_verdict`](crate::keymap::KeymapView::keymap_verdict)
//! is the decision that client reaches. The keybinding section applies
//! all-or-nothing: a user file that cannot be read or parsed, or whose
//! conflict verdict refuses it, leaves the view on the built-in defaults and
//! carries the reasons for the caller to surface.

use std::fs;
use std::path::{Path, PathBuf};

use koshi_config::conflict::{
    build_keymap_layers, detect_conflicts, ConflictReport, KeymapVerdict,
};
use koshi_config::keybinding::parse_keybindings;
use koshi_config::keymap_merge::{merge_keymaps, MergedKeymap};
use koshi_config::layer::{ConfigLayers, PartialKeybindingsConfig};
use koshi_config::types::KeybindingsConfig;
use koshi_core::registry::ActionRegistry;
use koshi_link::config::{load_keybinding_file, KeybindingFileError};

#[cfg(test)]
pub(crate) mod tests;

/// The effective keymap as seen from outside a session: the folded
/// keybinding settings, the merged per-mode lookup, the conflict report and
/// the verdict on the user layer, and the core action table.
pub struct KeymapView {
    /// The effective keybinding settings — the built-in defaults with the
    /// user file's fields folded on when the verdict is
    /// [`KeymapVerdict::Apply`].
    pub keybindings_config: KeybindingsConfig,
    /// The merged per-mode lookup the renderers read.
    pub merged_keymap: MergedKeymap,
    /// The core action table the bindings resolve against.
    pub action_registry: ActionRegistry,
    /// Every conflict-detection finding for the user layer, warnings
    /// included. Holds no findings when no user layer exists.
    pub conflict_report: ConflictReport,
    /// What a client does with the user keybinding file when it attaches.
    /// [`KeymapVerdict::Reject`] when the file cannot be read or parsed;
    /// otherwise the verdict of [`conflict_report`](Self::conflict_report),
    /// which is [`KeymapVerdict::Apply`] when no file exists. Any verdict
    /// other than `Apply` leaves the view on the built-in defaults.
    pub keymap_verdict: KeymapVerdict,
    /// The user keybinding file the view read, when one exists.
    pub user_keybinding_file_path: Option<PathBuf>,
    /// Why the user file could not be used, when it could not be parsed or
    /// read; rendered alongside the defaults-only listing.
    pub keybinding_file_error_message: Option<String>,
}

/// Load the offline keymap view: read `keybinding.kdl` from the koshi config
/// directory through [`load_keybinding_file`], and fold it onto the built-in
/// defaults. An absent file gives the defaults with no error. A file that
/// cannot be read gives the defaults with the read error in
/// `keybinding_file_error_message`. A file that does not parse gives the
/// defaults with its parse errors, joined by `; `, in
/// `keybinding_file_error_message`. A file that exists always sets
/// `user_keybinding_file_path`.
#[must_use]
pub fn load_keymap_view() -> KeymapView {
    let Some(keybinding_file_path) = koshi_paths::resolve_config_directory()
        .map(|config_directory| config_directory.join("keybinding.kdl"))
    else {
        return build_keymap_view_from_partial(None, None, None);
    };
    match load_keybinding_file(&keybinding_file_path) {
        Ok(None) => build_keymap_view_from_partial(None, None, None),
        Ok(Some(partial_keybindings_config)) => build_keymap_view_from_partial(
            Some(partial_keybindings_config),
            Some(keybinding_file_path),
            None,
        ),
        Err(KeybindingFileError::Unreadable(keybinding_file_read_error)) => {
            build_keymap_view_from_partial(
                None,
                Some(keybinding_file_path),
                Some(keybinding_file_read_error.to_string()),
            )
        }
        Err(KeybindingFileError::Unparseable(keybinding_parse_error)) => {
            build_keymap_view_from_partial(
                None,
                Some(keybinding_file_path),
                Some(keybinding_parse_error.list_problem_messages().join("; ")),
            )
        }
    }
}

/// Build the view for one already-parsed user layer (`None` = defaults
/// only). The keybinding file path and error pass through to the view, and an
/// error gives the verdict [`KeymapVerdict::Reject`]. Reads no file: this is
/// [`load_keymap_view`] without the file I/O.
///
/// The user fields fold onto the defaults through
/// [`ConfigLayers::resolve_effective_client_config`], and the folded leader,
/// unlock alternative, and chord depth feed conflict detection.
#[must_use]
pub fn build_keymap_view_from_partial(
    partial_keybindings_config: Option<PartialKeybindingsConfig>,
    user_keybinding_file_path: Option<PathBuf>,
    keybinding_file_error_message: Option<String>,
) -> KeymapView {
    let action_registry = ActionRegistry::new();
    let user_mode_bindings_by_name =
        partial_keybindings_config
            .as_ref()
            .and_then(|partial_keybindings_config| {
                partial_keybindings_config.mode_bindings_by_name.clone()
            });
    let candidate_keybindings_config =
        ConfigLayers::from_config_file_layers(None, None, partial_keybindings_config)
            .resolve_effective_client_config()
            .keybindings;
    let candidate_keymap_layers = build_keymap_layers(
        user_mode_bindings_by_name,
        candidate_keybindings_config.leader,
    );
    let conflict_report = detect_conflicts(
        &candidate_keymap_layers,
        candidate_keybindings_config.leader,
        candidate_keybindings_config.unlock_alternative,
        candidate_keybindings_config.maximum_chord_depth,
        &action_registry,
    );
    let keymap_verdict = if keybinding_file_error_message.is_some() {
        KeymapVerdict::Reject
    } else {
        conflict_report.get_verdict()
    };

    // All-or-nothing: a verdict other than `Apply` drops the whole section
    // back to the defaults.
    let (keybindings_config, keymap_layers) = if keymap_verdict == KeymapVerdict::Apply {
        (candidate_keybindings_config, candidate_keymap_layers)
    } else {
        let default_keybindings_config = KeybindingsConfig::default();
        let default_keymap_layers = build_keymap_layers(None, default_keybindings_config.leader);
        (default_keybindings_config, default_keymap_layers)
    };

    let merged_keymap = merge_keymaps(
        &keymap_layers,
        keybindings_config.unlock_alternative,
        keybindings_config.maximum_chord_depth,
        &action_registry,
    );
    KeymapView {
        keybindings_config,
        merged_keymap,
        action_registry,
        conflict_report,
        keymap_verdict,
        user_keybinding_file_path,
        keybinding_file_error_message,
    }
}

/// The outcome of dry-running one keybinding file.
pub enum KeymapValidationOutcome {
    /// The file did not parse; each element is one rendered problem.
    ParseFailed(Vec<String>),
    /// The file parsed. The conflict report carries every finding, and its
    /// verdict is the one a client reaches on the file when it attaches.
    Checked(ConflictReport),
}

/// Dry-run the keybinding file at `keybinding_file_path`: parse it and run
/// conflict detection the way [`build_keymap_view_from_partial`] does,
/// applying nothing.
///
/// # Errors
/// An [`std::io::Error`] when the file cannot be read, an absent file
/// included.
pub fn validate_keybinding_file(
    keybinding_file_path: &Path,
) -> Result<KeymapValidationOutcome, std::io::Error> {
    let keybinding_source_text = fs::read_to_string(keybinding_file_path)?;
    let partial_keybindings_config =
        match parse_keybindings(keybinding_file_path, &keybinding_source_text) {
            Ok(partial_keybindings_config) => partial_keybindings_config,
            Err(keybinding_parse_error) => {
                return Ok(KeymapValidationOutcome::ParseFailed(
                    keybinding_parse_error.list_problem_messages(),
                ))
            }
        };
    let keymap_view = build_keymap_view_from_partial(
        Some(partial_keybindings_config),
        Some(keybinding_file_path.to_path_buf()),
        None,
    );
    Ok(KeymapValidationOutcome::Checked(
        keymap_view.conflict_report,
    ))
}
