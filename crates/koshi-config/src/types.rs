//! The typed configuration schema and its built-in defaults.
//!
//! The tree is split by who reads it: [`ServerConfig`] holds what one session
//! shares across every viewer (the layout floor, scrollback caps, the child
//! environment), and [`ClientConfig`] holds what one viewer decides for itself
//! (keybindings, theme, mouse, copy). Both come from the same `koshi.kdl`;
//! each side folds only the sections it owns, so a viewer cannot set the shell
//! a session spawns and a session cannot set a viewer's colors.
//!
//! Every field has a default via [`Default`], so Koshi runs with zero user
//! config, and each side's `default()` is the baseline user overrides layer
//! onto. This module owns the schema and defaults only. The sibling
//! [`layer`](crate::layer) module folds override layers onto these defaults,
//! [`keybinding`](crate::keybinding) parses keybinding-file KDL, and
//! [`migration`](crate::migration) validates versioned files and moves them
//! through adjacent schemas. Disk discovery and reading live in the binary.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::str::FromStr;

use koshi_core::action::ActionRef;
use koshi_core::geometry::Direction;
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags};
use koshi_core::log::{LogFormat, LogLevel};
use koshi_core::resolve::ActionArgs;

use crate::error::ColorParseError;
use crate::key::Leader;
use crate::key_sequence::parse_sequence;

/// The config schema version written to and read from disk, bumped when the
/// on-disk shape changes. A file declaring an older version is migrated
/// forward to this shape; a file declaring a newer one is refused.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::CONFIG_SCHEMA`].
pub const SCHEMA_VERSION: u32 = koshi_core::compat::CONFIG_SCHEMA.max;

/// The name of the built-in theme, whose colors are compiled into koshi. It is
/// the theme in effect when `koshi.kdl` names no theme, names this one, or
/// names one whose `themes/<name>.kdl` cannot be loaded.
pub const DEFAULT_THEME: &str = "default";

/// The settings the session host reads: the shared layout floor, the
/// scrollback buffers it owns, the environment it spawns children into, its
/// own log file, and who else on this machine may reach it.
///
/// One session has one of these however many viewers are attached. Every
/// field here describes something all of them share. A viewer's own
/// preferences are [`ClientConfig`], and the two are read from the same
/// `koshi.kdl` — each side folding the sections it owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// The schema version this config was written against.
    pub version: u32,
    /// Pane sizing floor for the shared layout.
    pub pane: PaneConfig,
    /// Per-pane scrollback history caps.
    pub scrollback: ScrollbackLimits,
    /// Terminal environment presented to child processes.
    pub terminal: TerminalConfig,
    /// Log-file behavior for this process.
    pub logging: LoggingConfig,
    /// Whether entry points marked `#[beta_feature]` may run.
    pub allow_beta_features: bool,
    /// Whether other users of this machine may reach this session's socket.
    pub allow_other_users: bool,
    /// The TCP address the remote listener binds, such as `"0.0.0.0:7654"`.
    /// Setting it opens nothing; `koshi share grant` switches remote access on.
    pub remote_listen: Option<String>,
    /// The directory the session sockets other users reach live in. `None`
    /// takes the platform's machine-wide directory, `/tmp/koshi` on Unix and
    /// `%ProgramData%\koshi` on Windows.
    pub shared_sessions_dir: Option<PathBuf>,
    /// Whether the session ends when its last client leaves.
    pub auto_close_session: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            pane: PaneConfig::default(),
            scrollback: ScrollbackLimits::default(),
            terminal: TerminalConfig::default(),
            logging: LoggingConfig::default(),
            allow_beta_features: false,
            allow_other_users: false,
            remote_listen: None,
            shared_sessions_dir: None,
            auto_close_session: false,
        }
    }
}

/// The settings one viewer reads: how its keyboard and mouse are interpreted,
/// what it paints with, and what it does with copied text.
///
/// Each attached viewer holds its own, read from the `koshi.kdl` on the
/// machine it runs on, so two viewers of one session can bind different keys
/// and paint different colors. The settings the session itself needs are
/// [`ServerConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientConfig {
    /// The schema version this config was written against.
    pub version: u32,
    /// Keybinding timing, chord depth, leader prefix, and per-mode bindings.
    pub keybindings: KeybindingsConfig,
    /// Defaults applied when creating panes and layouts.
    pub layout: LayoutDefaults,
    /// Per-plugin activation and keymap opt-in preferences.
    pub plugins: PluginActivationConfig,
    /// Mouse routing behavior.
    pub mouse: MouseConfig,
    /// Selection and clipboard behavior.
    pub copy: CopyConfig,
    /// How this viewer's scrollback view follows live output.
    pub scrollback: ScrollbackView,
    /// Color theme.
    pub theme: ThemeConfig,
    /// Log-file behavior for this process.
    pub logging: LoggingConfig,
    /// Self-update checking behavior.
    pub update: UpdateConfig,
    /// Whether a viewer whose link to a session on another machine drops dials
    /// that machine again by itself. While it dials, the viewer draws
    /// `RECONNECTING` on its tab strip and keeps trying for up to 120 seconds,
    /// and joining again puts back the tab, the focused and zoomed pane of each
    /// tab, and the scroll offset of each pane. `false` ends the viewer on a
    /// dropped link, with the message that names how to attach again by hand. A
    /// link to a session on this machine ends the viewer either way.
    pub remote_reconnect: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            keybindings: KeybindingsConfig::default(),
            layout: LayoutDefaults::default(),
            plugins: PluginActivationConfig::default(),
            mouse: MouseConfig::default(),
            copy: CopyConfig::default(),
            scrollback: ScrollbackView::default(),
            theme: ThemeConfig::default(),
            logging: LoggingConfig::default(),
            update: UpdateConfig::default(),
            remote_reconnect: true,
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
    /// Blank cells between two panes that meet along a horizontal or vertical
    /// split. `0` places panes edge to edge.
    pub gap: u16,
}

impl Default for PaneConfig {
    fn default() -> Self {
        Self {
            min_cols: 2,
            min_rows: 1,
            gap: 0,
        }
    }
}

/// Per-pane scrollback history caps. The buffer these bound lives in the
/// pane's terminal engine, so one pane has one set of caps however many
/// viewers it has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollbackLimits {
    /// Maximum retained lines per pane.
    pub max_lines: usize,
    /// Maximum retained bytes of scrollback text per pane.
    pub max_bytes: usize,
}

impl Default for ScrollbackLimits {
    fn default() -> Self {
        Self {
            max_lines: 10_000,
            max_bytes: 32 * 1024 * 1024,
        }
    }
}

/// How one viewer's scrollback view behaves. Held per viewer, so two viewers
/// of the same pane can follow live output differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbackView {
    /// Whether input you send to a pane snaps its view back to the newest line
    /// when you had scrolled up into history. On: type or paste and the view
    /// jumps to the prompt. Off: the view stays in history and the input still
    /// goes through. Only the primary screen follows; the alternate
    /// screen's scroll position belongs to the full-screen program on it.
    pub scroll_on_input: bool,
}

impl Default for ScrollbackView {
    fn default() -> Self {
        Self {
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
/// A user keybinding file binds a key to an action reference alone, so every
/// binding it produces carries [`ActionArgs::None`]: an action choice with a
/// fixed set of values lives in the action name (`new-pane-left`,
/// `close-pane-tree`), and open-range values are reachable only through CLI
/// commands. Plugin manifests may pair their own actions with arguments.
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
/// action. The reverse is open: several sequences in one mode may name the
/// same action, though no shipped default does — within a mode every default
/// action has exactly one key (`core:focus-pane-left` is reachable only as
/// `<C-p> <Left>`). An action bound in two modes is two entries in two maps:
/// `core:quit` is `<C-q>` in both `normal` and `locked`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModeBindings {
    /// Key sequence → the action it triggers.
    pub keys: BTreeMap<KeySequence, BoundAction>,
    /// Key sequences this surface clears: a removed key voids whatever any
    /// lower-precedence layer bound on it, leaving the key free for this or
    /// a higher layer to rebind. Authored as `remove "<C-x>"` in a mode
    /// block. The built-in defaults carry none.
    pub removed: BTreeSet<KeySequence>,
}

/// The built-in default binding table: the `normal`-mode set plus the
/// reserved unlock, quit, and mouse-select in `locked` mode.
///
/// Sequences written with `<leader>` resolve against `leader`, so rebinding
/// the leader moves them. Explicit chords — `<A-f>`, the reserved unlock, and
/// the `Tab`/`Shift+Tab` pair — are written literally and never move.
///
/// Under the default `C-` leader every sequence OPENS with a non-typeable
/// chord (Ctrl or Alt held), with one exception: the bare `Tab`/`Shift+Tab`
/// tab-switching pair. Outside locked mode the keymap owns Tab, and a shell
/// sees a literal Tab only while the client is locked. A later chord in a
/// sequence may be a plain key; it is read only while the pending sequence is
/// live. No opening chord uses `<C-i>`, `<C-m>`, `<C-[>`, or `<C-h>`, which
/// unix terminals without the kitty keyboard protocol cannot tell apart from
/// Tab, Enter, Esc, and Backspace. Pane operations — lifecycle, directional
/// splits, and directional focus — live under the `<C-p>` prefix, resize under
/// `<C-s>`, and tab lifecycle under `<C-t>`. Every binding is argless: an
/// action choice with a fixed set of values is part of the action name
/// (`new-pane-left`, `close-pane-tree`), so any key here can be rebound from
/// `keybinding.kdl`.
pub fn default_mode_bindings(leader: Leader) -> BTreeMap<ModeName, ModeBindings> {
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
        // Lock — the reserved chord, written literally: it does not move with
        // the leader. The same chord unlocks in locked mode.
        (reserved(), bound("lock")),
        // Quit and mouse-select — leader-relative, and bound in locked mode
        // too. Mouse-select grabs the mouse: a drag highlights in koshi even
        // over a program that asked for the mouse itself.
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
        // Fullscreen — an explicit chord, so it stays put under any leader.
        (seq("<A-f>"), bound("toggle-pane-fullscreen")),
        // Directional focus: arrows under the pane prefix. These fire
        // continuous actions, so the prefix stays armed after each press.
        (seq("<leader>p <Left>"), bound("focus-pane-left")),
        (seq("<leader>p <Down>"), bound("focus-pane-down")),
        (seq("<leader>p <Up>"), bound("focus-pane-up")),
        (seq("<leader>p <Right>"), bound("focus-pane-right")),
        // Resize: one cell per press, arrows under the leader then `s`.
        (seq("<leader>s <Left>"), bound("resize-pane-left")),
        (seq("<leader>s <Down>"), bound("resize-pane-down")),
        (seq("<leader>s <Up>"), bound("resize-pane-up")),
        (seq("<leader>s <Right>"), bound("resize-pane-right")),
        // Copy and paste have NO bindings — they follow the OS.
        // Tab lifecycle, under the leader then `t`: `n` opens, `x` closes.
        // Switching is the bare Tab / Shift+Tab pair, written literally and
        // never leader-relative: outside locked mode the keymap owns Tab, and
        // a shell sees a literal Tab only while the client is locked.
        (seq("<leader>t n"), bound("new-tab")),
        (seq("<leader>t x"), bound("close-tab")),
        (seq("<Tab>"), bound("next-tab")),
        (seq("<S-Tab>"), bound("previous-tab")),
    ]
    .into_iter()
    .collect();

    // Locked mode intercepts exactly its bound chords and passes every other
    // key to the pane: the reserved unlock (the same chord that locks in
    // normal mode), the quit chord, and the mouse-select chord.
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
/// Returns three entries — `PANE`, `RESIZE`, `TAB` — when `leader` gives
/// `<leader>p`, `<leader>s`, and `<leader>t` three distinct opening chords,
/// and an empty map when it does not.
///
/// The hint bar shows a prefix's label (`<C-p> PANE`) only while every binding
/// under that prefix still comes from the untouched defaults; once any user
/// surface overrides, adds, or removes a binding under it, the group falls
/// back to a derived `+N` marker.
#[must_use]
pub fn default_prefix_labels(leader: Leader) -> BTreeMap<KeyChord, String> {
    let opening = |text: &str| {
        *parse_sequence(text, leader, u8::MAX)
            .expect("a built-in prefix must parse")
            .chords()
            .first()
            .expect("a prefix sequence has an opening chord")
    };
    let groups = [
        ("<leader>p", "PANE"),
        ("<leader>s", "RESIZE"),
        ("<leader>t", "TAB"),
    ];
    // `C-` gives each group its own opening chord: `<C-p> PANE`,
    // `<C-s> RESIZE`, `<C-t> TAB`. A chord leader opens every group at the
    // leader itself, so `<Space>` collapses all three onto one entry.
    let labels: BTreeMap<KeyChord, String> = groups
        .iter()
        .map(|(prefix, label)| (opening(prefix), (*label).to_string()))
        .collect();
    if labels.len() < groups.len() {
        return BTreeMap::new();
    }
    labels
}

/// Defaults applied when creating panes and layouts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutDefaults {
    /// Direction a new pane spawns relative to the focused pane. Each client —
    /// a viewer and the `koshi` CLI alike — reads its own copy and puts it on
    /// every new-pane command it sends. The `new-pane-<direction>` actions and
    /// an explicit `--direction` name their own direction and bypass it.
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
    /// Whether completing a selection copies it immediately. No `koshi.kdl`
    /// key sets it, so it always holds its default.
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

/// The clipboard backend copied text is written to. OSC 52 is the only
/// backend koshi builds.
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
    /// The theme's name: the file stem of the `themes/<name>.kdl` its colors
    /// were read from, or [`DEFAULT_THEME`] when the built-in colors are in
    /// effect.
    pub name: String,
    /// The theme's colors.
    pub colors: ColorPalette,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            name: DEFAULT_THEME.to_string(),
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
