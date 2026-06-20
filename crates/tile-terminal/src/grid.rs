//! Grid domain: fixed-size 2-D arrays of cells backing the screen buffers.
//!
//! [`Grid`](state::Grid) is the storage for one screen buffer (primary or
//! alternate); [`Cell`](state::Cell) is a single position — a character, its
//! display width, and its style.

pub mod state;

#[cfg(test)]
mod tests;
