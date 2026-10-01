//! Renderers for the `share` answers: the grant block printed once after a
//! token is handed out, the block naming what a fresh grant can reach, the
//! lines naming each grant a revoke stopped, the warning a `share revoke
//! --session` asks before it stops anything, and the listing of every grant
//! this machine has made.

use std::net::SocketAddr;

use super::*;
use koshi_core::text::sanitize_reported_text;
use koshi_host::host_addresses::HostAddress;
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
        /// The IP address and port that could not be taken.
        remote_listen_address: SocketAddr,
    },
    /// Remote access is on, and this is where it serves.
    On {
        /// The IP address and port remote clients are served on.
        remote_listen_address: SocketAddr,
        /// The addresses of this machine in the family of
        /// `remote_listen_address` when it is `0.0.0.0` or `[::]`, in the
        /// order [`list_host_addresses`](koshi_host::host_addresses::list_host_addresses)
        /// gives. Empty for every other address.
        host_addresses: Vec<HostAddress>,
    },
}

/// Render the secret a `share grant` minted: the block printed once, holding
/// the secret itself.
///
/// Carries no connect instructions; those are [`render_remote_ready`].
///
/// `has_replaced_grant` opens the block with the line naming the grant that stopped
/// working.
#[must_use]
pub fn render_share_grant(
    connection_token: &ConnectionToken,
    token_identity: &str,
    scope: &TokenScope,
    has_replaced_grant: bool,
) -> String {
    let mut rendered_output = String::new();
    if has_replaced_grant {
        rendered_output.push_str(&format!(
            "the token {token_identity} already held on {} stopped working.\n",
            format_scope_cell(scope)
        ));
    }
    rendered_output.push_str("anyone holding this token can run anything you can.\n");
    rendered_output.push_str(connection_token.expose_secret());
    rendered_output.push('\n');
    rendered_output
}

/// Render what a fresh grant can reach: the block that follows the secret,
/// once this machine's remote access has answered for itself.
///
/// One block per [`RemoteReady`] case. [`RemoteReady::On`] renders what
/// `render_connect_commands` gives. The secret never appears in it.
#[must_use]
pub fn render_remote_ready(token_identity: &str, remote_ready: &RemoteReady) -> String {
    match remote_ready {
        RemoteReady::NoAddress => "no remote listen address is set; add \
             `remote-listen \"<ip>:<port>\"` to koshi.kdl, then run `koshi share grant` again.\n"
            .to_string(),
        RemoteReady::Off => {
            "remote access stays off; this token cannot be used to connect yet.\n".to_string()
        }
        RemoteReady::Unknown => "this machine's remote access could not be read, so whether this \
             token can connect is unknown; run `koshi share grant` again, or check the reason \
             printed above.\n"
            .to_string(),
        RemoteReady::Blocked {
            remote_listen_address,
        } => format!(
            "remote access is on, and nothing is listening on {remote_listen_address}: another program holds \
             it. Free that address, then run `koshi share grant` again to open the port. This \
             token cannot be used to connect until then.\n"
        ),
        RemoteReady::On {
            remote_listen_address,
            host_addresses,
        } => render_connect_commands(token_identity, *remote_listen_address, host_addresses),
    }
}

/// The line naming where the secret goes, written after the connect commands
/// of every block that names one.
const REMOTE_SECRET_LINE: &str =
    "set KOSHI_REMOTE_SECRET to the secret above, or paste it when asked.\n";

/// Render the commands that connect to this machine's remote listener at
/// `remote_listen_address`. Each command carries `--save-as {token_identity}`
/// when `token_identity` is one word without the `host:port` shape.
///
/// - `0.0.0.0` or `[::]`: one command per entry of `host_addresses`, each
///   ending in `# <interface name>`. No entry gives one command with
///   `<this machine's address>` in place of the address.
/// - A loopback address, such as `127.0.0.1:7654`: no command, and a line
///   saying that no other machine can connect to it.
/// - Any other address, such as `192.168.1.20:7654`: one command with it, and
///   a line saying this machine accepts connections on that address only.
///
/// Example: `0.0.0.0:7654`, `alice`, and `192.168.1.20` on `en0` give
/// `  koshi attach --remote 192.168.1.20:7654 --save-as alice [SESSION]   # en0`.
fn render_connect_commands(
    token_identity: &str,
    remote_listen_address: SocketAddr,
    host_addresses: &[HostAddress],
) -> String {
    let save_as_option = build_save_as_option(token_identity);
    let listen_ip_address = remote_listen_address.ip();
    let listen_port = remote_listen_address.port();
    if listen_ip_address.is_loopback() {
        return format!(
            "remote-listen in koshi.kdl is {remote_listen_address}, a loopback address, so no \
             other machine can connect to it. Set it to an address of this machine, such as \
             0.0.0.0:{listen_port}, then run `koshi share grant` again.\n"
        );
    }
    if !listen_ip_address.is_unspecified() {
        return format!(
            "connect from another machine:\n  \
             koshi attach --remote {remote_listen_address}{save_as_option} [SESSION]\n\
             {REMOTE_SECRET_LINE}\
             this machine accepts connections on {listen_ip_address} only (remote-listen in \
             koshi.kdl); the other machine must connect to that address.\n"
        );
    }
    if host_addresses.is_empty() {
        return format!(
            "this machine has no network address right now. Connect from another machine with:\n  \
             koshi attach --remote <this machine's address>:{listen_port}{save_as_option} [SESSION]\n\
             {REMOTE_SECRET_LINE}"
        );
    }
    let mut rendered_output =
        "connect from another machine, at the address of this one it can reach:\n".to_string();
    for host_address in host_addresses {
        let connect_address = SocketAddr::new(host_address.ip_address, listen_port);
        rendered_output.push_str(&format!(
            "  koshi attach --remote {connect_address}{save_as_option} [SESSION]   # {}\n",
            sanitize_reported_text(&host_address.interface_name)
        ));
    }
    rendered_output.push_str(REMOTE_SECRET_LINE);
    rendered_output
}

/// The ` --save-as <token_identity>` the connect command carries, or an empty string
/// when `token_identity` has the `host:port` shape or is not a single word.
///
/// Example — `alice` gives `" --save-as alice"`; `desk:22` and `ada lovelace`
/// each give `""`.
fn build_save_as_option(token_identity: &str) -> String {
    if koshi_link::remote_client::validate_saved_server_name(token_identity).is_err() {
        return String::new();
    }
    if token_identity.split_whitespace().count() != 1 {
        return String::new();
    }
    format!(" --save-as {token_identity}")
}

/// Render a `share revoke` answer: one line per grant that stopped working,
/// or the one line saying the identity held none.
#[must_use]
pub fn render_share_revoke(scopes: &[TokenScope]) -> String {
    if scopes.is_empty() {
        return "this identity holds no grant.\n".to_string();
    }
    let mut rendered_output = String::new();
    for scope in scopes {
        rendered_output.push_str(&format!(
            "the grant on {} stopped working.\n",
            format_scope_cell(scope)
        ));
    }
    rendered_output
}

/// Render the warning a `share revoke --session` asks before it stops
/// anything, when `token_identity` also holds a host-wide grant.
///
/// `session_scope` is the session the revoke narrowed to. Names the wider grant that
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
pub fn render_revoke_host_wide_warning(token_identity: &str, session_scope: &TokenScope) -> String {
    let session_name = format_scope_cell(session_scope);
    format!(
        "{token_identity} also holds a host-wide grant, which reaches {session_name}.\n\
         stopping the grant on {session_name} alone leaves {token_identity} reaching it through the \
         host-wide one.\n\
         stopping both leaves {token_identity} reaching no session on this machine, not just \
         {session_name}.\n"
    )
}

/// Render a `share list` answer.
#[must_use]
pub fn render_share_list(token_entries: &[TokenEntry], output_format: OutputFormat) -> String {
    match output_format {
        OutputFormat::Json => render_json(&token_entries),
        OutputFormat::Table => render_table(
            SHARE_HEADERS,
            token_entries.iter().map(render_share_row_cells).collect(),
        ),
    }
}

/// Column headers for [`TokenEntry`] listings, matching [`render_share_row_cells`].
const SHARE_HEADERS: &[&str] = &[
    "identity",
    "scope",
    "issued",
    "expires",
    "last_used",
    "revoked",
];

/// One [`TokenEntry`] as table cells, in [`SHARE_HEADERS`] order.
fn render_share_row_cells(token_entry: &TokenEntry) -> Vec<String> {
    vec![
        token_entry.identity.clone(),
        format_scope_cell(&token_entry.scope),
        format_time_cell(token_entry.issued_at),
        format_optional_time_cell(token_entry.expires_at),
        format_optional_time_cell(token_entry.last_used_at),
        format_optional_time_cell(token_entry.revoked_at),
    ]
}

/// A scope as a cell: `host` for every session on this machine, else the id
/// of the one session it reaches.
fn format_scope_cell(scope: &TokenScope) -> String {
    match scope {
        TokenScope::HostWide => "host".to_string(),
        TokenScope::Session(session_id) => session_id.to_string(),
    }
}
