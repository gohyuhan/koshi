//! CSI parameter accessors: read cursor counts and coordinates out of a parsed
//! [`vte::Params`], applying the VT defaults (a missing or zero argument means
//! one; 1-based coordinates map to 0-based).

/// The first CSI parameter's primary value, or `None` when there are no
/// parameters.
pub(super) fn get_first_parameter_number(params: &vte::Params) -> Option<u16> {
    get_parameter_number_at(params, 0)
}

/// The `n`-th CSI parameter's primary value (0-based), or `None` when absent.
pub(super) fn get_parameter_number_at(params: &vte::Params, parameter_index: usize) -> Option<u16> {
    params
        .iter()
        .nth(parameter_index)
        .and_then(|parameter_values| parameter_values.first().copied())
}

/// A cursor-move distance: a missing argument or an explicit `0` both mean `1`.
pub(super) fn get_cursor_move_count(params: &vte::Params) -> u16 {
    get_first_parameter_number(params)
        .filter(|&parameter_value| parameter_value != 0)
        .unwrap_or(1)
}

/// A 1-based CUP/HVP coordinate converted to 0-based: missing or `0` → `1`,
/// then decremented, so the default lands on the top-left cell `(0, 0)`.
pub(super) fn get_cursor_coordinate(params: &vte::Params, parameter_index: usize) -> u16 {
    get_parameter_number_at(params, parameter_index)
        .filter(|&parameter_value| parameter_value != 0)
        .unwrap_or(1)
        .saturating_sub(1)
}
