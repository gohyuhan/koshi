//! Tests for pane attention in the session server: which program facts make a
//! pane wait for a person, which acts end the wait, and what every subscribed
//! client receives.

use std::sync::{mpsc, Arc};
use std::time::Duration;

use koshi_config::types::AttentionConfig;
use koshi_core::attention::AttentionReasonKind;
use koshi_core::command::{
    ClosePaneArgs, Command, CommandEnvelope, CommandResult, CommandSource, FocusTabArgs, TabTarget,
    ToggleLockModeArgs, WriteToPaneArgs,
};
use koshi_core::geometry::Point;
use koshi_core::ids::{CommandId, SessionId, TabId};
use koshi_core::key::{
    BindingModifierFlags, Key, KeyEventKind, KeyIdentity, KeyInput, KeyModifierFlags, NamedKey,
};
use koshi_core::mouse::{MouseButton, MouseKind};
use koshi_core::process::{ExitStatus, PtySize};
use koshi_layout::tree::LayoutNode;
use koshi_pane::pane::state::PaneRecord;
use koshi_pty::backend::state::PtyBackend;
use koshi_renderer::snapshot::Delivery;
use koshi_session::client::ClientRegistry;
use koshi_session::session::state::Session;
use koshi_terminal::engine::TerminalEngine;
use koshi_test_support::fake_pty::FakePtyBackend;
use koshi_test_support::fixtures::build_spawn_spec;

use crate::runtime::command::tests::{attach_client, build_in_session_source, build_tab};
use crate::runtime::delivery_queue::DeliveryReceiver;
use crate::runtime::pty_inbox::InboxSink;

use super::*;

/// A server holding one session with two tabs. Tab `code` shows
/// `build_pane_id`, and tab `docs` shows `docs_pane_id`. Each pane has an
/// 8x3 terminal engine and a child in the fake backend. Alice views `docs`
/// with her input on `docs_pane_id`, her terminal has reported no focus, and
/// `alice_delivery_receiver` receives what the bus delivers to her.
struct AttentionFixture {
    server: Server,
    fake_pty_backend: Arc<FakePtyBackend>,
    session_id: SessionId,
    code_tab_id: TabId,
    build_pane_id: PaneId,
    docs_pane_id: PaneId,
    alice_client_id: ClientId,
    alice_delivery_receiver: DeliveryReceiver,
}

fn build_attention_fixture() -> AttentionFixture {
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let fake_pty_backend = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender),
    )));
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let mut server =
        Server::from_runtime_parts(pty_backend, runtime_event_receiver, Arc::default());
    let (code_tab_id, docs_tab_id) = (TabId::new(), TabId::new());
    let (build_pane_id, docs_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = Session::from_identity_and_client_registry(
        SessionId::new(),
        "s".to_owned(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    for (tab_id, tab_name, tab_index, pane_id) in [
        (code_tab_id, "code", 0, build_pane_id),
        (docs_tab_id, "docs", 1, docs_pane_id),
    ] {
        session.tabs.insert(
            tab_id,
            build_tab(tab_id, tab_name, tab_index, LayoutNode::Pane(pane_id)),
        );
        session
            .panes
            .register_pane_record(PaneRecord::from_terminal_pane(pane_id))
            .expect("unique pane id");
        let pane_size = PtySize {
            column_count: 8,
            row_count: 3,
        };
        server
            .terminal_engine_by_pane_id
            .insert(pane_id, TerminalEngine::from_pty_size(pane_size));
        fake_pty_backend
            .spawn_pane(pane_id, build_spawn_spec(), pane_size)
            .expect("fake spawn succeeds");
    }
    let alice_client_id = ClientId::new();
    attach_client(
        &mut session,
        alice_client_id,
        docs_tab_id,
        Some(docs_pane_id),
    );
    let session_id = session.session_id;
    server.session_by_id.insert(session_id, session);
    let (alice_delivery_receiver, _) = server.subscribe(alice_client_id, 60, None);
    server.handle_attach_answer_received(alice_client_id);
    AttentionFixture {
        server,
        fake_pty_backend,
        session_id,
        code_tab_id,
        build_pane_id,
        docs_pane_id,
        alice_client_id,
        alice_delivery_receiver,
    }
}

impl AttentionFixture {
    /// Attach Bob viewing tab `code` with his input on `build_pane_id` and
    /// his terminal at `terminal_focus`, and return his id.
    fn attach_bob_to_build_pane(&mut self, terminal_focus: TerminalFocus) -> ClientId {
        let bob_client_id = ClientId::new();
        let session = self
            .server
            .session_by_id
            .get_mut(&self.session_id)
            .expect("the session exists");
        attach_client(
            session,
            bob_client_id,
            self.code_tab_id,
            Some(self.build_pane_id),
        );
        self.server
            .get_client_mut(bob_client_id)
            .expect("Bob is attached")
            .set_terminal_focus(terminal_focus);
        bob_client_id
    }

    /// Hand `output_bytes`, read at `read_at`, to `build_pane_id`.
    fn write_build_pane_output(&mut self, output_bytes: &[u8], read_at: SystemTime) {
        self.server
            .handle_pty_output(self.build_pane_id, output_bytes, read_at, Instant::now());
    }

    /// Everything the bus delivered to Alice since the last call, oldest
    /// first.
    fn take_alice_deliveries(&self) -> Vec<Delivery> {
        self.alice_delivery_receiver.try_iter().collect()
    }

    /// The attention that `pane_id`'s record holds.
    fn find_pane_attention(&self, pane_id: PaneId) -> Option<Attention> {
        self.server.session_by_id[&self.session_id]
            .panes
            .get_pane_record_by_id(pane_id)
            .expect("the pane has a record")
            .attention
            .clone()
    }

    /// The pane that takes `client_id`'s input.
    fn find_input_pane_id(&self, client_id: ClientId) -> Option<PaneId> {
        self.server.find_active_focused_pane_id(client_id)
    }

    /// Record `build_pane_id` as Alice's focused pane in tab `code`, the tab
    /// she does not view.
    fn focus_alice_on_build_pane_in_code_tab(&mut self) {
        let code_tab_id = self.code_tab_id;
        let build_pane_id = self.build_pane_id;
        self.server
            .get_client_mut(self.alice_client_id)
            .expect("Alice is attached")
            .update_focused_pane(code_tab_id, build_pane_id);
    }

    /// Dispatch `command` from `command_source`. Panics when the command is
    /// refused.
    fn dispatch_command(&mut self, command_source: CommandSource, command: Command) {
        let command_result = self.server.dispatch(CommandEnvelope::from_parts(
            CommandId::new(),
            command_source,
            command,
        ));
        if let CommandResult::Rejected { .. } = command_result {
            panic!("expected the command to apply, got {command_result:?}");
        }
    }
}

/// Each of `program_facts`, read at `read_instant`.
fn build_timed_program_facts(
    program_facts: Vec<ProgramFact>,
    read_instant: Instant,
) -> Vec<TimedProgramFact> {
    program_facts
        .into_iter()
        .map(|program_fact| TimedProgramFact {
            program_fact,
            read_instant,
        })
        .collect()
}

/// `seconds` after the Unix epoch.
fn build_wall_time(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

fn build_raised_delivery(
    pane_id: PaneId,
    tab_id: TabId,
    reason: AttentionReason,
    raised_at: SystemTime,
) -> Delivery {
    Delivery::PaneAttentionRaised(PaneAttentionRaised {
        pane_id,
        tab_id: Some(tab_id),
        reason,
        raised_at,
    })
}

fn build_cleared_delivery(pane_id: PaneId, tab_id: TabId, client_id: Option<ClientId>) -> Delivery {
    Delivery::PaneAttentionCleared(PaneAttentionCleared {
        pane_id,
        tab_id: Some(tab_id),
        client_id,
    })
}

fn build_bell_attention(raised_at: SystemTime) -> Attention {
    Attention {
        raised_at,
        reason: AttentionReason::Bell,
        withdrawable_notification_id: None,
    }
}

/// A fixture whose `build_pane_id` waits on a bell read at `raised_at`, with
/// the raise taken off Alice's queue.
fn build_fixture_with_waiting_build_pane(raised_at: SystemTime) -> AttentionFixture {
    let mut fixture = build_attention_fixture();
    fixture.write_build_pane_output(b"\x07", raised_at);
    assert_eq!(
        fixture.take_alice_deliveries(),
        [build_raised_delivery(
            fixture.build_pane_id,
            fixture.code_tab_id,
            AttentionReason::Bell,
            raised_at
        )]
    );
    fixture
}

/// An unmodified Enter press.
fn build_enter_press() -> KeyInput {
    KeyInput {
        key: KeyIdentity::Key(Key::Named(NamedKey::Enter)),
        key_event_kind: KeyEventKind::Press,
        shifted_key: None,
        base_layout_key: None,
        associated_text: String::new(),
        modifier_flags: KeyModifierFlags::NONE,
    }
}

fn switch_off_attention_kind(
    attention_config: &mut AttentionConfig,
    attention_reason_kind: AttentionReasonKind,
) {
    match attention_reason_kind {
        AttentionReasonKind::Bell => attention_config.should_raise_for_bell = false,
        AttentionReasonKind::Notify => attention_config.should_raise_for_notify = false,
        AttentionReasonKind::CommandFinished => {
            attention_config.should_raise_for_command_finished = false;
        }
    }
}

// ------------------------------------------------------------ raising ----

#[test]
fn plain_output_leaves_the_pane_waiting_for_nobody() {
    let mut fixture = build_attention_fixture();

    fixture.write_build_pane_output(b"hello", build_wall_time(1_000));

    assert_eq!(fixture.take_alice_deliveries(), Vec::new());
    assert_eq!(fixture.find_pane_attention(fixture.build_pane_id), None);
}

#[test]
fn a_bell_in_the_input_pane_of_a_client_whose_terminal_has_focus_is_dropped() {
    let mut fixture = build_attention_fixture();
    fixture.attach_bob_to_build_pane(TerminalFocus::Focused);

    fixture.write_build_pane_output(b"\x07", build_wall_time(1_000));

    assert_eq!(fixture.take_alice_deliveries(), Vec::new());
    assert_eq!(fixture.find_pane_attention(fixture.build_pane_id), None);
}

#[test]
fn a_bell_raises_when_no_client_with_terminal_focus_holds_the_pane() {
    let read_at = build_wall_time(1_000);
    for build_pane_viewer_focus in [
        Some(TerminalFocus::Unfocused),
        Some(TerminalFocus::Unreported),
        None,
    ] {
        let mut fixture = build_attention_fixture();
        if let Some(terminal_focus) = build_pane_viewer_focus {
            fixture.attach_bob_to_build_pane(terminal_focus);
        }
        let alice_client_id = fixture.alice_client_id;
        fixture
            .server
            .handle_terminal_focus_change(alice_client_id, true);

        fixture.write_build_pane_output(b"\x07", read_at);

        assert_eq!(
            fixture.take_alice_deliveries(),
            [build_raised_delivery(
                fixture.build_pane_id,
                fixture.code_tab_id,
                AttentionReason::Bell,
                read_at
            )],
            "viewer focus {build_pane_viewer_focus:?}"
        );
        assert_eq!(
            fixture.find_pane_attention(fixture.build_pane_id),
            Some(build_bell_attention(read_at)),
            "viewer focus {build_pane_viewer_focus:?}"
        );
    }
}

#[test]
fn every_subscribed_client_receives_the_raise() {
    let mut fixture = build_attention_fixture();
    let bob_client_id = fixture.attach_bob_to_build_pane(TerminalFocus::Unfocused);
    let (bob_delivery_receiver, _) = fixture.server.subscribe(bob_client_id, 60, None);
    fixture.server.handle_attach_answer_received(bob_client_id);
    let read_at = build_wall_time(1_000);

    fixture.write_build_pane_output(b"\x07", read_at);

    let expected_delivery = build_raised_delivery(
        fixture.build_pane_id,
        fixture.code_tab_id,
        AttentionReason::Bell,
        read_at,
    );
    assert_eq!(
        fixture.take_alice_deliveries(),
        std::slice::from_ref(&expected_delivery)
    );
    assert_eq!(
        bob_delivery_receiver.try_iter().collect::<Vec<_>>(),
        [expected_delivery]
    );
}

#[test]
fn a_signal_whose_kind_koshi_kdl_switches_off_is_dropped() {
    let build_command_finished_events =
        |pane_id| vec![Event::PaneCommandFinished(PaneCommandFinished { pane_id })];
    for (attention_reason_kind, program_fact) in [
        (AttentionReasonKind::Bell, ProgramFact::Bell),
        (
            AttentionReasonKind::Notify,
            ProgramFact::Notify {
                title: None,
                body: "done".to_string(),
                notification_id: None,
            },
        ),
        (
            AttentionReasonKind::CommandFinished,
            ProgramFact::CommandFinished { exit_code: Some(1) },
        ),
    ] {
        let mut fixture = build_attention_fixture();
        switch_off_attention_kind(&mut fixture.server.config.attention, attention_reason_kind);
        let expected_events = match attention_reason_kind {
            AttentionReasonKind::CommandFinished => {
                build_command_finished_events(fixture.build_pane_id)
            }
            AttentionReasonKind::Bell | AttentionReasonKind::Notify => Vec::new(),
        };

        let read_instant = Instant::now();
        let emitted_events = fixture.server.apply_program_facts(
            fixture.build_pane_id,
            build_timed_program_facts(vec![program_fact], read_instant),
            build_wall_time(1_000),
            read_instant,
        );

        assert_eq!(emitted_events, expected_events, "{attention_reason_kind:?}");
        assert_eq!(
            fixture.find_pane_attention(fixture.build_pane_id),
            None,
            "{attention_reason_kind:?}"
        );
    }
}

#[test]
fn a_command_raises_only_once_it_ran_five_whole_seconds() {
    for (elapsed_duration, expected_elapsed_secs) in [
        (Duration::from_millis(4_999), None),
        (Duration::from_secs(5), Some(5)),
    ] {
        let mut fixture = build_attention_fixture();
        let build_pane_id = fixture.build_pane_id;
        let started_at = Instant::now();
        let finished_read_at = build_wall_time(1_005);
        fixture.server.apply_program_facts(
            build_pane_id,
            build_timed_program_facts(vec![ProgramFact::CommandStarted], started_at),
            build_wall_time(1_000),
            started_at,
        );

        let emitted_events = fixture.server.apply_program_facts(
            build_pane_id,
            build_timed_program_facts(
                vec![ProgramFact::CommandFinished { exit_code: Some(0) }],
                started_at + elapsed_duration,
            ),
            finished_read_at,
            started_at + elapsed_duration,
        );

        let mut expected_events = vec![Event::PaneCommandFinished(PaneCommandFinished {
            pane_id: build_pane_id,
        })];
        if let Some(elapsed_secs) = expected_elapsed_secs {
            expected_events.push(Event::PaneAttentionRaised(PaneAttentionRaised {
                pane_id: build_pane_id,
                tab_id: Some(fixture.code_tab_id),
                reason: AttentionReason::CommandFinished {
                    exit_code: Some(0),
                    elapsed_secs: Some(elapsed_secs),
                },
                raised_at: finished_read_at,
            }));
        }
        assert_eq!(emitted_events, expected_events, "{elapsed_duration:?}");
    }
}

#[test]
fn a_command_that_ran_twelve_seconds_raises_with_its_exit_code_and_whole_seconds() {
    let mut fixture = build_attention_fixture();
    let build_pane_id = fixture.build_pane_id;
    let started_at = Instant::now();
    let finished_read_at = build_wall_time(1_012);

    let started_events = fixture.server.apply_program_facts(
        build_pane_id,
        build_timed_program_facts(vec![ProgramFact::CommandStarted], started_at),
        build_wall_time(1_000),
        started_at,
    );
    let finished_events = fixture.server.apply_program_facts(
        build_pane_id,
        build_timed_program_facts(
            vec![ProgramFact::CommandFinished {
                exit_code: Some(101),
            }],
            started_at + Duration::from_millis(12_200),
        ),
        finished_read_at,
        started_at + Duration::from_millis(12_200),
    );

    let finished_reason = AttentionReason::CommandFinished {
        exit_code: Some(101),
        elapsed_secs: Some(12),
    };
    assert_eq!(
        started_events,
        [Event::PaneCommandStarted(PaneCommandStarted {
            pane_id: build_pane_id
        })]
    );
    assert_eq!(
        finished_events,
        [
            Event::PaneCommandFinished(PaneCommandFinished {
                pane_id: build_pane_id
            }),
            Event::PaneAttentionRaised(PaneAttentionRaised {
                pane_id: build_pane_id,
                tab_id: Some(fixture.code_tab_id),
                reason: finished_reason.clone(),
                raised_at: finished_read_at,
            }),
        ]
    );
    assert_eq!(
        fixture.find_pane_attention(build_pane_id),
        Some(Attention {
            raised_at: finished_read_at,
            reason: finished_reason,
            withdrawable_notification_id: None,
        })
    );
    assert_eq!(
        fixture
            .server
            .command_started_at_by_pane_id
            .get(&build_pane_id),
        None
    );
}

#[test]
fn a_finish_whose_start_this_server_never_saw_raises_with_no_elapsed_time() {
    let mut fixture = build_attention_fixture();
    let build_pane_id = fixture.build_pane_id;
    let read_at = build_wall_time(1_000);
    let read_instant = Instant::now();

    let emitted_events = fixture.server.apply_program_facts(
        build_pane_id,
        build_timed_program_facts(
            vec![ProgramFact::CommandFinished { exit_code: Some(1) }],
            read_instant,
        ),
        read_at,
        read_instant,
    );

    assert_eq!(
        emitted_events,
        [
            Event::PaneCommandFinished(PaneCommandFinished {
                pane_id: build_pane_id
            }),
            Event::PaneAttentionRaised(PaneAttentionRaised {
                pane_id: build_pane_id,
                tab_id: Some(fixture.code_tab_id),
                reason: AttentionReason::CommandFinished {
                    exit_code: Some(1),
                    elapsed_secs: None,
                },
                raised_at: read_at,
            }),
        ]
    );
}

#[test]
fn five_hundred_bells_raise_once_and_a_notification_after_them_keeps_the_first_raise_time() {
    let first_read_at = build_wall_time(1_000);
    let mut fixture = build_fixture_with_waiting_build_pane(first_read_at);

    fixture.write_build_pane_output(&[0x07; 500], build_wall_time(1_001));
    assert_eq!(fixture.take_alice_deliveries(), Vec::new());

    fixture.write_build_pane_output(b"\x1b]9;tests passed\x1b\\", build_wall_time(1_002));

    let notify_reason = AttentionReason::Notify {
        title: None,
        body: "tests passed".to_string(),
    };
    assert_eq!(
        fixture.take_alice_deliveries(),
        [build_raised_delivery(
            fixture.build_pane_id,
            fixture.code_tab_id,
            notify_reason.clone(),
            first_read_at
        )]
    );
    assert_eq!(
        fixture.find_pane_attention(fixture.build_pane_id),
        Some(Attention {
            raised_at: first_read_at,
            reason: notify_reason,
            withdrawable_notification_id: None,
        })
    );
}

#[test]
fn a_run_of_bells_in_one_read_raises_once() {
    let mut fixture = build_attention_fixture();
    let read_at = build_wall_time(1_000);

    fixture.write_build_pane_output(&[0x07; 500], read_at);

    assert_eq!(
        fixture.take_alice_deliveries(),
        [build_raised_delivery(
            fixture.build_pane_id,
            fixture.code_tab_id,
            AttentionReason::Bell,
            read_at
        )]
    );
}

#[test]
fn alternating_bells_and_notifications_in_one_read_raise_once_with_the_newest_reason() {
    let mut fixture = build_attention_fixture();
    let read_at = build_wall_time(1_000);
    let alternating_signal_bytes = b"\x07\x1b]9;tests passed\x1b\\".repeat(250);

    fixture.write_build_pane_output(&alternating_signal_bytes, read_at);

    let notify_reason = AttentionReason::Notify {
        title: None,
        body: "tests passed".to_string(),
    };
    assert_eq!(
        fixture.take_alice_deliveries(),
        [build_raised_delivery(
            fixture.build_pane_id,
            fixture.code_tab_id,
            notify_reason.clone(),
            read_at
        )]
    );
    assert_eq!(
        fixture.find_pane_attention(fixture.build_pane_id),
        Some(Attention {
            raised_at: read_at,
            reason: notify_reason,
            withdrawable_notification_id: None,
        })
    );
}

#[test]
fn facts_released_together_raise_at_the_read_times_of_their_own_chunks() {
    // The start was read 8 s and the finish 3 s before the chunk that
    // released both, read at 1 008 s.
    let mut fixture = build_attention_fixture();
    let build_pane_id = fixture.build_pane_id;
    let started_read_instant = Instant::now();
    let mut program_facts =
        build_timed_program_facts(vec![ProgramFact::CommandStarted], started_read_instant);
    program_facts.extend(build_timed_program_facts(
        vec![ProgramFact::CommandFinished { exit_code: Some(0) }],
        started_read_instant + Duration::from_secs(5),
    ));

    let emitted_events = fixture.server.apply_program_facts(
        build_pane_id,
        program_facts,
        build_wall_time(1_008),
        started_read_instant + Duration::from_secs(8),
    );

    assert_eq!(
        emitted_events,
        [
            Event::PaneCommandStarted(PaneCommandStarted {
                pane_id: build_pane_id
            }),
            Event::PaneCommandFinished(PaneCommandFinished {
                pane_id: build_pane_id
            }),
            Event::PaneAttentionRaised(PaneAttentionRaised {
                pane_id: build_pane_id,
                tab_id: Some(fixture.code_tab_id),
                reason: AttentionReason::CommandFinished {
                    exit_code: Some(0),
                    elapsed_secs: Some(5),
                },
                raised_at: build_wall_time(1_005),
            }),
        ]
    );
}

// --------------------------------------------------- program withdrawal ----

#[test]
fn withdrawing_the_notification_by_its_id_clears_with_no_client() {
    let mut fixture = build_attention_fixture();
    let read_at = build_wall_time(1_000);
    fixture.write_build_pane_output(b"\x1b]99;i=7:p=body;deploy done\x1b\\", read_at);
    assert_eq!(
        fixture.take_alice_deliveries(),
        [build_raised_delivery(
            fixture.build_pane_id,
            fixture.code_tab_id,
            AttentionReason::Notify {
                title: None,
                body: "deploy done".to_string(),
            },
            read_at
        )]
    );

    fixture.write_build_pane_output(b"\x1b]99;i=8:p=close;\x1b\\", build_wall_time(1_001));
    assert_eq!(fixture.take_alice_deliveries(), Vec::new());

    fixture.write_build_pane_output(b"\x1b]99;i=7:p=close;\x1b\\", build_wall_time(1_002));
    assert_eq!(
        fixture.take_alice_deliveries(),
        [build_cleared_delivery(
            fixture.build_pane_id,
            fixture.code_tab_id,
            None
        )]
    );
    assert_eq!(fixture.find_pane_attention(fixture.build_pane_id), None);
}

#[test]
fn a_bell_after_the_notification_takes_its_withdrawal_away() {
    let first_read_at = build_wall_time(1_000);
    let mut fixture = build_attention_fixture();
    fixture.write_build_pane_output(b"\x1b]99;i=7:p=body;deploy done\x1b\\", first_read_at);
    fixture.write_build_pane_output(b"\x07", build_wall_time(1_001));
    assert_eq!(
        fixture.take_alice_deliveries(),
        [
            build_raised_delivery(
                fixture.build_pane_id,
                fixture.code_tab_id,
                AttentionReason::Notify {
                    title: None,
                    body: "deploy done".to_string(),
                },
                first_read_at
            ),
            build_raised_delivery(
                fixture.build_pane_id,
                fixture.code_tab_id,
                AttentionReason::Bell,
                first_read_at
            ),
        ]
    );

    fixture.write_build_pane_output(b"\x1b]99;i=7:p=close;\x1b\\", build_wall_time(1_002));

    assert_eq!(fixture.take_alice_deliveries(), Vec::new());
    assert_eq!(
        fixture.find_pane_attention(fixture.build_pane_id),
        Some(build_bell_attention(first_read_at))
    );
}

#[test]
fn a_notification_withdrawn_in_the_same_read_raises_nothing() {
    let mut fixture = build_attention_fixture();

    fixture.write_build_pane_output(
        b"\x1b]99;i=7:p=body;deploy done\x1b\\\x1b]99;i=7:p=close;\x1b\\",
        build_wall_time(1_000),
    );

    assert_eq!(fixture.take_alice_deliveries(), Vec::new());
    assert_eq!(fixture.find_pane_attention(fixture.build_pane_id), None);
}

#[test]
fn a_withdrawal_and_a_bell_in_one_read_raise_once_at_the_new_read_time() {
    let mut fixture = build_attention_fixture();
    fixture.write_build_pane_output(
        b"\x1b]99;i=7:p=body;deploy done\x1b\\",
        build_wall_time(1_000),
    );
    let _ = fixture.take_alice_deliveries();
    let second_read_at = build_wall_time(1_001);

    fixture.write_build_pane_output(b"\x1b]99;i=7:p=close;\x1b\\\x07", second_read_at);

    assert_eq!(
        fixture.take_alice_deliveries(),
        [build_raised_delivery(
            fixture.build_pane_id,
            fixture.code_tab_id,
            AttentionReason::Bell,
            second_read_at
        )]
    );
    assert_eq!(
        fixture.find_pane_attention(fixture.build_pane_id),
        Some(build_bell_attention(second_read_at))
    );
}

#[test]
fn the_same_notification_again_raises_nothing_and_takes_the_new_id() {
    let first_read_at = build_wall_time(1_000);
    let mut fixture = build_attention_fixture();
    fixture.write_build_pane_output(b"\x1b]99;i=7:p=body;deploy done\x1b\\", first_read_at);
    let _ = fixture.take_alice_deliveries();

    fixture.write_build_pane_output(
        b"\x1b]99;i=8:p=body;deploy done\x1b\\",
        build_wall_time(1_001),
    );

    assert_eq!(fixture.take_alice_deliveries(), Vec::new());
    assert_eq!(
        fixture.find_pane_attention(fixture.build_pane_id),
        Some(Attention {
            raised_at: first_read_at,
            reason: AttentionReason::Notify {
                title: None,
                body: "deploy done".to_string(),
            },
            withdrawable_notification_id: Some("8".to_string()),
        })
    );
}

// ------------------------------------------------- a client's own acts ----

#[test]
fn a_key_binding_or_mouse_command_that_lands_the_input_on_a_waiting_pane_clears_it() {
    for build_command_source in [CommandSource::from_key_binding, CommandSource::from_mouse] {
        let mut fixture = build_fixture_with_waiting_build_pane(build_wall_time(1_000));
        let alice_client_id = fixture.alice_client_id;
        fixture.focus_alice_on_build_pane_in_code_tab();

        fixture.dispatch_command(
            build_command_source(alice_client_id),
            Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Id(fixture.code_tab_id),
                client_id: None,
            }),
        );

        assert_eq!(
            fixture.find_input_pane_id(alice_client_id),
            Some(fixture.build_pane_id)
        );
        assert_eq!(
            fixture.take_alice_deliveries(),
            [build_cleared_delivery(
                fixture.build_pane_id,
                fixture.code_tab_id,
                Some(alice_client_id)
            )]
        );
        assert_eq!(fixture.find_pane_attention(fixture.build_pane_id), None);
    }
}

#[test]
fn a_cli_command_that_lands_the_input_on_a_waiting_pane_clears_nothing() {
    let first_read_at = build_wall_time(1_000);
    for cli_source_index in 0..2 {
        let mut fixture = build_fixture_with_waiting_build_pane(first_read_at);
        let alice_client_id = fixture.alice_client_id;
        fixture.focus_alice_on_build_pane_in_code_tab();
        let command_source = if cli_source_index == 0 {
            build_in_session_source(
                fixture.session_id,
                Some(alice_client_id),
                fixture.docs_pane_id,
            )
        } else {
            CommandSource::from_external_cli(Some(fixture.session_id), None)
        };

        fixture.dispatch_command(
            command_source,
            Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Id(fixture.code_tab_id),
                client_id: Some(alice_client_id),
            }),
        );

        assert_eq!(
            fixture.find_input_pane_id(alice_client_id),
            Some(fixture.build_pane_id)
        );
        assert_eq!(fixture.take_alice_deliveries(), Vec::new());
        assert_eq!(
            fixture.find_pane_attention(fixture.build_pane_id),
            Some(build_bell_attention(first_read_at))
        );
    }
}

#[test]
fn a_key_binding_that_leaves_the_input_where_it_was_clears_nothing() {
    let first_read_at = build_wall_time(1_000);
    let mut fixture = build_attention_fixture();
    let bob_client_id = fixture.attach_bob_to_build_pane(TerminalFocus::Unreported);
    fixture.write_build_pane_output(b"\x07", first_read_at);
    let _ = fixture.take_alice_deliveries();

    fixture.dispatch_command(
        CommandSource::from_key_binding(bob_client_id),
        Command::ToggleLockMode(ToggleLockModeArgs::default()),
    );

    assert_eq!(fixture.take_alice_deliveries(), Vec::new());
    assert_eq!(
        fixture.find_pane_attention(fixture.build_pane_id),
        Some(build_bell_attention(first_read_at))
    );
}

#[test]
fn focus_repair_after_a_pane_closes_leaves_the_landing_pane_waiting() {
    let first_read_at = build_wall_time(1_000);
    for close_index in 0..3 {
        let mut fixture = build_attention_fixture();
        let bob_client_id = fixture.attach_bob_to_build_pane(TerminalFocus::Unreported);
        fixture.write_build_pane_output(b"\x07", first_read_at);
        let _ = fixture.take_alice_deliveries();
        let docs_pane_id = fixture.docs_pane_id;

        if close_index == 0 {
            fixture
                .server
                .handle_child_exit(docs_pane_id, ExitStatus::ExitCode(0));
        } else {
            let closing_client_id = if close_index == 1 {
                bob_client_id
            } else {
                fixture.alice_client_id
            };
            fixture.dispatch_command(
                CommandSource::from_key_binding(closing_client_id),
                Command::ClosePane(ClosePaneArgs {
                    pane_id: Some(docs_pane_id),
                    should_force_close: false,
                    should_kill_process_tree: false,
                }),
            );
        }

        assert_eq!(
            fixture.find_input_pane_id(fixture.alice_client_id),
            Some(fixture.build_pane_id),
            "close {close_index}"
        );
        assert_eq!(
            fixture.take_alice_deliveries(),
            Vec::new(),
            "close {close_index}"
        );
        assert_eq!(
            fixture.find_pane_attention(fixture.build_pane_id),
            Some(build_bell_attention(first_read_at)),
            "close {close_index}"
        );
    }
}

#[test]
fn a_focus_in_from_the_client_whose_input_the_waiting_pane_takes_clears_it() {
    let mut fixture = build_attention_fixture();
    let bob_client_id = fixture.attach_bob_to_build_pane(TerminalFocus::Unfocused);
    fixture.write_build_pane_output(b"\x07", build_wall_time(1_000));
    let _ = fixture.take_alice_deliveries();

    fixture
        .server
        .handle_terminal_focus_change(bob_client_id, true);

    assert_eq!(
        fixture
            .server
            .get_client_mut(bob_client_id)
            .expect("Bob is attached")
            .get_terminal_focus(),
        TerminalFocus::Focused
    );
    assert_eq!(
        fixture.take_alice_deliveries(),
        [build_cleared_delivery(
            fixture.build_pane_id,
            fixture.code_tab_id,
            Some(bob_client_id)
        )]
    );
    assert_eq!(fixture.find_pane_attention(fixture.build_pane_id), None);
}

#[test]
fn a_focus_change_that_is_not_a_focus_in_on_the_waiting_pane_clears_nothing() {
    let first_read_at = build_wall_time(1_000);
    let mut fixture = build_attention_fixture();
    let bob_client_id = fixture.attach_bob_to_build_pane(TerminalFocus::Unreported);
    fixture.write_build_pane_output(b"\x07", first_read_at);
    let _ = fixture.take_alice_deliveries();
    let alice_client_id = fixture.alice_client_id;

    fixture
        .server
        .handle_terminal_focus_change(bob_client_id, false);
    fixture
        .server
        .handle_terminal_focus_change(alice_client_id, true);
    fixture
        .server
        .handle_terminal_focus_change(ClientId::new(), true);

    for (client_id, expected_terminal_focus) in [
        (bob_client_id, TerminalFocus::Unfocused),
        (alice_client_id, TerminalFocus::Focused),
    ] {
        assert_eq!(
            fixture
                .server
                .get_client_mut(client_id)
                .expect("the client is attached")
                .get_terminal_focus(),
            expected_terminal_focus
        );
    }
    assert_eq!(fixture.take_alice_deliveries(), Vec::new());
    assert_eq!(
        fixture.find_pane_attention(fixture.build_pane_id),
        Some(build_bell_attention(first_read_at))
    );
}

#[test]
fn a_key_a_paste_a_mouse_report_or_a_bound_write_into_the_waiting_pane_clears_it() {
    for input_index in 0..4 {
        let mut fixture = build_attention_fixture();
        let bob_client_id = fixture.attach_bob_to_build_pane(TerminalFocus::Unreported);
        fixture.write_build_pane_output(b"\x1b[?1000h\x1b[?1006h\x07", build_wall_time(1_000));
        let _ = fixture.take_alice_deliveries();
        let build_pane_id = fixture.build_pane_id;

        match input_index {
            0 => fixture
                .server
                .handle_key_inputs(bob_client_id, &[build_enter_press()]),
            1 => fixture.server.handle_host_paste(bob_client_id, "ls"),
            2 => fixture.server.forward_mouse_to_pane(
                bob_client_id,
                build_pane_id,
                MouseKind::Press(MouseButton::Left),
                BindingModifierFlags::NONE,
                Point { column: 0, row: 0 },
            ),
            _ => fixture.dispatch_command(
                CommandSource::from_key_binding(bob_client_id),
                Command::WriteToPane(WriteToPaneArgs {
                    pane_id: Some(build_pane_id),
                    pane_input_bytes: b"x".to_vec(),
                }),
            ),
        }

        assert_eq!(
            fixture.take_alice_deliveries(),
            [build_cleared_delivery(
                build_pane_id,
                fixture.code_tab_id,
                Some(bob_client_id)
            )],
            "input {input_index}"
        );
        assert_eq!(
            fixture.find_pane_attention(build_pane_id),
            None,
            "input {input_index}"
        );
    }
}

#[test]
fn koshi_input_into_the_waiting_pane_clears_nothing() {
    let first_read_at = build_wall_time(1_000);
    let mut fixture = build_fixture_with_waiting_build_pane(first_read_at);
    let build_pane_id = fixture.build_pane_id;

    fixture.dispatch_command(
        build_in_session_source(
            fixture.session_id,
            Some(fixture.alice_client_id),
            fixture.docs_pane_id,
        ),
        Command::WriteToPane(WriteToPaneArgs {
            pane_id: Some(build_pane_id),
            pane_input_bytes: b"ls\r".to_vec(),
        }),
    );

    assert_eq!(
        fixture
            .fake_pty_backend
            .list_pane_write_bytes(build_pane_id)
            .expect("the pane exists"),
        [b"ls\r".to_vec()]
    );
    assert_eq!(fixture.take_alice_deliveries(), Vec::new());
    assert_eq!(
        fixture.find_pane_attention(build_pane_id),
        Some(build_bell_attention(first_read_at))
    );
}

// ------------------------------------------------ what outlives a wait ----

#[test]
fn a_closed_pane_takes_its_attention_and_its_command_start_with_it() {
    let mut fixture = build_attention_fixture();
    let build_pane_id = fixture.build_pane_id;
    let read_instant = Instant::now();
    fixture.server.apply_program_facts(
        build_pane_id,
        build_timed_program_facts(
            vec![ProgramFact::CommandStarted, ProgramFact::Bell],
            read_instant,
        ),
        build_wall_time(1_000),
        read_instant,
    );

    fixture.dispatch_command(
        CommandSource::from_external_cli(Some(fixture.session_id), None),
        Command::ClosePane(ClosePaneArgs {
            pane_id: Some(build_pane_id),
            should_force_close: false,
            should_kill_process_tree: false,
        }),
    );

    assert_eq!(
        fixture.server.session_by_id[&fixture.session_id]
            .panes
            .get_pane_record_by_id(build_pane_id),
        None
    );
    assert_eq!(
        fixture
            .server
            .command_started_at_by_pane_id
            .get(&build_pane_id),
        None
    );
    assert_eq!(fixture.take_alice_deliveries(), Vec::new());
}

#[test]
fn a_waiting_pane_keeps_waiting_when_every_client_detaches() {
    let first_read_at = build_wall_time(1_000);
    let mut fixture = build_fixture_with_waiting_build_pane(first_read_at);
    let alice_client_id = fixture.alice_client_id;

    fixture.server.handle_client_detach(alice_client_id);

    assert_eq!(
        fixture.find_pane_attention(fixture.build_pane_id),
        Some(build_bell_attention(first_read_at))
    );
}

#[test]
fn switching_the_reason_off_leaves_a_waiting_pane_waiting_until_a_clear() {
    let first_read_at = build_wall_time(1_000);
    let mut fixture = build_attention_fixture();
    let bob_client_id = fixture.attach_bob_to_build_pane(TerminalFocus::Unreported);
    fixture.write_build_pane_output(b"\x07", first_read_at);
    let _ = fixture.take_alice_deliveries();

    fixture.server.config.attention.should_raise_for_bell = false;
    fixture.write_build_pane_output(b"\x07", build_wall_time(1_001));

    assert_eq!(fixture.take_alice_deliveries(), Vec::new());
    assert_eq!(
        fixture.find_pane_attention(fixture.build_pane_id),
        Some(build_bell_attention(first_read_at))
    );

    fixture
        .server
        .handle_key_inputs(bob_client_id, &[build_enter_press()]);

    assert_eq!(
        fixture.take_alice_deliveries(),
        [build_cleared_delivery(
            fixture.build_pane_id,
            fixture.code_tab_id,
            Some(bob_client_id)
        )]
    );
}
