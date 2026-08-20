//! Reading a yes-or-no answer from the terminal koshi was typed in.

use std::io::{self, Write};

/// Print `prompt` on standard output, flush it, and read one line from
/// standard input.
///
/// True for exactly `y`, `Y`, `yes` or `Yes` once the line is trimmed of
/// surrounding whitespace. False for every other line, and for standard input
/// that cannot be read.
pub(crate) fn yes(prompt: &str) -> bool {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes" | "Yes")
}
