//! Tests for the paint driver and the terminal output: the driver writes the
//! same bytes as a ratatui `Terminal` with a full-screen viewport, a frame
//! that lists its changed rows writes the same bytes as one that draws every
//! row, a frame draws every row after cleared cells, an unknown screen, a
//! resize or a row list out of order or past the screen, held bytes leave at a
//! flush or in writes of 64 KiB, a resize and a failed frame clear the
//! screen, and a failed write drops the held bytes. The terminal output
//! backend writes the bytes of a `CrosstermBackend` for seeded cells.

use super::*;

use std::num::NonZeroU16;

use ratatui::backend::TestBackend;
use ratatui::buffer::{CellDiffOption, CellWidth};
use ratatui::style::Style;
use ratatui::Terminal;

use koshi_test_support::output_workloads::SeededSequence;

/// A terminal sink that keeps every byte, the byte count of every write, and
/// how many flushes it took.
#[derive(Clone, Default)]
struct RecordingSink {
    recorded_bytes: Rc<RefCell<Vec<u8>>>,
    write_byte_counts: Rc<RefCell<Vec<usize>>>,
    is_next_write_failing: Rc<std::cell::Cell<bool>>,
    flush_count: Rc<std::cell::Cell<usize>>,
}

impl RecordingSink {
    fn take_recorded_bytes(&self) -> Vec<u8> {
        std::mem::take(&mut *self.recorded_bytes.borrow_mut())
    }

    fn take_write_byte_counts(&self) -> Vec<usize> {
        std::mem::take(&mut *self.write_byte_counts.borrow_mut())
    }
}

impl Write for RecordingSink {
    fn write(&mut self, output_bytes: &[u8]) -> io::Result<usize> {
        if self.is_next_write_failing.replace(false) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the test terminal is gone",
            ));
        }
        self.recorded_bytes
            .borrow_mut()
            .extend_from_slice(output_bytes);
        self.write_byte_counts.borrow_mut().push(output_bytes.len());
        Ok(output_bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_count.set(self.flush_count.get() + 1);
        Ok(())
    }
}

/// A `CrosstermBackend` over a [`RecordingSink`] that answers `size` with
/// `terminal_size` and reads no terminal. A ratatui `Terminal` over it writes
/// the bytes ratatui's own `draw` gives into the sink, with no terminal
/// attached. `is_draw_failing` makes the next `draw` fail with nothing written.
struct SizedCrosstermBackend {
    crossterm_backend: CrosstermBackend<RecordingSink>,
    terminal_size: RatatuiSize,
    is_draw_failing: bool,
}

impl SizedCrosstermBackend {
    fn from_sink(recording_sink: RecordingSink, terminal_size: Size) -> Self {
        Self {
            crossterm_backend: CrosstermBackend::new(recording_sink),
            terminal_size: RatatuiSize {
                width: terminal_size.column_count,
                height: terminal_size.row_count,
            },
            is_draw_failing: false,
        }
    }
}

impl Backend for SizedCrosstermBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, cell_updates: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        if std::mem::take(&mut self.is_draw_failing) {
            return Err(io::Error::other("the test backend rejected the draw"));
        }
        self.crossterm_backend.draw(cell_updates)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.crossterm_backend.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.crossterm_backend.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        panic!("painting must not ask the terminal for its cursor position");
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.crossterm_backend.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.crossterm_backend.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.crossterm_backend.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<RatatuiSize> {
        Ok(self.terminal_size)
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        panic!("painting must not ask the terminal for its window size");
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.crossterm_backend)
    }
}

/// A source of frames that differ from each other in a few cells, from one
/// seed: the same seed gives the same frames.
struct SeededFrameSource {
    seeded_sequence: SeededSequence,
    frame_cells: Buffer,
}

impl SeededFrameSource {
    fn from_seed(seed: u64, terminal_size: Size) -> Self {
        Self {
            seeded_sequence: SeededSequence::from_seed(seed),
            frame_cells: Buffer::empty(Rect::new(
                0,
                0,
                terminal_size.column_count,
                terminal_size.row_count,
            )),
        }
    }

    /// The next frame's size: every 25th frame takes one of four sizes, the
    /// narrower and the wider ones included.
    fn pick_terminal_size(&mut self, frame_index: usize) -> Size {
        let current_area = self.frame_cells.area;
        if frame_index % 25 != 24 {
            return Size {
                column_count: current_area.width,
                row_count: current_area.height,
            };
        }
        [
            Size {
                column_count: 80,
                row_count: 24,
            },
            Size {
                column_count: 60,
                row_count: 20,
            },
            Size {
                column_count: 100,
                row_count: 30,
            },
            Size {
                column_count: 79,
                row_count: 31,
            },
        ][self.seeded_sequence.compute_next_index(4)]
    }

    /// Change up to 120 cells of the frame at `terminal_size` and return the
    /// cells and the cursor of the new frame. A 2-column symbol never starts
    /// in a row's last column. `U+17D8` takes a forced width of 1 column.
    fn build_next_frame(&mut self, terminal_size: Size) -> (Buffer, Option<Position>) {
        let frame_area = Rect::new(0, 0, terminal_size.column_count, terminal_size.row_count);
        if frame_area != self.frame_cells.area {
            let mut resized_frame_cells = Buffer::empty(frame_area);
            let kept_area = frame_area.intersection(self.frame_cells.area);
            for row_index in kept_area.top()..kept_area.bottom() {
                for column_index in kept_area.left()..kept_area.right() {
                    resized_frame_cells[(column_index, row_index)] =
                        self.frame_cells[(column_index, row_index)].clone();
                }
                let last_cell = &mut resized_frame_cells[(frame_area.width - 1, row_index)];
                if last_cell.cell_width() > 1 {
                    last_cell.set_symbol("a");
                }
            }
            self.frame_cells = resized_frame_cells;
        }
        let symbols = [
            "a",
            "Z",
            " ",
            "漢",
            "é",
            "─",
            "😀",
            "\u{2764}\u{FE0F}",
            "e\u{0301}",
            "\u{0E01}\u{0E34}",
            "\u{17D8}",
        ];
        let colors = [
            Color::Reset,
            Color::Red,
            Color::Indexed(42),
            Color::Rgb(1, 2, 3),
        ];
        let modifiers = [
            Modifier::empty(),
            Modifier::BOLD,
            Modifier::ITALIC | Modifier::UNDERLINED,
            Modifier::REVERSED,
        ];
        let changed_cell_count = self.seeded_sequence.compute_next_index(121);
        for _ in 0..changed_cell_count {
            let column_index = u16::try_from(
                self.seeded_sequence
                    .compute_next_index(usize::from(frame_area.width)),
            )
            .expect("a column");
            let row_index = u16::try_from(
                self.seeded_sequence
                    .compute_next_index(usize::from(frame_area.height)),
            )
            .expect("a row");
            let picked_symbol = symbols[self.seeded_sequence.compute_next_index(symbols.len())];
            let is_wide_symbol = matches!(picked_symbol, "漢" | "😀" | "\u{2764}\u{FE0F}");
            let symbol = if is_wide_symbol && column_index + 1 == frame_area.width {
                "a"
            } else {
                picked_symbol
            };
            let cell_style = Style::default()
                .fg(colors[self.seeded_sequence.compute_next_index(colors.len())])
                .bg(colors[self.seeded_sequence.compute_next_index(colors.len())])
                .add_modifier(modifiers[self.seeded_sequence.compute_next_index(modifiers.len())]);
            let diff_option = if symbol == "\u{17D8}" {
                CellDiffOption::ForcedWidth(NonZeroU16::MIN)
            } else {
                CellDiffOption::None
            };
            self.frame_cells[(column_index, row_index)]
                .set_symbol(symbol)
                .set_style(cell_style)
                .set_diff_option(diff_option);
        }
        let cursor_position = (self.seeded_sequence.compute_next_index(3) != 0).then(|| Position {
            x: u16::try_from(
                self.seeded_sequence
                    .compute_next_index(usize::from(frame_area.width)),
            )
            .expect("a column"),
            y: u16::try_from(
                self.seeded_sequence
                    .compute_next_index(usize::from(frame_area.height)),
            )
            .expect("a row"),
        });
        (self.frame_cells.clone(), cursor_position)
    }
}

/// Copy `frame_cells` into `frame_buffer`, which has the same area.
fn copy_frame_cells(frame_cells: &Buffer, frame_buffer: &mut Buffer) {
    frame_buffer.content.clone_from(&frame_cells.content);
}

fn build_terminal_size(column_count: u16, row_count: u16) -> Size {
    Size {
        column_count,
        row_count,
    }
}

/// A paint driver over a `CrosstermBackend` that writes into one terminal
/// output in front of `recording_sink`, and that output's own handle.
fn build_buffered_paint_driver(
    recording_sink: &RecordingSink,
    terminal_size: Size,
) -> (
    PaintDriver<CrosstermBackend<TerminalOutput>>,
    TerminalOutput,
) {
    let frame_output = TerminalOutput::from_terminal_sink(Box::new(recording_sink.clone()));
    let paint_driver = PaintDriver::from_backend(
        CrosstermBackend::new(frame_output.build_held_flush_output()),
        terminal_size,
    );
    (paint_driver, frame_output)
}

#[test]
fn a_thousand_seeded_frames_write_the_same_bytes_as_a_ratatui_terminal() {
    let first_terminal_size = build_terminal_size(80, 24);
    let reference_sink = RecordingSink::default();
    let mut reference_terminal = Terminal::new(SizedCrosstermBackend::from_sink(
        reference_sink.clone(),
        first_terminal_size,
    ))
    .expect("a sized backend gives the terminal its size");
    let driver_sink = RecordingSink::default();
    let mut paint_driver = PaintDriver::from_backend(
        CrosstermBackend::new(driver_sink.clone()),
        first_terminal_size,
    );
    let mut frame_source = SeededFrameSource::from_seed(0x5eed_f00d_cafe_d00d, first_terminal_size);

    for frame_index in 0..1_000 {
        let terminal_size = frame_source.pick_terminal_size(frame_index);
        let (frame_cells, cursor_position) = frame_source.build_next_frame(terminal_size);
        reference_terminal.backend_mut().terminal_size = RatatuiSize {
            width: terminal_size.column_count,
            height: terminal_size.row_count,
        };
        reference_terminal
            .draw(|render_frame| {
                copy_frame_cells(&frame_cells, render_frame.buffer_mut());
                if let Some(cursor_position) = cursor_position {
                    render_frame.set_cursor_position(cursor_position);
                }
            })
            .expect("the reference frame draws");
        paint_driver
            .draw_frame(terminal_size, None, |frame_buffer, _drawn_rows| {
                copy_frame_cells(&frame_cells, frame_buffer);
                cursor_position
            })
            .expect("the driver frame draws");

        let reference_frame_bytes = reference_sink.take_recorded_bytes();
        let driver_frame_bytes = driver_sink.take_recorded_bytes();
        assert!(
            !driver_frame_bytes.is_empty(),
            "frame {frame_index} wrote no byte"
        );
        assert_eq!(
            String::from_utf8_lossy(&driver_frame_bytes),
            String::from_utf8_lossy(&reference_frame_bytes),
            "frame {frame_index}"
        );
    }
}

#[test]
fn a_resize_clears_the_screen_once_wider_and_twice_narrower_like_a_ratatui_terminal() {
    let reference_sink = RecordingSink::default();
    let mut reference_terminal = Terminal::new(SizedCrosstermBackend::from_sink(
        reference_sink.clone(),
        build_terminal_size(80, 24),
    ))
    .expect("a sized backend gives the terminal its size");
    let driver_sink = RecordingSink::default();
    let mut paint_driver = PaintDriver::from_backend(
        CrosstermBackend::new(driver_sink.clone()),
        build_terminal_size(80, 24),
    );

    for (terminal_size, expected_clear_bytes) in [
        (build_terminal_size(80, 24), &b""[..]),
        (build_terminal_size(60, 20), &b"\x1b[2J\x1b[2J"[..]),
        (build_terminal_size(100, 30), &b"\x1b[2J"[..]),
        (build_terminal_size(100, 12), &b"\x1b[2J"[..]),
    ] {
        reference_terminal.backend_mut().terminal_size = RatatuiSize {
            width: terminal_size.column_count,
            height: terminal_size.row_count,
        };
        reference_terminal
            .draw(|render_frame| {
                render_frame.buffer_mut()[(1, 1)].set_symbol("k");
            })
            .expect("the reference frame draws");
        paint_driver
            .draw_frame(terminal_size, None, |frame_buffer, _drawn_rows| {
                frame_buffer[(1, 1)].set_symbol("k");
                None
            })
            .expect("the driver frame draws");

        let driver_frame_bytes = driver_sink.take_recorded_bytes();
        assert_eq!(
            driver_frame_bytes,
            reference_sink.take_recorded_bytes(),
            "{terminal_size:?}"
        );
        assert!(
            driver_frame_bytes.starts_with(expected_clear_bytes)
                && !driver_frame_bytes[expected_clear_bytes.len()..].starts_with(b"\x1b[2J"),
            "{terminal_size:?}: {}",
            String::from_utf8_lossy(&driver_frame_bytes)
        );
    }
}

#[test]
fn a_frame_of_about_7_600_bytes_leaves_in_one_write() {
    let terminal_size = build_terminal_size(150, 48);
    let recording_sink = RecordingSink::default();
    let (mut paint_driver, mut frame_output) =
        build_buffered_paint_driver(&recording_sink, terminal_size);
    let reference_sink = RecordingSink::default();
    let mut reference_paint_driver =
        PaintDriver::from_backend(CrosstermBackend::new(reference_sink.clone()), terminal_size);
    let render_text_frame = |frame_buffer: &mut Buffer, _drawn_rows: DrawnRows<'_>| {
        for frame_cell in &mut frame_buffer.content {
            frame_cell.set_symbol("x");
        }
        Some(Position { x: 4, y: 2 })
    };

    paint_driver
        .draw_frame(terminal_size, None, render_text_frame)
        .expect("the frame draws");
    frame_output
        .write_all(b"\x1b]0;koshi\x07")
        .expect("the title holds");
    assert_eq!(recording_sink.take_write_byte_counts(), Vec::<usize>::new());
    frame_output.flush().expect("the frame writes out");
    reference_paint_driver
        .draw_frame(terminal_size, None, render_text_frame)
        .expect("the reference frame draws");

    let mut expected_frame_bytes = reference_sink.take_recorded_bytes();
    expected_frame_bytes.extend_from_slice(b"\x1b]0;koshi\x07");
    assert!(
        (7_500..7_700).contains(&expected_frame_bytes.len()),
        "{}",
        expected_frame_bytes.len()
    );
    assert_eq!(
        recording_sink.take_write_byte_counts(),
        vec![expected_frame_bytes.len()]
    );
    assert_eq!(recording_sink.take_recorded_bytes(), expected_frame_bytes);
}

#[test]
fn held_bytes_leave_in_writes_of_64_kib_and_the_rest_at_the_flush() {
    for (output_byte_count, expected_write_byte_counts) in [
        (65_535, vec![65_535]),
        (65_536, vec![65_536]),
        (65_537, vec![65_536, 1]),
        (204_800, vec![65_536, 65_536, 65_536, 8_192]),
    ] {
        let recording_sink = RecordingSink::default();
        let mut frame_output = TerminalOutput::from_terminal_sink(Box::new(recording_sink.clone()));
        let mut held_flush_output = frame_output.build_held_flush_output();
        let output_bytes = (0..output_byte_count)
            .map(|byte_index| u8::try_from(byte_index % 251).expect("a byte"))
            .collect::<Vec<u8>>();

        for (piece_index, output_piece) in output_bytes.chunks(3_000).enumerate() {
            if piece_index % 2 == 0 {
                frame_output
                    .write_all(output_piece)
                    .expect("the bytes hold");
            } else {
                held_flush_output
                    .write_all(output_piece)
                    .expect("the bytes hold");
                held_flush_output
                    .flush()
                    .expect("a held flush writes nothing");
            }
        }
        frame_output.flush().expect("the bytes write out");

        assert_eq!(
            recording_sink.take_write_byte_counts(),
            expected_write_byte_counts,
            "{output_byte_count} bytes"
        );
        assert_eq!(
            recording_sink.take_recorded_bytes(),
            output_bytes,
            "{output_byte_count} bytes"
        );
    }
}

#[test]
fn exactly_64_kib_of_held_bytes_leave_before_any_flush() {
    let recording_sink = RecordingSink::default();
    let mut frame_output = TerminalOutput::from_terminal_sink(Box::new(recording_sink.clone()));

    frame_output
        .write_all(&[b'a'; 65_535])
        .expect("the bytes hold");
    let write_byte_counts_below_the_slice = recording_sink.take_write_byte_counts();
    frame_output.write_all(b"b").expect("the bytes hold");

    assert_eq!(write_byte_counts_below_the_slice, Vec::<usize>::new());
    assert_eq!(recording_sink.take_write_byte_counts(), vec![65_536]);
    assert_eq!(recording_sink.flush_count.get(), 0);
}

#[test]
fn a_flush_writes_the_held_bytes_and_flushes_the_terminal_and_a_held_flush_does_neither() {
    let recording_sink = RecordingSink::default();
    let mut frame_output = TerminalOutput::from_terminal_sink(Box::new(recording_sink.clone()));
    let mut held_flush_output = frame_output.build_held_flush_output();

    held_flush_output
        .write_all(b"cell")
        .expect("the bytes hold");
    held_flush_output
        .flush()
        .expect("a held flush writes nothing");
    let flush_count_after_the_held_flush = recording_sink.flush_count.get();
    let write_byte_counts_after_the_held_flush = recording_sink.take_write_byte_counts();
    frame_output.flush().expect("the bytes write out");

    assert_eq!(flush_count_after_the_held_flush, 0);
    assert_eq!(write_byte_counts_after_the_held_flush, Vec::<usize>::new());
    assert_eq!(recording_sink.take_write_byte_counts(), vec![4]);
    assert_eq!(recording_sink.take_recorded_bytes(), b"cell");
    assert_eq!(recording_sink.flush_count.get(), 1);
}

#[test]
fn a_failed_write_drops_the_held_bytes_and_the_next_flush_writes_only_new_bytes() {
    let recording_sink = RecordingSink::default();
    let mut frame_output = TerminalOutput::from_terminal_sink(Box::new(recording_sink.clone()));
    frame_output
        .write_all(b"lost frame")
        .expect("the bytes hold");
    recording_sink.is_next_write_failing.set(true);

    let flush_error = frame_output
        .flush()
        .expect_err("the failed write is returned");
    frame_output.write_all(b"next").expect("the bytes hold");
    frame_output.flush().expect("the bytes write out");

    assert_eq!(flush_error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(recording_sink.take_recorded_bytes(), b"next");
    assert_eq!(recording_sink.take_write_byte_counts(), vec![4]);
}

#[test]
fn a_failed_slice_write_drops_the_held_bytes_and_the_rest_of_the_write() {
    let recording_sink = RecordingSink::default();
    let mut frame_output = TerminalOutput::from_terminal_sink(Box::new(recording_sink.clone()));
    frame_output
        .write_all(&[b'a'; 60_000])
        .expect("the bytes hold");
    recording_sink.is_next_write_failing.set(true);

    let write_error = frame_output
        .write_all(&[b'b'; 80_000])
        .expect_err("the failed slice write is returned");
    frame_output.flush().expect("nothing is left to write");

    assert_eq!(write_error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(recording_sink.take_recorded_bytes(), b"");
    assert_eq!(recording_sink.take_write_byte_counts(), Vec::<usize>::new());
}

#[test]
fn after_a_failed_frame_the_next_frame_clears_the_screen_and_draws_every_cell() {
    let terminal_size = build_terminal_size(20, 4);
    let render_first_frame = |frame_buffer: &mut Buffer, _drawn_rows: DrawnRows<'_>| {
        frame_buffer[(0, 0)].set_symbol("o");
        frame_buffer[(5, 1)].set_symbol("l");
        None
    };
    let render_second_frame = |frame_buffer: &mut Buffer, _drawn_rows: DrawnRows<'_>| {
        frame_buffer[(0, 0)].set_symbol("o");
        frame_buffer[(7, 2)].set_symbol("n");
        Some(Position { x: 3, y: 3 })
    };
    let driver_sink = RecordingSink::default();
    let mut paint_driver = PaintDriver::from_backend(
        SizedCrosstermBackend::from_sink(driver_sink.clone(), terminal_size),
        terminal_size,
    );
    paint_driver
        .draw_frame(terminal_size, None, render_first_frame)
        .expect("the first frame draws");
    driver_sink.take_recorded_bytes();
    paint_driver.backend.is_draw_failing = true;
    paint_driver
        .draw_frame(terminal_size, None, render_second_frame)
        .expect_err("the failed draw is returned");
    let failed_frame_bytes = driver_sink.take_recorded_bytes();

    paint_driver
        .draw_frame(terminal_size, None, render_second_frame)
        .expect("the retried frame draws");

    let fresh_sink = RecordingSink::default();
    let mut fresh_paint_driver =
        PaintDriver::from_backend(CrosstermBackend::new(fresh_sink.clone()), terminal_size);
    fresh_paint_driver
        .draw_frame(terminal_size, None, render_second_frame)
        .expect("the fresh frame draws");
    let mut expected_frame_bytes = b"\x1b[2J".to_vec();
    expected_frame_bytes.extend_from_slice(&fresh_sink.take_recorded_bytes());
    assert_eq!(failed_frame_bytes, b"");
    assert_eq!(
        String::from_utf8_lossy(&driver_sink.take_recorded_bytes()),
        String::from_utf8_lossy(&expected_frame_bytes)
    );

    paint_driver
        .draw_frame(terminal_size, None, render_second_frame)
        .expect("the unchanged frame draws");
    let unchanged_frame_sink = RecordingSink::default();
    let mut unchanged_frame_backend = CrosstermBackend::new(unchanged_frame_sink.clone());
    unchanged_frame_backend
        .draw(std::iter::empty())
        .expect("an empty draw writes");
    unchanged_frame_backend
        .show_cursor()
        .expect("the cursor shows");
    unchanged_frame_backend
        .set_cursor_position(Position { x: 3, y: 3 })
        .expect("the cursor moves");
    let unchanged_frame_bytes = unchanged_frame_sink.take_recorded_bytes();
    assert!(unchanged_frame_bytes.ends_with(b"\x1b[?25h\x1b[4;4H"));
    assert_eq!(driver_sink.take_recorded_bytes(), unchanged_frame_bytes);
}

#[test]
fn a_frame_erases_the_screen_exactly_when_the_driver_says_it_needs_an_erase() {
    let driver_sink = RecordingSink::default();
    let mut paint_driver = PaintDriver::from_backend(
        CrosstermBackend::new(driver_sink.clone()),
        build_terminal_size(80, 24),
    );
    let render_frame = |frame_buffer: &mut Buffer, _drawn_rows: DrawnRows<'_>| {
        frame_buffer[(1, 1)].set_symbol("k");
        None
    };

    for (step_name, terminal_size, expected_erase_bytes) in [
        ("the built size", build_terminal_size(80, 24), &b""[..]),
        ("the same size again", build_terminal_size(80, 24), &b""[..]),
        (
            "a wider size",
            build_terminal_size(100, 30),
            &b"\x1b[2J"[..],
        ),
        (
            "a narrower size",
            build_terminal_size(60, 20),
            &b"\x1b[2J\x1b[2J"[..],
        ),
        (
            "an unknown screen",
            build_terminal_size(60, 20),
            &b"\x1b[2J"[..],
        ),
        ("cleared cells", build_terminal_size(60, 20), &b""[..]),
    ] {
        match step_name {
            "an unknown screen" => paint_driver.mark_shown_screen_unknown(),
            "cleared cells" => paint_driver.clear_shown_cells(),
            _ => {}
        }
        let needs_screen_erase = paint_driver.needs_screen_erase(terminal_size);

        paint_driver
            .draw_frame(terminal_size, None, render_frame)
            .expect("the frame draws");

        let frame_bytes = driver_sink.take_recorded_bytes();
        assert_eq!(
            needs_screen_erase,
            !expected_erase_bytes.is_empty(),
            "{step_name}"
        );
        assert!(
            frame_bytes.starts_with(expected_erase_bytes)
                && !frame_bytes[expected_erase_bytes.len()..].starts_with(b"\x1b[2J"),
            "{step_name}: {}",
            String::from_utf8_lossy(&frame_bytes)
        );
        assert!(
            !paint_driver.needs_screen_erase(terminal_size),
            "{step_name}: the drawn frame still needs an erase"
        );
    }
}

#[test]
fn a_frame_after_a_marked_unknown_screen_clears_it_and_after_cleared_cells_does_not() {
    let terminal_size = build_terminal_size(10, 2);
    let mut paint_driver = PaintDriver::from_backend(TestBackend::new(10, 2), terminal_size);
    let mut stray_cell = Cell::default();
    stray_cell.set_symbol("?");
    let render_frame = |frame_buffer: &mut Buffer, _drawn_rows: DrawnRows<'_>| {
        frame_buffer[(2, 1)].set_symbol("q");
        None
    };
    paint_driver
        .draw_frame(terminal_size, None, render_frame)
        .expect("the first frame draws");
    Backend::draw(
        &mut paint_driver.backend,
        std::iter::once((4, 0, &stray_cell)),
    )
    .expect("the stray cell draws");

    paint_driver.mark_shown_screen_unknown();
    paint_driver
        .draw_frame(terminal_size, None, render_frame)
        .expect("the frame after the unknown screen draws");
    let mut expected_buffer = Buffer::empty(Rect::new(0, 0, 10, 2));
    expected_buffer[(2, 1)].set_symbol("q");
    assert_eq!(paint_driver.backend.buffer(), &expected_buffer);

    Backend::clear(&mut paint_driver.backend).expect("the screen erases");
    Backend::draw(
        &mut paint_driver.backend,
        std::iter::once((4, 0, &stray_cell)),
    )
    .expect("the stray cell draws");
    paint_driver.clear_shown_cells();
    paint_driver
        .draw_frame(terminal_size, None, render_frame)
        .expect("the frame after the cleared cells draws");
    expected_buffer[(4, 0)].set_symbol("?");
    assert_eq!(paint_driver.backend.buffer(), &expected_buffer);
}

/// Whether row `row_index` of `previous_frame_cells` and `next_frame_cells`,
/// two buffers of one area, holds different cells.
fn is_frame_row_changed(
    previous_frame_cells: &Buffer,
    next_frame_cells: &Buffer,
    row_index: u16,
) -> bool {
    let row_cells = compute_row_cell_range(
        usize::from(row_index),
        usize::from(next_frame_cells.area.width),
    );
    previous_frame_cells.content[row_cells.clone()] != next_frame_cells.content[row_cells]
}

#[test]
fn seeded_frames_that_list_their_changed_rows_write_the_same_bytes_as_frames_that_draw_every_row() {
    let first_terminal_size = build_terminal_size(80, 24);
    let every_row_sink = RecordingSink::default();
    let mut every_row_paint_driver = PaintDriver::from_backend(
        CrosstermBackend::new(every_row_sink.clone()),
        first_terminal_size,
    );
    let listed_row_sink = RecordingSink::default();
    let mut listed_row_paint_driver = PaintDriver::from_backend(
        CrosstermBackend::new(listed_row_sink.clone()),
        first_terminal_size,
    );
    let mut frame_source = SeededFrameSource::from_seed(0x5eed_0012_cafe_d00d, first_terminal_size);
    let mut previous_frame_cells = Buffer::empty(Rect::ZERO);
    let mut listed_frame_count = 0;
    let mut expected_listed_frame_count = 0;

    for frame_index in 0..1_000 {
        let terminal_size = frame_source.pick_terminal_size(frame_index);
        let (frame_cells, cursor_position) = frame_source.build_next_frame(terminal_size);
        let changed_rows: Vec<u16> = if frame_cells.area == previous_frame_cells.area {
            expected_listed_frame_count += 1;
            (0..frame_cells.area.height)
                .filter(|row_index| {
                    is_frame_row_changed(&previous_frame_cells, &frame_cells, *row_index)
                        || frame_source.seeded_sequence.compute_next_index(8) == 0
                })
                .collect()
        } else {
            vec![0]
        };
        every_row_paint_driver
            .draw_frame(terminal_size, None, |frame_buffer, _drawn_rows| {
                copy_frame_cells(&frame_cells, frame_buffer);
                cursor_position
            })
            .expect("the frame that draws every row draws");
        listed_row_paint_driver
            .draw_frame(
                terminal_size,
                Some(&changed_rows),
                |frame_buffer, drawn_rows| {
                    match drawn_rows {
                        DrawnRows::Every => copy_frame_cells(&frame_cells, frame_buffer),
                        DrawnRows::Listed(listed_rows) => {
                            listed_frame_count += 1;
                            let row_width = usize::from(frame_cells.area.width);
                            for listed_row in listed_rows {
                                let row_cells =
                                    compute_row_cell_range(usize::from(*listed_row), row_width);
                                frame_buffer.content[row_cells.clone()]
                                    .clone_from_slice(&frame_cells.content[row_cells]);
                            }
                        }
                    }
                    cursor_position
                },
            )
            .expect("the frame that lists its rows draws");

        assert_eq!(
            String::from_utf8_lossy(&listed_row_sink.take_recorded_bytes()),
            String::from_utf8_lossy(&every_row_sink.take_recorded_bytes()),
            "frame {frame_index}, listed rows {changed_rows:?}"
        );
        previous_frame_cells = frame_cells;
    }
    assert_eq!(listed_frame_count, expected_listed_frame_count);
}

#[test]
fn a_frame_draws_every_row_unless_the_shown_frame_and_a_sorted_row_list_inside_it_allow_listed_rows(
) {
    let terminal_size = build_terminal_size(10, 3);
    let driver_sink = RecordingSink::default();
    let mut paint_driver = PaintDriver::from_backend(
        SizedCrosstermBackend::from_sink(driver_sink.clone(), terminal_size),
        terminal_size,
    );
    let draw_listed_frame = |paint_driver: &mut PaintDriver<SizedCrosstermBackend>,
                             terminal_size: Size,
                             changed_rows: &[u16]| {
        let mut drawn_listed_rows = None;
        let draw_result = paint_driver.draw_frame(
            terminal_size,
            Some(changed_rows),
            |frame_buffer, drawn_rows| {
                frame_buffer[(1, 1)].set_symbol("k");
                if let DrawnRows::Listed(listed_rows) = drawn_rows {
                    drawn_listed_rows = Some(listed_rows.to_vec());
                }
                None
            },
        );
        (draw_result.is_ok(), drawn_listed_rows)
    };

    assert_eq!(
        draw_listed_frame(&mut paint_driver, terminal_size, &[1]),
        (true, None),
        "the first frame, over cleared cells"
    );
    assert_eq!(
        draw_listed_frame(&mut paint_driver, terminal_size, &[1]),
        (true, Some(vec![1])),
        "a frame after a drawn frame"
    );
    assert_eq!(
        draw_listed_frame(&mut paint_driver, terminal_size, &[]),
        (true, Some(Vec::new())),
        "an empty row list"
    );
    assert_eq!(
        draw_listed_frame(&mut paint_driver, terminal_size, &[0, 2]),
        (true, Some(vec![0, 2])),
        "rows up to the last row"
    );
    assert_eq!(
        draw_listed_frame(&mut paint_driver, terminal_size, &[2, 1]),
        (true, None),
        "rows out of order"
    );
    assert_eq!(
        draw_listed_frame(&mut paint_driver, terminal_size, &[1, 1]),
        (true, None),
        "a repeated row"
    );
    assert_eq!(
        draw_listed_frame(&mut paint_driver, terminal_size, &[3]),
        (true, None),
        "a row past the screen"
    );
    paint_driver.clear_shown_cells();
    assert_eq!(
        draw_listed_frame(&mut paint_driver, terminal_size, &[1]),
        (true, None),
        "a frame after cleared cells"
    );
    paint_driver.mark_shown_screen_unknown();
    assert_eq!(
        draw_listed_frame(&mut paint_driver, terminal_size, &[1]),
        (true, None),
        "a frame after an unknown screen"
    );
    let resized_terminal_size = build_terminal_size(10, 4);
    assert_eq!(
        draw_listed_frame(&mut paint_driver, resized_terminal_size, &[1]),
        (true, None),
        "a resized frame"
    );
    paint_driver.backend.is_draw_failing = true;
    assert_eq!(
        draw_listed_frame(&mut paint_driver, resized_terminal_size, &[1]),
        (false, Some(vec![1])),
        "a failed listed frame"
    );
    driver_sink.take_recorded_bytes();
    assert_eq!(
        draw_listed_frame(&mut paint_driver, resized_terminal_size, &[1]),
        (true, None),
        "a frame after a failed listed frame"
    );
    let fresh_sink = RecordingSink::default();
    let mut fresh_paint_driver = PaintDriver::from_backend(
        SizedCrosstermBackend::from_sink(fresh_sink.clone(), resized_terminal_size),
        resized_terminal_size,
    );
    draw_listed_frame(&mut fresh_paint_driver, resized_terminal_size, &[1]);
    let mut expected_frame_bytes = b"\x1b[2J".to_vec();
    expected_frame_bytes.extend_from_slice(&fresh_sink.take_recorded_bytes());
    assert_eq!(
        String::from_utf8_lossy(&driver_sink.take_recorded_bytes()),
        String::from_utf8_lossy(&expected_frame_bytes)
    );
}

/// Seeded cell updates over an `80x24` screen: runs of neighboring cells
/// and jumps, every modifier set from 12 bits, the reset, named, indexed and
/// truecolor foregrounds, backgrounds and underline colors, and symbols
/// from ASCII to `中`, `U+2764` with VS16 and `e` with `U+0301`.
fn build_seeded_cell_updates(frame_source: &mut SeededFrameSource) -> Vec<(u16, u16, Cell)> {
    let symbols = ["a", " ", "中", "\u{2764}\u{FE0F}", "e\u{0301}", "─"];
    let colors = [
        Color::Reset,
        Color::Red,
        Color::LightCyan,
        Color::Indexed(42),
        Color::Rgb(1, 2, 3),
    ];
    let mut cell_updates = Vec::new();
    let mut column_index = 0;
    let mut row_index = 0;
    for _ in 0..frame_source.seeded_sequence.compute_next_index(200) {
        if frame_source.seeded_sequence.compute_next_index(4) == 0 {
            column_index = u16::try_from(frame_source.seeded_sequence.compute_next_index(80))
                .expect("a column");
            row_index =
                u16::try_from(frame_source.seeded_sequence.compute_next_index(24)).expect("a row");
        } else {
            column_index = (column_index + 1) % 80;
        }
        let mut cell_update = Cell::default();
        cell_update.set_symbol(
            symbols[frame_source
                .seeded_sequence
                .compute_next_index(symbols.len())],
        );
        cell_update.fg = colors[frame_source
            .seeded_sequence
            .compute_next_index(colors.len())];
        cell_update.bg = colors[frame_source
            .seeded_sequence
            .compute_next_index(colors.len())];
        cell_update.underline_color = colors[frame_source
            .seeded_sequence
            .compute_next_index(colors.len())];
        cell_update.modifier = Modifier::from_bits_truncate(
            u16::try_from(frame_source.seeded_sequence.compute_next_index(1 << 12))
                .expect("12 bits"),
        );
        cell_updates.push((column_index, row_index, cell_update));
    }
    cell_updates
}

#[test]
fn the_terminal_output_backend_writes_the_bytes_of_a_crossterm_backend() {
    let mut frame_source =
        SeededFrameSource::from_seed(0x5eed_0015_cafe_d00d, build_terminal_size(80, 24));
    let recording_sink = RecordingSink::default();
    let mut frame_output = TerminalOutput::from_terminal_sink(Box::new(recording_sink.clone()));
    let mut terminal_output_backend = TerminalOutputBackend::from_terminal_output(&frame_output);
    let reference_sink = RecordingSink::default();
    let mut crossterm_backend = CrosstermBackend::new(reference_sink.clone());

    for draw_index in 0..500 {
        let cell_updates = build_seeded_cell_updates(&mut frame_source);
        let draw_cell_updates = || {
            cell_updates
                .iter()
                .map(|(column_index, row_index, cell_update)| {
                    (*column_index, *row_index, cell_update)
                })
        };
        Backend::draw(&mut terminal_output_backend, draw_cell_updates()).expect("the cells draw");
        terminal_output_backend
            .hide_cursor()
            .expect("the cursor hides");
        terminal_output_backend
            .set_cursor_position(Position { x: 2, y: 1 })
            .expect("the cursor moves");
        Backend::flush(&mut terminal_output_backend).expect("the backend flushes");
        frame_output.flush().expect("the frame writes out");
        Backend::draw(&mut crossterm_backend, draw_cell_updates()).expect("the cells draw");
        crossterm_backend.hide_cursor().expect("the cursor hides");
        crossterm_backend
            .set_cursor_position(Position { x: 2, y: 1 })
            .expect("the cursor moves");
        Backend::flush(&mut crossterm_backend).expect("the backend flushes");

        assert_eq!(
            String::from_utf8_lossy(&recording_sink.take_recorded_bytes()),
            String::from_utf8_lossy(&reference_sink.take_recorded_bytes()),
            "draw {draw_index}"
        );
    }
}

#[test]
fn a_bold_cell_writes_its_move_attribute_symbol_and_the_resets() {
    let recording_sink = RecordingSink::default();
    let mut frame_output = TerminalOutput::from_terminal_sink(Box::new(recording_sink.clone()));
    let mut terminal_output_backend = TerminalOutputBackend::from_terminal_output(&frame_output);
    let mut bold_cell = Cell::default();
    bold_cell.set_symbol("x").modifier = Modifier::BOLD;

    Backend::draw(
        &mut terminal_output_backend,
        std::iter::once((3, 0, &bold_cell)),
    )
    .expect("the cell draws");
    frame_output.flush().expect("the frame writes out");

    assert_eq!(
        recording_sink.take_recorded_bytes(),
        b"\x1b[1;4H\x1b[1mx\x1b[39m\x1b[49m\x1b[59m\x1b[0m"
    );
}
