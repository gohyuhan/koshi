//! `koshi-input` — the outer terminal's input boundary.
//!
//! [`keyboard::decode_key`] turns one host key event into a canonical
//! [`koshi_core::key::KeyChord`], and [`keyboard::encode`] turns a chord back
//! into the bytes a program running inside a pane expects.
//! [`mouse::decode_mouse`] turns one host mouse event into a canonical
//! [`koshi_core::mouse::MouseInput`].

pub mod host;
pub mod keyboard;
pub mod mouse;
