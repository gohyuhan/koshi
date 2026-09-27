//! The offline keymap view served by the `koshi keys` queries.
//!
//! `keys list`, `keys describe`, `keys conflicts`, and `keys validate` answer
//! without a running session: the view folds the user's keybinding file (when
//! one exists in the koshi config directory) onto the built-in defaults, runs
//! conflict detection against the core action table, and merges the layers
//! into the per-mode lookup the renderers read. The running session's layers
//! (`session`, `layout`) are not visible here.
//!
//! The keybinding section applies all-or-nothing: a user file that fails to
//! parse, or whose conflict verdict refuses it, leaves the view on the
//! built-in defaults and carries the reasons for the caller to surface.

use std::fs;
use std::path::{Path, PathBuf};

use koshi_config::conflict::{
    build_keymap_layers, detect_conflicts, ConflictReport, KeymapVerdict,
};
use koshi_config::keybinding::{parse_keybindings, KeybindingParseError};
use koshi_config::keymap_merge::{merge_keymaps, MergedKeyMap};
use koshi_config::layer::PartialKeybindingsConfig;
use koshi_config::types::KeybindingsConfig;
use koshi_core::registry::ActionRegistry;

#[cfg(test)]
mod tests;

/// The effective keymap as seen from outside a session: the folded
/// keybinding settings, the merged per-mode lookup, the conflict report that
/// admitted (or refused) the user layer, and the live core action table.
pub struct KeymapView {
    /// The effective keybinding settings — the built-in defaults with the
    /// user file's fields folded on when its verdict admitted it.
    pub keybindings_config: KeybindingsConfig,
    /// The merged per-mode lookup the renderers read.
    pub merged_keymap: MergedKeyMap,
    /// The core action table the bindings resolve against.
    pub action_registry: ActionRegistry,
    /// Every conflict-detection finding for the user layer, warnings
    /// included. Holds no findings when no user file exists.
    pub conflict_report: ConflictReport,
    /// True when the view holds the built-in defaults although a user file
    /// exists: its conflict verdict refused it, or it could not be read or
    /// parsed.
    pub is_reverted_to_defaults: bool,
    /// The user keybinding file the view read, when one exists.
    pub user_keybinding_file_path: Option<PathBuf>,
    /// Why the user file could not be used, when it could not be parsed or
    /// read; rendered alongside the defaults-only listing.
    pub keybinding_file_error_message: Option<String>,
}

/// Load the offline keymap view: read `keybinding.kdl` from the koshi config
/// directory when it exists, and fold it onto the built-in defaults.
#[must_use]
pub fn load_keymap_view() -> KeymapView {
    let user_keybinding_file_path = koshi_paths::resolve_config_directory()
        .map(|config_directory| config_directory.join("keybinding.kdl"));
    let Some(keybinding_file_path) = user_keybinding_file_path
        .filter(|candidate_keybinding_file_path| candidate_keybinding_file_path.exists())
    else {
        return build_keymap_view_from_partial(None, None, None);
    };
    let keybinding_source_text = match fs::read_to_string(&keybinding_file_path) {
        Ok(keybinding_source_text) => keybinding_source_text,
        Err(keybinding_file_read_error) => {
            return build_keymap_view_from_partial(
                None,
                Some(keybinding_file_path.clone()),
                Some(keybinding_file_read_error.to_string()),
            );
        }
    };
    match parse_keybindings(&keybinding_file_path, &keybinding_source_text) {
        Ok(partial_keybindings_config) => build_keymap_view_from_partial(
            Some(partial_keybindings_config),
            Some(keybinding_file_path),
            None,
        ),
        Err(keybinding_parse_error) => build_keymap_view_from_partial(
            None,
            Some(keybinding_file_path),
            Some(render_parse_error(&keybinding_parse_error)),
        ),
    }
}

/// Build the view for one already-parsed user layer (`None` = defaults
/// only). The keybinding file path and error pass through to the view. Reads
/// no file: this is [`load_keymap_view`] without the file I/O.
#[must_use]
pub fn build_keymap_view_from_partial(
    partial_keybindings_config: Option<PartialKeybindingsConfig>,
    user_keybinding_file_path: Option<PathBuf>,
    keybinding_file_error_message: Option<String>,
) -> KeymapView {
    let action_registry = ActionRegistry::new();
    let default_keybindings_config = KeybindingsConfig::default();

    // Fold the user fields onto the defaults to get the candidate settings.
    let mut keybindings_config = default_keybindings_config.clone();
    let user_keymap_modes = match partial_keybindings_config {
        Some(partial_keybindings_config) => {
            if let Some(chord_timeout_ms) = partial_keybindings_config.chord_timeout_ms {
                keybindings_config.chord_timeout_ms = chord_timeout_ms;
            }
            if let Some(which_key_delay_ms) = partial_keybindings_config.which_key_delay_ms {
                keybindings_config.which_key_delay_ms = which_key_delay_ms;
            }
            if let Some(maximum_chord_depth) = partial_keybindings_config.maximum_chord_depth {
                keybindings_config.maximum_chord_depth = maximum_chord_depth;
            }
            if let Some(leader) = partial_keybindings_config.leader {
                keybindings_config.leader = leader;
            }
            if let Some(unlock_alternative) = partial_keybindings_config.unlock_alternative {
                keybindings_config.unlock_alternative = unlock_alternative;
            }
            partial_keybindings_config.mode_bindings_by_name
        }
        None => None,
    };

    let keymap_layers = build_keymap_layers(user_keymap_modes, keybindings_config.leader);
    let conflict_report = detect_conflicts(
        &keymap_layers,
        keybindings_config.leader,
        keybindings_config.unlock_alternative,
        keybindings_config.maximum_chord_depth,
        &action_registry,
    );

    // All-or-nothing: a refused user layer drops the whole section back to
    // the defaults.
    let is_keymap_admitted = conflict_report.get_verdict() == KeymapVerdict::Apply;
    let (keybindings_config, keymap_layers) = if is_keymap_admitted {
        (keybindings_config, keymap_layers)
    } else {
        (
            default_keybindings_config.clone(),
            build_keymap_layers(None, default_keybindings_config.leader),
        )
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
        is_reverted_to_defaults: !is_keymap_admitted || keybinding_file_error_message.is_some(),
        user_keybinding_file_path,
        keybinding_file_error_message,
    }
}

/// The outcome of dry-running one keybinding file.
pub enum KeymapValidationOutcome {
    /// The file did not parse; each element is one rendered problem.
    ParseFailed(Vec<String>),
    /// The file parsed; the conflict report carries every finding and the
    /// verdict says whether a reload would apply it.
    Checked {
        /// The conflict-detection findings for the file's layer.
        conflict_report: ConflictReport,
        /// True when a reload would apply the file.
        is_applicable: bool,
    },
}

/// Dry-run the keybinding file at `keymap_file_path`: parse it and run conflict
/// detection, applying nothing.
///
/// # Errors
/// An [`std::io::Error`] when the file cannot be read.
pub fn validate_keymap_file(
    keymap_file_path: &Path,
) -> Result<KeymapValidationOutcome, std::io::Error> {
    let keybinding_source_text = fs::read_to_string(keymap_file_path)?;
    let partial_keybindings_config =
        match parse_keybindings(keymap_file_path, &keybinding_source_text) {
            Ok(partial_keybindings_config) => partial_keybindings_config,
            Err(keybinding_parse_error) => {
                return Ok(KeymapValidationOutcome::ParseFailed(
                    list_parse_error_lines(&keybinding_parse_error),
                ))
            }
        };
    let keymap_view = build_keymap_view_from_partial(
        Some(partial_keybindings_config),
        Some(keymap_file_path.to_path_buf()),
        None,
    );
    Ok(KeymapValidationOutcome::Checked {
        is_applicable: !keymap_view.is_reverted_to_defaults,
        conflict_report: keymap_view.conflict_report,
    })
}

/// One rendered line per problem in a parse failure.
fn list_parse_error_lines(keybinding_parse_error: &KeybindingParseError) -> Vec<String> {
    match keybinding_parse_error {
        KeybindingParseError::Syntax(syntax_error) => vec![syntax_error.to_string()],
        KeybindingParseError::Invalid { diagnostics, .. } => diagnostics
            .iter()
            .map(|diagnostic| diagnostic.get_diagnostic_message().to_string())
            .collect(),
    }
}

/// A parse failure as one string, for the view's `keybinding_file_error_message`.
fn render_parse_error(keybinding_parse_error: &KeybindingParseError) -> String {
    list_parse_error_lines(keybinding_parse_error).join("; ")
}
