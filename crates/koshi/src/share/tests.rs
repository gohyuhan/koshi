//! Tests for the `share` verbs: how an expiry argument parses, how the three
//! subcommands parse, and what each of the three answers renders to.

use super::*;

use std::time::{Duration, SystemTime};

use clap::Parser;
use koshi_core::ids::SessionId;
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::remote_tokens::TokenEntry;
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
fn a_grant_block_warns_shows_the_secret_and_says_it_cannot_connect() {
    let token = ConnectionToken::new("f00d");
    let rendered = output::render_share_grant(&token, "alice", &TokenScope::HostWide, false);

    assert_eq!(
        rendered,
        "anyone holding this token can run anything you can.\n\
         f00d\n\
         remote access is not configured on this machine, so this token cannot be used to \
         connect yet.\n"
    );
    assert_eq!(rendered.matches("f00d").count(), 1);
    assert!(!rendered.contains("://"));
}

#[test]
fn a_grant_block_that_replaced_one_opens_with_the_grant_that_stopped() {
    let token = ConnectionToken::new("f00d");
    let rendered =
        output::render_share_grant(&token, "alice", &TokenScope::Session(fixed_session()), true);

    assert_eq!(
        rendered,
        "the token alice already held on session-0192f0c1-2345-7000-8000-000000000001 stopped \
         working.\n\
         anyone holding this token can run anything you can.\n\
         f00d\n\
         remote access is not configured on this machine, so this token cannot be used to \
         connect yet.\n"
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
