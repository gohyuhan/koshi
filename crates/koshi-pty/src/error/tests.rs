//! Tests for the display text, the category and the severity of every
//! [`PtyError`] variant.

use super::*;

#[test]
fn a_spawn_failure_displays_its_detail_after_the_prefix() {
    let error = PtyError::Spawn {
        detail: "no such shell".into(),
    };
    assert_eq!(error.to_string(), "failed to spawn pty: no such shell");
}

#[test]
fn an_io_failure_displays_its_detail_after_the_prefix() {
    let error = PtyError::Io {
        detail: "read failed".into(),
    };
    assert_eq!(error.to_string(), "pty io error: read failed");
}

#[test]
fn a_signal_failure_displays_its_detail_after_the_prefix() {
    let error = PtyError::Signal {
        detail: "no such process".into(),
    };
    assert_eq!(error.to_string(), "pty signal error: no such process");
}

#[test]
fn an_unknown_pane_displays_the_pane_id() {
    let pane = PaneId::new();
    let error = PtyError::UnknownPane { pane };
    assert_eq!(error.to_string(), format!("invalid pane: id - {pane}"));
}

#[test]
fn an_empty_detail_leaves_the_prefix_and_its_trailing_space() {
    let error = PtyError::Io {
        detail: String::new(),
    };
    assert_eq!(error.to_string(), "pty io error: ");
}

#[test]
fn a_detail_holding_braces_and_non_ascii_reaches_display_unchanged() {
    let error = PtyError::Io {
        detail: "権限がありません {0} {}".into(),
    };
    assert_eq!(error.to_string(), "pty io error: 権限がありません {0} {}");
}

#[test]
fn every_variant_classifies_as_pty_and_recoverable() {
    let variants = [
        PtyError::Spawn { detail: "x".into() },
        PtyError::Io { detail: "x".into() },
        PtyError::Signal { detail: "x".into() },
        PtyError::UnknownPane {
            pane: PaneId::new(),
        },
    ];
    for error in variants {
        assert_eq!(error.category(), DomainCategory::Pty);
        assert_eq!(error.severity(), Severity::Recoverable);
    }
}
