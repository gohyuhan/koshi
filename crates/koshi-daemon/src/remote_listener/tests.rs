//! Tests for the remote listener's connection-attempt table: how many
//! connections one address may open inside a window, that an address crossing
//! its limit is logged once rather than once per attempt, that a window ends
//! on its own, and that the table cannot grow past the count it is bounded at.

use std::net::{IpAddr, Ipv4Addr};

use super::*;

/// The address `10.0.0.<last>`, for naming distinct callers in a test.
fn caller(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
}

/// A moment `seconds` after `start`.
fn later(start: Instant, seconds: u64) -> Instant {
    start + Duration::from_secs(seconds)
}

#[test]
fn an_address_inside_its_limit_is_served_every_time() {
    let mut table = RateTable::new();
    let now = Instant::now();

    for attempt in 1..=MAX_ATTEMPTS {
        assert!(
            matches!(table.allow(caller(1), now), Attempt::Serve),
            "attempt {attempt} of {MAX_ATTEMPTS} is inside the limit"
        );
    }
}

#[test]
fn crossing_the_limit_is_logged_once_and_then_dropped_in_silence() {
    // The whole point of the table: an address hammering the port writes one
    // line into the log, not one line per attempt.
    let mut table = RateTable::new();
    let now = Instant::now();

    for _ in 1..=MAX_ATTEMPTS {
        assert!(matches!(table.allow(caller(1), now), Attempt::Serve));
    }

    assert!(
        matches!(table.allow(caller(1), now), Attempt::DropAndSay),
        "the attempt that crosses the limit is the one that says so"
    );
    for attempt in 0..50 {
        assert!(
            matches!(table.allow(caller(1), now), Attempt::DropInSilence),
            "attempt {attempt} past the limit is dropped without a log line"
        );
    }
}

#[test]
fn a_window_that_has_passed_lets_the_same_address_back_in() {
    let mut table = RateTable::new();
    let opened = Instant::now();

    for _ in 1..=MAX_ATTEMPTS {
        assert!(matches!(table.allow(caller(1), opened), Attempt::Serve));
    }
    assert!(matches!(
        table.allow(caller(1), opened),
        Attempt::DropAndSay
    ));

    // One second before the window ends the address is still shut out.
    let nearly = later(opened, RATE_WINDOW.as_secs() - 1);
    assert!(matches!(
        table.allow(caller(1), nearly),
        Attempt::DropInSilence
    ));

    // Once the window has passed the address starts a fresh one.
    let after = later(opened, RATE_WINDOW.as_secs() + 1);
    assert!(matches!(table.allow(caller(1), after), Attempt::Serve));
}

#[test]
fn each_address_is_counted_on_its_own() {
    let mut table = RateTable::new();
    let now = Instant::now();

    for _ in 1..=MAX_ATTEMPTS {
        assert!(matches!(table.allow(caller(1), now), Attempt::Serve));
    }
    assert!(matches!(table.allow(caller(1), now), Attempt::DropAndSay));

    // A second address has opened nothing, so its first attempt is served.
    assert!(matches!(table.allow(caller(2), now), Attempt::Serve));
}

#[test]
fn the_table_never_holds_more_addresses_than_it_is_bounded_at() {
    // Every address gets one attempt, so nothing here is over its limit and
    // only the bound can be what keeps the table down.
    let mut table = RateTable::new();
    let now = Instant::now();

    for step in 0..u32::try_from(MAX_ENTRIES).expect("the bound fits") + 500 {
        let address = IpAddr::V4(Ipv4Addr::from(step));
        table.allow(address, now);
        assert!(
            table.entries.len() <= MAX_ENTRIES,
            "the table holds {} addresses after {step} of them, past the {MAX_ENTRIES} bound",
            table.entries.len()
        );
    }
    assert_eq!(table.entries.len(), MAX_ENTRIES);
}

#[test]
fn a_full_table_drops_the_address_whose_window_opened_first() {
    let mut table = RateTable::new();
    let opened = Instant::now();

    // Fill the table, each address one second after the one before it, so the
    // first one in is plainly the oldest.
    for step in 0..u32::try_from(MAX_ENTRIES).expect("the bound fits") {
        let at = later(opened, u64::from(step) % (RATE_WINDOW.as_secs() - 1));
        table.allow(IpAddr::V4(Ipv4Addr::from(step)), at);
    }
    assert_eq!(table.entries.len(), MAX_ENTRIES);
    let oldest = table
        .entries
        .iter()
        .min_by_key(|(_, window)| window.opened)
        .map(|(address, _)| *address)
        .expect("a full table holds an oldest address");

    table.allow(caller(200), opened);

    assert!(
        !table.entries.contains_key(&oldest),
        "the address whose window opened first left to make room"
    );
    assert!(
        table.entries.contains_key(&caller(200)),
        "the address that arrived took the room it made"
    );
    assert_eq!(table.entries.len(), MAX_ENTRIES);
}

#[test]
fn the_admission_window_holds_only_what_it_is_bounded_at() {
    // The attempt limit is per address, so a flood from many addresses reaches
    // this bound instead. Every place inside the window is taken, and the next
    // caller is turned away.
    let counted = Arc::new(AtomicUsize::new(0));
    let mut inside: Vec<InAdmission> = Vec::new();

    for place in 0..MAX_IN_ADMISSION {
        let taken = InAdmission::enter(&counted)
            .unwrap_or_else(|| panic!("place {place} of {MAX_IN_ADMISSION} is free"));
        inside.push(taken);
    }
    assert_eq!(counted.load(Ordering::Acquire), MAX_IN_ADMISSION);
    assert!(
        InAdmission::enter(&counted).is_none(),
        "the caller arriving at a full window is turned away"
    );
}

#[test]
fn a_place_in_the_admission_window_is_given_back_however_the_caller_left() {
    let counted = Arc::new(AtomicUsize::new(0));
    let mut inside: Vec<InAdmission> = (0..MAX_IN_ADMISSION)
        .map(|_| InAdmission::enter(&counted).expect("the window starts empty"))
        .collect();
    assert!(InAdmission::enter(&counted).is_none());

    // One caller leaves — admitted, refused or hung up, it is the same drop.
    inside.pop();
    assert_eq!(counted.load(Ordering::Acquire), MAX_IN_ADMISSION - 1);

    let next = InAdmission::enter(&counted).expect("the place it left is free");
    assert_eq!(counted.load(Ordering::Acquire), MAX_IN_ADMISSION);

    drop(next);
    drop(inside);
    assert_eq!(
        counted.load(Ordering::Acquire),
        0,
        "an empty window counts nothing"
    );
}
