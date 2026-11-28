//! Tests for pane attention in the session server: which program facts make a
//! pane wait for a person, which acts end the wait, and what every subscribed
//! client receives.

use std::sync::{mpsc, Arc};
use std::time::Duration;

use koshi_config::types::AttentionConfig;
use koshi_core::attention::AttentionReasonKind;
use koshi_core::command::{
    ClosePaneArgs, Command, CommandEnvelope, CommandResult, CommandSource, FocusPaneArgs,
    FocusTabArgs, FocusTarget, TabTarget, ToggleLockModeArgs, WriteToPaneArgs,
};
use koshi_core::event::PaneRemoved;
use koshi_core::geometry::Point;
use koshi_core::ids::{CommandId, SessionId, TabId};
use koshi_core::key::{
    BindingModifierFlags, Key, KeyEventKind, KeyIdentity, KeyInput, KeyModifierFlags, NamedKey,
};
use koshi_core::mouse::{MouseButton, MouseKind};
use koshi_core::notification::NotificationSeverity;
use koshi_core::process::{ExitStatus, PtySize};
use koshi_core::program_status::{
    BlockedKind, ProgramStatusRecord, ProgramStatusState, PROGRAM_STATUS_RECORDS_MAX,
};
use koshi_layout::tree::LayoutNode;
use koshi_pane::pane::state::PaneRecord;
use koshi_pty::backend::state::PtyBackend;
use koshi_renderer::snapshot::{Delivery, RenderSnapshot};
use koshi_session::client::ClientRegistry;
use koshi_session::session::state::Session;
use koshi_terminal::engine::{TerminalEngine, SYNCHRONIZED_OUTPUT_TIMEOUT_DURATION};
use koshi_terminal::state::ProgramStatusFact;
use koshi_test_support::fake_pty::FakePtyBackend;
use koshi_test_support::fixtures::build_spawn_spec;

use crate::runtime::command::tests::{
    attach_client, build_floating_new_pane_args, build_in_session_source, build_tab,
    create_floating_pane,
};
use crate::runtime::delivery_queue::DeliveryReceiver;
use crate::runtime::pty_inbox::InboxSink;
use crate::runtime::render_schedule::tests::{
    mark_changed_views_then_compute_frame_wait, DEFAULT_FRAME_INTERVAL_DURATION,
};

use super::*;

/// What one queued notification entry holds, as
/// [`AttentionFixture::list_session_entries`] lists it.
type SessionEntrySummary = (
    NotificationSeverity,
    String,
    Option<NotificationSubject>,
    u32,
);

/// The severity, the text after `<place> › <pane>` and the reason kind of the
/// one entry a raise posts, or `None` for a raise that posts no entry.
type ExpectedPostedEntry = Option<(NotificationSeverity, &'static str, AttentionReasonKind)>;

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
    docs_tab_id: TabId,
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
        docs_tab_id,
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
    /// first, except each `NotificationPosted`:
    /// [`list_session_entries`](Self::list_session_entries) reads the entries
    /// from the session's queue.
    fn take_alice_deliveries(&self) -> Vec<Delivery> {
        self.alice_delivery_receiver
            .try_iter()
            .filter(|delivery| !matches!(delivery, Delivery::NotificationPosted { .. }))
            .collect()
    }

    /// The severity, the text, the subject and the repeat count of each entry
    /// of the session's notification queue, oldest first.
    fn list_session_entries(&self) -> Vec<SessionEntrySummary> {
        self.server.session_by_id[&self.session_id]
            .notification_queue
            .list_queued_entries()
            .map(|queued_entry| {
                (
                    queued_entry.severity,
                    queued_entry.text.clone(),
                    queued_entry.subject,
                    queued_entry.repeat_count,
                )
            })
            .collect()
    }

    /// The reason kinds `build_pane_id` posted an entry for since its last
    /// visit.
    fn list_build_pane_notified_kinds(&self) -> Vec<AttentionReasonKind> {
        self.server.session_by_id[&self.session_id]
            .panes
            .get_pane_record_by_id(self.build_pane_id)
            .expect("the pane has a record")
            .notified_attention_kinds
            .clone()
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

    /// The program status records that `pane_id`'s record holds.
    fn list_program_status_records(&self, pane_id: PaneId) -> Vec<ProgramStatusRecord> {
        self.server.session_by_id[&self.session_id]
            .panes
            .get_pane_record_by_id(pane_id)
            .expect("the pane has a record")
            .program_status_records
            .clone()
    }

    /// Apply `program_facts` to `build_pane_id` as one read at `read_at`, and
    /// return their events.
    fn apply_build_pane_facts(
        &mut self,
        program_facts: Vec<ProgramFact>,
        read_at: SystemTime,
    ) -> Vec<Event> {
        let read_instant = Instant::now();
        self.server.apply_program_facts(
            self.build_pane_id,
            build_timed_program_facts(program_facts, read_instant),
            read_at,
            read_instant,
        )
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

/// An OSC 7501 report for `record_id` in `program_status_state` with
/// `message`, and no kind, progress, app name or title.
fn build_report_fact(
    record_id: &str,
    program_status_state: ProgramStatusState,
    message: Option<&str>,
) -> ProgramFact {
    ProgramFact::ProgramStatus(ProgramStatusFact::Reported {
        record_id: record_id.to_string(),
        program_status_state,
        blocked_kind: None,
        progress_percent: None,
        app_name: None,
        title: None,
        message: message.map(str::to_string),
    })
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
        AttentionReasonKind::ProgramBlocked => {
            attention_config.should_raise_for_program_blocked = false;
        }
        AttentionReasonKind::ProgramDone => attention_config.should_raise_for_program_done = false,
        AttentionReasonKind::ProgramFailed => {
            attention_config.should_raise_for_program_failed = false;
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
        (
            AttentionReasonKind::ProgramBlocked,
            build_report_fact("", ProgramStatusState::Blocked, None),
        ),
        (
            AttentionReasonKind::ProgramDone,
            build_report_fact("", ProgramStatusState::Done, None),
        ),
        (
            AttentionReasonKind::ProgramFailed,
            build_report_fact("", ProgramStatusState::Error, None),
        ),
    ] {
        let mut fixture = build_attention_fixture();
        switch_off_attention_kind(&mut fixture.server.config.attention, attention_reason_kind);
        let expected_events = match attention_reason_kind {
            AttentionReasonKind::CommandFinished => {
                build_command_finished_events(fixture.build_pane_id)
            }
            AttentionReasonKind::Bell
            | AttentionReasonKind::Notify
            | AttentionReasonKind::ProgramBlocked
            | AttentionReasonKind::ProgramDone
            | AttentionReasonKind::ProgramFailed => Vec::new(),
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
fn a_blocked_report_that_a_clear_removes_in_the_same_read_gives_no_event() {
    let mut fixture = build_attention_fixture();
    let read_instant = Instant::now();

    let emitted_events = fixture.server.apply_program_facts(
        fixture.build_pane_id,
        build_timed_program_facts(
            vec![
                ProgramFact::ProgramStatus(ProgramStatusFact::Reported {
                    record_id: String::new(),
                    program_status_state: ProgramStatusState::Blocked,
                    blocked_kind: Some(BlockedKind::Permission),
                    progress_percent: None,
                    app_name: Some("terraform".to_string()),
                    title: None,
                    message: Some("Apply?".to_string()),
                }),
                ProgramFact::ProgramStatus(ProgramStatusFact::Cleared {
                    record_id: String::new(),
                }),
                ProgramFact::ProgramStatus(ProgramStatusFact::PromptStarted),
                ProgramFact::ProgramStatus(ProgramStatusFact::TerminalReset),
            ],
            read_instant,
        ),
        build_wall_time(1_000),
        read_instant,
    );

    assert_eq!(emitted_events, Vec::new());
    assert_eq!(fixture.find_pane_attention(fixture.build_pane_id), None);
    assert_eq!(
        fixture.list_program_status_records(fixture.build_pane_id),
        Vec::new()
    );
}

#[test]
fn a_bell_between_program_status_facts_raises_as_it_does_alone() {
    let mut fixture = build_attention_fixture();
    let read_instant = Instant::now();
    let read_at = build_wall_time(1_000);

    let emitted_events = fixture.server.apply_program_facts(
        fixture.build_pane_id,
        build_timed_program_facts(
            vec![
                ProgramFact::ProgramStatus(ProgramStatusFact::PromptStarted),
                ProgramFact::Bell,
                ProgramFact::ProgramStatus(ProgramStatusFact::TerminalReset),
            ],
            read_instant,
        ),
        read_at,
        read_instant,
    );

    assert_eq!(
        emitted_events,
        vec![Event::PaneAttentionRaised(PaneAttentionRaised {
            pane_id: fixture.build_pane_id,
            tab_id: Some(fixture.code_tab_id),
            reason: AttentionReason::Bell,
            raised_at: read_at,
        })]
    );
    assert_eq!(
        fixture.find_pane_attention(fixture.build_pane_id),
        Some(Attention {
            raised_at: read_at,
            reason: AttentionReason::Bell,
            withdrawable_notification_id: None,
        })
    );
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
fn a_focus_pane_on_a_waiting_pane_in_another_tab_clears_it_from_a_mouse_and_not_from_the_cli() {
    let first_read_at = build_wall_time(1_000);
    for is_mouse_source in [true, false] {
        let mut fixture = build_fixture_with_waiting_build_pane(first_read_at);
        let alice_client_id = fixture.alice_client_id;
        let command_source = if is_mouse_source {
            CommandSource::from_mouse(alice_client_id)
        } else {
            CommandSource::from_external_cli(Some(fixture.session_id), None)
        };

        fixture.dispatch_command(
            command_source,
            Command::FocusPane(FocusPaneArgs {
                focus_target: FocusTarget::Pane(fixture.build_pane_id),
                client_id: Some(alice_client_id),
            }),
        );

        assert_eq!(
            fixture.find_input_pane_id(alice_client_id),
            Some(fixture.build_pane_id),
            "mouse source: {is_mouse_source}"
        );
        if is_mouse_source {
            assert_eq!(
                fixture.take_alice_deliveries(),
                [build_cleared_delivery(
                    fixture.build_pane_id,
                    fixture.code_tab_id,
                    Some(alice_client_id)
                )]
            );
            assert_eq!(fixture.find_pane_attention(fixture.build_pane_id), None);
        } else {
            assert_eq!(fixture.take_alice_deliveries(), Vec::new());
            assert_eq!(
                fixture.find_pane_attention(fixture.build_pane_id),
                Some(build_bell_attention(first_read_at))
            );
        }
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
            let child_exit_events = fixture
                .server
                .handle_child_exit(docs_pane_id, ExitStatus::ExitCode(0));
            fixture.server.publish_events(&child_exit_events);
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
            vec![Delivery::PaneRemoved(PaneRemoved {
                pane_id: docs_pane_id,
                tab_id: Some(fixture.docs_tab_id),
            })],
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
    assert_eq!(
        fixture.take_alice_deliveries(),
        vec![Delivery::PaneRemoved(PaneRemoved {
            pane_id: build_pane_id,
            tab_id: Some(fixture.code_tab_id),
        })]
    );
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

// ------------------------------------------------------------- frames ----

/// Each tab of `render_snapshot`'s tab bar as its name and whether a pane of
/// its layout tree waits for a person, then the session's count of waiting
/// panes.
fn list_waiting_marks(render_snapshot: &RenderSnapshot) -> (Vec<(String, bool)>, usize) {
    let tab_waiting_marks = render_snapshot
        .session_snapshot
        .tabs_metadata
        .iter()
        .map(|tab_metadata| (tab_metadata.tab_name.clone(), tab_metadata.has_waiting_pane))
        .collect();
    (
        tab_waiting_marks,
        render_snapshot.session_snapshot.waiting_pane_count,
    )
}

/// The waiting marks ([`list_waiting_marks`]) of every frame in
/// `deliveries`, oldest first.
fn list_delivered_frame_waiting_marks(
    deliveries: Vec<Delivery>,
) -> Vec<(Vec<(String, bool)>, usize)> {
    deliveries
        .iter()
        .filter_map(|delivery| match delivery {
            Delivery::Frame(render_snapshot) => Some(list_waiting_marks(render_snapshot)),
            _ => None,
        })
        .collect()
}

/// Alice views tab `docs`. A bell inside an open synchronized-output group in
/// `build_pane_id` raises nothing while the group is open. The pass whose
/// time ends the group raises the wait and delivers Alice a frame that marks
/// tab `code`.
#[test]
fn a_bell_released_when_a_synchronized_output_group_times_out_marks_the_frame_of_the_same_pass() {
    let mut fixture = build_attention_fixture();
    let build_pane_id = fixture.build_pane_id;
    fixture.server.push_due_frames(Instant::now());
    let _ = fixture.take_alice_deliveries();

    fixture.write_build_pane_output(b"\x1b[?2026h\x07", build_wall_time(1_000));
    assert_eq!(fixture.find_pane_attention(build_pane_id), None);

    fixture
        .server
        .push_due_frames(Instant::now() + SYNCHRONIZED_OUTPUT_TIMEOUT_DURATION);

    assert_eq!(
        fixture
            .find_pane_attention(build_pane_id)
            .map(|attention| attention.reason),
        Some(AttentionReason::Bell)
    );
    assert_eq!(
        list_delivered_frame_waiting_marks(fixture.take_alice_deliveries()),
        [(
            vec![("code".to_string(), true), ("docs".to_string(), false)],
            1
        )]
    );
}

/// Alice views tab `docs`. Output in `build_pane_id`, in tab `code`, leaves
/// her frame as it is, but a raise and a clear of its wait each repaint it.
#[test]
fn a_raise_and_a_clear_in_a_tab_the_client_does_not_view_repaint_its_frame() {
    let mut fixture = build_attention_fixture();
    let alice_client_id = fixture.alice_client_id;
    let first_frame_time = Instant::now();
    fixture.server.push_due_frames(first_frame_time);
    let _ = fixture.take_alice_deliveries();

    fixture.write_build_pane_output(b"hello", build_wall_time(1_000));
    assert_eq!(
        mark_changed_views_then_compute_frame_wait(
            &mut fixture.server,
            alice_client_id,
            first_frame_time
        ),
        None
    );

    fixture.write_build_pane_output(
        b"\x1b]99;i=7:p=body;deploy done\x1b\\",
        build_wall_time(1_001),
    );
    assert_eq!(
        mark_changed_views_then_compute_frame_wait(
            &mut fixture.server,
            alice_client_id,
            first_frame_time
        ),
        Some(DEFAULT_FRAME_INTERVAL_DURATION)
    );
    let second_frame_time = first_frame_time + DEFAULT_FRAME_INTERVAL_DURATION;
    fixture.server.push_due_frames(second_frame_time);
    assert_eq!(
        list_delivered_frame_waiting_marks(fixture.take_alice_deliveries()),
        [(
            vec![("code".to_string(), true), ("docs".to_string(), false)],
            1
        )]
    );

    fixture.write_build_pane_output(b"\x1b]99;i=7:p=close;\x1b\\", build_wall_time(1_002));
    assert_eq!(
        mark_changed_views_then_compute_frame_wait(
            &mut fixture.server,
            alice_client_id,
            second_frame_time
        ),
        Some(DEFAULT_FRAME_INTERVAL_DURATION)
    );
    fixture
        .server
        .push_due_frames(second_frame_time + DEFAULT_FRAME_INTERVAL_DURATION);
    assert_eq!(
        list_delivered_frame_waiting_marks(fixture.take_alice_deliveries()),
        [(
            vec![("code".to_string(), false), ("docs".to_string(), false)],
            0
        )]
    );
}

// ------------------------------------------------------ program status ----

/// The `PaneAttentionRaised` event of `build_pane_id` in tab `code`.
fn build_build_pane_raised_event(
    fixture: &AttentionFixture,
    reason: AttentionReason,
    raised_at: SystemTime,
) -> Event {
    Event::PaneAttentionRaised(PaneAttentionRaised {
        pane_id: fixture.build_pane_id,
        tab_id: Some(fixture.code_tab_id),
        reason,
        raised_at,
    })
}

/// The `PaneAttentionCleared` event of `build_pane_id` in tab `code` that
/// names `client_id`.
fn build_build_pane_cleared_event(
    fixture: &AttentionFixture,
    client_id: Option<ClientId>,
) -> Event {
    Event::PaneAttentionCleared(PaneAttentionCleared {
        pane_id: fixture.build_pane_id,
        tab_id: Some(fixture.code_tab_id),
        client_id,
    })
}

/// `ProgramBlocked` for the root record, with no kind and `message`.
fn build_root_blocked_reason(message: Option<&str>) -> AttentionReason {
    AttentionReason::ProgramBlocked {
        record_id: String::new(),
        blocked_kind: None,
        message: message.map(str::to_string),
    }
}

/// A fixture whose `build_pane_id` waits on its root record's `blocked`
/// report, read at `raised_at`.
fn build_fixture_with_blocked_build_pane(raised_at: SystemTime) -> AttentionFixture {
    let mut fixture = build_attention_fixture();
    let emitted_events = fixture.apply_build_pane_facts(
        vec![build_report_fact("", ProgramStatusState::Blocked, None)],
        raised_at,
    );
    assert_eq!(
        emitted_events,
        [build_build_pane_raised_event(
            &fixture,
            build_root_blocked_reason(None),
            raised_at
        )]
    );
    fixture
}

#[test]
fn a_blocked_report_in_a_pane_nobody_watches_raises_and_keeps_the_record() {
    let mut fixture = build_attention_fixture();
    let read_at = build_wall_time(1_000);

    fixture.write_build_pane_output(
        b"\x1b]7501;state=blocked:kind=question:app=codex\x1b\\",
        read_at,
    );

    let expected_reason = AttentionReason::ProgramBlocked {
        record_id: String::new(),
        blocked_kind: Some(BlockedKind::Question),
        message: None,
    };
    assert_eq!(
        fixture.take_alice_deliveries(),
        [build_raised_delivery(
            fixture.build_pane_id,
            fixture.code_tab_id,
            expected_reason.clone(),
            read_at
        )]
    );
    assert_eq!(
        fixture.find_pane_attention(fixture.build_pane_id),
        Some(Attention {
            raised_at: read_at,
            reason: expected_reason,
            withdrawable_notification_id: None,
        })
    );
    assert_eq!(
        fixture.list_program_status_records(fixture.build_pane_id),
        [ProgramStatusRecord {
            record_id: String::new(),
            program_status_state: ProgramStatusState::Blocked,
            blocked_kind: Some(BlockedKind::Question),
            progress_percent: None,
            app_name: Some("codex".to_string()),
            title: None,
            message: None,
            is_seen: false,
            updated_at: read_at,
        }]
    );
}

#[test]
fn a_working_report_after_blocked_ends_the_wait_with_no_client() {
    let mut fixture = build_fixture_with_blocked_build_pane(build_wall_time(1_000));

    let emitted_events = fixture.apply_build_pane_facts(
        vec![build_report_fact("", ProgramStatusState::Working, None)],
        build_wall_time(1_010),
    );

    assert_eq!(
        emitted_events,
        [build_build_pane_cleared_event(&fixture, None)]
    );
    assert_eq!(fixture.find_pane_attention(fixture.build_pane_id), None);
}

#[test]
fn a_blocked_report_in_the_focused_input_pane_is_seen_and_raises_nothing() {
    let mut fixture = build_attention_fixture();
    fixture.attach_bob_to_build_pane(TerminalFocus::Focused);

    let emitted_events = fixture.apply_build_pane_facts(
        vec![build_report_fact("", ProgramStatusState::Blocked, None)],
        build_wall_time(1_000),
    );

    assert_eq!(emitted_events, Vec::new());
    assert_eq!(fixture.find_pane_attention(fixture.build_pane_id), None);
    let program_status_records = fixture.list_program_status_records(fixture.build_pane_id);
    assert_eq!(
        program_status_records
            .iter()
            .map(|program_status_record| program_status_record.is_seen)
            .collect::<Vec<_>>(),
        [true]
    );
}

#[test]
fn done_and_error_reports_raise_their_reasons() {
    for (program_status_state, expected_reason) in [
        (
            ProgramStatusState::Done,
            AttentionReason::ProgramDone {
                record_id: "deploy".to_string(),
                message: Some("shipped".to_string()),
            },
        ),
        (
            ProgramStatusState::Error,
            AttentionReason::ProgramFailed {
                record_id: "deploy".to_string(),
                message: Some("shipped".to_string()),
            },
        ),
    ] {
        let mut fixture = build_attention_fixture();
        let read_at = build_wall_time(1_000);

        let emitted_events = fixture.apply_build_pane_facts(
            vec![build_report_fact(
                "deploy",
                program_status_state,
                Some("shipped"),
            )],
            read_at,
        );

        assert_eq!(
            emitted_events,
            [build_build_pane_raised_event(
                &fixture,
                expected_reason,
                read_at
            )],
            "{program_status_state:?}"
        );
    }
}

#[test]
fn blocked_then_done_replaces_the_reason_and_keeps_the_start_of_the_wait() {
    let raised_at = build_wall_time(1_000);
    let mut fixture = build_fixture_with_blocked_build_pane(raised_at);

    let emitted_events = fixture.apply_build_pane_facts(
        vec![build_report_fact("", ProgramStatusState::Done, None)],
        build_wall_time(1_240),
    );

    let expected_reason = AttentionReason::ProgramDone {
        record_id: String::new(),
        message: None,
    };
    assert_eq!(
        emitted_events,
        [build_build_pane_raised_event(
            &fixture,
            expected_reason,
            raised_at
        )]
    );
}

#[test]
fn blocked_then_done_in_one_read_gives_one_raise_with_the_newest_reason() {
    let mut fixture = build_attention_fixture();
    let read_at = build_wall_time(1_000);

    let emitted_events = fixture.apply_build_pane_facts(
        vec![
            build_report_fact("", ProgramStatusState::Blocked, None),
            build_report_fact("", ProgramStatusState::Done, None),
        ],
        read_at,
    );

    assert_eq!(
        emitted_events,
        [build_build_pane_raised_event(
            &fixture,
            AttentionReason::ProgramDone {
                record_id: String::new(),
                message: None,
            },
            read_at
        )]
    );
}

#[test]
fn a_rewrite_of_the_waiting_record_with_a_new_message_updates_the_reason() {
    let raised_at = build_wall_time(1_000);
    let mut fixture = build_fixture_with_blocked_build_pane(raised_at);

    let first_rewrite_events = fixture.apply_build_pane_facts(
        vec![build_report_fact(
            "",
            ProgramStatusState::Blocked,
            Some("Overwrite file?"),
        )],
        build_wall_time(1_005),
    );
    let second_rewrite_events = fixture.apply_build_pane_facts(
        vec![build_report_fact(
            "",
            ProgramStatusState::Blocked,
            Some("Overwrite file?"),
        )],
        build_wall_time(1_006),
    );

    assert_eq!(
        first_rewrite_events,
        [build_build_pane_raised_event(
            &fixture,
            build_root_blocked_reason(Some("Overwrite file?")),
            raised_at
        )]
    );
    assert_eq!(second_rewrite_events, Vec::new());
}

#[test]
fn a_rewrite_in_the_same_state_of_a_pane_that_waits_for_nobody_raises_nothing() {
    let mut fixture = build_attention_fixture();
    let bob_client_id = fixture.attach_bob_to_build_pane(TerminalFocus::Focused);
    fixture.apply_build_pane_facts(
        vec![build_report_fact("", ProgramStatusState::Blocked, None)],
        build_wall_time(1_000),
    );
    fixture
        .server
        .handle_terminal_focus_change(bob_client_id, false);

    let emitted_events = fixture.apply_build_pane_facts(
        vec![build_report_fact(
            "",
            ProgramStatusState::Blocked,
            Some("Still waiting"),
        )],
        build_wall_time(1_005),
    );

    assert_eq!(emitted_events, Vec::new());
    assert_eq!(fixture.find_pane_attention(fixture.build_pane_id), None);
}

#[test]
fn a_blocked_report_on_a_pane_that_waits_on_a_bell_replaces_the_reason() {
    let raised_at = build_wall_time(1_000);
    let mut fixture = build_fixture_with_waiting_build_pane(raised_at);

    let emitted_events = fixture.apply_build_pane_facts(
        vec![build_report_fact("", ProgramStatusState::Blocked, None)],
        build_wall_time(1_030),
    );

    assert_eq!(
        emitted_events,
        [build_build_pane_raised_event(
            &fixture,
            build_root_blocked_reason(None),
            raised_at
        )]
    );
}

#[test]
fn a_change_to_another_record_that_raises_nothing_keeps_the_wait() {
    let mut fixture = build_fixture_with_blocked_build_pane(build_wall_time(1_000));

    let emitted_events = fixture.apply_build_pane_facts(
        vec![
            build_report_fact("deploy", ProgramStatusState::Working, None),
            build_report_fact("deploy", ProgramStatusState::Idle, None),
            ProgramFact::ProgramStatus(ProgramStatusFact::Cleared {
                record_id: "deploy".to_string(),
            }),
        ],
        build_wall_time(1_010),
    );

    assert_eq!(emitted_events, Vec::new());
    assert_eq!(
        fixture
            .find_pane_attention(fixture.build_pane_id)
            .map(|attention| attention.reason),
        Some(build_root_blocked_reason(None))
    );
}

#[test]
fn a_clear_of_the_waiting_record_or_its_ancestor_ends_the_wait() {
    for cleared_record_id in ["deploy/us-east", "deploy", ""] {
        let mut fixture = build_attention_fixture();
        fixture.apply_build_pane_facts(
            vec![build_report_fact(
                "deploy/us-east",
                ProgramStatusState::Error,
                None,
            )],
            build_wall_time(1_000),
        );

        let emitted_events = fixture.apply_build_pane_facts(
            vec![ProgramFact::ProgramStatus(ProgramStatusFact::Cleared {
                record_id: cleared_record_id.to_string(),
            })],
            build_wall_time(1_010),
        );

        assert_eq!(
            emitted_events,
            [build_build_pane_cleared_event(&fixture, None)],
            "clear {cleared_record_id:?}"
        );
    }
}

#[test]
fn a_prompt_start_ends_a_blocked_wait_and_keeps_a_done_wait() {
    let mut blocked_fixture = build_fixture_with_blocked_build_pane(build_wall_time(1_000));
    let blocked_events = blocked_fixture.apply_build_pane_facts(
        vec![ProgramFact::ProgramStatus(ProgramStatusFact::PromptStarted)],
        build_wall_time(1_010),
    );
    assert_eq!(
        blocked_events,
        [build_build_pane_cleared_event(&blocked_fixture, None)]
    );

    let mut done_fixture = build_attention_fixture();
    let done_read_at = build_wall_time(1_000);
    done_fixture.apply_build_pane_facts(
        vec![build_report_fact("", ProgramStatusState::Done, None)],
        done_read_at,
    );
    let done_events = done_fixture.apply_build_pane_facts(
        vec![ProgramFact::ProgramStatus(ProgramStatusFact::PromptStarted)],
        build_wall_time(1_010),
    );
    assert_eq!(done_events, Vec::new());
    assert_eq!(
        done_fixture
            .find_pane_attention(done_fixture.build_pane_id)
            .map(|attention| attention.reason),
        Some(AttentionReason::ProgramDone {
            record_id: String::new(),
            message: None,
        })
    );
    assert_eq!(
        done_fixture
            .list_program_status_records(done_fixture.build_pane_id)
            .len(),
        1
    );
}

#[test]
fn a_terminal_reset_removes_every_record_and_ends_a_done_wait() {
    let mut fixture = build_attention_fixture();
    fixture.apply_build_pane_facts(
        vec![build_report_fact("", ProgramStatusState::Done, None)],
        build_wall_time(1_000),
    );

    let emitted_events = fixture.apply_build_pane_facts(
        vec![ProgramFact::ProgramStatus(ProgramStatusFact::TerminalReset)],
        build_wall_time(1_010),
    );

    assert_eq!(
        emitted_events,
        [build_build_pane_cleared_event(&fixture, None)]
    );
    assert_eq!(
        fixture.list_program_status_records(fixture.build_pane_id),
        Vec::new()
    );
}

#[test]
fn a_new_id_that_removes_the_waiting_record_as_the_oldest_ends_the_wait() {
    let mut fixture = build_fixture_with_blocked_build_pane(build_wall_time(1_000));
    let working_reports = (1..PROGRAM_STATUS_RECORDS_MAX)
        .map(|record_index| {
            build_report_fact(
                &format!("task-{record_index}"),
                ProgramStatusState::Working,
                None,
            )
        })
        .collect();
    assert_eq!(
        fixture.apply_build_pane_facts(working_reports, build_wall_time(1_010)),
        Vec::new()
    );

    let emitted_events = fixture.apply_build_pane_facts(
        vec![build_report_fact(
            "task-64",
            ProgramStatusState::Working,
            None,
        )],
        build_wall_time(1_020),
    );

    assert_eq!(
        emitted_events,
        [build_build_pane_cleared_event(&fixture, None)]
    );
}

#[test]
fn blocked_then_done_while_a_person_watches_ends_the_blocked_wait() {
    let mut fixture = build_fixture_with_blocked_build_pane(build_wall_time(1_000));
    fixture.attach_bob_to_build_pane(TerminalFocus::Focused);

    let emitted_events = fixture.apply_build_pane_facts(
        vec![build_report_fact("", ProgramStatusState::Done, None)],
        build_wall_time(1_010),
    );

    assert_eq!(
        emitted_events,
        [build_build_pane_cleared_event(&fixture, None)]
    );
    assert_eq!(fixture.find_pane_attention(fixture.build_pane_id), None);
}

#[test]
fn program_done_switched_off_raises_nothing_and_keeps_the_record() {
    let mut fixture = build_attention_fixture();
    fixture
        .server
        .config
        .attention
        .should_raise_for_program_done = false;

    let emitted_events = fixture.apply_build_pane_facts(
        vec![build_report_fact("", ProgramStatusState::Done, None)],
        build_wall_time(1_000),
    );

    assert_eq!(emitted_events, Vec::new());
    assert_eq!(fixture.find_pane_attention(fixture.build_pane_id), None);
    assert_eq!(
        fixture
            .list_program_status_records(fixture.build_pane_id)
            .iter()
            .map(|program_status_record| program_status_record.program_status_state)
            .collect::<Vec<_>>(),
        [ProgramStatusState::Done]
    );
}

#[test]
fn a_working_report_then_a_prompt_start_in_one_read_leave_no_working_record() {
    let mut fixture = build_attention_fixture();

    fixture.write_build_pane_output(
        b"\x1b]7501;state=working:app=cargo\x1b\\\x1b]133;A\x1b\\",
        build_wall_time(1_000),
    );

    assert_eq!(
        fixture.list_program_status_records(fixture.build_pane_id),
        Vec::new()
    );
}

#[test]
fn a_person_at_the_pane_marks_every_record_seen_and_removes_none() {
    let mut fixture = build_fixture_with_blocked_build_pane(build_wall_time(1_000));
    fixture.apply_build_pane_facts(
        vec![build_report_fact(
            "deploy",
            ProgramStatusState::Working,
            None,
        )],
        build_wall_time(1_005),
    );
    let bob_client_id = fixture.attach_bob_to_build_pane(TerminalFocus::Unfocused);

    fixture
        .server
        .handle_terminal_focus_change(bob_client_id, true);

    assert_eq!(
        fixture.take_alice_deliveries(),
        [build_cleared_delivery(
            fixture.build_pane_id,
            fixture.code_tab_id,
            Some(bob_client_id)
        )]
    );
    assert_eq!(
        fixture
            .list_program_status_records(fixture.build_pane_id)
            .iter()
            .map(|program_status_record| (
                program_status_record.record_id.as_str().to_owned(),
                program_status_record.is_seen
            ))
            .collect::<Vec<_>>(),
        [(String::new(), true), ("deploy".to_string(), true)]
    );
}

#[test]
fn input_into_a_pane_that_waits_for_nobody_marks_its_records_seen() {
    let mut fixture = build_attention_fixture();
    fixture.apply_build_pane_facts(
        vec![build_report_fact("", ProgramStatusState::Working, None)],
        build_wall_time(1_000),
    );
    let bob_client_id = fixture.attach_bob_to_build_pane(TerminalFocus::Unfocused);
    fixture.take_alice_deliveries();

    fixture
        .server
        .clear_attended_pane_attention(bob_client_id, fixture.build_pane_id);

    assert_eq!(fixture.take_alice_deliveries(), Vec::new());
    assert_eq!(
        fixture
            .list_program_status_records(fixture.build_pane_id)
            .iter()
            .map(|program_status_record| program_status_record.is_seen)
            .collect::<Vec<_>>(),
        [true]
    );
}

#[test]
fn a_child_exit_removes_the_pane_with_its_records_and_its_wait() {
    let mut fixture = build_fixture_with_blocked_build_pane(build_wall_time(1_000));

    let _ = fixture
        .server
        .handle_child_exit(fixture.build_pane_id, ExitStatus::ExitCode(0));

    assert_eq!(
        fixture.server.session_by_id[&fixture.session_id]
            .panes
            .get_pane_record_by_id(fixture.build_pane_id),
        None
    );
}

/// An OSC 7501 report for `record_id` in `program_status_state` with
/// `blocked_kind`, `app_name`, `title` and `message`, and no progress.
fn build_labelled_report_fact(
    record_id: &str,
    program_status_state: ProgramStatusState,
    blocked_kind: Option<BlockedKind>,
    app_name: Option<&str>,
    title: Option<&str>,
    message: Option<&str>,
) -> ProgramFact {
    ProgramFact::ProgramStatus(ProgramStatusFact::Reported {
        record_id: record_id.to_string(),
        program_status_state,
        blocked_kind,
        progress_percent: None,
        app_name: app_name.map(str::to_string),
        title: title.map(str::to_string),
        message: message.map(str::to_string),
    })
}

/// A desktop notification titled `title` with `body` and no OSC 99 id.
fn build_notify_fact(title: Option<&str>, body: &str) -> ProgramFact {
    ProgramFact::Notify {
        title: title.map(str::to_string),
        body: body.to_string(),
        notification_id: None,
    }
}

/// The subject of an entry that an attention raise of `attention_reason_kind`
/// on `pane_id` posts.
fn build_pane_subject(
    pane_id: PaneId,
    attention_reason_kind: AttentionReasonKind,
) -> Option<NotificationSubject> {
    Some(NotificationSubject::Pane {
        pane_id,
        attention_reason_kind: Some(attention_reason_kind),
    })
}

/// Apply a command start and, `command_seconds` later, a command finish with
/// `exit_code` to `pane_id` as one read.
fn apply_timed_command(
    fixture: &mut AttentionFixture,
    pane_id: PaneId,
    exit_code: Option<i32>,
    command_seconds: u64,
) {
    let started_instant = Instant::now();
    let finished_instant = started_instant + Duration::from_secs(command_seconds);
    fixture.server.apply_program_facts(
        pane_id,
        vec![
            TimedProgramFact {
                program_fact: ProgramFact::CommandStarted,
                read_instant: started_instant,
            },
            TimedProgramFact {
                program_fact: ProgramFact::CommandFinished { exit_code },
                read_instant: finished_instant,
            },
        ],
        build_wall_time(1000),
        finished_instant,
    );
}

#[test]
fn each_raise_posts_one_entry_with_the_severity_and_text_of_its_reason() {
    let read_at = build_wall_time(100);
    let cases: Vec<(Vec<ProgramFact>, ExpectedPostedEntry)> = vec![
        (
            vec![build_notify_fact(Some("Claude Code"), "Waiting for input")],
            Some((
                NotificationSeverity::Info,
                " · Claude Code: Waiting for input",
                AttentionReasonKind::Notify,
            )),
        ),
        (
            vec![build_notify_fact(None, "Build done")],
            Some((
                NotificationSeverity::Info,
                " · Build done",
                AttentionReasonKind::Notify,
            )),
        ),
        (
            vec![build_notify_fact(Some("Build"), "")],
            Some((
                NotificationSeverity::Info,
                " · Build",
                AttentionReasonKind::Notify,
            )),
        ),
        (
            vec![build_notify_fact(None, "")],
            Some((NotificationSeverity::Info, "", AttentionReasonKind::Notify)),
        ),
        (
            vec![ProgramFact::CommandFinished { exit_code: None }],
            Some((
                NotificationSeverity::Info,
                " · finished",
                AttentionReasonKind::CommandFinished,
            )),
        ),
        (
            vec![ProgramFact::CommandFinished { exit_code: Some(1) }],
            Some((
                NotificationSeverity::Warning,
                " · failed — exit 1",
                AttentionReasonKind::CommandFinished,
            )),
        ),
        (
            vec![build_labelled_report_fact(
                "",
                ProgramStatusState::Blocked,
                Some(BlockedKind::Question),
                Some("codex"),
                None,
                Some("Which branch?"),
            )],
            Some((
                NotificationSeverity::Warning,
                " · codex: blocked (question) — Which branch?",
                AttentionReasonKind::ProgramBlocked,
            )),
        ),
        (
            vec![build_report_fact("", ProgramStatusState::Blocked, None)],
            Some((
                NotificationSeverity::Warning,
                " · blocked",
                AttentionReasonKind::ProgramBlocked,
            )),
        ),
        (
            vec![build_labelled_report_fact(
                "",
                ProgramStatusState::Done,
                None,
                Some("codex"),
                None,
                Some("Merged"),
            )],
            Some((
                NotificationSeverity::Info,
                " · codex: done — Merged",
                AttentionReasonKind::ProgramDone,
            )),
        ),
        (
            vec![build_report_fact(
                "",
                ProgramStatusState::Error,
                Some("Tests failed"),
            )],
            Some((
                NotificationSeverity::Error,
                " · failed — Tests failed",
                AttentionReasonKind::ProgramFailed,
            )),
        ),
        (vec![ProgramFact::Bell], None),
    ];
    for (program_facts, expected_entry) in cases {
        let mut fixture = build_attention_fixture();
        let build_pane_id = fixture.build_pane_id;
        let description = format!("{program_facts:?}");

        fixture.apply_build_pane_facts(program_facts, read_at);

        let pane_place = format!("code › {}", build_pane_id.format_short_label());
        let expected_entries: Vec<SessionEntrySummary> = expected_entry
            .into_iter()
            .map(|(severity, reason_text, attention_reason_kind)| {
                (
                    severity,
                    format!("{pane_place}{reason_text}"),
                    build_pane_subject(build_pane_id, attention_reason_kind),
                    1,
                )
            })
            .collect();
        assert_eq!(
            fixture.list_session_entries(),
            expected_entries,
            "{description}"
        );
        let expected_kinds: Vec<AttentionReasonKind> = expected_entries
            .iter()
            .filter_map(|(_, _, subject, _)| match subject {
                Some(NotificationSubject::Pane {
                    attention_reason_kind,
                    ..
                }) => *attention_reason_kind,
                _ => None,
            })
            .collect();
        assert_eq!(
            fixture.list_build_pane_notified_kinds(),
            expected_kinds,
            "{description}"
        );
    }
}

#[test]
fn a_finished_command_names_its_exit_code_and_its_whole_running_time() {
    for (exit_code, command_seconds, expected_severity, expected_reason_text) in [
        (
            Some(0),
            45,
            NotificationSeverity::Info,
            "finished after 45s",
        ),
        (
            None,
            3780,
            NotificationSeverity::Info,
            "finished after 1h 3m",
        ),
        (
            Some(101),
            134,
            NotificationSeverity::Warning,
            "failed — exit 101 after 2m 14s",
        ),
    ] {
        let mut fixture = build_attention_fixture();
        let build_pane_id = fixture.build_pane_id;

        apply_timed_command(&mut fixture, build_pane_id, exit_code, command_seconds);

        assert_eq!(
            fixture.list_session_entries(),
            [(
                expected_severity,
                format!(
                    "code › {} · {expected_reason_text}",
                    build_pane_id.format_short_label()
                ),
                build_pane_subject(build_pane_id, AttentionReasonKind::CommandFinished),
                1,
            )]
        );
    }
}

#[test]
fn an_entry_names_the_pane_by_its_border_title_and_by_its_short_label_when_the_title_is_empty() {
    let mut fixture = build_attention_fixture();
    let build_pane_id = fixture.build_pane_id;
    fixture.write_build_pane_output(b"\x1b]2;cargo test\x07", build_wall_time(10));

    apply_timed_command(&mut fixture, build_pane_id, Some(101), 134);

    assert_eq!(
        fixture.list_session_entries(),
        [(
            NotificationSeverity::Warning,
            "code › cargo test · failed — exit 101 after 2m 14s".to_string(),
            build_pane_subject(build_pane_id, AttentionReasonKind::CommandFinished),
            1,
        )]
    );

    let mut untitled_fixture = build_attention_fixture();
    untitled_fixture.write_build_pane_output(b"\x1b]2;\x07", build_wall_time(10));

    untitled_fixture
        .apply_build_pane_facts(vec![build_notify_fact(None, "Done")], build_wall_time(20));

    assert_eq!(
        untitled_fixture.list_session_entries(),
        [(
            NotificationSeverity::Info,
            format!(
                "code › {} · Done",
                untitled_fixture.build_pane_id.format_short_label()
            ),
            build_pane_subject(untitled_fixture.build_pane_id, AttentionReasonKind::Notify),
            1,
        )]
    );
}

#[test]
fn a_child_record_is_labelled_with_its_inherited_app_name_and_its_title() {
    let mut fixture = build_attention_fixture();
    let build_pane_id = fixture.build_pane_id;

    fixture.apply_build_pane_facts(
        vec![
            build_labelled_report_fact(
                "deploy",
                ProgramStatusState::Working,
                None,
                Some("deploy"),
                None,
                None,
            ),
            build_labelled_report_fact(
                "deploy/eu-west",
                ProgramStatusState::Blocked,
                Some(BlockedKind::Permission),
                None,
                Some("EU West"),
                Some("Approve deploy to eu-west (production)?"),
            ),
        ],
        build_wall_time(100),
    );

    assert_eq!(
        fixture.list_session_entries(),
        [(
            NotificationSeverity::Warning,
            format!(
                "code › {} · deploy / EU West: blocked (permission) — Approve deploy to eu-west (production)?",
                build_pane_id.format_short_label()
            ),
            build_pane_subject(build_pane_id, AttentionReasonKind::ProgramBlocked),
            1,
        )]
    );
}

#[test]
fn a_floating_pane_posts_its_entry_under_the_place_floating() {
    let mut fixture = build_attention_fixture();
    let in_session_source = build_in_session_source(
        fixture.session_id,
        Some(fixture.alice_client_id),
        fixture.docs_pane_id,
    );
    let floating_pane_id = create_floating_pane(
        &mut fixture.server,
        in_session_source,
        build_floating_new_pane_args(None, None, false),
    );
    let read_instant = Instant::now();

    fixture.server.apply_program_facts(
        floating_pane_id,
        build_timed_program_facts(
            vec![build_notify_fact(Some("Claude Code"), "Waiting for input")],
            read_instant,
        ),
        build_wall_time(100),
        read_instant,
    );

    assert_eq!(
        fixture.list_session_entries(),
        [(
            NotificationSeverity::Info,
            format!(
                "floating › {} · Claude Code: Waiting for input",
                floating_pane_id.format_short_label()
            ),
            build_pane_subject(floating_pane_id, AttentionReasonKind::Notify),
            1,
        )]
    );
}

#[test]
fn a_program_that_loops_through_blocked_with_no_visit_posts_one_entry() {
    let mut fixture = build_attention_fixture();
    let build_pane_id = fixture.build_pane_id;
    let blocked_report = build_report_fact("", ProgramStatusState::Blocked, Some("Which branch?"));
    let working_report = build_report_fact("", ProgramStatusState::Working, None);

    for loop_index in 0..1000_u64 {
        fixture.apply_build_pane_facts(
            vec![blocked_report.clone()],
            build_wall_time(2 * loop_index),
        );
        fixture.apply_build_pane_facts(
            vec![working_report.clone()],
            build_wall_time(2 * loop_index + 1),
        );
    }
    fixture.apply_build_pane_facts(vec![blocked_report], build_wall_time(5000));

    assert_eq!(
        fixture.list_session_entries(),
        [(
            NotificationSeverity::Warning,
            format!(
                "code › {} · blocked — Which branch?",
                build_pane_id.format_short_label()
            ),
            build_pane_subject(build_pane_id, AttentionReasonKind::ProgramBlocked),
            1,
        )]
    );
    assert_eq!(
        fixture.list_build_pane_notified_kinds(),
        [AttentionReasonKind::ProgramBlocked]
    );
}

#[test]
fn blocked_then_done_with_no_visit_posts_one_entry_for_each_kind() {
    let mut fixture = build_attention_fixture();
    let build_pane_id = fixture.build_pane_id;
    let pane_label = build_pane_id.format_short_label();

    fixture.apply_build_pane_facts(
        vec![build_report_fact(
            "",
            ProgramStatusState::Blocked,
            Some("Apply?"),
        )],
        build_wall_time(10),
    );
    fixture.apply_build_pane_facts(
        vec![build_report_fact(
            "",
            ProgramStatusState::Done,
            Some("Applied"),
        )],
        build_wall_time(20),
    );
    fixture.apply_build_pane_facts(vec![build_notify_fact(None, "first")], build_wall_time(30));
    fixture.apply_build_pane_facts(vec![build_notify_fact(None, "second")], build_wall_time(40));

    assert_eq!(
        fixture.list_session_entries(),
        [
            (
                NotificationSeverity::Warning,
                format!("code › {pane_label} · blocked — Apply?"),
                build_pane_subject(build_pane_id, AttentionReasonKind::ProgramBlocked),
                1,
            ),
            (
                NotificationSeverity::Info,
                format!("code › {pane_label} · done — Applied"),
                build_pane_subject(build_pane_id, AttentionReasonKind::ProgramDone),
                1,
            ),
            (
                NotificationSeverity::Info,
                format!("code › {pane_label} · first"),
                build_pane_subject(build_pane_id, AttentionReasonKind::Notify),
                1,
            ),
        ]
    );
    assert_eq!(
        fixture.list_build_pane_notified_kinds(),
        [
            AttentionReasonKind::ProgramBlocked,
            AttentionReasonKind::ProgramDone,
            AttentionReasonKind::Notify,
        ]
    );
}

#[test]
fn a_visit_between_two_blocked_reports_posts_the_second_one() {
    let mut fixture = build_attention_fixture();
    let build_pane_id = fixture.build_pane_id;
    let alice_client_id = fixture.alice_client_id;
    let blocked_text = format!(
        "code › {} · blocked — Which branch?",
        build_pane_id.format_short_label()
    );
    fixture.apply_build_pane_facts(
        vec![build_report_fact(
            "",
            ProgramStatusState::Blocked,
            Some("Which branch?"),
        )],
        build_wall_time(10),
    );

    fixture
        .server
        .clear_attended_pane_attention(alice_client_id, build_pane_id);

    assert_eq!(fixture.list_build_pane_notified_kinds(), []);
    fixture.apply_build_pane_facts(
        vec![build_report_fact("", ProgramStatusState::Working, None)],
        build_wall_time(20),
    );
    fixture.apply_build_pane_facts(
        vec![build_report_fact(
            "",
            ProgramStatusState::Blocked,
            Some("Which branch?"),
        )],
        build_wall_time(30),
    );
    assert_eq!(
        fixture.list_session_entries(),
        [(
            NotificationSeverity::Warning,
            blocked_text,
            build_pane_subject(build_pane_id, AttentionReasonKind::ProgramBlocked),
            2,
        )]
    );
    assert_eq!(
        fixture.list_build_pane_notified_kinds(),
        [AttentionReasonKind::ProgramBlocked]
    );
}

#[test]
fn a_visit_to_a_pane_whose_wait_the_program_withdrew_still_empties_its_notified_kinds() {
    let mut fixture = build_attention_fixture();
    let build_pane_id = fixture.build_pane_id;
    let alice_client_id = fixture.alice_client_id;
    let pane_label = build_pane_id.format_short_label();
    fixture.apply_build_pane_facts(
        vec![build_report_fact(
            "",
            ProgramStatusState::Blocked,
            Some("Apply?"),
        )],
        build_wall_time(10),
    );
    fixture.apply_build_pane_facts(
        vec![build_report_fact("", ProgramStatusState::Working, None)],
        build_wall_time(20),
    );
    assert_eq!(fixture.find_pane_attention(build_pane_id), None);
    assert_eq!(
        fixture.list_build_pane_notified_kinds(),
        [AttentionReasonKind::ProgramBlocked]
    );

    fixture
        .server
        .clear_attended_pane_attention(alice_client_id, build_pane_id);
    fixture.apply_build_pane_facts(
        vec![build_report_fact(
            "",
            ProgramStatusState::Blocked,
            Some("Overwrite?"),
        )],
        build_wall_time(30),
    );

    assert_eq!(
        fixture.list_session_entries(),
        [
            (
                NotificationSeverity::Warning,
                format!("code › {pane_label} · blocked — Apply?"),
                build_pane_subject(build_pane_id, AttentionReasonKind::ProgramBlocked),
                1,
            ),
            (
                NotificationSeverity::Warning,
                format!("code › {pane_label} · blocked — Overwrite?"),
                build_pane_subject(build_pane_id, AttentionReasonKind::ProgramBlocked),
                1,
            ),
        ]
    );
}

#[test]
fn a_read_that_raises_nothing_it_keeps_posts_no_entry() {
    let withdrawn_notification = vec![
        ProgramFact::Notify {
            title: None,
            body: "Build done".to_string(),
            notification_id: Some("7".to_string()),
        },
        ProgramFact::NotificationWithdrawn {
            notification_id: "7".to_string(),
        },
    ];
    let blocked_then_cleared = vec![
        build_report_fact("", ProgramStatusState::Blocked, Some("Apply?")),
        ProgramFact::ProgramStatus(ProgramStatusFact::Cleared {
            record_id: String::new(),
        }),
    ];
    for program_facts in [withdrawn_notification, blocked_then_cleared] {
        let mut fixture = build_attention_fixture();
        let description = format!("{program_facts:?}");

        let emitted_events = fixture.apply_build_pane_facts(program_facts, build_wall_time(10));

        assert_eq!(emitted_events, [], "{description}");
        assert_eq!(fixture.list_session_entries(), [], "{description}");
        assert_eq!(
            fixture.list_build_pane_notified_kinds(),
            [],
            "{description}"
        );
    }
}

#[test]
fn five_hundred_bells_and_notifications_in_one_read_post_one_entry() {
    let mut fixture = build_attention_fixture();
    let build_pane_id = fixture.build_pane_id;
    let mut program_facts = Vec::new();
    for _ in 0..500 {
        program_facts.push(ProgramFact::Bell);
        program_facts.push(build_notify_fact(None, "x"));
    }

    fixture.apply_build_pane_facts(program_facts, build_wall_time(10));

    assert_eq!(
        fixture.list_session_entries(),
        [(
            NotificationSeverity::Info,
            format!("code › {} · x", build_pane_id.format_short_label()),
            build_pane_subject(build_pane_id, AttentionReasonKind::Notify),
            1,
        )]
    );
}

#[test]
fn a_new_reason_of_a_kind_already_posted_since_the_last_visit_posts_no_entry() {
    let mut fixture = build_attention_fixture();
    let build_pane_id = fixture.build_pane_id;
    fixture.apply_build_pane_facts(vec![build_notify_fact(None, "first")], build_wall_time(10));

    let emitted_events = fixture
        .apply_build_pane_facts(vec![build_notify_fact(None, "second")], build_wall_time(20));

    assert_eq!(
        emitted_events,
        [Event::PaneAttentionRaised(PaneAttentionRaised {
            pane_id: build_pane_id,
            tab_id: Some(fixture.code_tab_id),
            reason: AttentionReason::Notify {
                title: None,
                body: "second".to_string(),
            },
            raised_at: build_wall_time(10),
        })]
    );
    assert_eq!(
        fixture.list_session_entries(),
        [(
            NotificationSeverity::Info,
            format!("code › {} · first", build_pane_id.format_short_label()),
            build_pane_subject(build_pane_id, AttentionReasonKind::Notify),
            1,
        )]
    );
}

#[test]
fn a_dropped_raise_posts_no_entry() {
    let mut watched_fixture = build_attention_fixture();
    watched_fixture.attach_bob_to_build_pane(TerminalFocus::Focused);
    watched_fixture.apply_build_pane_facts(vec![build_notify_fact(None, "x")], build_wall_time(10));
    assert_eq!(watched_fixture.list_session_entries(), []);

    let mut switched_off_fixture = build_attention_fixture();
    switch_off_attention_kind(
        &mut switched_off_fixture.server.config.attention,
        AttentionReasonKind::Notify,
    );
    switched_off_fixture
        .apply_build_pane_facts(vec![build_notify_fact(None, "x")], build_wall_time(10));
    assert_eq!(switched_off_fixture.list_session_entries(), []);
    assert_eq!(switched_off_fixture.list_build_pane_notified_kinds(), []);

    let mut short_command_fixture = build_attention_fixture();
    let build_pane_id = short_command_fixture.build_pane_id;
    apply_timed_command(&mut short_command_fixture, build_pane_id, Some(0), 4);
    assert_eq!(short_command_fixture.list_session_entries(), []);
}

#[test]
fn every_subscribed_client_receives_the_posted_entry() {
    let mut fixture = build_attention_fixture();
    let build_pane_id = fixture.build_pane_id;
    let bob_client_id = fixture.attach_bob_to_build_pane(TerminalFocus::Unfocused);
    let (bob_delivery_receiver, _) = fixture.server.subscribe(bob_client_id, 60, None);
    fixture.server.handle_attach_answer_received(bob_client_id);
    let _ = fixture.alice_delivery_receiver.try_iter().count();
    let _ = bob_delivery_receiver.try_iter().count();

    fixture.apply_build_pane_facts(vec![build_notify_fact(None, "x")], build_wall_time(10));

    let expected_posted_entries = [(
        1,
        build_pane_subject(build_pane_id, AttentionReasonKind::Notify),
        format!("code › {} · x", build_pane_id.format_short_label()),
    )];
    for (client_name, delivery_receiver) in [
        ("alice", &fixture.alice_delivery_receiver),
        ("bob", &bob_delivery_receiver),
    ] {
        let posted_entries: Vec<(u64, Option<NotificationSubject>, String)> = delivery_receiver
            .try_iter()
            .filter_map(|delivery| match delivery {
                Delivery::NotificationPosted {
                    revision,
                    posted_entry,
                } => Some((revision, posted_entry.subject, posted_entry.text)),
                _ => None,
            })
            .collect();
        assert_eq!(posted_entries, expected_posted_entries, "{client_name}");
    }
}
