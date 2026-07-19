//! The offline keymap view served by the `koshi keys` queries.
//!
//! `keys list`, `keys describe`, `keys conflicts`, and `keys validate` answer
//! without a running session: the view folds the user's keybinding file (when
//! one exists in the koshi config directory) onto the built-in defaults, runs
//! conflict detection against the core action table, and merges the layers
//! into the per-mode lookup the renderers read. The running session's layers
//! (`session`, `layout`) are not visible here; querying them arrives with
//! the IPC client.
//!
//! The keybinding section applies all-or-nothing: a user file that fails to
//! parse, or whose conflict verdict refuses it, leaves the view on the
//! built-in defaults and carries the reasons for the caller to surface.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use koshi_config::conflict::{
    detect_conflicts, ConflictReport, KeyMapLayer, KeymapVerdict, LayerOrigin,
};
use koshi_config::key::Leader;
use koshi_config::keybinding::{parse_keybindings, KeybindingParseError};
use koshi_config::keymap_merge::{merge_keymaps, MergedKeyMap};
use koshi_config::layer::PartialKeybindingsConfig;
use koshi_config::types::{default_mode_bindings, KeybindingsConfig, ModeBindings, ModeName};
use koshi_core::lock::LockMode;
use koshi_core::registry::ActionRegistry;

#[cfg(test)]
mod tests;

/// The effective keymap as seen from outside a session: the folded
/// keybinding settings, the merged per-mode lookup, the conflict report that
/// admitted (or refused) the user layer, and the live core action table.
pub struct KeymapView {
    /// The effective keybinding settings — the built-in defaults with the
    /// user file's fields folded on when its verdict admitted it.
    pub config: KeybindingsConfig,
    /// The merged per-mode lookup the renderers read.
    pub merged: MergedKeyMap,
    /// The core action table the bindings resolve against.
    pub registry: ActionRegistry,
    /// Every conflict-detection finding for the user layer, warnings
    /// included. Holds no findings when no user file exists.
    pub report: ConflictReport,
    /// True when a user file exists but was not admitted — its conflict
    /// verdict refused it, or it failed to parse — so the view shows the
    /// built-in defaults.
    pub reverted: bool,
    /// The user keybinding file the view read, when one exists.
    pub user_file: Option<PathBuf>,
    /// Why the user file could not be used, when it could not be parsed or
    /// read; rendered alongside the defaults-only listing.
    pub file_error: Option<String>,
}

/// Load the offline keymap view: read `keybinding.kdl` from the koshi config
/// directory when it exists, and fold it onto the built-in defaults.
#[must_use]
pub fn load_keymap_view() -> KeymapView {
    let user_file = koshi_paths::config_dir().map(|dir| dir.join("keybinding.kdl"));
    let Some(path) = user_file.filter(|path| path.exists()) else {
        return view_from_partial(None, None, None);
    };
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(err) => {
            return view_from_partial(None, Some(path.clone()), Some(err.to_string()));
        }
    };
    match parse_keybindings(&path, &source) {
        Ok(partial) => view_from_partial(Some(partial), Some(path), None),
        Err(err) => view_from_partial(None, Some(path), Some(render_parse_error(&err))),
    }
}

/// Build the view for one already-parsed user layer (`None` = defaults
/// only). `user_file`/`file_error` pass through to the view. This is
/// [`load_keymap_view`] minus the file I/O, for callers that already hold
/// the parsed layer.
#[must_use]
pub fn view_from_partial(
    partial: Option<PartialKeybindingsConfig>,
    user_file: Option<PathBuf>,
    file_error: Option<String>,
) -> KeymapView {
    let registry = ActionRegistry::new();
    let defaults = KeybindingsConfig::default();

    // Fold the user fields onto the defaults to get the candidate settings.
    let mut config = defaults.clone();
    let user_modes = partial.as_ref().and_then(|partial| partial.modes.clone());
    if let Some(partial) = partial {
        if let Some(value) = partial.chord_timeout_ms {
            config.chord_timeout_ms = value;
        }
        if let Some(value) = partial.which_key_delay_ms {
            config.which_key_delay_ms = value;
        }
        if let Some(value) = partial.max_chord_depth {
            config.max_chord_depth = value;
        }
        if let Some(value) = partial.leader {
            config.leader = value;
        }
        if let Some(value) = partial.unlock_alternative {
            config.unlock_alternative = value;
        }
    }

    let layers = keymap_layers(user_modes, config.leader);
    let report = detect_conflicts(
        &layers,
        config.leader,
        config.unlock_alternative,
        config.max_chord_depth,
        &registry,
        &built_in_modes(),
    );

    // All-or-nothing: a refused user layer drops the whole section back to
    // the defaults.
    let admitted = report.verdict() == KeymapVerdict::Apply;
    let (config, layers) = if admitted {
        (config, layers)
    } else {
        (defaults.clone(), keymap_layers(None, defaults.leader))
    };

    let merged = merge_keymaps(
        &layers,
        config.unlock_alternative,
        config.max_chord_depth,
        &registry,
        &built_in_modes(),
    );
    KeymapView {
        config,
        merged,
        registry,
        report,
        reverted: !admitted || file_error.is_some(),
        user_file,
        file_error,
    }
}

/// The ordered offline keymap layers: the built-in default binding table,
/// plus the user file's modes when present, arguments stripped.
///
/// The default table is built against `leader` — the effective leader, the
/// user's when their file set one — so `koshi keys list/validate/conflicts`
/// resolve the same `<leader>`-relative defaults a running koshi does, instead
/// of the built-in `C-` table.
fn keymap_layers(
    user_modes: Option<std::collections::BTreeMap<ModeName, ModeBindings>>,
    leader: Leader,
) -> Vec<KeyMapLayer> {
    let mut layers = vec![KeyMapLayer {
        origin: LayerOrigin::Defaults,
        modes: default_mode_bindings(leader),
    }];
    if let Some(modes) = user_modes {
        layers.push(
            KeyMapLayer {
                origin: LayerOrigin::User,
                modes,
            }
            .with_user_args_stripped(),
        );
    }
    layers
}

/// Every built-in input mode's name.
fn built_in_modes() -> BTreeSet<ModeName> {
    LockMode::ALL
        .iter()
        .map(|mode| ModeName::new(mode.name()))
        .collect()
}

/// The outcome of dry-running one keybinding file.
pub enum ValidationOutcome {
    /// The file did not parse; each element is one rendered problem.
    ParseFailed(Vec<String>),
    /// The file parsed; the report carries every conflict finding and the
    /// verdict says whether a reload would apply it.
    Checked {
        /// The conflict-detection findings for the file's layer.
        report: ConflictReport,
        /// True when a reload would apply the file.
        applies: bool,
    },
}

/// Dry-run the keybinding file at `path`: parse it and run conflict
/// detection, applying nothing.
///
/// # Errors
/// An [`std::io::Error`] when the file cannot be read.
pub fn validate_file(path: &Path) -> Result<ValidationOutcome, std::io::Error> {
    let source = fs::read_to_string(path)?;
    let partial = match parse_keybindings(path, &source) {
        Ok(partial) => partial,
        Err(err) => return Ok(ValidationOutcome::ParseFailed(parse_error_lines(&err))),
    };
    let view = view_from_partial(Some(partial), Some(path.to_path_buf()), None);
    Ok(ValidationOutcome::Checked {
        applies: !view.reverted,
        report: view.report,
    })
}

/// One rendered line per problem in a parse failure.
fn parse_error_lines(err: &KeybindingParseError) -> Vec<String> {
    match err {
        KeybindingParseError::Syntax(err) => vec![err.to_string()],
        KeybindingParseError::Invalid { diagnostics, .. } => diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message().to_string())
            .collect(),
    }
}

/// A parse failure as one string, for the view's `file_error`.
fn render_parse_error(err: &KeybindingParseError) -> String {
    parse_error_lines(err).join("; ")
}
