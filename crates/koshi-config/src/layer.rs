//! Config layering: fold ordered override layers onto the built-in defaults.
//!
//! Koshi builds its effective config from the built-in defaults plus ordered
//! override layers, where a higher-precedence layer overrides a lower one field by
//! field. Each override layer is a [`PartialKoshiConfig`]: a mirror of the
//! whole file whose every field is wrapped in [`Option`], so a layer carries
//! only the fields it sets.
//!
//! One file parses into one layer, and the two sides fold that same layer
//! separately: [`merge_server`] reads the sections a session owns and
//! [`merge_client`] reads the sections a viewer owns. Each starts from a full
//! base config (normally the defaults) and applies each partial in order.
//! `scrollback` is the one section both read — its caps go to the session, its
//! follow behavior to the viewer. [`ConfigLayers`] holds one layer per config
//! file and folds them in a fixed order.
//!
//! Merge grain is deep and field-level for struct sections: a layer that sets
//! `scrollback.maximum_line_count` leaves `scrollback.maximum_byte_count` at the lower layer's
//! value. The collection-valued `keybindings.mode_bindings_by_name` is replaced whole when a
//! layer sets it; per-element merge is done by the keymap-merge pass, which
//! knows the element identity to merge on.
//!
//! The schema `version` is not layerable: it is a property of the defaults and
//! of migration, not a per-file override, so it has no partial field here.

use std::collections::BTreeMap;
use std::path::PathBuf;

use koshi_core::geometry::Direction;
use koshi_core::key::{ExtendedKeysMode, KeyChord};
use koshi_core::log::{LogFormat, LogLevel};

use crate::key::Leader;
use crate::types::{
    ClientConfig, ColorPalette, CopyConfig, KeybindingsConfig, LayoutDefaults, LoggingConfig,
    ModeBindings, ModeName, MouseConfig, PaneConfig, RgbColor, ScrollbackLimits, ScrollbackView,
    ServerConfig, TerminalConfig, ThemeConfig, UpdateConfig, WheelScroll,
};

/// Folds `layers` onto `base` in order and returns the session's effective
/// settings, reading only the sections a session owns.
///
/// `base` is the fully-populated lowest layer, normally
/// [`ServerConfig::default`](crate::types::ServerConfig::default). Each layer
/// in `layers` is applied in sequence, so higher-precedence entries win on any field they
/// set. Merging never fails: an empty layer leaves the config unchanged.
///
/// A layer's viewer-owned sections (theme, keybindings, mouse, copy, layout,
/// update, image support) are skipped here and folded by [`merge_client`]
/// instead.
pub fn merge_server(
    base_server_config: ServerConfig,
    config_layers: Vec<PartialKoshiConfig>,
) -> ServerConfig {
    let mut server_config = base_server_config;
    for config_layer in config_layers {
        config_layer.apply_to_server_config(&mut server_config);
    }
    server_config
}

/// Folds `layers` onto `base` in order and returns one viewer's effective
/// settings, reading only the sections a viewer owns.
///
/// The counterpart of [`merge_server`] over the same layers: a layer's
/// session-owned sections (pane floor, scrollback caps, terminal environment)
/// are skipped here.
pub fn merge_client(
    base_client_config: ClientConfig,
    config_layers: Vec<PartialKoshiConfig>,
) -> ClientConfig {
    let mut client_config = base_client_config;
    for config_layer in config_layers {
        config_layer.apply_to_client_config(&mut client_config);
    }
    client_config
}

/// The stored config overrides one viewer reads, one layer per config file,
/// folded onto the built-in defaults by
/// [`resolve_effective_client_config`](Self::resolve_effective_client_config).
///
/// One file fills one layer, so replacing a file's settings replaces its layer
/// alone and leaves the others as they are.
///
/// The theme and keybinding layers carry viewer-owned sections alone, so the
/// session's own settings come from the `koshi.kdl` layer through
/// [`merge_server`] instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigLayers {
    /// The `koshi.kdl` app-settings layer.
    app_config_layer: PartialKoshiConfig,
    /// The color-theme file's layer; only its theme section is set.
    theme_config_layer: PartialKoshiConfig,
    /// The `keybinding.kdl` layer; only its keybindings section is set.
    keybindings_config_layer: PartialKoshiConfig,
}

impl ConfigLayers {
    /// Layers built from the three parsed config files. A file given as `None`
    /// contributes an empty layer, leaving the lower layers untouched.
    ///
    /// `theme` and `keybindings` each go into their own layer holding that
    /// section alone. `app`'s `theme` and `keybindings` sections are set to
    /// `None`; [`parse_app_config`](crate::app_config::parse_app_config) never
    /// fills either one, and only a hand-built `app` value can carry them.
    #[must_use]
    pub fn from_files(
        app_config_layer: Option<PartialKoshiConfig>,
        theme_config_layer: Option<PartialThemeConfig>,
        keybindings_config_layer: Option<PartialKeybindingsConfig>,
    ) -> Self {
        let mut app_config_layer = app_config_layer.unwrap_or_default();
        app_config_layer.theme = None;
        app_config_layer.keybindings = None;
        ConfigLayers {
            app_config_layer,
            theme_config_layer: PartialKoshiConfig {
                theme: theme_config_layer,
                ..PartialKoshiConfig::default()
            },
            keybindings_config_layer: PartialKoshiConfig {
                keybindings: keybindings_config_layer,
                ..PartialKoshiConfig::default()
            },
        }
    }

    /// Fold the stored layers onto the built-in defaults, keeping the sections
    /// one viewer owns.
    ///
    /// Fold order: the app layer, then the theme layer, then the keybinding
    /// layer.
    #[must_use]
    pub fn resolve_effective_client_config(&self) -> ClientConfig {
        merge_client(
            ClientConfig::default(),
            vec![
                self.app_config_layer.clone(),
                self.theme_config_layer.clone(),
                self.keybindings_config_layer.clone(),
            ],
        )
    }
}

/// Overwrites `target_field` with `override_field_value` when the layer set one, leaving it
/// untouched otherwise.
fn merge_override_field<FieldValue>(
    target_field: &mut FieldValue,
    override_field_value: Option<FieldValue>,
) {
    if let Some(override_field_value) = override_field_value {
        *target_field = override_field_value;
    }
}

/// One config layer: every section of one config file, each optional. A
/// section left `None` leaves the lower layers untouched; a section set to
/// `Some` applies its own per-field overrides.
///
/// One layer covers both sides' sections, as one file holds both:
/// [`merge_server`] reads the [`ServerConfig`] ones and [`merge_client`] reads
/// the [`ClientConfig`] ones.
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
    /// Mouse behavior overrides.
    pub mouse: Option<PartialMouseConfig>,
    /// Copy overrides.
    pub copy: Option<PartialCopyConfig>,
    /// Terminal environment overrides.
    pub terminal: Option<PartialTerminalConfig>,
    /// Theme overrides.
    pub theme: Option<PartialThemeConfig>,
    /// Logging overrides.
    pub logging: Option<PartialLoggingConfig>,
    /// Self-update overrides.
    pub update: Option<PartialUpdateConfig>,
    /// Native image support override.
    pub supports_image_protocols: Option<bool>,
    /// Placement-animation override.
    pub should_reduce_motion: Option<bool>,
    /// Remote-reconnect override.
    pub should_reconnect_remote_session: Option<bool>,
    /// Beta-feature gate override.
    pub should_allow_beta_features: Option<bool>,
    /// Other-users gate override.
    pub should_allow_other_users: Option<bool>,
    /// Remote listen address override. The outer `Option` is whether this
    /// layer sets the field; the inner `Option` is the value (`None` = no
    /// address, so nothing binds).
    pub remote_listen: Option<Option<String>>,
    /// Shared sessions directory override. The outer `Option` is whether this
    /// layer sets the field; the inner `Option` is the value (`None` = the
    /// platform's machine-wide directory).
    pub shared_sessions_directory: Option<Option<PathBuf>>,
    /// Auto-close override.
    pub should_auto_close_session: Option<bool>,
}

impl PartialKoshiConfig {
    /// Applies the session-owned sections' overrides onto `server_config`, ignoring
    /// every viewer-owned section this layer carries.
    fn apply_to_server_config(self, server_config: &mut ServerConfig) {
        if let Some(pane) = self.pane {
            pane.apply_to_pane_config(&mut server_config.pane);
        }
        if let Some(scrollback) = self.scrollback {
            scrollback.apply_limits_to_scrollback(&mut server_config.scrollback);
        }
        if let Some(terminal) = self.terminal {
            terminal.apply_to_terminal_config(&mut server_config.terminal);
        }
        if let Some(logging) = self.logging {
            logging.apply_to_logging_config(&mut server_config.logging);
        }
        merge_override_field(
            &mut server_config.should_allow_beta_features,
            self.should_allow_beta_features,
        );
        merge_override_field(
            &mut server_config.should_allow_other_users,
            self.should_allow_other_users,
        );
        merge_override_field(&mut server_config.remote_listen, self.remote_listen);
        merge_override_field(
            &mut server_config.shared_sessions_directory,
            self.shared_sessions_directory,
        );
        merge_override_field(
            &mut server_config.should_auto_close_session,
            self.should_auto_close_session,
        );
    }

    /// Applies the viewer-owned sections' overrides onto `client_config`, ignoring
    /// every session-owned section this layer carries.
    fn apply_to_client_config(self, client_config: &mut ClientConfig) {
        if let Some(keybindings) = self.keybindings {
            keybindings.apply_to_keybindings_config(&mut client_config.keybindings);
        }
        if let Some(layout) = self.layout {
            layout.apply_to_layout_defaults(&mut client_config.layout);
        }
        if let Some(mouse) = self.mouse {
            mouse.apply_to_mouse_config(&mut client_config.mouse);
        }
        if let Some(copy) = self.copy {
            copy.apply_to_copy_config(&mut client_config.copy);
        }
        if let Some(scrollback) = self.scrollback {
            scrollback.apply_view_to_scrollback(&mut client_config.scrollback);
        }
        if let Some(theme) = self.theme {
            theme.apply_to_theme_config(&mut client_config.theme);
        }
        if let Some(logging) = self.logging {
            logging.apply_to_logging_config(&mut client_config.logging);
        }
        if let Some(update) = self.update {
            update.apply_to_update_config(&mut client_config.update);
        }
        merge_override_field(
            &mut client_config.supports_image_protocols,
            self.supports_image_protocols,
        );
        merge_override_field(
            &mut client_config.should_reduce_motion,
            self.should_reduce_motion,
        );
        merge_override_field(
            &mut client_config.should_reconnect_remote_session,
            self.should_reconnect_remote_session,
        );
    }

    /// The effective logging settings from this layer over the built-in
    /// defaults. Startup resolves logging on its own, before the full config
    /// merge, so tracing can decide whether — and how — to open the log file.
    #[must_use]
    pub fn get_logging_config(&self) -> LoggingConfig {
        let mut logging_config = LoggingConfig::default();
        if let Some(logging) = self.logging {
            logging.apply_to_logging_config(&mut logging_config);
        }
        logging_config
    }
}

/// Self-update overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialUpdateConfig {
    /// Whether an interactive launch checks for a newer release when due.
    pub should_auto_check_for_updates: Option<bool>,
    /// Days between startup update checks.
    pub check_interval_days: Option<u32>,
    /// Whether a pre-release build counts as a newer version.
    pub should_allow_prerelease_updates: Option<bool>,
}

impl PartialUpdateConfig {
    fn apply_to_update_config(self, update_config: &mut UpdateConfig) {
        merge_override_field(
            &mut update_config.should_auto_check_for_updates,
            self.should_auto_check_for_updates,
        );
        merge_override_field(
            &mut update_config.check_interval_days,
            self.check_interval_days,
        );
        merge_override_field(
            &mut update_config.should_allow_prerelease_updates,
            self.should_allow_prerelease_updates,
        );
    }
}

/// Pane sizing overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialPaneConfig {
    /// Minimum pane width in columns.
    pub minimum_column_count: Option<u16>,
    /// Minimum pane height in rows.
    pub minimum_row_count: Option<u16>,
    /// Blank cells between two panes that meet along a horizontal or vertical
    /// split. `0` places panes edge to edge.
    pub gap_cell_count: Option<u16>,
}

impl PartialPaneConfig {
    fn apply_to_pane_config(self, pane_config: &mut PaneConfig) {
        merge_override_field(
            &mut pane_config.minimum_column_count,
            self.minimum_column_count,
        );
        merge_override_field(&mut pane_config.minimum_row_count, self.minimum_row_count);
        merge_override_field(&mut pane_config.gap_cell_count, self.gap_cell_count);
    }
}

/// Scrollback cap overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialScrollbackConfig {
    /// Maximum retained lines per pane.
    pub maximum_line_count: Option<usize>,
    /// Maximum retained bytes of scrollback text per pane.
    pub maximum_byte_count: Option<usize>,
    /// Whether input to a pane snaps its scrolled-up view back to live output.
    pub should_scroll_to_input: Option<bool>,
}

impl PartialScrollbackConfig {
    /// The caps half, folded onto the session's buffer limits.
    fn apply_limits_to_scrollback(self, scrollback_limits: &mut ScrollbackLimits) {
        merge_override_field(
            &mut scrollback_limits.maximum_line_count,
            self.maximum_line_count,
        );
        merge_override_field(
            &mut scrollback_limits.maximum_byte_count,
            self.maximum_byte_count,
        );
    }

    /// The view half, folded onto one viewer's follow behavior.
    fn apply_view_to_scrollback(self, scrollback_view: &mut ScrollbackView) {
        merge_override_field(
            &mut scrollback_view.should_scroll_to_input,
            self.should_scroll_to_input,
        );
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
    /// Per-mode bindings by mode name. When set, the whole map replaces the lower layer's;
    /// per-mode keymap merging is done by the keymap-merge pass.
    pub mode_bindings_by_name: Option<BTreeMap<ModeName, ModeBindings>>,
    /// Replacement chord for the reserved unlock. The outer `Option` is
    /// whether this layer sets the field; the inner `Option` is the value
    /// (`None` = keep the built-in unlock key).
    pub unlock_alternative: Option<Option<KeyChord>>,
}

impl PartialKeybindingsConfig {
    fn apply_to_keybindings_config(self, keybindings_config: &mut KeybindingsConfig) {
        merge_override_field(
            &mut keybindings_config.chord_timeout_ms,
            self.chord_timeout_ms,
        );
        merge_override_field(
            &mut keybindings_config.which_key_delay_ms,
            self.which_key_delay_ms,
        );
        merge_override_field(
            &mut keybindings_config.max_chord_depth,
            self.max_chord_depth,
        );
        merge_override_field(&mut keybindings_config.leader, self.leader);
        // Replace the whole mode map; per-mode keymap merging runs separately.
        merge_override_field(
            &mut keybindings_config.mode_bindings_by_name,
            self.mode_bindings_by_name,
        );
        merge_override_field(
            &mut keybindings_config.unlock_alternative,
            self.unlock_alternative,
        );
    }
}

/// Layout default overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialLayoutDefaults {
    /// Direction a new pane spawns relative to the focused pane.
    pub new_pane_direction: Option<Direction>,
}

impl PartialLayoutDefaults {
    fn apply_to_layout_defaults(self, layout_defaults: &mut LayoutDefaults) {
        merge_override_field(
            &mut layout_defaults.new_pane_direction,
            self.new_pane_direction,
        );
    }
}

/// Mouse behavior overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialMouseConfig {
    /// Whether dragging a pane border resizes it.
    pub can_resize_pane_border: Option<bool>,
    /// Lines scrolled per mouse wheel notch.
    pub scroll_line_count: Option<u16>,
    /// What the wheel does over a plain pane.
    pub wheel: Option<WheelScroll>,
}

impl PartialMouseConfig {
    fn apply_to_mouse_config(self, mouse_config: &mut MouseConfig) {
        merge_override_field(
            &mut mouse_config.can_resize_pane_border,
            self.can_resize_pane_border,
        );
        merge_override_field(&mut mouse_config.scroll_line_count, self.scroll_line_count);
        merge_override_field(&mut mouse_config.wheel, self.wheel);
    }
}

/// Copy overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialCopyConfig {
    /// Whether trailing whitespace is trimmed from copied text.
    pub should_trim_trailing_whitespace: Option<bool>,
}

impl PartialCopyConfig {
    fn apply_to_copy_config(self, copy_config: &mut CopyConfig) {
        merge_override_field(
            &mut copy_config.should_trim_trailing_whitespace,
            self.should_trim_trailing_whitespace,
        );
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
    /// What a pane's program receives for a key whose legacy bytes another key
    /// also owns.
    pub extended_keys_mode: Option<ExtendedKeysMode>,
}

impl PartialTerminalConfig {
    fn apply_to_terminal_config(self, terminal_config: &mut TerminalConfig) {
        merge_override_field(&mut terminal_config.term, self.term);
        merge_override_field(&mut terminal_config.colorterm, self.colorterm);
        merge_override_field(&mut terminal_config.default_shell, self.default_shell);
        merge_override_field(
            &mut terminal_config.extended_keys_mode,
            self.extended_keys_mode,
        );
    }
}

/// Theme overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialThemeConfig {
    /// The theme's display name.
    pub theme_name: Option<String>,
    /// Per-role color overrides.
    pub colors: Option<PartialColorPalette>,
}

impl PartialThemeConfig {
    fn apply_to_theme_config(self, theme_config: &mut ThemeConfig) {
        merge_override_field(&mut theme_config.theme_name, self.theme_name);
        if let Some(colors) = self.colors {
            colors.apply_to_color_palette(&mut theme_config.colors);
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
    /// Border of the pane the pointer is hovering over.
    pub border_hover: Option<RgbColor>,
    /// Text of a collapsed stack member's header strip.
    pub stack_header_fg: Option<RgbColor>,
    /// Background of a collapsed stack member's header strip.
    pub stack_header_bg: Option<RgbColor>,
    /// Backdrop of the letterbox margin around a centered layout.
    pub letterbox: Option<RgbColor>,
    /// Background filling the tab bar and the key-hint bar.
    pub bar_bg: Option<RgbColor>,
}

impl PartialColorPalette {
    fn apply_to_color_palette(self, color_palette: &mut ColorPalette) {
        merge_override_field(&mut color_palette.ramp_start, self.ramp_start);
        merge_override_field(&mut color_palette.ramp_end, self.ramp_end);
        merge_override_field(&mut color_palette.on_ramp, self.on_ramp);
        merge_override_field(&mut color_palette.on_ramp_dim, self.on_ramp_dim);
        merge_override_field(&mut color_palette.accent, self.accent);
        merge_override_field(&mut color_palette.on_accent, self.on_accent);
        merge_override_field(&mut color_palette.border_focused, self.border_focused);
        merge_override_field(&mut color_palette.border_unfocused, self.border_unfocused);
        merge_override_field(&mut color_palette.border_hover, self.border_hover);
        merge_override_field(&mut color_palette.stack_header_fg, self.stack_header_fg);
        merge_override_field(&mut color_palette.stack_header_bg, self.stack_header_bg);
        merge_override_field(&mut color_palette.letterbox, self.letterbox);
        merge_override_field(&mut color_palette.bar_bg, self.bar_bg);
    }
}

/// Logging overrides.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PartialLoggingConfig {
    /// Whether koshi writes a log file.
    pub is_enabled: Option<bool>,
    /// The lowest severity written to the log file.
    pub level: Option<LogLevel>,
    /// How each written log line is rendered.
    pub log_format: Option<LogFormat>,
}

impl PartialLoggingConfig {
    fn apply_to_logging_config(self, logging_config: &mut LoggingConfig) {
        merge_override_field(&mut logging_config.is_enabled, self.is_enabled);
        merge_override_field(&mut logging_config.level, self.level);
        merge_override_field(&mut logging_config.log_format, self.log_format);
    }
}

#[cfg(test)]
mod tests;
