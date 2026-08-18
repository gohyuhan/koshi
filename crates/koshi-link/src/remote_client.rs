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

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use koshi_core::command::{Command, CommandEnvelope, CommandResult, CommandSource};
use koshi_core::discovery::SessionOverview;
use koshi_core::ids::{CommandId, SessionId};
use koshi_ipc::error::IpcError;
use koshi_ipc::protocol::{
    ConnectionToken, IncomingResponse, IpcRequest, IpcRequestKind, IpcResult, MIN_PROTOCOL_VERSION,
    PROTOCOL_VERSION,
};
use koshi_ipc::remote_servers::{store_path, Lookup, SavedServer, ServerStore};
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

/// Which server an invocation talks to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerArg {
    /// A server this machine has connected to before, with its secret and
    /// its pinned fingerprint.
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
    /// The server answered, and did not admit the saved secret.
    Refused {
        /// The server's name when it has one, else its address.
        server: String,
    },
    /// The server could not be reached inside the deadline.
    Unreachable,
}

/// The private data directory holding the saved-server store, or
/// [`CliError::IpcUnavailable`] when the machine has none.
fn dialling_data_dir() -> Result<PathBuf, CliError> {
    koshi_paths::data_dir().ok_or_else(|| CliError::IpcUnavailable {
        detail: "no data directory found".to_string(),
    })
}

/// The saved-server store and the path it came from.
fn read_store() -> Result<(PathBuf, ServerStore), CliError> {
    let path = store_path(&dialling_data_dir()?);
    let store = ServerStore::read(&path).map_err(store_failed)?;
    Ok((path, store))
}

/// A saved-server store that could not be read or written.
fn store_failed(error: IpcError) -> CliError {
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
/// A selector that matches more than one record is refused rather than taken
/// for a server this machine has not seen: that would dial with no pinned
/// certificate and save whichever one was presented.
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
/// [`SECRET_VARIABLE`] is read first. With it unset, the terminal is asked for
/// the secret and what is typed is not printed. Surrounding whitespace is
/// trimmed.
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
/// what is typed.
///
/// The terminal is put in raw mode while the secret is typed. A terminal that
/// cannot be put in raw mode reads one plain line instead, which the terminal
/// echoes.
fn read_secret(prompt: &str) -> Result<String, CliError> {
    print!("{prompt}");
    io::stdout().flush().map_err(prompt_failed)?;
    if crossterm::terminal::enable_raw_mode().is_err() {
        let mut line = String::new();
        io::stdin().read_line(&mut line).map_err(prompt_failed)?;
        return Ok(line);
    }
    let typed = read_hidden_line();
    let _ = crossterm::terminal::disable_raw_mode();
    println!();
    typed.map_err(prompt_failed)
}

/// Read from stdin until the Enter key, with none of it printed.
///
/// Ends at `\r`, `\n` or `0x04`. Backspace — `0x7f` or `0x08` — removes the
/// last byte. `0x03` returns an empty string. End of stream ends the entry
/// where it stands. Invalid UTF-8 is replaced.
fn read_hidden_line() -> io::Result<String> {
    let mut stdin = io::stdin().lock();
    let mut typed: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if stdin.read(&mut byte)? == 0 {
            break;
        }
        match byte[0] {
            b'\r' | b'\n' | 0x04 => break,
            0x03 => return Ok(String::new()),
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
        detail: format!("the secret could not be read: {error}"),
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
/// its last-used time is stamped once the connection opens. A store that will
/// not take that stamp leaves a log line and the connection stands.
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
            let link = connect(
                &record.address,
                &record.secret,
                Some(&record.fingerprint),
                DIAL_WAIT,
                reply_wait,
            )?;
            let now = SystemTime::now();
            match read_store() {
                Ok((path, mut store)) => {
                    store.touch(&record.address, now);
                    if let Err(error) = store.write(&path) {
                        tracing::warn!(%error, "the last-used time was not saved");
                    }
                }
                Err(error) => tracing::warn!(%error, "the last-used time was not saved"),
            }
            let mut used = record.clone();
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
                fingerprint: link.fingerprint.clone(),
                added_at: now,
                last_used_at: Some(now),
            };
            let (path, mut store) = read_store().map_err(DialError::Refused)?;
            store.save(saved.clone()).map_err(|taken| {
                DialError::Refused(CliError::InvalidArgs {
                    detail: taken.to_string(),
                })
            })?;
            store
                .write(&path)
                .map_err(|error| DialError::Refused(store_failed(error)))?;
            Ok((link, saved))
        }
    }
}

/// The sessions this connection's secret reaches, in the order the server
/// holds them.
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
/// The command's source is [`CommandSource::external_cli`] carrying `session`.
/// A pane-creating command carrying no working directory keeps none.
///
/// # Errors
/// Whatever [`connect_saved`] reports, and [`CliError::IpcUnavailable`] when
/// the exchange failed.
pub fn submit_remote(
    arg: &ServerArg,
    session: SessionId,
    command: Command,
) -> Result<CommandResult, CliError> {
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::external_cli(Some(session)),
        SystemTime::now(),
        command,
    );
    let request = IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::SubmitCommand(Box::new(envelope)),
    };
    match one_request(arg, session, request)? {
        IpcResult::CommandResult(result) => Ok(result),
        IpcResult::Error(refusal) => Err(refused(&refusal)),
        other => Err(talk::SESSION.unexpected_reply(&other)),
    }
}

/// Ask the session `session` on the server `arg` names to describe itself in
/// full: tabs, panes, and attached clients.
///
/// Sends [`IpcRequestKind::Discovery`] over one remote connection of its own.
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
    match one_request(arg, session, request)? {
        IpcResult::Overview(overview) => Ok(overview),
        IpcResult::Error(refusal) => Err(refused(&refusal)),
        other => Err(talk::SESSION.unexpected_reply(&other)),
    }
}

/// One request against one remote session: dial, attach, settle the version
/// from the Hello answer the server sent on this caller's behalf, then send
/// `request` and read its answer.
fn one_request(
    arg: &ServerArg,
    session: SessionId,
    request: IpcRequest,
) -> Result<IpcResult, CliError> {
    let (link, _) = connect_saved(arg, None, Some(REPLY_WAIT))?;
    let (mut reader, mut writer) = attach_remote(link, SessionSelector::Id(session))?;

    let hello_reply: IncomingResponse = reader.recv().map_err(talk_failed)?;
    match talk::SESSION.take_result(hello_reply)? {
        IpcResult::Hello {
            protocol_version, ..
        } => talk::SESSION.settled_version(protocol_version)?,
        IpcResult::Error(refusal) => return Err(refused(&refusal)),
        other => return Err(talk::SESSION.unexpected_reply(&other)),
    }

    writer.send(&request).map_err(talk_failed)?;
    let reply: IncomingResponse = reader.recv().map_err(talk_failed)?;
    talk::SESSION.take_result(reply)
}

/// Ask every saved server for its sessions at once, and return inside
/// `timeout` whatever the servers do.
///
/// `timeout` is one deadline over the whole call, not a budget each server
/// gets. Each record is asked on its own thread, and this returns with the
/// servers heard from by the deadline. A thread still running at the deadline
/// is never joined; it writes no file.
///
/// At most [`MAX_REACHED_AT_ONCE`] records are asked. The rest are named on
/// stderr and left out.
///
/// A server that answered and did not admit the saved secret is
/// [`Reach::Refused`]. A server that could not be reached is
/// [`Reach::Unreachable`]. A server still unanswered at the deadline is left
/// out of the answer entirely. A store that cannot be read reads as no saved
/// servers.
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
             name one with `koshi attach --remote <server>` to reach the rest"
        );
    }

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
    heard
}

/// Ask one saved server for its sessions.
///
/// The time left until `deadline` is given to the dial and again to the reply,
/// so this returns up to twice that after `deadline` passes. Writes no file.
fn probe(record: &SavedServer, deadline: Instant) -> Reach {
    let server = record
        .name
        .clone()
        .unwrap_or_else(|| record.address.clone());
    let left = deadline.saturating_duration_since(Instant::now());
    let mut link = match connect(
        &record.address,
        &record.secret,
        Some(&record.fingerprint),
        left,
        Some(left),
    ) {
        Ok(link) => link,
        Err(error) => match CliError::from(error) {
            CliError::Runtime { .. } => return Reach::Refused { server },
            _ => return Reach::Unreachable,
        },
    };
    match list_remote_sessions(&mut link) {
        Ok(rows) => Reach::Reached { server, rows },
        Err(CliError::Runtime { .. }) => Reach::Refused { server },
        Err(_) => Reach::Unreachable,
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
