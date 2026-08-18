//! Tests for the `share` verbs: how an expiry argument parses, how the three
//! subcommands parse, and what each of the three answers renders to.

use super::*;

use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use clap::Parser;
use koshi_core::client::ClientOrigin;
use koshi_core::command::CommandResult;
use koshi_core::discovery::{ClientInfo, SessionInfo, SessionOverview};
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::protocol::{
    ConnectionToken, IpcRequest, IpcRequestKind, IpcResponse, IpcResult, PROTOCOL_VERSION,
};
use koshi_ipc::remote_tokens::TokenEntry;
use koshi_ipc::transport::{Connection, Listener};
use uuid::Uuid;

use crate::cli::{parse_expiry, Cli, CliCommand, FormatArg};

/// The one message every bad expiry value comes back with.
const EXPECTED: &str = "expected a length such as 30s, 15m, 24h or 7d, or the word never";

/// The parsed `share` subcommand of `argv`.
fn share_command(argv: &[&str]) -> ShareCommand {
    match Cli::try_parse_from(argv)
        .expect("argv must parse")
        .command
        .expect("argv must carry a subcommand")
    {
        CliCommand::Share { command } => command,
        other => panic!("argv must parse as a share verb, got {other:?}"),
    }
}

/// A fixed session id so scope cells and JSON are exact.
fn fixed_session() -> SessionId {
    SessionId::from_uuid(
        Uuid::parse_str("0192f0c1-2345-7000-8000-000000000001").expect("literal UUID is valid"),
    )
}

/// The moment `seconds` after the Unix epoch.
fn at(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

#[test]
fn every_unit_of_an_expiry_parses_to_its_own_span() {
    assert_eq!(
        parse_expiry("30s"),
        Ok(Expiry::After(Duration::from_secs(30)))
    );
    assert_eq!(
        parse_expiry("15m"),
        Ok(Expiry::After(Duration::from_secs(900)))
    );
    assert_eq!(
        parse_expiry("24h"),
        Ok(Expiry::After(Duration::from_secs(86_400)))
    );
    assert_eq!(
        parse_expiry("7d"),
        Ok(Expiry::After(Duration::from_secs(604_800)))
    );
    assert_eq!(parse_expiry("never"), Ok(Expiry::Never));
}

#[test]
fn an_expiry_that_is_not_a_count_and_a_unit_is_refused_with_one_message() {
    assert_eq!(parse_expiry(""), Err(EXPECTED.to_string()));
    assert_eq!(parse_expiry("12"), Err(EXPECTED.to_string()));
    assert_eq!(parse_expiry("12x"), Err(EXPECTED.to_string()));
    assert_eq!(parse_expiry("h"), Err(EXPECTED.to_string()));
    assert_eq!(parse_expiry("-1h"), Err(EXPECTED.to_string()));
    assert_eq!(parse_expiry("NEVER"), Err(EXPECTED.to_string()));
}

#[test]
fn an_expiry_whose_unit_is_a_multi_byte_character_is_refused_and_never_panics() {
    // The unit is taken as a whole character, so a value ending in a
    // multi-byte one refuses instead of splitting the string mid-character.
    assert_eq!(parse_expiry("30é"), Err(EXPECTED.to_string()));
    assert_eq!(parse_expiry("30日"), Err(EXPECTED.to_string()));
    assert_eq!(parse_expiry("é"), Err(EXPECTED.to_string()));
}

#[test]
fn an_expiry_wrapped_in_whitespace_is_refused() {
    assert_eq!(parse_expiry(" 1h"), Err(EXPECTED.to_string()));
    assert_eq!(parse_expiry("1 h"), Err(EXPECTED.to_string()));
    assert_eq!(parse_expiry("1h "), Err(EXPECTED.to_string()));
    assert_eq!(parse_expiry(" never"), Err(EXPECTED.to_string()));
}

#[test]
fn leading_zeros_in_a_count_read_as_the_same_span() {
    assert_eq!(
        parse_expiry("007h"),
        Ok(Expiry::After(Duration::from_secs(7 * 3600)))
    );
}

#[test]
fn a_count_of_zero_parses_and_makes_a_grant_that_never_works() {
    // A zero span is taken as written: the grant runs out at the instant it
    // is made, so the token it prints admits nothing.
    assert_eq!(parse_expiry("0s"), Ok(Expiry::After(Duration::ZERO)));
    assert_eq!(parse_expiry("0h"), Ok(Expiry::After(Duration::ZERO)));
    assert_eq!(parse_expiry("0d"), Ok(Expiry::After(Duration::ZERO)));
}

#[test]
fn a_count_written_with_a_leading_plus_reads_as_that_count() {
    assert_eq!(
        parse_expiry("+1h"),
        Ok(Expiry::After(Duration::from_secs(3600)))
    );
    assert_eq!(
        parse_expiry("+30s"),
        Ok(Expiry::After(Duration::from_secs(30)))
    );
}

#[test]
fn a_count_whose_unit_multiply_overflows_is_refused_rather_than_wrapping() {
    // 18446744073709551615 days is u64::MAX days: the count itself fits, and
    // the multiply by 86400 seconds is what does not.
    assert_eq!(
        parse_expiry("18446744073709551615d"),
        Err(EXPECTED.to_string())
    );
}

#[test]
fn a_huge_count_of_seconds_parses_because_seconds_need_no_multiply() {
    assert_eq!(
        parse_expiry("10000000000000000000s"),
        Ok(Expiry::After(Duration::from_secs(
            10_000_000_000_000_000_000
        )))
    );
}

#[test]
fn a_bare_grant_covers_every_session_and_lasts_a_day() {
    assert_eq!(
        share_command(&["koshi", "share", "grant", "alice"]),
        ShareCommand::Grant {
            identity: "alice".to_string(),
            session: None,
            expires: Expiry::After(Duration::from_secs(86_400)),
        }
    );
}

#[test]
fn a_grant_takes_a_session_name_and_a_never_expiry() {
    assert_eq!(
        share_command(&[
            "koshi",
            "share",
            "grant",
            "alice",
            "--session",
            "quiet-lake",
            "--expires",
            "never",
        ]),
        ShareCommand::Grant {
            identity: "alice".to_string(),
            session: Some(SessionRef::Name("quiet-lake".to_string())),
            expires: Expiry::Never,
        }
    );
}

#[test]
fn a_listing_takes_the_json_format_flag() {
    assert_eq!(
        share_command(&["koshi", "share", "list", "--format", "json"]),
        ShareCommand::List {
            session: None,
            format: FormatArg::Json,
        }
    );
}

#[test]
fn a_grant_block_with_no_listen_address_names_the_config_key_that_sets_one() {
    let token = ConnectionToken::new("f00d");
    let rendered = output::render_share_grant(&token, "alice", &TokenScope::HostWide, false)
        + &output::render_remote_ready("alice", &RemoteReady::NoAddress);

    assert_eq!(
        rendered,
        "anyone holding this token can run anything you can.\n\
         f00d\n\
         no remote listen address is set; add `remote-listen \"<host:port>\"` to koshi.kdl, then \
         run `koshi share grant` again.\n"
    );
    assert_eq!(rendered.matches("f00d").count(), 1);
    assert!(!rendered.contains("://"));
}

#[test]
fn a_grant_block_with_remote_access_left_off_says_the_token_cannot_connect_yet() {
    let token = ConnectionToken::new("f00d");
    let rendered = output::render_share_grant(&token, "alice", &TokenScope::HostWide, false)
        + &output::render_remote_ready("alice", &RemoteReady::Off);

    assert_eq!(
        rendered,
        "anyone holding this token can run anything you can.\n\
         f00d\n\
         remote access stays off; this token cannot be used to connect yet.\n"
    );
}

#[test]
fn a_grant_block_with_remote_access_on_ends_with_the_command_that_connects() {
    let token = ConnectionToken::new("f00d");
    let rendered = output::render_share_grant(&token, "alice", &TokenScope::HostWide, false)
        + &output::render_remote_ready(
            "alice",
            &RemoteReady::On {
                address: "laptop.local:7654".to_string(),
            },
        );

    assert_eq!(
        rendered,
        "anyone holding this token can run anything you can.\n\
         f00d\n\
         connect from another machine:\n\
         \x20 koshi attach --remote laptop.local:7654 --save-as alice [SESSION]\n\
         set KOSHI_REMOTE_SECRET to the secret above, or paste it when asked.\n"
    );
    // The secret is printed once, on its own line, and never inside the
    // command a reader would paste into a shell.
    assert_eq!(rendered.matches("f00d").count(), 1);
    assert!(!rendered.contains("--remote laptop.local:7654 f00d"));
}

#[test]
fn a_grant_block_that_replaced_one_opens_with_the_grant_that_stopped() {
    let token = ConnectionToken::new("f00d");
    let rendered =
        output::render_share_grant(&token, "alice", &TokenScope::Session(fixed_session()), true)
            + &output::render_remote_ready("alice", &RemoteReady::NoAddress);

    assert_eq!(
        rendered,
        "the token alice already held on session-0192f0c1-2345-7000-8000-000000000001 stopped \
         working.\n\
         anyone holding this token can run anything you can.\n\
         f00d\n\
         no remote listen address is set; add `remote-listen \"<host:port>\"` to koshi.kdl, then \
         run `koshi share grant` again.\n"
    );
}

#[test]
fn a_revoke_names_every_grant_it_stopped() {
    assert_eq!(
        output::render_share_revoke(&[TokenScope::HostWide, TokenScope::Session(fixed_session())]),
        "the grant on host stopped working.\n\
         the grant on session-0192f0c1-2345-7000-8000-000000000001 stopped working.\n"
    );
}

#[test]
fn a_revoke_that_stopped_nothing_says_the_identity_holds_no_grant() {
    assert_eq!(
        output::render_share_revoke(&[]),
        "this identity holds no grant.\n"
    );
}

/// A live host-wide grant and a revoked session-scoped one.
fn two_entries() -> Vec<TokenEntry> {
    vec![
        TokenEntry {
            identity: "alice".to_string(),
            scope: TokenScope::HostWide,
            issued_at: at(1_000),
            expires_at: Some(at(87_400)),
            last_used_at: Some(at(2_000)),
            revoked_at: None,
        },
        TokenEntry {
            identity: "bob".to_string(),
            scope: TokenScope::Session(fixed_session()),
            issued_at: at(3_000),
            expires_at: None,
            last_used_at: None,
            revoked_at: Some(at(4_000)),
        },
    ]
}

#[test]
fn a_listing_renders_one_table_row_per_grant_with_absent_times_as_a_dash() {
    assert_eq!(
        output::render_share_list(&two_entries(), FormatArg::Table),
        "identity  scope                                         issued  expires  last_used  revoked\n\
         alice     host                                          1000    87400    2000       -\n\
         bob       session-0192f0c1-2345-7000-8000-000000000001  3000    -        -          4000\n"
    );
}

#[test]
fn a_listing_renders_the_serde_form_of_every_grant_as_json() {
    assert_eq!(
        output::render_share_list(&two_entries(), FormatArg::Json),
        r#"[
  {
    "identity": "alice",
    "scope": "HostWide",
    "issued_at": {
      "secs_since_epoch": 1000,
      "nanos_since_epoch": 0
    },
    "expires_at": {
      "secs_since_epoch": 87400,
      "nanos_since_epoch": 0
    },
    "last_used_at": {
      "secs_since_epoch": 2000,
      "nanos_since_epoch": 0
    },
    "revoked_at": null
  },
  {
    "identity": "bob",
    "scope": {
      "Session": "0192f0c1-2345-7000-8000-000000000001"
    },
    "issued_at": {
      "secs_since_epoch": 3000,
      "nanos_since_epoch": 0
    },
    "expires_at": null,
    "last_used_at": null,
    "revoked_at": {
      "secs_since_epoch": 4000,
      "nanos_since_epoch": 0
    }
  }
]
"#
    );
}

#[test]
fn an_empty_listing_is_the_header_row_alone_and_an_empty_json_array() {
    assert_eq!(
        output::render_share_list(&[], FormatArg::Table),
        "identity  scope  issued  expires  last_used  revoked\n"
    );
    assert_eq!(output::render_share_list(&[], FormatArg::Json), "[]\n");
}

#[test]
fn the_secret_block_stands_on_its_own_and_says_nothing_about_connecting() {
    // The block renders whole on its own, and names nothing about connecting.
    let token = ConnectionToken::new("f00d");
    let secret_block = output::render_share_grant(&token, "alice", &TokenScope::HostWide, false);

    assert_eq!(
        secret_block,
        "anyone holding this token can run anything you can.\n\
         f00d\n"
    );
    assert!(
        !secret_block.contains("connect"),
        "the secret block promises nothing about reaching anything"
    );
    assert_eq!(secret_block.matches("f00d").count(), 1);
}

#[test]
fn an_identity_shaped_like_an_address_is_not_offered_as_a_saved_name() {
    // `desk:22` has the `host:port` shape, so the flag is left off.
    let rendered = output::render_remote_ready(
        "desk:22",
        &RemoteReady::On {
            address: "laptop.local:7654".to_string(),
        },
    );

    assert_eq!(
        rendered,
        "connect from another machine:\n  \
         koshi attach --remote laptop.local:7654 [SESSION]\n\
         set KOSHI_REMOTE_SECRET to the secret above, or paste it when asked.\n"
    );
}

#[test]
fn an_identity_with_a_space_in_it_is_not_offered_as_a_saved_name() {
    // Two words, so the flag is left off.
    let rendered = output::render_remote_ready(
        "ada lovelace",
        &RemoteReady::On {
            address: "laptop.local:7654".to_string(),
        },
    );

    assert!(
        !rendered.contains("--save-as"),
        "a name that cannot be typed as one word is not offered: {rendered}"
    );
}

#[test]
fn a_plain_identity_is_still_offered_as_the_saved_name() {
    let rendered = output::render_remote_ready(
        "alice",
        &RemoteReady::On {
            address: "laptop.local:7654".to_string(),
        },
    );

    assert_eq!(
        rendered,
        "connect from another machine:\n  \
         koshi attach --remote laptop.local:7654 --save-as alice [SESSION]\n\
         set KOSHI_REMOTE_SECRET to the secret above, or paste it when asked.\n"
    );
}

#[test]
fn a_router_that_could_not_answer_leaves_the_state_unread_rather_than_off() {
    // `ready_or_unknown` maps a failed request to `Unknown`, and passes every
    // answer through unchanged.
    let failed = ready_or_unknown(Err(CliError::IpcUnavailable {
        detail: "the router is not running".to_string(),
    }));
    assert_eq!(failed, RemoteReady::Unknown);

    let answered = ready_or_unknown(Ok(RemoteReady::On {
        address: "laptop.local:7654".to_string(),
    }));
    assert_eq!(
        answered,
        RemoteReady::On {
            address: "laptop.local:7654".to_string()
        },
        "an answer is passed through as it stands"
    );

    let off = ready_or_unknown(Ok(RemoteReady::Off));
    assert_eq!(
        off,
        RemoteReady::Off,
        "including a machine that really is off"
    );
}

#[test]
fn remote_access_that_could_not_be_read_says_so_rather_than_saying_it_is_off() {
    // `Unknown` renders its own block, not the `Off` one.
    let rendered = output::render_remote_ready("alice", &RemoteReady::Unknown);

    assert_eq!(
        rendered,
        "this machine's remote access could not be read, so whether this token can \
         connect is unknown; run `koshi share grant` again, or check the reason \
         printed above.\n"
    );
    assert!(
        !rendered.contains("stays off"),
        "an unread state is not the same as switched off: {rendered}"
    );
}

#[test]
fn a_port_held_by_something_else_says_what_to_run_to_try_again() {
    let rendered = output::render_remote_ready(
        "alice",
        &RemoteReady::Blocked {
            address: "laptop.local:7654".to_string(),
        },
    );

    assert_eq!(
        rendered,
        "remote access is on, and nothing is listening on laptop.local:7654: another program \
         holds it. Free that address, then run `koshi share grant` again to open the port. This \
         token cannot be used to connect until then.\n"
    );
}

/// What one sink has been told so far.
#[derive(Default)]
struct Written {
    /// Every byte written, in order.
    bytes: Vec<u8>,
    /// How many of them had been written when `flush` was last called, or
    /// `None` when it never was.
    flushed: Option<usize>,
}

/// A sink the test can read while `write_grant` is still writing to it, and
/// which remembers where its flushes fell.
#[derive(Clone)]
struct Recorder(std::rc::Rc<std::cell::RefCell<Written>>);

impl Recorder {
    fn new() -> Recorder {
        Recorder(std::rc::Rc::new(
            std::cell::RefCell::new(Written::default()),
        ))
    }

    /// Everything written so far, as text.
    fn text(&self) -> String {
        String::from_utf8(self.0.borrow().bytes.clone()).expect("the bytes written so far")
    }

    /// Everything that had been flushed by the last flush, as text. Empty when
    /// nothing has been flushed.
    fn flushed_text(&self) -> String {
        let written = self.0.borrow();
        let upto = written.flushed.unwrap_or(0);
        String::from_utf8(written.bytes[..upto].to_vec()).expect("the bytes flushed so far")
    }
}

impl std::io::Write for Recorder {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut written = self.0.borrow_mut();
        written.flushed = Some(written.bytes.len());
        Ok(())
    }
}

#[test]
fn the_secret_is_written_before_anything_that_could_prompt_or_fail() {
    // Read from inside the closure: the secret is in `out` before `ready`
    // runs.
    let token = ConnectionToken::new("f00d");
    let mut out = Recorder::new();
    let seen = out.clone();
    let asked_after = std::cell::RefCell::new(String::new());

    write_grant(
        &mut out,
        &token,
        "alice",
        &TokenScope::HostWide,
        false,
        || {
            *asked_after.borrow_mut() = seen.text();
            RemoteReady::Off
        },
    )
    .expect("writing to a buffer");

    assert!(
        asked_after.borrow().contains("f00d"),
        "the secret was already written when the offer ran, and got: {:?}",
        asked_after.borrow()
    );
    assert_eq!(
        out.text(),
        "anyone holding this token can run anything you can.\n\
         f00d\n\
         remote access stays off; this token cannot be used to connect yet.\n"
    );
}

#[test]
fn the_secret_is_flushed_before_anything_that_could_prompt_or_fail() {
    // Read from inside the closure: the whole secret block has been flushed,
    // not only written.
    let token = ConnectionToken::new("f00d");
    let mut out = Recorder::new();
    let seen = out.clone();
    let flushed_when_asked = std::cell::RefCell::new(String::new());

    write_grant(
        &mut out,
        &token,
        "alice",
        &TokenScope::HostWide,
        false,
        || {
            *flushed_when_asked.borrow_mut() = seen.flushed_text();
            RemoteReady::Off
        },
    )
    .expect("writing to a buffer");

    assert_eq!(
        *flushed_when_asked.borrow(),
        "anyone holding this token can run anything you can.\n\
         f00d\n",
        "the whole secret block was flushed before the offer ran"
    );
}

// --- Which client a share verb acts as ---

/// One attached client row, at `id`, connected from `origin`.
fn client_row(id: ClientId, origin: ClientOrigin) -> ClientInfo {
    ClientInfo {
        id,
        session_id: SessionId::new(),
        attached_at: SystemTime::UNIX_EPOCH,
        viewport_size: Size { cols: 80, rows: 24 },
        active_tab: TabId::new(),
        focused_pane: None,
        lock_state: LockMode::Normal,
        origin,
    }
}

#[test]
fn the_client_a_pane_names_decides_the_verb_and_no_other_viewer_does() {
    let alice = ClientId::new();
    let bob = ClientId::new();
    let watched = vec![
        client_row(alice, ClientOrigin::Local),
        client_row(bob, ClientOrigin::Remote),
    ];

    assert!(
        !is_remote(&watched, alice),
        "a local user keeps `koshi share` while a remote viewer watches the same session"
    );
    assert!(
        is_remote(&watched, bob),
        "the remote viewer's own pane is refused"
    );
}

#[test]
fn a_client_the_session_no_longer_lists_is_not_remote() {
    let gone = ClientId::new();
    let listed = vec![client_row(ClientId::new(), ClientOrigin::Remote)];

    assert!(!is_remote(&listed, gone));
    assert!(!is_remote(&[], gone), "no attached client is no refusal");
}

#[test]
fn a_session_that_cannot_be_asked_refuses_nobody() {
    // A runtime directory that was never created answers no discovery, so the
    // guard lets the verb proceed and detaches nobody.
    let session_id = SessionId::new();
    let never_created = std::env::temp_dir().join(format!("koshi-share-guard-{session_id}"));
    let context = InSessionContext {
        session_id,
        client_id: Some(ClientId::new()),
        pane_id: PaneId::new(),
        socket: None,
    };

    refuse_remote_client(&never_created, &context)
        .expect("a session that cannot be asked refuses nobody");
}

// --- The refusal, driven over a stand-in session's socket ---

/// A fresh runtime directory for one test, wiped of any earlier run's leftovers.
fn test_runtime_dir(tag: &str) -> PathBuf {
    #[cfg(unix)]
    let base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    let dir = base.join(format!("koshi-share-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create runtime dir");
    dir
}

/// One session overview at `session_id` holding exactly `clients`, with no
/// tabs and no panes.
fn overview_holding(session_id: SessionId, clients: Vec<ClientInfo>) -> SessionOverview {
    SessionOverview {
        session: SessionInfo {
            id: session_id,
            name: "stand-in".to_string(),
            created_at: SystemTime::UNIX_EPOCH,
            attached_clients: clients.iter().map(|client| client.id).collect(),
            pane_count: 0,
        },
        tabs: Vec::new(),
        panes: Vec::new(),
        clients,
    }
}

/// Send one scripted reply back over `connection`.
fn reply(connection: &mut Connection, request_id: u64, result: IpcResult) {
    connection
        .send(&IpcResponse {
            request_id: Some(request_id),
            result,
        })
        .expect("send scripted reply");
}

/// A stand-in session: it advertises an endpoint file in `runtime_dir` and
/// answers one exchange per connection — a discovery exchange with `overview`,
/// a submitted command with a plain `Ok`. A connection that closes without
/// speaking ends it; [`end_session`] opens that connection. The handle joins
/// to every command that was submitted, in arrival order.
fn serve_session(runtime_dir: &Path, overview: SessionOverview) -> JoinHandle<Vec<Command>> {
    let session_id = overview.session.id;
    let socket = koshi_ipc::endpoint::socket_addr(runtime_dir, session_id);
    let token = ConnectionToken::generate();
    let listener = Listener::bind(&socket).expect("stand-in session binds");
    EndpointFile {
        socket,
        token: token.clone(),
        pid: std::process::id(),
    }
    .write(&EndpointFile::path(runtime_dir, session_id))
    .expect("endpoint file written");

    std::thread::spawn(move || {
        let mut overview = Some(overview);
        let mut submitted = Vec::new();
        loop {
            let mut connection = listener.accept().expect("accept");
            let Ok(hello) = connection.recv::<IpcRequest>() else {
                break;
            };
            let request: IpcRequest = connection
                .recv()
                .expect("read the request behind the hello");
            assert!(matches!(
                &hello.kind,
                IpcRequestKind::Hello { token: presented, .. } if presented == &token
            ));
            reply(
                &mut connection,
                hello.request_id,
                IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
            );
            match request.kind {
                IpcRequestKind::Discovery => {
                    let overview = overview.take().expect("one discovery exchange");
                    reply(
                        &mut connection,
                        request.request_id,
                        IpcResult::Overview(overview),
                    );
                }
                IpcRequestKind::SubmitCommand(envelope) => {
                    reply(
                        &mut connection,
                        request.request_id,
                        IpcResult::CommandResult(CommandResult::Ok {
                            command_id: envelope.id,
                            emitted_events: Vec::new(),
                        }),
                    );
                    submitted.push(envelope.command);
                }
                other => panic!("unexpected request: {other:?}"),
            }
        }
        submitted
    })
}

/// End the stand-in session: open one connection and close it unspoken.
fn end_session(runtime_dir: &Path, session_id: SessionId) {
    let socket = koshi_ipc::endpoint::socket_addr(runtime_dir, session_id);
    drop(Connection::connect(&socket).expect("reach the stand-in session"));
}

#[test]
fn a_verb_from_a_remote_client_is_refused_and_that_client_is_detached() {
    let session_id = SessionId::new();
    let remote = ClientId::new();
    let runtime_dir = test_runtime_dir("refuse-remote");
    let overview = overview_holding(session_id, vec![client_row(remote, ClientOrigin::Remote)]);
    let server = serve_session(&runtime_dir, overview);

    let context = InSessionContext {
        session_id,
        client_id: Some(remote),
        pane_id: PaneId::new(),
        socket: None,
    };
    let error = refuse_remote_client(&runtime_dir, &context)
        .expect_err("a remote client's verb is refused");

    match error {
        CliError::CommandRejected { reason, help } => {
            assert_eq!(reason, RejectReason::Unauthorized);
            assert_eq!(
                help.as_deref(),
                Some(
                    "`koshi share` only runs on the machine hosting the session; \
                     run it in a shell there"
                )
            );
        }
        other => panic!("expected a rejection, got {other:?}"),
    }

    end_session(&runtime_dir, session_id);
    assert_eq!(
        server.join().expect("the stand-in session exits"),
        vec![Command::Detach(DetachArgs {
            client: Some(remote),
            reason: DetachReason::HostOnlyRefusal,
        })],
        "the refusal detached the client it names, and did nothing else"
    );
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn a_verb_from_a_local_client_proceeds_while_a_remote_viewer_watches() {
    let session_id = SessionId::new();
    let local = ClientId::new();
    let runtime_dir = test_runtime_dir("local-proceeds");
    let overview = overview_holding(
        session_id,
        vec![
            client_row(local, ClientOrigin::Local),
            client_row(ClientId::new(), ClientOrigin::Remote),
        ],
    );
    let server = serve_session(&runtime_dir, overview);

    let context = InSessionContext {
        session_id,
        client_id: Some(local),
        pane_id: PaneId::new(),
        socket: None,
    };
    refuse_remote_client(&runtime_dir, &context)
        .expect("a local client keeps `koshi share` while a remote viewer watches");

    end_session(&runtime_dir, session_id);
    assert_eq!(
        server.join().expect("the stand-in session exits"),
        Vec::new(),
        "no detach was submitted"
    );
    let _ = std::fs::remove_dir_all(&runtime_dir);
}
