//! The client's paint driver and the output buffer every frame passes through.
//!
//! [`PaintDriver`] keeps the cells the terminal shows and writes the cells of
//! each new frame that differ from them. A frame whose caller lists the only
//! rows that can differ draws and compares those rows alone, and writes the
//! same bytes as a frame that draws every row. [`TerminalOutputBackend`]
//! writes each frame's cells and cursor into a [`TerminalOutput`]. It holds
//! the bytes and writes them to the terminal at its flush, or each time
//! [`TERMINAL_OUTPUT_SLICE_BYTE_COUNT`] bytes are held. Example: a frame of
//! 150 000 bytes leaves in three writes of 65 536, 65 536 and 18 928 bytes.

use std::cell::RefCell;
use std::io::{self, Write};
use std::ops::Range;
use std::rc::Rc;

use ratatui::backend::{Backend, ClearType, CrosstermBackend, IntoCrossterm, WindowSize};
use ratatui::buffer::{Buffer, Cell};
use ratatui::crossterm::cursor::MoveTo;
use ratatui::crossterm::queue;
use ratatui::crossterm::style::{
    Color as CrosstermColor, Colors, Print, SetBackgroundColor, SetColors, SetForegroundColor,
    SetUnderlineColor,
};
use ratatui::layout::{Position, Rect, Size as RatatuiSize};
use ratatui::style::{Color, Modifier};

use koshi_core::geometry::Size;
use koshi_renderer::DrawnRows;

/// The most bytes one write to the terminal carries, and the most bytes a
/// [`TerminalOutput`] holds: 64 KiB.
pub(crate) const TERMINAL_OUTPUT_SLICE_BYTE_COUNT: usize = 65_536;

/// The bytes on their way to the terminal: `held_bytes`, then `terminal_sink`.
struct HeldTerminalOutput {
    /// The bytes not yet written to `terminal_sink`. Never more than
    /// [`TERMINAL_OUTPUT_SLICE_BYTE_COUNT`] bytes.
    held_bytes: Vec<u8>,
    /// The terminal the bytes go to: standard output in a running client.
    terminal_sink: Box<dyn Write>,
}

impl HeldTerminalOutput {
    /// Append `output_bytes` to the held bytes. Each time the held bytes reach
    /// [`TERMINAL_OUTPUT_SLICE_BYTE_COUNT`], they go to `terminal_sink` in one
    /// write, and the held bytes start again empty.
    ///
    /// # Errors
    /// The write error of `terminal_sink`. The held bytes and the rest of
    /// `output_bytes` are dropped.
    fn hold_output_bytes(&mut self, mut output_bytes: &[u8]) -> io::Result<()> {
        while self.held_bytes.len() + output_bytes.len() >= TERMINAL_OUTPUT_SLICE_BYTE_COUNT {
            let (slice_fill_bytes, remaining_output_bytes) =
                output_bytes.split_at(TERMINAL_OUTPUT_SLICE_BYTE_COUNT - self.held_bytes.len());
            let write_result = if self.held_bytes.is_empty() {
                self.terminal_sink.write_all(slice_fill_bytes)
            } else {
                self.held_bytes.extend_from_slice(slice_fill_bytes);
                self.terminal_sink.write_all(&self.held_bytes)
            };
            self.held_bytes.clear();
            write_result?;
            output_bytes = remaining_output_bytes;
        }
        self.held_bytes.extend_from_slice(output_bytes);
        Ok(())
    }

    /// Write every held byte to `terminal_sink` in one write, then flush
    /// `terminal_sink`. With no held byte, only the flush runs.
    ///
    /// # Errors
    /// The write or flush error of `terminal_sink`. The held bytes are dropped
    /// either way.
    fn write_out_held_bytes(&mut self) -> io::Result<()> {
        let write_result = self.terminal_sink.write_all(&self.held_bytes);
        self.held_bytes.clear();
        write_result?;
        self.terminal_sink.flush()
    }
}

/// One handle to the bytes this client writes to its terminal.
///
/// A write holds its bytes, and [`TERMINAL_OUTPUT_SLICE_BYTE_COUNT`] held
/// bytes go to the terminal at once. A flush writes every held byte to the
/// terminal. On a handle from [`TerminalOutput::build_held_flush_output`], a
/// flush writes nothing.
pub(crate) struct TerminalOutput {
    /// The held bytes and the terminal, shared by every handle built from one
    /// [`TerminalOutput::from_terminal_sink`].
    held_terminal_output: Rc<RefCell<HeldTerminalOutput>>,
    /// Whether a flush through this handle writes nothing.
    is_flush_held: bool,
}

impl TerminalOutput {
    /// A handle to an empty output in front of `terminal_sink`. It reserves
    /// [`TERMINAL_OUTPUT_SLICE_BYTE_COUNT`] bytes, and its flush writes every
    /// held byte to `terminal_sink`.
    pub(crate) fn from_terminal_sink(terminal_sink: Box<dyn Write>) -> Self {
        Self {
            held_terminal_output: Rc::new(RefCell::new(HeldTerminalOutput {
                held_bytes: Vec::with_capacity(TERMINAL_OUTPUT_SLICE_BYTE_COUNT),
                terminal_sink,
            })),
            is_flush_held: false,
        }
    }

    /// A second handle to the same held bytes, whose flush writes nothing. A
    /// [`TerminalOutputBackend`] over it writes a frame's cells and cursor
    /// commands into the held bytes, and they leave at this output's own
    /// flush.
    pub(crate) fn build_held_flush_output(&self) -> Self {
        Self {
            held_terminal_output: Rc::clone(&self.held_terminal_output),
            is_flush_held: true,
        }
    }
}

impl Write for TerminalOutput {
    fn write(&mut self, output_bytes: &[u8]) -> io::Result<usize> {
        self.held_terminal_output
            .borrow_mut()
            .hold_output_bytes(output_bytes)?;
        Ok(output_bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.is_flush_held {
            return Ok(());
        }
        self.held_terminal_output
            .borrow_mut()
            .write_out_held_bytes()
    }
}

/// The backend a running client paints through. Its cells go into one handle
/// of a [`TerminalOutput`], and its cursor, clear and flush commands go
/// through a `CrosstermBackend` over a second handle to the same held bytes.
/// Every byte reaches the held bytes in call order. Neither handle's flush
/// writes anything.
///
/// [`Backend::draw`] writes the bytes `CrosstermBackend::draw` writes, and
/// allocates nothing. Example: one cell `x` in bold at `(3, 0)` writes
/// `ESC [ 1 ; 4 H`, `ESC [ 1 m`, `x`, then `ESC [ 39 m`, `ESC [ 49 m`,
/// `ESC [ 59 m` and `ESC [ 0 m`.
pub(crate) struct TerminalOutputBackend {
    /// The backend of every command other than the drawn cells.
    crossterm_backend: CrosstermBackend<TerminalOutput>,
    /// The handle the drawn cells are written into.
    cell_output: TerminalOutput,
}

impl TerminalOutputBackend {
    /// A backend whose two handles write into the held bytes of
    /// `terminal_output`.
    pub(crate) fn from_terminal_output(terminal_output: &TerminalOutput) -> Self {
        Self {
            crossterm_backend: CrosstermBackend::new(terminal_output.build_held_flush_output()),
            cell_output: terminal_output.build_held_flush_output(),
        }
    }
}

impl Backend for TerminalOutputBackend {
    type Error = io::Error;

    /// Write each of `cell_updates`, in order: `MoveTo` its cell unless it
    /// follows the last written cell on the same row, the attribute changes
    /// from the last written cell ([`write_modifier_change`]), `SetColors`
    /// when its foreground or background differs, `SetUnderlineColor` when its
    /// underline color differs, then its symbol. The first cell is compared
    /// with no attribute and reset colors. The foreground, background and
    /// underline colors and the attributes are reset after the last cell.
    fn draw<'a, I>(&mut self, cell_updates: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut foreground_color = Color::Reset;
        let mut background_color = Color::Reset;
        let mut underline_color = Color::Reset;
        let mut modifier = Modifier::empty();
        let mut last_position: Option<Position> = None;
        for (column_index, row_index, cell) in cell_updates {
            let is_after_last_cell = last_position.is_some_and(|last_position| {
                last_position.y == row_index && last_position.x.checked_add(1) == Some(column_index)
            });
            if !is_after_last_cell {
                queue!(self.cell_output, MoveTo(column_index, row_index))?;
            }
            last_position = Some(Position::new(column_index, row_index));
            if cell.modifier != modifier {
                write_modifier_change(&mut self.cell_output, modifier, cell.modifier)?;
                modifier = cell.modifier;
            }
            if cell.fg != foreground_color || cell.bg != background_color {
                queue!(
                    self.cell_output,
                    SetColors(Colors::new(
                        cell.fg.into_crossterm(),
                        cell.bg.into_crossterm()
                    ))
                )?;
                foreground_color = cell.fg;
                background_color = cell.bg;
            }
            if cell.underline_color != underline_color {
                queue!(
                    self.cell_output,
                    SetUnderlineColor(cell.underline_color.into_crossterm())
                )?;
                underline_color = cell.underline_color;
            }
            queue!(self.cell_output, Print(cell.symbol()))?;
        }
        queue!(
            self.cell_output,
            SetForegroundColor(CrosstermColor::Reset),
            SetBackgroundColor(CrosstermColor::Reset),
            SetUnderlineColor(CrosstermColor::Reset),
        )?;
        self.cell_output.write_all(b"\x1b[0m")
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.crossterm_backend.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.crossterm_backend.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.crossterm_backend.get_cursor_position()
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
        self.crossterm_backend.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.crossterm_backend.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.crossterm_backend)
    }
}

/// Write the SGR sequences that change the text attributes from
/// `previous_modifier` to `next_modifier`, in the order `CrosstermBackend`
/// writes them. First, for each removed attribute: `ESC [ 27 m` (reverse);
/// `ESC [ 22 m` (bold or dim), then `ESC [ 2 m` when `next_modifier` holds
/// dim and `ESC [ 1 m` when it holds bold; `ESC [ 23 m` (italic);
/// `ESC [ 24 m` (underline); `ESC [ 29 m` (crossed out); `ESC [ 28 m`
/// (hidden); and `ESC [ 25 m` (slow or rapid blink). Then, for each added
/// attribute: `ESC [ 7 m` (reverse); `ESC [ 1 m` (bold); `ESC [ 3 m`
/// (italic); `ESC [ 4 m` (underline); `ESC [ 2 m` (dim); `ESC [ 9 m`
/// (crossed out); `ESC [ 8 m` (hidden); `ESC [ 5 m` (slow blink); and
/// `ESC [ 6 m` (rapid blink). An added bold or dim writes nothing in this
/// second list when a removed bold or dim wrote `ESC [ 22 m`.
fn write_modifier_change(
    writer: &mut impl Write,
    previous_modifier: Modifier,
    next_modifier: Modifier,
) -> io::Result<()> {
    let removed_modifier = previous_modifier - next_modifier;
    if removed_modifier.contains(Modifier::REVERSED) {
        writer.write_all(b"\x1b[27m")?;
    }
    let is_intensity_reset =
        removed_modifier.contains(Modifier::BOLD) || removed_modifier.contains(Modifier::DIM);
    if is_intensity_reset {
        writer.write_all(b"\x1b[22m")?;
        if next_modifier.contains(Modifier::DIM) {
            writer.write_all(b"\x1b[2m")?;
        }
        if next_modifier.contains(Modifier::BOLD) {
            writer.write_all(b"\x1b[1m")?;
        }
    }
    if removed_modifier.contains(Modifier::ITALIC) {
        writer.write_all(b"\x1b[23m")?;
    }
    if removed_modifier.contains(Modifier::UNDERLINED) {
        writer.write_all(b"\x1b[24m")?;
    }
    if removed_modifier.contains(Modifier::CROSSED_OUT) {
        writer.write_all(b"\x1b[29m")?;
    }
    if removed_modifier.contains(Modifier::HIDDEN) {
        writer.write_all(b"\x1b[28m")?;
    }
    if removed_modifier.contains(Modifier::SLOW_BLINK)
        || removed_modifier.contains(Modifier::RAPID_BLINK)
    {
        writer.write_all(b"\x1b[25m")?;
    }
    let added_modifier = next_modifier - previous_modifier;
    if added_modifier.contains(Modifier::REVERSED) {
        writer.write_all(b"\x1b[7m")?;
    }
    if added_modifier.contains(Modifier::BOLD) && !is_intensity_reset {
        writer.write_all(b"\x1b[1m")?;
    }
    if added_modifier.contains(Modifier::ITALIC) {
        writer.write_all(b"\x1b[3m")?;
    }
    if added_modifier.contains(Modifier::UNDERLINED) {
        writer.write_all(b"\x1b[4m")?;
    }
    if added_modifier.contains(Modifier::DIM) && !is_intensity_reset {
        writer.write_all(b"\x1b[2m")?;
    }
    if added_modifier.contains(Modifier::CROSSED_OUT) {
        writer.write_all(b"\x1b[9m")?;
    }
    if added_modifier.contains(Modifier::HIDDEN) {
        writer.write_all(b"\x1b[8m")?;
    }
    if added_modifier.contains(Modifier::SLOW_BLINK) {
        writer.write_all(b"\x1b[5m")?;
    }
    if added_modifier.contains(Modifier::RAPID_BLINK) {
        writer.write_all(b"\x1b[6m")?;
    }
    Ok(())
}

/// Return the indices of the cells of row `row_index` in a buffer whose rows
/// are `row_width` cells wide. Example: row `2` of 80-cell rows is `160..240`.
fn compute_row_cell_range(row_index: usize, row_width: usize) -> Range<usize> {
    row_index * row_width..(row_index + 1) * row_width
}

/// What the terminal shows, as far as a [`PaintDriver`] knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShownScreen {
    /// The cells of the last frame the driver drew.
    DrawnFrame,
    /// Blank cells, after the caller erased the screen or before the first
    /// frame.
    Cleared,
    /// Cells the driver cannot name: the output of a frame failed after part
    /// of it could have reached the terminal.
    Unknown,
}

/// The client's paint driver: the cells its terminal shows, and the backend
/// that writes the cells of each new frame that differ from them.
///
/// [`PaintDriver::draw_frame`] makes the same backend calls, in the same
/// order, as `ratatui::Terminal::draw` with a full-screen viewport. It reads
/// no terminal size: the caller passes the size from the last resize event.
pub(crate) struct PaintDriver<B: Backend> {
    /// The backend each frame's cells and cursor go through.
    pub(crate) backend: B,
    /// The cells the terminal shows after the last drawn frame.
    shown_buffer: Buffer,
    /// The buffer each frame renders into. Between frames its cells hold
    /// nothing a frame reads.
    next_buffer: Buffer,
    /// The listed rows of `shown_buffer`, one under another, for a frame that
    /// draws listed rows.
    shown_rows_buffer: Buffer,
    /// The listed rows of `next_buffer`, one under another, for a frame that
    /// draws listed rows.
    drawn_rows_buffer: Buffer,
    /// What the terminal shows.
    shown_screen: ShownScreen,
}

impl<B: Backend> PaintDriver<B> {
    /// A driver for a blank terminal of `terminal_size`.
    pub(crate) fn from_backend(backend: B, terminal_size: Size) -> Self {
        let terminal_area = Rect::new(0, 0, terminal_size.column_count, terminal_size.row_count);
        Self {
            backend,
            shown_buffer: Buffer::empty(terminal_area),
            next_buffer: Buffer::empty(terminal_area),
            shown_rows_buffer: Buffer::empty(Rect::ZERO),
            drawn_rows_buffer: Buffer::empty(Rect::ZERO),
            shown_screen: ShownScreen::Cleared,
        }
    }

    /// Draw one frame on a terminal of `terminal_size`.
    ///
    /// `render_frame` renders the frame and returns the cursor position, or
    /// `None` to hide the cursor. `changed_rows` lists, in ascending order
    /// with none repeated, the only rows in which the frame can differ from
    /// the last frame this driver drew, or is `None` when any row can differ.
    ///
    /// The frame draws only the listed rows when the last frame this driver
    /// drew is what the terminal shows, `terminal_size` is that frame's size,
    /// and every listed row lies inside it. `render_frame` then gets the
    /// frame buffer with its cells as they are and [`DrawnRows::Listed`], and
    /// draws each listed row whole. The backend gets:
    ///
    /// 1. One [`Backend::draw`] with every cell of a listed row that differs
    ///    from the shown cell, as a whole-buffer diff gives them.
    /// 2. `hide_cursor`, or `show_cursor` then `set_cursor_position`.
    /// 3. `flush`.
    ///
    /// Any other frame draws every row. `render_frame` gets a blank buffer of
    /// `terminal_size` and [`DrawnRows::Every`], and the backend gets:
    ///
    /// 1. When `terminal_size` differs from the last frame's size, a clear of
    ///    the whole screen, two when the terminal is narrower. When the shown
    ///    screen is unknown ([`Self::mark_shown_screen_unknown`]), one clear.
    ///    After a clear, every cell that is not blank is drawn.
    /// 2. Every cell that differs from the shown cells ([`Backend::draw`]).
    /// 3. `hide_cursor`, or `show_cursor` then `set_cursor_position`.
    /// 4. `flush`.
    ///
    /// Both write the same bytes when every row outside `changed_rows` equals
    /// the shown row, and no cell of the shown or the new frame is wider than
    /// the columns from it to the end of its row. Example: a frame that changes
    /// only cell `(3, 0)`, to `x`, and puts the cursor at `(4, 0)` draws the
    /// one cell `x` at column 3 of row 0, then `show_cursor` and
    /// `set_cursor_position` at `(4, 0)`. A `CrosstermBackend` writes those two
    /// as `ESC [ ? 25 h` and `ESC [ 1 ; 5 H`.
    ///
    /// # Errors
    /// The backend's error. The shown screen is then unknown, and the next
    /// frame clears it.
    pub(crate) fn draw_frame(
        &mut self,
        terminal_size: Size,
        changed_rows: Option<&[u16]>,
        render_frame: impl FnOnce(&mut Buffer, DrawnRows<'_>) -> Option<Position>,
    ) -> Result<(), B::Error> {
        let frame_area = Rect::new(0, 0, terminal_size.column_count, terminal_size.row_count);
        let listed_rows = changed_rows.filter(|changed_rows| {
            self.shown_screen == ShownScreen::DrawnFrame
                && frame_area == self.shown_buffer.area
                && changed_rows
                    .windows(2)
                    .all(|row_pair| row_pair[0] < row_pair[1])
                && changed_rows
                    .last()
                    .is_none_or(|last_row| *last_row < frame_area.height)
        });
        if let Some(listed_rows) = listed_rows {
            let cursor_position =
                render_frame(&mut self.next_buffer, DrawnRows::Listed(listed_rows));
            if let Err(backend_error) = self.write_listed_rows(listed_rows, cursor_position) {
                self.shown_screen = ShownScreen::Unknown;
                return Err(backend_error);
            }
            let row_width = usize::from(frame_area.width);
            for listed_row in listed_rows {
                let row_cells = compute_row_cell_range(usize::from(*listed_row), row_width);
                self.shown_buffer.content[row_cells.clone()]
                    .clone_from_slice(&self.next_buffer.content[row_cells]);
            }
            return Ok(());
        }
        self.next_buffer.resize(frame_area);
        self.next_buffer.reset();
        let cursor_position = render_frame(&mut self.next_buffer, DrawnRows::Every);
        if let Err(backend_error) = self.write_frame(frame_area, cursor_position) {
            self.shown_screen = ShownScreen::Unknown;
            return Err(backend_error);
        }
        std::mem::swap(&mut self.shown_buffer, &mut self.next_buffer);
        self.shown_screen = ShownScreen::DrawnFrame;
        Ok(())
    }

    /// Write the clears, the changed cells of `self.next_buffer`, the cursor
    /// at `cursor_position` and the flush that [`Self::draw_frame`] lists for
    /// a frame that draws every row.
    fn write_frame(
        &mut self,
        frame_area: Rect,
        cursor_position: Option<Position>,
    ) -> Result<(), B::Error> {
        if frame_area != self.shown_buffer.area {
            if frame_area.width < self.shown_buffer.area.width {
                self.backend.clear_region(ClearType::All)?;
            }
            self.backend.clear_region(ClearType::All)?;
            self.shown_buffer.resize(frame_area);
            self.shown_buffer.reset();
        } else if self.shown_screen == ShownScreen::Unknown {
            self.backend.clear_region(ClearType::All)?;
            self.shown_buffer.reset();
        }
        self.backend
            .draw(self.shown_buffer.diff_iter(&self.next_buffer))?;
        self.write_cursor_and_flush(cursor_position)
    }

    /// Write the changed cells of the rows `listed_rows`, the cursor at
    /// `cursor_position` and the flush that [`Self::draw_frame`] lists for a
    /// frame that draws listed rows.
    ///
    /// The listed rows of the shown and the next buffer are stacked into two
    /// buffers one row each, in the order of `listed_rows`, and their diff
    /// goes to one [`Backend::draw`] with each row moved back to its own row.
    fn write_listed_rows(
        &mut self,
        listed_rows: &[u16],
        cursor_position: Option<Position>,
    ) -> Result<(), B::Error> {
        let row_width = usize::from(self.shown_buffer.area.width);
        let stacked_area = Rect::new(
            0,
            0,
            self.shown_buffer.area.width,
            u16::try_from(listed_rows.len()).unwrap_or(u16::MAX),
        );
        self.shown_rows_buffer.resize(stacked_area);
        self.drawn_rows_buffer.resize(stacked_area);
        for (stacked_row_index, listed_row) in listed_rows.iter().enumerate() {
            let row_cells = compute_row_cell_range(usize::from(*listed_row), row_width);
            let stacked_row_cells = compute_row_cell_range(stacked_row_index, row_width);
            self.shown_rows_buffer.content[stacked_row_cells.clone()]
                .clone_from_slice(&self.shown_buffer.content[row_cells.clone()]);
            self.drawn_rows_buffer.content[stacked_row_cells]
                .clone_from_slice(&self.next_buffer.content[row_cells]);
        }
        let changed_cells = self
            .shown_rows_buffer
            .diff_iter(&self.drawn_rows_buffer)
            .map(|(column_index, stacked_row_index, changed_cell)| {
                (
                    column_index,
                    listed_rows[usize::from(stacked_row_index)],
                    changed_cell,
                )
            });
        self.backend.draw(changed_cells)?;
        self.write_cursor_and_flush(cursor_position)
    }

    /// Hide the cursor, or show it and move it to `cursor_position`, then
    /// flush the backend.
    fn write_cursor_and_flush(
        &mut self,
        cursor_position: Option<Position>,
    ) -> Result<(), B::Error> {
        match cursor_position {
            None => self.backend.hide_cursor()?,
            Some(cursor_position) => {
                self.backend.show_cursor()?;
                self.backend.set_cursor_position(cursor_position)?;
            }
        }
        self.backend.flush()
    }

    /// Record that the terminal shows blank cells, after the caller erased
    /// the screen. The next frame draws every cell that is not blank, with no
    /// clear of its own.
    pub(crate) fn clear_shown_cells(&mut self) {
        self.shown_buffer.reset();
        self.shown_screen = ShownScreen::Cleared;
    }

    /// Record that the terminal can show cells other than the shown cells,
    /// after the output of a frame failed. The next frame clears the screen
    /// and draws every cell that is not blank.
    pub(crate) fn mark_shown_screen_unknown(&mut self) {
        self.shown_screen = ShownScreen::Unknown;
    }
}

#[cfg(test)]
mod tests;
