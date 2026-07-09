//! The typed configuration schema and its built-in defaults.
//!
//! [`KoshiConfig`] is the whole in-memory config tree the runtime reads. Every
//! field has a default via [`Default`], so Koshi runs with zero user config;
//! [`KoshiConfig::default`] is the baseline that user overrides layer onto. This
//! module owns the schema and defaults only. The sibling [`layer`](crate::layer)
//! module folds override layers onto these defaults; parsing KDL into a layer,
//! discovering and reading config files, validation, and migration live in later
//! loader passes.

use std::collections::BTreeMap;
use std::str::FromStr;

use koshi_core::geometry::Direction;

use crate::error::ColorParseError;

/// The config schema version written to and read from disk. Bumped when the
/// on-disk shape changes so old files migrate forward instead of misparsing.
pub const SCHEMA_VERSION: u32 = 1;

/// The complete configuration tree. Each field is an independent section with
/// its own defaults, so a user file that sets one section leaves the rest at the
/// built-in defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KoshiConfig {
    /// The schema version this config was written against.
    pub version: u32,
    /// Pane sizing and framing defaults.
    pub pane: PaneConfig,
    /// Per-pane scrollback history caps.
    pub scrollback: ScrollbackConfig,
    /// Keybinding timing, chord depth, leader, and per-mode bindings.
    pub keybindings: KeybindingsConfig,
    /// Defaults applied when creating layouts.
    pub layout: LayoutDefaults,
    /// Per-plugin activation and keymap opt-in preferences.
    pub plugins: PluginActivationConfig,
    /// Mouse routing behavior.
    pub mouse: MouseConfig,
    /// Selection and clipboard behavior.
    pub copy: CopyConfig,
    /// Terminal environment presented to child processes.
    pub terminal: TerminalConfig,
    /// Color theme.
    pub theme: ThemeConfig,
}

impl Default for KoshiConfig {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            pane: PaneConfig::default(),
            scrollback: ScrollbackConfig::default(),
            keybindings: KeybindingsConfig::default(),
            layout: LayoutDefaults::default(),
            plugins: PluginActivationConfig::default(),
            mouse: MouseConfig::default(),
            copy: CopyConfig::default(),
            terminal: TerminalConfig::default(),
            theme: ThemeConfig::default(),
        }
    }
}

/// Pane sizing floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneConfig {
    /// Minimum pane width in columns.
    pub min_cols: u16,
    /// Minimum pane height in rows.
    pub min_rows: u16,
}

impl Default for PaneConfig {
    fn default() -> Self {
        Self {
            min_cols: 2,
            min_rows: 1,
        }
    }
}

/// Per-pane scrollback history caps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollbackConfig {
    /// Maximum retained lines per pane.
    pub max_lines: usize,
    /// Maximum retained bytes of scrollback text per pane.
    pub max_bytes: usize,
}

impl Default for ScrollbackConfig {
    fn default() -> Self {
        Self {
            max_lines: 10_000,
            max_bytes: 32 * 1024 * 1024,
        }
    }
}

/// Keybinding timing, chord depth, leader chord, and per-mode bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingsConfig {
    /// Milliseconds to wait for the next chord in a multi-key sequence.
    pub chord_timeout_ms: u32,
    /// Milliseconds before the which-key continuation hint appears.
    pub which_key_delay_ms: u32,
    /// Maximum number of chords in one key sequence.
    pub max_chord_depth: u8,
    /// The leader chord that `<leader>` in a binding resolves to.
    pub leader: String,
    /// Bindings grouped by input mode. Populated by the default keybinding set
    /// and user config; empty here since binding values are parsed later.
    pub modes: BTreeMap<ModeName, ModeBindings>,
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        Self {
            chord_timeout_ms: 500,
            which_key_delay_ms: 300,
            max_chord_depth: 4,
            leader: "Ctrl".to_string(),
            modes: BTreeMap::new(),
        }
    }
}

/// The name of an input mode (`normal`, `locked`, `resize`, …). A dynamic string
/// rather than a closed enum, so plugins may register additional modes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModeName(String);

impl ModeName {
    /// Wraps a mode name string.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The mode name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The bindings for one input mode. The binding value type (key sequence to
/// action) is added by the chord parser and action registry passes; this stub
/// carries the map slot so the schema shape exists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModeBindings {}

/// Defaults applied when creating panes and layouts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutDefaults {
    /// Direction a new pane spawns relative to the focused pane when the command
    /// omits one. The CLI new-pane command may override it per call.
    pub new_pane_direction: Direction,
    /// A named layout to load at startup, if any.
    pub default_layout: Option<String>,
}

impl Default for LayoutDefaults {
    fn default() -> Self {
        Self {
            new_pane_direction: Direction::Right,
            default_layout: None,
        }
    }
}

/// Per-plugin activation and keymap opt-in preferences. Empty by default;
/// entries come from the user's `plugins` config block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginActivationConfig {
    /// One entry per plugin the user configured.
    pub entries: Vec<PluginActivation>,
}

/// One plugin's activation preference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginActivation {
    /// The plugin identifier.
    pub name: String,
    /// Whether to enable or disable the plugin.
    pub action: ActivationAction,
    /// The scope the preference applies to.
    pub scope: ActivationScope,
    /// Which of the plugin's recommended keymaps to adopt.
    pub keymaps: KeymapOptIn,
}

/// Whether a plugin activation entry enables or disables the plugin.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ActivationAction {
    /// Enable the plugin.
    #[default]
    Enable,
    /// Disable the plugin.
    Disable,
}

/// The scope a plugin activation preference applies to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ActivationScope {
    /// Applies to every session.
    #[default]
    Global,
    /// Applies to the named session only.
    Session(String),
}

/// How much of a plugin's recommended keymap set to adopt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum KeymapOptIn {
    /// Adopt none of the plugin's keymaps.
    #[default]
    None,
    /// Adopt all of the plugin's recommended keymaps.
    Recommended,
    /// Adopt only the recommendations for the listed local action names, at
    /// whatever key the plugin currently recommends for each.
    Subset(Vec<String>),
}

/// Mouse routing behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MouseConfig {
    /// Whether dragging a pane border resizes it.
    pub border_resize: bool,
    /// Whether border-drag resize also works in locked mode.
    pub border_resize_in_lock: bool,
    /// Whether clicking a pane focuses it.
    pub click_to_focus: bool,
    /// Lines scrolled per mouse wheel notch.
    pub scroll_lines: u16,
}

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            border_resize: true,
            border_resize_in_lock: false,
            click_to_focus: true,
            scroll_lines: 3,
        }
    }
}

/// Selection and clipboard behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyConfig {
    /// Whether completing a selection copies it immediately.
    pub copy_on_select: bool,
    /// Whether trailing whitespace is trimmed from copied text.
    pub trim_trailing_whitespace: bool,
    /// Which clipboard backend receives copied text.
    pub clipboard: ClipboardBackend,
}

impl Default for CopyConfig {
    fn default() -> Self {
        Self {
            copy_on_select: false,
            trim_trailing_whitespace: true,
            clipboard: ClipboardBackend::Osc52,
        }
    }
}

/// The clipboard backend copied text is written to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ClipboardBackend {
    /// Write to the outer terminal's clipboard via OSC 52.
    #[default]
    Osc52,
    /// Write to the operating system clipboard.
    Native,
    /// Write to both the OSC 52 and the OS clipboard.
    Both,
}

/// Terminal environment presented to child processes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalConfig {
    /// The `TERM` value advertised to child programs.
    pub term: String,
    /// The `COLORTERM` value advertised to child programs.
    pub colorterm: String,
    /// The shell to launch; `None` falls back to the user's `$SHELL`.
    pub default_shell: Option<String>,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            term: "xterm-256color".to_string(),
            colorterm: "truecolor".to_string(),
            default_shell: None,
        }
    }
}

/// A named color theme and its palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeConfig {
    /// The theme's display name.
    pub name: String,
    /// The theme's colors.
    pub colors: ColorPalette,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            colors: ColorPalette::default(),
        }
    }
}

/// The set of colors the renderer draws chrome with. Each field names one
/// role; the renderer maps its chrome styles onto these when themed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorPalette {
    /// Default text foreground.
    pub foreground: RgbColor,
    /// Default background.
    pub background: RgbColor,
    /// Highlight color for focus and active elements.
    pub accent: RgbColor,
    /// Border of the focused pane.
    pub border_focused: RgbColor,
    /// Border of unfocused panes.
    pub border_unfocused: RgbColor,
    /// Foreground of the active tab.
    pub tab_active_fg: RgbColor,
    /// Background of the active tab.
    pub tab_active_bg: RgbColor,
    /// Foreground of inactive tabs.
    pub tab_inactive_fg: RgbColor,
    /// Background of inactive tabs.
    pub tab_inactive_bg: RgbColor,
    /// Foreground of the mode indicator.
    pub mode_fg: RgbColor,
    /// Background of the mode indicator.
    pub mode_bg: RgbColor,
    /// Foreground of a stacked-pane header.
    pub stack_header_fg: RgbColor,
    /// Background of a stacked-pane header.
    pub stack_header_bg: RgbColor,
    /// The key glyph in a keybinding hint.
    pub hint_key: RgbColor,
    /// The label text in a keybinding hint.
    pub hint_label: RgbColor,
    /// Background of the keybinding hint bar.
    pub hint_bg: RgbColor,
}

impl Default for ColorPalette {
    /// A dark theme applied when no theme is configured.
    fn default() -> Self {
        Self {
            foreground: RgbColor::new(0xd4, 0xd4, 0xd4),
            background: RgbColor::new(0x1e, 0x1e, 0x1e),
            accent: RgbColor::new(0x00, 0xaf, 0xd7),
            border_focused: RgbColor::new(0x00, 0xaf, 0xd7),
            border_unfocused: RgbColor::new(0x58, 0x58, 0x58),
            tab_active_fg: RgbColor::new(0x1e, 0x1e, 0x1e),
            tab_active_bg: RgbColor::new(0xd4, 0xd4, 0xd4),
            tab_inactive_fg: RgbColor::new(0x80, 0x80, 0x80),
            tab_inactive_bg: RgbColor::new(0x1e, 0x1e, 0x1e),
            mode_fg: RgbColor::new(0x1e, 0x1e, 0x1e),
            mode_bg: RgbColor::new(0x00, 0xaf, 0x5f),
            stack_header_fg: RgbColor::new(0x1e, 0x1e, 0x1e),
            stack_header_bg: RgbColor::new(0x80, 0x80, 0x80),
            hint_key: RgbColor::new(0x00, 0xaf, 0xd7),
            hint_label: RgbColor::new(0xd4, 0xd4, 0xd4),
            hint_bg: RgbColor::new(0x30, 0x30, 0x30),
        }
    }
}

/// A 24-bit truecolor value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbColor {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

impl RgbColor {
    /// Builds a color from its red, green, and blue channels.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parses a `#RRGGBB` (or bare `RRGGBB`) hex string into a color.
    pub fn from_hex(s: &str) -> Result<Self, ColorParseError> {
        let hex = s.strip_prefix('#').unwrap_or(s);
        let bytes = hex.as_bytes();
        if bytes.len() != 6 {
            return Err(ColorParseError::BadLength {
                got: hex.chars().count(),
            });
        }
        if !bytes.iter().all(u8::is_ascii_hexdigit) {
            return Err(ColorParseError::BadDigit {
                value: hex.to_string(),
            });
        }
        // Every byte is an ASCII hex digit, so each two-byte slice is valid
        // ASCII and parses.
        let component = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).expect("validated hex");
        Ok(Self::new(component(0), component(2), component(4)))
    }
}

impl FromStr for RgbColor {
    type Err = ColorParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

#[cfg(test)]
mod tests;
