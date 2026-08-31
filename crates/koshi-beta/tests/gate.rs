//! Tests for the `#[beta_feature]` attribute.
//!
//! The attribute reads one process-wide flag. Every case lives in one test and
//! runs in sequence on that flag.

use std::sync::atomic::{AtomicU32, Ordering};

use koshi_beta::beta_feature;

/// Counts how many times a gated body ran.
static RUNS: AtomicU32 = AtomicU32::new(0);

/// A gated entry point written as ordinary code: nothing in the signature or
/// the body knows about the gate.
#[beta_feature(otherwise = Err("beta feature is off"))]
fn double(value: u32) -> Result<u32, &'static str> {
    RUNS.fetch_add(1, Ordering::Relaxed);
    Ok(value * 2)
}

/// A second site with its own `otherwise`.
#[beta_feature(otherwise = 0)]
fn count_runs() -> u32 {
    RUNS.load(Ordering::Relaxed)
}

/// A site called only by the warn-once check. Its first blocked call is the
/// one that warns.
#[beta_feature(otherwise = 0)]
fn warns_once() -> u32 {
    1
}

/// A site the cross-thread case calls. It runs on a spawned thread and touches
/// the gate only by reading it.
#[beta_feature(otherwise = 0)]
fn on_another_thread() -> u32 {
    1
}

/// Two sites that share the identifier `attach` and differ only in their
/// module.
mod session {
    #[koshi_beta::beta_feature(otherwise = 0)]
    pub fn attach() -> u32 {
        1
    }
}

mod router {
    #[koshi_beta::beta_feature(otherwise = 0)]
    pub fn attach() -> u32 {
        1
    }
}

/// Returns the captured log lines whose `function` field is `function`.
fn warnings_for<'a>(lines: &'a [String], function: &str) -> Vec<&'a String> {
    let field = format!(r#""function":"{function}""#);
    lines.iter().filter(|line| line.contains(&field)).collect()
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

    // A blocked site warns on its first blocked call and never again.
    for _ in 0..3 {
        assert_eq!(warns_once(), 0);
    }
    // Two blocked sites named `attach` in different modules.
    assert_eq!(session::attach(), 0);
    assert_eq!(router::attach(), 0);

    let lines = logs.lines();
    let warnings = warnings_for(&lines, "gate::warns_once");
    assert_eq!(
        warnings.len(),
        1,
        "three blocked calls must warn once, got {warnings:?}"
    );
    // The record is at `WARN` level, comes from `koshi_beta`, and carries the
    // whole message beside the `function` field.
    assert!(warnings[0].contains(r#""level":"WARN""#), "{warnings:?}");
    assert!(
        warnings[0].contains(r#""target":"koshi_beta""#),
        "{warnings:?}"
    );
    assert!(
        warnings[0].contains(
            r#""message":"`gate::warns_once` is a beta feature and did nothing; add a top-level `allow-beta-features #true` line to koshi.kdl to run it""#
        ),
        "{warnings:?}"
    );

    // The name is the module path plus the identifier, so two sites named
    // `attach` in different modules get one warning each under their own name.
    // No record carries the bare identifier.
    assert_eq!(
        warnings_for(&lines, "gate::session::attach").len(),
        1,
        "{lines:?}"
    );
    assert_eq!(
        warnings_for(&lines, "gate::router::attach").len(),
        1,
        "{lines:?}"
    );
    assert_eq!(warnings_for(&lines, "attach").len(), 0, "{lines:?}");

    // The limit is per site. `double` and `count_runs` were each blocked twice
    // with an allowed call in between; each warned once. Allowed calls log
    // nothing: the five warnings are the whole log.
    assert_eq!(warnings_for(&lines, "gate::double").len(), 1, "{lines:?}");
    assert_eq!(
        warnings_for(&lines, "gate::count_runs").len(),
        1,
        "{lines:?}"
    );
    assert_eq!(lines.len(), 5, "{lines:?}");

    // The flag is process-wide. A gated site called on a spawned thread, which
    // never sets the flag itself, answers what this thread stored last.
    koshi_beta::set_allowed(true);
    let on_a_spawned_thread = std::thread::spawn(on_another_thread).join().unwrap();
    assert_eq!(on_a_spawned_thread, 1);

    koshi_beta::set_allowed(false);
    let on_a_spawned_thread = std::thread::spawn(on_another_thread).join().unwrap();
    assert_eq!(on_a_spawned_thread, 0);
}
