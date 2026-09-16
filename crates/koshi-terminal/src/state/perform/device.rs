//! Device-query replies: the handlers for Device Attributes (DA1 `CSI c`,
//! DA2 `CSI > c`, DA3 `CSI = c`), Device Status Report (DSR `CSI 5/6 n` and
//! the DEC forms `CSI ? Ps n` — cursor position, printer, UDK, keyboard,
//! locator, macro space, checksum, data integrity, multi-session), and mode
//! reports (DECRQM `CSI ? Ps $ p`, ANSI RQM `CSI Ps $ p`).
//!
//! Each handler builds its reply bytes and appends them to the state's reply
//! queue; the runtime drains the queue and writes it back into the pane's
//! PTY. Reply formats follow the xterm control-sequence reference (DECRPM
//! values, the DA parameter lists, the page-less DECXCPR form, the DECRPTUI
//! unit-id report) and, for the printer status, the DEC VT510 manual's "no
//! printer" report.

use crate::state::{MouseEncoding, MouseTracking, Screen, TerminalState};

use super::params::{get_first_parameter_number, get_parameter_number_at};

/// DECRPM's value for a stored mode: `1` when the mode is set, `2` when it is
/// reset.
fn compute_mode_state_number(is_set: bool) -> u16 {
    if is_set {
        1
    } else {
        2
    }
}

/// A version string (`MAJOR.MINOR.PATCH`) packed into one number, two
/// decimal digits per component: `MAJOR * 10_000 + MINOR * 100 + PATCH`.
/// A component that fails to parse counts as `0`.
///
/// The suffix from the first `-` or `+` on (prerelease or build metadata) is
/// cut before packing: a prerelease packs as the version it precedes.
///
/// `"1.16.2"` → `11602`; `"0.2.0-pr.1"` → `200`.
fn compute_version_number(version_text: &str) -> u32 {
    let version_core_text = match version_text.find(['-', '+']) {
        Some(suffix_index) => &version_text[..suffix_index],
        None => version_text,
    };
    let mut packed_version_number: u32 = 0;
    for version_component_text in version_core_text.split('.') {
        packed_version_number = packed_version_number
            .saturating_mul(100)
            .saturating_add(version_component_text.parse::<u32>().unwrap_or(0));
    }
    packed_version_number
}

impl TerminalState {
    /// Answer character and pixel size queries for the active pane.
    pub(super) fn report_window_size(&mut self, params: &vte::Params) {
        if params.len() != 1 {
            return;
        }
        let (row_count, column_count) = self.get_active_grid().get_grid_dimensions();
        let device_reply_bytes = match get_first_parameter_number(params) {
            Some(18) => format!("\x1b[8;{row_count};{column_count}t"),
            Some(14) => {
                let Some(pixel_cell_size) = self.cell_size else {
                    return;
                };
                format!(
                    "\x1b[4;{};{}t",
                    u32::from(row_count) * u32::from(pixel_cell_size.get_pixel_height()),
                    u32::from(column_count) * u32::from(pixel_cell_size.get_pixel_width())
                )
            }
            Some(16) => {
                let Some(pixel_cell_size) = self.cell_size else {
                    return;
                };
                format!(
                    "\x1b[6;{};{}t",
                    pixel_cell_size.get_pixel_height(),
                    pixel_cell_size.get_pixel_width()
                )
            }
            _ => return,
        };
        self.device_query_replies
            .extend_from_slice(device_reply_bytes.as_bytes());
    }

    /// Reply to Primary Device Attributes (DA1, `CSI c` / `CSI 0 c`): queue
    /// `CSI ? 62 ; 22 c`, identifying a VT220-class terminal with the ANSI
    /// color extension (22). A nonzero parameter gets no reply.
    pub(super) fn report_primary_device_attributes(&mut self, params: &vte::Params) {
        if get_first_parameter_number(params).unwrap_or(0) != 0 {
            return;
        }
        self.device_query_replies.extend_from_slice(b"\x1b[?62;22c");
    }

    /// Reply to Secondary Device Attributes (DA2, `CSI > c` / `CSI > 0 c`):
    /// queue `CSI > 1 ; Pv ; 0 c` — terminal type 1 (VT220), firmware version
    /// `Pv` packed from this crate's version by [`compute_version_number`], and ROM
    /// cartridge number 0. A nonzero parameter gets no reply.
    pub(super) fn report_secondary_device_attributes(&mut self, params: &vte::Params) {
        if get_first_parameter_number(params).unwrap_or(0) != 0 {
            return;
        }
        let package_version_number = compute_version_number(env!("CARGO_PKG_VERSION"));
        let device_reply_bytes = format!("\x1b[>1;{package_version_number};0c");
        self.device_query_replies
            .extend_from_slice(device_reply_bytes.as_bytes());
    }

    /// Return the cursor coordinates used by CPR and DECXCPR. DECOM reports
    /// coordinates relative to the active vertical and horizontal margins.
    fn get_reported_cursor_position(&self) -> (u16, u16) {
        let (cursor_row_index, cursor_column_index) = self.get_active_cursor_position();
        if !self.active_cursor().origin {
            return (cursor_row_index, cursor_column_index);
        }
        let top_row_index = self.get_scroll_region().map_or(0, |(top, _)| top);
        let left_column_index = self.get_horizontal_margins().map_or(0, |(left, _)| left);
        (
            cursor_row_index.saturating_sub(top_row_index),
            cursor_column_index.saturating_sub(left_column_index),
        )
    }

    /// Reply to a Device Status Report (DSR, `CSI Ps n`): `Ps = 5` (operating
    /// status) queues the all-good `CSI 0 n`; `Ps = 6` (CPR, cursor position
    /// report) queues `CSI row ; column R` with the active cursor's 1-based
    /// position. Any other `Ps` gets no reply.
    pub(super) fn report_device_status(&mut self, params: &vte::Params) {
        match get_first_parameter_number(params).unwrap_or(0) {
            5 => self.device_query_replies.extend_from_slice(b"\x1b[0n"),
            6 => {
                let (cursor_row_index, cursor_column_index) = self.get_reported_cursor_position();
                let device_reply_bytes =
                    format!("\x1b[{};{}R", cursor_row_index + 1, cursor_column_index + 1);
                self.device_query_replies
                    .extend_from_slice(device_reply_bytes.as_bytes());
            }
            _ => {}
        }
    }

    /// Reply to Tertiary Device Attributes (DA3, `CSI = c` / `CSI = 0 c`):
    /// queue the DECRPTUI unit-id report `DCS ! | 00000000 ST` — all-zero
    /// site code and serial number. A nonzero parameter gets no reply.
    pub(super) fn report_tertiary_device_attributes(&mut self, params: &vte::Params) {
        if get_first_parameter_number(params).unwrap_or(0) != 0 {
            return;
        }
        self.device_query_replies
            .extend_from_slice(b"\x1bP!|00000000\x1b\\");
    }

    /// Reply to a DEC-form Device Status Report (`CSI ? Ps n`):
    ///
    /// - `6` (DECXCPR) — `CSI ? row ; column R`, the active cursor's 1-based
    ///   position, no page parameter.
    /// - `15` (printer) — `CSI ? 13 n`, "no printer".
    /// - `25` (UDK) — `CSI ? 21 n`, "locked": no user-defined keys.
    /// - `26` (keyboard) — `CSI ? 27 ; 1 ; 0 ; 0 n`, North American, ready.
    /// - `53`/`55` (locator status) — `CSI ? 53 n`, "no locator".
    /// - `56` (locator type) — `CSI ? 57 ; 0 n`, "cannot identify".
    /// - `62` (DECMSR, macro space) — `CSI 0 * {`, zero space.
    /// - `63` (DECCKSR, memory checksum) — `DCS Pid ! ~ 0000 ST`: `Pid` is
    ///   the second parameter (`0` when absent), the checksum is zero.
    /// - `75` (data integrity) — `CSI ? 70 n`, ready, no errors.
    /// - `85` (multi-session) — `CSI ? 83 n`, not configured for
    ///   multiple-session operation.
    ///
    /// Any other `Ps` gets no reply.
    pub(super) fn report_dec_device_status(&mut self, params: &vte::Params) {
        match get_first_parameter_number(params).unwrap_or(0) {
            6 => {
                let (cursor_row_index, cursor_column_index) = self.get_reported_cursor_position();
                let device_reply_bytes = format!(
                    "\x1b[?{};{}R",
                    cursor_row_index + 1,
                    cursor_column_index + 1
                );
                self.device_query_replies
                    .extend_from_slice(device_reply_bytes.as_bytes());
            }
            15 => self.device_query_replies.extend_from_slice(b"\x1b[?13n"),
            25 => self.device_query_replies.extend_from_slice(b"\x1b[?21n"),
            26 => self
                .device_query_replies
                .extend_from_slice(b"\x1b[?27;1;0;0n"),
            53 | 55 => self.device_query_replies.extend_from_slice(b"\x1b[?53n"),
            56 => self.device_query_replies.extend_from_slice(b"\x1b[?57;0n"),
            62 => self.device_query_replies.extend_from_slice(b"\x1b[0*{"),
            63 => {
                let request_id = get_parameter_number_at(params, 1).unwrap_or(0);
                let device_reply_bytes = format!("\x1bP{request_id}!~0000\x1b\\");
                self.device_query_replies
                    .extend_from_slice(device_reply_bytes.as_bytes());
            }
            75 => self.device_query_replies.extend_from_slice(b"\x1b[?70n"),
            85 => self.device_query_replies.extend_from_slice(b"\x1b[?83n"),
            _ => {}
        }
    }

    /// Reply to Request Mode, DEC form (DECRQM, `CSI ? Ps $ p`): queue the
    /// DECRPM report `CSI ? Ps ; Pm $ y`, where `Pm` is the mode's state from
    /// [`get_dec_mode_state_number`](Self::get_dec_mode_state_number).
    pub(super) fn report_dec_mode(&mut self, params: &vte::Params) {
        let mode_number = get_first_parameter_number(params).unwrap_or(0);
        let mode_state_value = self.get_dec_mode_state_number(mode_number);
        let device_reply_bytes = format!("\x1b[?{mode_number};{mode_state_value}$y");
        self.device_query_replies
            .extend_from_slice(device_reply_bytes.as_bytes());
    }

    /// Reply to Request Mode, ANSI form (`CSI Ps $ p`): queue the report
    /// `CSI Ps ; 0 $ y`. No ANSI (non-`?`) mode is stored, so every query
    /// reports `0`, "not recognized".
    pub(super) fn report_ansi_mode(&mut self, params: &vte::Params) {
        let mode_number = get_first_parameter_number(params).unwrap_or(0);
        let device_reply_bytes = format!("\x1b[{mode_number};0$y");
        self.device_query_replies
            .extend_from_slice(device_reply_bytes.as_bytes());
    }

    /// The DECRPM value for DEC private mode `mode`: `1` (set) or `2` (reset)
    /// read from the stored mode state, and `0` ("not recognized") for every
    /// mode that is not stored — including the ignored `?2`/`?3`/`?8` and the
    /// save/restore action `?1048`, which keeps no queryable state.
    ///
    /// The mutually exclusive families report per member: each mouse tracking
    /// level (`?9`/`?1000`/`?1002`/`?1003`) and encoding (`?1005`/`?1006`/
    /// `?1015`) is set exactly when it is the active one, and the alternate
    /// screen modes (`?47`/`?1047`/`?1049`) are set exactly while the
    /// alternate screen is active. `?25` reports the active screen's cursor
    /// visibility.
    fn get_dec_mode_state_number(&self, mode_number: u16) -> u16 {
        match mode_number {
            1 => compute_mode_state_number(self.modes.application_cursor_keys),
            5 => compute_mode_state_number(self.modes.reverse_video),
            6 => compute_mode_state_number(self.active_cursor().origin),
            7 => compute_mode_state_number(self.modes.autowrap),
            69 => compute_mode_state_number(self.modes.declrmm),
            9 => compute_mode_state_number(self.modes.mouse_tracking == MouseTracking::X10),
            12 => compute_mode_state_number(self.modes.cursor_blink),
            25 => compute_mode_state_number(self.active_cursor().is_visible),
            47 | 1047 | 1049 => compute_mode_state_number(self.active_screen == Screen::Alternate),
            1000 => compute_mode_state_number(self.modes.mouse_tracking == MouseTracking::Normal),
            1002 => {
                compute_mode_state_number(self.modes.mouse_tracking == MouseTracking::ButtonMotion)
            }
            1003 => {
                compute_mode_state_number(self.modes.mouse_tracking == MouseTracking::AnyMotion)
            }
            1005 => compute_mode_state_number(self.modes.mouse_encoding == MouseEncoding::Utf8),
            1006 => compute_mode_state_number(self.modes.mouse_encoding == MouseEncoding::Sgr),
            1007 => compute_mode_state_number(self.modes.alternate_scroll),
            1015 => compute_mode_state_number(self.modes.mouse_encoding == MouseEncoding::Urxvt),
            2004 => compute_mode_state_number(self.modes.bracketed_paste),
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests;
