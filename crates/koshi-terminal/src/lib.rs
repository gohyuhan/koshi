//! `koshi-terminal` — terminal emulation: decodes raw PTY output through a
//! VTE parser (a state machine that turns terminal escape-sequence bytes into
//! actions) and applies those actions to a per-pane screen model — the cell
//! grid, scrollback (lines that scrolled off the top, kept for viewing), the
//! alternate screen (the separate buffer full-screen apps like `vim` draw
//! to), cursor state, terminal modes, and the operations that mutate them.

pub mod engine;
pub mod error;
pub mod grid;
pub mod mouse_report;
pub mod scrollback;
pub mod selection;
pub mod state;
pub mod style;
pub mod types;
