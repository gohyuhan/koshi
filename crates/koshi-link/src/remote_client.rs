//! The dialling side of remote access: reach the sessions on another
//! machine.
//!
//! A user names a server either by the address it listens on, `host:port`, or
//! by the name they saved it under. [`resolve_server`] turns what they typed
//! into one of the two, reading the saved-server store on this machine.
//! A server this machine has not saved also needs the fingerprint of the
//! certificate it must present, from `--certificate-fingerprint`. A saved one
//! carries its own, and a saved one holding none is not dialled.
//!
//! The secret from a grant never arrives as a command-line argument.
//! [`resolve_server_connection_token`] reads it from
//! `KOSHI_REMOTE_SECRET`, or asks for it at the terminal without printing
//! what is typed. A saved server presents the secret it holds, and
//! [`resolve_server`] refuses it when `KOSHI_REMOTE_SECRET` holds a secret no
//! saved server holds. The sweep of every saved server does not read
//! `KOSHI_REMOTE_SECRET`.
//!
//! [`connect_remote_server`] opens the TLS stream, checks the certificate
//! against the fingerprint it was given, presents the secret, and hands back a
//! [`RemoteLink`] once the server answers
//! `Welcome`. [`connect_saved_server`] wraps
//! that with the store: a server reached for the first time is saved with the
//! fingerprint it was dialled with, and a saved one has its last-used time
//! stamped. A saved one that holds no name takes the name `--save-as` gives.
//!
//! From an open link a caller lists the sessions the secret reaches, attaches
//! to one, submits one command to one, or asks one to describe itself.
//! [`list_saved_server_session_rows`] asks every saved server at once and
//! returns, inside one deadline, the sessions of each server that answered.
//!
//! Every TLS and remote-frame detail stays inside this module. Callers name
//! sessions, secrets, addresses and certificate fingerprints, and never a
//! frame.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read};
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
use koshi_ipc::remote_migration::migrate_saved_server_file;
use koshi_ipc::remote_servers::{
    resolve_server_store_lock_path, resolve_server_store_path, SavedServer, SavedServerLookup,
    ServerStore,
};
use koshi_ipc::remote_wire::{
    self, RemoteClientFrame, RemoteServerFrame, RemoteSessionRow, MIN_REMOTE_PROTOCOL_VERSION,
    REMOTE_PROTOCOL_VERSION, SESSION_NOT_ANSWERING_MESSAGE,
};
use koshi_ipc::router::{SessionSelector, ROUTER_RESTARTING_MESSAGE};
use koshi_ipc::tls::{format_certificate_fingerprint, CERTIFICATE_FINGERPRINT_PREFIX};
use koshi_ipc::transport::{parse_answer, FrameReader, FrameWriter};
use koshi_ipc::wire::WireName;
use serde_json::value::RawValue;

use crate::error::CliError;
use crate::ipc_client;
use crate::talk::{self, build_ipc_unavailable_error, build_peer_refusal_error};

/// The environment variable holding the secret from a grant, read before the
/// terminal is asked for one.
const SECRET_ENVIRONMENT_VARIABLE: &str = "KOSHI_REMOTE_SECRET";

/// How long one dial has to open: the name lookup aside, the connect, the TLS
/// handshake and the secret exchange share it.
pub const DIAL_TIMEOUT_DURATION: Duration = Duration::from_secs(10);

/// How long a wait for a server that is restarting pauses between dials. Each
/// dial runs the whole admission — TLS, the secret, and the scope check.
pub const REMOTE_RESTART_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(250);

/// How long each of the two parts of a join of a client to a session on
/// another machine has to finish. The first part runs from the open connection
/// to the session's `Prepared`: the Attach, the session's Hello carried back
/// through the bridge, and the answer to the `Prepare`. The second part runs
/// from the `Commit` to the answer that names the client.
///
/// The deadline is taken off once the client is joined.
pub const JOIN_TIMEOUT_DURATION: Duration = Duration::from_secs(20);

/// How long one command sent to a session on another machine has to come back,
/// counted from the moment the connection opens.
///
/// The dial before it has [`DIAL_TIMEOUT_DURATION`] of its own, so one request takes at most
/// `DIAL_TIMEOUT_DURATION + REPLY_TIMEOUT_DURATION`. A client that joins a session as a viewer
/// passes [`JOIN_TIMEOUT_DURATION`] instead.
pub const REPLY_TIMEOUT_DURATION: Duration = Duration::from_secs(20);

/// How many saved servers [`reach_all_saved_servers`] asks at once, one thread each. Records
/// past this count are not asked; [`reach_all_saved_servers`] names how many on stderr.
const MAX_CONCURRENT_REACH_COUNT: usize = 16;

/// How long [`reach_all_saved_servers`] waits for every saved server together, one deadline
/// over the whole sweep.
const REACH_TIMEOUT_DURATION: Duration = Duration::from_secs(2);

/// How long a change to a store under `remote/` — the saved servers or the
/// remote access tokens — waits for another koshi to finish its own change
/// before it gives up. The operating system releases the lock if that koshi
/// dies.
pub const STORE_LOCK_TIMEOUT_DURATION: Duration = Duration::from_secs(5);

/// How long the wait for a store lock pauses between attempts on the lock.
const STORE_LOCK_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(20);

/// The error detail of a saved-server store lock that another koshi still held
/// at the deadline.
const SAVED_SERVER_STORE_HELD_DETAIL: &str =
    "another koshi is changing the saved servers; try again";

/// Which server an invocation talks to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerReference {
    /// A server this machine has saved, with its secret and the fingerprint
    /// its certificate must have.
    Saved(SavedServer),
    /// A server this machine has not saved, named by the address it listens
    /// on.
    New {
        /// Where the server listens, as `host:port`.
        server_address: String,
        /// The fingerprint the server's certificate must have, as 64
        /// lowercase hex characters.
        certificate_fingerprint: String,
    },
}

impl ServerReference {
    /// How this server is named in a message: its saved name when it has one,
    /// else the address it listens on.
    #[must_use]
    pub fn format_server_label(&self) -> String {
        match self {
            Self::Saved(saved_server) => format_saved_server_label(saved_server),
            Self::New { server_address, .. } => server_address.clone(),
        }
    }
}

/// The sentence that refuses to dial the saved server `saved_server_label`
/// names, which holds no certificate fingerprint.
///
/// Example — `desk` gives ``desk holds no certificate fingerprint, so it is
/// not dialled; run `koshi remote edit desk` and give the fingerprint `koshi
/// share grant` printed on that machine``.
fn format_missing_fingerprint_refusal(saved_server_label: &str) -> String {
    format!(
        "{saved_server_label} holds no certificate fingerprint, so it is not dialled; run \
         `koshi remote edit {saved_server_label}` and give the fingerprint `koshi share grant` \
         printed on that machine"
    )
}

/// How one saved server is named in a message: the name the user chose when
/// they chose one, else the address it listens on. Either way it is a word
/// `koshi remote` takes.
#[must_use]
pub fn format_saved_server_label(saved_server: &SavedServer) -> String {
    saved_server
        .server_name
        .clone()
        .unwrap_or_else(|| saved_server.server_address.clone())
}

/// An open connection to a server, past the secret exchange.
#[derive(Debug)]
pub struct RemoteLink {
    /// The frames the server sends.
    pub frame_reader: FrameReader,
    /// The frames this client sends.
    pub frame_writer: FrameWriter,
}

/// Why one dial did not hand back a connection.
#[derive(Debug)]
pub enum DialError {
    /// The path to the server failed, and dialling again can succeed.
    Unreachable(CliError),
    /// The server's router answered that it is restarting into a new build,
    /// or the session refused the request with `Restarting` while it replaces
    /// its process image, and a dial after the restart can succeed.
    Restarting(CliError),
    /// The server — or the certificate check — answered, and every
    /// identical dial after it gets the same answer.
    Refused(CliError),
}

impl DialError {
    /// The [`DialError`] for `session_refusal`, an answer of the session that
    /// ends a dial: [`DialError::Restarting`] for
    /// [`CliError::SessionRestarting`], which the session answers while it
    /// replaces its process image, and [`DialError::Refused`] for every other
    /// answer.
    #[must_use]
    pub fn from_session_refusal(session_refusal: CliError) -> DialError {
        match session_refusal {
            session_restarting @ CliError::SessionRestarting { .. } => {
                DialError::Restarting(session_restarting)
            }
            session_refusal => DialError::Refused(session_refusal),
        }
    }
}

/// The [`CliError`] the variant carries, unchanged.
impl From<DialError> for CliError {
    fn from(dial_error: DialError) -> Self {
        match dial_error {
            DialError::Unreachable(cli_error)
            | DialError::Restarting(cli_error)
            | DialError::Refused(cli_error) => cli_error,
        }
    }
}

/// What asking one saved server for its sessions produced.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Reach {
    /// The server answered with the sessions this machine's secret reaches.
    Reached {
        /// The server's name when it has one, else its address.
        server_label: String,
        /// The sessions, in the order the server sent them.
        session_rows: Vec<RemoteSessionRow>,
    },
    /// The dial or the listing failed before the deadline: the server refused
    /// the secret or the listing, presented a certificate other than the one
    /// its record holds the fingerprint of, refused or closed the connection,
    /// or answered a frame the request cannot produce.
    Failed {
        /// The server's name when it has one, else its address.
        server_label: String,
        /// The failure in the words a dial to that server prints for it: the
        /// `Display` of its [`CliError`].
        failure_message: String,
    },
    /// Nothing answered before the deadline: the dial or the listing was
    /// still running when it passed, or failed at or after it.
    Unanswered {
        /// The server's name when it has one, else its address.
        server_label: String,
    },
    /// The record holds no certificate fingerprint, and the sweep did not
    /// dial it.
    Unchecked {
        /// The server's name when it has one, else its address.
        server_label: String,
    },
}

/// The saved-server store and the path it came from, under the private data
/// directory. A format 1 store is converted under its lock before it is
/// read.
///
/// # Errors
/// [`CliError::IpcUnavailable`] when the machine has no data directory or
/// the store lock, migration, or read fails.
pub fn load_saved_server_store() -> Result<(PathBuf, ServerStore), CliError> {
    let private_data_directory = resolve_private_data_directory()?;
    let saved_server_store_path = resolve_server_store_path(&private_data_directory);
    let store_lock_file = acquire_saved_server_store_lock(&private_data_directory)?;
    migrate_saved_server_file(&saved_server_store_path).map_err(build_saved_server_store_error)?;
    let saved_server_store = ServerStore::load_server_store_from_path(&saved_server_store_path)
        .map_err(build_saved_server_store_error)?;
    drop(store_lock_file);
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
/// Takes the store's lock file, converts a format 1 store, reads it,
/// hands it to `update_store`, and writes it back. The lock is released when
/// this returns, either way. An
/// `update_store` that refuses stops the write, so the store on disk keeps
/// what it held.
///
/// The lock is taken again every 20 milliseconds for up to 5 seconds. A wait
/// that runs out returns an error naming the other koshi, and writes nothing.
///
/// Nothing inside `update_store` may ask the user a question: every other
/// koshi that changes the store waits for this one to finish.
///
/// # Errors
/// [`CliError::IpcUnavailable`] when the machine has no data directory, when
/// the lock could not be taken, when the store could not be read, and when it
/// could not be written. Whatever `update_store` reports, with nothing written.
pub fn update_saved_server_store<UpdateOutcome>(
    update_store: impl FnOnce(&mut ServerStore) -> Result<UpdateOutcome, CliError>,
) -> Result<UpdateOutcome, CliError> {
    let private_data_directory = resolve_private_data_directory()?;
    let saved_server_store_path = resolve_server_store_path(&private_data_directory);
    let store_lock_file = acquire_saved_server_store_lock(&private_data_directory)?;
    migrate_saved_server_file(&saved_server_store_path).map_err(build_saved_server_store_error)?;
    let mut saved_server_store = ServerStore::load_server_store_from_path(&saved_server_store_path)
        .map_err(build_saved_server_store_error)?;
    let store_update_outcome = update_store(&mut saved_server_store)?;
    saved_server_store
        .write_server_store_to_path(&saved_server_store_path)
        .map_err(build_saved_server_store_error)?;
    drop(store_lock_file);
    Ok(store_update_outcome)
}

/// Take the advisory lock on the file at `lock_file_path`, creating the file and the
/// directory holding it when they are missing.
///
/// Both are restricted to the owning user on Unix: mode `0700` on the
/// directory, set whether or not this call made it, and mode `0600` on a lock
/// file this call creates. On Windows both take the data directory's
/// owner-scoped ACLs.
///
/// The attempt is repeated every 20 ms for up to `lock_wait_duration`. Dropping
/// the returned file releases the lock, and so does the operating system when
/// the process holding it dies.
///
/// # Errors
/// The error detail: `held_lock_detail` when another process still held the
/// lock at the deadline, else the path and the operating system error of the
/// step that failed — making the directory, making it private, opening the
/// file, or attempting the lock.
pub fn acquire_store_lock(
    lock_file_path: &Path,
    lock_wait_duration: Duration,
    held_lock_detail: &str,
) -> Result<File, String> {
    if let Some(parent_directory) = lock_file_path.parent() {
        std::fs::create_dir_all(parent_directory).map_err(|io_error| {
            format!(
                "{} could not be made: {io_error}",
                parent_directory.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent_directory, std::fs::Permissions::from_mode(0o700))
                .map_err(|io_error| {
                    format!(
                        "{} could not be made private: {io_error}",
                        parent_directory.display()
                    )
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
        format!(
            "{} could not be opened: {io_error}",
            lock_file_path.display()
        )
    })?;
    let lock_deadline = Instant::now() + lock_wait_duration;
    loop {
        match FileExt::try_lock(&lock_file) {
            Ok(()) => return Ok(lock_file),
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= lock_deadline {
                    return Err(held_lock_detail.to_string());
                }
                std::thread::sleep(STORE_LOCK_POLL_INTERVAL_DURATION);
            }
            Err(TryLockError::Error(io_error)) => {
                return Err(format!(
                    "{} could not be locked: {io_error}",
                    lock_file_path.display()
                ))
            }
        }
    }
}

/// The saved-server store lock under `private_data_directory`, taken through
/// [`acquire_store_lock`] with [`STORE_LOCK_TIMEOUT_DURATION`].
///
/// # Errors
/// [`CliError::IpcUnavailable`] carrying the detail [`acquire_store_lock`]
/// reports; a lock still held at the deadline reads
/// `another koshi is changing the saved servers; try again`.
fn acquire_saved_server_store_lock(private_data_directory: &Path) -> Result<File, CliError> {
    acquire_store_lock(
        &resolve_server_store_lock_path(private_data_directory),
        STORE_LOCK_TIMEOUT_DURATION,
        SAVED_SERVER_STORE_HELD_DETAIL,
    )
    .map_err(|error_detail| CliError::IpcUnavailable {
        detail: error_detail,
    })
}

/// A saved-server store that could not be read or written.
fn build_saved_server_store_error(ipc_error: IpcError) -> CliError {
    CliError::IpcUnavailable {
        detail: ipc_error.to_string(),
    }
}

/// The server selector names: a saved record whose name or address matches the
/// selector, or a server this machine has not saved when the selector is an
/// address.
///
/// `certificate_fingerprint_text` is the value given with
/// `--certificate-fingerprint`, such as `sha256:` followed by 64 lowercase hex
/// characters, or `None` when none was given. A server this machine has not
/// saved needs it. A saved one takes it when it names the fingerprint the
/// record holds, and refuses any other.
///
/// A saved one is refused when `KOSHI_REMOTE_SECRET`, with its surrounding
/// whitespace trimmed, holds a secret that no saved server holds, or bytes that
/// are not UTF-8. An unset variable, an empty value, and a value any saved
/// server holds pass, and the saved one presents the secret it holds.
///
/// Example — `work` matches the record the user named `work`, and
/// `laptop.local:7654` with no matching record and a fingerprint given is
/// [`ServerReference::New`].
///
/// A selector that matches more than one record is refused, and no dial is
/// made.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the selector matches no record and is not an
/// address, when it matches more than one, when it matches a saved server and
/// a fingerprint other than the one that record holds is given, when it
/// matches a saved server and `KOSHI_REMOTE_SECRET` holds a secret no saved
/// server holds, when a given fingerprint fails
/// [`parse_certificate_fingerprint`], and when it is an address no record holds
/// and the fingerprint is missing.
/// [`CliError::IpcUnavailable`] when the saved-server store cannot be read.
pub fn resolve_server(
    server_selector_text: &str,
    certificate_fingerprint_text: Option<&str>,
) -> Result<ServerReference, CliError> {
    let (_, saved_server_store) = load_saved_server_store()?;
    let server_reference = resolve_server_reference(
        saved_server_store.find_saved_server(server_selector_text),
        server_selector_text,
        certificate_fingerprint_text,
    )?;
    if let ServerReference::Saved(saved_server) = &server_reference {
        validate_environment_secret(
            &saved_server_store.saved_servers,
            saved_server,
            std::env::var_os(SECRET_ENVIRONMENT_VARIABLE).as_deref(),
        )?;
    }
    Ok(server_reference)
}

/// The saved server that a row of [`list_saved_server_session_rows`] names by
/// `server_label`, its saved name or else its address, as
/// [`resolve_server`] resolves it with no `--certificate-fingerprint`.
/// `KOSHI_REMOTE_SECRET` is not read.
///
/// # Errors
/// [`CliError::InvalidArgs`] when no single saved server answers to
/// `server_label`. [`CliError::IpcUnavailable`] when the saved-server store
/// cannot be read.
pub fn resolve_listed_server(server_label: &str) -> Result<ServerReference, CliError> {
    let (_, saved_server_store) = load_saved_server_store()?;
    resolve_server_reference(
        saved_server_store.find_saved_server(server_label),
        server_label,
        None,
    )
}

/// Why [`find_saved_server_at_address`] found no single saved server at an
/// address. The text names what happened.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SavedServerAtAddressError {
    /// The saved-server store holds no server at `server_address`: it was
    /// forgotten, or saved under another address.
    #[error(
        "no saved server is at {server_address} any more: it was forgotten, or saved under \
         another address"
    )]
    NotSaved {
        /// The address looked up, as `host:port`.
        server_address: String,
    },
    /// More than one server in the saved-server store answers to
    /// `server_address`.
    #[error("more than one saved server is at {server_address}")]
    SavedMoreThanOnce {
        /// The address looked up, as `host:port`.
        server_address: String,
    },
    /// The saved-server store could not be read.
    #[error("the saved servers could not be read: {store_error_detail}")]
    StoreUnreadable {
        /// What reading the store reported, such as `another koshi is
        /// changing the saved servers; try again`.
        store_error_detail: String,
    },
}

/// The server the saved-server store holds at `server_address` now, read
/// with its lock held. `KOSHI_REMOTE_SECRET` is not read.
///
/// Example — a server saved as `work` at `laptop.local:7654` and renamed
/// `lab` gives the server named `lab`; once forgotten, it gives
/// [`SavedServerAtAddressError::NotSaved`].
///
/// # Errors
/// [`SavedServerAtAddressError::NotSaved`] when no saved server answers to
/// `server_address`, [`SavedServerAtAddressError::SavedMoreThanOnce`] when
/// more than one does, and [`SavedServerAtAddressError::StoreUnreadable`]
/// carrying the reason when the store cannot be read.
pub fn find_saved_server_at_address(
    server_address: &str,
) -> Result<SavedServer, SavedServerAtAddressError> {
    let saved_server_store = match load_saved_server_store() {
        Ok((_, saved_server_store)) => saved_server_store,
        Err(CliError::IpcUnavailable { detail }) => {
            return Err(SavedServerAtAddressError::StoreUnreadable {
                store_error_detail: detail,
            })
        }
        Err(store_error) => {
            return Err(SavedServerAtAddressError::StoreUnreadable {
                store_error_detail: store_error.to_string(),
            })
        }
    };
    match saved_server_store.find_saved_server(server_address) {
        SavedServerLookup::Saved(saved_server) => Ok(saved_server.clone()),
        SavedServerLookup::NotSaved => Err(SavedServerAtAddressError::NotSaved {
            server_address: server_address.to_string(),
        }),
        SavedServerLookup::Ambiguous => Err(SavedServerAtAddressError::SavedMoreThanOnce {
            server_address: server_address.to_string(),
        }),
    }
}

/// Refuse a dial to `saved_server` when `environment_secret`, the value of
/// `KOSHI_REMOTE_SECRET`, holds a secret that no server in `saved_servers`
/// holds.
///
/// The value is compared with its surrounding whitespace trimmed. An unset
/// variable, and a value that is empty once trimmed, give no secret, and pass.
/// A value that is not UTF-8 is refused.
///
/// Example — with `work` holding `aaaa` and `desk` holding `bbbb`, the values
/// `aaaa`, `bbbb`, and `aaaa` followed by a line break pass for `work`, and
/// `cccc` is refused.
///
/// # Errors
/// [`CliError::InvalidArgs`] naming `saved_server` and `koshi remote
/// set-secret` when the value holds a secret no saved server holds.
fn validate_environment_secret(
    saved_servers: &[SavedServer],
    saved_server: &SavedServer,
    environment_secret: Option<&OsStr>,
) -> Result<(), CliError> {
    let Some(environment_secret) = environment_secret else {
        return Ok(());
    };
    if let Some(environment_secret_text) = environment_secret.to_str() {
        let trimmed_secret_text = environment_secret_text.trim();
        if trimmed_secret_text.is_empty() {
            return Ok(());
        }
        let environment_token = ConnectionToken::from_secret(trimmed_secret_text);
        if saved_servers
            .iter()
            .any(|listed_server| listed_server.connection_token == environment_token)
        {
            return Ok(());
        }
    }
    let saved_server_label = format_saved_server_label(saved_server);
    Err(CliError::InvalidArgs {
        detail: format!(
            "{saved_server_label} holds a saved secret, and {SECRET_ENVIRONMENT_VARIABLE} gives \
             one that no saved server holds; run `koshi remote set-secret {saved_server_label}` \
             to replace the saved secret with it, or unset {SECRET_ENVIRONMENT_VARIABLE} to dial \
             with the saved one"
        ),
    })
}

/// Which server the selector names, given what the store said about it and
/// the text given with `--certificate-fingerprint`.
///
/// [`SavedServerLookup::Saved`] dials with the fingerprint that record holds.
/// It takes a `certificate_fingerprint_text` naming that fingerprint, and
/// refuses another one. A record holding no fingerprint takes any well-formed
/// `certificate_fingerprint_text`, and the dial refuses that record.
/// [`SavedServerLookup::NotSaved`] with an address shape dials with the
/// fingerprint `certificate_fingerprint_text` names, and refuses to go on
/// without one.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the selector names nothing and is not an
/// address, when it names more than one saved server, when it names a saved
/// server that holds a fingerprint other than the one
/// `certificate_fingerprint_text` names, when `certificate_fingerprint_text`
/// is `Some` and fails [`parse_certificate_fingerprint`], and when it is an
/// address nothing is saved under and `certificate_fingerprint_text` is
/// `None`.
fn resolve_server_reference(
    server_lookup: SavedServerLookup<'_>,
    server_selector_text: &str,
    certificate_fingerprint_text: Option<&str>,
) -> Result<ServerReference, CliError> {
    match server_lookup {
        SavedServerLookup::Saved(saved_server) => {
            let Some(certificate_fingerprint_text) = certificate_fingerprint_text else {
                return Ok(ServerReference::Saved(saved_server.clone()));
            };
            let certificate_fingerprint =
                parse_certificate_fingerprint(certificate_fingerprint_text)?;
            match saved_server.certificate_fingerprint.as_deref() {
                Some(saved_certificate_fingerprint)
                    if saved_certificate_fingerprint != certificate_fingerprint =>
                {
                    let saved_server_label = format_saved_server_label(saved_server);
                    Err(CliError::InvalidArgs {
                        detail: format!(
                            "{saved_server_label} expects the certificate fingerprint {}, and \
                             --certificate-fingerprint gives {}; run `koshi remote edit \
                             {saved_server_label}` to change the fingerprint it expects",
                            format_certificate_fingerprint(saved_certificate_fingerprint),
                            format_certificate_fingerprint(&certificate_fingerprint)
                        ),
                    })
                }
                // The fingerprint the record holds, or a record that holds none,
                // which the dial refuses.
                _ => Ok(ServerReference::Saved(saved_server.clone())),
            }
        }
        SavedServerLookup::Ambiguous => {
            Err(build_ambiguous_saved_server_error(server_selector_text))
        }
        SavedServerLookup::NotSaved if is_server_address(server_selector_text) => {
            let Some(certificate_fingerprint_text) = certificate_fingerprint_text else {
                return Err(CliError::InvalidArgs {
                    detail: format!(
                        "{server_selector_text} is not saved, so the fingerprint of its \
                         certificate is needed: add `--certificate-fingerprint <FINGERPRINT>` \
                         with the fingerprint `koshi share grant` printed on that machine"
                    ),
                });
            };
            Ok(ServerReference::New {
                server_address: server_selector_text.to_string(),
                certificate_fingerprint: parse_certificate_fingerprint(
                    certificate_fingerprint_text,
                )?,
            })
        }
        SavedServerLookup::NotSaved => {
            Err(build_saved_server_not_found_error(server_selector_text))
        }
    }
}

/// The refusal of `server_reference`, a word typed to name a saved server,
/// that no saved server answers to. A word that is 64 lowercase hex
/// characters once its surrounding whitespace is trimmed, the shape of a
/// secret, is not repeated.
///
/// Example — `work` gives ``no saved server is named work; run `koshi remote
/// list` ``.
#[must_use]
pub fn build_saved_server_not_found_error(server_reference: &str) -> CliError {
    let detail = if is_secret_shaped(server_reference) {
        "no saved server is named by the 64 hex characters given, the shape of a secret, and a \
         secret is never given here; run `koshi remote list`"
            .to_string()
    } else {
        format!("no saved server is named {server_reference}; run `koshi remote list`")
    };
    CliError::InvalidArgs { detail }
}

/// The refusal of `server_reference`, a word typed to name a saved server,
/// that more than one saved server answers to. A word that is 64 lowercase
/// hex characters once its surrounding whitespace is trimmed, the shape of a
/// secret, is not repeated.
///
/// Example — `work` gives ``work is the name of one saved server and the
/// address of another; run `koshi remote list` and name the one you mean``.
#[must_use]
pub fn build_ambiguous_saved_server_error(server_reference: &str) -> CliError {
    let detail = if is_secret_shaped(server_reference) {
        "the 64 hex characters given are the name of one saved server and the address of \
         another; run `koshi remote list` and name the one you mean"
            .to_string()
    } else {
        format!(
            "{server_reference} is the name of one saved server and the address of another; run \
             `koshi remote list` and name the one you mean"
        )
    };
    CliError::InvalidArgs { detail }
}

/// Whether the selector has the `host:port` shape: a host before the last colon, and
/// a port of decimal digits after it.
///
/// A host holding a colon of its own must be bracketed: a bare IPv6 literal is
/// not an address. The port is digits alone: no sign and no spaces.
///
/// Example — `laptop.local:7654` and `[::1]:22` are addresses; `work`,
/// `laptop.local`, `laptop.local:door`, `2001:db8::1` and `desk.local:+7654` are
/// not.
#[must_use]
pub fn is_server_address(server_selector_text: &str) -> bool {
    let Some((host_text, port_text)) = server_selector_text.rsplit_once(':') else {
        return false;
    };
    if host_text.is_empty() || port_text.is_empty() {
        return false;
    }
    let is_host_shaped =
        !host_text.contains(':') || (host_text.starts_with('[') && host_text.ends_with(']'));
    is_host_shaped
        && port_text
            .bytes()
            .all(|port_byte| port_byte.is_ascii_digit())
        && port_text.parse::<u16>().is_ok()
}

/// Refuse a saved name that is empty, that has the shape of a secret, or that
/// has the `host:port` shape.
///
/// A name has the shape of a secret when it is 64 lowercase hex characters
/// once its surrounding whitespace is trimmed. Its refusal does not repeat it.
///
/// # Errors
/// [`CliError::InvalidArgs`] naming the shape, and for an empty name.
pub fn validate_saved_server_name(saved_server_name: &str) -> Result<(), CliError> {
    if saved_server_name.is_empty() {
        return Err(CliError::InvalidArgs {
            detail: "a saved name must not be empty. Pick a plain name.".to_string(),
        });
    }
    if is_secret_shaped(saved_server_name) {
        return Err(CliError::InvalidArgs {
            detail: "a saved name must not be 64 hex characters, the shape of a secret. Pick a \
                     plain name."
                .to_string(),
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

/// The 64 lowercase hex characters of the certificate fingerprint
/// `certificate_fingerprint_text` names, typed as `sha256:` followed by them,
/// the shape `koshi share grant` prints.
///
/// A refusal never repeats `certificate_fingerprint_text`. The refusal of a
/// text that is 64 lowercase hex characters once its surrounding whitespace
/// is trimmed says that this is the shape of a secret.
///
/// Example — `sha256:` followed by 64 `a` characters gives the 64 `a`
/// characters. 64 `a` characters alone, `SHA256:` followed by 64 `a`
/// characters, and `sha256:` followed by 63 `a` characters are refused.
///
/// # Errors
/// [`CliError::InvalidArgs`] naming the shape.
pub fn parse_certificate_fingerprint(
    certificate_fingerprint_text: &str,
) -> Result<String, CliError> {
    if let Some(certificate_fingerprint) =
        certificate_fingerprint_text.strip_prefix(CERTIFICATE_FINGERPRINT_PREFIX)
    {
        if is_32_byte_lowercase_hex(certificate_fingerprint) {
            return Ok(certificate_fingerprint.to_string());
        }
    }
    let shape_sentence = "a certificate fingerprint is sha256: followed by 64 lowercase hex \
                          characters, as `koshi share grant` prints it on the machine that serves";
    let refusal_detail = if is_secret_shaped(certificate_fingerprint_text) {
        format!(
            "{shape_sentence}. 64 hex characters alone is the shape of a secret, and a secret is \
             never given here"
        )
    } else {
        shape_sentence.to_string()
    };
    Err(CliError::InvalidArgs {
        detail: refusal_detail,
    })
}

/// Whether `hex_text` is 64 lowercase hex characters: 32 bytes written as
/// [`format_hex`](koshi_ipc::bytes::format_hex) writes them, the shape of a
/// secret and of a certificate fingerprint.
///
/// Example — 64 `a` characters are; 64 `A` characters, 63 `a` characters,
/// and 64 `g` characters are not.
fn is_32_byte_lowercase_hex(hex_text: &str) -> bool {
    hex_text.len() == 64
        && hex_text
            .bytes()
            .all(|hex_byte| matches!(hex_byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Whether `typed_text` has the shape of a secret: 64 lowercase hex
/// characters once its surrounding whitespace is trimmed.
///
/// Example — 64 `a` characters, alone or with a space before them or a line
/// break after them, are; 64 `A` characters and 64 `a` characters followed by
/// ` b` are not.
fn is_secret_shaped(typed_text: &str) -> bool {
    is_32_byte_lowercase_hex(typed_text.trim())
}

/// Refuse a saved name that another saved server already answers to, by its
/// own name or its own address ([`ServerStore::is_server_name_free`]). The
/// server at `server_address` may keep a name it holds. Reads the store.
///
/// # Errors
/// [`CliError::InvalidArgs`] when another address already holds the saved
/// name. [`CliError::IpcUnavailable`] when the store cannot be read.
fn validate_save_as_is_free(saved_server_name: &str, server_address: &str) -> Result<(), CliError> {
    let (_, saved_server_store) = load_saved_server_store()?;
    if !saved_server_store.is_server_name_free(saved_server_name, server_address) {
        let owning_server_address = saved_server_store
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
                "the name {saved_server_name} already belongs to {owning_server_address}; run `koshi remote forget {saved_server_name}` \
                 first, or pick another name"
            ),
        });
    }
    Ok(())
}

/// The secret to present to the server at `server_address`.
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
        Ok(environment_secret_text) => environment_secret_text,
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

/// Print `prompt` on standard error, then read one secret from the terminal
/// without printing what is typed, with surrounding whitespace trimmed. The
/// answer can be empty.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the terminal could not be read, when the
/// entry was interrupted with `0x03`, and when the input ended before an
/// answer arrived.
pub fn prompt_secret(prompt: &str) -> Result<String, CliError> {
    Ok(read_terminal_secret(prompt)?.trim().to_string())
}

/// Print `prompt` on standard error, then read one line from the terminal,
/// which the terminal echoes, with surrounding whitespace trimmed. The answer
/// can be empty.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the terminal could not be read, and when the
/// input ended before a line arrived.
pub fn prompt_line(prompt: &str) -> Result<String, CliError> {
    eprint!("{prompt}");
    Ok(read_terminal_line()?.trim().to_string())
}

/// Print `prompt` on standard error, then read one secret from the terminal
/// without printing what is typed.
///
/// The terminal is put in raw mode while the secret is typed, and a line break
/// follows on standard error once the secret is read. A terminal that cannot
/// be put in raw mode reads one plain line instead, which the terminal echoes.
fn read_terminal_secret(prompt: &str) -> Result<String, CliError> {
    eprint!("{prompt}");
    if crossterm::terminal::enable_raw_mode().is_err() {
        return read_terminal_line();
    }
    let hidden_line_read = read_hidden_terminal_line(&mut io::stdin().lock());
    let _ = crossterm::terminal::disable_raw_mode();
    eprintln!();
    hidden_line_read.map_err(build_prompt_error)
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
/// `saved_server_label` is the name, else the address, of the saved server
/// whose secret `connection_token` is, or `None` for a secret given to this
/// dial. When the server does not admit the secret, the last step of the
/// refusal names `koshi remote set-secret <saved_server_label>`. With `None`
/// it names `koshi remote set-secret` for a saved server, or the secret given
/// when the next dial asks.
///
/// `expected_certificate_fingerprint` is the fingerprint the server's
/// certificate must have. A server presenting another certificate is refused
/// before `connection_token` is sent. The last step of that refusal names
/// `koshi remote edit <saved_server_label>`. With `None` it names
/// `--certificate-fingerprint`, or `koshi remote edit` for a saved server.
///
/// `dial_timeout` bounds the connect, the TLS handshake and the secret exchange
/// together. The name lookup before them is the operating system's own and
/// carries no timeout. `reply_timeout` bounds the reply after the connection
/// opens when it is `Some`.
///
/// # Errors
/// [`DialError::Unreachable`] when the connection could not be opened or the
/// exchange ran out of time. [`DialError::Refused`] when the certificate is not
/// the expected one, the server did not admit the secret, the remote protocol
/// version it settled on is one this build does not speak, or it answered
/// something else.
pub fn connect_remote_server(
    server_address: &str,
    connection_token: &ConnectionToken,
    saved_server_label: Option<&str>,
    expected_certificate_fingerprint: &str,
    dial_timeout: Duration,
    reply_timeout: Option<Duration>,
) -> Result<RemoteLink, DialError> {
    let remote_hello = RemoteClientFrame::Hello {
        minimum_remote_version: MIN_REMOTE_PROTOCOL_VERSION,
        maximum_remote_version: REMOTE_PROTOCOL_VERSION,
        minimum_protocol_version: MIN_PROTOCOL_VERSION,
        maximum_protocol_version: PROTOCOL_VERSION,
        connection_token: connection_token.clone(),
    };
    let (frame_reader, frame_writer, remote_server_answer) = remote_wire::open_remote_connection(
        server_address,
        expected_certificate_fingerprint,
        &remote_hello,
        dial_timeout,
        reply_timeout,
    )
    .map_err(|ipc_error| classify_dial_failure(ipc_error, saved_server_label))?;
    validate_remote_server_answer(server_address, saved_server_label, &remote_server_answer)?;
    Ok(RemoteLink {
        frame_reader,
        frame_writer,
    })
}

/// How one dial's transport failure classifies: a certificate that is not the
/// expected one is [`DialError::Refused`], naming `saved_server_label` as the
/// saved server whose fingerprint `koshi remote edit` changes. Every other
/// transport failure is [`DialError::Unreachable`]. The message is
/// [`build_ipc_unavailable_error`]'s either way.
fn classify_dial_failure(ipc_error: IpcError, saved_server_label: Option<&str>) -> DialError {
    match ipc_error {
        IpcError::CertificateMismatch {
            server_address,
            expected_certificate_fingerprint,
            presented_certificate_fingerprint,
            ..
        } => DialError::Refused(build_ipc_unavailable_error(IpcError::CertificateMismatch {
            server_address,
            expected_certificate_fingerprint,
            presented_certificate_fingerprint,
            saved_server_label: saved_server_label.map(str::to_string),
        })),
        _ => DialError::Unreachable(build_ipc_unavailable_error(ipc_error)),
    }
}

/// `Ok(())` when `remote_server_frame` is a `Welcome` carrying a remote
/// protocol version between [`MIN_REMOTE_PROTOCOL_VERSION`] and
/// [`REMOTE_PROTOCOL_VERSION`], else the [`DialError::Refused`] to report.
///
/// A `Refused` frame carrying
/// [`REMOTE_REFUSED`](koshi_ipc::remote_wire::REMOTE_REFUSED) gives a refusal
/// that names these causes, in this order, and the steps for them:
///
/// - The token expired, was revoked, or was never granted on that machine, or
///   that machine already serves as many remote connections as it admits:
///   `koshi share list` there shows when each grant expires or was revoked.
///   Run `koshi share grant` there, then store the new secret with `koshi
///   remote set-secret <saved_server_label>` when `saved_server_label` is
///   `Some`. With `None` the step reads: store the new secret with `koshi
///   remote set-secret` for a saved server, or give it when the next dial
///   asks.
/// - Only when the frame carries no `remote_protocol_version`: that machine
///   runs koshi 0.3.0 or 0.4.0, which cannot read this koshi's request.
///   Update koshi there.
///
/// Any other refusal message is the server's own sentence, filtered by
/// [`sanitize_reported_text`], with `server_address` after it.
///
/// Every refusal built here carries [`CliError::Runtime`].
///
/// Example — a `Refused` frame carrying `"the session is gone"` from
/// `desk.local:7654` reads `"the session is gone (server desk.local:7654)"`.
/// A frame carrying `"\u{1b}[2Jgone"` reads `"gone (server desk.local:7654)"`.
fn validate_remote_server_answer(
    server_address: &str,
    saved_server_label: Option<&str>,
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
        } => Err(DialError::Refused(CliError::Runtime {
            detail: format!(
                "server {server_address} settled on remote protocol version \
                 {remote_protocol_version}, which this koshi does not speak: it speaks \
                 {MIN_REMOTE_PROTOCOL_VERSION} to {REMOTE_PROTOCOL_VERSION}"
            ),
        })),
        RemoteServerFrame::Refused {
            message: refusal_message,
            remote_protocol_version,
        } if refusal_message == remote_wire::REMOTE_REFUSED => {
            let (refusal_causes, previous_release_step) = match remote_protocol_version {
                Some(_) => (
                    "the token expired, was revoked, or was never granted on that machine, or \
                     that machine already serves as many remote connections as it admits",
                    "",
                ),
                None => (
                    "the token expired, was revoked, or was never granted on that machine, that \
                     machine already serves as many remote connections as it admits, or it runs \
                     koshi 0.3.0 or 0.4.0, which cannot read this koshi's request",
                    ". if that machine runs koshi 0.3.0 or 0.4.0, update koshi there",
                ),
            };
            let new_secret_step = match saved_server_label {
                Some(saved_server_label) => {
                    format!("`koshi remote set-secret {saved_server_label}`")
                }
                None => "`koshi remote set-secret` for a saved server, or give it when the next \
                         dial asks"
                    .to_string(),
            };
            Err(DialError::Refused(CliError::Runtime {
                detail: format!(
                    "the server {server_address} did not admit the connection: {refusal_causes}. \
                     `koshi share list` on that machine shows when each grant expires or was \
                     revoked. to replace the token, run `koshi share grant` there, then store \
                     the new secret with {new_secret_step}{previous_release_step}"
                ),
            }))
        }
        RemoteServerFrame::Refused {
            message: refusal_message,
            ..
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
/// A saved server presents the secret its record holds and expects the
/// fingerprint its record holds, and its last-used time is stamped once the
/// connection opens. A record holding no fingerprint is refused, and nothing
/// is dialled. A store that will not take the stamp leaves a log line and the
/// connection stands.
///
/// A server reached for the first time asks for its secret ([`resolve_server_connection_token`]),
/// expects the fingerprint its [`ServerReference::New`] carries, and is saved
/// with that fingerprint under `save_as` once it admits the connection. A store
/// that will not take that record fails the call.
///
/// `save_as` is the name the server is saved under. A `save_as` that
/// [`validate_saved_server_name`] refuses is refused first, whatever the
/// server holds, and nothing is dialled. A saved server that holds
/// no name takes it once it admits the connection, and the stamp of its
/// last-used time goes into the same write. A store that will not take that
/// name fails the call. A saved server that holds `save_as` already is dialled
/// as with no `save_as`. A saved server that holds another name refuses
/// `save_as`, and nothing is dialled.
///
/// `reply_timeout` is passed straight to [`connect_remote_server`].
///
/// The saved record comes back alongside the connection.
///
/// # Errors
/// Whatever [`connect_remote_server`] reports. [`DialError::Refused`] carrying
/// [`CliError::InvalidArgs`] when [`validate_saved_server_name`] refuses
/// `save_as`, when `save_as` differs from the name a saved
/// server holds, when a saved server holds no fingerprint, when the name cannot be
/// given to that address, when the saved server changed while the connection
/// opened, and when no secret was given; carrying
/// [`CliError::IpcUnavailable`] when the record could not be saved.
pub fn connect_saved_server(
    server_reference: &ServerReference,
    save_as: Option<&str>,
    reply_timeout: Option<Duration>,
) -> Result<(RemoteLink, SavedServer), DialError> {
    if let Some(save_as_name) = save_as {
        validate_saved_server_name(save_as_name).map_err(DialError::Refused)?;
    }
    match server_reference {
        ServerReference::Saved(saved_server_record) => {
            let server_name_to_give = match (save_as, saved_server_record.server_name.as_deref()) {
                (Some(save_as_name), None) => {
                    validate_save_as_is_free(save_as_name, &saved_server_record.server_address)
                        .map_err(DialError::Refused)?;
                    Some(save_as_name)
                }
                (Some(save_as_name), Some(saved_server_name))
                    if save_as_name != saved_server_name =>
                {
                    return Err(DialError::Refused(CliError::InvalidArgs {
                        detail: format!(
                            "{} is already saved as {saved_server_name}; run `koshi remote \
                                 edit {saved_server_name}` to rename it to {save_as_name}",
                            saved_server_record.server_address
                        ),
                    }));
                }
                // No `save_as`, or `save_as` names the name the record holds.
                _ => None,
            };
            let remote_link = dial_saved_server(saved_server_record, reply_timeout)?;
            let connection_opened_at = SystemTime::now();
            if let Some(save_as_name) = server_name_to_give {
                let named_saved_server = update_saved_server_store(|saved_server_store| {
                    name_saved_server(
                        saved_server_store,
                        &saved_server_record.server_address,
                        save_as_name,
                        connection_opened_at,
                    )
                })
                .map_err(DialError::Refused)?;
                return Ok((remote_link, named_saved_server));
            }
            let store_update_result = update_saved_server_store(|saved_server_store| {
                saved_server_store
                    .mark_server_used(&saved_server_record.server_address, connection_opened_at);
                Ok(())
            });
            if let Err(store_update_error) = store_update_result {
                tracing::warn!(%store_update_error, "the record was not updated");
            }
            let mut updated_saved_server = saved_server_record.clone();
            updated_saved_server.last_used_at = Some(connection_opened_at);
            Ok((remote_link, updated_saved_server))
        }
        ServerReference::New {
            server_address,
            certificate_fingerprint,
        } => {
            if let Some(save_as_name) = save_as {
                validate_save_as_is_free(save_as_name, server_address)
                    .map_err(DialError::Refused)?;
            }
            let connection_token =
                resolve_server_connection_token(server_address).map_err(DialError::Refused)?;
            let remote_link = connect_remote_server(
                server_address,
                &connection_token,
                None,
                certificate_fingerprint,
                DIAL_TIMEOUT_DURATION,
                reply_timeout,
            )?;
            let connection_opened_at = SystemTime::now();
            let new_saved_server = SavedServer {
                server_name: save_as.map(str::to_string),
                server_address: server_address.clone(),
                connection_token,
                certificate_fingerprint: Some(certificate_fingerprint.clone()),
                added_at: connection_opened_at,
                last_used_at: Some(connection_opened_at),
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

/// Give the saved server at `server_address` the name `server_name` and the
/// last-used time `last_used_at`, and return that server as `server_store`
/// now holds it.
///
/// The server must hold no name, or `server_name` already. Its secret, its
/// fingerprint and its added time stay as `server_store` holds them. It moves
/// to the end of `saved_servers`, as [`ServerStore::save_server`] places every
/// server it saves. A refusal leaves `server_store` as it was.
///
/// The store is not written; the caller does that.
///
/// # Errors
/// [`CliError::InvalidArgs`] when no single saved server is at `server_address`,
/// when that server holds another name, and when another saved server answers
/// to `server_name`.
fn name_saved_server(
    server_store: &mut ServerStore,
    server_address: &str,
    server_name: &str,
    last_used_at: SystemTime,
) -> Result<SavedServer, CliError> {
    let changed_server_error = CliError::InvalidArgs {
        detail: format!(
            "{server_address} changed while this connection opened, so it was not named \
             {server_name}; run the command again"
        ),
    };
    let SavedServerLookup::Saved(stored_saved_server) =
        server_store.find_saved_server(server_address)
    else {
        return Err(changed_server_error);
    };
    if stored_saved_server
        .server_name
        .as_deref()
        .is_some_and(|stored_server_name| stored_server_name != server_name)
    {
        return Err(changed_server_error);
    }
    let named_saved_server = SavedServer {
        server_name: Some(server_name.to_string()),
        last_used_at: Some(last_used_at),
        ..stored_saved_server.clone()
    };
    server_store
        .save_server(named_saved_server.clone())
        .map_err(|name_taken_error| CliError::InvalidArgs {
            detail: name_taken_error.to_string(),
        })?;
    Ok(named_saved_server)
}

/// Open a connection to the saved server `saved_server_record` names,
/// presenting the secret its record holds and expecting the fingerprint its
/// record holds, as [`connect_remote_server`] does with the record's name,
/// else its address, [`DIAL_TIMEOUT_DURATION`] and `reply_timeout`. The
/// saved-server store is not read or written.
///
/// # Errors
/// [`DialError::Refused`] carrying [`CliError::InvalidArgs`] when the record
/// holds no fingerprint, and nothing is dialled. Whatever
/// [`connect_remote_server`] reports.
fn dial_saved_server(
    saved_server_record: &SavedServer,
    reply_timeout: Option<Duration>,
) -> Result<RemoteLink, DialError> {
    let Some(saved_certificate_fingerprint) =
        saved_server_record.certificate_fingerprint.as_deref()
    else {
        return Err(DialError::Refused(CliError::InvalidArgs {
            detail: format_missing_fingerprint_refusal(&format_saved_server_label(
                saved_server_record,
            )),
        }));
    };
    connect_remote_server(
        &saved_server_record.server_address,
        &saved_server_record.connection_token,
        Some(&format_saved_server_label(saved_server_record)),
        saved_certificate_fingerprint,
        DIAL_TIMEOUT_DURATION,
        reply_timeout,
    )
}

/// The sessions this connection's secret reaches, in the order the server
/// holds them, each name carrying the bytes the server sent.
///
/// # Errors
/// [`CliError::Runtime`] when the server refused the request, carrying the
/// server's own sentence filtered by [`sanitize_reported_text`], and
/// [`CliError::IpcUnavailable`] when the exchange failed or the server
/// answered something else.
pub fn list_remote_sessions(
    remote_link: &mut RemoteLink,
) -> Result<Vec<RemoteSessionRow>, CliError> {
    remote_link
        .frame_writer
        .send(&RemoteClientFrame::List)
        .map_err(build_ipc_unavailable_error)?;
    match remote_link
        .frame_reader
        .recv::<RemoteServerFrame>()
        .map_err(build_ipc_unavailable_error)?
    {
        RemoteServerFrame::Sessions { session_rows } => Ok(session_rows),
        RemoteServerFrame::Refused { message, .. } => Err(CliError::Runtime {
            detail: sanitize_reported_text(&message),
        }),
        RemoteServerFrame::Welcome { .. } => Err(CliError::IpcUnavailable {
            detail: format_unexpected_remote_answer("the server", "Welcome"),
        }),
    }
}

/// Ask to attach to `session_selector` and hand the connection's two halves
/// back.
///
/// The bytes after this belong to that session's own server. The machine
/// serving it sends the session-plane Hello carrying that session's endpoint
/// token and the versions this build named. The next frame the caller reads
/// is that session server's Hello answer.
///
/// `link_timeout` goes up in the Attach as `link_timeout_ms`, in whole
/// milliseconds: the serving machine ends the connection once a socket write
/// to this caller takes no byte, or a socket read from this caller gets no
/// byte, for that long. A socket read before this caller's first byte after
/// the Attach waits for the larger of that and 20 seconds. A timeout past
/// `u32::MAX` milliseconds goes up as `u32::MAX`.
///
/// # Errors
/// [`CliError::IpcUnavailable`] when the request could not be sent.
pub fn attach_remote_session(
    remote_link: RemoteLink,
    session_selector: SessionSelector,
    link_timeout: Duration,
) -> Result<(FrameReader, FrameWriter), CliError> {
    let RemoteLink {
        frame_reader,
        mut frame_writer,
    } = remote_link;
    let link_timeout_ms = u32::try_from(link_timeout.as_millis()).unwrap_or(u32::MAX);
    frame_writer
        .send(&RemoteClientFrame::Attach {
            session_selector,
            link_timeout_ms: Some(link_timeout_ms),
        })
        .map_err(build_ipc_unavailable_error)?;
    Ok((frame_reader, frame_writer))
}

/// Read the frame that answers the Hello the serving machine wrote on this
/// caller's behalf after an Attach for `session_selector`.
///
/// `server_address` is the address of the serving machine. Every refusal read
/// here names it.
///
/// Two senders write this one frame: the serving machine writes a refusal when
/// it does not attach the caller, and otherwise the session server's own
/// answer arrives unread through the bridge. The frame is held as its JSON
/// text and parsed as a refusal first, then as an [`IncomingResponse`].
///
/// Example — a `Refused` frame carrying [`ROUTER_RESTARTING_MESSAGE`] is
/// [`DialError::Restarting`]. One carrying
/// [`SESSION_NOT_ANSWERING_MESSAGE`] from `desk.local:7654` for the selector
/// `quiet-lake` is [`DialError::Refused`] reading `session quiet-lake did not
/// answer on the server desk.local:7654`, and one carrying any other sentence
/// reads `the token saved for the server desk.local:7654 does not reach
/// session quiet-lake`. An answer from a koshi 0.4.0 session
/// `quiet-lake` is [`DialError::Refused`] reading `session quiet-lake answered
/// in the format of koshi 0.4.0 or older, which this koshi cannot talk to; on
/// the machine that serves it, the user who started it runs: koshi
/// restart-servers`.
///
/// # Errors
/// [`DialError::Unreachable`] when the frame could not be read at all.
/// [`DialError::Restarting`] for a refusal carrying
/// [`ROUTER_RESTARTING_MESSAGE`]. [`DialError::Refused`] for every other
/// refusal, for an answer from a session that koshi 0.1.0 to 0.4.0 started,
/// and for an answer that parses as neither, carrying the parse error
/// filtered by [`sanitize_reported_text`].
pub fn read_forwarded_hello_answer(
    frame_reader: &mut FrameReader,
    server_address: &str,
    session_selector: &SessionSelector,
) -> Result<IncomingResponse, DialError> {
    let hello_answer_frame: Box<RawValue> = frame_reader
        .recv()
        .map_err(|ipc_error| DialError::Unreachable(build_ipc_unavailable_error(ipc_error)))?;
    if let Ok(RemoteServerFrame::Refused {
        message: refusal_message,
        ..
    }) = serde_json::from_str(hello_answer_frame.get())
    {
        if refusal_message == ROUTER_RESTARTING_MESSAGE {
            return Err(DialError::Restarting(CliError::IpcUnavailable {
                detail: refusal_message,
            }));
        }
        if refusal_message == SESSION_NOT_ANSWERING_MESSAGE {
            return Err(DialError::Refused(CliError::Runtime {
                detail: format!(
                    "session {} did not answer on the server {server_address}",
                    format_session_selector_name(session_selector)
                ),
            }));
        }
        return Err(DialError::Refused(CliError::Runtime {
            detail: format!(
                "the token saved for the server {server_address} does not reach session {}",
                format_session_selector_name(session_selector)
            ),
        }));
    }
    match parse_answer(hello_answer_frame.get().as_bytes()) {
        Ok(incoming_response) => Ok(incoming_response),
        Err(IpcError::PreviousReleaseAnswer) => Err(DialError::Refused(CliError::Runtime {
            detail: format!(
                "session {} answered in the format of koshi 0.4.0 or older, which this koshi \
                 cannot talk to; on the machine that serves it, the user who started it runs: \
                 koshi restart-servers",
                format_session_selector_name(session_selector)
            ),
        })),
        Err(IpcError::MalformedFrame { error_detail }) => {
            Err(DialError::Refused(CliError::IpcUnavailable {
                detail: format!(
                    "the server answered with a frame this attach cannot read: {}",
                    sanitize_reported_text(&error_detail)
                ),
            }))
        }
        Err(answer_parse_error) => Err(DialError::Refused(build_ipc_unavailable_error(
            answer_parse_error,
        ))),
    }
}

/// How a selector reads in a message: the id itself, or the display name.
fn format_session_selector_name(session_selector: &SessionSelector) -> String {
    match session_selector {
        SessionSelector::SessionId(session_id) => session_id.to_string(),
        SessionSelector::SessionName(session_name) => session_name.clone(),
    }
}

/// Run `dial_connection`, and dial again while the server's router or the
/// session restarts.
///
/// A first dial that is not [`DialError::Restarting`] is the answer. After
/// one, the pause of [`REMOTE_RESTART_POLL_INTERVAL_DURATION`] comes before
/// each dial, and a dial that is [`DialError::Restarting`] or
/// [`DialError::Unreachable`] is dialed again until `restart_window_duration`
/// has passed since that first answer. A dial while the restarting router's
/// port is closed is [`DialError::Unreachable`].
///
/// Example — a remote `koshi attach work` whose router answers restarting
/// pauses 250 ms, finds the port closed, pauses again, and joins `work` once
/// the restarted router answers.
///
/// # Errors
/// The first answer that is neither of the two, or the last dial's answer once
/// the window has passed.
pub fn dial_through_router_restart<Joined>(
    restart_window_duration: Duration,
    mut dial_connection: impl FnMut() -> Result<Joined, DialError>,
) -> Result<Joined, DialError> {
    let first_dial_result = dial_connection();
    if !matches!(first_dial_result, Err(DialError::Restarting(_))) {
        return first_dial_result;
    }
    let restart_deadline = Instant::now() + restart_window_duration;
    loop {
        std::thread::sleep(REMOTE_RESTART_POLL_INTERVAL_DURATION);
        match dial_connection() {
            Err(DialError::Restarting(_) | DialError::Unreachable(_))
                if Instant::now() < restart_deadline => {}
            dial_result => return dial_result,
        }
    }
}

/// Submit `command` to the session `session_id` on the saved server
/// `saved_server_record` names, and hand back the dispatcher's result.
///
/// The command's source is [`CommandSource::ExternalCli`] carrying `session_id`
/// and `client_id`. A pane-creating command carrying no working directory
/// keeps none. A rejection's hint is filtered by [`sanitize_reported_text`].
///
/// Naming a `client_id` changes nothing about the exchange: the session's Hello
/// answer settles the version either way, and a session that settled on a
/// version this build does not speak is refused before the command is written.
///
/// The command goes inside a `Prepare`. Once the session answers `Prepared`, a
/// `Commit` runs it, and the `Commit`'s answer has [`REPLY_TIMEOUT_DURATION`]
/// from the moment it is written. Example: a session whose dispatcher stays
/// busy past the reply timeout never runs the command, and this gives
/// [`CliError::IpcUnavailable`].
///
/// A session that refuses the `Prepare` or the `Commit` with `Restarting`
/// did not run the command, and is dialled again through
/// [`dial_through_router_restart`] until
/// [`RESTART_WINDOW_DURATION`](koshi_ipc::endpoint::RESTART_WINDOW_DURATION)
/// has passed.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the saved record holds no fingerprint, and
/// nothing is dialled. The error inside what [`connect_remote_server`] reports,
/// and [`CliError::IpcUnavailable`] when the exchange failed before the
/// `Commit`, or the `Prepare` got an answer that is neither `Prepared` nor a
/// refusal: the command never runs. [`CliError::CommittedRequestUnanswered`]
/// when the `Commit` got no answer, or an answer that is not the command's
/// result: the command may run. [`CliError::SessionRestarting`] when the
/// session still refuses once the restart window has passed.
pub fn submit_remote_command(
    saved_server_record: &SavedServer,
    session_id: SessionId,
    client_id: Option<ClientId>,
    command: Command,
) -> Result<CommandResult, CliError> {
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_external_cli(Some(session_id), client_id),
        command,
    );
    let prepare_request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Prepare(Box::new(IpcRequestKind::SubmitCommand(Box::new(
            command_envelope,
        )))),
    };
    run_remote_session_exchange(
        saved_server_record,
        session_id,
        &prepare_request,
        |frame_reader, frame_writer, prepare_result| {
            let command_ipc_result = ipc_client::commit_after_prepare_answer(
                prepare_result,
                frame_reader,
                frame_writer,
                Some(REPLY_TIMEOUT_DURATION),
            )?;
            match command_ipc_result {
                IpcResult::CommandResult(command_result) => {
                    Ok(talk::filter_rejection_hint(command_result))
                }
                IpcResult::Error(ipc_error_payload) => {
                    Err(build_peer_refusal_error(&ipc_error_payload))
                }
                unexpected_ipc_result => Err(CliError::CommittedRequestUnanswered {
                    detail: talk::SESSION_PEER_WORDS
                        .format_unexpected_reply_detail(unexpected_ipc_result.get_wire_name()),
                }),
            }
        },
    )
}

/// Ask the session `session_id` on the saved server `saved_server_record`
/// names to describe itself in full: tabs, panes, and attached clients.
///
/// Sends [`IpcRequestKind::Discovery`] over one remote connection of its own.
/// The answer passes through
/// [`filter_session_overview_text`](crate::discovery::filter_session_overview_text) before it
/// is handed back: the session name, the tab names, and each pane's title,
/// working directory and argv are filtered.
///
/// A session that refuses the request with `Restarting` is dialled again
/// through [`dial_through_router_restart`] until
/// [`RESTART_WINDOW_DURATION`](koshi_ipc::endpoint::RESTART_WINDOW_DURATION)
/// has passed.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the saved record holds no fingerprint, and
/// nothing is dialled. The error inside what [`connect_remote_server`] reports,
/// and [`CliError::IpcUnavailable`] when the exchange failed.
/// [`CliError::SessionRestarting`] when the session still refuses once the
/// restart window has passed.
pub fn fetch_remote_overview(
    saved_server_record: &SavedServer,
    session_id: SessionId,
) -> Result<SessionOverview, CliError> {
    let discovery_request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Discovery,
    };
    run_remote_session_exchange(
        saved_server_record,
        session_id,
        &discovery_request,
        |_, _, discovery_result| match discovery_result {
            IpcResult::Overview(mut session_overview) => {
                crate::discovery::filter_session_overview_text(&mut session_overview);
                Ok(session_overview)
            }
            IpcResult::Error(ipc_error_payload) => {
                Err(build_peer_refusal_error(&ipc_error_payload))
            }
            unexpected_ipc_result => {
                Err(talk::SESSION_PEER_WORDS.build_unexpected_reply_error(&unexpected_ipc_result))
            }
        },
    )
}

/// One request against one remote session on the saved server
/// `saved_server_record` names: dial as [`dial_saved_server`] states, attach,
/// settle the version from the Hello answer the server sent on this caller's
/// behalf, send `ipc_request`, read its result, and hand both halves of the
/// link, which stay open, and that result to `finish_exchange`. Gives what
/// `finish_exchange` gives.
///
/// The Attach names [`REPLY_TIMEOUT_DURATION`] as its link timeout.
///
/// Each dial runs through [`dial_through_router_restart`] with
/// [`RESTART_WINDOW_DURATION`](koshi_ipc::endpoint::RESTART_WINDOW_DURATION):
/// a server whose router is restarting, and a `finish_exchange` that gives
/// [`CliError::SessionRestarting`] for a session that refused the request
/// while it replaced its process image, are dialed again until the window has
/// passed. A request refused that way did not run.
///
/// # Errors
/// What the last dial gives: what [`dial_saved_server`] reports,
/// [`CliError::IpcUnavailable`] for a write or read that failed before
/// `finish_exchange` ran, and what `finish_exchange` gives.
fn run_remote_session_exchange<ExchangeAnswer>(
    saved_server_record: &SavedServer,
    session_id: SessionId,
    ipc_request: &IpcRequest,
    mut finish_exchange: impl FnMut(
        &mut FrameReader,
        &mut FrameWriter,
        IpcResult,
    ) -> Result<ExchangeAnswer, CliError>,
) -> Result<ExchangeAnswer, CliError> {
    let session_selector = SessionSelector::SessionId(session_id);
    let build_link_failure =
        |ipc_error| DialError::Unreachable(build_ipc_unavailable_error(ipc_error));
    dial_through_router_restart(koshi_ipc::endpoint::RESTART_WINDOW_DURATION, || {
        let remote_link = dial_saved_server(saved_server_record, Some(REPLY_TIMEOUT_DURATION))?;
        let (mut frame_reader, mut frame_writer) = attach_remote_session(
            remote_link,
            session_selector.clone(),
            REPLY_TIMEOUT_DURATION,
        )
        .map_err(DialError::Unreachable)?;
        let hello_response = read_forwarded_hello_answer(
            &mut frame_reader,
            &saved_server_record.server_address,
            &session_selector,
        )?;
        talk::parse_session_hello_answer(hello_response).map_err(DialError::Refused)?;
        frame_writer.send(ipc_request).map_err(build_link_failure)?;
        let incoming_response: IncomingResponse =
            frame_reader.recv_answer().map_err(build_link_failure)?;
        let ipc_result = talk::SESSION_PEER_WORDS
            .take_response_result(incoming_response)
            .map_err(DialError::Refused)?;
        finish_exchange(&mut frame_reader, &mut frame_writer, ipc_result)
            .map_err(DialError::from_session_refusal)
    })
    .map_err(CliError::from)
}

/// The sessions on every saved server that answered within 2 seconds, each
/// beside the label of the server serving it: its saved name, else its
/// address. The rows come in the order of the server labels.
///
/// Each saved server presents the secret it holds. `KOSHI_REMOTE_SECRET` is
/// not read.
///
/// At most 16 saved servers are asked, all at once. When the store holds more,
/// one line on standard error names the count, and the rest add no row:
/// ``koshi: asking the first 16 of <count> saved servers; name one with
/// `--remote <server>` to reach the rest``.
///
/// Every other asked server prints one line on standard error and adds no row:
///
/// - a dial or listing that failed before the deadline: `koshi: <server>:
///   <the failure as a dial to it prints it>; its sessions are not listed`;
/// - no answer before the deadline: `koshi: <server> did not answer; its
///   sessions are not listed`;
/// - no fingerprint: ``koshi: <server> holds no certificate fingerprint, so it
///   is not dialled; run `koshi remote edit <server>` and give the fingerprint
///   `koshi share grant` printed on that machine``.
#[must_use]
pub fn list_saved_server_session_rows() -> Vec<(String, RemoteSessionRow)> {
    let mut saved_server_session_rows = Vec::new();
    for reach in reach_all_saved_servers(REACH_TIMEOUT_DURATION) {
        match reach {
            Reach::Reached {
                server_label,
                session_rows,
            } => {
                saved_server_session_rows.extend(
                    session_rows
                        .into_iter()
                        .map(|remote_session_row| (server_label.clone(), remote_session_row)),
                );
            }
            Reach::Failed {
                server_label,
                failure_message,
            } => {
                eprintln!("koshi: {server_label}: {failure_message}; its sessions are not listed");
            }
            Reach::Unanswered { server_label } => {
                eprintln!("koshi: {server_label} did not answer; its sessions are not listed");
            }
            Reach::Unchecked { server_label } => eprintln!(
                "koshi: {}",
                format_missing_fingerprint_refusal(&server_label)
            ),
        }
    }
    saved_server_session_rows
}

/// Ask every saved server for its sessions at once, and return inside
/// `reach_timeout` whatever the servers do.
///
/// `reach_timeout` is one deadline over the whole call, not a budget each
/// server gets. Each record is asked on its own thread. A thread still running at
/// the deadline is never joined; it writes no file.
///
/// At most 16 records are asked. The rest are named on stderr and left out.
///
/// A record holding no certificate fingerprint is [`Reach::Unchecked`], and no
/// secret is presented to it. Every other record presents the secret it holds.
/// `KOSHI_REMOTE_SECRET` is not read.
///
/// A server whose dial or listing failed before the deadline is
/// [`Reach::Failed`], and a server still unanswered at the deadline is
/// [`Reach::Unanswered`], as [`probe_saved_server`] states. Every
/// record comes back as exactly one entry, sorted by server label. A store that
/// cannot be read reads as no saved servers.
#[must_use]
fn reach_all_saved_servers(reach_timeout: Duration) -> Vec<Reach> {
    let reach_deadline = Instant::now() + reach_timeout;
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
                let _ = reach_sender.send(probe_saved_server(&saved_server_record, reach_deadline));
            });
        if spawn_result.is_ok() {
            requested_server_count += 1;
        }
    }
    drop(reach_sender);

    let mut received_reaches = Vec::with_capacity(requested_server_count);
    while received_reaches.len() < requested_server_count {
        let remaining_wait_duration = reach_deadline.saturating_duration_since(Instant::now());
        if remaining_wait_duration.is_zero() {
            break;
        }
        match reach_receiver.recv_timeout(remaining_wait_duration) {
            Ok(received_reach) => received_reaches.push(received_reach),
            Err(_) => break,
        }
    }
    complete_reach_results(received_reaches, requested_server_labels)
}

/// The server one [`Reach`] is about, whatever it answered.
fn get_reach_server_label(reach: &Reach) -> &str {
    match reach {
        Reach::Reached { server_label, .. }
        | Reach::Failed { server_label, .. }
        | Reach::Unanswered { server_label }
        | Reach::Unchecked { server_label } => server_label,
    }
}

/// Every requested server as exactly one entry, sorted by server: the answers
/// in `received_reaches`, plus one [`Reach::Unanswered`] per label in
/// `requested_server_labels` that no answer names.
///
/// Example — `received_reaches` naming only `desk` with
/// `requested_server_labels` `["desk", "work"]` gives `desk`'s answer and
/// `Unanswered { server_label: "work" }`, in that order.
fn complete_reach_results(
    mut received_reaches: Vec<Reach>,
    mut requested_server_labels: Vec<String>,
) -> Vec<Reach> {
    for received_reach in &received_reaches {
        if let Some(removal_index) = requested_server_labels
            .iter()
            .position(|server_label| server_label == get_reach_server_label(received_reach))
        {
            requested_server_labels.remove(removal_index);
        }
    }
    received_reaches.extend(
        requested_server_labels
            .into_iter()
            .map(|server_label| Reach::Unanswered { server_label }),
    );
    received_reaches.sort_by(|left_reach, right_reach| {
        get_reach_server_label(left_reach).cmp(get_reach_server_label(right_reach))
    });
    received_reaches
}

/// Ask one saved server for its sessions, presenting the secret its record
/// holds as [`connect_remote_server`] does with the record's name, else its
/// address.
///
/// A record holding no certificate fingerprint is [`Reach::Unchecked`] and is
/// not dialled. A dial or listing that fails before `reach_deadline` is
/// [`Reach::Failed`], carrying the `Display` of its [`CliError`]. One that
/// fails at or after `reach_deadline` is [`Reach::Unanswered`].
///
/// The time left until `reach_deadline` is given to the dial and again to the
/// reply: this returns up to twice that after `reach_deadline` passes. Writes no
/// file.
fn probe_saved_server(saved_server_record: &SavedServer, reach_deadline: Instant) -> Reach {
    let server_label = format_saved_server_label(saved_server_record);
    let Some(saved_certificate_fingerprint) =
        saved_server_record.certificate_fingerprint.as_deref()
    else {
        return Reach::Unchecked { server_label };
    };
    let remaining_wait_duration = reach_deadline.saturating_duration_since(Instant::now());
    let listing_result = connect_remote_server(
        &saved_server_record.server_address,
        &saved_server_record.connection_token,
        Some(&server_label),
        saved_certificate_fingerprint,
        remaining_wait_duration,
        Some(remaining_wait_duration),
    )
    .map_err(CliError::from)
    .and_then(|mut remote_link| list_remote_sessions(&mut remote_link));
    match listing_result {
        Ok(session_rows) => Reach::Reached {
            server_label,
            session_rows,
        },
        Err(_) if Instant::now() >= reach_deadline => Reach::Unanswered { server_label },
        Err(listing_error) => Reach::Failed {
            server_label,
            failure_message: listing_error.to_string(),
        },
    }
}

/// The sentence naming a remote frame the request cannot produce, in the
/// words [`talk::PeerWords::build_unexpected_wire_name_error`] uses for a
/// session-plane one.
///
/// `server_address` is the address dialled or `"the server"`;
/// `remote_frame_name` is the [`RemoteServerFrame`] variant that came back. `("desk.local:7654",
/// "Sessions")` gives `desk.local:7654 answered with an unexpected Sessions
/// reply`.
fn format_unexpected_remote_answer(server_address: &str, remote_frame_name: &str) -> String {
    format!("{server_address} answered with an unexpected {remote_frame_name} reply")
}

#[cfg(test)]
mod tests;
