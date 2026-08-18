//! Tests for the `share` verbs: how an expiry argument parses, how the three
//! subcommands parse, and what each of the three answers renders to.

use super::*;

use std::time::{Duration, SystemTime};

use clap::Parser;
use koshi_core::client::ClientOrigin;
use koshi_core::discovery::{ClientInfo, SessionInfo, SessionOverview};
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode, IpcErrorPayload};
use koshi_ipc::remote_tokens::TokenEntry;
use koshi_link::in_session::InSessionContext;
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

// --- Where a share verb may run ---

/// One attached client row at `id`, connected from `origin`.
fn client_row(id: ClientId, origin: Option<ClientOrigin>) -> ClientInfo {
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

/// One session overview holding `clients`, with no tabs and no panes.
fn overview_holding(session_id: SessionId, clients: Vec<ClientInfo>) -> SessionOverview {
    SessionOverview {
        session: SessionInfo {
            id: session_id,
            name: "quiet-lake".to_string(),
            created_at: SystemTime::UNIX_EPOCH,
            attached_clients: clients.iter().map(|client| client.id).collect(),
            pane_count: 0,
        },
        tabs: Vec::new(),
        panes: Vec::new(),
        clients,
    }
}

/// A pane environment naming `session_id` and no designated client, which is
/// what a session server's first pane carries.
fn pane_in(session_id: SessionId) -> InSessionContext {
    InSessionContext {
        session_id,
        client_id: None,
        pane_id: PaneId::new(),
        socket: None,
    }
}

#[test]
fn a_pane_of_a_session_nobody_watches_from_elsewhere_keeps_share() {
    let session_id = SessionId::new();
    let overview = overview_holding(
        session_id,
        vec![
            client_row(ClientId::new(), Some(ClientOrigin::Local)),
            client_row(ClientId::new(), Some(ClientOrigin::Local)),
        ],
    );

    refuse_while_watched_from_another_machine(&pane_in(session_id), |_| Ok(overview.clone()))
        .expect("a session nobody reaches over the network keeps `koshi share`");
}

#[test]
fn a_pane_of_a_remotely_watched_session_refuses_share() {
    let session_id = SessionId::new();
    let overview = overview_holding(
        session_id,
        vec![
            client_row(ClientId::new(), Some(ClientOrigin::Local)),
            client_row(ClientId::new(), Some(ClientOrigin::Remote)),
        ],
    );

    let error = refuse_while_watched_from_another_machine(&pane_in(session_id), |asked| {
        assert_eq!(asked, session_id, "the pane's own session is the one asked");
        Ok(overview.clone())
    })
    .expect_err("a remotely watched session refuses the verb");

    let CliError::CommandRejected { reason, help } = error else {
        panic!("expected a rejection, got {error:?}");
    };
    assert_eq!(reason, RejectReason::Unauthorized);
    assert!(
        help.expect("the refusal names why")
            .contains("someone is attached to this session from another machine"),
        "the refusal names who sees the pane"
    );
}

#[test]
fn a_client_whose_origin_the_session_did_not_answer_refuses_share() {
    // A session server built before the origin field serves rows with no
    // origin. That is not a row saying `Local`.
    let session_id = SessionId::new();
    let overview = overview_holding(session_id, vec![client_row(ClientId::new(), None)]);

    let error =
        refuse_while_watched_from_another_machine(&pane_in(session_id), |_| Ok(overview.clone()))
            .expect_err("an unanswered origin refuses the verb");

    let CliError::CommandRejected { reason, help } = error else {
        panic!("expected a rejection, got {error:?}");
    };
    assert_eq!(reason, RejectReason::Unauthorized);
    assert!(
        help.expect("the refusal names why")
            .contains("someone is attached to this session from another machine"),
        "an unanswered origin takes the same branch a remote row takes"
    );
}

#[test]
fn a_pane_of_a_session_nobody_is_attached_to_keeps_share() {
    let session_id = SessionId::new();

    refuse_while_watched_from_another_machine(&pane_in(session_id), |_| {
        Ok(overview_holding(session_id, Vec::new()))
    })
    .expect("a session with no attached client keeps `koshi share`");
}

#[test]
fn a_session_that_cannot_be_asked_refuses_share() {
    // The session server paints the pane and the router serves `share`; they
    // are separate processes. One being unreachable says nothing about whether
    // anyone is watching this pane.
    let session_id = SessionId::new();

    let error = refuse_while_watched_from_another_machine(&pane_in(session_id), |_| {
        Err(CliError::SessionNotFound {
            session: session_id.to_string(),
        })
    })
    .expect_err("a session that cannot be asked refuses the verb");

    let CliError::CommandRejected { reason, help } = error else {
        panic!("expected a rejection, got {error:?}");
    };
    assert_eq!(reason, RejectReason::Unauthorized);
    assert!(
        help.expect("the refusal names why")
            .contains("this session could not say who is attached to it"),
        "the refusal names what could not be answered"
    );
}

/// A stand-in router: answers control-plane requests from canned data and
/// records the scope each `RevokeToken` named.
///
/// Opens no socket and starts no process, so it behaves the same on every
/// platform and can never reach `spawn_router_detached`.
struct StandInRouter {
    entries: Vec<TokenEntry>,
    refuse_host_wide: bool,
    revokes: Vec<Option<TokenScope>>,
}

impl StandInRouter {
    /// A router holding `entries`, answering every `RevokeToken`.
    fn new(entries: Vec<TokenEntry>) -> Self {
        StandInRouter {
            entries,
            refuse_host_wide: false,
            revokes: Vec::new(),
        }
    }

    /// The same router, refusing a `RevokeToken` that names
    /// [`TokenScope::HostWide`].
    fn refusing_host_wide(mut self) -> Self {
        self.refuse_host_wide = true;
        self
    }

    /// Answer one request, recording the scope of every `RevokeToken`.
    ///
    /// `ListTokens` answers with the held entries. `RevokeToken` answers with
    /// the scope of each held grant it stopped, by the rule
    /// [`TokenStore::revoke`](koshi_ipc::remote_tokens::TokenStore::revoke)
    /// uses: the identity matches, the grant still stands, and a named scope
    /// matches exactly. A request that matches nothing answers `Revoked([])`,
    /// which is what the router sends when a `--session` revoke finds no grant
    /// scoped to that session.
    fn ask(&mut self, kind: RouterRequestKind) -> Result<RouterResult, CliError> {
        match kind {
            RouterRequestKind::ListTokens { .. } => Ok(RouterResult::Tokens(self.entries.clone())),
            RouterRequestKind::RevokeToken { identity, scope } => {
                self.revokes.push(scope.clone());
                if self.refuse_host_wide && scope == Some(TokenScope::HostWide) {
                    return Ok(RouterResult::Error(IpcErrorPayload {
                        code: IpcErrorCode::Unknown,
                        message: "the token store could not be written".to_string(),
                    }));
                }
                let now = SystemTime::now();
                let stopped: Vec<TokenScope> = self
                    .entries
                    .iter()
                    .filter(|entry| {
                        entry.identity == identity
                            && entry.is_live(now)
                            && scope.as_ref().is_none_or(|wanted| *wanted == entry.scope)
                    })
                    .map(|entry| entry.scope.clone())
                    .collect();
                self.entries.retain(|entry| {
                    entry.identity != identity
                        || scope.as_ref().is_some_and(|wanted| *wanted != entry.scope)
                });
                Ok(RouterResult::Revoked(stopped))
            }
            other => panic!("unexpected control-plane request: {other:?}"),
        }
    }
}

/// One token listing row, live unless `expires_at` is already past.
fn token_entry(identity: &str, scope: TokenScope, expires_at: Option<SystemTime>) -> TokenEntry {
    TokenEntry {
        identity: identity.to_string(),
        scope,
        issued_at: SystemTime::UNIX_EPOCH,
        expires_at,
        last_used_at: None,
        revoked_at: None,
    }
}

// --- A revoke that narrowed to one session ---

#[test]
fn the_host_wide_warning_names_the_grant_and_what_stopping_both_costs() {
    let rendered = crate::output::render_revoke_host_wide_warning("alice", &TokenScope::HostWide);

    assert_eq!(
        rendered,
        "alice also holds a host-wide grant, which reaches host.\n\
         stopping the grant on host alone leaves alice reaching it through the host-wide one.\n\
         stopping both leaves alice reaching no session on this machine, not just host.\n"
    );
}

/// Run [`revoke`] for `identity` narrowed to `session` against `router`,
/// answering the confirm with `answer`, and hand back the scope each
/// `RevokeToken` named.
fn revoke_against(
    identity: &str,
    session: TokenScope,
    mut router: StandInRouter,
    answer: bool,
) -> Vec<Option<TokenScope>> {
    revoke(
        identity,
        Some(&session),
        |_| answer,
        |kind| router.ask(kind),
    )
    .expect("the router answers");
    router.revokes
}

#[test]
fn a_confirmed_session_revoke_stops_the_host_wide_grant_with_it() {
    let session = TokenScope::Session(SessionId::new());
    let served = revoke_against(
        "alice",
        session.clone(),
        StandInRouter::new(vec![token_entry("alice", TokenScope::HostWide, None)]),
        true,
    );

    assert_eq!(
        served,
        vec![Some(session), Some(TokenScope::HostWide)],
        "the session grant stops first, then the host-wide one that reaches it"
    );
}

#[test]
fn a_session_revoke_that_stops_nothing_still_cascades_to_the_host_wide_grant() {
    // The identity holds only a host-wide grant, so the router answers the
    // session revoke with `Revoked([])`. The cascade is decided by what the
    // listing holds, not by what the first revoke stopped.
    let session = TokenScope::Session(SessionId::new());
    let served = revoke_against(
        "alice",
        session.clone(),
        StandInRouter::new(vec![token_entry("alice", TokenScope::HostWide, None)]),
        true,
    );

    assert_eq!(
        served,
        vec![Some(session), Some(TokenScope::HostWide)],
        "nothing stopped on the session scope, and the host-wide grant still stopped"
    );
}

#[test]
fn a_refused_confirm_stops_neither_grant() {
    let served = revoke_against(
        "alice",
        TokenScope::Session(SessionId::new()),
        StandInRouter::new(vec![token_entry("alice", TokenScope::HostWide, None)]),
        false,
    );

    assert_eq!(served, Vec::new(), "a no leaves both grants standing");
}

#[test]
fn a_session_revoke_with_no_host_wide_grant_asks_nothing_and_stops_that_one() {
    let session = TokenScope::Session(SessionId::new());
    let served = revoke_against(
        "alice",
        session.clone(),
        StandInRouter::new(vec![token_entry("bob", TokenScope::HostWide, None)]),
        false,
    );

    assert_eq!(
        served,
        vec![Some(session)],
        "another identity's host-wide grant prompts nothing, and the answer is not asked for"
    );
}

#[test]
fn a_revoked_host_wide_grant_prompts_nothing() {
    let session = TokenScope::Session(SessionId::new());
    let mut entry = token_entry("alice", TokenScope::HostWide, None);
    entry.revoked_at = Some(SystemTime::UNIX_EPOCH);
    let served = revoke_against(
        "alice",
        session.clone(),
        StandInRouter::new(vec![entry]),
        false,
    );

    assert_eq!(served, vec![Some(session)]);
}

#[test]
fn an_expired_host_wide_grant_prompts_nothing() {
    let session = TokenScope::Session(SessionId::new());
    let expired = SystemTime::now() - Duration::from_secs(60);
    let served = revoke_against(
        "alice",
        session.clone(),
        StandInRouter::new(vec![token_entry(
            "alice",
            TokenScope::HostWide,
            Some(expired),
        )]),
        false,
    );

    assert_eq!(served, vec![Some(session)]);
}

#[test]
fn a_refused_second_revoke_reports_the_grant_left_standing() {
    // The session grant stopped, then the router refused the host-wide one. The
    // operator is half done, so the answer names what still stands and the
    // command that finishes it.
    let session = TokenScope::Session(SessionId::new());
    let mut router = StandInRouter::new(vec![token_entry("alice", TokenScope::HostWide, None)])
        .refusing_host_wide();

    let error = revoke("alice", Some(&session), |_| true, |kind| router.ask(kind))
        .expect_err("the second revoke was refused");

    let message = error.to_string();
    assert!(
        message.contains("alice's host-wide grant is still standing"),
        "the answer names what survived: {message}"
    );
    assert!(
        message.contains("run `koshi share revoke alice` to stop it"),
        "the answer names the command that finishes it: {message}"
    );
    assert_eq!(
        router.revokes,
        vec![Some(session), Some(TokenScope::HostWide)],
        "both revokes were attempted"
    );
}

#[test]
fn a_revoke_naming_no_session_stops_everything_without_asking() {
    let mut router = StandInRouter::new(vec![token_entry("alice", TokenScope::HostWide, None)]);

    revoke(
        "alice",
        None,
        |_| panic!("a bare revoke asks nothing"),
        |kind| router.ask(kind),
    )
    .expect("the router answers");

    assert_eq!(
        router.revokes,
        vec![None],
        "one request, naming no scope, which stops every grant the identity holds"
    );
}
