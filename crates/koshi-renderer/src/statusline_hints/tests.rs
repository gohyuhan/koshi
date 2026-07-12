//! Tests for the keybinding hint bar: idle grouping (leaf hints, labeled
//! default prefix groups, `+N` fallbacks once a user entry or removal touches
//! a group), the pending-sequence face (breadcrumb plus continuations, nested
//! groups), pinned-first ordering, whole-item truncation, the right-aligned
//! keymap-revert marker, and the blanked row for a mode with nothing to hint.

use super::*;

use std::collections::BTreeSet;
use std::sync::Arc;

use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, SessionId, TabId};
use koshi_core::key::{Key, KeySequence, ModFlags, NamedKey};
use koshi_core::lock::LockMode;
use koshi_layout::mode::LayoutMode;

use crate::snapshot::{
    ClientSnapshot, HintBinding, PluginUiSnapshot, SessionSnapshot, TabSnapshot,
};

/// A `Ctrl`-modified character chord.
fn ctrl(key: char) -> KeyChord {
    KeyChord::new(ModFlags::CTRL, Key::Char(key))
}

/// An unmodified character chord.
fn plain(key: char) -> KeyChord {
    KeyChord::new(ModFlags::NONE, Key::Char(key))
}

/// A sequence from chords in press order.
fn seq(chords: &[KeyChord]) -> KeySequence {
    KeySequence::new(chords[0], chords[1..].to_vec())
}

/// A hint binding with the given flags.
fn binding(sequence: KeySequence, label: &str, user_set: bool, pinned: bool) -> HintBinding {
    HintBinding {
        sequence,
        label: label.to_string(),
        user_set,
        pinned,
    }
}

/// Assemble a [`KeymapHints`] from its parts.
fn hints(
    entries: Vec<HintBinding>,
    labels: &[(KeyChord, &str)],
    removed: Vec<KeySequence>,
    reverted: bool,
) -> KeymapHints {
    KeymapHints {
        entries: Arc::new(entries),
        prefix_labels: Arc::new(
            labels
                .iter()
                .map(|(chord, label)| (*chord, (*label).to_string()))
                .collect(),
        ),
        removed: Arc::new(removed.into_iter().collect::<BTreeSet<_>>()),
        reverted,
    }
}

/// A skeletal snapshot carrying only what the hint bar reads: the hints and
/// the client's pending sequence.
fn snap(keymap_hints: KeymapHints, pending: Option<KeySequence>) -> RenderSnapshot {
    let tab_id = TabId::new();
    RenderSnapshot {
        session: SessionSnapshot {
            id: SessionId::new(),
            name: String::new(),
            active_tab: TabSnapshot {
                id: tab_id,
                name: String::new(),
                layout_solved: Vec::new(),
                effective_size: Size { cols: 80, rows: 24 },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                all_suppressed: false,
            },
            tabs_metadata: Vec::new(),
        },
        panes: Vec::new(),
        client: ClientSnapshot {
            id: ClientId::new(),
            viewport: Size { cols: 80, rows: 24 },
            active_tab: tab_id,
            focused_pane: None,
            lock_mode: LockMode::Normal,
            pending_sequence: pending,
        },
        plugin_ui: PluginUiSnapshot::default(),
        keymap_hints,
        theme: Theme::default(),
    }
}

/// Draw the bar into a fresh one-row buffer of `width` cells.
fn draw(snapshot: &RenderSnapshot, width: u16) -> Buffer {
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width,
        height: 1,
    };
    let mut buf = Buffer::empty(area);
    draw_hint_bar(snapshot, area, &mut buf);
    buf
}

/// The buffer's single row as a string, trailing spaces trimmed.
fn row_text(buf: &Buffer) -> String {
    let row: String = (0..buf.area.width)
        .map(|x| buf[(x, 0)].symbol().to_string())
        .collect::<Vec<_>>()
        .join("");
    row.trim_end().to_string()
}

/// The default-shaped fixture: two sequences under `<C-p>` labeled `PANE`,
/// plus a single-chord `Lock` binding.
fn pane_fixture(user_close: bool) -> KeymapHints {
    hints(
        vec![
            binding(seq(&[ctrl('l')]), "Lock", false, false),
            binding(seq(&[ctrl('p'), plain('n')]), "New Pane", false, false),
            binding(
                seq(&[ctrl('p'), plain('x')]),
                "Close Pane",
                user_close,
                false,
            ),
        ],
        &[(ctrl('p'), "PANE")],
        Vec::new(),
        false,
    )
}

#[test]
fn idle_shows_leaf_hints_and_labeled_default_group() {
    let snapshot = snap(pane_fixture(false), None);
    assert_eq!(row_text(&draw(&snapshot, 80)), " Ctrl +  l  Lock  p  PANE");
}

#[test]
fn modifier_key_and_action_ribbons_use_the_group_ramp_stop() {
    let snapshot = snap(pane_fixture(false), None);
    let buf = draw(&snapshot, 80);
    // One modifier group → the ramp's purple end everywhere in it: the
    // header as text color, the key block as background, the label block as
    // the dimmed background.
    let purple = Color::Rgb(0x58, 0x1c, 0x87);
    let purple_dim = Color::Rgb(0x30, 0x0f, 0x4a);
    assert_eq!(buf[(1, 0)].fg, purple);
    assert!(buf[(1, 0)].modifier.contains(Modifier::BOLD));
    assert_eq!(buf[(9, 0)].bg, purple);
    assert_eq!(buf[(9, 0)].fg, Color::Rgb(0xf4, 0xf1, 0xfa));
    assert_eq!(buf[(12, 0)].bg, purple_dim);
    assert_eq!(buf[(12, 0)].fg, Color::Rgb(0xc9, 0xc4, 0xd4));
}

#[test]
fn human_modifier_groups_fold_same_action_keys() {
    let keymap = hints(
        vec![
            binding(
                seq(&[KeyChord::new(ModFlags::CTRL, Key::Named(NamedKey::Left))]),
                "Focus Pane",
                false,
                false,
            ),
            binding(
                seq(&[KeyChord::new(ModFlags::CTRL, Key::Named(NamedKey::Down))]),
                "Focus Pane",
                false,
                false,
            ),
            binding(
                seq(&[KeyChord::new(ModFlags::ALT, Key::Char('h'))]),
                "Focus Pane",
                false,
                false,
            ),
            binding(
                seq(&[KeyChord::new(ModFlags::ALT, Key::Char('j'))]),
                "Focus Pane",
                false,
                false,
            ),
        ],
        &[],
        Vec::new(),
        false,
    );
    let snapshot = snap(keymap, None);
    assert_eq!(
        row_text(&draw(&snapshot, 80)),
        " Ctrl +  ←↓  Focus Pane  Alt +  hj  Focus Pane"
    );
}

#[test]
fn bare_key_wears_the_header_style_not_a_key_block() {
    let shift_tab = KeyChord::new(ModFlags::SHIFT, Key::Named(NamedKey::Tab));
    let bare_tab = KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Tab));
    let keymap = hints(
        vec![
            binding(seq(&[ctrl('l')]), "Lock", false, false),
            binding(seq(&[shift_tab]), "Previous Tab", false, false),
            binding(seq(&[bare_tab]), "Next Tab", false, false),
        ],
        &[],
        Vec::new(),
        false,
    );
    let snapshot = snap(keymap, None);
    let buf = draw(&snapshot, 80);
    assert_eq!(
        row_text(&buf),
        " Ctrl +  l  Lock  Shift +  Tab  Previous Tab  Tab  Next Tab"
    );
    // The Shift group's key is a block: light text on the mid ramp stop.
    assert_eq!(buf[(27, 0)].bg, Color::Rgb(0x4a, 0x4f, 0xbe));
    assert_eq!(buf[(27, 0)].fg, Color::Rgb(0xf4, 0xf1, 0xfa));
    // The bare Tab is its own opener: header-styled text on the bar itself —
    // the blue ramp end as foreground, no block behind it.
    assert_eq!(buf[(46, 0)].fg, Color::Rgb(0x3b, 0x82, 0xf6));
    assert_eq!(buf[(46, 0)].bg, Color::Reset);
    assert!(buf[(46, 0)].modifier.contains(Modifier::BOLD));
    // Its action label keeps the dimmed block, same as any other ribbon.
    assert_eq!(buf[(51, 0)].bg, Color::Rgb(0x20, 0x47, 0x87));
}

#[test]
fn user_entry_under_prefix_swaps_label_for_count() {
    let snapshot = snap(pane_fixture(true), None);
    assert_eq!(row_text(&draw(&snapshot, 80)), " Ctrl +  l  Lock  p  +2");
}

#[test]
fn removal_under_prefix_swaps_label_for_count() {
    let keymap = hints(
        vec![binding(
            seq(&[ctrl('p'), plain('n')]),
            "New Pane",
            false,
            false,
        )],
        &[(ctrl('p'), "PANE")],
        vec![seq(&[ctrl('p'), plain('x')])],
        false,
    );
    let snapshot = snap(keymap, None);
    assert_eq!(row_text(&draw(&snapshot, 80)), " Ctrl +  p  +1");
}

#[test]
fn unlabeled_group_shows_count() {
    let keymap = hints(
        vec![
            binding(seq(&[ctrl('t'), plain('n')]), "New Tab", false, false),
            binding(seq(&[ctrl('t'), plain('x')]), "Close Tab", false, false),
        ],
        &[],
        Vec::new(),
        false,
    );
    let snapshot = snap(keymap, None);
    assert_eq!(row_text(&draw(&snapshot, 80)), " Ctrl +  t  +2");
}

#[test]
fn pending_prefix_shows_breadcrumb_and_continuations() {
    let snapshot = snap(pane_fixture(false), Some(seq(&[ctrl('p')])));
    assert_eq!(
        row_text(&draw(&snapshot, 80)),
        " Ctrl +  p  PANE  ▶  n  New Pane  x  Close Pane"
    );
}

#[test]
fn customized_pending_prefix_uses_count_not_shipped_label() {
    let snapshot = snap(pane_fixture(true), Some(seq(&[ctrl('p')])));
    assert_eq!(
        row_text(&draw(&snapshot, 80)),
        " Ctrl +  p  +2  ▶  n  New Pane  x  Close Pane"
    );
}

#[test]
fn pending_prefix_without_label_shows_derived_count() {
    let keymap = hints(
        vec![binding(
            seq(&[ctrl('t'), plain('n')]),
            "New Tab",
            false,
            false,
        )],
        &[],
        Vec::new(),
        false,
    );
    let snapshot = snap(keymap, Some(seq(&[ctrl('t')])));
    assert_eq!(
        row_text(&draw(&snapshot, 80)),
        " Ctrl +  t  +1  ▶  n  New Tab"
    );
}

#[test]
fn nested_group_inside_pending_shows_count() {
    let keymap = hints(
        vec![
            binding(
                seq(&[ctrl('p'), plain('n'), plain('a')]),
                "Deep A",
                false,
                false,
            ),
            binding(
                seq(&[ctrl('p'), plain('n'), plain('b')]),
                "Deep B",
                false,
                false,
            ),
        ],
        &[(ctrl('p'), "PANE")],
        Vec::new(),
        false,
    );
    let snapshot = snap(keymap, Some(seq(&[ctrl('p')])));
    assert_eq!(row_text(&draw(&snapshot, 80)), " Ctrl +  p  PANE  ▶  n  +2");
}

#[test]
fn chord_bound_and_extended_shows_action_with_count() {
    let keymap = hints(
        vec![
            binding(seq(&[ctrl('p')]), "Pane Menu", false, false),
            binding(seq(&[ctrl('p'), plain('n')]), "New Pane", false, false),
        ],
        &[(ctrl('p'), "PANE")],
        Vec::new(),
        false,
    );
    let snapshot = snap(keymap, None);
    assert_eq!(row_text(&draw(&snapshot, 80)), " Ctrl +  p  Pane Menu +1");
}

#[test]
fn pinned_hint_sorts_first_and_survives_truncation() {
    let keymap = hints(
        vec![
            binding(seq(&[ctrl('a')]), "Aardvark", false, false),
            binding(seq(&[ctrl('g')]), "Unlock", false, true),
        ],
        &[],
        Vec::new(),
        false,
    );
    let snapshot = snap(keymap, None);
    // Wide: pinned first despite `<C-a>` sorting lower.
    assert_eq!(
        row_text(&draw(&snapshot, 80)),
        " Ctrl +  g  Unlock  a  Aardvark"
    );
    // Narrow: only the pinned hint fits; the dropped one leaves a `…`.
    assert_eq!(row_text(&draw(&snapshot, 19)), " Ctrl +  g  Unlock…");
}

#[test]
fn truncation_drops_whole_trailing_hints() {
    let snapshot = snap(pane_fixture(false), None);
    // Shared `Ctrl +` header plus the first ribbon is 17 cells; the second
    // ribbon needs 9 more, so below 26 it is dropped whole behind a `…`.
    assert_eq!(row_text(&draw(&snapshot, 25)), " Ctrl +  l  Lock …");
    assert_eq!(row_text(&draw(&snapshot, 26)), " Ctrl +  l  Lock  p  PANE");
}

#[test]
fn revert_marker_holds_right_edge_and_hints_stop_short() {
    let keymap = KeymapHints {
        reverted: true,
        ..pane_fixture(false)
    };
    let snapshot = snap(keymap, None);
    let buf = draw(&snapshot, 30);
    let row = row_text(&buf);
    assert_eq!(row, " Ctrl +  l  Lock …      keys!");
    // Marker text holds the right edge, with one background-padding cell.
    assert_eq!(buf[(28, 0)].symbol(), "!");
}

#[test]
fn empty_mode_blanks_the_row() {
    let snapshot = snap(hints(Vec::new(), &[], Vec::new(), false), None);
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: 20,
        height: 1,
    };
    let mut buf = Buffer::empty(area);
    // Pre-fill the row: the bar owns it, so stale cells must be cleared.
    buf.set_string(0, 0, "X".repeat(20), Style::default());
    draw_hint_bar(&snapshot, area, &mut buf);
    assert_eq!(row_text(&buf), "");
}

#[test]
fn zero_size_area_draws_nothing() {
    let snapshot = snap(pane_fixture(false), None);
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    };
    let mut buf = Buffer::empty(RatatuiRect {
        x: 0,
        y: 0,
        width: 10,
        height: 1,
    });
    draw_hint_bar(&snapshot, area, &mut buf);
    assert_eq!(row_text(&buf), "");
}

/// A non-default palette recolors the bar: the pending breadcrumb takes the
/// theme's accent pair and a group's key block sits on the custom ramp.
#[test]
fn a_custom_theme_recolors_the_bar() {
    let mut snapshot = snap(pane_fixture(false), Some(seq(&[ctrl('p')])));
    snapshot.theme = Theme {
        ramp_start: (0xff, 0x00, 0x00),
        ramp_end: (0x00, 0x00, 0xff),
        accent: Color::Rgb(0x00, 0xff, 0x00),
        on_accent: Color::Rgb(0x01, 0x02, 0x03),
        ..Theme::default()
    };
    let buf = draw(&snapshot, 80);
    // Row: " Ctrl +  p  PANE  ▶  n  New Pane …". The breadcrumb's `Ctrl +`
    // is accent text; its key block is on-accent text on the accent.
    assert_eq!(buf[(1, 0)].fg, Color::Rgb(0x00, 0xff, 0x00));
    assert_eq!(buf[(9, 0)].fg, Color::Rgb(0x01, 0x02, 0x03));
    assert_eq!(buf[(9, 0)].bg, Color::Rgb(0x00, 0xff, 0x00));
    // The modifier-less continuation key wears the group's header style: the
    // custom ramp's start stop as its text color.
    let n_x = (0..80)
        .find(|&x| buf[(x, 0)].symbol() == "n")
        .expect("continuation key drawn");
    assert_eq!(buf[(n_x, 0)].fg, Color::Rgb(0xff, 0x00, 0x00));
}
