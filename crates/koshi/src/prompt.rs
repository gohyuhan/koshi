//! Reading a yes-or-no answer from the terminal koshi was typed in.

use std::io::{self, Write};

/// True for `y` and `yes` in any letter case, once `answer` is trimmed of
/// surrounding whitespace. False for every other answer, an empty one
/// included.
///
/// Example — `" YES\n"` is true, and `"yep"` is false.
pub(crate) fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Print `prompt` on standard output, flush it, read one line from standard
/// input, and answer it with [`is_yes`].
///
/// False for standard input that cannot be read.
pub(crate) fn yes(prompt: &str) -> bool {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    is_yes(&line)
}

#[cfg(test)]
mod tests;
