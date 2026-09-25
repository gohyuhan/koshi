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

use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::str::FromStr;

use koshi_core::action::ActionReference;
use koshi_core::geometry::Direction;
use koshi_core::key::{ExtendedKeysMode, Key, KeyChord, KeySequence, ModFlags};
use koshi_core::log::{LogFormat, LogLevel};
use koshi_core::resolve::{ActionArgs, DEFAULT_SCROLL_LINE_COUNT};

use crate::error::ColorParseError;
use crate::key::Leader;
use crate::key_sequence::parse_sequence;

/// The config schema version written to and read from disk, bumped when the
/// on-disk shape changes. A file declaring an older version is migrated
/// forward to this shape; a file declaring a newer one is refused.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::CONFIG_SCHEMA`].
pub const SCHEMA_VERSION: u32 = koshi_core::compat::CONFIG_SCHEMA.maximum_version;

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
    pub config_schema_version: u32,
    /// Pane sizing floor for the shared layout.
    pub pane: PaneConfig,
    /// Per-pane scrollback history caps.
    pub scrollback: ScrollbackLimits,
    /// Terminal environment presented to child processes.
    pub terminal: TerminalConfig,
    /// Log-file behavior for this process.
    pub logging: LoggingConfig,
    /// Whether entry points marked `#[beta_feature]` may run.
    pub should_allow_beta_features: bool,
    /// Whether other users of this machine may reach this session's socket.
    pub should_allow_other_users: bool,
    /// The TCP address the remote listener binds, such as `"0.0.0.0:7654"`.
    /// Setting it opens nothing; `koshi share grant` switches remote access on.
    pub remote_listen: Option<String>,
    /// The directory the session sockets other users reach live in. `None`
    /// takes the platform's machine-wide directory, `/tmp/koshi` on Unix and
    /// `%ProgramData%\koshi` on Windows.
    pub shared_sessions_directory: Option<PathBuf>,
    /// Whether the session ends when its last client leaves.
    pub should_auto_close_session: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            config_schema_version: SCHEMA_VERSION,
            pane: PaneConfig::default(),
            scrollback: ScrollbackLimits::default(),
            terminal: TerminalConfig::default(),
            logging: LoggingConfig::default(),
            should_allow_beta_features: false,
            should_allow_other_users: false,
            remote_listen: None,
            shared_sessions_directory: None,
            should_auto_close_session: false,
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
    pub config_schema_version: u32,
    /// Keybinding timing, chord depth, leader prefix, and per-mode bindings.
    pub keybindings: KeybindingsConfig,
    /// Defaults applied when creating panes and layouts.
    pub layout: LayoutDefaults,
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
    /// Whether this viewer sends native image protocols to its terminal.
    pub supports_image_protocols: bool,
    /// Whether this viewer skips placement interpolation while retaining its
    /// placement highlight and confirmation state. `true` also draws another
    /// viewer's accepted placement at its committed rectangles at once.
    pub should_reduce_motion: bool,
    /// Whether pane placement mode stays on after the session accepts a
    /// placement confirmed with Enter or a mouse drop. `true` keeps the mode on,
    /// until Esc, for the next placement. `false` ends the mode once the
    /// accepted layout arrives. A placement the session rejects leaves the mode
    /// as it was: a mode that lasts until Esc stays on, and a mode that lasts
    /// until its pane-handle drag ends is closed.
    pub should_stay_in_pane_placement_mode_after_placement: bool,
    /// Whether a viewer whose link to a session on another machine drops dials
    /// that machine again by itself. While it dials, the viewer draws
    /// `RECONNECTING` on its tab strip and keeps trying for up to 120 seconds,
    /// and joining again puts back the tab, the focused and zoomed pane of each
    /// tab, and the scroll offset of each pane. `false` ends the viewer on a
    /// dropped link, with the message that names how to attach again by hand. A
    /// link to a session on this machine ends the viewer either way.
    pub should_reconnect_remote_session: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            config_schema_version: SCHEMA_VERSION,
            keybindings: KeybindingsConfig::default(),
            layout: LayoutDefaults::default(),
            mouse: MouseConfig::default(),
            copy: CopyConfig::default(),
            scrollback: ScrollbackView::default(),
            theme: ThemeConfig::default(),
            logging: LoggingConfig::default(),
            update: UpdateConfig::default(),
            supports_image_protocols: true,
            should_reduce_motion: false,
            should_stay_in_pane_placement_mode_after_placement: true,
            should_reconnect_remote_session: true,
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
    pub should_auto_check_for_updates: bool,
    /// Days to wait between startup update checks.
    pub check_interval_days: u32,
    /// Whether a pre-release build counts as a newer version to update to.
    pub should_allow_prerelease_updates: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            should_auto_check_for_updates: true,
            check_interval_days: 14,
            should_allow_prerelease_updates: false,
        }
    }
}

/// Pane sizing floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneConfig {
    /// Minimum pane width in columns.
    pub minimum_column_count: u16,
    /// Minimum pane height in rows.
    pub minimum_row_count: u16,
    /// Blank cells between two panes that meet along a horizontal or vertical
    /// split. `0` places panes edge to edge.
    pub gap_cell_count: u16,
}

impl Default for PaneConfig {
    fn default() -> Self {
        Self {
            minimum_column_count: 2,
            minimum_row_count: 1,
            gap_cell_count: 0,
        }
    }
}

/// Per-pane scrollback history caps. The buffer these bound lives in the
/// pane's terminal engine, so one pane has one set of caps however many
/// viewers it has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollbackLimits {
    /// Maximum retained lines per pane.
    pub maximum_line_count: usize,
    /// Maximum retained bytes of scrollback text per pane.
    pub maximum_byte_count: usize,
}

impl Default for ScrollbackLimits {
    fn default() -> Self {
        Self {
            maximum_line_count: 10_000,
            maximum_byte_count: 32 * 1024 * 1024,
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
    pub should_scroll_to_input: bool,
}

impl Default for ScrollbackView {
    fn default() -> Self {
        Self {
            should_scroll_to_input: true,
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
    /// Bindings grouped by input mode name. `Default` ships the built-in binding
    /// set (`normal` plus the reserved unlock in `locked`); user layers
    /// override it at merge.
    pub mode_bindings_by_name: BTreeMap<ModeName, ModeBindings>,
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
    pub const RESERVED_UNLOCK: KeyChord = KeyChord::from_parts(ModFlags::CTRL, Key::Char('l'));
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        Self {
            chord_timeout_ms: 500,
            which_key_delay_ms: 300,
            max_chord_depth: 4,
            leader: Leader::default(),
            mode_bindings_by_name: build_default_mode_bindings(Leader::default()),
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
    pub fn from_text(mode_name_text: impl Into<String>) -> Self {
        Self(mode_name_text.into())
    }

    /// The mode name as a string slice.
    pub fn get_name(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for ModeName {
    fn borrow(&self) -> &str {
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
    pub action_reference: ActionReference,
    /// The arguments handed to action resolution alongside it.
    pub action_arguments: ActionArgs,
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
    pub bound_action_by_key_sequence: BTreeMap<KeySequence, BoundAction>,
    /// Key sequences this surface clears: a removed key voids whatever any
    /// lower-precedence layer bound on it, leaving the key free for this or
    /// a higher layer to rebind. Authored as `remove "<C-x>"` in a mode
    /// block. The built-in defaults carry none.
    pub removed_key_sequences: BTreeSet<KeySequence>,
}

/// The built-in default binding table: the `normal`-mode set, the reserved
/// unlock, quit, and mouse-select in `locked` mode, and the placement actions
/// in the `pane-placement` mode.
///
/// Sequences written with `<leader>` resolve against `leader`, so rebinding
/// the leader moves them. Explicit chords — `<A-f>`, the reserved unlock, and
/// the `Tab`/`Shift+Tab` pair — are written literally and never move.
///
/// Under the default `C-` leader every sequence OPENS with a non-typeable
/// chord (Ctrl or Alt held), with one exception: the bare `Tab`/`Shift+Tab`
/// tab-switching pair. Outside locked mode the keymap owns Tab, and a shell
/// sees a literal Tab only while the client is locked. A subsequent chord in a
/// sequence may be a plain key; it is read only while the pending sequence is
/// live. No opening chord uses `<C-i>`, `<C-m>`, `<C-[>`, or `<C-h>`, which
/// unix terminals without the kitty keyboard protocol cannot tell apart from
/// Tab, Enter, Esc, and Backspace. Pane operations — lifecycle, directional
/// splits, and directional focus — live under the `<C-p>` prefix, resize under
/// `<C-s>`, and tab lifecycle under `<C-t>`. Placement actions live in the
/// `pane-placement` mode. Every binding is argless: an action choice with a fixed set
/// of values is part of the action name (`new-pane-left`,
/// `select-pane-target-left`), so any key here can be rebound from
/// `keybinding.kdl`.
pub fn build_default_mode_bindings(leader: Leader) -> BTreeMap<ModeName, ModeBindings> {
    let parse_default_key_sequence = |key_sequence_text: &str| {
        parse_sequence(key_sequence_text, leader, u8::MAX)
            .expect("a built-in default binding must parse")
    };
    let build_reserved_unlock_sequence = || KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK);
    let build_bound_action = |action_name: &str| BoundAction {
        action_reference: ActionReference::from_core_action_name(action_name)
            .expect("default binding action name must satisfy the action-name grammar"),
        action_arguments: ActionArgs::None,
    };

    let normal_mode_bindings: BTreeMap<KeySequence, BoundAction> = [
        // Lock — the reserved chord, written literally: it does not move with
        // the leader. The same chord unlocks in locked mode.
        (build_reserved_unlock_sequence(), build_bound_action("lock")),
        // Quit and mouse-select — leader-relative, and bound in locked mode
        // too. Mouse-select grabs the mouse: a drag highlights in koshi even
        // over a program that asked for the mouse itself.
        (
            parse_default_key_sequence("<leader>q"),
            build_bound_action("quit"),
        ),
        (
            parse_default_key_sequence("<leader>g"),
            build_bound_action("mouse-select"),
        ),
        // Pane lifecycle, under the leader then `p`. `n` splits in the
        // configured default direction; the vim letters pick the side.
        (
            parse_default_key_sequence("<leader>p n"),
            build_bound_action("new-pane"),
        ),
        (
            parse_default_key_sequence("<leader>p h"),
            build_bound_action("new-pane-left"),
        ),
        (
            parse_default_key_sequence("<leader>p j"),
            build_bound_action("new-pane-down"),
        ),
        (
            parse_default_key_sequence("<leader>p k"),
            build_bound_action("new-pane-up"),
        ),
        (
            parse_default_key_sequence("<leader>p l"),
            build_bound_action("new-pane-right"),
        ),
        // `s` adds the new pane to the focused pane's stack.
        (
            parse_default_key_sequence("<leader>p s"),
            build_bound_action("new-pane-stacked"),
        ),
        // Move opens the viewer-owned read-only placement preview.
        (
            parse_default_key_sequence("<leader>p m"),
            build_bound_action("begin-pane-placement"),
        ),
        // The close key kills the pane's whole process group.
        (
            parse_default_key_sequence("<leader>p x"),
            build_bound_action("close-pane-tree"),
        ),
        // Fullscreen — an explicit chord, so it stays put under any leader.
        (
            parse_default_key_sequence("<A-f>"),
            build_bound_action("toggle-pane-fullscreen"),
        ),
        // Directional focus: arrows under the pane prefix. These fire
        // continuous actions, so the prefix stays armed after each press.
        (
            parse_default_key_sequence("<leader>p <Left>"),
            build_bound_action("focus-pane-left"),
        ),
        (
            parse_default_key_sequence("<leader>p <Down>"),
            build_bound_action("focus-pane-down"),
        ),
        (
            parse_default_key_sequence("<leader>p <Up>"),
            build_bound_action("focus-pane-up"),
        ),
        (
            parse_default_key_sequence("<leader>p <Right>"),
            build_bound_action("focus-pane-right"),
        ),
        // Resize: one cell per press, arrows under the leader then `s`.
        (
            parse_default_key_sequence("<leader>s <Left>"),
            build_bound_action("resize-pane-left"),
        ),
        (
            parse_default_key_sequence("<leader>s <Down>"),
            build_bound_action("resize-pane-down"),
        ),
        (
            parse_default_key_sequence("<leader>s <Up>"),
            build_bound_action("resize-pane-up"),
        ),
        (
            parse_default_key_sequence("<leader>s <Right>"),
            build_bound_action("resize-pane-right"),
        ),
        // Copy and paste have NO bindings — they follow the OS.
        // Tab lifecycle, under the leader then `t`: `n` opens, `x` closes.
        // Switching is the bare Tab / Shift+Tab pair, written literally and
        // never leader-relative: outside locked mode the keymap owns Tab, and
        // a shell sees a literal Tab only while the client is locked.
        (
            parse_default_key_sequence("<leader>t n"),
            build_bound_action("new-tab"),
        ),
        (
            parse_default_key_sequence("<leader>t x"),
            build_bound_action("close-tab"),
        ),
        (
            parse_default_key_sequence("<Tab>"),
            build_bound_action("next-tab"),
        ),
        (
            parse_default_key_sequence("<S-Tab>"),
            build_bound_action("previous-tab"),
        ),
    ]
    .into_iter()
    .collect();

    // Locked mode intercepts exactly its bound chords and passes every other
    // key to the pane: the reserved unlock (the same chord that locks in
    // normal mode), the pane placement opener, the quit chord, and the mouse-select
    // chord.
    let locked_mode_bindings: BTreeMap<KeySequence, BoundAction> = [
        (
            build_reserved_unlock_sequence(),
            build_bound_action("unlock"),
        ),
        (
            parse_default_key_sequence("<leader>p m"),
            build_bound_action("begin-pane-placement"),
        ),
        (
            parse_default_key_sequence("<leader>q"),
            build_bound_action("quit"),
        ),
        (
            parse_default_key_sequence("<leader>g"),
            build_bound_action("mouse-select"),
        ),
    ]
    .into_iter()
    .collect();

    // Placement is a viewer-local submode. Its bindings take priority over
    // the base normal or locked mode while the placement interaction is open.
    let pane_placement_mode_bindings: BTreeMap<KeySequence, BoundAction> = [
        (
            parse_default_key_sequence("<Left>"),
            build_bound_action("select-pane-target-left"),
        ),
        (
            parse_default_key_sequence("<Down>"),
            build_bound_action("select-pane-target-down"),
        ),
        (
            parse_default_key_sequence("<Up>"),
            build_bound_action("select-pane-target-up"),
        ),
        (
            parse_default_key_sequence("<Right>"),
            build_bound_action("select-pane-target-right"),
        ),
        (
            parse_default_key_sequence("<S-Left>"),
            build_bound_action("select-pane-insertion-left"),
        ),
        (
            parse_default_key_sequence("<S-Down>"),
            build_bound_action("select-pane-insertion-down"),
        ),
        (
            parse_default_key_sequence("<S-Up>"),
            build_bound_action("select-pane-insertion-up"),
        ),
        (
            parse_default_key_sequence("<S-Right>"),
            build_bound_action("select-pane-insertion-right"),
        ),
        (
            parse_default_key_sequence("<Space>"),
            build_bound_action("cycle-pane-placement-span"),
        ),
        (
            parse_default_key_sequence("<Tab>"),
            build_bound_action("select-next-placement-tab"),
        ),
        (
            parse_default_key_sequence("<S-Tab>"),
            build_bound_action("select-previous-placement-tab"),
        ),
        (
            parse_default_key_sequence("<CR>"),
            build_bound_action("confirm-pane-placement"),
        ),
        (
            parse_default_key_sequence("<Esc>"),
            build_bound_action("cancel-pane-placement"),
        ),
    ]
    .into_iter()
    .collect();

    BTreeMap::from([
        (
            ModeName::from_text("normal"),
            ModeBindings {
                bound_action_by_key_sequence: normal_mode_bindings,
                removed_key_sequences: BTreeSet::new(),
            },
        ),
        (
            ModeName::from_text("locked"),
            ModeBindings {
                bound_action_by_key_sequence: locked_mode_bindings,
                removed_key_sequences: BTreeSet::new(),
            },
        ),
        (
            ModeName::from_text("pane-placement"),
            ModeBindings {
                bound_action_by_key_sequence: pane_placement_mode_bindings,
                removed_key_sequences: BTreeSet::new(),
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
    let parse_opening_chord = |prefix_text: &str| {
        *parse_sequence(prefix_text, leader, u8::MAX)
            .expect("a built-in prefix must parse")
            .list_chords()
            .first()
            .expect("a prefix sequence has an opening chord")
    };
    let prefix_groups = [
        ("<leader>p", "PANE"),
        ("<leader>s", "RESIZE"),
        ("<leader>t", "TAB"),
    ];
    // `C-` gives each group its own opening chord: `<C-p> PANE`,
    // `<C-s> RESIZE`, `<C-t> TAB`. A chord leader opens every group at the
    // leader itself, so `<Space>` collapses all three onto one entry.
    let prefix_labels: BTreeMap<KeyChord, String> = prefix_groups
        .iter()
        .map(|(prefix_text, label_text)| {
            (parse_opening_chord(prefix_text), (*label_text).to_string())
        })
        .collect();
    if prefix_labels.len() < prefix_groups.len() {
        return BTreeMap::new();
    }
    prefix_labels
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

/// Mouse routing behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MouseConfig {
    /// Whether dragging a pane border resizes it.
    pub can_resize_pane_border: bool,
    /// Lines scrolled per mouse wheel notch.
    pub scroll_line_count: u16,
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
            can_resize_pane_border: true,
            scroll_line_count: DEFAULT_SCROLL_LINE_COUNT,
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
    pub should_copy_on_select: bool,
    /// Whether trailing whitespace is trimmed from copied text.
    pub should_trim_trailing_whitespace: bool,
    /// Which clipboard backend receives copied text.
    pub clipboard: ClipboardBackend,
}

impl Default for CopyConfig {
    fn default() -> Self {
        Self {
            should_copy_on_select: true,
            should_trim_trailing_whitespace: true,
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
    /// What a pane's program receives for a key whose legacy bytes another key
    /// also owns, such as Shift+Enter.
    pub extended_keys_mode: ExtendedKeysMode,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            term: "xterm-256color".to_string(),
            colorterm: "truecolor".to_string(),
            default_shell: None,
            extended_keys_mode: ExtendedKeysMode::default(),
        }
    }
}

/// A named color theme and its palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeConfig {
    /// The theme's name: the file stem of the `themes/<name>.kdl` its colors
    /// were read from, or [`DEFAULT_THEME`] when the built-in colors are in
    /// effect.
    pub theme_name: String,
    /// The theme's colors.
    pub colors: ColorPalette,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            theme_name: DEFAULT_THEME.to_string(),
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
            ramp_start: RgbColor::from_channels(0xd0, 0xa5, 0xff),
            ramp_end: RgbColor::from_channels(0x7d, 0xbc, 0xff),
            on_ramp: RgbColor::from_channels(0x12, 0x09, 0x1f),
            on_ramp_dim: RgbColor::from_channels(0xf0, 0xec, 0xfa),
            accent: RgbColor::from_channels(0xf5, 0xc2, 0xff),
            on_accent: RgbColor::from_channels(0x1e, 0x10, 0x33),
            border_focused: RgbColor::from_channels(0x00, 0xaf, 0xd7),
            border_unfocused: RgbColor::from_channels(0x58, 0x58, 0x58),
            border_hover: RgbColor::from_channels(0xaf, 0x5f, 0xff),
            stack_header_fg: RgbColor::from_channels(0xf4, 0xf1, 0xfa),
            stack_header_bg: RgbColor::from_channels(0x30, 0x0f, 0x4a),
            letterbox: RgbColor::from_channels(0x58, 0x58, 0x58),
            bar_bg: RgbColor::from_channels(0x00, 0x00, 0x00),
        }
    }
}

/// A 24-bit truecolor value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbColor {
    /// Red channel.
    pub red: u8,
    /// Green channel.
    pub green: u8,
    /// Blue channel.
    pub blue: u8,
}

impl RgbColor {
    /// Builds a color from its red, green, and blue channels.
    pub const fn from_channels(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    /// Parses a `#RRGGBB` (or bare `RRGGBB`) hex string into a color.
    ///
    /// # Errors
    /// - [`ColorParseError::BadLength`] if the value, after stripping a
    ///   leading `#`, is not exactly six characters.
    /// - [`ColorParseError::BadDigit`] if any of those six characters is not
    ///   a hex digit (`0-9`, `a-f`, `A-F`).
    pub fn from_hex(hex_text: &str) -> Result<Self, ColorParseError> {
        // Accept the value with or without its leading `#`.
        let bare_hex_text = hex_text.strip_prefix('#').unwrap_or(hex_text);
        let hex_character_count = bare_hex_text.chars().count();
        if hex_character_count != 6 {
            return Err(ColorParseError::BadLength {
                character_count: hex_character_count,
            });
        }
        if !bare_hex_text
            .chars()
            .all(|hex_character| hex_character.is_ascii_hexdigit())
        {
            return Err(ColorParseError::BadDigit {
                invalid_hex_text: bare_hex_text.to_string(),
            });
        }
        // Six ASCII hex digits: one byte per character, so each two-byte
        // slice is valid ASCII and parses.
        let parse_color_component = |component_start_index: usize| {
            u8::from_str_radix(
                &bare_hex_text[component_start_index..component_start_index + 2],
                16,
            )
            .expect("validated hex")
        };
        Ok(Self::from_channels(
            parse_color_component(0),
            parse_color_component(2),
            parse_color_component(4),
        ))
    }
}

impl FromStr for RgbColor {
    type Err = ColorParseError;

    fn from_str(hex_text: &str) -> Result<Self, Self::Err> {
        Self::from_hex(hex_text)
    }
}

/// Log-file behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoggingConfig {
    /// Whether koshi writes a log file. Disabled, nothing is logged and no
    /// log file or `logs/` directory is created; enabled, log lines at or
    /// above [`level`](Self::level) are written to a per-session file under
    /// the platform state directory, created on the first line written.
    pub is_enabled: bool,
    /// The lowest severity that gets written. A line below this is dropped —
    /// e.g. [`LogLevel::Warning`] drops `info` lines.
    pub level: LogLevel,
    /// How each written line is rendered.
    pub log_format: LogFormat,
}

impl Default for LoggingConfig {
    /// Logging is off, and when turned on writes warnings and errors in the
    /// human-readable format.
    fn default() -> Self {
        Self {
            is_enabled: false,
            level: LogLevel::Warning,
            log_format: LogFormat::Pretty,
        }
    }
}

#[cfg(test)]
mod tests;
