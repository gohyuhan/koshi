//! The dialling side of remote access: reach the sessions on another
//! machine.
//!
//! A user names a server either by the address it listens on, `host:port`, or
//! by the name they gave it when they first connected.
//! [`resolve_server`] turns what they
//! typed into one of the two, reading the saved-server store on this machine.
//!
//! The secret from a grant never arrives as a command-line argument.
//! [`resolve_server_connection_token`] reads it from
//! `KOSHI_REMOTE_SECRET`, or asks for it at the terminal without printing
//! what is typed.
//!
//! [`connect_remote_server`] opens the TLS stream, presents
//! the secret, and hands back a
//! [`RemoteLink`] once the server answers
//! `Welcome`. [`connect_saved_server`] wraps
//! that with the store: a server reached for the first time is saved with the
//! fingerprint it presented, and a saved one has its last-used time stamped.
//!
//! From an open link a caller lists the sessions the secret reaches, attaches
//! to one, submits one command to one, or asks one to describe itself.
//! [`reach_all_saved_servers`] asks every saved server at
//! once and returns inside one deadline, whatever the servers do.
//!
//! Every TLS and remote-frame detail stays inside this module. Callers name
//! sessions, secrets and addresses, and never a certificate or a frame.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use fs4::{FileExt, TryLockError};

use koshi_core::command::{Command, CommandEnvelope, CommandResult, CommandSource};
use koshi_core::discovery::SessionOverview;
use koshi_core::ids::{ClientId, CommandId, SessionId};
use koshi_core::text::sanitize_reported_text;
use koshi_ipc::error::IpcError;
use koshi_ipc::protocol::{
    ConnectionToken, IncomingResponse, IpcRequest, IpcRequestKind, IpcResult, MIN_PROTOCOL_VERSION,
    PROTOCOL_VERSION,
};
use koshi_ipc::remote_servers::{
    resolve_server_store_lock_path, resolve_server_store_path, SavedServer, SavedServerLookup,
    ServerStore,
};
use koshi_ipc::remote_wire::{
    self, RemoteClientFrame, RemoteServerFrame, RemoteSessionRow, MIN_REMOTE_PROTOCOL_VERSION,
    REMOTE_PROTOCOL_VERSION,
};
use koshi_ipc::router::SessionSelector;
use koshi_ipc::transport::{FrameReader, FrameWriter};

use crate::error::CliError;
use crate::talk::{self, build_ipc_unavailable_error, build_peer_refusal_error};

/// The environment variable holding the secret from a grant, read before the
/// terminal is asked for one.
const SECRET_ENVIRONMENT_VARIABLE: &str = "KOSHI_REMOTE_SECRET";

/// How long one dial has to open: the name lookup aside, the connect, the TLS
/// handshake and the secret exchange share it.
pub const DIAL_TIMEOUT_DURATION: Duration = Duration::from_secs(10);

/// How long the frames that join a client to a session on another machine have
/// to arrive: the Attach, the session's Hello carried back through the bridge,
/// and the answer that names the client.
///
/// The deadline is taken off once the client is joined.
pub const JOIN_TIMEOUT_DURATION: Duration = Duration::from_secs(20);

/// How long one command sent to a session on another machine has to come back,
/// counted from the moment the connection opens.
///
/// The dial before it has [`DIAL_TIMEOUT_DURATION`] of its own, so one request takes at
/// most `DIAL_TIMEOUT_DURATION + REPLY_TIMEOUT_DURATION`. An attached client passes `None` instead and
/// waits as long as it takes.
pub const REPLY_TIMEOUT_DURATION: Duration = Duration::from_secs(20);

/// How many saved servers [`reach_all_saved_servers`] asks at once, one thread each. Records
/// past this count are not asked; [`reach_all_saved_servers`] names how many on stderr.
const MAX_CONCURRENT_REACH_COUNT: usize = 16;

/// How long [`reach_all_saved_servers`] waits for every saved server together, one deadline
/// over the whole sweep.
pub const REACH_TIMEOUT_DURATION: Duration = Duration::from_secs(2);

/// How long a change to the saved-server store waits for another koshi to
/// finish its own change before it gives up. The operating system releases the
/// lock if that koshi dies.
const STORE_LOCK_TIMEOUT_DURATION: Duration = Duration::from_secs(5);

/// How long the wait for the saved-server store pauses between attempts on the
/// lock.
const STORE_LOCK_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(20);

/// Which server an invocation talks to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerReference {
    /// A server this machine has saved, with its secret and — once a
    /// connection to it has opened — its pinned fingerprint.
    Saved(SavedServer),
    /// A server this machine has not connected to, named by the address it
    /// listens on.
    New {
        /// Where the server listens, as `host:port`.
        server_address: String,
    },
}

impl ServerReference {
    /// How this server is named in a message: its saved name when it has one,
    /// else the address it listens on.
    #[must_use]
    pub fn format_server_label(&self) -> String {
        match self {
            Self::Saved(saved_server) => format_saved_server_label(saved_server),
            Self::New { server_address } => server_address.clone(),
        }
    }
}

/// How one saved server is named in a message: the name the user chose when
/// they chose one, else the address it listens on. Either way it is a word
/// `koshi remote` takes.
fn format_saved_server_label(saved_server: &SavedServer) -> String {
    saved_server
        .server_name
        .clone()
        .unwrap_or_else(|| saved_server.server_address.clone())
}

/// An open connection to a server, past the secret exchange.
#[derive(Debug)]
pub struct RemoteLink {
    /// The frames the server sends.
    pub reader: FrameReader,
    /// The frames this client sends.
    pub writer: FrameWriter,
    /// The sha256 of the certificate the server presented, as 64 lowercase
    /// hex characters.
    pub certificate_fingerprint: String,
}

/// Why one dial did not hand back a connection.
#[derive(Debug)]
pub enum DialError {
    /// The path to the server failed, and dialling again can succeed.
    Unreachable(CliError),
    /// The server — or the pinned-certificate check — answered, and every
    /// identical dial after it gets the same answer.
    Refused(CliError),
}

/// The [`CliError`] the variant carries, unchanged.
impl From<DialError> for CliError {
    fn from(dial_error: DialError) -> Self {
        match dial_error {
            DialError::Unreachable(cli_error) | DialError::Refused(cli_error) => cli_error,
        }
    }
}

/// What asking one saved server for its sessions produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// The server answered with the sessions this machine's secret reaches.
    Reached {
        /// The server's name when it has one, else its address.
        server_label: String,
        /// The sessions, in the order the server holds them.
        session_rows: Vec<RemoteSessionRow>,
    },
    /// The server answered with a refusal: it did not admit the saved secret,
    /// it settled on a doorway version outside the range this build speaks, or
    /// it refused the listing.
    Refused {
        /// The server's name when it has one, else its address.
        server_label: String,
    },
    /// The server answered and presented a certificate other than the one
    /// pinned for it.
    CertificateChanged {
        /// The server's name when it has one, else its address.
        server_label: String,
        /// The sentence naming the pinned and the presented certificate.
        certificate_error_detail: String,
    },
    /// The server could not be reached, was still unanswered at the deadline,
    /// or answered a frame the request cannot produce.
    Unreachable {
        /// The server's name when it has one, else its address.
        server_label: String,
    },
    /// The record pins no certificate, and the sweep did not dial it.
    Unchecked {
        /// The server's name when it has one, else its address.
        server_label: String,
    },
}

/// The saved-server store and the path it came from, under the private data
/// directory.
///
/// # Errors
/// [`CliError::IpcUnavailable`] when the machine has no data directory, and
/// when the store could not be read.
pub fn load_saved_server_store() -> Result<(PathBuf, ServerStore), CliError> {
    let private_data_directory = resolve_private_data_directory()?;
    let saved_server_store_path = resolve_server_store_path(&private_data_directory);
    let saved_server_store = ServerStore::load_server_store_from_path(&saved_server_store_path)
        .map_err(saved_server_store_failed)?;
    Ok((saved_server_store_path, saved_server_store))
}

/// The private data directory this machine keeps koshi's files in.
///
/// # Errors
/// [`CliError::IpcUnavailable`] when the machine has no such directory.
fn resolve_private_data_directory() -> Result<PathBuf, CliError> {
    koshi_paths::resolve_data_directory().ok_or_else(|| CliError::IpcUnavailable {
        detail: "no data directory found".to_string(),
    })
}

/// Change the saved-server store, holding it against every other koshi from
/// the read to the write.
///
/// Takes the lock at `lock_file_path`, reads the store, hands it to `change`, and
/// writes it back. The lock is released when this returns, either way. A
/// `change` that refuses stops the write, so the store on disk keeps what it
/// held.
///
/// The lock is taken again every 20 milliseconds for up to 5 seconds. A wait
/// that runs out reports the other koshi rather than writing over it.
///
/// Nothing inside `change` may ask the user a question: every other koshi that
/// changes the store waits for this one to finish.
///
/// # Errors
/// [`CliError::IpcUnavailable`] when the machine has no data directory, when
/// the lock could not be taken, when the store could not be read, and when it
/// could not be written. Whatever `change` reports, with nothing written.
pub fn update_saved_server_store<T>(
    update_store: impl FnOnce(&mut ServerStore) -> Result<T, CliError>,
) -> Result<T, CliError> {
    let private_data_directory = resolve_private_data_directory()?;
    let saved_server_store_path = resolve_server_store_path(&private_data_directory);
    let store_lock_file = acquire_store_lock(
        &resolve_server_store_lock_path(&private_data_directory),
        STORE_LOCK_TIMEOUT_DURATION,
    )?;
    let mut saved_server_store = ServerStore::load_server_store_from_path(&saved_server_store_path)
        .map_err(saved_server_store_failed)?;
    let updated_store_value = update_store(&mut saved_server_store)?;
    saved_server_store
        .write_server_store_to_path(&saved_server_store_path)
        .map_err(saved_server_store_failed)?;
    drop(store_lock_file);
    Ok(updated_store_value)
}

/// Take the advisory lock on the file at `path`, creating the file and the
/// directory holding it when they are missing.
///
/// Both are restricted to the owning user on Unix: mode `0700` on the
/// directory, set whether or not this call made it, and mode `0600` on a lock
/// file this call creates. On Windows both take the data directory's
/// owner-scoped ACLs.
///
/// The attempt is repeated every [`STORE_LOCK_POLL_INTERVAL_DURATION`] for up to `lock_wait`.
/// Dropping the returned file releases the lock, and so does the operating
/// system when the process holding it dies.
///
/// # Errors
/// [`CliError::IpcUnavailable`] when the directory or the file could not be
/// made, when the lock could not be attempted, and when another koshi still
/// held it at the deadline.
fn acquire_store_lock(lock_file_path: &Path, lock_wait: Duration) -> Result<File, CliError> {
    let build_unavailable_error = |error_detail: String| CliError::IpcUnavailable {
        detail: error_detail,
    };
    if let Some(parent_directory) = lock_file_path.parent() {
        std::fs::create_dir_all(parent_directory).map_err(|io_error| {
            build_unavailable_error(format!(
                "{} could not be made: {io_error}",
                parent_directory.display()
            ))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent_directory, std::fs::Permissions::from_mode(0o700))
                .map_err(|io_error| {
                    build_unavailable_error(format!(
                        "{} could not be made private: {io_error}",
                        parent_directory.display()
                    ))
                })?;
        }
    }
    let mut lock_file_options = File::options();
    lock_file_options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        lock_file_options.mode(0o600);
    }
    let lock_file = lock_file_options.open(lock_file_path).map_err(|io_error| {
        build_unavailable_error(format!(
            "{} could not be opened: {io_error}",
            lock_file_path.display()
        ))
    })?;
    let deadline = Instant::now() + lock_wait;
    loop {
        match FileExt::try_lock(&lock_file) {
            Ok(()) => return Ok(lock_file),
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(build_unavailable_error(
                        "another koshi is changing the saved servers; try again".to_string(),
                    ));
                }
                std::thread::sleep(STORE_LOCK_POLL_INTERVAL_DURATION);
            }
            Err(TryLockError::Error(io_error)) => {
                return Err(build_unavailable_error(format!(
                    "{} could not be locked: {io_error}",
                    lock_file_path.display()
                )))
            }
        }
    }
}

/// A saved-server store that could not be read or written.
fn saved_server_store_failed(ipc_error: IpcError) -> CliError {
    CliError::IpcUnavailable {
        detail: ipc_error.to_string(),
    }
}

/// The server selector names: a saved record whose name or address matches the
/// selector, or a server this machine has not connected to when the selector is
/// an address.
///
/// Example — `work` matches the record the user named `work`, and
/// `laptop.local:7654` with no matching record is [`ServerReference::New`].
///
/// A selector that matches more than one record is refused, and no dial is
/// made.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the selector matches no record and is not an
/// address, so there is nothing to dial, and when it matches more than one.
pub fn resolve_server(server_selector_text: &str) -> Result<ServerReference, CliError> {
    let (_, saved_server_store) = load_saved_server_store()?;
    resolve_server_reference(
        saved_server_store.find_saved_server(server_selector_text),
        server_selector_text,
    )
}

/// Which server the selector names, given what the store said about it.
///
/// [`SavedServerLookup::Saved`] dials with that record's pinned fingerprint;
/// [`SavedServerLookup::NotSaved`] with an address shape dials with none.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the selector names nothing and is not an
/// address, and when it names more than one saved server.
fn resolve_server_reference(
    server_lookup: SavedServerLookup<'_>,
    server_selector_text: &str,
) -> Result<ServerReference, CliError> {
    match server_lookup {
        SavedServerLookup::Saved(saved_server) => {
            Ok(ServerReference::Saved(saved_server.clone()))
        }
        SavedServerLookup::Ambiguous => Err(CliError::InvalidArgs {
            detail: format!(
                "{server_selector_text} is the name of one saved server and the address of another; \
                 run `koshi remote list` and name the one you mean"
            ),
        }),
        SavedServerLookup::NotSaved if is_server_address(server_selector_text) => {
            Ok(ServerReference::New {
                server_address: server_selector_text.to_string(),
            })
        }
        SavedServerLookup::NotSaved => Err(CliError::InvalidArgs {
            detail: format!(
                "no saved server is named {server_selector_text}; run `koshi remote list`"
            ),
        }),
    }
}

/// Whether the selector has the `host:port` shape: a host before the last colon, and
/// a port of decimal digits after it.
///
/// A host holding a colon of its own must be bracketed, so a bare IPv6 literal
/// is not an address. The port is digits alone: no sign and no spaces.
///
/// Example — `laptop.local:7654` and `[::1]:22` are addresses; `work`,
/// `laptop.local`, `laptop.local:door`, `fe80::1` and `desk.local:+7654` are
/// not.
#[must_use]
pub fn is_server_address(server_selector_text: &str) -> bool {
    let Some((host, port)) = server_selector_text.rsplit_once(':') else {
        return false;
    };
    if host.is_empty() || port.is_empty() {
        return false;
    }
    let host_is_shaped = !host.contains(':') || (host.starts_with('[') && host.ends_with(']'));
    host_is_shaped
        && port.bytes().all(|port_byte| port_byte.is_ascii_digit())
        && port.parse::<u16>().is_ok()
}

/// Refuse a saved name that is empty or has the `host:port` shape.
///
/// # Errors
/// [`CliError::InvalidArgs`] naming the shape, and for an empty name.
pub fn validate_saved_server_name(saved_server_name: &str) -> Result<(), CliError> {
    if saved_server_name.is_empty() {
        return Err(CliError::InvalidArgs {
            detail: "a saved name must not be empty. Pick a plain name.".to_string(),
        });
    }
    if is_server_address(saved_server_name) {
        return Err(CliError::InvalidArgs {
            detail: format!(
                "{saved_server_name} is the shape of an address, and a saved name must not be: \
                 a lookup would take it for the server listening there. Pick a plain name."
            ),
        });
    }
    Ok(())
}

/// Refuse a saved name that cannot be given to the server at `address`.
///
/// Two names are refused: one with the `host:port` shape
/// ([`validate_saved_server_name`]), and one another record already answers to
/// ([`ServerStore::is_server_name_free`]). Reads the store.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the saved name has the `host:port` shape, and when
/// another address already holds it.
fn validate_save_as(saved_server_name: &str, server_address: &str) -> Result<(), CliError> {
    validate_saved_server_name(saved_server_name)?;
    let (_, saved_server_store) = load_saved_server_store()?;
    if !saved_server_store.is_server_name_free(saved_server_name, server_address) {
        let taken = saved_server_store
            .saved_servers
            .iter()
            .find(|saved_server| {
                saved_server.server_name.as_deref() == Some(saved_server_name)
                    || saved_server.server_address == saved_server_name
            })
            .map(|saved_server| saved_server.server_address.clone())
            .unwrap_or_default();
        return Err(CliError::InvalidArgs {
            detail: format!(
                "the name {saved_server_name} already belongs to {taken}; run `koshi remote forget {saved_server_name}` \
                 first, or pick another name"
            ),
        });
    }
    Ok(())
}

/// The secret to present to the server at `address`.
///
/// `KOSHI_REMOTE_SECRET` is read first. With it unset, or holding bytes that
/// are not UTF-8, the terminal is asked for the secret and what is typed is
/// not printed. Surrounding whitespace is trimmed.
///
/// # Errors
/// [`CliError::InvalidArgs`] when nothing was given, and when the terminal
/// could not be read.
pub fn resolve_server_connection_token(server_address: &str) -> Result<ConnectionToken, CliError> {
    let secret_text = match std::env::var(SECRET_ENVIRONMENT_VARIABLE) {
        Ok(secret) => secret,
        Err(_) => read_terminal_secret(&format!("secret for {server_address}: "))?,
    };
    let trimmed_secret_text = secret_text.trim();
    if trimmed_secret_text.is_empty() {
        return Err(CliError::InvalidArgs {
            detail: format!(
                "no secret was given; set {SECRET_ENVIRONMENT_VARIABLE} or paste it when asked"
            ),
        });
    }
    Ok(ConnectionToken::from_secret(trimmed_secret_text))
}

/// Print `prompt`, then read one secret from the terminal without printing
/// what is typed, with surrounding whitespace trimmed. The answer can be
/// empty.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the terminal could not be read, when the
/// entry was interrupted with `0x03`, and when the input ended before an
/// answer arrived.
pub fn prompt_secret(prompt: &str) -> Result<String, CliError> {
    Ok(read_terminal_secret(prompt)?.trim().to_string())
}

/// Print `prompt`, then read one line from the terminal, which the terminal
/// echoes, with surrounding whitespace trimmed. The answer can be empty.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the terminal could not be read, and when the
/// input ended before a line arrived.
pub fn prompt_line(prompt: &str) -> Result<String, CliError> {
    print!("{prompt}");
    io::stdout().flush().map_err(build_prompt_error)?;
    Ok(read_terminal_line()?.trim().to_string())
}

/// Print `prompt`, then read one secret from the terminal without printing
/// what is typed.
///
/// The terminal is put in raw mode while the secret is typed. A terminal that
/// cannot be put in raw mode reads one plain line instead, which the terminal
/// echoes.
fn read_terminal_secret(prompt: &str) -> Result<String, CliError> {
    print!("{prompt}");
    io::stdout().flush().map_err(build_prompt_error)?;
    if crossterm::terminal::enable_raw_mode().is_err() {
        return read_terminal_line();
    }
    let secret_input = read_hidden_terminal_line(&mut io::stdin().lock());
    let _ = crossterm::terminal::disable_raw_mode();
    println!();
    secret_input.map_err(build_prompt_error)
}

/// Read one line from standard input, as the terminal echoes it.
///
/// # Errors
/// [`CliError::InvalidArgs`] when standard input could not be read, and when
/// it ended before a line arrived.
fn read_terminal_line() -> Result<String, CliError> {
    let mut entered_line = String::new();
    match io::stdin()
        .read_line(&mut entered_line)
        .map_err(build_prompt_error)?
    {
        0 => Err(build_input_ended_error()),
        _ => Ok(entered_line),
    }
}

/// Read from the input reader until the Enter key, with none of it printed.
///
/// Ends at `\r`, `\n` or `0x04`. Backspace — `0x7f` or `0x08` — removes the
/// last byte. `0x03` is [`io::ErrorKind::Interrupted`]. End of stream ends the
/// entry where it stands, and is [`io::ErrorKind::UnexpectedEof`] when nothing
/// was typed. Invalid UTF-8 is replaced.
fn read_hidden_terminal_line(input_reader: &mut impl Read) -> io::Result<String> {
    let mut entered_bytes: Vec<u8> = Vec::new();
    let mut input_byte_buffer = [0u8; 1];
    loop {
        if input_reader.read(&mut input_byte_buffer)? == 0 {
            if entered_bytes.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the entry ended",
                ));
            }
            break;
        }
        match input_byte_buffer[0] {
            b'\r' | b'\n' | 0x04 => break,
            0x03 => {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "the entry was interrupted",
                ))
            }
            0x7f | 0x08 => {
                entered_bytes.pop();
            }
            entered_byte => entered_bytes.push(entered_byte),
        }
    }
    Ok(String::from_utf8_lossy(&entered_bytes).into_owned())
}

/// A terminal that could not be printed to or read from.
fn build_prompt_error(io_error: io::Error) -> CliError {
    CliError::InvalidArgs {
        detail: format!("the answer could not be read: {io_error}"),
    }
}

/// Input that ended before the answer arrived.
fn build_input_ended_error() -> CliError {
    CliError::InvalidArgs {
        detail: "the input ended before the answer arrived".to_string(),
    }
}

/// Open a connection to `server_address` and present `connection_token`.
///
/// `pinned_certificate_fingerprint` is the fingerprint saved from an earlier
/// connection, or `None` on the first connection to this server. A server
/// presenting a different certificate is refused.
///
/// `dial_timeout` bounds the connect, the TLS handshake and the secret exchange
/// together. The name lookup before them is the operating system's own and
/// carries no timeout. `reply_timeout` bounds the reply after the connection
/// opens when it is `Some`.
///
/// # Errors
/// [`DialError::Unreachable`] when the connection could not be opened or the
/// exchange ran out of time. [`DialError::Refused`] when the certificate
/// changed, the server did not admit the secret, the doorway version it
/// settled on is one this build does not speak, or it answered something else.
pub fn connect_remote_server(
    server_address: &str,
    connection_token: &ConnectionToken,
    pinned_certificate_fingerprint: Option<&str>,
    dial_timeout: Duration,
    reply_timeout: Option<Duration>,
) -> Result<RemoteLink, DialError> {
    let remote_hello = RemoteClientFrame::Hello {
        min_remote_version: MIN_REMOTE_PROTOCOL_VERSION,
        max_remote_version: REMOTE_PROTOCOL_VERSION,
        min_protocol_version: MIN_PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        connection_token: connection_token.clone(),
    };
    let (frame_reader, frame_writer, certificate_fingerprint, remote_server_answer) =
        remote_wire::open_remote_connection(
            server_address,
            pinned_certificate_fingerprint,
            &remote_hello,
            dial_timeout,
            reply_timeout,
        )
        .map_err(classify_dial_failure)?;
    validate_remote_server_answer(server_address, &remote_server_answer)?;
    Ok(RemoteLink {
        reader: frame_reader,
        writer: frame_writer,
        certificate_fingerprint,
    })
}

/// How one dial's transport failure classifies: a certificate that does not
/// match the pinned one is [`DialError::Refused`], every other transport
/// failure is [`DialError::Unreachable`]. The message is
/// [`build_ipc_unavailable_error`]'s either way.
fn classify_dial_failure(ipc_error: IpcError) -> DialError {
    match ipc_error {
        IpcError::CertificateChanged { .. } => {
            DialError::Refused(build_ipc_unavailable_error(ipc_error))
        }
        _ => DialError::Unreachable(build_ipc_unavailable_error(ipc_error)),
    }
}

/// `Ok(())` when `remote_server_frame` is a `Welcome` carrying a doorway version between
/// [`MIN_REMOTE_PROTOCOL_VERSION`] and [`REMOTE_PROTOCOL_VERSION`], else the
/// [`DialError::Refused`] to report.
///
/// A `Refused` frame carrying
/// [`REMOTE_REFUSED`](koshi_ipc::remote_wire::REMOTE_REFUSED) reads as a
/// rejected or revoked token and names both ways to replace it. Any other
/// refusal message is the server's own sentence, filtered by
/// [`sanitize_reported_text`], with `address` after it.
///
/// Every refusal built here carries [`CliError::Runtime`], which is what
/// [`probe_saved_server`] reads a [`DialError::Refused`] carrying
/// [`CliError::IpcUnavailable`]
/// as the pinned-certificate check.
///
/// Example — a `Refused` frame carrying `"the session is gone"` from
/// `desk.local:7654` reads `"the session is gone (server desk.local:7654)"`.
/// A frame carrying `"\u{1b}[2Jgone"` reads `"gone (server desk.local:7654)"`.
fn validate_remote_server_answer(
    server_address: &str,
    remote_server_frame: &RemoteServerFrame,
) -> Result<(), DialError> {
    match remote_server_frame {
        RemoteServerFrame::Welcome {
            remote_protocol_version,
        } if (MIN_REMOTE_PROTOCOL_VERSION..=REMOTE_PROTOCOL_VERSION)
            .contains(remote_protocol_version) =>
        {
            Ok(())
        }
        RemoteServerFrame::Welcome {
            remote_protocol_version,
        } => {
            Err(DialError::Refused(CliError::Runtime {
                detail: format!(
                    "server {server_address} settled on remote doorway {remote_protocol_version}, which this \
                     koshi does not speak: it speaks {MIN_REMOTE_PROTOCOL_VERSION} to \
                     {REMOTE_PROTOCOL_VERSION}"
                ),
            }))
        }
        RemoteServerFrame::Refused {
            message: refusal_message,
        } if refusal_message == remote_wire::REMOTE_REFUSED => {
            Err(DialError::Refused(CliError::Runtime {
                detail: format!(
                    "the server {server_address} did not admit the connection: the token was rejected \
                     or revoked. re-grant it on that machine with `koshi share grant`; store \
                     the new secret with `koshi remote set-secret` for a saved server, or \
                     give it when the next dial asks"
                ),
            }))
        }
        RemoteServerFrame::Refused {
            message: refusal_message,
        } => Err(DialError::Refused(CliError::Runtime {
            detail: format!(
                "{} (server {server_address})",
                sanitize_reported_text(refusal_message.as_str())
            ),
        })),
        RemoteServerFrame::Sessions { .. } => Err(DialError::Refused(CliError::Runtime {
            detail: format_unexpected_remote_answer(server_address, "Sessions"),
        })),
    }
}

/// Open a connection to the server argument, saving what the next
/// connection needs.
///
/// A saved server presents the secret and the fingerprint its record holds, and
/// its last-used time is stamped once the connection opens. A record holding
/// no fingerprint takes the one the store pins for that address now, and pins
/// the certificate this connection presented when the store pins none either.
/// A store that will not take those changes leaves a log line and the
/// connection stands.
///
/// A server reached for the first time asks for its secret ([`resolve_server_connection_token`]),
/// pins whatever certificate it presents, and is saved under `save_as` once it
/// admits the connection. A store that will not take that record fails the
/// call.
///
/// `save_as` names a server this machine has not connected to. Given for a
/// server that is already saved, it is refused.
///
/// `reply_timeout` is passed straight to [`connect_remote_server`].
///
/// The saved record comes back alongside the connection.
///
/// # Errors
/// Whatever [`connect_remote_server`] reports. [`DialError::Refused`] carrying
/// [`CliError::InvalidArgs`] when `save_as` names a server that is already
/// saved, when the name cannot be given to that address, and when no secret was
/// given; carrying [`CliError::IpcUnavailable`] when a server reached for the
/// first time could not be saved.
pub fn connect_saved_server(
    server_reference: &ServerReference,
    save_as: Option<&str>,
    reply_timeout: Option<Duration>,
) -> Result<(RemoteLink, SavedServer), DialError> {
    match server_reference {
        ServerReference::Saved(saved_server_record) => {
            if let Some(save_as_name) = save_as {
                return Err(DialError::Refused(CliError::InvalidArgs {
                    detail: format!(
                        "{} is already saved, so --save-as {save_as_name} would change nothing; \
                         run `koshi remote forget {}` first to save it under another name",
                        format_saved_server_label(saved_server_record),
                        format_saved_server_label(saved_server_record)
                    ),
                }));
            }
            let pinned_certificate_fingerprint = saved_server_record
                .certificate_fingerprint
                .clone()
                .or_else(|| {
                    load_saved_server_store()
                        .ok()
                        .and_then(|(_, saved_server_store)| {
                            find_pinned_certificate_fingerprint(
                                &saved_server_store,
                                &saved_server_record.server_address,
                            )
                        })
                });
            let remote_link = connect_remote_server(
                &saved_server_record.server_address,
                &saved_server_record.connection_token,
                pinned_certificate_fingerprint.as_deref(),
                DIAL_TIMEOUT_DURATION,
                reply_timeout,
            )?;
            let now = SystemTime::now();
            let store_update_result = update_saved_server_store(|saved_server_store| {
                saved_server_store.mark_server_used(&saved_server_record.server_address, now);
                if saved_server_record.certificate_fingerprint.is_none() {
                    saved_server_store.pin_certificate_fingerprint(
                        &saved_server_record.server_address,
                        remote_link.certificate_fingerprint.clone(),
                    );
                }
                Ok(())
            });
            if let Err(store_update_error) = store_update_result {
                tracing::warn!(%store_update_error, "the record was not updated");
            }
            let mut updated_saved_server = saved_server_record.clone();
            updated_saved_server.certificate_fingerprint =
                Some(remote_link.certificate_fingerprint.clone());
            updated_saved_server.last_used_at = Some(now);
            Ok((remote_link, updated_saved_server))
        }
        ServerReference::New { server_address } => {
            if let Some(save_as_name) = save_as {
                validate_save_as(save_as_name, server_address).map_err(DialError::Refused)?;
            }
            let connection_token =
                resolve_server_connection_token(server_address).map_err(DialError::Refused)?;
            let remote_link = connect_remote_server(
                server_address,
                &connection_token,
                None,
                DIAL_TIMEOUT_DURATION,
                reply_timeout,
            )?;
            let now = SystemTime::now();
            let new_saved_server = SavedServer {
                server_name: save_as.map(str::to_string),
                server_address: server_address.clone(),
                connection_token,
                certificate_fingerprint: Some(remote_link.certificate_fingerprint.clone()),
                added_at: now,
                last_used_at: Some(now),
            };
            update_saved_server_store(|saved_server_store| {
                saved_server_store
                    .save_server(new_saved_server.clone())
                    .map_err(|taken_server_address| CliError::InvalidArgs {
                        detail: taken_server_address.to_string(),
                    })
            })
            .map_err(DialError::Refused)?;
            Ok((remote_link, new_saved_server))
        }
    }
}

/// The fingerprint the saved-server store pins for `address`, or `None` when no record
/// answers to it, more than one does, or the one that does pins nothing.
fn find_pinned_certificate_fingerprint(
    saved_server_store: &ServerStore,
    server_address: &str,
) -> Option<String> {
    match saved_server_store.find_saved_server(server_address) {
        SavedServerLookup::Saved(saved_server_record) => {
            saved_server_record.certificate_fingerprint.clone()
        }
        SavedServerLookup::NotSaved | SavedServerLookup::Ambiguous => None,
    }
}

/// The sessions this connection's secret reaches, in the order the server
/// holds them, each name carrying the bytes the server sent.
///
/// # Errors
/// [`CliError::Runtime`] when the server refused the request, carrying the
/// server's own sentence filtered by [`sanitize_reported_text`], and
/// [`CliError::IpcUnavailable`] when the exchange failed or the server
/// answered something else.
pub fn list_remote_sessions(link: &mut RemoteLink) -> Result<Vec<RemoteSessionRow>, CliError> {
    link.writer
        .send(&RemoteClientFrame::List)
        .map_err(build_ipc_unavailable_error)?;
    match link
        .reader
        .recv::<RemoteServerFrame>()
        .map_err(build_ipc_unavailable_error)?
    {
        RemoteServerFrame::Sessions { session_rows } => Ok(session_rows),
        RemoteServerFrame::Refused { message } => Err(CliError::Runtime {
            detail: sanitize_reported_text(&message),
        }),
        RemoteServerFrame::Welcome { .. } => Err(CliError::IpcUnavailable {
            detail: format_unexpected_remote_answer("the server", "Welcome"),
        }),
    }
}
/// Ask to attach to `selector` and hand the connection's two halves back.
///
/// The bytes after this belong to that session's own server. The machine
/// serving it sends the session-plane Hello carrying that session's endpoint
/// token and the versions this build named, so the next frame the caller reads
/// is that session server's Hello answer.
///
/// # Errors
/// [`CliError::IpcUnavailable`] when the request could not be sent.
pub fn attach_remote_session(
    link: RemoteLink,
    selector: SessionSelector,
) -> Result<(FrameReader, FrameWriter), CliError> {
    let RemoteLink {
        reader, mut writer, ..
    } = link;
    writer
        .send(&RemoteClientFrame::Attach {
            session_selector: selector,
        })
        .map_err(build_ipc_unavailable_error)?;
    Ok((reader, writer))
}

/// Submit `command` to the session `session_id` on the server argument, and
/// hand back the dispatcher's result.
///
/// The command's source is [`CommandSource::ExternalCli`] carrying `session`
/// and the client the caller named. A pane-creating command carrying no
/// working directory keeps none. A rejection's hint is filtered by
/// [`sanitize_reported_text`].
///
/// A named `client` reaches only a session that settled on protocol version 3
/// or newer; a session that settled below it is refused with
/// [`CliError::IpcUnavailable`] before the command is written. `None` names no
/// client, and the command is written whatever the session settled on.
///
/// # Errors
/// Whatever [`connect_saved_server`] reports, and [`CliError::IpcUnavailable`] when
/// the exchange failed.
pub fn submit_remote_command(
    server_reference: &ServerReference,
    session_id: SessionId,
    client_id: Option<ClientId>,
    command: Command,
) -> Result<CommandResult, CliError> {
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_external_cli(Some(session_id), client_id),
        SystemTime::now(),
        command,
    );
    let command_request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::SubmitCommand(Box::new(command_envelope)),
    };
    match send_remote_ipc_request(
        server_reference,
        session_id,
        command_request,
        client_id.is_some(),
    )? {
        IpcResult::CommandResult(command_result) => Ok(talk::filter_rejection_hint(command_result)),
        IpcResult::Error(refusal) => Err(build_peer_refusal_error(&refusal)),
        unexpected_result => {
            Err(talk::SESSION_PEER_WORDS.build_unexpected_reply_error(&unexpected_result))
        }
    }
}

/// Ask the session `session_id` on the server argument to describe itself in
/// full: tabs, panes, and attached clients.
///
/// Sends [`IpcRequestKind::Discovery`] over one remote connection of its own.
/// The answer passes through
/// [`filter_session_overview_text`](crate::discovery::filter_session_overview_text) before it
/// is handed back, so the session name, the tab names, and each pane's title,
/// working directory and argv are filtered.
///
/// # Errors
/// Whatever [`connect_saved_server`] reports, and [`CliError::IpcUnavailable`] when
/// the exchange failed.
pub fn fetch_remote_overview(
    server_reference: &ServerReference,
    session_id: SessionId,
) -> Result<SessionOverview, CliError> {
    let discovery_request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Discovery,
    };
    match send_remote_ipc_request(server_reference, session_id, discovery_request, false)? {
        IpcResult::Overview(mut session_overview) => {
            crate::discovery::filter_session_overview_text(&mut session_overview);
            Ok(session_overview)
        }
        IpcResult::Error(refusal) => Err(build_peer_refusal_error(&refusal)),
        unexpected_result => {
            Err(talk::SESSION_PEER_WORDS.build_unexpected_reply_error(&unexpected_result))
        }
    }
}

/// One request against one remote session: dial, attach, settle the version
/// from the Hello answer the server sent on this caller's behalf, then send
/// the IPC request and read its answer.
///
/// `has_client_target` `true` refuses a session that settled below
/// [`TARGET_CLIENT_PROTOCOL`](crate::talk::TARGET_CLIENT_PROTOCOL) with
/// [`CliError::IpcUnavailable`], before the IPC request is written. `false`
/// writes the IPC request whatever the session settled on.
fn send_remote_ipc_request(
    server_reference: &ServerReference,
    session_id: SessionId,
    ipc_request: IpcRequest,
    has_client_target: bool,
) -> Result<IpcResult, CliError> {
    let (remote_link, _) =
        connect_saved_server(server_reference, None, Some(REPLY_TIMEOUT_DURATION))?;
    let (mut frame_reader, mut frame_writer) =
        attach_remote_session(remote_link, SessionSelector::SessionId(session_id))?;

    let hello_response: IncomingResponse =
        frame_reader.recv().map_err(build_ipc_unavailable_error)?;
    let (settled_protocol_version, _) = talk::parse_session_hello_version(hello_response)?;
    talk::validate_client_targeting(settled_protocol_version, has_client_target)?;

    frame_writer
        .send(&ipc_request)
        .map_err(build_ipc_unavailable_error)?;
    let incoming_response: IncomingResponse =
        frame_reader.recv().map_err(build_ipc_unavailable_error)?;
    talk::SESSION_PEER_WORDS.take_response_result(incoming_response)
}

/// Ask every saved server for its sessions at once, and return inside
/// `timeout` whatever the servers do.
///
/// `timeout` is one deadline over the whole call, not a budget each server
/// gets. Each record is asked on its own thread. A thread still running at
/// the deadline is never joined; it writes no file.
///
/// At most 16 records are asked. The rest are named on stderr and left out.
///
/// A record pinning no certificate is [`Reach::Unchecked`], and no secret is
/// presented to it.
///
/// A server that answered with a refusal is [`Reach::Refused`]. A server that
/// could not be reached, was still unanswered at the deadline, or presented a
/// certificate other than the pinned one is [`Reach::Unreachable`]. Every
/// record comes back as exactly one entry, sorted by server name. A store that
/// cannot be read reads as no saved servers.
#[must_use]
pub fn reach_all_saved_servers(timeout: Duration) -> Vec<Reach> {
    let deadline = Instant::now() + timeout;
    let Ok((_, saved_server_store)) = load_saved_server_store() else {
        return Vec::new();
    };

    let saved_server_count = saved_server_store.saved_servers.len();
    if saved_server_count > MAX_CONCURRENT_REACH_COUNT {
        eprintln!(
            "koshi: asking the first {MAX_CONCURRENT_REACH_COUNT} of {saved_server_count} saved servers; \
             name one with `--remote <server>` to reach the rest"
        );
    }

    let requested_server_labels: Vec<String> = saved_server_store
        .saved_servers
        .iter()
        .take(MAX_CONCURRENT_REACH_COUNT)
        .map(format_saved_server_label)
        .collect();

    let (reach_sender, reach_receiver) = mpsc::channel();
    let mut requested_server_count = 0usize;
    for saved_server_record in saved_server_store
        .saved_servers
        .into_iter()
        .take(MAX_CONCURRENT_REACH_COUNT)
    {
        let reach_sender = reach_sender.clone();
        let spawn_result = std::thread::Builder::new()
            .name("koshi-remote-reach".to_string())
            .spawn(move || {
                let _ = reach_sender.send(probe_saved_server(&saved_server_record, deadline));
            });
        if spawn_result.is_ok() {
            requested_server_count += 1;
        }
    }
    drop(reach_sender);

    let mut received_reaches = Vec::with_capacity(requested_server_count);
    while received_reaches.len() < requested_server_count {
        let remaining_wait = deadline.saturating_duration_since(Instant::now());
        if remaining_wait.is_zero() {
            break;
        }
        match reach_receiver.recv_timeout(remaining_wait) {
            Ok(reach_result) => received_reaches.push(reach_result),
            Err(_) => break,
        }
    }
    complete_reach_results(received_reaches, requested_server_labels)
}

/// The server one [`Reach`] is about, whatever it answered.
fn get_reach_server_label(reach: &Reach) -> &str {
    match reach {
        Reach::Reached { server_label, .. }
        | Reach::Refused { server_label }
        | Reach::CertificateChanged { server_label, .. }
        | Reach::Unreachable { server_label }
        | Reach::Unchecked { server_label } => server_label,
    }
}

/// Every requested server as exactly one entry, sorted by server: the answers
/// in `received_reaches`, plus one [`Reach::Unreachable`] per label in
/// `requested_server_labels` that no answer names.
///
/// Example — `received_reaches` naming only `desk` with
/// `requested_server_labels` `["desk", "work"]` gives `desk`'s answer and
/// `Unreachable { server_label: "work" }`, in that order.
fn complete_reach_results(
    mut received_reaches: Vec<Reach>,
    mut requested_server_labels: Vec<String>,
) -> Vec<Reach> {
    for reach_result in &received_reaches {
        if let Some(removal_index) = requested_server_labels
            .iter()
            .position(|server_label| server_label == get_reach_server_label(reach_result))
        {
            requested_server_labels.remove(removal_index);
        }
    }
    received_reaches.extend(
        requested_server_labels
            .into_iter()
            .map(|server_label| Reach::Unreachable { server_label }),
    );
    received_reaches.sort_by(|left_reach, right_reach| {
        get_reach_server_label(left_reach).cmp(get_reach_server_label(right_reach))
    });
    received_reaches
}

/// Ask one saved server for its sessions.
///
/// A record pinning no certificate is [`Reach::Unchecked`] and is not dialled.
///
/// A failure carrying [`CliError::Runtime`] — every refusal the server sent —
/// is [`Reach::Refused`]. A dial refused with [`CliError::IpcUnavailable`] is
/// the pinned-certificate check and is [`Reach::CertificateChanged`]:
/// [`dial_failed`] is the only place that builds one, and every refusal
/// [`validate_remote_server_answer`] builds carries [`CliError::Runtime`]. Every other failure
/// is [`Reach::Unreachable`].
///
/// The time left until `deadline` is given to the dial and again to the reply,
/// so this returns up to twice that after `deadline` passes. Writes no file.
fn probe_saved_server(saved_server_record: &SavedServer, deadline: Instant) -> Reach {
    let server_label = format_saved_server_label(saved_server_record);
    let Some(pinned_certificate_fingerprint) =
        saved_server_record.certificate_fingerprint.as_deref()
    else {
        return Reach::Unchecked { server_label };
    };
    let remaining_wait_duration = deadline.saturating_duration_since(Instant::now());
    let mut remote_link = match connect_remote_server(
        &saved_server_record.server_address,
        &saved_server_record.connection_token,
        Some(pinned_certificate_fingerprint),
        remaining_wait_duration,
        Some(remaining_wait_duration),
    ) {
        Ok(remote_link) => remote_link,
        Err(DialError::Refused(CliError::IpcUnavailable {
            detail: certificate_error_detail,
        })) => {
            return Reach::CertificateChanged {
                server_label,
                certificate_error_detail,
            }
        }
        Err(dial_error) => match CliError::from(dial_error) {
            CliError::Runtime { .. } => return Reach::Refused { server_label },
            _ => return Reach::Unreachable { server_label },
        },
    };
    match list_remote_sessions(&mut remote_link) {
        Ok(session_rows) => Reach::Reached {
            server_label,
            session_rows,
        },
        Err(CliError::Runtime { .. }) => Reach::Refused { server_label },
        Err(_) => Reach::Unreachable { server_label },
    }
}

/// The sentence naming a doorway frame the request cannot produce, in the
/// words [`talk::PeerWords::build_unexpected_wire_name_error`] uses for a session-plane one.
///
/// `server_address` is the address dialled or `"the server"`; `frame_name` is the
/// [`RemoteServerFrame`] variant that came back. `("desk.local:7654",
/// "Sessions")` gives `desk.local:7654 answered with an unexpected Sessions
/// reply`.
fn format_unexpected_remote_answer(server_address: &str, remote_frame_name: &str) -> String {
    format!("{server_address} answered with an unexpected {remote_frame_name} reply")
}

#[cfg(test)]
mod tests;
