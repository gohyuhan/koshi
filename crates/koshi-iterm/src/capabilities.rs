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

fn iterm_feature_string_supports(feature_string: &[u8], requested_feature: &[u8]) -> bool {
    let feature_prefix = feature_string
        .iter()
        .position(|feature_byte| !feature_byte.is_ascii_alphanumeric())
        .map_or(feature_string, |prefix_end_index| {
            &feature_string[..prefix_end_index]
        });
    let mut feature_index = 0;
    while feature_index < feature_prefix.len() {
        if !feature_prefix[feature_index].is_ascii_uppercase() {
            return false;
        }
        let token_start_index = feature_index;
        feature_index += 1;
        while feature_index < feature_prefix.len()
            && feature_prefix[feature_index].is_ascii_lowercase()
        {
            feature_index += 1;
        }
        while feature_index < feature_prefix.len() && feature_prefix[feature_index].is_ascii_digit()
        {
            feature_index += 1;
        }
        if &feature_prefix[token_start_index..feature_index] == requested_feature {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests;
