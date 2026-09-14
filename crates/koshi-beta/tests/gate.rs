//! Tests for the `#[beta_feature]` attribute.
//!
//! The attribute reads one process-wide flag. Every case lives in one test and
//! runs in sequence on that flag.

use std::sync::atomic::{AtomicU32, Ordering};

use koshi_beta::beta_feature;

/// Counts how many times a gated body ran.
static BETA_BODY_EXECUTION_COUNT: AtomicU32 = AtomicU32::new(0);

/// A gated entry point written as ordinary code: nothing in the signature or
/// the body knows about the gate.
#[beta_feature(otherwise = Err("beta feature is off"))]
fn double_number(input_number: u32) -> Result<u32, &'static str> {
    BETA_BODY_EXECUTION_COUNT.fetch_add(1, Ordering::Relaxed);
    Ok(input_number * 2)
}

/// A second site with its own `otherwise`.
#[beta_feature(otherwise = 0)]
fn get_beta_body_execution_count() -> u32 {
    BETA_BODY_EXECUTION_COUNT.load(Ordering::Relaxed)
}

/// A site called only by the warn-once check. Its first blocked call is the
/// one that warns.
#[beta_feature(otherwise = 0)]
fn emit_blocked_warning_once() -> u32 {
    1
}

/// A site the cross-thread case calls. It runs on a spawned thread and touches
/// the gate only by reading it.
#[beta_feature(otherwise = 0)]
fn run_on_spawned_thread() -> u32 {
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
fn list_warnings_for_function<'a>(log_lines: &'a [String], function_name: &str) -> Vec<&'a String> {
    let function_field = format!(r#""function":"{function_name}""#);
    log_lines
        .iter()
        .filter(|log_line| log_line.contains(&function_field))
        .collect()
}

#[test]
fn a_gated_body_runs_only_when_beta_features_are_allowed() {
    let (_guard, logs) = koshi_observability::logging::with_test_writer();

    // Off: the body never runs and the call returns the `otherwise` value.
    koshi_beta::set_beta_features_allowed(false);
    assert_eq!(double_number(21), Err("beta feature is off"));
    assert_eq!(get_beta_body_execution_count(), 0);
    assert_eq!(BETA_BODY_EXECUTION_COUNT.load(Ordering::Relaxed), 0);

    // On: the body runs and the call returns what the body returns.
    koshi_beta::set_beta_features_allowed(true);
    assert_eq!(double_number(21), Ok(42));
    assert_eq!(get_beta_body_execution_count(), 1);
    assert_eq!(BETA_BODY_EXECUTION_COUNT.load(Ordering::Relaxed), 1);

    // Off again: the same site stops running mid-process. The execution count is 1 here, so
    // the second site answers 0 only while it is blocked.
    koshi_beta::set_beta_features_allowed(false);
    assert_eq!(double_number(21), Err("beta feature is off"));
    assert_eq!(get_beta_body_execution_count(), 0);
    assert_eq!(BETA_BODY_EXECUTION_COUNT.load(Ordering::Relaxed), 1);

    // A blocked site warns on its first blocked call and never again.
    for _ in 0..3 {
        assert_eq!(emit_blocked_warning_once(), 0);
    }
    // Two blocked sites named `attach` in different modules.
    assert_eq!(session::attach(), 0);
    assert_eq!(router::attach(), 0);

    let log_lines = logs.lines();
    let warnings = list_warnings_for_function(&log_lines, "gate::emit_blocked_warning_once");
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
            r#""message":"`gate::emit_blocked_warning_once` is a beta feature and did nothing; add a top-level `allow-beta-features #true` line to koshi.kdl to run it""#
        ),
        "{warnings:?}"
    );

    // The name is the module path plus the identifier, so two sites named
    // `attach` in different modules get one warning each under their own name.
    // No record carries the bare identifier.
    assert_eq!(
        list_warnings_for_function(&log_lines, "gate::session::attach").len(),
        1,
        "{log_lines:?}"
    );
    assert_eq!(
        list_warnings_for_function(&log_lines, "gate::router::attach").len(),
        1,
        "{log_lines:?}"
    );
    assert_eq!(
        list_warnings_for_function(&log_lines, "attach").len(),
        0,
        "{log_lines:?}"
    );

    // The limit is per site. `double_number` and `get_beta_body_execution_count` were each blocked twice
    // with an allowed call in between; each warned once. Allowed calls log
    // nothing: the five warnings are the whole log.
    assert_eq!(
        list_warnings_for_function(&log_lines, "gate::double_number").len(),
        1,
        "{log_lines:?}"
    );
    assert_eq!(
        list_warnings_for_function(&log_lines, "gate::get_beta_body_execution_count").len(),
        1,
        "{log_lines:?}"
    );
    assert_eq!(log_lines.len(), 5, "{log_lines:?}");

    // The flag is process-wide. A gated site called on a spawned thread, which
    // never sets the flag itself, answers what this thread stored last.
    koshi_beta::set_beta_features_allowed(true);
    let spawned_thread_result = std::thread::spawn(run_on_spawned_thread).join().unwrap();
    assert_eq!(spawned_thread_result, 1);

    koshi_beta::set_beta_features_allowed(false);
    let spawned_thread_result = std::thread::spawn(run_on_spawned_thread).join().unwrap();
    assert_eq!(spawned_thread_result, 0);
}
