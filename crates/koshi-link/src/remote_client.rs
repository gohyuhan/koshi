//! The dialling side of remote access: reach the sessions on another
//! machine.
//!
//! A user names a server either by the address it listens on, `host:port`, or
//! by the name they gave it when they first connected.
//! [`resolve_server`](crate::remote_client::resolve_server) turns what they
//! typed into one of the two, reading the saved-server store on this machine.
//!
//! The secret from a grant never arrives as a command-line argument.
//! [`secret_for`](crate::remote_client::secret_for) reads it from
//! `KOSHI_REMOTE_SECRET`, or asks for it at the terminal without printing
//! what is typed.
//!
//! [`connect`](crate::remote_client::connect) opens the TLS stream, presents
//! the secret, and hands back a
//! [`RemoteLink`](crate::remote_client::RemoteLink) once the server answers
//! `Welcome`. [`connect_saved`](crate::remote_client::connect_saved) wraps
//! that with the store: a server reached for the first time is saved with the
//! fingerprint it presented, and a saved one has its last-used time stamped.
//!
//! From an open link a caller lists the sessions the secret reaches, attaches
//! to one, submits one command to one, or asks one to describe itself.
//! [`reach_all`](crate::remote_client::reach_all) asks every saved server at
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
use koshi_ipc::error::IpcError;
use koshi_ipc::protocol::{
    ConnectionToken, IncomingResponse, IpcRequest, IpcRequestKind, IpcResult, MIN_PROTOCOL_VERSION,
    PROTOCOL_VERSION,
};
use koshi_ipc::remote_servers::{lock_path, store_path, Lookup, SavedServer, ServerStore};
use koshi_ipc::remote_wire::{
    self, RemoteClientFrame, RemoteServerFrame, RemoteSessionRow, MIN_REMOTE_PROTOCOL_VERSION,
    REMOTE_PROTOCOL_VERSION,
};
use koshi_ipc::router::SessionSelector;
use koshi_ipc::transport::{FrameReader, FrameWriter};

use crate::error::CliError;
use crate::talk::{self, refused, talk_failed};

/// The environment variable holding the secret from a grant, read before the
/// terminal is asked for one.
pub const SECRET_VARIABLE: &str = "KOSHI_REMOTE_SECRET";

/// How long one dial has to open: the name lookup aside, the connect, the TLS
/// handshake and the secret exchange share it.
pub const DIAL_WAIT: Duration = Duration::from_secs(10);

/// How long the frames that join a client to a session on another machine have
/// to arrive: the Attach, the session's Hello carried back through the bridge,
/// and the answer that names the client.
///
/// The deadline is taken off once the client is joined.
pub const JOIN_WAIT: Duration = Duration::from_secs(20);

/// How long one command sent to a session on another machine has to come back,
/// counted from the moment the connection opens.
///
/// The dial before it has [`DIAL_WAIT`] of its own, so one request takes at
/// most `DIAL_WAIT + REPLY_WAIT`. An attached client passes `None` instead and
/// waits as long as it takes.
pub const REPLY_WAIT: Duration = Duration::from_secs(20);

/// How many saved servers [`reach_all`] asks at once, one thread each. Records
/// past this count are not asked; [`reach_all`] names how many on stderr.
pub const MAX_REACHED_AT_ONCE: usize = 16;

/// How long [`reach_all`] waits for every saved server together, one deadline
/// over the whole sweep.
pub const REACH_WAIT: Duration = Duration::from_secs(2);

/// How long a change to the saved-server store waits for another koshi to
/// finish its own change before it gives up. The operating system releases the
/// lock if that koshi dies.
pub const STORE_LOCK_WAIT: Duration = Duration::from_secs(5);

/// How long the wait for the saved-server store pauses between attempts on the
/// lock.
pub const STORE_LOCK_POLL: Duration = Duration::from_millis(20);

/// Which server an invocation talks to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerArg {
    /// A server this machine has saved, with its secret and — once a
    /// connection to it has opened — its pinned fingerprint.
    Saved(SavedServer),
    /// A server this machine has not connected to, named by the address it
    /// listens on.
    New {
        /// Where the server listens, as `host:port`.
        address: String,
    },
}

impl ServerArg {
    /// How this server is named in a message: its saved name when it has one,
    /// else the address it listens on.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Saved(record) => label_of(record),
            Self::New { address } => address.clone(),
        }
    }
}

/// How one saved server is named in a message: the name the user chose when
/// they chose one, else the address it listens on. Either way it is a word
/// `koshi remote` takes.
fn label_of(record: &SavedServer) -> String {
    record
        .name
        .clone()
        .unwrap_or_else(|| record.address.clone())
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
    pub fingerprint: String,
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
    fn from(error: DialError) -> Self {
        match error {
            DialError::Unreachable(inner) | DialError::Refused(inner) => inner,
        }
    }
}

/// What asking one saved server for its sessions produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    /// The server answered with the sessions this machine's secret reaches.
    Reached {
        /// The server's name when it has one, else its address.
        server: String,
        /// The sessions, in the order the server holds them.
        rows: Vec<RemoteSessionRow>,
    },
    /// The server answered with a refusal: it did not admit the saved secret,
    /// it settled on a doorway version outside the range this build speaks, or
    /// it refused the listing.
    Refused {
        /// The server's name when it has one, else its address.
        server: String,
    },
    /// The server could not be reached, was still unanswered at the deadline,
    /// presented a certificate other than the one pinned for it, or answered a
    /// frame the request cannot produce.
    Unreachable {
        /// The server's name when it has one, else its address.
        server: String,
    },
    /// The record pins no certificate, and the sweep did not dial it.
    Unchecked {
        /// The server's name when it has one, else its address.
        server: String,
    },
}

/// The saved-server store and the path it came from, under the private data
/// directory.
///
/// # Errors
/// [`CliError::IpcUnavailable`] when the machine has no data directory, and
/// when the store could not be read.
pub fn read_store() -> Result<(PathBuf, ServerStore), CliError> {
    let data_dir = private_data_dir()?;
    let path = store_path(&data_dir);
    let store = ServerStore::read(&path).map_err(store_failed)?;
    Ok((path, store))
}

/// The private data directory this machine keeps koshi's files in.
///
/// # Errors
/// [`CliError::IpcUnavailable`] when the machine has no such directory.
fn private_data_dir() -> Result<PathBuf, CliError> {
    koshi_paths::data_dir().ok_or_else(|| CliError::IpcUnavailable {
        detail: "no data directory found".to_string(),
    })
}

/// Change the saved-server store, holding it against every other koshi from
/// the read to the write.
///
/// Takes the lock at [`lock_path`], reads the store, hands it to `change`, and
/// writes it back. The lock is released when this returns, either way. A
/// `change` that refuses stops the write, so the store on disk keeps what it
/// held.
///
/// The lock is taken again every [`STORE_LOCK_POLL`] for up to
/// [`STORE_LOCK_WAIT`]. A wait that runs out reports the other koshi rather
/// than writing over it.
///
/// Nothing inside `change` may ask the user a question: every other koshi that
/// changes the store waits for this one to finish.
///
/// # Errors
/// [`CliError::IpcUnavailable`] when the machine has no data directory, when
/// the lock could not be taken, when the store could not be read, and when it
/// could not be written. Whatever `change` reports, with nothing written.
pub fn update_store<T>(
    change: impl FnOnce(&mut ServerStore) -> Result<T, CliError>,
) -> Result<T, CliError> {
    let data_dir = private_data_dir()?;
    let path = store_path(&data_dir);
    let held = hold_store(&lock_path(&data_dir), STORE_LOCK_WAIT)?;
    let mut store = ServerStore::read(&path).map_err(store_failed)?;
    let settled = change(&mut store)?;
    store.write(&path).map_err(store_failed)?;
    drop(held);
    Ok(settled)
}

/// Take the advisory lock on the file at `path`, creating the file and the
/// directory holding it when they are missing.
///
/// Both are restricted to the owning user on Unix: mode `0700` on the
/// directory, set whether or not this call made it, and mode `0600` on a lock
/// file this call creates. On Windows both take the data directory's
/// owner-scoped ACLs.
///
/// The attempt is repeated every [`STORE_LOCK_POLL`] for up to `wait`.
/// Dropping the returned file releases the lock, and so does the operating
/// system when the process holding it dies.
///
/// # Errors
/// [`CliError::IpcUnavailable`] when the directory or the file could not be
/// made, when the lock could not be attempted, and when another koshi still
/// held it at the deadline.
fn hold_store(path: &Path, wait: Duration) -> Result<File, CliError> {
    let unavailable = |detail: String| CliError::IpcUnavailable { detail };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            unavailable(format!("{} could not be made: {error}", parent.display()))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(
                |error| {
                    unavailable(format!(
                        "{} could not be made private: {error}",
                        parent.display()
                    ))
                },
            )?;
        }
    }
    let mut options = File::options();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|error| unavailable(format!("{} could not be opened: {error}", path.display())))?;
    let deadline = Instant::now() + wait;
    loop {
        match FileExt::try_lock(&file) {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(unavailable(
                        "another koshi is changing the saved servers; try again".to_string(),
                    ));
                }
                std::thread::sleep(STORE_LOCK_POLL);
            }
            Err(TryLockError::Error(error)) => {
                return Err(unavailable(format!(
                    "{} could not be locked: {error}",
                    path.display()
                )))
            }
        }
    }
}

/// A saved-server store that could not be read or written.
pub fn store_failed(error: IpcError) -> CliError {
    CliError::IpcUnavailable {
        detail: error.to_string(),
    }
}

/// The server `arg` names: a saved record whose name or address is `arg`, or
/// a server this machine has not connected to when `arg` is an address.
///
/// Example — `work` matches the record the user named `work`, and
/// `laptop.local:7654` with no matching record is [`ServerArg::New`].
///
/// A selector that matches more than one record is refused, and no dial is
/// made.
///
/// # Errors
/// [`CliError::InvalidArgs`] when `arg` matches no record and is not an
/// address, so there is nothing to dial, and when it matches more than one.
pub fn resolve_server(arg: &str) -> Result<ServerArg, CliError> {
    let (_, store) = read_store()?;
    server_from(store.find(arg), arg)
}

/// Which server `arg` names, given what the store said about it.
///
/// [`Lookup::Saved`] dials with that record's pinned fingerprint;
/// [`Lookup::NotSaved`] with an address shape dials with none.
///
/// # Errors
/// [`CliError::InvalidArgs`] when `arg` names nothing and is not an address,
/// and when it names more than one saved server.
fn server_from(found: Lookup<'_>, arg: &str) -> Result<ServerArg, CliError> {
    match found {
        Lookup::Saved(record) => Ok(ServerArg::Saved(record.clone())),
        Lookup::Ambiguous => Err(CliError::InvalidArgs {
            detail: format!(
                "{arg} is the name of one saved server and the address of another; \
                 run `koshi remote list` and name the one you mean"
            ),
        }),
        Lookup::NotSaved if looks_like_address(arg) => Ok(ServerArg::New {
            address: arg.to_string(),
        }),
        Lookup::NotSaved => Err(CliError::InvalidArgs {
            detail: format!("no saved server is named {arg}; run `koshi remote list`"),
        }),
    }
}

/// Whether `arg` has the `host:port` shape: text before the last colon, and a
/// port number after it.
///
/// Example — `laptop.local:7654` and `[::1]:22` are addresses; `work`,
/// `laptop.local` and `laptop.local:door` are not.
#[must_use]
pub fn looks_like_address(arg: &str) -> bool {
    match arg.rsplit_once(':') {
        Some((host, port)) => !host.is_empty() && port.parse::<u16>().is_ok(),
        None => false,
    }
}

/// Refuse a saved name that has the `host:port` shape.
///
/// # Errors
/// [`CliError::InvalidArgs`] naming the shape.
pub fn check_name_shape(name: &str) -> Result<(), CliError> {
    if looks_like_address(name) {
        return Err(CliError::InvalidArgs {
            detail: format!(
                "{name} is the shape of an address, and a saved name must not be: \
                 a lookup would take it for the server listening there. Pick a plain name."
            ),
        });
    }
    Ok(())
}

/// Refuse a saved name that cannot be given to the server at `address`.
///
/// Two names are refused: one with the `host:port` shape
/// ([`check_name_shape`]), and one another record already answers to
/// ([`ServerStore::name_free_for`]). Reads the store.
///
/// # Errors
/// [`CliError::InvalidArgs`] when `name` has the `host:port` shape, and when
/// another address already holds it.
pub fn check_save_as(name: &str, address: &str) -> Result<(), CliError> {
    check_name_shape(name)?;
    let (_, store) = read_store()?;
    if !store.name_free_for(name, address) {
        let taken = store
            .records
            .iter()
            .find(|record| record.name.as_deref() == Some(name))
            .map(|record| record.address.clone())
            .unwrap_or_default();
        return Err(CliError::InvalidArgs {
            detail: format!(
                "the name {name} already belongs to {taken}; run `koshi remote forget {name}` \
                 first, or pick another name"
            ),
        });
    }
    Ok(())
}

/// The secret to present to the server at `address`.
///
/// [`SECRET_VARIABLE`] is read first. With it unset, or holding bytes that are
/// not UTF-8, the terminal is asked for the secret and what is typed is not
/// printed. Surrounding whitespace is trimmed.
///
/// # Errors
/// [`CliError::InvalidArgs`] when nothing was given, and when the terminal
/// could not be read.
pub fn secret_for(address: &str) -> Result<ConnectionToken, CliError> {
    let given = match std::env::var(SECRET_VARIABLE) {
        Ok(secret) => secret,
        Err(_) => read_secret(&format!("secret for {address}: "))?,
    };
    let given = given.trim();
    if given.is_empty() {
        return Err(CliError::InvalidArgs {
            detail: format!("no secret was given; set {SECRET_VARIABLE} or paste it when asked"),
        });
    }
    Ok(ConnectionToken::new(given))
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
    Ok(read_secret(prompt)?.trim().to_string())
}

/// Print `prompt`, then read one line from the terminal, which the terminal
/// echoes, with surrounding whitespace trimmed. The answer can be empty.
///
/// # Errors
/// [`CliError::InvalidArgs`] when the terminal could not be read, and when the
/// input ended before a line arrived.
pub fn prompt_line(prompt: &str) -> Result<String, CliError> {
    print!("{prompt}");
    io::stdout().flush().map_err(prompt_failed)?;
    Ok(read_plain_line()?.trim().to_string())
}

/// Print `prompt`, then read one secret from the terminal without printing
/// what is typed.
///
/// The terminal is put in raw mode while the secret is typed. A terminal that
/// cannot be put in raw mode reads one plain line instead, which the terminal
/// echoes.
fn read_secret(prompt: &str) -> Result<String, CliError> {
    print!("{prompt}");
    io::stdout().flush().map_err(prompt_failed)?;
    if crossterm::terminal::enable_raw_mode().is_err() {
        return read_plain_line();
    }
    let typed = read_hidden_line(&mut io::stdin().lock());
    let _ = crossterm::terminal::disable_raw_mode();
    println!();
    typed.map_err(prompt_failed)
}

/// Read one line from standard input, as the terminal echoes it.
///
/// # Errors
/// [`CliError::InvalidArgs`] when standard input could not be read, and when
/// it ended before a line arrived.
fn read_plain_line() -> Result<String, CliError> {
    let mut line = String::new();
    match io::stdin().read_line(&mut line).map_err(prompt_failed)? {
        0 => Err(entry_ended()),
        _ => Ok(line),
    }
}

/// Read from `input` until the Enter key, with none of it printed.
///
/// Ends at `\r`, `\n` or `0x04`. Backspace — `0x7f` or `0x08` — removes the
/// last byte. `0x03` is [`io::ErrorKind::Interrupted`]. End of stream ends the
/// entry where it stands, and is [`io::ErrorKind::UnexpectedEof`] when nothing
/// was typed. Invalid UTF-8 is replaced.
fn read_hidden_line(input: &mut impl Read) -> io::Result<String> {
    let mut typed: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if input.read(&mut byte)? == 0 {
            if typed.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the entry ended",
                ));
            }
            break;
        }
        match byte[0] {
            b'\r' | b'\n' | 0x04 => break,
            0x03 => {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "the entry was interrupted",
                ))
            }
            0x7f | 0x08 => {
                typed.pop();
            }
            other => typed.push(other),
        }
    }
    Ok(String::from_utf8_lossy(&typed).into_owned())
}

/// A terminal that could not be printed to or read from.
fn prompt_failed(error: io::Error) -> CliError {
    CliError::InvalidArgs {
        detail: format!("the answer could not be read: {error}"),
    }
}

/// Input that ended before the answer arrived.
fn entry_ended() -> CliError {
    CliError::InvalidArgs {
        detail: "the input ended before the answer arrived".to_string(),
    }
}

/// Open a connection to the server at `address` and present `secret`.
///
/// `pinned` is the fingerprint saved from an earlier connection, or `None` on
/// the first connection to this server. A server presenting a different
/// certificate is refused.
///
/// `timeout` bounds the connect, the TLS handshake and the secret exchange
/// together. The name lookup before them is the operating system's own and
/// carries no timeout.
///
/// # Errors
/// [`DialError::Unreachable`] when the connection could not be opened or the
/// exchange ran out of time. [`DialError::Refused`] when the certificate
/// changed, the server did not admit the secret, the doorway version it
/// settled on is one this build does not speak, or it answered something else.
pub fn connect(
    address: &str,
    secret: &ConnectionToken,
    pinned: Option<&str>,
    timeout: Duration,
    reply_wait: Option<Duration>,
) -> Result<RemoteLink, DialError> {
    let hello = RemoteClientFrame::Hello {
        min_remote_version: MIN_REMOTE_PROTOCOL_VERSION,
        max_remote_version: REMOTE_PROTOCOL_VERSION,
        min_protocol_version: MIN_PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        token: secret.clone(),
    };
    let (reader, writer, fingerprint, answer) =
        remote_wire::open(address, pinned, &hello, timeout, reply_wait).map_err(dial_failed)?;
    check_answer(address, &answer)?;
    Ok(RemoteLink {
        reader,
        writer,
        fingerprint,
    })
}

/// How one dial's transport failure classifies: a certificate that does not
/// match the pinned one is [`DialError::Refused`], every other transport
/// failure is [`DialError::Unreachable`]. The message is
/// [`talk_failed`](crate::talk::talk_failed)'s either way.
fn dial_failed(error: IpcError) -> DialError {
    match error {
        IpcError::CertificateChanged { .. } => DialError::Refused(talk_failed(error)),
        _ => DialError::Unreachable(talk_failed(error)),
    }
}

/// `Ok(())` when `answer` is a `Welcome` carrying a doorway version between
/// [`MIN_REMOTE_PROTOCOL_VERSION`] and [`REMOTE_PROTOCOL_VERSION`], else the
/// [`DialError::Refused`] to report.
///
/// A `Refused` frame carrying
/// [`REMOTE_REFUSED`](koshi_ipc::remote_wire::REMOTE_REFUSED) reads as a
/// rejected or revoked token and names both ways to replace it. Any other
/// message is the server's own sentence with `address` after it.
///
/// Example — a `Refused` frame carrying `"the session is gone"` from
/// `desk.local:7654` reads `"the session is gone (server desk.local:7654)"`.
fn check_answer(address: &str, answer: &RemoteServerFrame) -> Result<(), DialError> {
    match answer {
        RemoteServerFrame::Welcome { remote_version }
            if (MIN_REMOTE_PROTOCOL_VERSION..=REMOTE_PROTOCOL_VERSION).contains(remote_version) =>
        {
            Ok(())
        }
        RemoteServerFrame::Welcome { remote_version } => {
            Err(DialError::Refused(CliError::Runtime {
                detail: format!(
                    "server {address} settled on remote doorway {remote_version}, which this \
                     koshi does not speak: it speaks {MIN_REMOTE_PROTOCOL_VERSION} to \
                     {REMOTE_PROTOCOL_VERSION}"
                ),
            }))
        }
        RemoteServerFrame::Refused { message } if message == remote_wire::REMOTE_REFUSED => {
            Err(DialError::Refused(CliError::Runtime {
                detail: format!(
                    "the server {address} did not admit the connection: the token was rejected \
                     or revoked. re-grant it on that machine with `koshi share grant`; store \
                     the new secret with `koshi remote set-secret` for a saved server, or \
                     give it when the next dial asks"
                ),
            }))
        }
        RemoteServerFrame::Refused { message } => Err(DialError::Refused(CliError::Runtime {
            detail: format!("{message} (server {address})"),
        })),
        RemoteServerFrame::Sessions { .. } => Err(DialError::Refused(unexpected_answer(address))),
    }
}

/// Open a connection to the server `arg` names, saving what the next
/// connection needs.
///
/// A saved server presents the secret and the fingerprint its record holds, and
/// its last-used time is stamped once the connection opens. A record holding
/// no fingerprint takes the one the store pins for that address now, and pins
/// the certificate this connection presented when the store pins none either.
/// A store that will not take those changes leaves a log line and the
/// connection stands.
///
/// A server reached for the first time asks for its secret ([`secret_for`]),
/// pins whatever certificate it presents, and is saved under `save_as` once it
/// admits the connection. A store that will not take that record fails the
/// call.
///
/// `save_as` names a server this machine has not connected to. Given for a
/// server that is already saved, it is refused.
///
/// `reply_wait` is passed straight to [`connect`].
///
/// The saved record comes back alongside the connection.
///
/// # Errors
/// Whatever [`connect`] reports. [`DialError::Refused`] carrying
/// [`CliError::InvalidArgs`] when `save_as` names a server that is already
/// saved, when the name cannot be given to that address, and when no secret was
/// given; carrying [`CliError::IpcUnavailable`] when a server reached for the
/// first time could not be saved.
pub fn connect_saved(
    arg: &ServerArg,
    save_as: Option<&str>,
    reply_wait: Option<Duration>,
) -> Result<(RemoteLink, SavedServer), DialError> {
    match arg {
        ServerArg::Saved(record) => {
            if let Some(name) = save_as {
                return Err(DialError::Refused(CliError::InvalidArgs {
                    detail: format!(
                        "{} is already saved, so --save-as {name} would change nothing; \
                         run `koshi remote forget {}` first to save it under another name",
                        label_of(record),
                        label_of(record)
                    ),
                }));
            }
            let pinned = record.fingerprint.clone().or_else(|| {
                read_store()
                    .ok()
                    .and_then(|(_, store)| pinned_in(&store, &record.address))
            });
            let link = connect(
                &record.address,
                &record.secret,
                pinned.as_deref(),
                DIAL_WAIT,
                reply_wait,
            )?;
            let now = SystemTime::now();
            let stamped = update_store(|store| {
                store.touch(&record.address, now);
                if record.fingerprint.is_none() {
                    store.pin(&record.address, link.fingerprint.clone());
                }
                Ok(())
            });
            if let Err(error) = stamped {
                tracing::warn!(%error, "the record was not updated");
            }
            let mut used = record.clone();
            used.fingerprint = Some(link.fingerprint.clone());
            used.last_used_at = Some(now);
            Ok((link, used))
        }
        ServerArg::New { address } => {
            if let Some(name) = save_as {
                check_save_as(name, address).map_err(DialError::Refused)?;
            }
            let secret = secret_for(address).map_err(DialError::Refused)?;
            let link = connect(address, &secret, None, DIAL_WAIT, reply_wait)?;
            let now = SystemTime::now();
            let saved = SavedServer {
                name: save_as.map(str::to_string),
                address: address.clone(),
                secret,
                fingerprint: Some(link.fingerprint.clone()),
                added_at: now,
                last_used_at: Some(now),
            };
            update_store(|store| {
                store
                    .save(saved.clone())
                    .map_err(|taken| CliError::InvalidArgs {
                        detail: taken.to_string(),
                    })
            })
            .map_err(DialError::Refused)?;
            Ok((link, saved))
        }
    }
}

/// The fingerprint `store` pins for `address`, or `None` when no record
/// answers to it, more than one does, or the one that does pins nothing.
fn pinned_in(store: &ServerStore, address: &str) -> Option<String> {
    match store.find(address) {
        Lookup::Saved(record) => record.fingerprint.clone(),
        Lookup::NotSaved | Lookup::Ambiguous => None,
    }
}

/// The sessions this connection's secret reaches, in the order the server
/// holds them, each name carrying the bytes the server sent.
///
/// # Errors
/// [`CliError::Runtime`] when the server refused the request, and
/// [`CliError::IpcUnavailable`] when the exchange failed or the server
/// answered something else.
pub fn list_remote_sessions(link: &mut RemoteLink) -> Result<Vec<RemoteSessionRow>, CliError> {
    link.writer
        .send(&RemoteClientFrame::List)
        .map_err(talk_failed)?;
    match link
        .reader
        .recv::<RemoteServerFrame>()
        .map_err(talk_failed)?
    {
        RemoteServerFrame::Sessions { rows } => Ok(rows),
        RemoteServerFrame::Refused { message } => Err(CliError::Runtime { detail: message }),
        RemoteServerFrame::Welcome { .. } => Err(unexpected_answer("the server")),
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
pub fn attach_remote(
    link: RemoteLink,
    selector: SessionSelector,
) -> Result<(FrameReader, FrameWriter), CliError> {
    let RemoteLink {
        reader, mut writer, ..
    } = link;
    writer
        .send(&RemoteClientFrame::Attach { session: selector })
        .map_err(talk_failed)?;
    Ok((reader, writer))
}

/// Submit `command` to the session `session` on the server `arg` names, and
/// hand back the dispatcher's result.
///
/// The command's source is [`CommandSource::external_cli`] carrying `session`
/// and the client the caller named. A pane-creating command carrying no
/// working directory keeps none.
///
/// A named `client` reaches only a session that settled on protocol version 3
/// or later; a session that settled below it is refused with
/// [`CliError::IpcUnavailable`] before the command is written. `None` names no
/// client, and the command is written whatever the session settled on.
///
/// # Errors
/// Whatever [`connect_saved`] reports, and [`CliError::IpcUnavailable`] when
/// the exchange failed.
pub fn submit_remote(
    arg: &ServerArg,
    session: SessionId,
    client: Option<ClientId>,
    command: Command,
) -> Result<CommandResult, CliError> {
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::external_cli(Some(session), client),
        SystemTime::now(),
        command,
    );
    let request = IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::SubmitCommand(Box::new(envelope)),
    };
    match one_request(
        arg,
        session,
        request,
        client.is_some().then_some(talk::TARGET_CLIENT_PROTOCOL),
    )? {
        IpcResult::CommandResult(result) => Ok(result),
        IpcResult::Error(refusal) => Err(refused(&refusal)),
        other => Err(talk::SESSION.unexpected_reply(&other)),
    }
}

/// Ask the session `session` on the server `arg` names to describe itself in
/// full: tabs, panes, and attached clients.
///
/// Sends [`IpcRequestKind::Discovery`] over one remote connection of its own.
/// The session name, the tab names and the pane titles carry the bytes the
/// session sent.
///
/// # Errors
/// Whatever [`connect_saved`] reports, and [`CliError::IpcUnavailable`] when
/// the exchange failed.
pub fn fetch_remote_overview(
    arg: &ServerArg,
    session: SessionId,
) -> Result<SessionOverview, CliError> {
    let request = IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::Discovery,
    };
    match one_request(arg, session, request, None)? {
        IpcResult::Overview(overview) => Ok(overview),
        IpcResult::Error(refusal) => Err(refused(&refusal)),
        other => Err(talk::SESSION.unexpected_reply(&other)),
    }
}

/// One request against one remote session: dial, attach, settle the version
/// from the Hello answer the server sent on this caller's behalf, then send
/// `request` and read its answer.
///
/// `least_version` `Some(least)` refuses a session that settled below `least`
/// with [`CliError::IpcUnavailable`], before `request` is written. `None`
/// writes `request` whatever the session settled on.
fn one_request(
    arg: &ServerArg,
    session: SessionId,
    request: IpcRequest,
    least_version: Option<u32>,
) -> Result<IpcResult, CliError> {
    let (link, _) = connect_saved(arg, None, Some(REPLY_WAIT))?;
    let (mut reader, mut writer) = attach_remote(link, SessionSelector::Id(session))?;

    let hello_reply: IncomingResponse = reader.recv().map_err(talk_failed)?;
    let (settled, _) = talk::session_hello_version(hello_reply)?;
    talk::require_settled_version(settled, least_version)?;

    writer.send(&request).map_err(talk_failed)?;
    let reply: IncomingResponse = reader.recv().map_err(talk_failed)?;
    talk::SESSION.take_result(reply)
}

/// Ask every saved server for its sessions at once, and return inside
/// `timeout` whatever the servers do.
///
/// `timeout` is one deadline over the whole call, not a budget each server
/// gets. Each record is asked on its own thread. A thread still running at
/// the deadline is never joined; it writes no file.
///
/// At most [`MAX_REACHED_AT_ONCE`] records are asked. The rest are named on
/// stderr and left out.
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
pub fn reach_all(timeout: Duration) -> Vec<Reach> {
    let deadline = Instant::now() + timeout;
    let Ok((_, store)) = read_store() else {
        return Vec::new();
    };

    let saved = store.records.len();
    if saved > MAX_REACHED_AT_ONCE {
        eprintln!(
            "koshi: asking the first {MAX_REACHED_AT_ONCE} of {saved} saved servers; \
             name one with `--remote <server>` to reach the rest"
        );
    }

    let unheard: Vec<String> = store
        .records
        .iter()
        .take(MAX_REACHED_AT_ONCE)
        .map(label_of)
        .collect();

    let (send, receive) = mpsc::channel();
    let mut asked = 0usize;
    for record in store.records.into_iter().take(MAX_REACHED_AT_ONCE) {
        let send = send.clone();
        let started = std::thread::Builder::new()
            .name("koshi-remote-reach".to_string())
            .spawn(move || {
                let _ = send.send(probe(&record, deadline));
            });
        if started.is_ok() {
            asked += 1;
        }
    }
    drop(send);

    let mut heard = Vec::with_capacity(asked);
    while heard.len() < asked {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match receive.recv_timeout(left) {
            Ok(reach) => heard.push(reach),
            Err(_) => break,
        }
    }
    complete_sweep(heard, unheard)
}

/// The server one [`Reach`] is about, whatever it answered.
fn server_of(reach: &Reach) -> &str {
    match reach {
        Reach::Reached { server, .. }
        | Reach::Refused { server }
        | Reach::Unreachable { server }
        | Reach::Unchecked { server } => server,
    }
}

/// Every asked server as exactly one entry, sorted by server: the answers in
/// `heard`, plus one [`Reach::Unreachable`] per label in `asked` that no
/// answer names.
///
/// Example — `heard` naming only `desk` with `asked` `["desk", "work"]` gives
/// `desk`'s answer and `Unreachable { server: "work" }`, in that order.
fn complete_sweep(mut heard: Vec<Reach>, mut asked: Vec<String>) -> Vec<Reach> {
    for reach in &heard {
        if let Some(at) = asked.iter().position(|label| label == server_of(reach)) {
            asked.remove(at);
        }
    }
    heard.extend(
        asked
            .into_iter()
            .map(|server| Reach::Unreachable { server }),
    );
    heard.sort_by(|a, b| server_of(a).cmp(server_of(b)));
    heard
}

/// Ask one saved server for its sessions.
///
/// A record pinning no certificate is [`Reach::Unchecked`] and is not dialled.
///
/// A failure carrying [`CliError::Runtime`] — every refusal the server sent —
/// is [`Reach::Refused`]. Every other failure, the changed certificate among
/// them, is [`Reach::Unreachable`].
///
/// The time left until `deadline` is given to the dial and again to the reply,
/// so this returns up to twice that after `deadline` passes. Writes no file.
fn probe(record: &SavedServer, deadline: Instant) -> Reach {
    let server = label_of(record);
    let Some(pinned) = record.fingerprint.as_deref() else {
        return Reach::Unchecked { server };
    };
    let left = deadline.saturating_duration_since(Instant::now());
    let mut link = match connect(
        &record.address,
        &record.secret,
        Some(pinned),
        left,
        Some(left),
    ) {
        Ok(link) => link,
        Err(error) => match CliError::from(error) {
            CliError::Runtime { .. } => return Reach::Refused { server },
            _ => return Reach::Unreachable { server },
        },
    };
    match list_remote_sessions(&mut link) {
        Ok(rows) => Reach::Reached { server, rows },
        Err(CliError::Runtime { .. }) => Reach::Refused { server },
        Err(_) => Reach::Unreachable { server },
    }
}

/// The server sent a frame this request cannot produce.
fn unexpected_answer(server: &str) -> CliError {
    CliError::IpcUnavailable {
        detail: format!("{server} answered with a frame this request cannot produce"),
    }
}

#[cfg(test)]
mod tests;
