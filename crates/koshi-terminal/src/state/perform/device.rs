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

use super::params::{first_param, nth_param};

/// DECRPM's value for a stored mode: `1` when the mode is set, `2` when it is
/// reset.
fn mode_flag(on: bool) -> u16 {
    if on {
        1
    } else {
        2
    }
}

/// `version` (a `MAJOR.MINOR.PATCH` string) packed into one number, two
/// decimal digits per component: `MAJOR * 10_000 + MINOR * 100 + PATCH`.
/// A component that fails to parse counts as `0`.
///
/// The suffix from the first `-` or `+` on (prerelease or build metadata) is
/// cut before packing: a prerelease packs as the version it precedes.
///
/// `"1.16.2"` → `11602`; `"0.2.0-pr.1"` → `200`.
fn version_number(version: &str) -> u32 {
    let core = match version.find(['-', '+']) {
        Some(suffix_at) => &version[..suffix_at],
        None => version,
    };
    let mut number: u32 = 0;
    for part in core.split('.') {
        number = number
            .saturating_mul(100)
            .saturating_add(part.parse::<u32>().unwrap_or(0));
    }
    number
}

impl TerminalState {
    /// Reply to Primary Device Attributes (DA1, `CSI c` / `CSI 0 c`): queue
    /// `CSI ? 62 ; 22 c`, identifying a VT220-class terminal with the ANSI
    /// color extension (22). A nonzero parameter gets no reply.
    pub(super) fn device_attributes_primary(&mut self, params: &vte::Params) {
        if first_param(params).unwrap_or(0) != 0 {
            return;
        }
        self.replies.extend_from_slice(b"\x1b[?62;22c");
    }

    /// Reply to Secondary Device Attributes (DA2, `CSI > c` / `CSI > 0 c`):
    /// queue `CSI > 1 ; Pv ; 0 c` — terminal type 1 (VT220), firmware version
    /// `Pv` packed from this crate's version by [`version_number`], and ROM
    /// cartridge number 0. A nonzero parameter gets no reply.
    pub(super) fn device_attributes_secondary(&mut self, params: &vte::Params) {
        if first_param(params).unwrap_or(0) != 0 {
            return;
        }
        let version = version_number(env!("CARGO_PKG_VERSION"));
        let reply = format!("\x1b[>1;{version};0c");
        self.replies.extend_from_slice(reply.as_bytes());
    }

    /// Reply to a Device Status Report (DSR, `CSI Ps n`): `Ps = 5` (operating
    /// status) queues the all-good `CSI 0 n`; `Ps = 6` (CPR, cursor position
    /// report) queues `CSI row ; col R` with the active cursor's 1-based
    /// position. Any other `Ps` gets no reply.
    pub(super) fn device_status_report(&mut self, params: &vte::Params) {
        match first_param(params).unwrap_or(0) {
            5 => self.replies.extend_from_slice(b"\x1b[0n"),
            6 => {
                let (row, col) = self.active_cursor_position();
                let reply = format!("\x1b[{};{}R", row + 1, col + 1);
                self.replies.extend_from_slice(reply.as_bytes());
            }
            _ => {}
        }
    }

    /// Reply to Tertiary Device Attributes (DA3, `CSI = c` / `CSI = 0 c`):
    /// queue the DECRPTUI unit-id report `DCS ! | 00000000 ST` — all-zero
    /// site code and serial number. A nonzero parameter gets no reply.
    pub(super) fn device_attributes_tertiary(&mut self, params: &vte::Params) {
        if first_param(params).unwrap_or(0) != 0 {
            return;
        }
        self.replies.extend_from_slice(b"\x1bP!|00000000\x1b\\");
    }

    /// Reply to a DEC-form Device Status Report (`CSI ? Ps n`):
    ///
    /// - `6` (DECXCPR) — `CSI ? row ; col R`, the active cursor's 1-based
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
    pub(super) fn dec_device_status_report(&mut self, params: &vte::Params) {
        match first_param(params).unwrap_or(0) {
            6 => {
                let (row, col) = self.active_cursor_position();
                let reply = format!("\x1b[?{};{}R", row + 1, col + 1);
                self.replies.extend_from_slice(reply.as_bytes());
            }
            15 => self.replies.extend_from_slice(b"\x1b[?13n"),
            25 => self.replies.extend_from_slice(b"\x1b[?21n"),
            26 => self.replies.extend_from_slice(b"\x1b[?27;1;0;0n"),
            53 | 55 => self.replies.extend_from_slice(b"\x1b[?53n"),
            56 => self.replies.extend_from_slice(b"\x1b[?57;0n"),
            62 => self.replies.extend_from_slice(b"\x1b[0*{"),
            63 => {
                let request_id = nth_param(params, 1).unwrap_or(0);
                let reply = format!("\x1bP{request_id}!~0000\x1b\\");
                self.replies.extend_from_slice(reply.as_bytes());
            }
            75 => self.replies.extend_from_slice(b"\x1b[?70n"),
            85 => self.replies.extend_from_slice(b"\x1b[?83n"),
            _ => {}
        }
    }

    /// Reply to Request Mode, DEC form (DECRQM, `CSI ? Ps $ p`): queue the
    /// DECRPM report `CSI ? Ps ; Pm $ y`, where `Pm` is the mode's state from
    /// [`dec_mode_state`](Self::dec_mode_state).
    pub(super) fn report_dec_mode(&mut self, params: &vte::Params) {
        let mode = first_param(params).unwrap_or(0);
        let value = self.dec_mode_state(mode);
        let reply = format!("\x1b[?{mode};{value}$y");
        self.replies.extend_from_slice(reply.as_bytes());
    }

    /// Reply to Request Mode, ANSI form (`CSI Ps $ p`): queue the report
    /// `CSI Ps ; 0 $ y`. No ANSI (non-`?`) mode is stored, so every query
    /// reports `0`, "not recognized".
    pub(super) fn report_ansi_mode(&mut self, params: &vte::Params) {
        let mode = first_param(params).unwrap_or(0);
        let reply = format!("\x1b[{mode};0$y");
        self.replies.extend_from_slice(reply.as_bytes());
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
    fn dec_mode_state(&self, mode: u16) -> u16 {
        match mode {
            1 => mode_flag(self.modes.app_cursor_keys),
            5 => mode_flag(self.modes.reverse_video),
            7 => mode_flag(self.modes.autowrap),
            9 => mode_flag(self.modes.mouse_tracking == MouseTracking::X10),
            12 => mode_flag(self.modes.cursor_blink),
            25 => mode_flag(self.active_cursor().is_visible),
            47 | 1047 | 1049 => mode_flag(self.active == Screen::Alternate),
            1000 => mode_flag(self.modes.mouse_tracking == MouseTracking::Normal),
            1002 => mode_flag(self.modes.mouse_tracking == MouseTracking::ButtonMotion),
            1003 => mode_flag(self.modes.mouse_tracking == MouseTracking::AnyMotion),
            1005 => mode_flag(self.modes.mouse_encoding == MouseEncoding::Utf8),
            1006 => mode_flag(self.modes.mouse_encoding == MouseEncoding::Sgr),
            1007 => mode_flag(self.modes.alt_scroll),
            1015 => mode_flag(self.modes.mouse_encoding == MouseEncoding::Urxvt),
            2004 => mode_flag(self.modes.bracketed_paste),
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests;
