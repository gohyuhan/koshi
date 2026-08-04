//! Tests for the `#[beta_feature]` attribute.
//!
//! The attribute reads a process-wide flag, so every case lives in one test:
//! separate tests would run in parallel and race each other over it.

use std::sync::atomic::{AtomicU32, Ordering};

use koshi_beta::beta_feature;

/// Counts how many times a gated body actually ran.
static RUNS: AtomicU32 = AtomicU32::new(0);

/// A gated entry point written as ordinary code: nothing in the signature or
/// the body knows about the gate.
#[beta_feature(otherwise = Err("beta feature is off"))]
fn double(value: u32) -> Result<u32, &'static str> {
    RUNS.fetch_add(1, Ordering::Relaxed);
    Ok(value * 2)
}

/// A second site, to show the fallback is per site rather than one shape.
#[beta_feature(otherwise = 0)]
fn count_runs() -> u32 {
    RUNS.load(Ordering::Relaxed)
}

/// Its own site, so the `Once` it spends is nobody else's.
#[beta_feature(otherwise = 0)]
fn warns_once() -> u32 {
    1
}

#[test]
fn a_gated_body_runs_only_when_beta_features_are_allowed() {
    let (_guard, logs) = koshi_observability::logging::with_test_writer();

    // Off: the body never runs and the call returns the `otherwise` value.
    koshi_beta::set_allowed(false);
    assert_eq!(double(21), Err("beta feature is off"));
    assert_eq!(count_runs(), 0);
    assert_eq!(RUNS.load(Ordering::Relaxed), 0);

    // On: the body runs and the call returns what the body returns.
    koshi_beta::set_allowed(true);
    assert_eq!(double(21), Ok(42));
    assert_eq!(count_runs(), 1);
    assert_eq!(RUNS.load(Ordering::Relaxed), 1);

    // Off again: the same site stops running mid-process. `RUNS` is 1 here, so
    // the second site answers 0 only while it is blocked.
    koshi_beta::set_allowed(false);
    assert_eq!(double(21), Err("beta feature is off"));
    assert_eq!(count_runs(), 0);
    assert_eq!(RUNS.load(Ordering::Relaxed), 1);

    // A blocked site warns once however often it is called, so a gated entry
    // point on a hot path cannot flood the log.
    for _ in 0..3 {
        assert_eq!(warns_once(), 0);
    }
    let lines = logs.lines();
    let warnings: Vec<&String> = lines
        .iter()
        .filter(|line| line.contains("warns_once"))
        .collect();
    assert_eq!(
        warnings.len(),
        1,
        "three blocked calls must warn once, got {warnings:?}"
    );
    // The line names the function that did nothing and gives the exact line to
    // add. Asserted whole: a message that named a block the file has no place
    // for would send the user to write KDL that does not parse.
    assert!(
        warnings[0].contains(
            "`warns_once` is a beta feature and did nothing; \
             add a top-level `allow-beta-features #true` line to koshi.kdl to run it"
        ),
        "{warnings:?}"
    );
    // Warning level, so the line survives the `logging { level "warning" }` a
    // user can set, and a machine-readable `function` field beside the prose.
    assert!(warnings[0].contains(r#""level":"WARN""#), "{warnings:?}");
    assert!(
        warnings[0].contains(r#""function":"warns_once""#),
        "{warnings:?}"
    );
}
