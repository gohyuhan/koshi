//! The attribute against every function shape a real entry point can have:
//! methods with a receiver, trait implementations, generics, `async`, a unit
//! return, and a body whose tail is a block. Also when an `async fn` reads the
//! gate, which is at its first poll rather than at the call.
//!
//! The gate is one process-wide flag, so every case lives in one test:
//! separate tests would run in parallel and race each other over it.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use koshi_beta::beta_feature;

/// Drives a future that never awaits anything to its value. Koshi runs no
/// async executor, so one poll is the whole story.
fn now<F: Future>(future: F) -> F::Output {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("this future awaits nothing and must finish on the first poll"),
    }
}

struct Server {
    count: u32,
}

impl Server {
    #[beta_feature(otherwise = Err("off"))]
    fn attach(&mut self, id: u32) -> Result<u32, &'static str> {
        self.count += id;
        Ok(self.count)
    }

    #[beta_feature(otherwise = 0)]
    fn peek(&self) -> u32 {
        self.count
    }
}

trait Attachable {
    fn go(&self) -> u32;
}

impl Attachable for Server {
    #[beta_feature(otherwise = 0)]
    fn go(&self) -> u32 {
        self.count + 1
    }
}

#[beta_feature(otherwise = None)]
fn generic<T: Clone>(value: &T) -> Option<T> {
    Some(value.clone())
}

#[beta_feature(otherwise = Err("off"))]
async fn asynchronous(value: u32) -> Result<u32, &'static str> {
    Ok(value)
}

#[beta_feature(otherwise = ())]
fn returns_unit(slot: &mut u32) {
    *slot = 9;
}

#[beta_feature(otherwise = 0)]
fn tail_is_a_block(flag: bool) -> u32 {
    if flag {
        1
    } else {
        2
    }
}

#[test]
fn every_shape_compiles_and_gates() {
    let mut server = Server { count: 0 };
    let mut slot = 0;

    koshi_beta::set_allowed(false);
    assert_eq!(server.attach(5), Err("off"));
    assert_eq!(server.peek(), 0);
    assert_eq!(Attachable::go(&server), 0);
    assert_eq!(generic(&7u32), None);
    returns_unit(&mut slot);
    assert_eq!(slot, 0);
    assert_eq!(tail_is_a_block(true), 0);
    assert_eq!(now(asynchronous(1)), Err("off"));
    assert_eq!(server.count, 0);

    koshi_beta::set_allowed(true);
    assert_eq!(server.attach(5), Ok(5));
    assert_eq!(server.peek(), 5);
    assert_eq!(Attachable::go(&server), 6);
    assert_eq!(generic(&7u32), Some(7));
    returns_unit(&mut slot);
    assert_eq!(slot, 9);
    assert_eq!(tail_is_a_block(true), 1);
    assert_eq!(now(asynchronous(1)), Ok(1));

    // The `&self` method re-checked where the two answers differ: `count` is
    // 5 here, so it answers 0 only while it is blocked.
    koshi_beta::set_allowed(false);
    assert_eq!(server.peek(), 0);

    // An `async fn` reads the gate where its body starts, and that is the first
    // poll: the answer belongs to the poll, not to the call that built it.
    let built_while_off = asynchronous(1);
    koshi_beta::set_allowed(true);
    assert_eq!(now(built_while_off), Ok(1));

    let built_while_on = asynchronous(1);
    koshi_beta::set_allowed(false);
    assert_eq!(now(built_while_on), Err("off"));
}
