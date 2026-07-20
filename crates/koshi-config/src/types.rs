//! The typed configuration schema and its built-in defaults.
//!
//! [`KoshiConfig`] is the whole in-memory config tree the runtime reads. Every
//! field has a default via [`Default`], so Koshi runs with zero user config;
//! [`KoshiConfig::default`] is the baseline that user overrides layer onto. This
//! module owns the schema and defaults only. The sibling [`layer`](crate::layer)
//! module folds override layers onto these defaults, and
//! [`keybinding`](crate::keybinding) parses keybinding-file KDL into its
//! layer; discovering and reading config files, full validation, and
//! migration live in later loader passes.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use koshi_core::action::ActionRef;
use koshi_core::geometry::Direction;
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags};
use koshi_core::log::{LogFormat, LogLevel};
use koshi_core::resolve::ActionArgs;

use crate::error::ColorParseError;
use crate::key::Leader;
use crate::key_sequence::parse_sequence;

/// The config schema version written to and read from disk. Bumped when the
/// on-disk shape changes, so an older file can be recognized by its version
/// number and migrated forward to the current shape.
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
    /// Keybinding timing, chord depth, leader prefix, and per-mode bindings.
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
    /// Log-file behavior.
    pub logging: LoggingConfig,
    /// Self-update checking behavior.
    pub update: UpdateConfig,
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
            logging: LoggingConfig::default(),
            update: UpdateConfig::default(),
        }
    }
}

/// Self-update checking behavior. `koshi update` reads these to decide whether
/// to look for a newer release on startup, how often, and whether pre-releases
/// count as updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateConfig {
    /// Whether an interactive launch checks GitHub for a newer release when a
    /// check is due.
    pub auto_check: bool,
    /// Days to wait between startup update checks.
    pub check_interval_days: u32,
    /// Whether a pre-release build counts as a newer version to update to.
    pub allow_prerelease: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            auto_check: true,
            check_interval_days: 14,
            allow_prerelease: false,
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
    /// Whether input you send to a pane snaps its view back to the newest line
    /// when you had scrolled up into history. On for a live feel: type or paste
    /// and the view jumps to the prompt. Off to stay parked in history while the
    /// input still goes through. Only the primary screen follows; the alternate
    /// screen's scroll position belongs to the full-screen program on it.
    pub scroll_on_input: bool,
}

impl Default for ScrollbackConfig {
    fn default() -> Self {
        Self {
            max_lines: 10_000,
            max_bytes: 32 * 1024 * 1024,
            scroll_on_input: true,
        }
    }
}

/// Keybinding timing, chord depth, leader prefix, and per-mode bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingsConfig {
    /// Milliseconds to wait for the next chord in a multi-key sequence.
    pub chord_timeout_ms: u32,
    /// Milliseconds before the which-key continuation hint appears.
    pub which_key_delay_ms: u32,
    /// Maximum number of chords in one key sequence.
    pub max_chord_depth: u8,
    /// The prefix that `<leader>` in a binding resolves to. A modifier run
    /// merges into the chord that follows it; a chord stands on its own.
    pub leader: Leader,
    /// Bindings grouped by input mode. `Default` ships the built-in binding
    /// set (`normal` plus the reserved unlock in `locked`); user layers
    /// override it at merge.
    pub modes: BTreeMap<ModeName, ModeBindings>,
    /// Replacement chord for the reserved unlock. When set, this chord (not
    /// [`RESERVED_UNLOCK`](Self::RESERVED_UNLOCK)) is the guaranteed
    /// locked-mode escape: conflict detection requires it bound to
    /// `core:unlock` in locked mode and refuses a typeable chord, and the
    /// default unlock key becomes free to rebind.
    pub unlock_alternative: Option<KeyChord>,
}

impl KeybindingsConfig {
    /// The reserved unlock chord — the same chord that locks in normal mode,
    /// so one key flips the client both ways. In `locked` mode this chord
    /// fires `core:unlock` and is intercepted ahead of pane pass-through;
    /// validation refuses a config that removes it without naming an
    /// explicit alternative.
    pub const RESERVED_UNLOCK: KeyChord = KeyChord::new(ModFlags::CTRL, Key::Char('l'));
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        Self {
            chord_timeout_ms: 500,
            which_key_delay_ms: 300,
            max_chord_depth: 4,
            leader: Leader::default(),
            modes: default_mode_bindings(Leader::default()),
            unlock_alternative: None,
        }
    }
}

/// The name of an input mode (`normal`, `locked`, `resize`, …), stored as a
/// plain string so plugins can register additional mode names beyond the
/// built-in set.
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

/// The action a key sequence triggers: the action reference plus the
/// arguments bound at the binding site.
///
/// Bindings carry no arguments in practice: an action choice with a fixed
/// set of values lives in the action name (`new-pane-left`,
/// `close-pane-tree`), and open-range values are reachable only through CLI
/// commands. The `args` field remains for system-authored presets — plugin
/// manifests may pair their own actions with arguments; user keybinding
/// surfaces bind a key to an action reference only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundAction {
    /// The action to resolve when the sequence fires.
    pub action: ActionRef,
    /// The arguments handed to action resolution alongside it.
    pub args: ActionArgs,
}

/// The bindings for one input mode, keyed by the key sequence pressed.
///
/// The map key is the sequence, so one sequence resolves to exactly one
/// action by construction — the hard binding invariant. Several sequences
/// may name the same action (`<C-p> <Left>` and `<A-h>` both bind
/// `focus-pane-left` in the defaults).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModeBindings {
    /// Key sequence → the action it triggers.
    pub keys: BTreeMap<KeySequence, BoundAction>,
    /// Key sequences this surface clears: a removed key voids whatever any
    /// lower-precedence layer bound on it, leaving the key free for this or
    /// a higher layer to rebind. Authored as `remove "<C-x>"` in a mode
    /// block. The built-in defaults carry none — no layer sits below them.
    pub removed: BTreeSet<KeySequence>,
}

/// The built-in default binding table: the `normal`-mode set plus the
/// reserved unlock, quit, and mouse-select in `locked` mode.
///
/// Every sequence OPENS with a non-typeable chord (Ctrl or Alt held) — with
/// ONE owner-chosen exception, the bare `Tab`/`Shift+Tab` tab-switching pair
/// (2026-07-12): outside locked mode the keymap owns Tab, and a shell sees a
/// literal Tab only while the client is locked. A later chord in a sequence
/// may be a plain key, since it is only read while the pending sequence is
/// live. No opening chord uses `<C-i>`, `<C-m>`, `<C-[>`, or `<C-h>`, which
/// unix terminals without the kitty keyboard protocol cannot tell apart from
/// Tab, Enter, Esc, and Backspace. Pane operations — lifecycle, directional
/// splits, and directional focus — live under the `<C-p>` prefix and resize
/// under the `<C-s>` prefix. Every binding is argless: an action choice with
/// a fixed set of values is part of the action name (`new-pane-left`,
/// `close-pane-tree`), so any key here can be rebound from `keybinding.kdl`.
/// Action names here are compile-time constants known to satisfy the
/// action-name grammar; an invalid one is a bug in this table and is caught
/// by its tests.
pub fn default_mode_bindings(leader: Leader) -> BTreeMap<ModeName, ModeBindings> {
    // Leader-relative bindings are written with `<leader>` and resolved against
    // `leader`, so rebinding the leader moves them; explicit chords (the Alt
    // alternatives, the reserved unlock) are written literally and never move.
    let seq = |text: &str| {
        parse_sequence(text, leader, u8::MAX).expect("a built-in default binding must parse")
    };
    let reserved = || KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK);
    let bound = |name: &str| BoundAction {
        action: ActionRef::core(name)
            .expect("default binding action name must satisfy the action-name grammar"),
        args: ActionArgs::None,
    };

    let normal: BTreeMap<KeySequence, BoundAction> = [
        // Lock — the reserved chord, explicit. It locks here and unlocks in
        // locked mode, one key both ways, and never moves with the leader so
        // the escape stays where a locked pane expects it.
        (reserved(), bound("lock")),
        // Quit and mouse-select — leader-relative. Mouse-select grabs the
        // mouse so a drag highlights in koshi even over a program that asked
        // for it; it is bound in locked mode too.
        (seq("<leader>q"), bound("quit")),
        (seq("<leader>g"), bound("mouse-select")),
        // Pane lifecycle, under the leader then `p`. `n` splits in the
        // configured default direction; the vim letters pick the side.
        (seq("<leader>p n"), bound("new-pane")),
        (seq("<leader>p h"), bound("new-pane-left")),
        (seq("<leader>p j"), bound("new-pane-down")),
        (seq("<leader>p k"), bound("new-pane-up")),
        (seq("<leader>p l"), bound("new-pane-right")),
        // The close key kills the pane's whole process group.
        (seq("<leader>p x"), bound("close-pane-tree")),
        // Fullscreen — an explicit Alt alternative.
        (seq("<A-f>"), bound("toggle-pane-fullscreen")),
        // Directional focus: arrows under the pane prefix, and explicit Alt+vim
        // letters. Both fire continuous actions, so the prefix stays armed
        // after each press.
        (seq("<leader>p <Left>"), bound("focus-pane-left")),
        (seq("<leader>p <Down>"), bound("focus-pane-down")),
        (seq("<leader>p <Up>"), bound("focus-pane-up")),
        (seq("<leader>p <Right>"), bound("focus-pane-right")),
        (seq("<A-h>"), bound("focus-pane-left")),
        (seq("<A-j>"), bound("focus-pane-down")),
        (seq("<A-k>"), bound("focus-pane-up")),
        (seq("<A-l>"), bound("focus-pane-right")),
        // Resize: one cell per press, under the leader then `s`.
        (seq("<leader>s h"), bound("resize-pane-left")),
        (seq("<leader>s j"), bound("resize-pane-down")),
        (seq("<leader>s k"), bound("resize-pane-up")),
        (seq("<leader>s l"), bound("resize-pane-right")),
        // Copy and paste have NO bindings — they follow the OS.
        // Tabs. New tab is an explicit Alt binding; switching is the bare
        // Tab / Shift+Tab pair — an OWNER-CHOSEN exception, never
        // leader-relative: outside locked mode the keymap owns Tab, so a shell
        // sees a literal Tab only while the client is locked.
        (seq("<A-t>"), bound("new-tab")),
        (seq("<Tab>"), bound("next-tab")),
        (seq("<S-Tab>"), bound("previous-tab")),
    ]
    .into_iter()
    .collect();

    // Locked mode intercepts exactly its bound chords and passes every other
    // key to the pane: the reserved unlock (the same chord that locks in
    // normal mode) as the guaranteed escape, plus the quit chord so quitting
    // works from either side of the lock.
    let locked: BTreeMap<KeySequence, BoundAction> = [
        (reserved(), bound("unlock")),
        (seq("<leader>q"), bound("quit")),
        (seq("<leader>g"), bound("mouse-select")),
    ]
    .into_iter()
    .collect();

    BTreeMap::from([
        (
            ModeName::new("normal"),
            ModeBindings {
                keys: normal,
                removed: BTreeSet::new(),
            },
        ),
        (
            ModeName::new("locked"),
            ModeBindings {
                keys: locked,
                removed: BTreeSet::new(),
            },
        ),
    ])
}

/// The display labels for the default binding table's prefix chords, keyed by
/// the opening chord of the multi-chord sequences it groups.
///
/// The hint bar shows a prefix's label (`<C-p> PANE`) only while every binding
/// under that prefix still comes from the untouched defaults; once any user
/// surface overrides, adds, or removes a binding under it, the group falls
/// back to a derived `+N` marker, since the shipped label no longer describes
/// the set. Lives beside the default binding table so the labels and the
/// sequences they describe change together.
#[must_use]
pub fn default_prefix_labels(leader: Leader) -> BTreeMap<KeyChord, String> {
    // Key by the opening chord of each prefix, resolved against the leader, so
    // the label follows the prefix when the leader is rebound.
    let opening = |text: &str| {
        *parse_sequence(text, leader, u8::MAX)
            .expect("a built-in prefix must parse")
            .chords()
            .first()
            .expect("a prefix sequence has an opening chord")
    };
    let pane = opening("<leader>p");
    let resize = opening("<leader>s");
    // A modifier-run leader opens these at distinct chords (`<C-p>`, `<C-s>`),
    // each naming its own group. A chord leader (e.g. `<Space>`) makes both open
    // the SAME leader chord, so no single group label fits it — leave it
    // unlabeled and let the hint bar show its derived `+N` count instead.
    if pane == resize {
        return BTreeMap::new();
    }
    BTreeMap::from([(pane, "PANE".to_string()), (resize, "RESIZE".to_string())])
}

/// Defaults applied when creating panes and layouts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutDefaults {
    /// Direction a new pane spawns relative to the focused pane when the command
    /// omits one. The CLI new-pane command and the `new-pane-<direction>`
    /// actions name their own direction and bypass it.
    pub new_pane_direction: Direction,
}

impl Default for LayoutDefaults {
    fn default() -> Self {
        Self {
            new_pane_direction: Direction::Right,
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
    /// Lines scrolled per mouse wheel notch.
    pub scroll_lines: u16,
    /// What the wheel does over a plain pane — one with no text highlighted, no
    /// program asking for the mouse, and no alternate-scroll mode on. The other
    /// cases are fixed: a highlight holds and scrolls koshi's own scrollback, a
    /// mouse-aware program gets the wheel as a report, and an alternate-screen
    /// program with `?1007` on gets arrow keys.
    pub wheel: WheelScroll,
}

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            border_resize: true,
            scroll_lines: 3,
            wheel: WheelScroll::default(),
        }
    }
}

/// What the mouse wheel does over a plain pane (see [`MouseConfig::wheel`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WheelScroll {
    /// Scroll koshi's own scrollback view of the pane the pointer is over.
    #[default]
    ScrollScrollback,
    /// Do nothing.
    Ignore,
}

/// Selection and clipboard behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyConfig {
    /// Whether completing a selection copies it immediately. Kept internal
    /// until copy actions and user keybindings can provide another copy path.
    pub copy_on_select: bool,
    /// Whether trailing whitespace is trimmed from copied text.
    pub trim_trailing_whitespace: bool,
    /// Which clipboard backend receives copied text.
    pub clipboard: ClipboardBackend,
}

impl Default for CopyConfig {
    fn default() -> Self {
        Self {
            copy_on_select: true,
            trim_trailing_whitespace: true,
            clipboard: ClipboardBackend::Osc52,
        }
    }
}

/// The clipboard backend copied text is written to. OSC 52 is the only backend
/// koshi builds today; a native operating-system backend adds its own variant
/// here without reshaping the copy flow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ClipboardBackend {
    /// Write to the outer terminal's clipboard via OSC 52.
    #[default]
    Osc52,
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
///
/// Chrome elements that come in runs — the tab ribbon, the hint bar's
/// modifier groups — are colored as a gradient between [`ramp_start`] and
/// [`ramp_end`], each element taking one interpolated stop by its position.
/// For example, `ramp_start "#ff0000"` with `ramp_end "#0000ff"` turns a
/// five-tab ribbon into five stops fading red → blue.
///
/// [`ramp_start`]: ColorPalette::ramp_start
/// [`ramp_end`]: ColorPalette::ramp_end
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorPalette {
    /// First endpoint of the chrome gradient, taken whole by the first
    /// element of a run.
    pub ramp_start: RgbColor,
    /// Second endpoint of the chrome gradient, taken whole by the last
    /// element of a run.
    pub ramp_end: RgbColor,
    /// Text drawn over a ramp-colored block.
    pub on_ramp: RgbColor,
    /// Text drawn over a dimmed ramp block.
    pub on_ramp_dim: RgbColor,
    /// The in-progress accent: marks the chords already pressed in a pending
    /// key sequence.
    pub accent: RgbColor,
    /// Text drawn over an accent block.
    pub on_accent: RgbColor,
    /// Border of the focused pane.
    pub border_focused: RgbColor,
    /// Border of unfocused panes.
    pub border_unfocused: RgbColor,
    /// Border of the pane the pointer is hovering over — the pane the wheel
    /// scrolls, marked so the target is visible before the wheel is turned.
    pub border_hover: RgbColor,
    /// Text of a collapsed stack member's header strip.
    pub stack_header_fg: RgbColor,
    /// Background of a collapsed stack member's header strip.
    pub stack_header_bg: RgbColor,
    /// Backdrop of the letterbox margin around a centered layout.
    pub letterbox: RgbColor,
    /// Background filling koshi's own two rows whole: the tab bar on top and
    /// the key-hint bar on the bottom.
    pub bar_bg: RgbColor,
}

impl Default for ColorPalette {
    /// The stock koshi chrome — a light-purple → light-blue ramp with a pink
    /// accent over black bars — applied when no theme is configured.
    fn default() -> Self {
        Self {
            ramp_start: RgbColor::new(0xd0, 0xa5, 0xff),
            ramp_end: RgbColor::new(0x7d, 0xbc, 0xff),
            on_ramp: RgbColor::new(0x12, 0x09, 0x1f),
            on_ramp_dim: RgbColor::new(0xf0, 0xec, 0xfa),
            accent: RgbColor::new(0xf5, 0xc2, 0xff),
            on_accent: RgbColor::new(0x1e, 0x10, 0x33),
            border_focused: RgbColor::new(0x00, 0xaf, 0xd7),
            border_unfocused: RgbColor::new(0x58, 0x58, 0x58),
            border_hover: RgbColor::new(0xaf, 0x5f, 0xff),
            stack_header_fg: RgbColor::new(0xf4, 0xf1, 0xfa),
            stack_header_bg: RgbColor::new(0x30, 0x0f, 0x4a),
            letterbox: RgbColor::new(0x58, 0x58, 0x58),
            bar_bg: RgbColor::new(0x00, 0x00, 0x00),
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
    ///
    /// # Errors
    /// - [`ColorParseError::BadLength`] if the value, after stripping a
    ///   leading `#`, is not exactly six characters.
    /// - [`ColorParseError::BadDigit`] if any of those six characters is not
    ///   a hex digit (`0-9`, `a-f`, `A-F`).
    pub fn from_hex(s: &str) -> Result<Self, ColorParseError> {
        // Accept the value with or without its leading `#`.
        let hex = s.strip_prefix('#').unwrap_or(s);
        let char_count = hex.chars().count();
        if char_count != 6 {
            return Err(ColorParseError::BadLength { got: char_count });
        }
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ColorParseError::BadDigit {
                value: hex.to_string(),
            });
        }
        // Six ASCII hex digits: one byte per character, so each two-byte
        // slice is valid ASCII and parses.
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

/// Log-file behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoggingConfig {
    /// Whether koshi writes a log file. Disabled, nothing is logged and no
    /// log file or `logs/` directory is created; enabled, log lines at or
    /// above [`level`](Self::level) are written to a per-session file under
    /// the platform state directory, created on the first line written.
    pub enabled: bool,
    /// The lowest severity that gets written. A line below this is dropped —
    /// e.g. [`LogLevel::Warning`] drops `info` lines.
    pub level: LogLevel,
    /// How each written line is rendered.
    pub format: LogFormat,
}

impl Default for LoggingConfig {
    /// Logging is off, and when turned on writes warnings and errors in the
    /// human-readable format.
    fn default() -> Self {
        Self {
            enabled: false,
            level: LogLevel::Warning,
            format: LogFormat::Pretty,
        }
    }
}

#[cfg(test)]
mod tests;
