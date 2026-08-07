//! `koshi-input` — outer terminal input: keyboard/mouse decoding, keybinding
//! resolution, input modes, lock/unlock, mouse drag state, and the
//! privacy-aware input event classifier for typing/Enter events.
//!
//! A keybinding is a key the user writes in the config, paired with the action
//! koshi runs when that key arrives.

pub mod error;
pub mod keyboard;
pub mod mouse;
pub mod types;

pub mod input;
