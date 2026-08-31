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

#[beta_feature(otherwise = "")]
fn first_word<'a>(text: &'a str) -> &'a str {
    text.split(' ').next().unwrap_or("")
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

#[beta_feature(otherwise = Err("off"))]
fn early_return(flag: bool) -> Result<u32, &'static str> {
    if flag {
        return Err("early");
    }
    let value = Ok::<u32, &'static str>(3)?;
    Ok(value)
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
    assert_eq!(generic(&String::from("seven")), None);
    assert_eq!(first_word("one two"), "");
    returns_unit(&mut slot);
    assert_eq!(slot, 0);
    assert_eq!(tail_is_a_block(true), 0);
    assert_eq!(tail_is_a_block(false), 0);
    assert_eq!(early_return(true), Err("off"));
    assert_eq!(early_return(false), Err("off"));
    assert_eq!(now(asynchronous(1)), Err("off"));
    assert_eq!(server.count, 0);

    koshi_beta::set_allowed(true);
    assert_eq!(server.attach(5), Ok(5));
    assert_eq!(server.peek(), 5);
    assert_eq!(Attachable::go(&server), 6);
    assert_eq!(generic(&7u32), Some(7));
    assert_eq!(generic(&String::from("seven")), Some(String::from("seven")));
    assert_eq!(first_word("one two"), "one");
    returns_unit(&mut slot);
    assert_eq!(slot, 9);
    assert_eq!(tail_is_a_block(true), 1);
    assert_eq!(tail_is_a_block(false), 2);
    assert_eq!(early_return(true), Err("early"));
    assert_eq!(early_return(false), Ok(3));
    assert_eq!(now(asynchronous(1)), Ok(1));

    // The `&self` method re-checked where the two answers differ: `count` is
    // 5 here, so it answers 0 only while it is blocked.
    koshi_beta::set_allowed(false);
    assert_eq!(server.peek(), 0);

    // An `async fn` reads the gate at its first poll. The answer follows the
    // flag at the poll, not at the call that built the future.
    let built_while_off = asynchronous(1);
    koshi_beta::set_allowed(true);
    assert_eq!(now(built_while_off), Ok(1));

    let built_while_on = asynchronous(1);
    koshi_beta::set_allowed(false);
    assert_eq!(now(built_while_on), Err("off"));
}
