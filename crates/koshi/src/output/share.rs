//! Renderers for the `share` answers: the grant block printed once after a
//! token is handed out, the block naming what a fresh grant or a `share open`
//! can reach, the lines naming each grant a revoke stopped, the warning a
//! `share revoke --session` asks before it stops anything, and the listing of
//! every grant this machine has made.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use super::*;
use koshi_core::text::sanitize_reported_text;
use koshi_host::host_addresses::HostAddress;
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::remote_tokens::{TokenEntry, TokenScope};
use koshi_ipc::tls::format_certificate_fingerprint;

/// What this machine's remote access leaves a fresh grant able to do, which
/// decides the block a `share grant` and a `share open` close with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteReady {
    /// The running router read no listen address from `koshi.kdl` when it
    /// started, and `koshi.kdl` names none now.
    NoAddress,
    /// The address is set and the operator answered no to switching remote
    /// access on.
    Off,
    /// This machine's remote access could not be read. The reason is written to
    /// stderr as it happens.
    Unknown,
    /// The port is not open.
    Blocked {
        /// The IP address and port that is not open.
        remote_listen_address: SocketAddr,
        /// Whether remote access is switched on.
        is_remote_access_enabled: bool,
        /// The sentence the router refused to open the port with, or `None`
        /// when the router was not asked to open it.
        open_refusal: Option<String>,
    },
    /// The port is not open, and the running router read another listen
    /// address than the one `koshi.kdl` names now.
    ConfigMismatched {
        /// The IP address and port the router read when it started, or `None`
        /// when it read none.
        router_listen_address: Option<SocketAddr>,
        /// Which `koshi.kdl` names what.
        config_mismatch: ConfigMismatch,
    },
    /// Remote access is on, and this is where it serves.
    On {
        /// The IP address and port remote clients are served on.
        remote_listen_address: SocketAddr,
        /// The addresses of this machine in the family of
        /// `remote_listen_address` when it is `0.0.0.0`, `[::ffff:0.0.0.0]` or
        /// `[::]`, in the order
        /// [`list_host_addresses`](koshi_host::host_addresses::list_host_addresses)
        /// gives. `[::ffff:0.0.0.0]` gives the IPv4 addresses. Empty for every
        /// other address.
        host_addresses: Vec<HostAddress>,
        /// The fingerprint of the certificate the remote listener presents,
        /// as 64 lowercase hex characters. The connect commands print it as
        /// `sha256:` followed by those characters.
        certificate_fingerprint: String,
        /// Which `koshi.kdl` names another listen address than
        /// `remote_listen_address`, or `None` when `koshi.kdl` names that one.
        config_mismatch: Option<ConfigMismatch>,
    },
}

/// Why the listen address the running router read when it started is not the
/// one `koshi.kdl` names now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigMismatch {
    /// The running router read `config_file_path` when it started, and that
    /// file now names `configured_address`.
    FileChanged {
        /// The `koshi.kdl` the router read.
        config_file_path: PathBuf,
        /// The listen address that file names now, or `None` when it names
        /// none.
        configured_address: Option<SocketAddr>,
    },
    /// The running router reads another `koshi.kdl` than this koshi, and that
    /// file still names what the router read.
    OtherFile {
        /// The `koshi.kdl` the router reads, or `None` when the router names
        /// none.
        router_config_file_path: Option<PathBuf>,
        /// The `koshi.kdl` this koshi reads, or `None` when this koshi has no
        /// config directory.
        config_file_path: Option<PathBuf>,
        /// The listen address `config_file_path` names, or `None` when it
        /// names none.
        configured_address: Option<SocketAddr>,
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

/// The sentence a grant's block ends with while the port is not open.
const TOKEN_WAITS_FOR_PORT_SENTENCE: &str =
    " This token cannot be used to connect until the port opens.";

/// Render what a fresh grant, or a `share open`, can reach: the block that
/// follows the secret, once this machine's remote access has answered for
/// itself.
///
/// `token_identity` names the grant the block follows, or is `None` for the
/// block `koshi share open` prints. One block per [`RemoteReady`] case.
/// [`RemoteReady::On`] renders what `render_connect_commands` gives, then,
/// when `koshi.kdl` names another address, one line: the facts
/// [`render_config_mismatch_fact`] gives and the step that makes the router
/// read the file again. [`RemoteReady::ConfigMismatched`] renders that line
/// alone, with the step ending in `koshi share open` where opening the port
/// is left to do. The secret never appears in it.
///
/// Example: [`RemoteReady::Blocked`] for `192.0.2.20:7654`, remote access off,
/// and the refusal `the remote listener could not open 192.0.2.20:7654:
/// Address already in use`, after a grant to `alice`, renders `remote access
/// stays off, and the port did not open: the remote listener could not open
/// 192.0.2.20:7654: Address already in use. Run `koshi share open` once that is
/// fixed. This token cannot be used to connect until the port opens.`
#[must_use]
pub fn render_remote_ready(token_identity: Option<&str>, remote_ready: &RemoteReady) -> String {
    let token_sentence = match token_identity {
        Some(_) => TOKEN_WAITS_FOR_PORT_SENTENCE,
        None => "",
    };
    match remote_ready {
        RemoteReady::NoAddress => match token_identity {
            Some(_) => "koshi.kdl names no remote listen address, so this token cannot be used to \
                        connect yet. Set `remote-listen \"<ip>:<port>\"` in koshi.kdl, run `koshi \
                        restart-servers`, then run `koshi share open`.\n"
                .to_string(),
            None => "koshi.kdl names no remote listen address. Set `remote-listen \
                     \"<ip>:<port>\"` in koshi.kdl, run `koshi restart-servers`, then run `koshi \
                     share open` again.\n"
                .to_string(),
        },
        RemoteReady::Off => format!(
            "remote access stays off. Run `koshi share open` to open the port.{token_sentence}\n"
        ),
        RemoteReady::Unknown => match token_identity {
            Some(_) => "this machine's remote access could not be read, so whether this token \
                        can connect is unknown; check the reason printed above, then run `koshi \
                        share open`, which prints how to connect.\n"
                .to_string(),
            None => "this machine's remote access could not be read; check the reason printed \
                     above, then run `koshi share open` again.\n"
                .to_string(),
        },
        RemoteReady::Blocked {
            remote_listen_address,
            is_remote_access_enabled,
            open_refusal,
        } => {
            let remote_access_state = if *is_remote_access_enabled {
                "remote access is on"
            } else {
                "remote access stays off"
            };
            match open_refusal {
                Some(open_refusal) => format!(
                    "{remote_access_state}, and the port did not open: {open_refusal}. Run `koshi \
                     share open` once that is fixed.{token_sentence}\n"
                ),
                None => format!(
                    "{remote_access_state}, and nothing is listening on {remote_listen_address}: \
                     the router could not open it when it started. Run `koshi share open` to try \
                     again.{token_sentence}\n"
                ),
            }
        }
        RemoteReady::ConfigMismatched {
            router_listen_address,
            config_mismatch,
        } => format!(
            "{}. {}.{token_sentence}\n",
            render_config_mismatch_fact(config_mismatch, *router_listen_address),
            render_config_mismatch_step(config_mismatch, false)
        ),
        RemoteReady::On {
            remote_listen_address,
            host_addresses,
            certificate_fingerprint,
            config_mismatch,
        } => {
            let mut rendered_output = render_connect_commands(
                token_identity,
                *remote_listen_address,
                host_addresses,
                certificate_fingerprint,
            );
            if let Some(config_mismatch) = config_mismatch {
                rendered_output.push_str(&format!(
                    "{}. {}.\n",
                    render_config_mismatch_fact(config_mismatch, Some(*remote_listen_address)),
                    render_config_mismatch_step(config_mismatch, true)
                ));
            }
            rendered_output
        }
    }
}

/// `the remote listen address <address>` for `Some`, `no remote listen
/// address` for `None`.
fn describe_listen_address(remote_listen_address: Option<SocketAddr>) -> String {
    match remote_listen_address {
        Some(remote_listen_address) => format!("the remote listen address {remote_listen_address}"),
        None => "no remote listen address".to_string(),
    }
}

/// `<path>, which names <address>` for a `koshi.kdl` this koshi reads, or `no
/// koshi.kdl` when `config_file_path` is `None`.
fn describe_config_file(
    config_file_path: Option<&Path>,
    configured_address: Option<SocketAddr>,
) -> String {
    match config_file_path {
        Some(config_file_path) => format!(
            "{}, which names {}",
            config_file_path.display(),
            describe_listen_address(configured_address)
        ),
        None => "no koshi.kdl".to_string(),
    }
}

/// The facts behind `config_mismatch`, with no step and no closing period.
/// `router_listen_address` is the address the running router read when it
/// started.
///
/// - [`ConfigMismatch::FileChanged`]: `<path> names <configured>, and the
///   running router read <router address> from it when it started`.
/// - [`ConfigMismatch::OtherFile`] naming the router's file: `the running
///   router reads <router path>, which names <router address>, and this koshi
///   reads <path>, which names <configured>`.
/// - [`ConfigMismatch::OtherFile`] naming no router file: `the running router
///   does not name the koshi.kdl it reads, and read <router address> when it
///   started; this koshi reads <path>, which names <configured>`.
///
/// Example: `FileChanged` for `/home/user/.config/koshi/koshi.kdl` naming
/// `127.0.0.1:7654`, and a router that read none, gives
/// `/home/user/.config/koshi/koshi.kdl names the remote listen address
/// 127.0.0.1:7654, and the running router read no remote listen address from
/// it when it started`.
#[must_use]
pub fn render_config_mismatch_fact(
    config_mismatch: &ConfigMismatch,
    router_listen_address: Option<SocketAddr>,
) -> String {
    let router_address_description = describe_listen_address(router_listen_address);
    match config_mismatch {
        ConfigMismatch::FileChanged {
            config_file_path,
            configured_address,
        } => format!(
            "{} names {}, and the running router read {router_address_description} from it when \
             it started",
            config_file_path.display(),
            describe_listen_address(*configured_address)
        ),
        ConfigMismatch::OtherFile {
            router_config_file_path: Some(router_config_file_path),
            config_file_path,
            configured_address,
        } => format!(
            "the running router reads {}, which names {router_address_description}, and this koshi \
             reads {}",
            router_config_file_path.display(),
            describe_config_file(config_file_path.as_deref(), *configured_address)
        ),
        ConfigMismatch::OtherFile {
            router_config_file_path: None,
            config_file_path,
            configured_address,
        } => format!(
            "the running router does not name the koshi.kdl it reads, and read \
             {router_address_description} when it started; this koshi reads {}",
            describe_config_file(config_file_path.as_deref(), *configured_address)
        ),
    }
}

/// The step that makes the running router serve what `koshi.kdl` names, with
/// no closing period. `is_listening` says whether the router holds the port: a
/// router that holds it opens it again on its next start, and the step then
/// ends at the restart.
///
/// - [`ConfigMismatch::FileChanged`]: `Run `koshi restart-servers` so the
///   router reads it again`, then `, then run `koshi share open`` while the
///   port is not open and the file names an address.
/// - [`ConfigMismatch::OtherFile`] naming the router's file: `Set
///   remote-listen in <router path>, then run `koshi restart-servers``, then
///   `, then run `koshi share open`` while the port is not open.
/// - [`ConfigMismatch::OtherFile`] naming no router file: `Run `koshi
///   restart-servers``, then `, then run `koshi share open`` while the port is
///   not open.
fn render_config_mismatch_step(config_mismatch: &ConfigMismatch, is_listening: bool) -> String {
    let (restart_step, can_open_after_restart) = match config_mismatch {
        ConfigMismatch::FileChanged {
            configured_address, ..
        } => (
            "Run `koshi restart-servers` so the router reads it again".to_string(),
            configured_address.is_some(),
        ),
        ConfigMismatch::OtherFile {
            router_config_file_path: Some(router_config_file_path),
            ..
        } => (
            format!(
                "Set remote-listen in {}, then run `koshi restart-servers`",
                router_config_file_path.display()
            ),
            true,
        ),
        ConfigMismatch::OtherFile {
            router_config_file_path: None,
            ..
        } => ("Run `koshi restart-servers`".to_string(), true),
    };
    if is_listening || !can_open_after_restart {
        return restart_step;
    }
    format!("{restart_step}, then run `koshi share open`")
}

/// The line naming where the secret goes after a grant, written after the
/// connect commands of every block that names one.
const REMOTE_SECRET_LINE: &str =
    "set KOSHI_REMOTE_SECRET to the secret above, or paste it when asked.\n";

/// The line naming where the secret goes after a `share open`, written after
/// the connect commands of every block that names one.
const GRANTED_SECRET_LINE: &str =
    "set KOSHI_REMOTE_SECRET to the secret `koshi share grant` printed, or paste it when asked.\n";

/// Render the commands that connect to this machine's remote listener at
/// `remote_listen_address`. Each command carries
/// `--certificate-fingerprint sha256:{certificate_fingerprint}`, and
/// `--save-as {token_identity}` when `token_identity` is one word without the
/// `host:port` shape. A `token_identity` of `None`, for `koshi share open`,
/// gives no `--save-as`, and the secret line names the secret `koshi share
/// grant` printed in place of the secret above. An IPv4-mapped IPv6 address,
/// such as `[::ffff:192.0.2.20]`, is read as its IPv4 form, `192.0.2.20`, and
/// a command names that form.
///
/// - `0.0.0.0` or `[::]`: one command per entry of `host_addresses`, each
///   ending in `# <interface name>`. No entry gives one command with
///   `<this machine's address>` in place of the address.
/// - A loopback address, such as `127.0.0.1:7654`: one command with it,
///   under `connect from this machine:`, and a line saying that no other
///   machine can connect to it and naming `0.0.0.0:<port>` as a
///   `remote-listen` value that lets another machine connect after `koshi
///   restart-servers`.
/// - Any other address, such as `192.0.2.20:7654`: one command with it, and
///   a line saying this machine accepts connections on that address only.
///
/// Example: `0.0.0.0:7654`, `ada`, `192.0.2.20` on `en0`, and the fingerprint
/// `9c1e` give one line: two spaces, this command, three spaces, and `# en0`:
///
/// `koshi attach --remote 192.0.2.20:7654 --certificate-fingerprint sha256:9c1e --save-as ada [SESSION]`
fn render_connect_commands(
    token_identity: Option<&str>,
    remote_listen_address: SocketAddr,
    host_addresses: &[HostAddress],
    certificate_fingerprint: &str,
) -> String {
    let (save_as_option, secret_line) = match token_identity {
        Some(token_identity) => (build_save_as_option(token_identity), REMOTE_SECRET_LINE),
        None => (String::new(), GRANTED_SECRET_LINE),
    };
    let connect_options = format!(
        " --certificate-fingerprint {}{save_as_option}",
        format_certificate_fingerprint(certificate_fingerprint)
    );
    let listen_ip_address = remote_listen_address.ip().to_canonical();
    let listen_port = remote_listen_address.port();
    if !listen_ip_address.is_unspecified() {
        let connect_command = format!(
            "  koshi attach --remote {}{connect_options} [SESSION]\n",
            SocketAddr::new(listen_ip_address, listen_port)
        );
        if listen_ip_address.is_loopback() {
            return format!(
                "connect from this machine:\n\
                 {connect_command}\
                 {secret_line}\
                 the router listens on {remote_listen_address}, a loopback address, so no other \
                 machine can connect to it. To let another machine connect, set remote-listen in \
                 koshi.kdl to an address of this machine, such as 0.0.0.0:{listen_port}, then run \
                 `koshi restart-servers`.\n"
            );
        }
        return format!(
            "connect from another machine:\n\
             {connect_command}\
             {secret_line}\
             this machine accepts connections on {listen_ip_address} only; the other machine \
             must connect to that address.\n"
        );
    }
    if host_addresses.is_empty() {
        return format!(
            "this machine has no network address right now. Connect from another machine with:\n  \
             koshi attach --remote <this machine's address>:{listen_port}{connect_options} [SESSION]\n\
             {secret_line}"
        );
    }
    let mut rendered_output =
        "connect from another machine, at the address of this one it can reach:\n".to_string();
    for host_address in host_addresses {
        let connect_address = SocketAddr::new(host_address.ip_address, listen_port);
        rendered_output.push_str(&format!(
            "  koshi attach --remote {connect_address}{connect_options} [SESSION]   # {}\n",
            sanitize_reported_text(&host_address.interface_name)
        ));
    }
    rendered_output.push_str(secret_line);
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
    let grant_rows: Vec<GrantRow> = token_entries.iter().map(build_grant_row).collect();
    match output_format {
        OutputFormat::Json => render_json(&grant_rows),
        OutputFormat::Table => render_table(
            SHARE_HEADERS,
            grant_rows.iter().map(render_share_row_cells).collect(),
        ),
    }
}

/// One grant as both formats of `share list` print it.
#[derive(Serialize)]
struct GrantRow {
    /// Who the grant was handed to.
    identity: String,
    /// How far the grant reaches, as [`format_scope_cell`] names it.
    scope: String,
    /// When the grant was made, as [`format_utc_timestamp`] writes it.
    issued_at: String,
    /// When the grant stops working on its own, or `None` when it never does.
    expires_at: Option<String>,
    /// When a presented secret last reached a session through the grant, or
    /// `None` when none ever has.
    last_used_at: Option<String>,
    /// When an operator stopped the grant, or `None` while it still stands.
    revoked_at: Option<String>,
}

/// One [`TokenEntry`] as both formats print it, every time written by
/// [`format_utc_timestamp`].
fn build_grant_row(token_entry: &TokenEntry) -> GrantRow {
    GrantRow {
        identity: token_entry.identity.clone(),
        scope: format_scope_cell(&token_entry.scope),
        issued_at: format_utc_timestamp(token_entry.issued_at),
        expires_at: token_entry.expires_at.map(format_utc_timestamp),
        last_used_at: token_entry.last_used_at.map(format_utc_timestamp),
        revoked_at: token_entry.revoked_at.map(format_utc_timestamp),
    }
}

/// Column headers for [`GrantRow`] listings, matching [`render_share_row_cells`].
const SHARE_HEADERS: &[&str] = &[
    "identity",
    "scope",
    "issued",
    "expires",
    "last_used",
    "revoked",
];

/// One [`GrantRow`] as table cells, in [`SHARE_HEADERS`] order.
fn render_share_row_cells(grant_row: &GrantRow) -> Vec<String> {
    vec![
        grant_row.identity.clone(),
        grant_row.scope.clone(),
        grant_row.issued_at.clone(),
        format_optional_cell(grant_row.expires_at.as_ref()),
        format_optional_cell(grant_row.last_used_at.as_ref()),
        format_optional_cell(grant_row.revoked_at.as_ref()),
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
