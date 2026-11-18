//! Join and leave notices: the `Info` entry a session posts when a client
//! says that it read its attach answer, or when the session removes a
//! client's record, and the rule that decides whether it posts one.
//!
//! Example: a remote client labelled `bob` with `color_index` `1` attaches
//! and reads its answer, and every other attached client shows
//! `bob (2) joined remotely`.

use koshi_core::client::{compute_client_number, ClientOrigin};
use koshi_session::client::Client;

/// What happened to a client's record that a join or leave notice reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PresenceChange {
    /// A client arrived that the other clients were never told about, neither
    /// as itself nor as an earlier client whose view it took back.
    Joined,
    /// A client took back the view of a client that the other clients were
    /// told arrived.
    Reconnected,
    /// The session removed the record of a client that left: its `Leaving`
    /// request said that it does not attach again, or a detach or quit
    /// command removed it.
    Left,
    /// The session removed the record of a client whose connection ended
    /// without leaving: a client that may attach again, a connection that
    /// ended with no `Leaving`, a client whose token a new attach presented
    /// while the session still held it, or a client that did not come back
    /// before the restart grace window closed.
    Disconnected,
}

/// The text of the notice that reports `presence_change` for `client`, or
/// `None` when no notice posts. `session_clients` are the client records
/// `client`'s session holds after the change, attached or awaiting their
/// re-attach after a restart. `client` itself among them is skipped, and so
/// is a client the session has not announced yet
/// ([`Client::get_unannounced_arrival`]): the rule counts every client the
/// other clients were told about and not yet told is gone.
///
/// A remote client, and a client of another account of this machine, always
/// get a notice. A client of the host account
/// (`ClientOrigin::Local { is_same_user: true }`) gets one only when no other
/// counted client is of the host account and at least one counted client is
/// of another person: the host's first terminal joins, or its last one
/// leaves, while somebody else is attached or on the way back after a
/// restart.
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
/// while `bob` is present → `host (1) left`, and while the session also holds
/// another announced host terminal, attached or awaiting its re-attach →
/// `None`.
pub(crate) fn build_presence_notice<'a>(
    client: &Client,
    session_clients: impl IntoIterator<Item = &'a Client>,
    presence_change: PresenceChange,
) -> Option<String> {
    let client_origin = client.get_origin();
    if client_origin == (ClientOrigin::Local { is_same_user: true }) {
        let mut has_other_person = false;
        for session_client in session_clients {
            let is_announced = session_client.get_unannounced_arrival().is_none();
            if session_client.get_client_id() == client.get_client_id() || !is_announced {
                continue;
            }
            if session_client.get_origin() == (ClientOrigin::Local { is_same_user: true }) {
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
