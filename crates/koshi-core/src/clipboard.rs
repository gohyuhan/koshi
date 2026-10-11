//! The clipboard targets of a clipboard copy that a program in a pane makes,
//! through an `OSC 52` sequence or through a clipboard tool such as `pbcopy`.

/// Adds `target_byte` to the end of `clipboard_targets` when it is one of `c`,
/// `p`, `q`, `s` and `0` to `7`, and `clipboard_targets` does not hold it yet.
/// Every other byte changes nothing, so `clipboard_targets` holds at most 12
/// bytes.
///
/// Example: pushing `p`, `X`, `c` and `p` into an empty string gives `"pc"`.
pub fn push_clipboard_target(clipboard_targets: &mut String, target_byte: u8) {
    let is_clipboard_target_byte = matches!(target_byte, b'c' | b'p' | b'q' | b's' | b'0'..=b'7');
    if !is_clipboard_target_byte {
        return;
    }
    let target_character = char::from(target_byte);
    if !clipboard_targets.contains(target_character) {
        clipboard_targets.push(target_character);
    }
}

/// The clipboard targets in `raw_target_bytes`, the first field of an
/// `OSC 52` sequence: each byte from `c`, `p`, `q`, `s` and `0` to `7`, once,
/// in the order of its first use. Every other byte is removed. An empty result
/// tells the outer terminal to use its own default targets.
///
/// Example: `b"c"` gives `"c"`, `b"pXcp"` gives `"pc"`, and `b""` and `b"x"`
/// give `""`.
#[must_use]
pub fn filter_clipboard_targets(raw_target_bytes: &[u8]) -> String {
    let mut clipboard_targets = String::new();
    for &target_byte in raw_target_bytes {
        push_clipboard_target(&mut clipboard_targets, target_byte);
    }
    clipboard_targets
}

#[cfg(test)]
mod tests;
