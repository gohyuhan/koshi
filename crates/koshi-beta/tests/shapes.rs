//! The attribute against every function shape a real entry point can have:
//! methods with a receiver, trait implementations, generics, lifetimes,
//! `async`, a unit return, a body whose tail is a block, and a body with an
//! early `return` and a `?`. Also where an `async fn` reads the gate: at its
//! first poll, not at the call.
//!
//! The gate is one process-wide flag. Every case lives in one test and runs in
//! sequence on that flag.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use koshi_beta::beta_feature;

/// Polls `future` once and returns its value. Panics if the first poll is
/// `Pending`.
fn poll_future_once<FutureType: Future>(future: FutureType) -> FutureType::Output {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(future_output) => future_output,
        Poll::Pending => panic!("this future awaits nothing and must finish on the first poll"),
    }
}

struct AttachmentServer {
    attachment_count: u32,
}

impl AttachmentServer {
    #[beta_feature(otherwise = Err("off"))]
    fn increment_attachment_count(
        &mut self,
        attachment_count_delta: u32,
    ) -> Result<u32, &'static str> {
        self.attachment_count += attachment_count_delta;
        Ok(self.attachment_count)
    }

    #[beta_feature(otherwise = 0)]
    fn get_attachment_count(&self) -> u32 {
        self.attachment_count
    }
}

trait AttachmentProbe {
    fn compute_attachment_count(&self) -> u32;
}

impl AttachmentProbe for AttachmentServer {
    #[beta_feature(otherwise = 0)]
    fn compute_attachment_count(&self) -> u32 {
        self.attachment_count + 1
    }
}

#[beta_feature(otherwise = None)]
fn clone_cloneable<Cloneable: Clone>(cloneable_reference: &Cloneable) -> Option<Cloneable> {
    Some(cloneable_reference.clone())
}

#[beta_feature(otherwise = "")]
fn get_first_word<'a>(source_text: &'a str) -> &'a str {
    source_text.split(' ').next().unwrap_or("")
}

#[beta_feature(otherwise = Err("off"))]
async fn run_asynchronous_task(input_number: u32) -> Result<u32, &'static str> {
    Ok(input_number)
}

#[beta_feature(otherwise = ())]
fn set_slot_number(slot_number: &mut u32) {
    *slot_number = 9;
}

#[beta_feature(otherwise = 0)]
fn compute_branch_number(is_first_branch: bool) -> u32 {
    if is_first_branch {
        1
    } else {
        2
    }
}

#[beta_feature(otherwise = Err("off"))]
fn return_early_error(is_error_branch: bool) -> Result<u32, &'static str> {
    if is_error_branch {
        return Err("early");
    }
    let computed_number = Ok::<u32, &'static str>(3)?;
    Ok(computed_number)
}

#[test]
fn beta_feature_supports_function_shapes_and_gates_results() {
    let mut attachment_server = AttachmentServer {
        attachment_count: 0,
    };
    let mut slot_number = 0;

    koshi_beta::set_beta_features_allowed(false);
    assert_eq!(attachment_server.increment_attachment_count(5), Err("off"));
    assert_eq!(attachment_server.get_attachment_count(), 0);
    assert_eq!(
        AttachmentProbe::compute_attachment_count(&attachment_server),
        0
    );
    assert_eq!(clone_cloneable(&7u32), None);
    assert_eq!(clone_cloneable(&String::from("seven")), None);
    assert_eq!(get_first_word("one two"), "");
    set_slot_number(&mut slot_number);
    assert_eq!(slot_number, 0);
    assert_eq!(compute_branch_number(true), 0);
    assert_eq!(compute_branch_number(false), 0);
    assert_eq!(return_early_error(true), Err("off"));
    assert_eq!(return_early_error(false), Err("off"));
    assert_eq!(poll_future_once(run_asynchronous_task(1)), Err("off"));
    assert_eq!(attachment_server.attachment_count, 0);

    koshi_beta::set_beta_features_allowed(true);
    assert_eq!(attachment_server.increment_attachment_count(5), Ok(5));
    assert_eq!(attachment_server.get_attachment_count(), 5);
    assert_eq!(
        AttachmentProbe::compute_attachment_count(&attachment_server),
        6
    );
    assert_eq!(clone_cloneable(&7u32), Some(7));
    assert_eq!(
        clone_cloneable(&String::from("seven")),
        Some(String::from("seven"))
    );
    assert_eq!(get_first_word("one two"), "one");
    set_slot_number(&mut slot_number);
    assert_eq!(slot_number, 9);
    assert_eq!(compute_branch_number(true), 1);
    assert_eq!(compute_branch_number(false), 2);
    assert_eq!(return_early_error(true), Err("early"));
    assert_eq!(return_early_error(false), Ok(3));
    assert_eq!(poll_future_once(run_asynchronous_task(1)), Ok(1));

    // The `&self` method re-checked where the two answers differ:
    // `attachment_count` is
    // 5 here, so it answers 0 only while it is blocked.
    koshi_beta::set_beta_features_allowed(false);
    assert_eq!(attachment_server.get_attachment_count(), 0);

    // An `async fn` reads the gate at its first poll. The answer follows the
    // flag at the poll, not at the call that built the future.
    let built_while_off = run_asynchronous_task(1);
    koshi_beta::set_beta_features_allowed(true);
    assert_eq!(poll_future_once(built_while_off), Ok(1));

    let built_while_on = run_asynchronous_task(1);
    koshi_beta::set_beta_features_allowed(false);
    assert_eq!(poll_future_once(built_while_on), Err("off"));
}
