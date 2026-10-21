//! Invented terminal output for the output-path tests.
//!
//! [`build_workload_bytes`](crate::output_workloads::build_workload_bytes)
//! returns the bytes a program would print for one
//! [`OutputWorkload`](crate::output_workloads::OutputWorkload), generated from
//! a seed by a SplitMix64 sequence. The same seed gives the same bytes on
//! every machine and every run.
//!
//! Example — `build_workload_bytes(OutputWorkload::Plain, 4 * 1024 * 1024,
//! DEFAULT_WORKLOAD_SEED)` returns at least 4 MiB of 190-column ASCII lines,
//! each ending in `\r\n`, and stops at the end of the line that reaches 4 MiB.

/// The seed the output-path tests pass to [`build_workload_bytes`].
pub const DEFAULT_WORKLOAD_SEED: u64 = 0x6b6f_7368_6900_0001;

/// The column count of every line in the `plain`, `sgr256` and `truecolor`
/// workloads, and the widest line of the `unicode` workload.
const WORKLOAD_LINE_COLUMN_COUNT: usize = 190;

/// The row count of the screen the `fullscreen` workload repaints.
const FULLSCREEN_ROW_COUNT: usize = 50;

/// The column count of the screen the `fullscreen` workload repaints.
const FULLSCREEN_COLUMN_COUNT: usize = 200;

/// One kind of program output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputWorkload {
    /// Lines of 190 printable ASCII characters, each ending in `\r\n`.
    Plain,
    /// Lines of 190 columns cut into runs of 3 to 12 characters. Each run
    /// starts with a 256-color foreground `ESC [ 38 ; 5 ; n m`, and one run
    /// in four also sets a 256-color background. Each line ends in
    /// `ESC [ 0 m \r \n`.
    Sgr256,
    /// The `sgr256` shape with 24-bit colors: `ESC [ 38 ; 2 ; r ; g ; b m`
    /// and `ESC [ 48 ; 2 ; r ; g ; b m`.
    Truecolor,
    /// Lines of at most 190 columns that mix ASCII, CJK ideographs (two
    /// columns), emoji (two columns) and Latin letters followed by one
    /// combining acute accent, each ending in `\r\n`.
    Unicode,
    /// Full repaints of a 50×200 screen: `ESC [ H`, then each row addressed
    /// with `ESC [ row ; 1 H` and filled with 200 printable ASCII characters
    /// in a 256-color foreground.
    Fullscreen,
    /// One progress line of a 40-cell bar, rewritten after `\r`. Step 420 of
    /// 1000 is this line:
    ///
    /// `building [################........................]  42% (420/1000)`
    Progress,
}

impl OutputWorkload {
    /// Every workload: `plain`, `sgr256`, `truecolor`, `unicode`,
    /// `fullscreen`, then `progress`.
    pub const ALL: [OutputWorkload; 6] = [
        OutputWorkload::Plain,
        OutputWorkload::Sgr256,
        OutputWorkload::Truecolor,
        OutputWorkload::Unicode,
        OutputWorkload::Fullscreen,
        OutputWorkload::Progress,
    ];

    /// The name of the workload: `plain`, `sgr256`, `truecolor`,
    /// `unicode`, `fullscreen` or `progress`.
    #[must_use]
    pub fn get_workload_name(self) -> &'static str {
        match self {
            OutputWorkload::Plain => "plain",
            OutputWorkload::Sgr256 => "sgr256",
            OutputWorkload::Truecolor => "truecolor",
            OutputWorkload::Unicode => "unicode",
            OutputWorkload::Fullscreen => "fullscreen",
            OutputWorkload::Progress => "progress",
        }
    }
}

/// A SplitMix64 sequence: each call returns the next 64-bit value. The same
/// seed gives the same values on every machine.
pub struct SeededSequence {
    sequence_state: u64,
}

impl SeededSequence {
    /// The sequence that starts from `seed`.
    #[must_use]
    pub fn from_seed(seed: u64) -> Self {
        SeededSequence {
            sequence_state: seed,
        }
    }

    /// The next value of the sequence.
    pub fn compute_next_value(&mut self) -> u64 {
        self.sequence_state = self.sequence_state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut mixed_value = self.sequence_state;
        mixed_value = (mixed_value ^ (mixed_value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed_value = (mixed_value ^ (mixed_value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        mixed_value ^ (mixed_value >> 31)
    }

    /// A value in `0..upper_bound_exclusive`.
    ///
    /// # Panics
    /// Panics when `upper_bound_exclusive` is `0`.
    pub fn compute_next_index(&mut self, upper_bound_exclusive: usize) -> usize {
        (self.compute_next_value() % upper_bound_exclusive as u64) as usize
    }

    /// A value in `first_value..=last_value`.
    ///
    /// # Panics
    /// Panics when `last_value` is below `first_value`.
    pub fn compute_next_in_range(&mut self, first_value: usize, last_value: usize) -> usize {
        first_value + self.compute_next_index(last_value - first_value + 1)
    }
}

/// The 64-bit FNV-1a hash of `hashed_bytes`: offset basis
/// `0xcbf2_9ce4_8422_2325`, prime `0x0000_0100_0000_01b3`. An empty slice
/// gives the offset basis.
#[must_use]
pub fn compute_fnv1a_hash(hashed_bytes: &[u8]) -> u64 {
    hashed_bytes
        .iter()
        .fold(0xcbf2_9ce4_8422_2325, |hash, hashed_byte| {
            (hash ^ u64::from(*hashed_byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

/// One printable ASCII byte, `0x20` to `0x7e`, picked by `seeded_sequence`.
fn pick_printable_ascii_byte(seeded_sequence: &mut SeededSequence) -> u8 {
    b' ' + seeded_sequence.compute_next_index(95) as u8
}

/// The output of `workload`, generated from `seed`, cut at the end of the
/// line or repaint that reaches `minimum_byte_count`. The `progress` workload
/// is cut at the end of the update that reaches it.
#[must_use]
pub fn build_workload_bytes(
    workload: OutputWorkload,
    minimum_byte_count: usize,
    seed: u64,
) -> Vec<u8> {
    let mut seeded_sequence = SeededSequence::from_seed(seed);
    let mut workload_bytes = Vec::with_capacity(minimum_byte_count + 16 * 1024);
    let mut update_index = 0usize;
    while workload_bytes.len() < minimum_byte_count {
        match workload {
            OutputWorkload::Plain => append_plain_line(&mut workload_bytes, &mut seeded_sequence),
            OutputWorkload::Sgr256 => append_colored_line(
                &mut workload_bytes,
                &mut seeded_sequence,
                ColorKind::Indexed,
            ),
            OutputWorkload::Truecolor => {
                append_colored_line(&mut workload_bytes, &mut seeded_sequence, ColorKind::Rgb)
            }
            OutputWorkload::Unicode => {
                append_unicode_line(&mut workload_bytes, &mut seeded_sequence)
            }
            OutputWorkload::Fullscreen => {
                append_fullscreen_repaint(&mut workload_bytes, &mut seeded_sequence)
            }
            OutputWorkload::Progress => append_progress_update(&mut workload_bytes, update_index),
        }
        update_index += 1;
    }
    workload_bytes
}

/// Append one `plain` line: 190 printable ASCII bytes, then `\r\n`.
fn append_plain_line(workload_bytes: &mut Vec<u8>, seeded_sequence: &mut SeededSequence) {
    for _ in 0..WORKLOAD_LINE_COLUMN_COUNT {
        workload_bytes.push(pick_printable_ascii_byte(seeded_sequence));
    }
    workload_bytes.extend_from_slice(b"\r\n");
}

/// The color form of a colored line: `Indexed` writes `5;n`, and `Rgb` writes
/// `2;r;g;b`.
#[derive(Clone, Copy)]
enum ColorKind {
    Indexed,
    Rgb,
}

/// Append one color number, `0` to `255` picked by `seeded_sequence`, in
/// decimal.
fn append_color_number(workload_bytes: &mut Vec<u8>, seeded_sequence: &mut SeededSequence) {
    workload_bytes.extend_from_slice(
        seeded_sequence
            .compute_next_index(256)
            .to_string()
            .as_bytes(),
    );
}

/// Append the color parameters that follow `38;` or `48;`: `5;n` for
/// [`ColorKind::Indexed`], and `2;r;g;b` for [`ColorKind::Rgb`].
fn append_color_parameters(
    workload_bytes: &mut Vec<u8>,
    seeded_sequence: &mut SeededSequence,
    color_kind: ColorKind,
) {
    match color_kind {
        ColorKind::Indexed => {
            workload_bytes.extend_from_slice(b"5;");
            append_color_number(workload_bytes, seeded_sequence);
        }
        ColorKind::Rgb => {
            workload_bytes.extend_from_slice(b"2");
            for _ in 0..3 {
                workload_bytes.push(b';');
                append_color_number(workload_bytes, seeded_sequence);
            }
        }
    }
}

/// Append one `sgr256` or `truecolor` line, as [`OutputWorkload::Sgr256`]
/// describes, in the color form of `color_kind`.
fn append_colored_line(
    workload_bytes: &mut Vec<u8>,
    seeded_sequence: &mut SeededSequence,
    color_kind: ColorKind,
) {
    let mut written_column_count = 0usize;
    while written_column_count < WORKLOAD_LINE_COLUMN_COUNT {
        let run_column_count = seeded_sequence
            .compute_next_in_range(3, 12)
            .min(WORKLOAD_LINE_COLUMN_COUNT - written_column_count);
        workload_bytes.extend_from_slice(b"\x1b[38;");
        append_color_parameters(workload_bytes, seeded_sequence, color_kind);
        if seeded_sequence.compute_next_index(4) == 0 {
            workload_bytes.extend_from_slice(b";48;");
            append_color_parameters(workload_bytes, seeded_sequence, color_kind);
        }
        workload_bytes.push(b'm');
        for _ in 0..run_column_count {
            workload_bytes.push(pick_printable_ascii_byte(seeded_sequence));
        }
        written_column_count += run_column_count;
    }
    workload_bytes.extend_from_slice(b"\x1b[0m\r\n");
}

/// Append one `unicode` line, as [`OutputWorkload::Unicode`] describes.
fn append_unicode_line(workload_bytes: &mut Vec<u8>, seeded_sequence: &mut SeededSequence) {
    let mut written_column_count = 0usize;
    let mut encoded_character = [0u8; 4];
    while written_column_count < WORKLOAD_LINE_COLUMN_COUNT - 1 {
        match seeded_sequence.compute_next_index(4) {
            0 => {
                workload_bytes.push(pick_printable_ascii_byte(seeded_sequence));
                written_column_count += 1;
            }
            1 => {
                let ideograph =
                    char::from_u32(0x4e00 + seeded_sequence.compute_next_index(0x5000) as u32)
                        .expect("U+4E00..U+9DFF are scalar values");
                workload_bytes
                    .extend_from_slice(ideograph.encode_utf8(&mut encoded_character).as_bytes());
                written_column_count += 2;
            }
            2 => {
                let emoji =
                    char::from_u32(0x1f600 + seeded_sequence.compute_next_index(0x40) as u32)
                        .expect("U+1F600..U+1F63F are scalar values");
                workload_bytes
                    .extend_from_slice(emoji.encode_utf8(&mut encoded_character).as_bytes());
                written_column_count += 2;
            }
            _ => {
                workload_bytes.push(b'a' + seeded_sequence.compute_next_index(26) as u8);
                workload_bytes.extend_from_slice("\u{0301}".as_bytes());
                written_column_count += 1;
            }
        }
    }
    workload_bytes.extend_from_slice(b"\r\n");
}

/// Append one repaint, as [`OutputWorkload::Fullscreen`] describes, then
/// `ESC [ 0 m`.
fn append_fullscreen_repaint(workload_bytes: &mut Vec<u8>, seeded_sequence: &mut SeededSequence) {
    workload_bytes.extend_from_slice(b"\x1b[H");
    for row_number in 1..=FULLSCREEN_ROW_COUNT {
        workload_bytes.extend_from_slice(format!("\x1b[{row_number};1H\x1b[38;5;").as_bytes());
        append_color_number(workload_bytes, seeded_sequence);
        workload_bytes.push(b'm');
        for _ in 0..FULLSCREEN_COLUMN_COUNT {
            workload_bytes.push(pick_printable_ascii_byte(seeded_sequence));
        }
    }
    workload_bytes.extend_from_slice(b"\x1b[0m");
}

/// Append update `update_index` of the progress line, as
/// [`OutputWorkload::Progress`] describes: step `update_index % 1001` of `1000`.
fn append_progress_update(workload_bytes: &mut Vec<u8>, update_index: usize) {
    const STEP_COUNT: usize = 1000;
    const BAR_CELL_COUNT: usize = 40;
    let completed_step_count = update_index % (STEP_COUNT + 1);
    let filled_cell_count = completed_step_count * BAR_CELL_COUNT / STEP_COUNT;
    workload_bytes.extend_from_slice(b"\rbuilding [");
    workload_bytes.extend(std::iter::repeat_n(b'#', filled_cell_count));
    workload_bytes.extend(std::iter::repeat_n(
        b'.',
        BAR_CELL_COUNT - filled_cell_count,
    ));
    workload_bytes.extend_from_slice(
        format!(
            "] {:>3}% ({completed_step_count}/{STEP_COUNT})",
            completed_step_count * 100 / STEP_COUNT
        )
        .as_bytes(),
    );
}

#[cfg(test)]
mod tests;
