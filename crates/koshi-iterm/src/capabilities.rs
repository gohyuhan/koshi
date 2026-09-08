//! iTerm2 feature-reporting helpers.

/// The iTerm2 feature-reporting request, including its OSC terminator.
pub const ITERM_CAPABILITIES_QUERY: &[u8] = b"\x1b]1337;Capabilities\x1b\\";

/// Return whether an iTerm2 feature string advertises the `FILE` feature.
///
/// The official encoding joins feature codes without separators. A code starts
/// with an uppercase ASCII letter and continues with lowercase letters and,
/// for integer features, digits. The parser stops at the first non-alphanumeric
/// byte, as required by the feature-reporting specification. Thus `F` is true,
/// `Foo` is false, and `F;unknown` is true.
pub fn iterm_feature_string_supports_file(feature_string: &[u8]) -> bool {
    iterm_feature_string_supports(feature_string, b"F")
}

/// Return whether an iTerm2 feature string advertises the `SIXEL` feature.
pub fn iterm_feature_string_supports_sixel(feature_string: &[u8]) -> bool {
    iterm_feature_string_supports(feature_string, b"Sx")
}

fn iterm_feature_string_supports(feature_string: &[u8], wanted: &[u8]) -> bool {
    let prefix = feature_string
        .iter()
        .position(|byte| !byte.is_ascii_alphanumeric())
        .map_or(feature_string, |index| &feature_string[..index]);
    let mut index = 0;
    while index < prefix.len() {
        if !prefix[index].is_ascii_uppercase() {
            return false;
        }
        let token_start = index;
        index += 1;
        while index < prefix.len() && prefix[index].is_ascii_lowercase() {
            index += 1;
        }
        while index < prefix.len() && prefix[index].is_ascii_digit() {
            index += 1;
        }
        if &prefix[token_start..index] == wanted {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests;
