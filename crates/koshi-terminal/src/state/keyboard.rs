//! One screen's Kitty keyboard protocol flag stack: the entries a pane program
//! pushes, pops and sets, and the flags a query reports.

use serde::{Deserialize, Serialize};

/// The flag bits a stack entry holds: disambiguate escape codes (`1`), report
/// event types (`2`), report alternate keys (`4`), report all keys as escape
/// codes (`8`), and report associated text (`16`). A push, a set and a restore
/// keep these bits and drop every other bit.
const KNOWN_KEYBOARD_FLAGS: u8 = 0b0001_1111;

/// The entries one stack holds. A push onto a full stack drops the oldest
/// entry, and a restore keeps the newest entries within this bound.
const MAX_KEYBOARD_STACK_DEPTH: usize = 8;

/// `flags` reduced to [`KNOWN_KEYBOARD_FLAGS`]: `17` stays `17`, `256` becomes
/// `0`.
fn mask_known_keyboard_flags(keyboard_flags: u16) -> u8 {
    (keyboard_flags & u16::from(KNOWN_KEYBOARD_FLAGS)) as u8
}

/// One screen's Kitty keyboard flag stack, oldest entry first. The last entry
/// gives the flags in effect; an empty stack means flags `0`.
///
/// Serialized as its entry list. A restored list keeps its newest
/// [`MAX_KEYBOARD_STACK_DEPTH`] entries, each masked to
/// [`KNOWN_KEYBOARD_FLAGS`]: `[1, 2, 3, 4, 5, 6, 7, 8, 9, 64]` restores as
/// `[3, 4, 5, 6, 7, 8, 9, 0]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "Vec<u8>", into = "Vec<u8>")]
pub(crate) struct KeyboardStack {
    keyboard_flag_entries: Vec<u8>,
}

impl From<Vec<u8>> for KeyboardStack {
    fn from(restored_keyboard_flag_entries: Vec<u8>) -> Self {
        let first_retained_keyboard_flag_entry_index = restored_keyboard_flag_entries
            .len()
            .saturating_sub(MAX_KEYBOARD_STACK_DEPTH);
        Self {
            keyboard_flag_entries: restored_keyboard_flag_entries
                [first_retained_keyboard_flag_entry_index..]
                .iter()
                .map(|keyboard_flag_entry| keyboard_flag_entry & KNOWN_KEYBOARD_FLAGS)
                .collect(),
        }
    }
}

impl From<KeyboardStack> for Vec<u8> {
    fn from(keyboard_stack: KeyboardStack) -> Self {
        keyboard_stack.keyboard_flag_entries
    }
}

impl KeyboardStack {
    /// The flags in effect: the last entry, or `0` when the stack is empty.
    pub(crate) fn get_current_keyboard_flags(&self) -> u8 {
        self.keyboard_flag_entries.last().copied().unwrap_or(0)
    }

    /// Append `flags` as the new last entry, keeping only the known bits.
    /// A push onto a full stack drops the oldest entry first, so pushing `1`
    /// through `9` onto an empty stack leaves `[2, 3, 4, 5, 6, 7, 8, 9]`.
    pub(crate) fn push_keyboard_flags(&mut self, keyboard_flags: u16) {
        if self.keyboard_flag_entries.len() >= MAX_KEYBOARD_STACK_DEPTH {
            self.keyboard_flag_entries.remove(0);
        }
        self.keyboard_flag_entries
            .push(mask_known_keyboard_flags(keyboard_flags));
    }

    /// Remove the last `entry_count` entries. A count past the entries held
    /// empties the stack, which leaves flags `0`.
    pub(crate) fn pop_keyboard_flag_entries(&mut self, keyboard_flag_entry_count: u16) {
        let retained_keyboard_flag_entry_count = self
            .keyboard_flag_entries
            .len()
            .saturating_sub(usize::from(keyboard_flag_entry_count));
        self.keyboard_flag_entries
            .truncate(retained_keyboard_flag_entry_count);
    }

    /// Change the last entry, creating it when the stack is empty. Operation
    /// `1` replaces it with `keyboard_flags`, `2` adds `keyboard_flags` to it,
    /// and `3` clears those flags from it; any other operation changes
    /// nothing. Only known bits take part, and preceding entries stay as they are.
    ///
    /// Last entry `9`, `keyboard_flags` `4`, operation `2` → last entry `13`.
    pub(crate) fn set_current_keyboard_flags(
        &mut self,
        keyboard_flags: u16,
        keyboard_flag_operation_code: u16,
    ) {
        let masked_keyboard_flags = mask_known_keyboard_flags(keyboard_flags);
        let current_keyboard_flags = self.get_current_keyboard_flags();
        let updated_keyboard_flags = match keyboard_flag_operation_code {
            1 => masked_keyboard_flags,
            2 => current_keyboard_flags | masked_keyboard_flags,
            3 => current_keyboard_flags & !masked_keyboard_flags,
            _ => return,
        };
        match self.keyboard_flag_entries.last_mut() {
            Some(last_keyboard_flag_entry) => *last_keyboard_flag_entry = updated_keyboard_flags,
            None => self.keyboard_flag_entries.push(updated_keyboard_flags),
        }
    }

    /// Remove every entry, which leaves flags `0`.
    pub(crate) fn clear_keyboard_flag_entries(&mut self) {
        self.keyboard_flag_entries.clear();
    }
}

#[cfg(test)]
mod tests;
