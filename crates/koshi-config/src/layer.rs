//! Config layering: fold ordered override layers onto the built-in defaults.
//!
//! Koshi builds its effective config from ordered layers —
//! `built-in defaults → user → session → CLI flags` — where a later
//! layer overrides an earlier one field by field. Each override layer is a
//! [`PartialKoshiConfig`]: a mirror of [`KoshiConfig`] whose every field is
//! wrapped in [`Option`], so a layer carries only the
//! fields it sets. [`merge`] starts from a full base config (normally the
//! defaults) and applies each partial in order.
//!
//! Merge grain is deep and field-level for struct sections: a layer that sets
//! `scrollback.max_lines` leaves `scrollback.max_bytes` at the lower layer's
//! value. Collection-valued fields (`keybindings.modes`, `plugins.entries`) are
//! replaced whole when a layer sets them; per-element merge for those is done by
//! the keymap-merge and plugin-activation passes, which know the element
//! identity to merge on.
//!
//! The schema `version` is not layerable: it is a property of the defaults and
//! of migration, not a per-file override, so it has no partial field here.

use std::collections::BTreeMap;

use koshi_core::geometry::Direction;
use koshi_core::key::KeyChord;

use crate::key::Leader;
use crate::types::{
    ClipboardBackend, ColorPalette, CopyConfig, KeybindingsConfig, KoshiConfig, LayoutDefaults,
    LoggingConfig, ModeBindings, ModeName, MouseConfig, PaneConfig, PluginActivation,
    PluginActivationConfig, RgbColor, ScrollbackConfig, TerminalConfig, ThemeConfig,
};

/// Folds `layers` onto `base` in order and returns the effective config.
///
/// `base` is the fully-populated lowest layer, normally
/// [`KoshiConfig::default`](crate::types::KoshiConfig::default). Each layer in
/// `layers` is applied in sequence, so later entries win on any field they set.
/// Merging never fails: an empty layer leaves the config unchanged.
pub fn merge(base: KoshiConfig, layers: Vec<PartialKoshiConfig>) -> KoshiConfig {
    let mut config = base;
    for layer in layers {
        layer.apply(&mut config);
    }
    config
}

/// Overwrites `field` with `value` when the layer set one, leaving it
/// untouched otherwise — the field-level merge grain every section below uses.
fn merge_field<T>(field: &mut T, value: Option<T>) {
    if let Some(value) = value {
        *field = value;
    }
}

/// One config layer: [`KoshiConfig`] with every section optional. A section
/// left `None` leaves the lower layers untouched;
/// a section set to `Some` applies its own per-field overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialKoshiConfig {
    /// Pane sizing overrides.
    pub pane: Option<PartialPaneConfig>,
    /// Scrollback cap overrides.
    pub scrollback: Option<PartialScrollbackConfig>,
    /// Keybinding config overrides.
    pub keybindings: Option<PartialKeybindingsConfig>,
    /// Layout default overrides.
    pub layout: Option<PartialLayoutDefaults>,
    /// Plugin activation overrides.
    pub plugins: Option<PartialPluginActivationConfig>,
    /// Mouse behavior overrides.
    pub mouse: Option<PartialMouseConfig>,
    /// Copy and clipboard overrides.
    pub copy: Option<PartialCopyConfig>,
    /// Terminal environment overrides.
    pub terminal: Option<PartialTerminalConfig>,
    /// Theme overrides.
    pub theme: Option<PartialThemeConfig>,
    /// Logging overrides.
    pub logging: Option<PartialLoggingConfig>,
}

impl PartialKoshiConfig {
    /// Applies each present section's overrides onto `config`.
    fn apply(self, config: &mut KoshiConfig) {
        if let Some(pane) = self.pane {
            pane.apply(&mut config.pane);
        }
        if let Some(scrollback) = self.scrollback {
            scrollback.apply(&mut config.scrollback);
        }
        if let Some(keybindings) = self.keybindings {
            keybindings.apply(&mut config.keybindings);
        }
        if let Some(layout) = self.layout {
            layout.apply(&mut config.layout);
        }
        if let Some(plugins) = self.plugins {
            plugins.apply(&mut config.plugins);
        }
        if let Some(mouse) = self.mouse {
            mouse.apply(&mut config.mouse);
        }
        if let Some(copy) = self.copy {
            copy.apply(&mut config.copy);
        }
        if let Some(terminal) = self.terminal {
            terminal.apply(&mut config.terminal);
        }
        if let Some(theme) = self.theme {
            theme.apply(&mut config.theme);
        }
        if let Some(logging) = self.logging {
            logging.apply(&mut config.logging);
        }
    }
}

/// Pane sizing overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialPaneConfig {
    /// Minimum pane width in columns.
    pub min_cols: Option<u16>,
    /// Minimum pane height in rows.
    pub min_rows: Option<u16>,
}

impl PartialPaneConfig {
    fn apply(self, target: &mut PaneConfig) {
        merge_field(&mut target.min_cols, self.min_cols);
        merge_field(&mut target.min_rows, self.min_rows);
    }
}

/// Scrollback cap overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialScrollbackConfig {
    /// Maximum retained lines per pane.
    pub max_lines: Option<usize>,
    /// Maximum retained bytes of scrollback text per pane.
    pub max_bytes: Option<usize>,
}

impl PartialScrollbackConfig {
    fn apply(self, target: &mut ScrollbackConfig) {
        merge_field(&mut target.max_lines, self.max_lines);
        merge_field(&mut target.max_bytes, self.max_bytes);
    }
}

/// Keybinding config overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialKeybindingsConfig {
    /// Milliseconds to wait for the next chord in a multi-key sequence.
    pub chord_timeout_ms: Option<u32>,
    /// Milliseconds before the which-key continuation hint appears.
    pub which_key_delay_ms: Option<u32>,
    /// Maximum number of chords in one key sequence.
    pub max_chord_depth: Option<u8>,
    /// The prefix that `<leader>` in a binding resolves to.
    pub leader: Option<Leader>,
    /// Per-mode bindings. When set, the whole map replaces the lower layer's;
    /// per-mode keymap merging is done by the keymap-merge pass.
    pub modes: Option<BTreeMap<ModeName, ModeBindings>>,
    /// Replacement chord for the reserved unlock. The outer `Option` is
    /// whether this layer sets the field; the inner `Option` is the value
    /// (`None` = keep the built-in unlock key).
    pub unlock_alternative: Option<Option<KeyChord>>,
}

impl PartialKeybindingsConfig {
    fn apply(self, target: &mut KeybindingsConfig) {
        merge_field(&mut target.chord_timeout_ms, self.chord_timeout_ms);
        merge_field(&mut target.which_key_delay_ms, self.which_key_delay_ms);
        merge_field(&mut target.max_chord_depth, self.max_chord_depth);
        merge_field(&mut target.leader, self.leader);
        // ponytail: whole-map replace; per-mode keymap merge is the keymap pass.
        merge_field(&mut target.modes, self.modes);
        merge_field(&mut target.unlock_alternative, self.unlock_alternative);
    }
}

/// Layout default overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialLayoutDefaults {
    /// Direction a new pane spawns when the command omits one.
    pub new_pane_direction: Option<Direction>,
    /// The named layout to load at startup. The outer `Option` is whether this
    /// layer sets the field; the inner `Option` is the value (`None` = no
    /// startup layout).
    pub default_layout: Option<Option<String>>,
}

impl PartialLayoutDefaults {
    fn apply(self, target: &mut LayoutDefaults) {
        merge_field(&mut target.new_pane_direction, self.new_pane_direction);
        merge_field(&mut target.default_layout, self.default_layout);
    }
}

/// Plugin activation overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialPluginActivationConfig {
    /// The full activation entry list. When set, it replaces the lower layer's;
    /// per-entry merging is done by the plugin-activation pass.
    pub entries: Option<Vec<PluginActivation>>,
}

impl PartialPluginActivationConfig {
    fn apply(self, target: &mut PluginActivationConfig) {
        // ponytail: whole-list replace; per-entry merge is the plugin pass.
        merge_field(&mut target.entries, self.entries);
    }
}

/// Mouse behavior overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialMouseConfig {
    /// Whether dragging a pane border resizes it.
    pub border_resize: Option<bool>,
    /// Lines scrolled per mouse wheel notch.
    pub scroll_lines: Option<u16>,
}

impl PartialMouseConfig {
    fn apply(self, target: &mut MouseConfig) {
        merge_field(&mut target.border_resize, self.border_resize);
        merge_field(&mut target.scroll_lines, self.scroll_lines);
    }
}

/// Copy and clipboard overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialCopyConfig {
    /// Whether completing a selection copies it immediately.
    pub copy_on_select: Option<bool>,
    /// Whether trailing whitespace is trimmed from copied text.
    pub trim_trailing_whitespace: Option<bool>,
    /// Which clipboard backend receives copied text.
    pub clipboard: Option<ClipboardBackend>,
}

impl PartialCopyConfig {
    fn apply(self, target: &mut CopyConfig) {
        merge_field(&mut target.copy_on_select, self.copy_on_select);
        merge_field(
            &mut target.trim_trailing_whitespace,
            self.trim_trailing_whitespace,
        );
        merge_field(&mut target.clipboard, self.clipboard);
    }
}

/// Terminal environment overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialTerminalConfig {
    /// The `TERM` value advertised to child programs.
    pub term: Option<String>,
    /// The `COLORTERM` value advertised to child programs.
    pub colorterm: Option<String>,
    /// The shell to launch. The outer `Option` is whether this layer sets the
    /// field; the inner `Option` is the value (`None` = fall back to `$SHELL`).
    pub default_shell: Option<Option<String>>,
}

impl PartialTerminalConfig {
    fn apply(self, target: &mut TerminalConfig) {
        merge_field(&mut target.term, self.term);
        merge_field(&mut target.colorterm, self.colorterm);
        merge_field(&mut target.default_shell, self.default_shell);
    }
}

/// Theme overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialThemeConfig {
    /// The theme's display name.
    pub name: Option<String>,
    /// Per-role color overrides.
    pub colors: Option<PartialColorPalette>,
}

impl PartialThemeConfig {
    fn apply(self, target: &mut ThemeConfig) {
        merge_field(&mut target.name, self.name);
        if let Some(colors) = self.colors {
            colors.apply(&mut target.colors);
        }
    }
}

/// Per-role color overrides. Each role is set independently; a role left `None`
/// keeps the lower layer's color.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialColorPalette {
    /// First endpoint of the chrome gradient.
    pub ramp_start: Option<RgbColor>,
    /// Second endpoint of the chrome gradient.
    pub ramp_end: Option<RgbColor>,
    /// Text drawn over a ramp-colored block.
    pub on_ramp: Option<RgbColor>,
    /// Text drawn over a dimmed ramp block.
    pub on_ramp_dim: Option<RgbColor>,
    /// The in-progress accent for the pending-sequence breadcrumb.
    pub accent: Option<RgbColor>,
    /// Text drawn over an accent block.
    pub on_accent: Option<RgbColor>,
    /// Border of the focused pane.
    pub border_focused: Option<RgbColor>,
    /// Border of unfocused panes.
    pub border_unfocused: Option<RgbColor>,
    /// Text of a collapsed stack member's header strip.
    pub stack_header_fg: Option<RgbColor>,
    /// Background of a collapsed stack member's header strip.
    pub stack_header_bg: Option<RgbColor>,
    /// Backdrop of the letterbox margin around a centered layout.
    pub letterbox: Option<RgbColor>,
}

/// Logging overrides.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PartialLoggingConfig {
    /// Whether koshi writes a log file.
    pub enabled: Option<bool>,
}

impl PartialLoggingConfig {
    fn apply(self, target: &mut LoggingConfig) {
        merge_field(&mut target.enabled, self.enabled);
    }
}

impl PartialColorPalette {
    fn apply(self, target: &mut ColorPalette) {
        merge_field(&mut target.ramp_start, self.ramp_start);
        merge_field(&mut target.ramp_end, self.ramp_end);
        merge_field(&mut target.on_ramp, self.on_ramp);
        merge_field(&mut target.on_ramp_dim, self.on_ramp_dim);
        merge_field(&mut target.accent, self.accent);
        merge_field(&mut target.on_accent, self.on_accent);
        merge_field(&mut target.border_focused, self.border_focused);
        merge_field(&mut target.border_unfocused, self.border_unfocused);
        merge_field(&mut target.stack_header_fg, self.stack_header_fg);
        merge_field(&mut target.stack_header_bg, self.stack_header_bg);
        merge_field(&mut target.letterbox, self.letterbox);
    }
}

#[cfg(test)]
mod tests;
