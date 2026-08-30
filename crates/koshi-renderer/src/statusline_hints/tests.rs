//! Tests for the keybinding hint bar: idle grouping (leaf hints, labeled
//! default prefix groups, `+N` fallbacks once a user entry or removal touches
//! a group), the pending-sequence face (breadcrumb plus continuations, nested
//! groups), pinned-first ordering, whole-item truncation, the right-aligned
//! keymap-revert marker, the blanked row for a mode with nothing to hint, and
//! the cells outside the given area that the bar leaves untouched.

use super::*;

use std::collections::BTreeSet;
use std::sync::Arc;

use koshi_core::key::{Key, KeySequence, ModFlags, NamedKey};
use ratatui::buffer::Cell;

use crate::snapshot::HintBinding;

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

/// Draw the bar into a fresh one-row buffer of `width` cells.
fn draw(hints: &KeymapHints, width: u16) -> Buffer {
    draw_themed(hints, &Theme::default(), width)
}

/// Paint `hints` into `buf` over `area`, in `theme`'s colors. `pending` carries
/// the chords already pressed of an open key sequence, and is `None` when no
/// sequence is open.
fn paint_bar(
    hints: &KeymapHints,
    theme: &Theme,
    pending: Option<&KeySequence>,
    area: RatatuiRect,
    buf: &mut Buffer,
) {
    draw_hint_bar(
        &StatuslineDto {
            hints,
            theme,
            pending,
        },
        area,
        buf,
    );
}

/// Draw in `theme`'s colors with an open sequence.
fn draw_themed_pending(
    hints: &KeymapHints,
    theme: &Theme,
    pending: &KeySequence,
    width: u16,
) -> Buffer {
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width,
        height: 1,
    };
    let mut buf = Buffer::empty(area);
    paint_bar(hints, theme, Some(pending), area, &mut buf);
    buf
}

/// Draw with an open sequence, which the viewer owns and hands to the bar.
fn draw_pending(hints: &KeymapHints, pending: &KeySequence, width: u16) -> Buffer {
    draw_themed_pending(hints, &Theme::default(), pending, width)
}

/// Paint the hint bar in `theme`'s colors, for the tests that check which
/// color a piece of the bar takes.
fn draw_themed(hints: &KeymapHints, theme: &Theme, width: u16) -> Buffer {
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width,
        height: 1,
    };
    let mut buf = Buffer::empty(area);
    paint_bar(hints, theme, None, area, &mut buf);
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

/// One cell per `char` of `text`, each carrying that char and `style`.
///
/// `painted("ab", Style::default().fg(Color::Reset))` gives two cells whose
/// symbols are `a` and `b`.
fn painted(text: &str, style: Style) -> Vec<Cell> {
    text.chars()
        .map(|c| {
            let mut cell = Cell::default();
            cell.set_char(c);
            cell.set_style(style);
            cell
        })
        .collect()
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
    let bar = pane_fixture(false);
    assert_eq!(row_text(&draw(&bar, 80)), " Ctrl +  l  Lock  p  PANE");
}

#[test]
fn modifier_key_and_action_ribbons_use_the_group_ramp_stop() {
    let bar = pane_fixture(false);
    let buf = draw(&bar, 80);
    // One modifier group → the ramp's purple end everywhere in it: the
    // header as text color, the key block as background, the label block as
    // the dimmed background.
    let purple = Color::Rgb(0xd0, 0xa5, 0xff);
    let purple_dim = Color::Rgb(0x72, 0x5a, 0x8c);
    assert_eq!(buf[(1, 0)].fg, purple);
    assert!(buf[(1, 0)].modifier.contains(Modifier::BOLD));
    assert_eq!(buf[(9, 0)].bg, purple);
    assert_eq!(buf[(9, 0)].fg, Color::Rgb(0x12, 0x09, 0x1f));
    assert_eq!(buf[(12, 0)].bg, purple_dim);
    assert_eq!(buf[(12, 0)].fg, Color::Rgb(0xf0, 0xec, 0xfa));
}

#[test]
fn every_cell_of_the_hint_row_is_painted_the_same_way() {
    let buf = draw(&pane_fixture(false), 30);
    let header = Style::default()
        .fg(Color::Rgb(0xd0, 0xa5, 0xff))
        .bg(Color::Rgb(0x00, 0x00, 0x00))
        .add_modifier(Modifier::BOLD);
    let key = Style::default()
        .fg(Color::Rgb(0x12, 0x09, 0x1f))
        .bg(Color::Rgb(0xd0, 0xa5, 0xff))
        .add_modifier(Modifier::BOLD);
    let label = Style::default()
        .fg(Color::Rgb(0xf0, 0xec, 0xfa))
        .bg(Color::Rgb(0x72, 0x5a, 0x8c));
    let fill = Style::default().bg(Color::Rgb(0x00, 0x00, 0x00));
    let expected: Vec<Cell> = [
        painted(" Ctrl + ", header),
        painted(" l ", key),
        painted(" Lock ", label),
        painted(" p ", key),
        painted(" PANE ", label),
        painted("    ", fill),
    ]
    .concat();
    assert_eq!(expected.len(), 30);
    for (x, want) in expected.iter().enumerate() {
        assert_eq!(buf[(x as u16, 0)], *want, "col {x}");
    }
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
    let bar = keymap;
    assert_eq!(
        row_text(&draw(&bar, 80)),
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
    let bar = keymap;
    let buf = draw(&bar, 80);
    assert_eq!(
        row_text(&buf),
        " Ctrl +  l  Lock  Shift +  Tab  Previous Tab  Tab  Next Tab"
    );
    // The Shift group's key is a block: dark text on the mid ramp stop.
    assert_eq!(buf[(27, 0)].bg, Color::Rgb(0xa7, 0xb0, 0xff));
    assert_eq!(buf[(27, 0)].fg, Color::Rgb(0x12, 0x09, 0x1f));
    // The bare Tab is its own opener: header-styled text on the bar itself —
    // the blue ramp end as foreground over the bar background, no block
    // behind it.
    assert_eq!(buf[(46, 0)].fg, Color::Rgb(0x7d, 0xbc, 0xff));
    assert_eq!(buf[(46, 0)].bg, Color::Rgb(0x00, 0x00, 0x00));
    assert!(buf[(46, 0)].modifier.contains(Modifier::BOLD));
    // Its action label keeps the dimmed block, same as any other ribbon.
    assert_eq!(buf[(51, 0)].bg, Color::Rgb(0x44, 0x67, 0x8c));
}

#[test]
fn arrow_keys_sort_left_down_up_right_ahead_of_other_keys() {
    let arrow = |named| {
        binding(
            seq(&[KeyChord::new(ModFlags::CTRL, Key::Named(named))]),
            "Focus Pane",
            false,
            false,
        )
    };
    let keymap = hints(
        vec![
            arrow(NamedKey::Right),
            binding(seq(&[ctrl('z')]), "Focus Pane", false, false),
            arrow(NamedKey::Up),
            arrow(NamedKey::Left),
            arrow(NamedKey::Down),
        ],
        &[],
        Vec::new(),
        false,
    );
    // One action, so all five keys fold into one ribbon: the four arrows read
    // in screen order, then every other key.
    assert_eq!(row_text(&draw(&keymap, 80)), " Ctrl +  ←↓↑→z  Focus Pane");
}

#[test]
fn named_keys_read_as_their_own_names() {
    let named = |key, label: &str| {
        binding(
            seq(&[KeyChord::new(ModFlags::NONE, Key::Named(key))]),
            label,
            false,
            false,
        )
    };
    let keymap = hints(
        vec![
            named(NamedKey::Enter, "Accept"),
            named(NamedKey::Esc, "Cancel"),
            named(NamedKey::Space, "Pick"),
            named(NamedKey::Backspace, "Undo"),
        ],
        &[],
        Vec::new(),
        false,
    );
    assert_eq!(
        row_text(&draw(&keymap, 80)),
        " BACKSPACE  Undo  ENTER  Accept  ESC  Cancel  SPACE  Pick"
    );
}

#[test]
fn user_entry_under_prefix_swaps_label_for_count() {
    let bar = pane_fixture(true);
    assert_eq!(row_text(&draw(&bar, 80)), " Ctrl +  l  Lock  p  +2");
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
    let bar = keymap;
    assert_eq!(row_text(&draw(&bar, 80)), " Ctrl +  p  +1");
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
    let bar = keymap;
    assert_eq!(row_text(&draw(&bar, 80)), " Ctrl +  t  +2");
}

#[test]
fn pending_prefix_shows_breadcrumb_and_continuations() {
    let pending = seq(&[ctrl('p')]);
    let bar = pane_fixture(false);
    assert_eq!(
        row_text(&draw_pending(&bar, &pending, 80)),
        " Ctrl +  p  PANE  ▶  n  New Pane  x  Close Pane"
    );
}

#[test]
fn pending_prefix_with_no_continuations_shows_bare_breadcrumb_and_no_groups() {
    // The user pressed a chord that isn't a prefix of anything bound: no
    // matching entries means no label and no continuation groups — just the
    // breadcrumb and arrow, with no panic on the now-empty group list.
    let pending = seq(&[ctrl('z')]);
    let bar = pane_fixture(false);
    assert_eq!(row_text(&draw_pending(&bar, &pending, 80)), " Ctrl +  z  ▶");
}

#[test]
fn customized_pending_prefix_uses_count_not_shipped_label() {
    let pending = seq(&[ctrl('p')]);
    let bar = pane_fixture(true);
    assert_eq!(
        row_text(&draw_pending(&bar, &pending, 80)),
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
    let pending = seq(&[ctrl('t')]);
    let bar = keymap;
    assert_eq!(
        row_text(&draw_pending(&bar, &pending, 80)),
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
    let pending = seq(&[ctrl('p')]);
    let bar = keymap;
    assert_eq!(
        row_text(&draw_pending(&bar, &pending, 80)),
        " Ctrl +  p  PANE  ▶  n  +2"
    );
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
    let bar = keymap;
    assert_eq!(row_text(&draw(&bar, 80)), " Ctrl +  p  Pane Menu +1");
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
    let bar = keymap;
    // Wide: pinned first despite `<C-a>` sorting lower.
    assert_eq!(row_text(&draw(&bar, 80)), " Ctrl +  g  Unlock  a  Aardvark");
    // Narrow: only the pinned hint fits; the dropped one leaves a `…`.
    assert_eq!(row_text(&draw(&bar, 19)), " Ctrl +  g  Unlock…");
}

#[test]
fn a_pinned_hint_does_not_pull_its_modifier_group_ahead() {
    // Pinned puts a hint first inside its own group; the groups themselves
    // still read in modifier order, so `Ctrl` leads the pinned `Alt` hint.
    let keymap = hints(
        vec![
            binding(seq(&[ctrl('l')]), "Lock", false, false),
            binding(
                seq(&[KeyChord::new(ModFlags::ALT, Key::Char('u'))]),
                "Unlock",
                false,
                true,
            ),
        ],
        &[],
        Vec::new(),
        false,
    );
    assert_eq!(
        row_text(&draw(&keymap, 80)),
        " Ctrl +  l  Lock  Alt +  u  Unlock"
    );
}

#[test]
fn a_pinned_and_an_unpinned_hint_with_one_label_stay_two_ribbons() {
    // Folding keys into one ribbon needs the same label *and* the same pinned
    // flag, so these two keep their own blocks with the pinned one first.
    let keymap = hints(
        vec![
            binding(seq(&[ctrl('w')]), "Save", false, false),
            binding(seq(&[ctrl('s')]), "Save", false, true),
        ],
        &[],
        Vec::new(),
        false,
    );
    assert_eq!(row_text(&draw(&keymap, 80)), " Ctrl +  s  Save  w  Save");
}

#[test]
fn truncation_drops_whole_trailing_hints() {
    let bar = pane_fixture(false);
    // Shared `Ctrl +` header plus the first ribbon is 17 cells; the second
    // ribbon needs 9 more, so below 26 it is dropped whole behind a `…`.
    assert_eq!(row_text(&draw(&bar, 25)), " Ctrl +  l  Lock …");
    assert_eq!(row_text(&draw(&bar, 26)), " Ctrl +  l  Lock  p  PANE");
}

#[test]
fn a_group_header_is_never_painted_without_its_first_ribbon() {
    let keymap = hints(
        vec![
            binding(seq(&[ctrl('l')]), "Lock", false, false),
            binding(
                seq(&[KeyChord::new(ModFlags::ALT, Key::Char('u'))]),
                "Unlock",
                false,
                false,
            ),
        ],
        &[],
        Vec::new(),
        false,
    );
    // The `Ctrl` group is 17 cells, the `Alt` group's ` Alt + ` header plus its
    // first ribbon is 18 more. One cell short of both, the header is skipped
    // whole rather than painted over an empty group.
    assert_eq!(row_text(&draw(&keymap, 34)), " Ctrl +  l  Lock …");
    assert_eq!(
        row_text(&draw(&keymap, 35)),
        " Ctrl +  l  Lock  Alt +  u  Unlock"
    );
}

#[test]
fn a_breadcrumb_with_no_room_for_the_arrow_ends_in_the_overflow_marker() {
    let pending = seq(&[ctrl('p')]);
    let bar = pane_fixture(false);
    // The breadcrumb ` Ctrl +  p  PANE ` is 17 cells and the ` ▶ ` arrow 3
    // more. At 17 the arrow is dropped and the `…` takes the breadcrumb's last
    // cell; at 20 both fit and the `…` stands for the dropped hint groups.
    assert_eq!(
        row_text(&draw_pending(&bar, &pending, 17)),
        " Ctrl +  p  PANE…"
    );
    assert_eq!(
        row_text(&draw_pending(&bar, &pending, 20)),
        " Ctrl +  p  PANE  ▶…"
    );
}

#[test]
fn an_overflow_marker_with_no_cell_left_takes_the_last_one() {
    let bar = pane_fixture(false);
    // The `Ctrl +` header plus the first ribbon fill all 17 cells exactly, so
    // the `…` standing for the dropped second ribbon has no cell of its own and
    // overwrites the last cell of the ribbon before it.
    assert_eq!(row_text(&draw(&bar, 17)), " Ctrl +  l  Lock…");
}

#[test]
fn the_overflow_marker_is_bold_dim_ramp_text_on_the_bar_background() {
    let buf = draw(&pane_fixture(false), 25);
    // Column 17 is the first cell past the ` Ctrl +  l  Lock ` group, so the
    // `…` there lands on bar background rather than on a ribbon.
    assert_eq!(buf[(17, 0)].symbol(), "…");
    assert_eq!(buf[(17, 0)].fg, Color::Rgb(0xf0, 0xec, 0xfa));
    assert_eq!(buf[(17, 0)].bg, Color::Rgb(0x00, 0x00, 0x00));
    assert!(buf[(17, 0)].modifier.contains(Modifier::BOLD));
}

#[test]
fn the_revert_marker_is_bold_white_on_red() {
    let keymap = KeymapHints {
        reverted: true,
        ..pane_fixture(false)
    };
    let buf = draw(&keymap, 30);
    assert_eq!(buf[(24, 0)].symbol(), "k");
    assert_eq!(buf[(24, 0)].fg, Color::White);
    assert_eq!(buf[(24, 0)].bg, Color::Red);
    assert!(buf[(24, 0)].modifier.contains(Modifier::BOLD));
}

#[test]
fn a_named_key_with_no_symbol_reads_as_its_chord_spelling() {
    let named = |key, label: &str| {
        binding(
            seq(&[KeyChord::new(ModFlags::NONE, Key::Named(key))]),
            label,
            false,
            false,
        )
    };
    let keymap = hints(
        vec![
            named(NamedKey::Home, "Top"),
            named(NamedKey::End, "Bottom"),
            named(NamedKey::PageUp, "Page Up"),
            named(NamedKey::Delete, "Delete"),
            named(NamedKey::F(1), "Help"),
        ],
        &[],
        Vec::new(),
        false,
    );
    assert_eq!(
        row_text(&draw(&keymap, 80)),
        " Del  Delete  End  Bottom  F1  Help  Home  Top  PageUp  Page Up"
    );
}

#[test]
fn the_bar_paints_only_the_cells_of_the_area_it_is_given() {
    let keymap = hints(
        vec![binding(seq(&[plain('q')]), "Go", false, false)],
        &[],
        Vec::new(),
        false,
    );
    let buf_area = RatatuiRect {
        x: 0,
        y: 0,
        width: 20,
        height: 3,
    };
    let mut buf = Buffer::empty(buf_area);
    for y in 0..3 {
        buf.set_string(0, y, "X".repeat(20), Style::default());
    }

    paint_bar(
        &keymap,
        &Theme::default(),
        None,
        RatatuiRect {
            x: 3,
            y: 1,
            width: 12,
            height: 1,
        },
        &mut buf,
    );

    let row = |y: u16| -> String { (0..20).map(|x| buf[(x, y)].symbol().to_string()).collect() };
    assert_eq!(row(0), "X".repeat(20));
    assert_eq!(row(1), "XXX q  Go      XXXXX");
    assert_eq!(row(2), "X".repeat(20));
}

#[test]
fn a_removal_under_a_pending_prefix_swaps_every_label_it_touches_for_a_count() {
    let entries = || {
        vec![binding(
            seq(&[ctrl('p'), plain('n'), plain('a')]),
            "Deep A",
            false,
            false,
        )]
    };
    let labels: &[(KeyChord, &str)] = &[(ctrl('p'), "PANE"), (plain('n'), "NESTED")];
    let pending = seq(&[ctrl('p')]);

    let untouched = hints(entries(), labels, Vec::new(), false);
    assert_eq!(
        row_text(&draw_pending(&untouched, &pending, 80)),
        " Ctrl +  p  PANE  ▶  n  NESTED"
    );

    // `<C-p> n b` was removed. The removal sits under `<C-p>` and under
    // `<C-p> n`, so both labels give way to their binding counts.
    let with_removal = hints(
        entries(),
        labels,
        vec![seq(&[ctrl('p'), plain('n'), plain('b')])],
        false,
    );
    assert_eq!(
        row_text(&draw_pending(&with_removal, &pending, 80)),
        " Ctrl +  p  +1  ▶  n  +1"
    );
}

#[test]
fn revert_marker_holds_right_edge_and_hints_stop_short() {
    let keymap = KeymapHints {
        reverted: true,
        ..pane_fixture(false)
    };
    let bar = keymap;
    let buf = draw(&bar, 30);
    let row = row_text(&buf);
    assert_eq!(row, " Ctrl +  l  Lock …      keys!");
    // Marker text holds the right edge, with one background-padding cell.
    assert_eq!(buf[(28, 0)].symbol(), "!");
}

#[test]
fn modifier_groups_read_ctrl_alt_ctrl_shift_shift_super_then_the_rest() {
    let entry = |mods, key: char, label: &str| {
        binding(
            seq(&[KeyChord::new(mods, Key::Char(key))]),
            label,
            false,
            false,
        )
    };
    // Fed in reverse of the order they must come out in.
    let keymap = hints(
        vec![
            entry(ModFlags::CTRL | ModFlags::ALT, 'f', "CtrlAlt"),
            entry(ModFlags::NONE, 'g', "Bare"),
            entry(ModFlags::SUPER, 'a', "Super"),
            entry(ModFlags::SHIFT, 'b', "Shift"),
            entry(ModFlags::CTRL | ModFlags::SHIFT, 'c', "CtrlShift"),
            entry(ModFlags::ALT, 'd', "Alt"),
            entry(ModFlags::CTRL, 'e', "Ctrl"),
        ],
        &[],
        Vec::new(),
        false,
    );
    assert_eq!(
        row_text(&draw(&keymap, 200)),
        concat!(
            " Ctrl +  e  Ctrl  Alt +  d  Alt  Ctrl+Shift +  c  CtrlShift ",
            " Shift +  b  Shift  Super +  a  Super  g  Bare  Ctrl+Alt +  f  CtrlAlt"
        )
    );
}

#[test]
fn only_the_opening_chord_of_a_pending_sequence_shows_a_prefix_label() {
    // `n` is a labeled top-level prefix in its own right, but here it is the
    // second chord of the open sequence, so its `NESTED` label stays off the
    // breadcrumb — only the chord that opened the sequence is labeled.
    let keymap = hints(
        vec![
            binding(
                seq(&[ctrl('p'), plain('n'), plain('a')]),
                "Deep A",
                false,
                false,
            ),
            binding(seq(&[plain('n'), plain('z')]), "Other", false, false),
        ],
        &[(ctrl('p'), "PANE"), (plain('n'), "NESTED")],
        Vec::new(),
        false,
    );
    let pending = seq(&[ctrl('p'), plain('n')]);
    assert_eq!(
        row_text(&draw_pending(&keymap, &pending, 80)),
        " Ctrl +  p  PANE  n  ▶  a  Deep A"
    );
}

#[test]
fn a_pending_sequence_that_is_itself_bound_lists_only_its_continuations() {
    // `<C-p>` runs an action of its own and also opens deeper bindings. Once it
    // is pending, its own entry is behind the viewer, so only `n` is listed.
    let keymap = hints(
        vec![
            binding(seq(&[ctrl('p')]), "Pane Menu", false, false),
            binding(seq(&[ctrl('p'), plain('n')]), "New Pane", false, false),
        ],
        &[(ctrl('p'), "PANE")],
        Vec::new(),
        false,
    );
    let pending = seq(&[ctrl('p')]);
    assert_eq!(
        row_text(&draw_pending(&keymap, &pending, 80)),
        " Ctrl +  p  PANE  ▶  n  New Pane"
    );
}

#[test]
fn a_row_too_narrow_for_the_breadcrumb_shows_only_the_overflow_marker() {
    // The breadcrumb ribbon ` Ctrl +  p  PANE ` needs 17 cells; at 10 nothing
    // of it fits, so the row is just the `…`.
    let pending = seq(&[ctrl('p')]);
    let bar = pane_fixture(false);
    assert_eq!(row_text(&draw_pending(&bar, &pending, 10)), "…");
    assert_eq!(row_text(&draw_pending(&bar, &pending, 1)), "…");
}

#[test]
fn the_revert_marker_and_the_overflow_marker_share_the_narrowest_row_that_fits_both() {
    // ` keys! ` is 7 cells; at 8 the marker takes the right edge and the `…`
    // for every dropped hint takes the one cell left of it.
    let keymap = KeymapHints {
        reverted: true,
        ..pane_fixture(false)
    };
    let buf = draw(&keymap, 8);
    assert_eq!(row_text(&buf), "… keys!");
    assert_eq!(buf[(0, 0)].symbol(), "…");
    assert_eq!(buf[(7, 0)].symbol(), " ");
}

#[test]
fn a_zero_height_area_leaves_the_row_untouched() {
    let bar = pane_fixture(false);
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: 10,
        height: 0,
    };
    let mut buf = Buffer::empty(RatatuiRect {
        x: 0,
        y: 0,
        width: 10,
        height: 1,
    });
    buf.set_string(0, 0, "X".repeat(10), Style::default());
    paint_bar(&bar, &Theme::default(), None, area, &mut buf);
    assert_eq!(row_text(&buf), "XXXXXXXXXX");
}

#[test]
fn empty_mode_blanks_the_row() {
    let bar = hints(Vec::new(), &[], Vec::new(), false);
    let area = RatatuiRect {
        x: 0,
        y: 0,
        width: 20,
        height: 1,
    };
    let mut buf = Buffer::empty(area);
    // Pre-fill the row: the bar owns it, so stale cells must be cleared.
    buf.set_string(0, 0, "X".repeat(20), Style::default());
    paint_bar(&bar, &Theme::default(), None, area, &mut buf);
    assert_eq!(row_text(&buf), "");
    // Blank of text, but not of color: the row still carries the bar
    // background, so an empty mode reads as a bar rather than a hole.
    for x in 0..20 {
        assert_eq!(buf[(x, 0)].bg, Color::Rgb(0x00, 0x00, 0x00), "col {x}");
    }
}

#[test]
fn zero_size_area_draws_nothing() {
    let bar = pane_fixture(false);
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
    paint_bar(&bar, &Theme::default(), None, area, &mut buf);
    assert_eq!(row_text(&buf), "");
}

/// A non-default palette recolors the bar: the pending breadcrumb takes the
/// theme's accent pair and a group's key block sits on the custom ramp.
#[test]
fn a_custom_theme_recolors_the_bar() {
    let pending = seq(&[ctrl('p')]);
    let bar = pane_fixture(false);
    let theme = Theme {
        ramp_start: (0xff, 0x00, 0x00),
        ramp_end: (0x00, 0x00, 0xff),
        accent: Color::Rgb(0x00, 0xff, 0x00),
        on_accent: Color::Rgb(0x01, 0x02, 0x03),
        ..Theme::default()
    };
    let buf = draw_themed_pending(&bar, &theme, &pending, 80);
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

#[test]
fn the_overflow_marker_never_paints_left_of_the_area() {
    // The revert marker fills the bar and pulls the right edge down to the
    // area's own left edge, so the overflow marker has nowhere inside to go.
    let keymap = KeymapHints {
        reverted: true,
        ..pane_fixture(false)
    };
    let mut buf = Buffer::empty(RatatuiRect {
        x: 0,
        y: 0,
        width: 20,
        height: 1,
    });
    buf.set_string(0, 0, "X".repeat(20), Style::default());

    paint_bar(
        &keymap,
        &Theme::default(),
        None,
        RatatuiRect {
            x: 5,
            y: 0,
            width: 6,
            height: 1,
        },
        &mut buf,
    );

    let outside: String = (0..5).map(|x| buf[(x, 0)].symbol().to_string()).collect();
    assert_eq!(outside, "XXXXX", "columns 0 to 4 belong to the caller");
}

#[test]
fn a_hint_wider_than_the_cell_counter_is_dropped_behind_the_overflow_marker() {
    // A ribbon of 65 536 cells reads as 65 535, which never fits, instead of
    // wrapping to 0 and painting nothing where the marker belongs.
    let long = "L".repeat(65_536 - 5);
    let keymap = hints(
        vec![
            binding(seq(&[plain('a')]), &long, false, false),
            binding(seq(&[plain('b')]), "Second", false, false),
        ],
        &[],
        Vec::new(),
        false,
    );

    assert_eq!(row_text(&draw(&keymap, 40)), "…");
}
