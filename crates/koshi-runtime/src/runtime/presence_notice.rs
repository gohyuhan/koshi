//! Join and leave notices: the `Info` entry a session posts when it creates
//! or removes a client's record, and the rule that decides whether it posts
//! one.
//!
//! Example: a remote client labelled `bob` with `color_index` `1` attaches,
//! and every other attached client shows `bob (2) joined remotely`.

use koshi_core::client::{compute_client_number, ClientOrigin};
use koshi_session::client::Client;

/// What happened to a client's record that a join or leave notice reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PresenceChange {
    /// The session created the record for a client that took back no saved
    /// view.
    Joined,
    /// The session created the record for a client that took back a saved
    /// view.
    Reconnected,
    /// The session removed the record of a client that does not attach again.
    Left,
    /// The session removed the record of a client that may attach again.
    Disconnected,
}

/// The text of the notice that reports `presence_change` for `client`, or
/// `None` when no notice posts. `present_clients` are the clients of
/// `client`'s session that frames list as peers after the change; `client`
/// itself among them is skipped.
///
/// A remote client, and a client of another account of this machine, always
/// get a notice. A client of the host account
/// (`ClientOrigin::Local { is_same_user: true }`) gets one only when no other
/// present client is of the host account and at least one present client is
/// of another person: the host's first terminal joins, or its last one
/// leaves, while somebody else is attached.
///
/// The text is the label, the client number in brackets
/// ([`compute_client_number`]), and the words of the change:
///
/// - `Joined`: `joined remotely` for a remote client, `joined from another
///   account on this machine` for another account, and `joined` for the host
///   account;
/// - `Reconnected`, `Left` and `Disconnected`: `reconnected`, `left` and
///   `disconnected`.
///
/// Example: remote `bob` at `color_index` `1` that joined →
/// `bob (2) joined remotely`; the host's terminal `host` at `0` that left
/// while `bob` is present → `host (1) left`, and while only another host
/// terminal is present → `None`.
pub(crate) fn build_presence_notice<'a>(
    client: &Client,
    present_clients: impl IntoIterator<Item = &'a Client>,
    presence_change: PresenceChange,
) -> Option<String> {
    let client_origin = client.get_origin();
    if client_origin == (ClientOrigin::Local { is_same_user: true }) {
        let mut has_other_person = false;
        for present_client in present_clients {
            if present_client.get_client_id() == client.get_client_id() {
                continue;
            }
            if present_client.get_origin() == (ClientOrigin::Local { is_same_user: true }) {
                return None;
            }
            has_other_person = true;
        }
        if !has_other_person {
            return None;
        }
    }
    let change_words = match (presence_change, client_origin) {
        (PresenceChange::Joined, ClientOrigin::Remote) => "joined remotely",
        (
            PresenceChange::Joined,
            ClientOrigin::Local {
                is_same_user: false,
            },
        ) => "joined from another account on this machine",
        (PresenceChange::Joined, ClientOrigin::Local { is_same_user: true }) => "joined",
        (PresenceChange::Reconnected, _) => "reconnected",
        (PresenceChange::Left, _) => "left",
        (PresenceChange::Disconnected, _) => "disconnected",
    };
    Some(format!(
        "{} ({}) {change_words}",
        client.get_label(),
        compute_client_number(client.get_color_index())
    ))
}

#[cfg(test)]
mod tests;
