//! CSI parameter accessors: read cursor counts and coordinates out of a parsed
//! [`vte::Params`], applying the VT defaults (a missing or zero argument means
//! one; 1-based coordinates map to 0-based).

/// The first CSI parameter's primary value, or `None` if empty.
pub(super) fn first_param(params: &vte::Params) -> Option<u16> {
    params.iter().next().and_then(|p| p.first().copied())
}

/// The `n`-th CSI parameter's primary value (0-based), or `None` when absent.
pub(super) fn nth_param(params: &vte::Params, n: usize) -> Option<u16> {
    params.iter().nth(n).and_then(|p| p.first().copied())
}

/// A cursor-move distance: a missing argument or an explicit `0` both mean `1`.
pub(super) fn move_count(params: &vte::Params) -> u16 {
    first_param(params).filter(|&v| v != 0).unwrap_or(1)
}

/// A 1-based CUP/HVP coordinate converted to 0-based: missing or `0` → `1`,
/// then decremented, so the default lands on the top-left cell `(0, 0)`.
pub(super) fn coord_param(params: &vte::Params, n: usize) -> u16 {
    nth_param(params, n)
        .filter(|&v| v != 0)
        .unwrap_or(1)
        .saturating_sub(1)
}
