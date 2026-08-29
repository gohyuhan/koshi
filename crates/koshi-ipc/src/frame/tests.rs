//! Tests for the painted frame's wire form: run-length encoding folds and
//! expands a row without changing a cell, a count past `u16::MAX` splits into
//! further runs, and a frame's encoding is pinned field for field so a rename
//! fails here.

use koshi_core::geometry::Point;
use serde_json::json;
use uuid::Uuid;

use super::*;

/// A plain cell style with `fg` as its foreground: no underline color, no
/// attributes set.
fn style(fg: FrameColor) -> FrameStyle {
    FrameStyle {
        fg,
        bg: FrameColor::Default,
        underline_color: None,
        attrs: FrameAttrs {
            bold: false,
            italic: false,
            reverse: false,
            faint: false,
            blink: false,
            conceal: false,
            strike: false,
            overline: false,
            underline: FrameUnderline::None,
        },
    }
}

/// A one-column cell holding `ch` in `fg`.
fn cell(ch: char, fg: FrameColor) -> FrameCell {
    FrameCell {
        ch,
        combining: Vec::new(),
        width: 1,
        style: style(fg),
    }
}

/// A one-column blank cell in the default colors.
fn blank() -> FrameCell {
    cell(' ', FrameColor::Default)
}

/// A one-pane frame at fixed ids, so its encoding is byte-stable. The pane
/// reports button-event mouse tracking, which travels as
/// [`MouseTracking::ButtonMotion`].
fn frame() -> PaintedFrame {
    let tab = TabId::from_uuid(Uuid::from_u128(2));
    let pane = PaneId::from_uuid(Uuid::from_u128(4));

    PaintedFrame {
        session: FrameSession {
            id: SessionId::from_uuid(Uuid::from_u128(1)),
            name: "quiet-lake".to_string(),
            active_tab: FrameTab {
                id: tab,
                name: "edit".to_string(),
                slots: vec![FrameSlot {
                    pane_id: pane,
                    rect: Rect {
                        origin: Point { x: 0, y: 0 },
                        size: Size { cols: 4, rows: 3 },
                    },
                    inner_rect: Some(Rect {
                        origin: Point { x: 1, y: 1 },
                        size: Size { cols: 2, rows: 1 },
                    }),
                    kind: PaneKind::Terminal,
                    visible: true,
                    suppressed: false,
                    dead: false,
                }],
                effective_size: Size { cols: 4, rows: 3 },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                all_suppressed: false,
                gap: 0,
            },
            tabs: vec![FrameTabMeta {
                id: tab,
                name: "edit".to_string(),
                index: 0,
                active: true,
            }],
        },
        panes: vec![FramePane {
            id: pane,
            title: Some("vim".to_string()),
            cursor: FrameCursor {
                row: 0,
                col: 1,
                visible: true,
                blink: false,
                shape: Some(FrameCursorShape::Bar),
            },
            window: Some(FrameWindow {
                cols: 2,
                rows: vec![FrameRow::from_cells(
                    [
                        cell('h', FrameColor::Default),
                        cell('i', FrameColor::Default),
                    ],
                    FrameRowEnd::Hard,
                )],
                view_offset: 0,
            }),
            reverse_video: false,
            mouse_tracking: MouseTracking::ButtonMotion,
            alt_scroll: false,
            on_alt_screen: false,
            view_top_row: 7,
            selection: Some(FrameSelection {
                rows: vec![(0, 0, 1)],
            }),
            has_selection: true,
            scrollback: FrameScrollback {
                truncated: false,
                retained_lines: 12,
            },
        }],
        client: FrameClient {
            id: ClientId::from_uuid(Uuid::from_u128(3)),
            viewport: Size { cols: 4, rows: 3 },
            active_tab: tab,
            focused_pane: Some(pane),
            lock_mode: LockMode::Normal,
            mouse_select: false,
        },
    }
}

#[test]
fn an_eighty_column_blank_row_travels_as_one_run() {
    let cells: Vec<FrameCell> = std::iter::repeat_n(blank(), 80).collect();

    let row = FrameRow::from_cells(cells.iter().cloned(), FrameRowEnd::Hard);

    assert_eq!(
        row.runs,
        vec![FrameRun {
            count: 80,
            cell: blank()
        }]
    );
    assert_eq!(row.cells(), cells);
}

#[test]
fn stretches_of_two_styles_fold_into_one_run_each() {
    let red = cell('x', FrameColor::Indexed(1));
    let blue = cell('x', FrameColor::Rgb(0, 0, 255));
    let cells = vec![
        red.clone(),
        red.clone(),
        blue.clone(),
        blue.clone(),
        red.clone(),
        red.clone(),
    ];

    let row = FrameRow::from_cells(cells.iter().cloned(), FrameRowEnd::Hard);

    assert_eq!(
        row.runs,
        vec![
            FrameRun {
                count: 2,
                cell: red.clone()
            },
            FrameRun {
                count: 2,
                cell: blue
            },
            FrameRun {
                count: 2,
                cell: red
            },
        ]
    );
    assert_eq!(row.cells(), cells);
}

#[test]
fn a_run_longer_than_a_count_can_hold_splits_at_the_cap() {
    let cells: Vec<FrameCell> = std::iter::repeat_n(blank(), 70_000).collect();

    let row = FrameRow::from_cells(cells.iter().cloned(), FrameRowEnd::Hard);

    assert_eq!(
        row.runs,
        vec![
            FrameRun {
                count: u16::MAX,
                cell: blank()
            },
            FrameRun {
                count: 4_465,
                cell: blank()
            },
        ]
    );
    assert_eq!(row.cells(), cells);
}

#[test]
fn an_empty_row_folds_to_no_runs_and_expands_to_no_cells() {
    let row = FrameRow::from_cells([], FrameRowEnd::Hard);

    assert_eq!(row.runs, Vec::new());
    assert_eq!(row.cells(), Vec::new());
}

#[test]
fn a_frame_survives_a_round_trip_field_for_field() {
    let sent = frame();

    let encoded = serde_json::to_string(&sent).expect("encodes");
    let received: PaintedFrame = serde_json::from_str(&encoded).expect("decodes");

    assert_eq!(received, sent);
    assert_eq!(
        received.panes[0].mouse_tracking,
        MouseTracking::ButtonMotion
    );
}

#[test]
fn a_frame_encodes_to_the_shape_a_client_decodes() {
    // A client and a server only agree on a frame if both were built at this
    // shape. Add, remove or rename anything below and every client older than
    // the change stops decoding the frames it is sent.
    //
    // A default cell carries no `combining`, no `underline_color` and no set
    // attribute, so those names are absent from the encoding below.
    let plain_cell = json!({
        "ch": "h",
        "width": 1,
        "style": {
            "fg": "Default",
            "bg": "Default",
            "attrs": { "underline": "None" }
        }
    });
    let mut second_cell = plain_cell.clone();
    second_cell["ch"] = json!("i");

    assert_eq!(
        serde_json::to_value(frame()).expect("frame encodes"),
        json!({
            "session": {
                "id": "00000000-0000-0000-0000-000000000001",
                "name": "quiet-lake",
                "active_tab": {
                    "id": "00000000-0000-0000-0000-000000000002",
                    "name": "edit",
                    "slots": [{
                        "pane_id": "00000000-0000-0000-0000-000000000004",
                        "rect": {
                            "origin": { "x": 0, "y": 0 },
                            "size": { "cols": 4, "rows": 3 }
                        },
                        "inner_rect": {
                            "origin": { "x": 1, "y": 1 },
                            "size": { "cols": 2, "rows": 1 }
                        },
                        "kind": "Terminal",
                        "visible": true,
                        "suppressed": false,
                        "dead": false
                    }],
                    "effective_size": { "cols": 4, "rows": 3 },
                    "stack_headers": [],
                    "layout_mode": "Tiled",
                    "all_suppressed": false,
                    "gap": 0
                },
                "tabs": [{
                    "id": "00000000-0000-0000-0000-000000000002",
                    "name": "edit",
                    "index": 0,
                    "active": true
                }]
            },
            "panes": [{
                "id": "00000000-0000-0000-0000-000000000004",
                "title": "vim",
                "cursor": {
                    "row": 0,
                    "col": 1,
                    "visible": true,
                    "blink": false,
                    "shape": "Bar"
                },
                "window": {
                    "cols": 2,
                    "rows": [{
                        "runs": [
                            { "count": 1, "cell": plain_cell },
                            { "count": 1, "cell": second_cell }
                        ]
                    }],
                    "view_offset": 0
                },
                "reverse_video": false,
                "mouse_tracking": "ButtonMotion",
                "alt_scroll": false,
                "on_alt_screen": false,
                "view_top_row": 7,
                "selection": { "rows": [[0, 0, 1]] },
                "has_selection": true,
                "scrollback": { "truncated": false, "retained_lines": 12 }
            }],
            "client": {
                "id": "00000000-0000-0000-0000-000000000003",
                "viewport": { "cols": 4, "rows": 3 },
                "active_tab": "00000000-0000-0000-0000-000000000002",
                "focused_pane": "00000000-0000-0000-0000-000000000004",
                "lock_mode": "Normal",
                "mouse_select": false
            }
        })
    );
}

#[test]
fn a_frame_carrying_an_unknown_field_ignores_it() {
    let mut encoded = serde_json::to_value(frame()).expect("frame encodes");
    encoded["panes"][0]
        .as_object_mut()
        .expect("a pane encodes as an object")
        .insert("zoomed".to_string(), serde_json::Value::Bool(true));

    // Decoded from text, the way the transport does it: the frame arrives as
    // bytes on a socket, never as an already-built value.
    let decoded: PaintedFrame = serde_json::from_str(&encoded.to_string())
        .expect("a field this build does not know is ignored");

    assert_eq!(
        decoded,
        frame(),
        "the extra field left nothing behind in the decoded frame"
    );
}

/// A frame from a server that sends no `gap` reads as `0`, and a frame that
/// sends one reads back the value it was written with.
#[test]
fn a_frame_without_a_gap_reads_as_zero() {
    let mut encoded = serde_json::to_value(frame()).expect("frame encodes");
    encoded["session"]["active_tab"]
        .as_object_mut()
        .expect("a tab encodes as an object")
        .remove("gap")
        .expect("the tab encodes a gap");

    // Decoded from text, the way the transport does it.
    let decoded: PaintedFrame =
        serde_json::from_str(&encoded.to_string()).expect("a frame with no gap decodes");
    assert_eq!(decoded.session.active_tab.gap, 0);

    let mut spaced = frame();
    spaced.session.active_tab.gap = 2;
    let encoded = serde_json::to_value(&spaced).expect("frame encodes");
    let decoded: PaintedFrame =
        serde_json::from_str(&encoded.to_string()).expect("a frame with a gap decodes");
    assert_eq!(decoded.session.active_tab.gap, 2);
}

/// A value enum this build has no name for falls back to its plainest value,
/// so one unfamiliar colour or underline never costs the whole frame.
#[test]
fn a_cell_value_this_build_has_no_name_for_falls_back() {
    let mut encoded = serde_json::to_value(frame()).expect("frame encodes");
    let style = encoded["panes"][0]["window"]["rows"][0]["runs"][0]["cell"]["style"]
        .as_object_mut()
        .expect("a style encodes as an object");
    style.insert("fg".to_string(), serde_json::json!("Neon"));
    style["attrs"]
        .as_object_mut()
        .expect("attributes encode as an object")
        .insert("underline".to_string(), serde_json::json!("Dotted2"));

    // Decoded from text, the way the transport does it.
    let decoded: PaintedFrame = serde_json::from_str(&encoded.to_string())
        .expect("an unfamiliar value falls back, it does not fail");

    let cell = &decoded.panes[0]
        .window
        .as_ref()
        .expect("the pane has a window")
        .rows[0]
        .runs[0]
        .cell;
    assert_eq!(
        cell.style.fg,
        FrameColor::Default,
        "a colour with no name here draws as the default colour"
    );
    assert_eq!(
        cell.style.attrs.underline,
        FrameUnderline::None,
        "an underline style with no name here draws as no underline"
    );
}

/// A row that soft-wrapped must arrive soft-wrapped. A viewer that reads a
/// soft wrap as a hard one breaks the logical line when its text is copied
/// out, and the wire form leaves the default off, so only the two wrapped
/// endings travel at all.
#[test]
fn a_wrapped_row_carries_its_ending_and_an_ended_row_leaves_it_off() {
    let encoded =
        |end| serde_json::to_value(FrameRow::from_cells([blank()], end)).expect("a row encodes");

    assert_eq!(encoded(FrameRowEnd::Soft)["end"], json!("Soft"));
    assert_eq!(encoded(FrameRowEnd::SoftWide)["end"], json!("SoftWide"));
    assert_eq!(encoded(FrameRowEnd::Hard).get("end"), None);
}

#[test]
fn a_row_reads_back_with_the_ending_it_was_written_with() {
    for end in [FrameRowEnd::Hard, FrameRowEnd::Soft, FrameRowEnd::SoftWide] {
        let text =
            serde_json::to_string(&FrameRow::from_cells([blank()], end)).expect("a row encodes");

        let read: FrameRow = serde_json::from_str(&text).expect("a row decodes");

        assert_eq!(read.end, end);
        assert_eq!(read.cells(), vec![blank()]);
    }
}

#[test]
fn a_row_ending_this_build_has_no_name_for_reads_as_hard() {
    let read: FrameRow = serde_json::from_str(r#"{"runs":[],"end":"SoftDouble"}"#)
        .expect("an ending with no name here falls back, it does not fail");

    assert_eq!(read.end, FrameRowEnd::Hard);
}

/// The two optional presentation values fall back the same way the colours
/// do: to nothing at all, leaving the user's own cursor and the foreground
/// colour standing.
#[test]
fn a_cursor_shape_and_an_underline_colour_with_no_name_here_read_as_none() {
    let mut encoded = serde_json::to_value(frame()).expect("frame encodes");
    encoded["panes"][0]["cursor"]["shape"] = json!("Beam");
    encoded["panes"][0]["window"]["rows"][0]["runs"][0]["cell"]["style"]["underline_color"] =
        json!("Neon");

    // Decoded from text, the way the transport does it.
    let decoded: PaintedFrame = serde_json::from_str(&encoded.to_string())
        .expect("a value with no name here falls back, it does not fail");

    assert_eq!(decoded.panes[0].cursor.shape, None);
    assert_eq!(
        decoded.panes[0]
            .window
            .as_ref()
            .expect("the pane has a window")
            .rows[0]
            .runs[0]
            .cell
            .style
            .underline_color,
        None
    );
}

/// A `gap` that is not a cell count — negative, or a string — reads as `0`
/// and leaves the rest of the frame intact.
#[test]
fn a_frame_whose_gap_is_not_a_count_reads_as_zero() {
    for hostile in [serde_json::json!(-1), serde_json::json!("2")] {
        let mut encoded = serde_json::to_value(frame()).expect("frame encodes");
        encoded["session"]["active_tab"]["gap"] = hostile;
        let decoded: PaintedFrame =
            serde_json::from_str(&encoded.to_string()).expect("a frame with a bad gap decodes");
        assert_eq!(decoded, frame());
    }
}
