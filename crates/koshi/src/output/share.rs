//! Renderers for the three `share` answers: the grant block printed once
//! after a token is handed out, the lines naming each grant a revoke stopped,
//! and the listing of every grant this machine has made.

use super::*;
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::remote_tokens::{TokenEntry, TokenScope};

/// What this machine's remote access leaves a fresh grant able to do, which
/// decides the block a `share grant` closes with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteReady {
    /// `koshi.kdl` names no address to serve remote clients on.
    NoAddress,
    /// The address is set and the operator left remote access switched off.
    Off,
    /// Remote access is on, and this is where it serves.
    On {
        /// Where remote clients are served, as `host:port`.
        address: String,
    },
}

/// Render a `share grant` answer: the block printed once, holding the secret
/// itself.
///
/// `replaced` opens the block with the grant that stopped working, so the
/// operator learns the old token is dead before reading the new one.
///
/// `ready` closes the block: with remote access on, the command that connects
/// from another machine, and otherwise what is missing before this token can
/// connect at all. The secret never appears inside that command.
#[must_use]
pub fn render_share_grant(
    token: &ConnectionToken,
    identity: &str,
    scope: &TokenScope,
    replaced: bool,
    ready: &RemoteReady,
) -> String {
    let mut rendered = String::new();
    if replaced {
        rendered.push_str(&format!(
            "the token {identity} already held on {} stopped working.\n",
            scope_cell(scope)
        ));
    }
    rendered.push_str("anyone holding this token can run anything you can.\n");
    rendered.push_str(token.expose());
    rendered.push('\n');
    match ready {
        RemoteReady::NoAddress => rendered.push_str(
            "no remote listen address is set; add `remote-listen \"<host:port>\"` to koshi.kdl, \
             then run `koshi share grant` again.\n",
        ),
        RemoteReady::Off => rendered
            .push_str("remote access stays off; this token cannot be used to connect yet.\n"),
        RemoteReady::On { address } => {
            rendered.push_str("connect from another machine:\n");
            rendered.push_str(&format!(
                "  koshi attach --remote {address} --save-as {identity} [SESSION]\n"
            ));
            rendered
                .push_str("set KOSHI_REMOTE_SECRET to the secret above, or paste it when asked.\n");
        }
    }
    rendered
}

/// Render a `share revoke` answer: one line per grant that stopped working,
/// or the one line saying the identity held none.
#[must_use]
pub fn render_share_revoke(scopes: &[TokenScope]) -> String {
    if scopes.is_empty() {
        return "this identity holds no grant.\n".to_string();
    }
    let mut rendered = String::new();
    for scope in scopes {
        rendered.push_str(&format!(
            "the grant on {} stopped working.\n",
            scope_cell(scope)
        ));
    }
    rendered
}

/// Render a `share list` answer.
#[must_use]
pub fn render_share_list(entries: &[TokenEntry], format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(&entries),
        FormatArg::Table => table(SHARE_HEADERS, entries.iter().map(share_row_cells).collect()),
    }
}

/// Column headers for [`TokenEntry`] listings, matching [`share_row_cells`].
const SHARE_HEADERS: &[&str] = &[
    "identity",
    "scope",
    "issued",
    "expires",
    "last_used",
    "revoked",
];

/// One [`TokenEntry`] as table cells, in [`SHARE_HEADERS`] order.
fn share_row_cells(entry: &TokenEntry) -> Vec<String> {
    vec![
        entry.identity.clone(),
        scope_cell(&entry.scope),
        time_cell(entry.issued_at),
        optional_time_cell(entry.expires_at),
        optional_time_cell(entry.last_used_at),
        optional_time_cell(entry.revoked_at),
    ]
}

/// A scope as a cell: `host` for every session on this machine, else the id
/// of the one session it reaches.
fn scope_cell(scope: &TokenScope) -> String {
    match scope {
        TokenScope::HostWide => "host".to_string(),
        TokenScope::Session(id) => id.to_string(),
    }
}

/// A time that may be absent as a cell, absent printing as `-`.
fn optional_time_cell(time: Option<SystemTime>) -> String {
    time.map_or_else(|| "-".to_string(), time_cell)
}
