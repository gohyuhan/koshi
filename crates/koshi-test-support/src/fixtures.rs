//! Shared test fixtures.

use koshi_core::key::{KeyChord, KeyEventKind, KeyIdentity, KeyInput, KeyModifierFlags, ModFlags};
use tempfile::TempDir;

/// Each keybinding modifier beside the reported modifier that stands for it.
/// The two sets number their bits differently, so a chord's bit pattern is
/// never a reported bit pattern.
const REPORTED_MODIFIER_BY_BINDING_MODIFIER: [(ModFlags, KeyModifierFlags); 4] = [
    (ModFlags::CTRL, KeyModifierFlags::CTRL),
    (ModFlags::ALT, KeyModifierFlags::ALT),
    (ModFlags::SHIFT, KeyModifierFlags::SHIFT),
    (ModFlags::SUPER, KeyModifierFlags::SUPER),
];

/// The complete key event a terminal reports for one chord: a press, with no
/// alternative keys and no associated text.
///
/// [`KeyInput::to_binding_chord`] on the result gives `chord` back.
#[must_use]
pub fn build_key_input_for_chord(chord: KeyChord) -> KeyInput {
    let mut modifier_flags = KeyModifierFlags::NONE;
    for (binding_modifier, reported_modifier) in REPORTED_MODIFIER_BY_BINDING_MODIFIER {
        if chord.modifier_flags.has_all_modifiers(binding_modifier) {
            modifier_flags = modifier_flags.union(reported_modifier);
        }
    }
    KeyInput {
        key: KeyIdentity::Key(chord.key),
        key_event_kind: KeyEventKind::Press,
        shifted_key: None,
        base_layout_key: None,
        associated_text: String::new(),
        modifier_flags,
    }
}

/// Create an isolated runtime directory and remove it when the returned
/// [`TempDir`] drops.
///
/// Unix uses `/tmp` as the parent directory. Windows uses
/// [`std::env::temp_dir`].
///
/// # Panics
///
/// Panics when the directory cannot be created.
#[must_use]
pub fn build_test_runtime_directory() -> TempDir {
    #[cfg(unix)]
    let base = std::path::PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    TempDir::new_in(base).expect("a temporary runtime directory")
}

#[cfg(test)]
mod tests;
