//! Tests for the viewer action requests `koshi actions run` sends: the checks
//! that refuse a request before it reaches a client, the client's answer,
//! and every answer the session gives for a client that did not answer.

use super::*;

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::SystemTime;

use koshi_core::geometry::Size;
use koshi_renderer::snapshot::Delivery;

use crate::runtime::bus::tests::build_test_placement_event;
use crate::runtime::bus::SUBSCRIBER_QUEUE_CAPACITY;
use crate::runtime::command::tests::{
    attach_second_client, build_in_session_source, build_runtime_with_fake,
    create_session_with_attached_client, get_only_session_slot,
};
use crate::runtime::delivery_queue::DeliveryReceiver;

/// A runtime holding one session with one attached local client and one
/// pane, and the client's id.
fn build_runtime_with_one_client() -> (Server, ClientId) {
    let (mut runtime, _fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let client_id = create_session_with_attached_client(
        &mut runtime,
        SessionId::new(),
        Size {
            column_count: 80,
            row_count: 24,
        },
        SystemTime::UNIX_EPOCH,
    )
    .expect("bootstrap the session");
    (runtime, client_id)
}

/// `core:toggle-notification-center`.
fn build_toggle_notification_center_reference() -> ActionReference {
    ActionReference::from_core_action_name("toggle-notification-center").expect("valid name")
}

/// Ask `runtime` to have a client run `core:toggle-notification-center` for
/// an external CLI naming `client_id` in `session_id`. Returns the request's
/// command id and the receiver of its answer.
fn request_external_client_action(
    runtime: &mut Server,
    session_id: SessionId,
    client_id: Option<ClientId>,
) -> (CommandId, Receiver<IpcResult>) {
    let command_id = CommandId::new();
    let (response_sender, response_receiver) = mpsc::channel();
    runtime.request_client_action(
        command_id,
        &CommandSource::from_external_cli(Some(session_id), client_id),
        build_toggle_notification_center_reference(),
        response_sender,
    );
    (command_id, response_receiver)
}

/// The rejection `command_id` gets with `reason` and `help`.
fn build_rejected_answer(command_id: CommandId, reason: RejectReason, help: &str) -> IpcResult {
    IpcResult::CommandResult(CommandResult::Rejected {
        command_id,
        reason,
        help: Some(help.to_string()),
    })
}

/// The `Ok` answer of `command_id`.
fn build_ok_answer(command_id: CommandId) -> IpcResult {
    IpcResult::CommandResult(CommandResult::Ok {
        command_id,
        emitted_events: Vec::new(),
    })
}

/// The request number of the one viewer action request on
/// `delivery_receiver`, which holds nothing after it.
fn take_client_action_request_id(delivery_receiver: &DeliveryReceiver) -> u64 {
    let Ok(Delivery::ClientActionRequested {
        client_action_request_id,
        action_reference,
    }) = delivery_receiver.try_recv()
    else {
        panic!("the queue holds a viewer action request");
    };
    assert_eq!(
        action_reference,
        build_toggle_notification_center_reference()
    );
    assert!(delivery_receiver.try_recv().is_err(), "nothing follows it");
    client_action_request_id
}

#[test]
fn a_request_reaches_the_clients_queue_and_its_answer_reaches_the_caller() {
    let (mut runtime, client_id) = build_runtime_with_one_client();
    let (session_id, _, _) = get_only_session_slot(&runtime);
    let (delivery_receiver, _) = runtime.subscribe(client_id, 60);

    let (command_id, response_receiver) =
        request_external_client_action(&mut runtime, session_id, Some(client_id));

    assert_eq!(take_client_action_request_id(&delivery_receiver), 0);
    assert_eq!(response_receiver.try_recv(), Err(TryRecvError::Empty));
    runtime.answer_client_action(client_id, 0, None);
    assert_eq!(
        response_receiver.try_recv(),
        Ok(build_ok_answer(command_id))
    );
    assert_eq!(
        runtime
            .waiting_client_actions
            .find_earliest_answer_deadline(),
        None
    );
}

#[test]
fn a_client_refusal_reaches_the_caller_with_its_reason_and_help() {
    let (mut runtime, client_id) = build_runtime_with_one_client();
    let (session_id, _, _) = get_only_session_slot(&runtime);
    let (delivery_receiver, _) = runtime.subscribe(client_id, 60);
    let (command_id, response_receiver) =
        request_external_client_action(&mut runtime, session_id, None);
    let client_action_request_id = take_client_action_request_id(&delivery_receiver);

    runtime.answer_client_action(
        client_id,
        client_action_request_id,
        Some(ClientActionRefusal {
            reject_reason: RejectReason::InvalidState,
            help: "core:select-pane-target-left runs only in pane placement mode".to_string(),
        }),
    );

    assert_eq!(
        response_receiver.try_recv(),
        Ok(build_rejected_answer(
            command_id,
            RejectReason::InvalidState,
            "core:select-pane-target-left runs only in pane placement mode",
        ))
    );
}

#[test]
fn an_answer_from_another_client_or_after_the_first_answer_is_dropped() {
    let (mut runtime, client_id) = build_runtime_with_one_client();
    let (session_id, _, _) = get_only_session_slot(&runtime);
    let other_client_id = attach_second_client(&mut runtime, client_id, None);
    let (delivery_receiver, _) = runtime.subscribe(client_id, 60);
    let (command_id, response_receiver) =
        request_external_client_action(&mut runtime, session_id, Some(client_id));
    let client_action_request_id = take_client_action_request_id(&delivery_receiver);

    runtime.answer_client_action(other_client_id, client_action_request_id, None);
    assert_eq!(response_receiver.try_recv(), Err(TryRecvError::Empty));

    runtime.answer_client_action(client_id, client_action_request_id, None);
    assert_eq!(
        response_receiver.try_recv(),
        Ok(build_ok_answer(command_id))
    );

    runtime.answer_client_action(
        client_id,
        client_action_request_id,
        Some(ClientActionRefusal {
            reject_reason: RejectReason::InvalidState,
            help: "a second answer".to_string(),
        }),
    );
    assert_eq!(
        response_receiver.try_recv(),
        Err(TryRecvError::Disconnected)
    );
}

#[test]
fn requests_carry_rising_numbers_and_the_first_one_has_the_earliest_deadline() {
    let (mut runtime, client_id) = build_runtime_with_one_client();
    let (session_id, _, _) = get_only_session_slot(&runtime);
    let (delivery_receiver, _) = runtime.subscribe(client_id, 60);

    let earliest_request_time = Instant::now();
    let _first_request = request_external_client_action(&mut runtime, session_id, None);
    let latest_request_time = Instant::now();
    let _second_request = request_external_client_action(&mut runtime, session_id, None);

    let mut client_action_request_ids = Vec::new();
    while let Ok(Delivery::ClientActionRequested {
        client_action_request_id,
        ..
    }) = delivery_receiver.try_recv()
    {
        client_action_request_ids.push(client_action_request_id);
    }
    assert_eq!(client_action_request_ids, vec![0, 1]);
    let earliest_answer_deadline = runtime
        .waiting_client_actions
        .find_earliest_answer_deadline()
        .expect("two requests wait");
    assert!(
        earliest_answer_deadline >= earliest_request_time + CLIENT_ACTION_ANSWER_TIMEOUT_DURATION
            && earliest_answer_deadline
                <= latest_request_time + CLIENT_ACTION_ANSWER_TIMEOUT_DURATION,
        "the first request's deadline is 5 s after it"
    );
}

#[test]
fn a_client_not_attached_to_the_session_is_refused_by_name() {
    let (mut runtime, client_id) = build_runtime_with_one_client();
    let (session_id, _, _) = get_only_session_slot(&runtime);
    let (delivery_receiver, _) = runtime.subscribe(client_id, 60);
    let session_name = runtime.session_by_id[&session_id].session_name.clone();
    let absent_client_id = ClientId::new();

    let (command_id, response_receiver) =
        request_external_client_action(&mut runtime, session_id, Some(absent_client_id));

    assert_eq!(
        response_receiver.try_recv(),
        Ok(build_rejected_answer(
            command_id,
            RejectReason::TargetNotFound,
            &format!("client {absent_client_id} is not attached to session `{session_name}`"),
        ))
    );
    assert!(delivery_receiver.try_recv().is_err(), "no client is asked");
}

#[test]
fn several_clients_and_no_client_named_is_refused_as_ambiguous() {
    let (mut runtime, client_id) = build_runtime_with_one_client();
    let (session_id, _, _) = get_only_session_slot(&runtime);
    attach_second_client(&mut runtime, client_id, None);

    let (command_id, response_receiver) =
        request_external_client_action(&mut runtime, session_id, None);

    assert_eq!(
        response_receiver.try_recv(),
        Ok(build_rejected_answer(
            command_id,
            RejectReason::TargetAmbiguous,
            "several clients are attached; name the target client",
        ))
    );
}

#[test]
fn a_client_awaiting_reconnect_is_refused_as_reconnecting() {
    let (mut runtime, client_id) = build_runtime_with_one_client();
    let (session_id, _, _) = get_only_session_slot(&runtime);
    runtime.client_ids_awaiting_reconnect.insert(client_id);

    let (command_id, response_receiver) =
        request_external_client_action(&mut runtime, session_id, Some(client_id));

    assert_eq!(
        response_receiver.try_recv(),
        Ok(build_rejected_answer(
            command_id,
            RejectReason::InvalidState,
            &format!("client {client_id} is reconnecting"),
        ))
    );
}

#[test]
fn a_client_with_no_subscriber_is_refused_as_not_reading() {
    let (mut runtime, client_id) = build_runtime_with_one_client();
    let (session_id, _, _) = get_only_session_slot(&runtime);

    let (command_id, response_receiver) =
        request_external_client_action(&mut runtime, session_id, Some(client_id));

    assert_eq!(
        response_receiver.try_recv(),
        Ok(build_rejected_answer(
            command_id,
            RejectReason::InvalidState,
            &format!("client {client_id} is not reading the session; the action did not run"),
        ))
    );
    assert_eq!(
        runtime
            .waiting_client_actions
            .find_earliest_answer_deadline(),
        None
    );
}

#[test]
fn a_client_whose_queue_is_full_is_refused_as_not_reading() {
    let (mut runtime, client_id) = build_runtime_with_one_client();
    let (session_id, tab_id, _) = get_only_session_slot(&runtime);
    let (_delivery_receiver, _) = runtime.subscribe(client_id, 60);
    let queued_events: Vec<Event> = (0..SUBSCRIBER_QUEUE_CAPACITY)
        .map(|_| build_test_placement_event(tab_id))
        .collect();
    runtime.publish_events(&queued_events);

    let (command_id, response_receiver) =
        request_external_client_action(&mut runtime, session_id, Some(client_id));

    assert_eq!(
        response_receiver.try_recv(),
        Ok(build_rejected_answer(
            command_id,
            RejectReason::InvalidState,
            &format!("client {client_id} is not reading the session; the action did not run"),
        ))
    );
    assert_eq!(
        runtime
            .waiting_client_actions
            .find_earliest_answer_deadline(),
        None
    );
}

#[test]
fn a_request_from_a_pane_that_is_gone_is_refused() {
    let (mut runtime, client_id) = build_runtime_with_one_client();
    let (session_id, _, _) = get_only_session_slot(&runtime);
    let (response_sender, response_receiver) = mpsc::channel();
    let command_id = CommandId::new();

    runtime.request_client_action(
        command_id,
        &build_in_session_source(session_id, Some(client_id), PaneId::new()),
        build_toggle_notification_center_reference(),
        response_sender,
    );

    assert_eq!(
        response_receiver.try_recv(),
        Ok(build_rejected_answer(
            command_id,
            RejectReason::TargetGone,
            "source pane no longer exists",
        ))
    );
}

#[test]
fn a_request_from_a_pane_reaches_the_client_that_pane_names() {
    let (mut runtime, client_id) = build_runtime_with_one_client();
    let (session_id, _, pane_id) = get_only_session_slot(&runtime);
    let other_client_id = attach_second_client(&mut runtime, client_id, None);
    let (delivery_receiver, _) = runtime.subscribe(client_id, 60);
    let (other_delivery_receiver, _) = runtime.subscribe(other_client_id, 60);
    let (response_sender, response_receiver) = mpsc::channel();
    let command_id = CommandId::new();

    runtime.request_client_action(
        command_id,
        &build_in_session_source(session_id, Some(client_id), pane_id),
        build_toggle_notification_center_reference(),
        response_sender,
    );

    let client_action_request_id = take_client_action_request_id(&delivery_receiver);
    assert!(
        other_delivery_receiver.try_recv().is_err(),
        "only one client is asked"
    );
    runtime.answer_client_action(client_id, client_action_request_id, None);
    assert_eq!(
        response_receiver.try_recv(),
        Ok(build_ok_answer(command_id))
    );
}

#[test]
fn a_client_that_leaves_before_it_answers_is_answered_for() {
    let (mut runtime, client_id) = build_runtime_with_one_client();
    let (session_id, _, _) = get_only_session_slot(&runtime);
    let other_client_id = attach_second_client(&mut runtime, client_id, None);
    let (_delivery_receiver, _) = runtime.subscribe(client_id, 60);
    let (_other_delivery_receiver, _) = runtime.subscribe(other_client_id, 60);
    let (command_id, response_receiver) =
        request_external_client_action(&mut runtime, session_id, Some(client_id));
    let (other_command_id, other_response_receiver) =
        request_external_client_action(&mut runtime, session_id, Some(other_client_id));

    runtime.handle_client_detach(client_id);

    assert_eq!(
        response_receiver.try_recv(),
        Ok(build_rejected_answer(
            command_id,
            RejectReason::TargetGone,
            &format!("client {client_id} left the session before it answered"),
        ))
    );
    assert_eq!(other_response_receiver.try_recv(), Err(TryRecvError::Empty));
    runtime.answer_client_action(other_client_id, 1, None);
    assert_eq!(
        other_response_receiver.try_recv(),
        Ok(build_ok_answer(other_command_id))
    );
}

#[test]
fn a_request_past_its_deadline_is_answered_for_and_a_late_answer_is_dropped() {
    let (mut runtime, client_id) = build_runtime_with_one_client();
    let (session_id, _, _) = get_only_session_slot(&runtime);
    let (delivery_receiver, _) = runtime.subscribe(client_id, 60);
    let (command_id, response_receiver) =
        request_external_client_action(&mut runtime, session_id, Some(client_id));
    let client_action_request_id = take_client_action_request_id(&delivery_receiver);
    let answer_deadline = runtime
        .waiting_client_actions
        .find_earliest_answer_deadline()
        .expect("the request waits");

    runtime.answer_overdue_client_actions(answer_deadline - Duration::from_millis(1));
    assert_eq!(response_receiver.try_recv(), Err(TryRecvError::Empty));

    runtime.answer_overdue_client_actions(answer_deadline);
    assert_eq!(
        response_receiver.try_recv(),
        Ok(build_rejected_answer(
            command_id,
            RejectReason::InvalidState,
            &format!(
                "client {client_id} did not answer within 5 s; the action runs if that client \
                 reads the request"
            ),
        ))
    );

    runtime.answer_client_action(client_id, client_action_request_id, None);
    assert_eq!(
        response_receiver.try_recv(),
        Err(TryRecvError::Disconnected)
    );
}

#[test]
fn every_waiting_request_is_answered_when_the_session_ends() {
    let (mut runtime, client_id) = build_runtime_with_one_client();
    let (session_id, _, _) = get_only_session_slot(&runtime);
    let (_delivery_receiver, _) = runtime.subscribe(client_id, 60);
    let (command_id, response_receiver) =
        request_external_client_action(&mut runtime, session_id, Some(client_id));

    runtime.answer_client_actions_at_session_ending(SessionEnding::Quit);

    assert_eq!(
        response_receiver.try_recv(),
        Ok(build_rejected_answer(
            command_id,
            RejectReason::InvalidState,
            &format!(
                "the session began to stop before client {client_id} answered; the action ran \
                 if that client read the request"
            ),
        ))
    );
}

#[test]
fn every_waiting_request_is_answered_when_the_restart_carries_the_session_out() {
    let (mut runtime, client_id) = build_runtime_with_one_client();
    let (session_id, _, _) = get_only_session_slot(&runtime);
    let (_delivery_receiver, _) = runtime.subscribe(client_id, 60);
    let (command_id, response_receiver) =
        request_external_client_action(&mut runtime, session_id, Some(client_id));

    runtime.answer_client_actions_at_session_ending(SessionEnding::Restarting);

    assert_eq!(
        response_receiver.try_recv(),
        Ok(build_rejected_answer(
            command_id,
            RejectReason::InvalidState,
            &format!(
                "the session began to restart before client {client_id} answered; the action \
                 ran if that client read the request"
            ),
        ))
    );
}

#[test]
fn a_request_after_the_session_told_its_clients_it_restarts_is_refused_with_restarting() {
    let (mut runtime, client_id) = build_runtime_with_one_client();
    let (session_id, _, _) = get_only_session_slot(&runtime);
    let (delivery_receiver, _) = runtime.subscribe(client_id, 60);
    runtime.announce_restarting();
    while delivery_receiver.try_recv().is_ok() {}

    let (_, response_receiver) =
        request_external_client_action(&mut runtime, session_id, Some(client_id));

    assert_eq!(
        response_receiver.try_recv(),
        Ok(IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::Restarting,
            message: "the session is restarting; ask again in a moment".to_string(),
        }))
    );
    assert!(delivery_receiver.try_recv().is_err(), "no client is asked");
}

#[test]
fn a_request_waiting_when_the_session_tells_its_clients_it_restarts_keeps_waiting() {
    let (mut runtime, client_id) = build_runtime_with_one_client();
    let (session_id, _, _) = get_only_session_slot(&runtime);
    let (delivery_receiver, _) = runtime.subscribe(client_id, 60);
    let (command_id, response_receiver) =
        request_external_client_action(&mut runtime, session_id, Some(client_id));

    runtime.announce_restarting();

    assert_eq!(response_receiver.try_recv(), Err(TryRecvError::Empty));
    let client_action_request_id =
        take_client_action_request_id_before_restarting(&delivery_receiver);
    runtime.answer_client_action(client_id, client_action_request_id, None);
    assert_eq!(
        response_receiver.try_recv(),
        Ok(build_ok_answer(command_id))
    );
}

/// The request number of the viewer action request on `delivery_receiver`,
/// which the `Restarting` frame follows.
fn take_client_action_request_id_before_restarting(delivery_receiver: &DeliveryReceiver) -> u64 {
    let Ok(Delivery::ClientActionRequested {
        client_action_request_id,
        ..
    }) = delivery_receiver.try_recv()
    else {
        panic!("the request comes first");
    };
    assert_eq!(delivery_receiver.try_recv(), Ok(Delivery::Restarting));
    client_action_request_id
}

#[test]
fn a_request_after_the_session_told_its_clients_it_ended_is_refused_as_stopping() {
    let (mut runtime, client_id) = build_runtime_with_one_client();
    let (session_id, _, _) = get_only_session_slot(&runtime);
    let (_delivery_receiver, _) = runtime.subscribe(client_id, 60);
    runtime.announce_quit();

    let (command_id, response_receiver) =
        request_external_client_action(&mut runtime, session_id, Some(client_id));

    assert_eq!(
        response_receiver.try_recv(),
        Ok(build_rejected_answer(
            command_id,
            RejectReason::InvalidState,
            "session is stopping",
        ))
    );
}

#[test]
fn a_dispatch_pass_with_nothing_to_do_wakes_at_the_answer_deadline_and_answers() {
    let (mut runtime, client_id) = build_runtime_with_one_client();
    let (session_id, _, _) = get_only_session_slot(&runtime);
    let (_delivery_receiver, _) = runtime.subscribe(client_id, 60);
    let (command_id, response_receiver) =
        request_external_client_action(&mut runtime, session_id, Some(client_id));
    let answer_deadline = Instant::now() + Duration::from_millis(50);
    for waiting_client_action in runtime
        .waiting_client_actions
        .waiting_client_action_by_request_id
        .values_mut()
    {
        waiting_client_action.answer_deadline = answer_deadline;
    }

    let caller_answer = loop {
        runtime.run_dispatch_pass();
        if let Ok(caller_answer) = response_receiver.try_recv() {
            break caller_answer;
        }
    };

    assert!(Instant::now() >= answer_deadline);
    assert_eq!(
        caller_answer,
        build_rejected_answer(
            command_id,
            RejectReason::InvalidState,
            &format!(
                "client {client_id} did not answer within 5 s; the action runs if that client \
                 reads the request"
            ),
        )
    );
}
