//! Tests for the `share` verbs: how an expiry argument parses, how the three
//! subcommands parse, and what each of the three answers renders to.

use super::*;

use std::time::{Duration, SystemTime};

use clap::Parser;
use koshi_core::client::ClientOrigin;
use koshi_core::discovery::{ClientDiscovery, SessionDiscovery, SessionOverview};
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode, IpcErrorPayload};
use koshi_ipc::remote_tokens::TokenEntry;
use koshi_link::in_session::InSessionContext;
use uuid::Uuid;

use crate::cli::{parse_expiry, Cli, CliCommand, OutputFormat};

/// The one message every bad expiry value comes back with.
const EXPECTED_EXPIRY_ERROR: &str =
    "expected a length such as 30s, 15m, 24h or 7d, or the word never";

/// The parsed `share` subcommand of `argv`.
fn parse_share_command(argv: &[&str]) -> ShareCommand {
    match Cli::try_parse_from(argv)
        .expect("argv must parse")
        .command
        .expect("argv must carry a subcommand")
    {
        CliCommand::Share { command } => command,
        unexpected_cli_command => {
            panic!("argv must parse as a share verb, got {unexpected_cli_command:?}")
        }
    }
}

/// A fixed session id so scope cells and JSON are exact.
fn fixed_session_id() -> SessionId {
    SessionId::from_uuid(
        Uuid::parse_str("0192f0c1-2345-7000-8000-000000000001").expect("literal UUID is valid"),
    )
}

/// The moment `seconds` after the Unix epoch.
fn timestamp_at_seconds(elapsed_seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(elapsed_seconds)
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
    assert_eq!(parse_expiry(""), Err(EXPECTED_EXPIRY_ERROR.to_string()));
    assert_eq!(parse_expiry("12"), Err(EXPECTED_EXPIRY_ERROR.to_string()));
    assert_eq!(parse_expiry("12x"), Err(EXPECTED_EXPIRY_ERROR.to_string()));
    assert_eq!(parse_expiry("h"), Err(EXPECTED_EXPIRY_ERROR.to_string()));
    assert_eq!(parse_expiry("-1h"), Err(EXPECTED_EXPIRY_ERROR.to_string()));
    assert_eq!(
        parse_expiry("NEVER"),
        Err(EXPECTED_EXPIRY_ERROR.to_string())
    );
}

#[test]
fn an_expiry_whose_unit_is_a_multi_byte_character_is_refused_and_never_panics() {
    // The unit is taken as a whole character, so a value ending in a
    // multi-byte one refuses instead of splitting the string mid-character.
    assert_eq!(parse_expiry("30é"), Err(EXPECTED_EXPIRY_ERROR.to_string()));
    assert_eq!(parse_expiry("30日"), Err(EXPECTED_EXPIRY_ERROR.to_string()));
    assert_eq!(parse_expiry("é"), Err(EXPECTED_EXPIRY_ERROR.to_string()));
}

#[test]
fn an_expiry_wrapped_in_whitespace_is_refused() {
    assert_eq!(parse_expiry(" 1h"), Err(EXPECTED_EXPIRY_ERROR.to_string()));
    assert_eq!(parse_expiry("1 h"), Err(EXPECTED_EXPIRY_ERROR.to_string()));
    assert_eq!(parse_expiry("1h "), Err(EXPECTED_EXPIRY_ERROR.to_string()));
    assert_eq!(
        parse_expiry(" never"),
        Err(EXPECTED_EXPIRY_ERROR.to_string())
    );
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
        Err(EXPECTED_EXPIRY_ERROR.to_string())
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
        parse_share_command(&["koshi", "share", "grant", "alice"]),
        ShareCommand::Grant {
            identity: "alice".to_string(),
            session_reference: None,
            token_expiry: Expiry::After(Duration::from_secs(86_400)),
        }
    );
}

#[test]
fn a_grant_takes_a_session_name_and_a_never_expiry() {
    assert_eq!(
        parse_share_command(&[
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
            session_reference: Some(SessionReference::SessionName("quiet-lake".to_string())),
            token_expiry: Expiry::Never,
        }
    );
}

#[test]
fn a_listing_takes_the_json_format_flag() {
    assert_eq!(
        parse_share_command(&["koshi", "share", "list", "--format", "json"]),
        ShareCommand::List {
            session_reference: None,
            output_format: OutputFormat::Json,
        }
    );
}

#[test]
fn a_grant_block_with_no_listen_address_names_the_config_key_that_sets_one() {
    let connection_token = ConnectionToken::from_secret("f00d");
    let rendered_output =
        output::render_share_grant(&connection_token, "alice", &TokenScope::HostWide, false)
            + &output::render_remote_ready("alice", &RemoteReady::NoAddress);

    assert_eq!(
        rendered_output,
        "anyone holding this token can run anything you can.\n\
         f00d\n\
         no remote listen address is set; add `remote-listen \"<host:port>\"` to koshi.kdl, then \
         run `koshi share grant` again.\n"
    );
    assert_eq!(rendered_output.matches("f00d").count(), 1);
    assert!(!rendered_output.contains("://"));
}

#[test]
fn a_grant_block_with_remote_access_left_off_says_the_token_cannot_connect_yet() {
    let connection_token = ConnectionToken::from_secret("f00d");
    let rendered_output =
        output::render_share_grant(&connection_token, "alice", &TokenScope::HostWide, false)
            + &output::render_remote_ready("alice", &RemoteReady::Off);

    assert_eq!(
        rendered_output,
        "anyone holding this token can run anything you can.\n\
         f00d\n\
         remote access stays off; this token cannot be used to connect yet.\n"
    );
}

#[test]
fn a_grant_block_with_remote_access_on_ends_with_the_command_that_connects() {
    let connection_token = ConnectionToken::from_secret("f00d");
    let rendered_output =
        output::render_share_grant(&connection_token, "alice", &TokenScope::HostWide, false)
            + &output::render_remote_ready(
                "alice",
                &RemoteReady::On {
                    remote_listen_address: "laptop.local:7654".to_string(),
                },
            );

    assert_eq!(
        rendered_output,
        "anyone holding this token can run anything you can.\n\
         f00d\n\
         connect from another machine:\n\
         \x20 koshi attach --remote laptop.local:7654 --save-as alice [SESSION]\n\
         set KOSHI_REMOTE_SECRET to the secret above, or paste it when asked.\n"
    );
    // The secret is printed once, on its own line, and never inside the
    // command a reader would paste into a shell.
    assert_eq!(rendered_output.matches("f00d").count(), 1);
    assert!(!rendered_output.contains("--remote laptop.local:7654 f00d"));
}

#[test]
fn a_grant_block_that_replaced_one_opens_with_the_grant_that_stopped() {
    let connection_token = ConnectionToken::from_secret("f00d");
    let rendered_output = output::render_share_grant(
        &connection_token,
        "alice",
        &TokenScope::Session(fixed_session_id()),
        true,
    ) + &output::render_remote_ready("alice", &RemoteReady::NoAddress);

    assert_eq!(
        rendered_output,
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
        output::render_share_revoke(&[
            TokenScope::HostWide,
            TokenScope::Session(fixed_session_id()),
        ]),
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
fn build_sample_token_entries() -> Vec<TokenEntry> {
    vec![
        TokenEntry {
            identity: "alice".to_string(),
            scope: TokenScope::HostWide,
            issued_at: timestamp_at_seconds(1_000),
            expires_at: Some(timestamp_at_seconds(87_400)),
            last_used_at: Some(timestamp_at_seconds(2_000)),
            revoked_at: None,
        },
        TokenEntry {
            identity: "bob".to_string(),
            scope: TokenScope::Session(fixed_session_id()),
            issued_at: timestamp_at_seconds(3_000),
            expires_at: None,
            last_used_at: None,
            revoked_at: Some(timestamp_at_seconds(4_000)),
        },
    ]
}

#[test]
fn a_listing_renders_one_table_row_per_grant_with_absent_times_as_a_dash() {
    assert_eq!(
        output::render_share_list(&build_sample_token_entries(), OutputFormat::Table),
        "identity  scope                                         issued  expires  last_used  revoked\n\
         alice     host                                          1000    87400    2000       -\n\
         bob       session-0192f0c1-2345-7000-8000-000000000001  3000    -        -          4000\n"
    );
}

#[test]
fn a_listing_renders_the_serde_form_of_every_grant_as_json() {
    assert_eq!(
        output::render_share_list(&build_sample_token_entries(), OutputFormat::Json),
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
        output::render_share_list(&[], OutputFormat::Table),
        "identity  scope  issued  expires  last_used  revoked\n"
    );
    assert_eq!(output::render_share_list(&[], OutputFormat::Json), "[]\n");
}

#[test]
fn the_secret_block_stands_on_its_own_and_says_nothing_about_connecting() {
    // The block renders whole on its own, and names nothing about connecting.
    let connection_token = ConnectionToken::from_secret("f00d");
    let secret_block =
        output::render_share_grant(&connection_token, "alice", &TokenScope::HostWide, false);

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
    let rendered_output = output::render_remote_ready(
        "desk:22",
        &RemoteReady::On {
            remote_listen_address: "laptop.local:7654".to_string(),
        },
    );

    assert_eq!(
        rendered_output,
        "connect from another machine:\n  \
         koshi attach --remote laptop.local:7654 [SESSION]\n\
         set KOSHI_REMOTE_SECRET to the secret above, or paste it when asked.\n"
    );
}

#[test]
fn an_identity_with_a_space_in_it_is_not_offered_as_a_saved_name() {
    // Two words, so the flag is left off.
    let rendered_output = output::render_remote_ready(
        "ada lovelace",
        &RemoteReady::On {
            remote_listen_address: "laptop.local:7654".to_string(),
        },
    );

    assert!(
        !rendered_output.contains("--save-as"),
        "a name that cannot be typed as one word is not offered: {rendered_output}"
    );
}

#[test]
fn a_plain_identity_is_still_offered_as_the_saved_name() {
    let rendered_output = output::render_remote_ready(
        "alice",
        &RemoteReady::On {
            remote_listen_address: "laptop.local:7654".to_string(),
        },
    );

    assert_eq!(
        rendered_output,
        "connect from another machine:\n  \
         koshi attach --remote laptop.local:7654 --save-as alice [SESSION]\n\
         set KOSHI_REMOTE_SECRET to the secret above, or paste it when asked.\n"
    );
}

#[test]
fn a_router_that_could_not_answer_leaves_the_state_unread_rather_than_off() {
    // `resolve_remote_ready_or_unknown` maps a failed request to `Unknown`, and passes every
    // answer through unchanged.
    let failed_remote_ready = resolve_remote_ready_or_unknown(Err(CliError::IpcUnavailable {
        detail: "the router is not running".to_string(),
    }));
    assert_eq!(failed_remote_ready, RemoteReady::Unknown);

    let answered_remote_ready = resolve_remote_ready_or_unknown(Ok(RemoteReady::On {
        remote_listen_address: "laptop.local:7654".to_string(),
    }));
    assert_eq!(
        answered_remote_ready,
        RemoteReady::On {
            remote_listen_address: "laptop.local:7654".to_string()
        },
        "an answer is passed through as it stands"
    );

    let off_remote_ready = resolve_remote_ready_or_unknown(Ok(RemoteReady::Off));
    assert_eq!(
        off_remote_ready,
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
            remote_listen_address: "laptop.local:7654".to_string(),
        },
    );

    assert_eq!(
        rendered,
        "remote access is on, and nothing is listening on laptop.local:7654: another program \
         holds it. Free that address, then run `koshi share grant` again to open the port. This \
         token cannot be used to connect until then.\n"
    );
}

/// What one output sink has been told so far.
#[derive(Default)]
struct WrittenOutput {
    /// Every byte written, in order.
    written_bytes: Vec<u8>,
    /// How many of them had been written when `flush` was last called, or
    /// `None` when it never was.
    flushed_byte_count: Option<usize>,
}

/// A sink the test can read while `write_share_grant` is still writing to it, and
/// which remembers where its flushes fell.
#[derive(Clone)]
struct OutputRecorder(std::rc::Rc<std::cell::RefCell<WrittenOutput>>);

impl OutputRecorder {
    fn new() -> OutputRecorder {
        OutputRecorder(std::rc::Rc::new(std::cell::RefCell::new(
            WrittenOutput::default(),
        )))
    }

    /// Everything written so far, as text.
    fn get_written_text(&self) -> String {
        String::from_utf8(self.0.borrow().written_bytes.clone()).expect("the bytes written so far")
    }

    /// Everything that had been flushed by the last flush, as text. Empty when
    /// nothing has been flushed.
    fn get_flushed_text(&self) -> String {
        let written_output = self.0.borrow();
        let flushed_byte_count = written_output.flushed_byte_count.unwrap_or(0);
        String::from_utf8(written_output.written_bytes[..flushed_byte_count].to_vec())
            .expect("the bytes flushed so far")
    }
}

impl std::io::Write for OutputRecorder {
    fn write(&mut self, output_bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .borrow_mut()
            .written_bytes
            .extend_from_slice(output_bytes);
        Ok(output_bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut written_output = self.0.borrow_mut();
        written_output.flushed_byte_count = Some(written_output.written_bytes.len());
        Ok(())
    }
}

#[test]
fn the_secret_is_written_before_anything_that_could_prompt_or_fail() {
    // Read from inside the closure: the secret is in `out` before `ready`
    // runs.
    let token = ConnectionToken::from_secret("f00d");
    let mut output_recorder = OutputRecorder::new();
    let recorder_snapshot = output_recorder.clone();
    let output_seen_before_prompt = std::cell::RefCell::new(String::new());

    write_share_grant(
        &mut output_recorder,
        &token,
        "alice",
        &TokenScope::HostWide,
        false,
        || {
            *output_seen_before_prompt.borrow_mut() = recorder_snapshot.get_written_text();
            RemoteReady::Off
        },
    )
    .expect("writing to a buffer");

    assert!(
        output_seen_before_prompt.borrow().contains("f00d"),
        "the secret was already written when the offer ran, and got: {:?}",
        output_seen_before_prompt.borrow()
    );
    assert_eq!(
        output_recorder.get_written_text(),
        "anyone holding this token can run anything you can.\n\
         f00d\n\
         remote access stays off; this token cannot be used to connect yet.\n"
    );
}

#[test]
fn the_secret_is_flushed_before_anything_that_could_prompt_or_fail() {
    // Read from inside the closure: the whole secret block has been flushed,
    // not only written.
    let token = ConnectionToken::from_secret("f00d");
    let mut output_recorder = OutputRecorder::new();
    let recorder_snapshot = output_recorder.clone();
    let flushed_output_before_prompt = std::cell::RefCell::new(String::new());

    write_share_grant(
        &mut output_recorder,
        &token,
        "alice",
        &TokenScope::HostWide,
        false,
        || {
            *flushed_output_before_prompt.borrow_mut() = recorder_snapshot.get_flushed_text();
            RemoteReady::Off
        },
    )
    .expect("writing to a buffer");

    assert_eq!(
        *flushed_output_before_prompt.borrow(),
        "anyone holding this token can run anything you can.\n\
         f00d\n",
        "the whole secret block was flushed before the offer ran"
    );
}

// --- Where a share verb may run ---

/// One attached client record at `client_id`, connected from `client_origin`.
fn build_client_discovery(
    client_id: ClientId,
    client_origin: Option<ClientOrigin>,
) -> ClientDiscovery {
    ClientDiscovery {
        client_id,
        session_id: SessionId::new(),
        attached_at: SystemTime::UNIX_EPOCH,
        viewport_size: Size {
            column_count: 80,
            row_count: 24,
        },
        active_tab_id: TabId::new(),
        focused_pane_id: None,
        lock_mode: LockMode::Normal,
        origin: client_origin,
        pane_area: None,
    }
}

/// One session overview holding `client_records`, with no tabs and no panes.
fn build_session_overview_with_clients(
    session_id: SessionId,
    client_records: Vec<ClientDiscovery>,
) -> SessionOverview {
    SessionOverview {
        session: SessionDiscovery {
            session_id,
            session_name: "quiet-lake".to_string(),
            created_at: SystemTime::UNIX_EPOCH,
            attached_client_ids: client_records
                .iter()
                .map(|client_record| client_record.client_id)
                .collect(),
            pane_count: 0,
        },
        tabs: Vec::new(),
        panes: Vec::new(),
        clients: client_records,
    }
}

/// A pane environment naming `session_id` and no designated client, which is
/// what a session server's first pane carries.
fn build_in_session_context(session_id: SessionId) -> InSessionContext {
    InSessionContext {
        session_id,
        client_id: None,
        pane_id: PaneId::new(),
    }
}

#[test]
fn a_pane_of_a_session_nobody_watches_from_elsewhere_keeps_share() {
    let session_id = SessionId::new();
    let session_overview = build_session_overview_with_clients(
        session_id,
        vec![
            build_client_discovery(ClientId::new(), Some(ClientOrigin::Local)),
            build_client_discovery(ClientId::new(), Some(ClientOrigin::Local)),
        ],
    );

    refuse_while_watched_from_another_machine(&build_in_session_context(session_id), |_| {
        Ok(session_overview.clone())
    })
    .expect("a session nobody reaches over the network keeps `koshi share`");
}

#[test]
fn a_pane_of_a_remotely_watched_session_refuses_share() {
    let session_id = SessionId::new();
    let session_overview = build_session_overview_with_clients(
        session_id,
        vec![
            build_client_discovery(ClientId::new(), Some(ClientOrigin::Local)),
            build_client_discovery(ClientId::new(), Some(ClientOrigin::Remote)),
        ],
    );

    let share_error = refuse_while_watched_from_another_machine(
        &build_in_session_context(session_id),
        |asked_session_id| {
            assert_eq!(
                asked_session_id, session_id,
                "the pane's own session is the one asked"
            );
            Ok(session_overview.clone())
        },
    )
    .expect_err("a remotely watched session refuses the verb");

    let CliError::CommandRejected { reason, help } = share_error else {
        panic!("expected a rejection, got {share_error:?}");
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
    let session_overview = build_session_overview_with_clients(
        session_id,
        vec![build_client_discovery(ClientId::new(), None)],
    );

    let share_error =
        refuse_while_watched_from_another_machine(&build_in_session_context(session_id), |_| {
            Ok(session_overview.clone())
        })
        .expect_err("an unanswered origin refuses the verb");

    let CliError::CommandRejected { reason, help } = share_error else {
        panic!("expected a rejection, got {share_error:?}");
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

    refuse_while_watched_from_another_machine(&build_in_session_context(session_id), |_| {
        Ok(build_session_overview_with_clients(session_id, Vec::new()))
    })
    .expect("a session with no attached client keeps `koshi share`");
}

#[test]
fn a_session_that_cannot_be_asked_refuses_share() {
    // The session server paints the pane and the router serves `share`; they
    // are separate processes. One being unreachable says nothing about whether
    // anyone is watching this pane.
    let session_id = SessionId::new();

    let share_error =
        refuse_while_watched_from_another_machine(&build_in_session_context(session_id), |_| {
            Err(CliError::SessionNotFound {
                session_name: session_id.to_string(),
            })
        })
        .expect_err("a session that cannot be asked refuses the verb");

    let CliError::CommandRejected { reason, help } = share_error else {
        panic!("expected a rejection, got {share_error:?}");
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
    token_entries: Vec<TokenEntry>,
    should_refuse_host_wide: bool,
    requested_revoke_scopes: Vec<Option<TokenScope>>,
}

impl StandInRouter {
    /// A router holding `token_entries`, answering every `RevokeToken`.
    fn from_token_entries(token_entries: Vec<TokenEntry>) -> Self {
        StandInRouter {
            token_entries,
            should_refuse_host_wide: false,
            requested_revoke_scopes: Vec::new(),
        }
    }

    /// The same router, refusing a `RevokeToken` that names
    /// [`TokenScope::HostWide`].
    fn with_host_wide_refusal(mut self) -> Self {
        self.should_refuse_host_wide = true;
        self
    }

    /// Answer one request, recording the scope of every `RevokeToken`.
    ///
    /// `ListTokens` answers with the held entries. `RevokeToken` answers with
    /// the scope of each held grant it stopped, by the rule
    /// [`TokenStore::revoke_token_grants`](koshi_ipc::remote_tokens::TokenStore::revoke_token_grants)
    /// uses: the identity matches, the grant still stands, and a named scope
    /// matches exactly. A request that matches nothing answers `Revoked([])`,
    /// which is what the router sends when a `--session` revoke finds no grant
    /// scoped to that session.
    fn submit_router_request(
        &mut self,
        router_request_kind: RouterRequestKind,
    ) -> Result<RouterResult, CliError> {
        match router_request_kind {
            RouterRequestKind::ListTokens { .. } => {
                Ok(RouterResult::Tokens(self.token_entries.clone()))
            }
            RouterRequestKind::RevokeToken { identity, scope } => {
                self.requested_revoke_scopes.push(scope.clone());
                if self.should_refuse_host_wide && scope == Some(TokenScope::HostWide) {
                    return Ok(RouterResult::Error(IpcErrorPayload {
                        code: IpcErrorCode::Unknown,
                        message: "the token store could not be written".to_string(),
                    }));
                }
                let current_time = SystemTime::now();
                let revoked_scopes: Vec<TokenScope> = self
                    .token_entries
                    .iter()
                    .filter(|token_entry| {
                        token_entry.identity == identity
                            && token_entry.is_active_at(current_time)
                            && scope
                                .as_ref()
                                .is_none_or(|requested_scope| *requested_scope == token_entry.scope)
                    })
                    .map(|token_entry| token_entry.scope.clone())
                    .collect();
                self.token_entries.retain(|token_entry| {
                    token_entry.identity != identity
                        || scope
                            .as_ref()
                            .is_some_and(|requested_scope| *requested_scope != token_entry.scope)
                });
                Ok(RouterResult::Revoked(revoked_scopes))
            }
            unexpected_router_request => {
                panic!("unexpected control-plane request: {unexpected_router_request:?}")
            }
        }
    }
}

/// One token listing row, live unless `expires_at` is already past.
fn build_token_entry(
    identity: &str,
    token_scope: TokenScope,
    expiration_time: Option<SystemTime>,
) -> TokenEntry {
    TokenEntry {
        identity: identity.to_string(),
        scope: token_scope,
        issued_at: SystemTime::UNIX_EPOCH,
        expires_at: expiration_time,
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

/// Run [`revoke_share_grants`] for `identity` narrowed to `session_scope`
/// against `stand_in_router`, answering the confirm with `confirm_answer`, and
/// hand back each requested revoke scope.
/// `RevokeToken` named.
fn run_session_revoke(
    identity: &str,
    session_scope: TokenScope,
    mut stand_in_router: StandInRouter,
    confirm_answer: bool,
) -> Vec<Option<TokenScope>> {
    revoke_share_grants(
        identity,
        Some(&session_scope),
        |_| confirm_answer,
        |router_request_kind| stand_in_router.submit_router_request(router_request_kind),
    )
    .expect("the router answers");
    stand_in_router.requested_revoke_scopes
}

#[test]
fn a_confirmed_session_revoke_stops_the_host_wide_grant_with_it() {
    let session_scope = TokenScope::Session(SessionId::new());
    let requested_revoke_scopes = run_session_revoke(
        "alice",
        session_scope.clone(),
        StandInRouter::from_token_entries(vec![build_token_entry(
            "alice",
            TokenScope::HostWide,
            None,
        )]),
        true,
    );

    assert_eq!(
        requested_revoke_scopes,
        vec![Some(session_scope), Some(TokenScope::HostWide)],
        "the session grant stops first, then the host-wide one that reaches it"
    );
}

#[test]
fn a_session_revoke_that_stops_nothing_still_cascades_to_the_host_wide_grant() {
    // The identity holds only a host-wide grant, so the router answers the
    // session revoke with `Revoked([])`. The cascade is decided by what the
    // listing holds, not by what the first revoke stopped.
    let session_scope = TokenScope::Session(SessionId::new());
    let requested_revoke_scopes = run_session_revoke(
        "alice",
        session_scope.clone(),
        StandInRouter::from_token_entries(vec![build_token_entry(
            "alice",
            TokenScope::HostWide,
            None,
        )]),
        true,
    );

    assert_eq!(
        requested_revoke_scopes,
        vec![Some(session_scope), Some(TokenScope::HostWide)],
        "nothing stopped on the session scope, and the host-wide grant still stopped"
    );
}

#[test]
fn a_refused_confirm_stops_neither_grant() {
    let requested_revoke_scopes = run_session_revoke(
        "alice",
        TokenScope::Session(SessionId::new()),
        StandInRouter::from_token_entries(vec![build_token_entry(
            "alice",
            TokenScope::HostWide,
            None,
        )]),
        false,
    );

    assert_eq!(
        requested_revoke_scopes,
        Vec::new(),
        "a no leaves both grants standing"
    );
}

#[test]
fn a_session_revoke_with_no_host_wide_grant_asks_nothing_and_stops_that_one() {
    let session_scope = TokenScope::Session(SessionId::new());
    let requested_revoke_scopes = run_session_revoke(
        "alice",
        session_scope.clone(),
        StandInRouter::from_token_entries(vec![build_token_entry(
            "bob",
            TokenScope::HostWide,
            None,
        )]),
        false,
    );

    assert_eq!(
        requested_revoke_scopes,
        vec![Some(session_scope)],
        "another identity's host-wide grant prompts nothing, and the answer is not asked for"
    );
}

#[test]
fn a_revoked_host_wide_grant_prompts_nothing() {
    let session_scope = TokenScope::Session(SessionId::new());
    let mut revoked_token_entry = build_token_entry("alice", TokenScope::HostWide, None);
    revoked_token_entry.revoked_at = Some(SystemTime::UNIX_EPOCH);
    let requested_revoke_scopes = run_session_revoke(
        "alice",
        session_scope.clone(),
        StandInRouter::from_token_entries(vec![revoked_token_entry]),
        false,
    );

    assert_eq!(requested_revoke_scopes, vec![Some(session_scope)]);
}

#[test]
fn an_expired_host_wide_grant_prompts_nothing() {
    let session_scope = TokenScope::Session(SessionId::new());
    let expired_time = SystemTime::now() - Duration::from_secs(60);
    let requested_revoke_scopes = run_session_revoke(
        "alice",
        session_scope.clone(),
        StandInRouter::from_token_entries(vec![build_token_entry(
            "alice",
            TokenScope::HostWide,
            Some(expired_time),
        )]),
        false,
    );

    assert_eq!(requested_revoke_scopes, vec![Some(session_scope)]);
}

#[test]
fn a_refused_second_revoke_reports_the_grant_left_standing() {
    // The session grant stopped, then the router refused the host-wide one. The
    // operator is half done, so the answer names what still stands and the
    // command that finishes it.
    let session_scope = TokenScope::Session(SessionId::new());
    let mut stand_in_router = StandInRouter::from_token_entries(vec![build_token_entry(
        "alice",
        TokenScope::HostWide,
        None,
    )])
    .with_host_wide_refusal();

    let revoke_error = revoke_share_grants(
        "alice",
        Some(&session_scope),
        |_| true,
        |router_request_kind| stand_in_router.submit_router_request(router_request_kind),
    )
    .expect_err("the second revoke was refused");

    let error_message = revoke_error.to_string();
    assert!(
        error_message.contains("alice's host-wide grant is still standing"),
        "the answer names what survived: {error_message}"
    );
    assert!(
        error_message.contains("run `koshi share revoke alice` to stop it"),
        "the answer names the command that finishes it: {error_message}"
    );
    assert_eq!(
        stand_in_router.requested_revoke_scopes,
        vec![Some(session_scope), Some(TokenScope::HostWide)],
        "both revokes were attempted"
    );
}

#[test]
fn a_revoke_naming_no_session_stops_everything_without_asking() {
    let mut stand_in_router = StandInRouter::from_token_entries(vec![build_token_entry(
        "alice",
        TokenScope::HostWide,
        None,
    )]);

    revoke_share_grants(
        "alice",
        None,
        |_| panic!("a bare revoke asks nothing"),
        |router_request_kind| stand_in_router.submit_router_request(router_request_kind),
    )
    .expect("the router answers");

    assert_eq!(
        stand_in_router.requested_revoke_scopes,
        vec![None],
        "one request, naming no scope, which stops every grant the identity holds"
    );
}

#[test]
fn a_router_refusal_is_reported_with_the_routers_own_message() {
    let router_error = build_router_refusal(&RouterResult::Error(IpcErrorPayload {
        code: IpcErrorCode::NotFound,
        message: "this caller may not grant tokens".to_string(),
    }));

    assert_eq!(router_error.to_string(), "this caller may not grant tokens");
}

/// A reply of a kind the request cannot produce is not a refusal, so it is
/// reported as the control plane answering something else, naming the kind.
#[test]
fn a_reply_the_request_cannot_produce_is_reported_by_its_wire_name() {
    let router_error = build_router_refusal(&RouterResult::Restarting);

    assert_eq!(
        router_error.to_string(),
        "IPC unavailable: the router answered with an unexpected Restarting reply"
    );
}
