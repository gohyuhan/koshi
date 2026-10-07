//! Reading a yes-or-no answer from the terminal koshi was typed in.

use koshi_link::remote_client::prompt_line;

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

/// Print `prompt_text` on standard error and read one line from standard input
/// through [`prompt_line`], then answer that line with [`is_yes_answer`].
///
/// False for standard input that cannot be read, and for standard input that
/// ends before a line arrives.
pub(crate) fn read_yes_answer(prompt_text: &str) -> bool {
    prompt_line(prompt_text).is_ok_and(|answer_line| is_yes_answer(&answer_line))
}

#[cfg(test)]
mod tests;
