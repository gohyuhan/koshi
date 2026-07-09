//! Config layering: fold ordered override layers onto the built-in defaults.
//!
//! Koshi builds its effective config from ordered layers —
//! `built-in defaults → user → project → session → CLI flags` — where a later
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

use crate::key::Leader;
use crate::types::{
    ClipboardBackend, ColorPalette, CopyConfig, KeybindingsConfig, KoshiConfig, LayoutDefaults,
    ModeBindings, ModeName, MouseConfig, PaneConfig, PluginActivation, PluginActivationConfig,
    RgbColor, ScrollbackConfig, TerminalConfig, ThemeConfig,
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
        if let Some(v) = self.min_cols {
            target.min_cols = v;
        }
        if let Some(v) = self.min_rows {
            target.min_rows = v;
        }
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
        if let Some(v) = self.max_lines {
            target.max_lines = v;
        }
        if let Some(v) = self.max_bytes {
            target.max_bytes = v;
        }
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
}

impl PartialKeybindingsConfig {
    fn apply(self, target: &mut KeybindingsConfig) {
        if let Some(v) = self.chord_timeout_ms {
            target.chord_timeout_ms = v;
        }
        if let Some(v) = self.which_key_delay_ms {
            target.which_key_delay_ms = v;
        }
        if let Some(v) = self.max_chord_depth {
            target.max_chord_depth = v;
        }
        if let Some(v) = self.leader {
            target.leader = v;
        }
        // ponytail: whole-map replace; per-mode keymap merge is the keymap pass.
        if let Some(v) = self.modes {
            target.modes = v;
        }
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
        if let Some(v) = self.new_pane_direction {
            target.new_pane_direction = v;
        }
        if let Some(v) = self.default_layout {
            target.default_layout = v;
        }
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
        if let Some(v) = self.entries {
            target.entries = v;
        }
    }
}

/// Mouse behavior overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialMouseConfig {
    /// Whether dragging a pane border resizes it.
    pub border_resize: Option<bool>,
    /// Whether border-drag resize also works in locked mode.
    pub border_resize_in_lock: Option<bool>,
    /// Whether clicking a pane focuses it.
    pub click_to_focus: Option<bool>,
    /// Lines scrolled per mouse wheel notch.
    pub scroll_lines: Option<u16>,
}

impl PartialMouseConfig {
    fn apply(self, target: &mut MouseConfig) {
        if let Some(v) = self.border_resize {
            target.border_resize = v;
        }
        if let Some(v) = self.border_resize_in_lock {
            target.border_resize_in_lock = v;
        }
        if let Some(v) = self.click_to_focus {
            target.click_to_focus = v;
        }
        if let Some(v) = self.scroll_lines {
            target.scroll_lines = v;
        }
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
        if let Some(v) = self.copy_on_select {
            target.copy_on_select = v;
        }
        if let Some(v) = self.trim_trailing_whitespace {
            target.trim_trailing_whitespace = v;
        }
        if let Some(v) = self.clipboard {
            target.clipboard = v;
        }
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
        if let Some(v) = self.term {
            target.term = v;
        }
        if let Some(v) = self.colorterm {
            target.colorterm = v;
        }
        if let Some(v) = self.default_shell {
            target.default_shell = v;
        }
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
        if let Some(v) = self.name {
            target.name = v;
        }
        if let Some(colors) = self.colors {
            colors.apply(&mut target.colors);
        }
    }
}

/// Per-role color overrides. Each role is set independently; a role left `None`
/// keeps the lower layer's color.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialColorPalette {
    /// Default text foreground.
    pub foreground: Option<RgbColor>,
    /// Default background.
    pub background: Option<RgbColor>,
    /// Highlight color for focus and active elements.
    pub accent: Option<RgbColor>,
    /// Border of the focused pane.
    pub border_focused: Option<RgbColor>,
    /// Border of unfocused panes.
    pub border_unfocused: Option<RgbColor>,
    /// Foreground of the active tab.
    pub tab_active_fg: Option<RgbColor>,
    /// Background of the active tab.
    pub tab_active_bg: Option<RgbColor>,
    /// Foreground of inactive tabs.
    pub tab_inactive_fg: Option<RgbColor>,
    /// Background of inactive tabs.
    pub tab_inactive_bg: Option<RgbColor>,
    /// Foreground of the mode indicator.
    pub mode_fg: Option<RgbColor>,
    /// Background of the mode indicator.
    pub mode_bg: Option<RgbColor>,
    /// Foreground of a stacked-pane header.
    pub stack_header_fg: Option<RgbColor>,
    /// Background of a stacked-pane header.
    pub stack_header_bg: Option<RgbColor>,
    /// The key glyph in a keybinding hint.
    pub hint_key: Option<RgbColor>,
    /// The label text in a keybinding hint.
    pub hint_label: Option<RgbColor>,
    /// Background of the keybinding hint bar.
    pub hint_bg: Option<RgbColor>,
}

impl PartialColorPalette {
    fn apply(self, target: &mut ColorPalette) {
        if let Some(v) = self.foreground {
            target.foreground = v;
        }
        if let Some(v) = self.background {
            target.background = v;
        }
        if let Some(v) = self.accent {
            target.accent = v;
        }
        if let Some(v) = self.border_focused {
            target.border_focused = v;
        }
        if let Some(v) = self.border_unfocused {
            target.border_unfocused = v;
        }
        if let Some(v) = self.tab_active_fg {
            target.tab_active_fg = v;
        }
        if let Some(v) = self.tab_active_bg {
            target.tab_active_bg = v;
        }
        if let Some(v) = self.tab_inactive_fg {
            target.tab_inactive_fg = v;
        }
        if let Some(v) = self.tab_inactive_bg {
            target.tab_inactive_bg = v;
        }
        if let Some(v) = self.mode_fg {
            target.mode_fg = v;
        }
        if let Some(v) = self.mode_bg {
            target.mode_bg = v;
        }
        if let Some(v) = self.stack_header_fg {
            target.stack_header_fg = v;
        }
        if let Some(v) = self.stack_header_bg {
            target.stack_header_bg = v;
        }
        if let Some(v) = self.hint_key {
            target.hint_key = v;
        }
        if let Some(v) = self.hint_label {
            target.hint_label = v;
        }
        if let Some(v) = self.hint_bg {
            target.hint_bg = v;
        }
    }
}

#[cfg(test)]
mod tests;
