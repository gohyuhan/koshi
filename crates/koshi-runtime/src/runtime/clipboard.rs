//! Clipboard writes: the copies koshi and the programs in its panes make.
//!
//! OSC 52 is the terminal escape that sets the clipboard. It travels to the
//! **outer terminal** — the program koshi itself runs in — which owns the real
//! clipboard. The payload is base64. Base64 carries every byte value.
//!
//! OSC 52 is the only clipboard koshi writes to. Three copies reach it:
//!
//! - koshi's own copy of a highlight goes to the client that made it
//!   (`Server::copy_to_clipboard`);
//! - an `OSC 52` copy that a program in a pane writes goes to every client whose
//!   focused pane is that pane (`Server::copy_pane_clipboard_to_clients`);
//! - a copy that a stand-in clipboard tool, such as `pbcopy`, hands the session
//!   goes to every client on another machine whose focused pane is the tool's
//!   pane (`Server::route_clipboard_tool_copy`).
//!
//! The last two reach a client only while `pane-clipboard-writes` allows
//! writes.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use koshi_core::client::ClientOrigin;
use koshi_core::clipboard::filter_clipboard_targets;
use koshi_core::ids::{ClientId, PaneId};
use koshi_terminal::engine::ClipboardCopy;

use crate::runtime::bus::HostWrite;
use crate::server::Server;

/// The OSC 52 sequence that puts `copied_bytes` on the clipboard targets
/// `clipboard_targets`: `ESC ] 52 ; <clipboard_targets> ; <base64 of
/// copied_bytes> BEL`.
///
/// `"c"` and `hello` → `\x1b]52;c;aGVsbG8=\x07`. `""` and no bytes →
/// `\x1b]52;;\x07`.
#[must_use]
pub(crate) fn encode_osc52_copy(clipboard_targets: &str, copied_bytes: &[u8]) -> Vec<u8> {
    let base64_byte_count = copied_bytes.len().div_ceil(3) * 4;
    let mut osc52_sequence = String::with_capacity(
        "\x1b]52;".len() + clipboard_targets.len() + ";".len() + base64_byte_count + "\x07".len(),
    );
    osc52_sequence.push_str("\x1b]52;");
    osc52_sequence.push_str(clipboard_targets);
    osc52_sequence.push(';');
    STANDARD.encode_string(copied_bytes, &mut osc52_sequence);
    osc52_sequence.push('\x07');
    osc52_sequence.into_bytes()
}

impl Server {
    /// Queue the OSC 52 escape carrying `copied_text` for the target `c`, the
    /// clipboard, for `client_id`'s outer terminal, as
    /// [`queue_host_write`](Self::queue_host_write) states. An empty
    /// `copied_text` still queues the sequence. A copy that the client's
    /// waiting host writes refuse is skipped.
    pub(crate) fn copy_to_clipboard(&mut self, client_id: ClientId, copied_text: &str) {
        self.queue_host_write(
            client_id,
            &HostWrite::from_clipboard_copy("c", copied_text.as_bytes()),
        );
    }

    /// Queue each of `clipboard_copies`, the `OSC 52` copies the program in
    /// `pane_id` wrote, in order, for every client whose focused pane is
    /// `pane_id`, on this machine and on other machines alike, as
    /// [`queue_host_write`](Self::queue_host_write) states. A pane that no
    /// client focuses queues nothing. A copy that a client's waiting host
    /// writes refuse is skipped for that client, and each other client still
    /// takes it.
    pub(crate) fn copy_pane_clipboard_to_clients(
        &mut self,
        pane_id: PaneId,
        clipboard_copies: &[ClipboardCopy],
    ) {
        let focusing_clients = self.list_clients_focusing_pane(pane_id);
        if focusing_clients.is_empty() {
            return;
        }
        for clipboard_copy in clipboard_copies {
            let host_write = HostWrite::from_clipboard_copy(
                &clipboard_copy.clipboard_targets,
                &clipboard_copy.copied_bytes,
            );
            for &(client_id, _) in &focusing_clients {
                self.queue_host_write(client_id, &host_write);
            }
        }
    }

    /// Route `copied_bytes`, a copy for `clipboard_targets` that a stand-in
    /// clipboard tool in `pane_id` hands the session, to the clients on other
    /// machines whose focused pane is `pane_id`, and return whether the copy
    /// was routed: at least one client focuses `pane_id`, every one of them is
    /// on another machine, and each one took the copy. The tool runs the real
    /// clipboard tool with a copy that was not routed.
    ///
    /// `clipboard_targets` passes through [`filter_clipboard_targets`]. Each
    /// client that takes the copy gets it as an OSC 52 host write
    /// ([`queue_host_write`](Self::queue_host_write)).
    ///
    /// The copy is not routed when:
    ///
    /// - `pane-clipboard-writes` does not allow writes, or the copy is longer
    ///   than its `max-copy-bytes`
    ///   ([`can_take_clipboard_tool_copy`](Self::can_take_clipboard_tool_copy)).
    ///   No client takes it.
    /// - no client focuses `pane_id`, which a pane this session does not hold
    ///   matches too. No client takes it.
    /// - a client on this machine focuses `pane_id`. Each client on another
    ///   machine that focuses `pane_id` still takes it.
    /// - a client on another machine that focuses `pane_id` has no subscriber,
    ///   its waiting host writes refuse the copy, or the wake finds its
    ///   receiver gone, which a client whose connection's writing thread has
    ///   ended matches ([`queue_host_write`](Self::queue_host_write)). Each
    ///   other client on another machine that focuses `pane_id` still takes
    ///   it.
    ///
    /// Example: two clients on other machines focus `pane_id`, and one of them
    /// has no subscriber. The other one takes the copy, and the copy is not
    /// routed.
    ///
    /// A client on another machine whose link dropped takes the copy while its
    /// connection's writing thread still runs, and its terminal does not get
    /// the copy. Example: when that client is the only one that focuses
    /// `pane_id`, the copy is routed and reaches no clipboard.
    pub(crate) fn route_clipboard_tool_copy(
        &mut self,
        pane_id: PaneId,
        clipboard_targets: &str,
        copied_bytes: &[u8],
    ) -> bool {
        if !self.can_take_clipboard_tool_copy(copied_bytes.len()) {
            return false;
        }
        let focusing_clients = self.list_clients_focusing_pane(pane_id);
        let mut is_routed = !focusing_clients.is_empty();
        let mut host_write: Option<HostWrite> = None;
        for (client_id, client_origin) in focusing_clients {
            if client_origin == ClientOrigin::Local {
                is_routed = false;
                continue;
            }
            let host_write = host_write.get_or_insert_with(|| {
                HostWrite::from_clipboard_copy(
                    &filter_clipboard_targets(clipboard_targets.as_bytes()),
                    copied_bytes,
                )
            });
            if !self.queue_host_write(client_id, host_write) {
                is_routed = false;
            }
        }
        is_routed
    }

    /// Whether a client takes a stand-in clipboard tool copy of
    /// `copied_byte_count` bytes from `pane_id` when
    /// [`route_clipboard_tool_copy`](Self::route_clipboard_tool_copy) routes
    /// it: the session takes the copy
    /// ([`can_take_clipboard_tool_copy`](Self::can_take_clipboard_tool_copy)),
    /// and a client on another machine focuses `pane_id`.
    ///
    /// Example: one client on this machine and one on another machine focus
    /// `pane_id`, and the copy fits `max-copy-bytes`: `true`.
    pub(crate) fn can_route_clipboard_tool_copy(
        &self,
        pane_id: PaneId,
        copied_byte_count: usize,
    ) -> bool {
        self.can_take_clipboard_tool_copy(copied_byte_count)
            && self
                .list_clients_focusing_pane(pane_id)
                .iter()
                .any(|&(_, client_origin)| client_origin != ClientOrigin::Local)
    }

    /// Whether the session takes a stand-in clipboard tool copy of
    /// `copied_byte_count` bytes: `pane-clipboard-writes` allows writes, and
    /// `copied_byte_count` is at most its `max-copy-bytes`.
    pub(crate) fn can_take_clipboard_tool_copy(&self, copied_byte_count: usize) -> bool {
        let pane_clipboard_writes = self.config.pane_clipboard_writes;
        pane_clipboard_writes.should_allow_writes
            && copied_byte_count <= pane_clipboard_writes.maximum_copy_byte_count
    }

    /// Every attached client whose focused pane is `pane_id`, with where it
    /// connected from. The clients of one session come in client-id order.
    fn list_clients_focusing_pane(&self, pane_id: PaneId) -> Vec<(ClientId, ClientOrigin)> {
        self.session_by_id
            .values()
            .flat_map(|session| session.clients.list_attached_clients())
            .filter(|client| client.get_active_focused_pane_id() == Some(pane_id))
            .map(|client| (client.get_client_id(), client.get_origin()))
            .collect()
    }
}

/// The bytes a paste writes into a pane's PTY: `pasted_text` with every line break as
/// a carriage return, the byte the Enter key sends. `is_bracketed_paste_enabled` true wraps the
/// payload in the bracketed-paste markers `ESC [ 200 ~` … `ESC [ 201 ~`; the
/// caller sets it when the pane turned that mode on.
///
/// Every line-break spelling becomes ONE return: `\r\n`, `\n` and `\r` each
/// leave a single `\r`. `"a\r\nb"` gives `a\rb` unbracketed, and
/// `\x1b[200~a\rb\x1b[201~` bracketed. Every other byte of `pasted_text` reaches the
/// PTY unchanged, an `ESC [ 201 ~` spelled inside `pasted_text` included.
#[must_use]
pub(crate) fn build_paste_bytes(pasted_text: &str, is_bracketed_paste_enabled: bool) -> Vec<u8> {
    let normalized_paste_text = pasted_text.replace("\r\n", "\r").replace('\n', "\r");
    let mut paste_output_bytes = Vec::with_capacity(normalized_paste_text.len() + 12);
    if is_bracketed_paste_enabled {
        paste_output_bytes.extend_from_slice(b"\x1b[200~");
    }
    paste_output_bytes.extend_from_slice(normalized_paste_text.as_bytes());
    if is_bracketed_paste_enabled {
        paste_output_bytes.extend_from_slice(b"\x1b[201~");
    }
    paste_output_bytes
}

#[cfg(test)]
mod tests;
