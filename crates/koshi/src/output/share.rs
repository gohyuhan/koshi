//! Renderers for the `share` answers: the grant block printed once after a
//! token is handed out, the block naming what a fresh grant can reach, the
//! lines naming each grant a revoke stopped, the warning a `share revoke
//! --session` asks before it stops anything, and the listing of every grant
//! this machine has made.

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
    /// This machine's remote access could not be read. The reason is written to
    /// stderr as it happens.
    Unknown,
    /// Remote access is switched on and the port is not open.
    Blocked {
        /// The address that could not be taken, as `host:port`.
        address: String,
    },
    /// Remote access is on, and this is where it serves.
    On {
        /// Where remote clients are served, as `host:port`.
        address: String,
    },
}

/// Render the secret a `share grant` minted: the block printed once, holding
/// the secret itself.
///
/// Carries no connect instructions; those are [`render_remote_ready`].
///
/// `replaced` opens the block with the line naming the grant that stopped
/// working.
#[must_use]
pub fn render_share_grant(
    token: &ConnectionToken,
    identity: &str,
    scope: &TokenScope,
    replaced: bool,
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
    rendered
}

/// Render what a fresh grant can reach: the block that follows the secret,
/// once this machine's remote access has answered for itself.
///
/// One block per [`RemoteReady`] case. [`RemoteReady::On`] renders the command
/// that connects, carrying the address, and `--save-as {identity}` when
/// `identity` is one word without the `host:port` shape. The secret never
/// appears in it.
#[must_use]
pub fn render_remote_ready(identity: &str, ready: &RemoteReady) -> String {
    match ready {
        RemoteReady::NoAddress => "no remote listen address is set; add \
             `remote-listen \"<host:port>\"` to koshi.kdl, then run `koshi share grant` again.\n"
            .to_string(),
        RemoteReady::Off => {
            "remote access stays off; this token cannot be used to connect yet.\n".to_string()
        }
        RemoteReady::Unknown => "this machine's remote access could not be read, so whether this \
             token can connect is unknown; run `koshi share grant` again, or check the reason \
             printed above.\n"
            .to_string(),
        RemoteReady::Blocked { address } => format!(
            "remote access is on, and nothing is listening on {address}: another program holds \
             it. Free that address, then run `koshi share grant` again to open the port. This \
             token cannot be used to connect until then.\n"
        ),
        RemoteReady::On { address } => format!(
            "connect from another machine:\n  \
             koshi attach --remote {address}{} [SESSION]\n\
             set KOSHI_REMOTE_SECRET to the secret above, or paste it when asked.\n",
            save_as_offer(identity)
        ),
    }
}

/// The ` --save-as <identity>` the connect command carries, or an empty string
/// when `identity` has the `host:port` shape or is not a single word.
///
/// Example — `alice` gives `" --save-as alice"`; `desk:22` and `ada lovelace`
/// each give `""`.
fn save_as_offer(identity: &str) -> String {
    if koshi_link::remote_client::check_name_shape(identity).is_err() {
        return String::new();
    }
    if identity.split_whitespace().count() != 1 {
        return String::new();
    }
    format!(" --save-as {identity}")
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

/// Render the warning a `share revoke --session` asks before it stops
/// anything, when `identity` also holds a host-wide grant.
///
/// `session` is the session the revoke narrowed to. Names the wider grant that
/// reaches it, and what stopping both costs: a host-wide grant reaches every
/// session on this machine, so stopping it stops them all.
///
/// Example — `alice` and session `quiet-lake` render:
///
/// ```text
/// alice also holds a host-wide grant, which reaches quiet-lake.
/// stopping the grant on quiet-lake alone leaves alice reaching it through the
/// host-wide one.
/// stopping both leaves alice reaching no session on this machine, not just
/// quiet-lake.
/// ```
#[must_use]
pub fn render_revoke_host_wide_warning(identity: &str, session: &TokenScope) -> String {
    let session = scope_cell(session);
    format!(
        "{identity} also holds a host-wide grant, which reaches {session}.\n\
         stopping the grant on {session} alone leaves {identity} reaching it through the \
         host-wide one.\n\
         stopping both leaves {identity} reaching no session on this machine, not just \
         {session}.\n"
    )
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
