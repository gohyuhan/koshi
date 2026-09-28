//! Kitty keyboard protocol sequences: the pushes, pops and sets that change
//! the active screen's flag stack, and the query that reports its flags.

use crate::state::TerminalState;

use super::params::{get_first_parameter_number, get_parameter_number_at};

impl TerminalState {
    /// `CSI > flags u` — push `flags` onto the active screen's stack. An absent
    /// parameter pushes flags `0`.
    pub(super) fn push_keyboard_flags(&mut self, csi_parameters: &vte::Params) {
        let keyboard_flags = get_first_parameter_number(csi_parameters).unwrap_or(0);
        self.get_active_keyboard_stack_mut()
            .push_keyboard_flags(keyboard_flags);
    }

    /// `CSI < count u` — pop `count` entries off the active screen's stack. An
    /// absent parameter and an explicit `0` both pop one entry. A count past
    /// the entries held empties the stack, which leaves flags `0`.
    pub(super) fn pop_keyboard_flags(&mut self, csi_parameters: &vte::Params) {
        let keyboard_flag_entry_count = get_first_parameter_number(csi_parameters)
            .filter(|&csi_parameter_number| csi_parameter_number != 0)
            .unwrap_or(1);
        self.get_active_keyboard_stack_mut()
            .pop_keyboard_flag_entries(keyboard_flag_entry_count);
    }

    /// `CSI = flags ; mode u` — change the active screen's current entry,
    /// creating it when the stack is empty. Mode `1` replaces it with `flags`,
    /// `2` adds `flags` to it, and `3` clears `flags` from it; an absent mode
    /// and an explicit `0` both mean `1`. Absent flags mean `0`.
    pub(super) fn set_keyboard_flags(&mut self, csi_parameters: &vte::Params) {
        let keyboard_flags = get_first_parameter_number(csi_parameters).unwrap_or(0);
        let keyboard_flag_operation_code = get_parameter_number_at(csi_parameters, 1)
            .filter(|&csi_parameter_number| csi_parameter_number != 0)
            .unwrap_or(1);
        self.get_active_keyboard_stack_mut()
            .set_current_keyboard_flags(keyboard_flags, keyboard_flag_operation_code);
    }

    /// `CSI ? u` — queue `CSI ? flags u` for the app, reporting the active
    /// screen's current flags. An empty stack reports `CSI ? 0 u`.
    pub(super) fn report_keyboard_flags(&mut self) {
        let keyboard_flags = self.get_keyboard_flags();
        self.device_query_replies
            .extend_from_slice(format!("\x1b[?{keyboard_flags}u").as_bytes());
    }
}

#[cfg(test)]
mod tests;
