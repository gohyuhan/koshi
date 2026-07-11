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
use koshi_core::key::{Key, KeySequence, ModFlags};
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
    assert_eq!(row_text(&draw(&snapshot, 80)), "<C-l> Lock  <C-p> PANE");
}

#[test]
fn user_entry_under_prefix_swaps_label_for_count() {
    let snapshot = snap(pane_fixture(true), None);
    assert_eq!(row_text(&draw(&snapshot, 80)), "<C-l> Lock  <C-p> +2");
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
    assert_eq!(row_text(&draw(&snapshot, 80)), "<C-p> +1");
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
    assert_eq!(row_text(&draw(&snapshot, 80)), "<C-t> +2");
}

#[test]
fn pending_prefix_shows_breadcrumb_and_continuations() {
    let snapshot = snap(pane_fixture(false), Some(seq(&[ctrl('p')])));
    assert_eq!(
        row_text(&draw(&snapshot, 80)),
        "<C-p> PANE ▸ n New Pane  x Close Pane"
    );
}

#[test]
fn pending_prefix_without_label_shows_bare_breadcrumb() {
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
    assert_eq!(row_text(&draw(&snapshot, 80)), "<C-t> ▸ n New Tab");
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
    assert_eq!(row_text(&draw(&snapshot, 80)), "<C-p> PANE ▸ n +2");
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
    assert_eq!(row_text(&draw(&snapshot, 80)), "<C-p> Pane Menu +1");
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
        "<C-g> Unlock  <C-a> Aardvark"
    );
    // Narrow: only the pinned hint fits; the other is dropped whole.
    assert_eq!(row_text(&draw(&snapshot, 14)), "<C-g> Unlock");
}

#[test]
fn truncation_drops_whole_trailing_hints() {
    let snapshot = snap(pane_fixture(false), None);
    // `<C-l> Lock` is 10 cells; the next hint would start at 12 and needs 10
    // more, so any width under 22 keeps only the first.
    assert_eq!(row_text(&draw(&snapshot, 21)), "<C-l> Lock");
    assert_eq!(row_text(&draw(&snapshot, 22)), "<C-l> Lock  <C-p> PANE");
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
    assert_eq!(row, "<C-l> Lock  <C-p> PANE [keys!]");
    // The marker's last cell is the row's last cell.
    assert_eq!(buf[(29, 0)].symbol(), "]");
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
