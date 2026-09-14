//! Reading a yes-or-no answer from the terminal koshi was typed in.

use std::io::{self, Write};

/// True for `y` and `yes` in any letter case, once `answer_text` is trimmed of
/// surrounding whitespace. False for every other answer, an empty one
/// included.
///
/// Example — `" YES\n"` is true, and `"yep"` is false.
pub(crate) fn is_yes_answer(answer_text: &str) -> bool {
    matches!(
        answer_text.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    )
}

/// Print `prompt_text` on standard output, flush it, read one line from standard
/// input, and answer it with [`is_yes_answer`].
///
/// False for standard input that cannot be read.
pub(crate) fn read_yes_answer(prompt_text: &str) -> bool {
    print!("{prompt_text}");
    let _ = io::stdout().flush();
    let mut answer_line = String::new();
    if io::stdin().read_line(&mut answer_line).is_err() {
        return false;
    }
    is_yes_answer(&answer_line)
}

#[cfg(test)]
mod tests;
