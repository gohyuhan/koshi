//! The typed configuration schema and its built-in defaults.
//!
//! The tree is split by who reads it: [`ServerConfig`] holds what one session
//! shares across every viewer (the layout floor, scrollback caps, the child
//! environment), and [`ClientConfig`] holds what one viewer decides for itself
//! (keybindings, theme, mouse, copy). Both come from the same `koshi.kdl`.
//! Each side folds only the sections it owns: a viewer cannot set the shell a
//! session spawns, and a session cannot set a viewer's colors.
//!
//! Every field has a default via [`Default`]: koshi runs with no user config,
//! and each side's `default()` is the baseline that user layers fold onto.
//! This module owns the schema and defaults only. The sibling
//! [`layer`](crate::layer) module folds override layers onto these defaults,
//! [`keybinding`](crate::keybinding) parses keybinding-file KDL, and
//! [`migration`](crate::migration) validates versioned files and moves them
//! through adjacent schemas. Disk discovery and reading live in the binary.

use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use koshi_core::action::ActionReference;
use koshi_core::constant::DEFAULT_MAX_FRAMES_PER_SECOND;
use koshi_core::geometry::Direction;
use koshi_core::key::{BindingModifierFlags, ExtendedKeysMode, Key, KeyChord, KeySequence};
use koshi_core::lock::LockMode;
use koshi_core::log::{LogFormat, LogLevel};
use koshi_core::resolve::DEFAULT_SCROLL_LINE_COUNT;

use crate::error::ColorParseError;
use crate::key::Leader;
use crate::key_sequence::parse_sequence;

/// The config schema version written to and read from disk. A file declaring
/// an older version is migrated forward to this shape; a file declaring a
/// newer one is refused.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::CONFIG_SCHEMA`].
pub const SCHEMA_VERSION: u32 = koshi_core::compat::CONFIG_SCHEMA.maximum_version;

/// The name of the built-in theme, whose colors are compiled into koshi. It is
/// the theme in effect when `koshi.kdl` names no theme, names this one, or
/// names one whose `themes/<name>.kdl` cannot be loaded.
pub const DEFAULT_THEME_NAME: &str = "default";

/// The `logging.max-log-file-size-in-mb` of a `koshi.kdl` that sets none: 4.
pub const DEFAULT_MAX_LOG_FILE_SIZE_IN_MB: u32 = 4;

/// The least `notifications.toast-duration-ms` that `koshi.kdl` takes: 1000.
pub const LOWEST_TOAST_DURATION_MS: u32 = 1_000;

/// The greatest `notifications.toast-duration-ms` that `koshi.kdl` takes:
/// 60000.
pub const HIGHEST_TOAST_DURATION_MS: u32 = 60_000;

/// The least `logging.max-log-file-size-in-mb` that `koshi.kdl` takes: 1.
pub const LOWEST_MAX_LOG_FILE_SIZE_IN_MB: u32 = 1;

/// The greatest `logging.max-log-file-size-in-mb` that `koshi.kdl` takes: 1024.
pub const HIGHEST_MAX_LOG_FILE_SIZE_IN_MB: u32 = 1024;

/// The settings the session host reads: the shared layout floor, the
/// scrollback buffers it owns, the environment it spawns children into, its
/// own log file, who else on this machine may reach it, and whether its panes
/// may write the clipboard.
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
    /// Nothing reads it on Windows.
    pub should_allow_other_users: bool,
    /// The IP address and port the remote listener binds, such as
    /// `0.0.0.0:7654`. Setting it opens nothing; `koshi share open`, or a yes
    /// to the offer `koshi share grant` makes, switches remote access on. The
    /// router reads it when it starts.
    pub remote_listen_address: Option<SocketAddr>,
    /// The directory the session sockets other users reach live in. `None`
    /// takes `/tmp/koshi`. Nothing reads it on Windows.
    pub shared_sessions_directory: Option<PathBuf>,
    /// Whether the session ends when its last client leaves.
    pub should_auto_close_session: bool,
    /// Whether a program in a pane may write the clipboard of the clients that
    /// focus that pane, and the largest copy it may write.
    pub pane_clipboard_writes: PaneClipboardWriteConfig,
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
            remote_listen_address: None,
            shared_sessions_directory: None,
            should_auto_close_session: false,
            pane_clipboard_writes: PaneClipboardWriteConfig::default(),
        }
    }
}

/// The `pane-clipboard-writes` block: whether a program in a pane may write
/// the clipboard, through an `OSC 52` sequence or through a clipboard tool
/// such as `pbcopy`, and the largest copy it may write.
///
/// The session reads it when it starts and when it restarts into a new build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneClipboardWriteConfig {
    /// Whether the session reads the `OSC 52` copies of every pane, and whether
    /// a pane started while it is `true` finds the stand-in clipboard tools
    /// first on its `PATH`. `false` makes a pane's copy reach no clipboard
    /// through koshi.
    pub should_allow_writes: bool,
    /// The most decoded bytes one copy may carry, at least `1`. An `OSC 52`
    /// copy past it makes no copy. A clipboard tool copy past it goes to the
    /// real tool on the session's machine with every byte.
    pub maximum_copy_byte_count: usize,
}

impl Default for PaneClipboardWriteConfig {
    fn default() -> Self {
        Self {
            should_allow_writes: false,
            maximum_copy_byte_count: 8 * 1024 * 1024,
        }
    }
}

/// The settings one viewer reads: how its keyboard and mouse are interpreted,
/// what it paints with, and what it does with copied text.
///
/// Each attached viewer holds its own, read from the `koshi.kdl` on the
/// machine it runs on: two viewers of one session can bind different keys
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
    /// How this viewer previews each new notification.
    pub notifications: NotificationConfig,
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
    /// viewer's accepted placement at its committed rectangles, and a pane
    /// this viewer undocks at its floating rectangle, at once.
    pub should_reduce_motion: bool,
    /// Whether pane placement mode opened by a key, such as `<C-p> m`, stays
    /// on after the session accepts a placement confirmed with Enter or a
    /// mouse drop. `true` keeps the mode on, until Esc, for the next
    /// placement. `false` ends the mode once the accepted layout arrives. A
    /// placement the session rejects leaves that mode on. Pane placement mode
    /// opened by a pane-handle drag ends once the session accepts or rejects
    /// its drop, whatever this says.
    ///
    /// It also governs floating pane move mode: `true` keeps the mode on after
    /// the session runs a floating pane move sent with Enter or a mouse
    /// release, and `false` ends the mode once the session runs the move. A
    /// move the session rejects leaves the mode on.
    pub should_stay_in_pane_placement_mode_after_placement: bool,
    /// Whether a viewer whose link to a session on another machine drops dials
    /// that machine again by itself. While it dials, the viewer draws
    /// `RECONNECTING` on its tab strip and keeps trying for up to 120 seconds,
    /// and joining again puts back the tab, the focused and zoomed pane of each
    /// tab, and the scroll offset of each pane. `false` ends the viewer on a
    /// dropped link, with the message that names how to attach again by hand. A
    /// link to a session on this machine ends the viewer either way.
    pub should_reconnect_remote_session: bool,
    /// The longest a viewer's link to a session on another machine may carry
    /// no byte, in milliseconds, at least `1000`. The viewer sends the session
    /// a ping every third of this time, and the session sends the viewer a
    /// pong whenever it has sent the viewer nothing for a third of this time.
    /// A viewer that reads no byte on the link for this long counts the link
    /// as dropped, and a socket write from the viewer that puts no byte on the
    /// link for this long ends the viewer's writing on that link. The router
    /// of the machine that serves the session ends its link to the viewer when
    /// a socket read from the viewer gets no byte, or a socket write to the
    /// viewer takes no byte, for this long, and the session then removes the
    /// viewer's client. While the viewer joins, that router waits at least 20
    /// seconds for the viewer's first request to the session. Example: `30000`
    /// sends a ping every 10 seconds, a laptop that sleeps loses its link 30
    /// seconds after the last byte, and a paste that takes two minutes to send
    /// keeps the link while its bytes keep moving. A link to a session on this
    /// machine sends no ping and has no limit.
    pub remote_link_timeout_ms: u32,
    /// The most frames a second the session sends this viewer, from `30` to
    /// `120`. The session sends a frame at most once per second divided by
    /// this value, and the frame after the viewer's own key, paste or mouse
    /// event goes without that wait. The viewer's placement preview and pane
    /// slides step once per the same interval. Example: `60` sends at most one
    /// frame per 16666667 nanoseconds.
    pub max_frames_per_second: u16,
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
            notifications: NotificationConfig::default(),
            theme: ThemeConfig::default(),
            logging: LoggingConfig::default(),
            update: UpdateConfig::default(),
            supports_image_protocols: true,
            should_reduce_motion: false,
            should_stay_in_pane_placement_mode_after_placement: true,
            should_reconnect_remote_session: true,
            remote_link_timeout_ms: 30_000,
            max_frames_per_second: DEFAULT_MAX_FRAMES_PER_SECOND,
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
/// pane's terminal engine: one pane has one set of caps however many viewers
/// it has.
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

/// How one viewer's scrollback view behaves. Each viewer holds its own: two
/// viewers of the same pane can follow live output differently.
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

/// How one viewer previews each new notification: the `notifications { … }`
/// block of `koshi.kdl`. Every entry enters the viewer's notification center
/// whatever these say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotificationConfig {
    /// Whether a new notification slides in as a toast. `false` shows no toast.
    pub should_show_toasts: bool,
    /// The corner of the pane area the toast sits in.
    pub toast_position: ToastPosition,
    /// How long a toast stays in place between sliding in and sliding out,
    /// in milliseconds, from [`LOWEST_TOAST_DURATION_MS`] to
    /// [`HIGHEST_TOAST_DURATION_MS`]. A toast with another toast waiting
    /// behind it stays 500 milliseconds.
    pub toast_duration_ms: u32,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            should_show_toasts: true,
            toast_position: ToastPosition::TopRight,
            toast_duration_ms: 3_000,
        }
    }
}

/// The corner of the pane area a toast sits in. A toast in a right corner
/// slides in from the right edge, and a toast in a left corner from the left
/// edge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToastPosition {
    /// `top-right`.
    #[default]
    TopRight,
    /// `top-left`.
    TopLeft,
    /// `bottom-right`.
    BottomRight,
    /// `bottom-left`.
    BottomLeft,
}

/// Keybinding timing, chord depth, leader prefix, and per-mode bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingsConfig {
    /// Milliseconds a sequence that is a complete binding and also a longer
    /// binding's prefix waits for its next chord before the complete binding
    /// fires.
    pub chord_timeout_ms: u32,
    /// Milliseconds from a sequence's first chord until the hint bar shows the
    /// chords that continue it. `0` shows them with the first chord.
    pub which_key_delay_ms: u32,
    /// Maximum number of chords in one key sequence.
    pub maximum_chord_depth: u8,
    /// The prefix that `<leader>` in a binding resolves to. A modifier run
    /// merges into the chord that follows it; a chord stands on its own.
    pub leader: Leader,
    /// Bindings grouped by input mode name. `Default` holds the built-in
    /// binding set of [`build_default_mode_bindings`]; user layers override it
    /// at merge.
    pub mode_bindings_by_name: BTreeMap<ModeName, ModeBindings>,
    /// Replacement chord for the reserved unlock. When set, this chord (not
    /// [`RESERVED_UNLOCK`](Self::RESERVED_UNLOCK)) is the guaranteed
    /// locked-mode escape: conflict detection requires it bound to
    /// `core:unlock` in locked mode and refuses a typeable chord, and the
    /// default unlock key becomes free to rebind.
    pub unlock_alternative: Option<KeyChord>,
}

impl KeybindingsConfig {
    /// The reserved unlock chord, `<C-l>`. The same chord locks in normal
    /// mode: one key flips the client both ways. In `locked` mode this chord
    /// fires `core:unlock` and is intercepted ahead of pane pass-through;
    /// validation refuses a config that removes it without naming an
    /// explicit alternative.
    pub const RESERVED_UNLOCK: KeyChord =
        KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('l'));

    /// The chord that unlocks a locked client: `unlock_alternative` when set,
    /// otherwise [`RESERVED_UNLOCK`](Self::RESERVED_UNLOCK).
    pub fn resolve_unlock_chord(unlock_alternative: Option<KeyChord>) -> KeyChord {
        unlock_alternative.unwrap_or(Self::RESERVED_UNLOCK)
    }
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        Self {
            chord_timeout_ms: 500,
            which_key_delay_ms: 300,
            maximum_chord_depth: 4,
            leader: Leader::default(),
            mode_bindings_by_name: build_default_mode_bindings(Leader::default()),
            unlock_alternative: None,
        }
    }
}

/// The name of an input mode (`normal`, `locked`, `pane-placement`, …), stored
/// as a plain string. A keybinding file may name a mode outside the built-in set;
/// conflict detection warns that mode as unregistered.
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

/// The action a key sequence triggers.
///
/// A binding names an action and no arguments: an action choice with a fixed
/// set of values lives in the action name (`new-pane-left`,
/// `close-pane-tree`), and open-range values are reachable only through CLI
/// commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundAction {
    /// The action to resolve when the sequence fires.
    pub action_reference: ActionReference,
}

/// The bindings for one input mode, keyed by the key sequence pressed.
///
/// The map key is the sequence: one sequence resolves to exactly one action.
/// The reverse is open: several sequences in one mode may name the
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
/// unlock, quit, and mouse-select in `locked` mode, the placement actions in
/// the `pane-placement` mode, the chooser actions in the `pane-move-choice`
/// mode, the floating pane move actions in the `floating-pane-move` mode, the
/// dock target and dock actions in the `dock-undock` mode, the tray actions
/// in the `floating-pane-tray` mode, and the notification center actions in
/// the `notification-center` mode.
///
/// Sequences written with `<leader>` resolve against `leader`: rebinding the
/// leader moves them. Explicit chords — `<A-f>`, `<A-n>`, the reserved unlock, and
/// the `Tab`/`Shift+Tab` pair — are written literally and never move.
///
/// Under the default `C-` leader every `normal` and `locked` sequence OPENS
/// with a non-typeable chord (Ctrl or Alt held), with one exception: the bare
/// `Tab`/`Shift+Tab` tab-switching pair. Outside locked mode the keymap owns
/// Tab, and a shell sees a literal Tab only while the client is locked. A
/// subsequent chord in a sequence may be a plain key; it is read only while
/// the pending sequence is live. No opening chord uses `<C-i>`, `<C-m>`,
/// `<C-[>`, or `<C-h>`, which unix terminals without the kitty keyboard
/// protocol cannot tell apart from Tab, Enter, Esc, and Backspace. Pane
/// operations — lifecycle, directional splits, and directional focus — live
/// under the `<C-p>` prefix, resize under `<C-s>`, and tab lifecycle under
/// `<C-t>`. Placement actions live in the `pane-placement` mode, the
/// chooser's `t`, `f`, Enter and Esc in the `pane-move-choice` mode, the
/// floating pane move keys in the `floating-pane-move` mode, the dock keys in
/// the `dock-undock` mode, the tray keys in the `floating-pane-tray` mode,
/// and the notification center keys in the `notification-center` mode. An
/// action choice with a fixed set of values is part of the action
/// name (`new-pane-left`, `select-pane-target-left`). Any key here can be
/// rebound from `keybinding.kdl`.
pub fn build_default_mode_bindings(leader: Leader) -> BTreeMap<ModeName, ModeBindings> {
    let parse_default_key_sequence = |key_sequence_text: &str| {
        parse_sequence(key_sequence_text, leader, u8::MAX)
            .expect("a built-in default binding must parse")
    };
    let build_reserved_unlock_sequence = || KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK);
    let build_bound_action = |action_name: &str| BoundAction {
        action_reference: ActionReference::from_core_action_name(action_name)
            .expect("default binding action name must satisfy the action-name grammar"),
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
        // `m` opens pane placement mode, a preview only this viewer sees.
        (
            parse_default_key_sequence("<leader>p m"),
            build_bound_action("begin-pane-placement"),
        ),
        // `u` opens dock/undock mode: Enter floats the selected tiled pane,
        // or docks the selected floating pane at its target.
        (
            parse_default_key_sequence("<leader>p u"),
            build_bound_action("begin-dock-undock"),
        ),
        // Tab and Shift+Tab under the pane prefix show and focus the next or
        // previous floating pane, in the order the panes became floating.
        // These fire continuous actions: the prefix stays armed.
        (
            parse_default_key_sequence("<leader>p <Tab>"),
            build_bound_action("focus-next-floating-pane"),
        ),
        (
            parse_default_key_sequence("<leader>p <S-Tab>"),
            build_bound_action("focus-previous-floating-pane"),
        ),
        // `w` opens the floating pane tray under the tabline's tray button `⧉`.
        (
            parse_default_key_sequence("<leader>p w"),
            build_bound_action("toggle-floating-pane-tray"),
        ),
        // The close key kills the pane's whole process group.
        (
            parse_default_key_sequence("<leader>p x"),
            build_bound_action("close-pane-tree"),
        ),
        // Fullscreen: an explicit chord that does not move with the leader.
        (
            parse_default_key_sequence("<A-f>"),
            build_bound_action("toggle-pane-fullscreen"),
        ),
        // The notification center: an explicit chord that does not move with
        // the leader.
        (
            parse_default_key_sequence("<A-n>"),
            build_bound_action("toggle-notification-center"),
        ),
        // Directional focus: arrows under the pane prefix. These fire
        // continuous actions: the prefix stays armed after each press.
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
    // normal mode), the quit chord, and the mouse-select chord.
    let locked_mode_bindings: BTreeMap<KeySequence, BoundAction> = [
        (
            build_reserved_unlock_sequence(),
            build_bound_action("unlock"),
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

    // The pane placement and dock/undock submodes select a target with the
    // same keys.
    let placement_target_bindings: [(KeySequence, BoundAction); 12] = [
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
            parse_default_key_sequence("s"),
            build_bound_action("toggle-pane-placement-stack"),
        ),
        (
            parse_default_key_sequence("<Tab>"),
            build_bound_action("select-next-placement-tab"),
        ),
        (
            parse_default_key_sequence("<S-Tab>"),
            build_bound_action("select-previous-placement-tab"),
        ),
    ];

    // Placement is a viewer-local submode. Its bindings take priority over
    // the base normal or locked mode while the placement interaction is open.
    let pane_placement_mode_bindings: BTreeMap<KeySequence, BoundAction> =
        placement_target_bindings
            .clone()
            .into_iter()
            .chain([
                (
                    parse_default_key_sequence("<CR>"),
                    build_bound_action("confirm-pane-placement"),
                ),
                (
                    parse_default_key_sequence("<Esc>"),
                    build_bound_action("cancel-pane-placement"),
                ),
            ])
            .collect();

    // Dock/undock mode is a viewer-local submode too. It selects a dock target
    // with the placement target keys, and has its own Enter and Esc.
    let dock_undock_mode_bindings: BTreeMap<KeySequence, BoundAction> = placement_target_bindings
        .into_iter()
        .chain([
            (
                parse_default_key_sequence("<CR>"),
                build_bound_action("confirm-dock-undock"),
            ),
            (
                parse_default_key_sequence("<Esc>"),
                build_bound_action("cancel-dock-undock"),
            ),
        ])
        .collect();

    // The pane move chooser and floating pane move mode are viewer-local
    // submodes too, each overlaying the base mode while it is open.
    let pane_move_choice_mode_bindings: BTreeMap<KeySequence, BoundAction> = [
        (
            parse_default_key_sequence("t"),
            build_bound_action("begin-tiled-pane-placement"),
        ),
        (
            parse_default_key_sequence("f"),
            build_bound_action("begin-floating-pane-move"),
        ),
        (
            parse_default_key_sequence("<CR>"),
            build_bound_action("confirm-pane-move-choice"),
        ),
        (
            parse_default_key_sequence("<Esc>"),
            build_bound_action("cancel-pane-move-choice"),
        ),
    ]
    .into_iter()
    .collect();

    let floating_pane_move_mode_bindings: BTreeMap<KeySequence, BoundAction> = [
        (
            parse_default_key_sequence("<Left>"),
            build_bound_action("move-floating-pane-left"),
        ),
        (
            parse_default_key_sequence("<Down>"),
            build_bound_action("move-floating-pane-down"),
        ),
        (
            parse_default_key_sequence("<Up>"),
            build_bound_action("move-floating-pane-up"),
        ),
        (
            parse_default_key_sequence("<Right>"),
            build_bound_action("move-floating-pane-right"),
        ),
        (
            parse_default_key_sequence("<Tab>"),
            build_bound_action("select-next-floating-pane"),
        ),
        (
            parse_default_key_sequence("<S-Tab>"),
            build_bound_action("select-previous-floating-pane"),
        ),
        (
            parse_default_key_sequence("<CR>"),
            build_bound_action("confirm-floating-pane-move"),
        ),
        (
            parse_default_key_sequence("<Esc>"),
            build_bound_action("cancel-floating-pane-move"),
        ),
    ]
    .into_iter()
    .collect();

    let floating_pane_tray_mode_bindings: BTreeMap<KeySequence, BoundAction> = [
        (
            parse_default_key_sequence("<Left>"),
            build_bound_action("select-tray-tile-left"),
        ),
        (
            parse_default_key_sequence("<Down>"),
            build_bound_action("select-tray-tile-down"),
        ),
        (
            parse_default_key_sequence("<Up>"),
            build_bound_action("select-tray-tile-up"),
        ),
        (
            parse_default_key_sequence("<Right>"),
            build_bound_action("select-tray-tile-right"),
        ),
        (
            parse_default_key_sequence("<CR>"),
            build_bound_action("toggle-selected-tray-tile"),
        ),
        (
            parse_default_key_sequence("a"),
            build_bound_action("toggle-minimize-all-floating-panes"),
        ),
        (
            parse_default_key_sequence("<Esc>"),
            build_bound_action("close-floating-pane-tray"),
        ),
        (
            parse_default_key_sequence("<leader>p w"),
            build_bound_action("toggle-floating-pane-tray"),
        ),
    ]
    .into_iter()
    .collect();

    let notification_center_mode_bindings: BTreeMap<KeySequence, BoundAction> = [
        (
            parse_default_key_sequence("<Up>"),
            build_bound_action("select-newer-notification"),
        ),
        (
            parse_default_key_sequence("<Down>"),
            build_bound_action("select-older-notification"),
        ),
        (
            parse_default_key_sequence("x"),
            build_bound_action("dismiss-selected-notification"),
        ),
        (
            parse_default_key_sequence("c"),
            build_bound_action("clear-notifications"),
        ),
        (
            parse_default_key_sequence("<Esc>"),
            build_bound_action("close-notification-center"),
        ),
        (
            parse_default_key_sequence("<A-n>"),
            build_bound_action("toggle-notification-center"),
        ),
    ]
    .into_iter()
    .collect();

    BTreeMap::from([
        (
            ModeName::from_text(LockMode::Normal.get_keymap_name()),
            ModeBindings {
                bound_action_by_key_sequence: normal_mode_bindings,
                removed_key_sequences: BTreeSet::new(),
            },
        ),
        (
            ModeName::from_text(LockMode::Locked.get_keymap_name()),
            ModeBindings {
                bound_action_by_key_sequence: locked_mode_bindings,
                removed_key_sequences: BTreeSet::new(),
            },
        ),
        (
            ModeName::from_text(LockMode::PanePlacement.get_keymap_name()),
            ModeBindings {
                bound_action_by_key_sequence: pane_placement_mode_bindings,
                removed_key_sequences: BTreeSet::new(),
            },
        ),
        (
            ModeName::from_text(LockMode::PaneMoveChoice.get_keymap_name()),
            ModeBindings {
                bound_action_by_key_sequence: pane_move_choice_mode_bindings,
                removed_key_sequences: BTreeSet::new(),
            },
        ),
        (
            ModeName::from_text(LockMode::FloatingPaneMove.get_keymap_name()),
            ModeBindings {
                bound_action_by_key_sequence: floating_pane_move_mode_bindings,
                removed_key_sequences: BTreeSet::new(),
            },
        ),
        (
            ModeName::from_text(LockMode::DockUndock.get_keymap_name()),
            ModeBindings {
                bound_action_by_key_sequence: dock_undock_mode_bindings,
                removed_key_sequences: BTreeSet::new(),
            },
        ),
        (
            ModeName::from_text(LockMode::FloatingPaneTray.get_keymap_name()),
            ModeBindings {
                bound_action_by_key_sequence: floating_pane_tray_mode_bindings,
                removed_key_sequences: BTreeSet::new(),
            },
        ),
        (
            ModeName::from_text(LockMode::NotificationCenter.get_keymap_name()),
            ModeBindings {
                bound_action_by_key_sequence: notification_center_mode_bindings,
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
pub fn build_default_prefix_labels(leader: Leader) -> BTreeMap<KeyChord, String> {
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
    // leader itself: under `<Space>` all three collapse onto one entry.
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
    pub wheel_scroll: WheelScroll,
}

impl Default for MouseConfig {
    fn default() -> Self {
        Self {
            can_resize_pane_border: true,
            scroll_line_count: DEFAULT_SCROLL_LINE_COUNT,
            wheel_scroll: WheelScroll::default(),
        }
    }
}

/// What the mouse wheel does over a plain pane (see [`MouseConfig::wheel_scroll`]).
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
    /// Whether trailing whitespace is trimmed from copied text.
    pub should_trim_trailing_whitespace: bool,
}

impl Default for CopyConfig {
    fn default() -> Self {
        Self {
            should_trim_trailing_whitespace: true,
        }
    }
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
    /// were read from, or [`DEFAULT_THEME_NAME`] when the built-in colors are in
    /// effect.
    pub theme_name: String,
    /// The theme's colors.
    pub colors: ColorPalette,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            theme_name: DEFAULT_THEME_NAME.to_string(),
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
    /// Border of the pane the pointer is over: the pane the wheel scrolls.
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
        // Six ASCII hex digits: one byte per character. Each two-byte slice
        // is one hex pair.
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
    /// above [`log_level`](Self::log_level) are written to a per-session file under
    /// the platform state directory, created on the first line written.
    pub is_enabled: bool,
    /// The lowest severity that gets written. A line below this is dropped —
    /// e.g. [`LogLevel::Warning`] drops `info` lines.
    pub log_level: LogLevel,
    /// How each written line is rendered.
    pub log_format: LogFormat,
    /// The size a log file may reach, in mebibytes of 1,048,576 bytes, from
    /// [`LOWEST_MAX_LOG_FILE_SIZE_IN_MB`] to [`HIGHEST_MAX_LOG_FILE_SIZE_IN_MB`].
    /// A line that takes the file over it moves the file to `<name>.1`.
    pub max_log_file_size_in_mb: u32,
}

impl Default for LoggingConfig {
    /// Logging is off, and when turned on writes warnings and errors in the
    /// human-readable format, and moves a file to `<name>.1` over 4 MiB.
    fn default() -> Self {
        Self {
            is_enabled: false,
            log_level: LogLevel::Warning,
            log_format: LogFormat::Pretty,
            max_log_file_size_in_mb: DEFAULT_MAX_LOG_FILE_SIZE_IN_MB,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
