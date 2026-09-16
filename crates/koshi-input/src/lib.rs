//! `koshi-input` — the outer terminal's input boundary.
//!
//! [`keyboard::decode_key_event`] turns one host key event into a complete
//! [`koshi_core::key::KeyInput`], which keeps every field the terminal
//! reported. [`koshi_core::key::KeyInput::to_binding_chord`] projects that
//! event onto the canonical [`koshi_core::key::KeyChord`] a keybinding
//! matches, and [`keyboard::encode_key_chord`] turns a chord back into the
//! bytes a program running inside a pane expects.
//! [`mouse::decode_mouse`] turns one host mouse event into a canonical
//! [`koshi_core::mouse::MouseInput`].

pub mod host;
pub mod keyboard;
pub mod mouse;
