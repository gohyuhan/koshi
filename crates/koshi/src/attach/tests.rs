//! Tests for the attached client: how one frame read from the event stream
//! decides whether the loop keeps reading and, when it does not, how it ended.

use koshi_core::command::CliExitCode;
use koshi_core::ids::{ClientId, PaneId, TabId};

use super::*;

#[test]
fn the_detached_frame_ends_the_stream_cleanly() {
    assert_eq!(
        classify(&Ok(SessionEvent::Detached)),
        Some(Ending::Detached)
    );
}

#[test]
fn the_quit_frame_ends_the_stream_with_the_session() {
    assert_eq!(
        classify(&Ok(SessionEvent::Quit)),
        Some(Ending::SessionEnded)
    );
}

#[test]
fn a_closed_socket_ends_the_stream_as_a_death() {
    assert_eq!(classify(&Err(IpcError::Disconnected)), Some(Ending::Died));
}

#[test]
fn a_frame_that_does_not_decode_ends_the_stream_as_a_death() {
    let frame = Err(IpcError::MalformedFrame {
        detail: "expected value".to_string(),
    });
    assert_eq!(classify(&frame), Some(Ending::Died));
}

#[test]
fn a_transport_failure_ends_the_stream_as_a_death() {
    let frame = Err(IpcError::Transport {
        detail: "connection reset".to_string(),
    });
    assert_eq!(classify(&frame), Some(Ending::Died));
}

#[test]
fn every_other_frame_keeps_the_stream_reading() {
    let tab_id = TabId::new();
    let frames = [
        SessionEvent::PaneCreated {
            pane_id: PaneId::new(),
            tab_id,
        },
        SessionEvent::PaneProcessExited {
            pane_id: PaneId::new(),
            exit_code: Some(0),
        },
        SessionEvent::PaneClosing {
            pane_id: PaneId::new(),
        },
        SessionEvent::PaneRemoved {
            pane_id: PaneId::new(),
            tab_id,
        },
        SessionEvent::PaneFocused {
            client_id: ClientId::new(),
            tab_id,
            pane_id: PaneId::new(),
            prior_pane: None,
        },
        SessionEvent::LayoutChanged { tab_id },
        SessionEvent::TabCreated { tab_id },
        SessionEvent::TabClosed { tab_id },
        SessionEvent::TabFocused {
            client_id: ClientId::new(),
            tab_id,
            prior_tab: TabId::new(),
        },
        SessionEvent::TabMoved {
            tab_id,
            old_index: 0,
            new_index: 1,
        },
        SessionEvent::Resync { dropped_count: 3 },
    ];
    for frame in frames {
        assert_eq!(classify(&Ok(frame)), None, "{frame:?}");
    }
}

#[test]
fn a_death_reports_the_cause_and_how_to_reattach() {
    let session_id = SessionId::new();
    let error = report(Ending::Died, session_id).expect_err("a death is an error");
    assert_eq!(
        error.to_string(),
        format!(
            "the session ended unexpectedly\n  \
             run `koshi list-sessions`; if session {session_id} is still listed, \
             reattach with `koshi --attach {session_id}`"
        )
    );
    assert_eq!(CliExitCode::from(&error), CliExitCode::RuntimeAction);
}

#[test]
fn a_detach_and_a_session_end_both_succeed() {
    let session_id = SessionId::new();
    assert!(report(Ending::Detached, session_id).is_ok());
    assert!(report(Ending::SessionEnded, session_id).is_ok());
}
