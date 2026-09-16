//! Kitty keyboard protocol sequences: the pushes, pops and sets that change
//! the active screen's flag stack, and the query that reports its flags.

use crate::state::TerminalState;

use super::params::{get_first_parameter_number, get_parameter_number_at};

impl TerminalState {
    /// `CSI > flags u` — push `flags` onto the active screen's stack. An absent
    /// parameter pushes flags `0`.
    pub(super) fn push_keyboard_flags(&mut self, params: &vte::Params) {
        let flags = get_first_parameter_number(params).unwrap_or(0);
        self.active_keyboard_stack_mut().push_flags(flags);
    }

    /// `CSI < count u` — pop `count` entries off the active screen's stack. An
    /// absent parameter and an explicit `0` both pop one entry. A count past
    /// the entries held empties the stack, which leaves flags `0`.
    pub(super) fn pop_keyboard_flags(&mut self, params: &vte::Params) {
        let entry_count = get_first_parameter_number(params)
            .filter(|&parameter_value| parameter_value != 0)
            .unwrap_or(1);
        self.active_keyboard_stack_mut().pop_entries(entry_count);
    }

    /// `CSI = flags ; mode u` — change the active screen's current entry,
    /// creating it when the stack is empty. Mode `1` replaces it with `flags`,
    /// `2` adds `flags` to it, and `3` clears `flags` from it; an absent mode
    /// and an explicit `0` both mean `1`. Absent flags mean `0`.
    pub(super) fn set_keyboard_flags(&mut self, params: &vte::Params) {
        let flags = get_first_parameter_number(params).unwrap_or(0);
        let mode = get_parameter_number_at(params, 1)
            .filter(|&parameter_value| parameter_value != 0)
            .unwrap_or(1);
        self.active_keyboard_stack_mut()
            .set_current_flags(flags, mode);
    }

    /// `CSI ? u` — queue `CSI ? flags u` for the app, reporting the active
    /// screen's current flags. An empty stack reports `CSI ? 0 u`.
    pub(super) fn report_keyboard_flags(&mut self) {
        let flags = self.get_keyboard_flags();
        self.device_query_replies
            .extend_from_slice(format!("\x1b[?{flags}u").as_bytes());
    }
}

#[cfg(test)]
mod tests;
