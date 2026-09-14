//! Tests for the `koshi doctor` checks: every branch of every check, the row
//! order, and the failure a run ends with.

use std::fs;
use std::time::Duration;

use koshi_ipc::remote_tokens::{TokenRecord, TokenScope};
use tempfile::TempDir;

use super::*;

/// A `DoctorContext` whose every fact is known, rooted at `root_directory`. A test changes the
/// one field it is about and leaves the rest.
///
/// Creates no file and reads no environment variable. A test that needs
/// `<directory_path>/runtime`, `<directory_path>/config`, `<directory_path>/log`, `<directory_path>/config/plugins` or
/// `<directory_path>/shell` to be there creates it itself.
fn build_doctor_context(root_directory: &Path) -> DoctorContext {
    DoctorContext {
        config_directory: Some(root_directory.join("config")),
        runtime_directory: Some(root_directory.join("runtime")),
        runtime_directory_rule: Some(RuntimeDirectoryRule::EnvironmentVariable),
        runtime_directory_mode: Some(0o700),
        log_directory: Some(root_directory.join("log")),
        plugins_directory: Some(root_directory.join("config").join("plugins")),
        shared_directory: Some(root_directory.join("shared")),
        shell: root_directory.join("shell"),
        shell_source: ShellSource::Environment,
        path_entries: Some(root_directory.as_os_str().to_os_string()),
        term: Some("xterm-256color".to_string()),
        colorterm: None,
        is_other_user_access_allowed: false,
        remote_listen: None,
        is_logging_enabled: false,
        standing_grant_count: Ok(0),
        router_connections: RemoteConnections::NotRunning,
    }
}

/// Create `shell_path` as a file this machine can run: an empty file, mode `0755`
/// on Unix and the bare write on Windows.
fn write_runnable(shell_path: &Path) {
    fs::write(shell_path, "").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(shell_path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The config directory of `doctor_context`, created and empty.
fn build_config_directory(doctor_context: &DoctorContext) -> PathBuf {
    let config_directory = doctor_context.config_directory.clone().unwrap();
    fs::create_dir_all(&config_directory).unwrap();
    config_directory
}

// ---------------------------------------------------------------- config

#[test]
fn config_fails_when_this_machine_reports_no_home_directory() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.config_directory = None;

    assert_eq!(
        check_config(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Fail,
            reason: "this machine reports no home directory, so koshi finds no config directory"
                .to_string(),
            help: Some("give this user a home directory".to_string()),
            detail: None,
        }
    );
}

#[test]
fn config_fails_on_a_file_that_does_not_validate() {
    let test_directory = TempDir::new().unwrap();
    let doctor_context = build_doctor_context(test_directory.path());
    let config_directory = build_config_directory(&doctor_context);
    fs::write(config_directory.join("koshi.kdl"), "pane {}\n").unwrap();

    assert_eq!(
        check_config(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Fail,
            reason: format!(
                "invalid config version in {}: file must declare `version`",
                config_directory.join("koshi.kdl").display()
            ),
            help: Some("run koshi config check to see each file".to_string()),
            detail: None,
        }
    );
}

#[test]
fn config_is_ok_when_the_directory_holds_no_config_file() {
    let test_directory = TempDir::new().unwrap();
    let doctor_context = build_doctor_context(test_directory.path());
    let config_directory = build_config_directory(&doctor_context);

    assert_eq!(
        check_config(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: format!(
                "no config file is present in {}",
                config_directory.display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn config_counts_one_validated_file_in_the_singular() {
    let test_directory = TempDir::new().unwrap();
    let doctor_context = build_doctor_context(test_directory.path());
    let config_directory = build_config_directory(&doctor_context);
    fs::write(config_directory.join("koshi.kdl"), "version 1\n").unwrap();

    assert_eq!(
        check_config(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: "1 config file validated".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn config_counts_two_validated_files_in_the_plural() {
    let test_directory = TempDir::new().unwrap();
    let doctor_context = build_doctor_context(test_directory.path());
    let config_directory = build_config_directory(&doctor_context);
    fs::write(config_directory.join("koshi.kdl"), "version 1\n").unwrap();
    fs::write(
        config_directory.join("keybinding.kdl"),
        "version 1\nmode \"normal\" {}\n",
    )
    .unwrap();

    assert_eq!(
        check_config(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: "2 config files validated".to_string(),
            help: None,
            detail: None,
        }
    );
}

// ----------------------------------------------------------------- shell

#[test]
fn shell_is_ok_when_the_named_shell_is_on_this_machine() {
    let test_directory = TempDir::new().unwrap();
    let doctor_context = build_doctor_context(test_directory.path());
    write_runnable(&doctor_context.shell);

    assert_eq!(
        check_shell(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: format!("a new pane runs {}", doctor_context.shell.display()),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn shell_warns_when_the_shell_comes_from_the_fallback() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.shell_source = ShellSource::Fallback;
    write_runnable(&doctor_context.shell);

    assert_eq!(
        check_shell(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Warn,
            reason: format!(
                "{SHELL_ENVIRONMENT_VARIABLE} is not set, so a new pane runs {}",
                doctor_context.shell.display()
            ),
            help: Some(format!(
                "set {SHELL_ENVIRONMENT_VARIABLE} to the shell you want a new pane to run"
            )),
            detail: None,
        }
    );
}

#[test]
fn shell_fails_when_the_named_shell_is_not_on_this_machine() {
    let test_directory = TempDir::new().unwrap();
    let doctor_context = build_doctor_context(test_directory.path());

    assert_eq!(
        check_shell(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Fail,
            reason: format!(
                "a new pane would run {}, which is not on this machine",
                doctor_context.shell.display()
            ),
            help: Some(format!(
                "set {SHELL_ENVIRONMENT_VARIABLE} to a shell that exists"
            )),
            detail: None,
        }
    );
}

#[test]
fn shell_is_ok_when_koshi_kdl_names_a_shell_on_this_machine() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.shell_source = ShellSource::Config;
    write_runnable(&doctor_context.shell);

    assert_eq!(
        check_shell(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: format!(
                "koshi.kdl names {}, which a new pane runs",
                doctor_context.shell.display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn shell_fails_when_koshi_kdl_names_a_shell_that_is_not_on_this_machine() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.shell_source = ShellSource::Config;

    assert_eq!(
        check_shell(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Fail,
            reason: format!(
                "koshi.kdl names {}, which is not on this machine",
                doctor_context.shell.display()
            ),
            help: Some(
                "set terminal.default-shell in koshi.kdl to a shell that exists".to_string()
            ),
            detail: None,
        }
    );
}

#[test]
fn shell_named_without_a_directory_is_found_through_path() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.shell = PathBuf::from("bare-shell");
    write_runnable(&test_directory.path().join("bare-shell"));

    assert_eq!(
        check_shell(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: "a new pane runs bare-shell".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn shell_named_without_a_directory_fails_when_path_is_unset() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.shell = PathBuf::from("bare-shell");
    doctor_context.path_entries = None;
    write_runnable(&test_directory.path().join("bare-shell"));

    assert_eq!(
        check_shell(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Fail,
            reason: "a new pane would run bare-shell, which is not on this machine".to_string(),
            help: Some(format!(
                "set {SHELL_ENVIRONMENT_VARIABLE} to a shell that exists"
            )),
            detail: None,
        }
    );
}

// -------------------------------------------------------------- terminal

/// The one help line every terminal branch carries.
const TERMINAL_HELP: &str = "set TERM before running koshi, for example TERM=xterm-256color";

#[test]
fn terminal_warns_when_term_is_unset() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.term = None;

    assert_eq!(
        check_terminal(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Warn,
            reason: "TERM is not set".to_string(),
            help: Some(TERMINAL_HELP.to_string()),
            detail: None,
        }
    );
}

#[test]
fn terminal_warns_when_term_is_dumb() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.term = Some("dumb".to_string());

    assert_eq!(
        check_terminal(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Warn,
            reason: "TERM is dumb, which names a terminal with no cursor control".to_string(),
            help: Some(TERMINAL_HELP.to_string()),
            detail: None,
        }
    );
}

#[test]
fn terminal_reports_term_without_colorterm() {
    let test_directory = TempDir::new().unwrap();
    let doctor_context = build_doctor_context(test_directory.path());

    assert_eq!(
        check_terminal(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: "TERM is xterm-256color, COLORTERM is not set".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn terminal_reports_term_with_colorterm() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.colorterm = Some("truecolor".to_string());

    assert_eq!(
        check_terminal(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: "TERM is xterm-256color, COLORTERM is truecolor".to_string(),
            help: None,
            detail: None,
        }
    );
}

// ----------------------------------------------------- runtime directory

/// The runtime directory of `doctor_context`, created and empty.
fn build_runtime_directory(doctor_context: &DoctorContext) -> PathBuf {
    let runtime_directory = doctor_context.runtime_directory.clone().unwrap();
    fs::create_dir_all(&runtime_directory).unwrap();
    runtime_directory
}

#[test]
fn runtime_directory_fails_when_this_machine_reports_no_home_directory() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.runtime_directory = None;
    doctor_context.runtime_directory_rule = None;

    assert_eq!(
        check_runtime_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Fail,
            reason: "this machine reports no home directory, so koshi finds no runtime directory"
                .to_string(),
            help: Some("give this user a home directory".to_string()),
            detail: None,
        }
    );
}

#[test]
fn runtime_directory_is_ok_when_it_does_not_exist_yet() {
    let test_directory = TempDir::new().unwrap();
    let doctor_context = build_doctor_context(test_directory.path());

    assert_eq!(
        check_runtime_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: format!(
                "{} does not exist yet; koshi creates it under {} when a session starts; KOSHI_RUNTIME_DIR names it",
                doctor_context.runtime_directory.as_deref().unwrap().display(),
                test_directory.path().display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn runtime_directory_fails_when_it_cannot_be_read() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    let runtime_directory_path = test_directory.path().join("runtime-file");
    fs::write(&runtime_directory_path, "").unwrap();
    doctor_context.runtime_directory = Some(runtime_directory_path.clone());
    let runtime_directory_read_error = fs::read_dir(&runtime_directory_path).unwrap_err();

    assert_eq!(
        check_runtime_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Fail,
            reason: format!(
                "{} cannot be read: {runtime_directory_read_error}; KOSHI_RUNTIME_DIR names it",
                runtime_directory_path.display()
            ),
            help: Some(format!(
                "make sure you own {}",
                runtime_directory_path.display()
            )),
            detail: None,
        }
    );
}

#[test]
fn runtime_directory_fails_on_a_mode_other_than_700() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    let runtime_directory_path = build_runtime_directory(&doctor_context);
    doctor_context.runtime_directory_mode = Some(0o755);

    assert_eq!(
        check_runtime_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Fail,
            reason: format!(
                "{} has mode 755; koshi serves a session socket only from a directory with mode 700; KOSHI_RUNTIME_DIR names it",
                runtime_directory_path.display()
            ),
            help: Some(format!(
                "run chmod 700 {}",
                runtime_directory_path.display()
            )),
            detail: None,
        }
    );
}

#[test]
fn runtime_directory_with_mode_700_is_ok() {
    let test_directory = TempDir::new().unwrap();
    let doctor_context = build_doctor_context(test_directory.path());
    let runtime_directory_path = build_runtime_directory(&doctor_context);

    assert_eq!(
        check_runtime_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: format!(
                "{} is ready; KOSHI_RUNTIME_DIR names it",
                runtime_directory_path.display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn runtime_directory_with_unknown_mode_is_ok() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    let runtime_directory_path = build_runtime_directory(&doctor_context);
    doctor_context.runtime_directory_mode = None;

    assert_eq!(
        check_runtime_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: format!(
                "{} is ready; KOSHI_RUNTIME_DIR names it",
                runtime_directory_path.display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn runtime_directory_says_nothing_about_the_router() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    let runtime_directory_path = build_runtime_directory(&doctor_context);
    let expected_runtime_directory_outcome = DoctorOutcome {
        verdict: Verdict::Ok,
        reason: format!(
            "{} is ready; KOSHI_RUNTIME_DIR names it",
            runtime_directory_path.display()
        ),
        help: None,
        detail: None,
    };

    for router_connections in [
        RemoteConnections::Answered(Some(0)),
        RemoteConnections::Answered(None),
        RemoteConnections::NotRunning,
        RemoteConnections::OlderBuild,
        RemoteConnections::NoAnswer {
            error_detail: "connection refused".to_string(),
        },
    ] {
        doctor_context.router_connections = router_connections;
        assert_eq!(
            check_runtime_directory(&doctor_context),
            expected_runtime_directory_outcome
        );
    }
}

#[test]
fn runtime_directory_names_the_variable_that_set_it() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    let runtime_directory_path = build_runtime_directory(&doctor_context);
    doctor_context.runtime_directory_rule = Some(RuntimeDirectoryRule::EnvironmentVariable);

    assert_eq!(
        check_runtime_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: format!(
                "{} is ready; KOSHI_RUNTIME_DIR names it",
                runtime_directory_path.display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn runtime_directory_names_the_user_id_rule() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    let runtime_directory_path = build_runtime_directory(&doctor_context);
    doctor_context.runtime_directory_rule = Some(RuntimeDirectoryRule::UserId);

    assert_eq!(
        check_runtime_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: format!(
                "{} is ready; koshi names it after your user id",
                runtime_directory_path.display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn runtime_directory_names_the_data_directory_rule() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    let runtime_directory_path = build_runtime_directory(&doctor_context);
    doctor_context.runtime_directory_rule = Some(RuntimeDirectoryRule::DataDirectory);

    assert_eq!(
        check_runtime_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: format!(
                "{} is ready; koshi puts it under your application data directory",
                runtime_directory_path.display()
            ),
            help: None,
            detail: None,
        }
    );
}

// ---------------------------------------------------------------- router

#[test]
fn router_is_ok_when_one_answered() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.router_connections = RemoteConnections::Answered(Some(2));

    assert_eq!(
        check_router(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: "a router answers on its control socket".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn router_is_ok_when_one_answered_without_a_count() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.router_connections = RemoteConnections::Answered(None);

    assert_eq!(
        check_router(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: "a router answers on its control socket".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn router_is_ok_when_none_runs() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.router_connections = RemoteConnections::NotRunning;

    assert_eq!(
        check_router(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: "no koshi is running".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn router_warns_on_an_older_build_and_does_not_fail_the_run() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.router_connections = RemoteConnections::OlderBuild;

    assert_eq!(
        check_router(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Warn,
            reason: "the running router is an older koshi build".to_string(),
            help: Some("end every koshi process on this machine and start one again".to_string()),
            detail: None,
        }
    );
}

#[test]
fn router_fails_when_a_listening_router_did_not_answer_and_keeps_the_full_text() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.router_connections = RemoteConnections::NoAnswer {
        error_detail: "connection refused".to_string(),
    };

    assert_eq!(
        check_router(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Fail,
            reason: "a router is listening and did not answer".to_string(),
            help: Some("end every koshi process on this machine and start one again".to_string()),
            detail: Some("connection refused".to_string()),
        }
    );
}

// --------------------------------------------------------- log directory

#[test]
fn log_directory_warns_with_no_home_directory_because_a_log_still_lands_somewhere() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.log_directory = None;

    assert_eq!(
        check_log_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Warn,
            reason: "this machine reports no home directory, so a log file lands in whichever \
                     directory koshi is started from"
                .to_string(),
            help: Some("give this user a home directory".to_string()),
            detail: None,
        }
    );
}

#[test]
fn log_directory_is_ok_when_it_does_not_exist_yet() {
    let test_directory = TempDir::new().unwrap();
    let doctor_context = build_doctor_context(test_directory.path());

    assert_eq!(
        check_log_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: format!(
                "{} does not exist yet; koshi creates it under {} when logging is on",
                doctor_context.log_directory.as_deref().unwrap().display(),
                test_directory.path().display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn log_directory_reports_logging_off_when_it_is_writable() {
    let test_directory = TempDir::new().unwrap();
    let doctor_context = build_doctor_context(test_directory.path());
    let log_directory_path = doctor_context.log_directory.clone().unwrap();
    fs::create_dir_all(&log_directory_path).unwrap();

    assert_eq!(
        check_log_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: format!(
                "{} is writable and logging is off",
                log_directory_path.display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn log_directory_reports_logging_on_when_it_is_writable() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    let log_directory_path = doctor_context.log_directory.clone().unwrap();
    fs::create_dir_all(&log_directory_path).unwrap();
    doctor_context.is_logging_enabled = true;

    assert_eq!(
        check_log_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: format!(
                "{} is writable and logging is on",
                log_directory_path.display()
            ),
            help: None,
            detail: None,
        }
    );
}

/// The reason ends in ` at path "<name>"`, where `<name>` is the random file
/// name `tempfile` tried. The assertion covers everything before it.
#[test]
fn log_directory_fails_when_it_cannot_be_written() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    let log_directory_path = test_directory.path().join("log-file");
    fs::write(&log_directory_path, "").unwrap();
    doctor_context.log_directory = Some(log_directory_path.clone());
    let log_file_probe_error = tempfile::NamedTempFile::new_in(&log_directory_path)
        .unwrap_err()
        .to_string();
    let log_directory_error = log_file_probe_error
        .split_once(" at path ")
        .unwrap()
        .0
        .to_string();

    let check_outcome = check_log_directory(&doctor_context);

    assert_eq!(check_outcome.verdict, Verdict::Fail);
    assert_eq!(
        check_outcome.help,
        Some(format!(
            "make sure you own {}",
            log_directory_path.display()
        ))
    );
    assert_eq!(
        check_outcome.reason.split_once(" at path ").unwrap().0,
        format!(
            "{} cannot be written: {log_directory_error}",
            log_directory_path.display()
        )
    );
}

// ----------------------------------------------------- plugins directory

#[test]
fn plugins_directory_fails_when_this_machine_reports_no_home_directory() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.plugins_directory = None;

    assert_eq!(
        check_plugins_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Fail,
            reason: "this machine reports no home directory, so koshi finds no plugins directory"
                .to_string(),
            help: Some("give this user a home directory".to_string()),
            detail: None,
        }
    );
}

#[test]
fn plugins_directory_is_ok_when_it_does_not_exist() {
    let test_directory = TempDir::new().unwrap();
    let doctor_context = build_doctor_context(test_directory.path());

    assert_eq!(
        check_plugins_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: format!(
                "{} does not exist",
                doctor_context
                    .plugins_directory
                    .as_deref()
                    .unwrap()
                    .display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn plugins_directory_is_ok_when_it_is_readable() {
    let test_directory = TempDir::new().unwrap();
    let doctor_context = build_doctor_context(test_directory.path());
    let plugins_directory_path = doctor_context.plugins_directory.clone().unwrap();
    fs::create_dir_all(&plugins_directory_path).unwrap();

    assert_eq!(
        check_plugins_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: format!("{} is readable", plugins_directory_path.display()),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn plugins_directory_fails_when_it_cannot_be_read() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    let plugins_directory_path = test_directory.path().join("plugins-file");
    fs::write(&plugins_directory_path, "").unwrap();
    doctor_context.plugins_directory = Some(plugins_directory_path.clone());
    let plugins_directory_read_error = fs::read_dir(&plugins_directory_path).unwrap_err();

    assert_eq!(
        check_plugins_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Fail,
            reason: format!(
                "{} cannot be read: {plugins_directory_read_error}",
                plugins_directory_path.display()
            ),
            help: Some(format!(
                "make sure you own {}",
                plugins_directory_path.display()
            )),
            detail: None,
        }
    );
}

// ----------------------------------------------------- session directory

#[test]
fn session_directory_without_other_users_and_without_a_home_directory() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.runtime_directory = None;

    assert_eq!(
        check_session_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: "allow-other-users is off, so only you may reach your sessions".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn session_directory_without_other_users_names_the_runtime_directory_and_its_mode() {
    let test_directory = TempDir::new().unwrap();
    let doctor_context = build_doctor_context(test_directory.path());

    assert_eq!(
        check_session_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: format!(
                "sessions are advertised in {} (mode 700), which only you may reach",
                doctor_context
                    .runtime_directory
                    .as_deref()
                    .unwrap()
                    .display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn session_directory_without_other_users_leaves_out_a_mode_it_does_not_know() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.runtime_directory_mode = None;

    assert_eq!(
        check_session_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: format!(
                "sessions are advertised in {}, which only you may reach",
                doctor_context
                    .runtime_directory
                    .as_deref()
                    .unwrap()
                    .display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn session_directory_with_other_users_names_the_shared_directory() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.is_other_user_access_allowed = true;

    assert_eq!(
        check_session_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: format!(
                "allow-other-users is on: sessions are also advertised in {}, which every user of this machine may reach",
                doctor_context.shared_directory.as_deref().unwrap().display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn session_directory_with_other_users_and_no_shared_directory() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.is_other_user_access_allowed = true;
    doctor_context.shared_directory = None;

    assert_eq!(
        check_session_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: "allow-other-users is on, and this machine names no shared session directory, so no other user reaches your sessions"
                .to_string(),
            help: None,
            detail: None,
        }
    );
}

// --------------------------------------------------------- remote access

#[test]
fn remote_access_without_an_address_counts_zero_one_and_two_grants() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());

    assert_eq!(
        check_remote_access(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason:
                "koshi.kdl names no remote listen address, and this machine holds 0 standing grants"
                    .to_string(),
            help: None,
            detail: None,
        }
    );

    doctor_context.standing_grant_count = Ok(1);
    assert_eq!(
        check_remote_access(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason:
                "koshi.kdl names no remote listen address, and this machine holds 1 standing grant"
                    .to_string(),
            help: None,
            detail: None,
        }
    );

    doctor_context.standing_grant_count = Ok(2);
    assert_eq!(
        check_remote_access(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason:
                "koshi.kdl names no remote listen address, and this machine holds 2 standing grants"
                    .to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn remote_access_with_an_address_counts_zero_one_and_two_grants() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.remote_listen = Some("0.0.0.0:7777".to_string());

    assert_eq!(
        check_remote_access(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: "koshi.kdl names the remote listen address 0.0.0.0:7777, and this machine holds 0 standing grants"
                .to_string(),
            help: None,
            detail: None,
        }
    );

    doctor_context.standing_grant_count = Ok(1);
    assert_eq!(
        check_remote_access(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: "koshi.kdl names the remote listen address 0.0.0.0:7777, and this machine holds 1 standing grant"
                .to_string(),
            help: None,
            detail: None,
        }
    );

    doctor_context.standing_grant_count = Ok(2);
    assert_eq!(
        check_remote_access(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: "koshi.kdl names the remote listen address 0.0.0.0:7777, and this machine holds 2 standing grants"
                .to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn remote_access_warns_when_the_grants_could_not_be_read() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.standing_grant_count = Err("grants.json: unexpected end of file".to_string());

    assert_eq!(
        check_remote_access(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Warn,
            reason: "koshi.kdl names no remote listen address, and the grants could not be read: grants.json: unexpected end of file"
                .to_string(),
            help: Some("make sure you own the koshi data directory".to_string()),
            detail: None,
        }
    );
}

// ---------------------------------------------------- remote connections

#[test]
fn remote_connections_counts_what_the_router_answered() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.router_connections = RemoteConnections::Answered(Some(0));

    assert_eq!(
        check_remote_connections(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: "this machine holds 0 open connections from another machine".to_string(),
            help: None,
            detail: None,
        }
    );

    doctor_context.router_connections = RemoteConnections::Answered(Some(1));
    assert_eq!(
        check_remote_connections(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: "this machine holds 1 open connection from another machine".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn remote_connections_is_ok_when_no_router_runs() {
    let test_directory = TempDir::new().unwrap();
    let doctor_context = build_doctor_context(test_directory.path());

    assert_eq!(
        check_remote_connections(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: "no koshi is running, so nothing from another machine is connected".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn remote_connections_never_reads_a_missing_count_as_zero() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.router_connections = RemoteConnections::Answered(None);

    assert_eq!(
        check_remote_connections(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: "the running router reports no count, so this is not known".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn remote_connections_reports_not_known_and_never_rates_a_router_that_did_not_answer() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    let expected_unknown_connection_outcome = DoctorOutcome {
        verdict: Verdict::Ok,
        reason: "the running router did not answer, so this is not known".to_string(),
        help: None,
        detail: None,
    };

    for router_connections in [
        RemoteConnections::OlderBuild,
        RemoteConnections::NoAnswer {
            error_detail: "connection refused".to_string(),
        },
    ] {
        doctor_context.router_connections = router_connections;
        assert_eq!(
            check_remote_connections(&doctor_context),
            expected_unknown_connection_outcome
        );
    }
}

// -------------------------------------------------------------- postures

#[test]
fn no_session_or_remote_access_check_ever_fails() {
    let test_directory = TempDir::new().unwrap();

    let mut doctor_contexts = Vec::new();
    for is_other_user_access_allowed in [false, true] {
        for shared_directory in [None, Some(test_directory.path().join("shared"))] {
            for runtime_directory_path in [None, Some(test_directory.path().join("runtime"))] {
                for runtime_directory_mode in [None, Some(0o700), Some(0o755)] {
                    let mut doctor_context = build_doctor_context(test_directory.path());
                    doctor_context.is_other_user_access_allowed = is_other_user_access_allowed;
                    doctor_context.shared_directory = shared_directory.clone();
                    doctor_context.runtime_directory = runtime_directory_path.clone();
                    doctor_context.runtime_directory_mode = runtime_directory_mode;
                    doctor_contexts.push(doctor_context);
                }
            }
        }
    }
    for doctor_context in &doctor_contexts {
        assert_eq!(check_session_directory(doctor_context).verdict, Verdict::Ok);
    }

    for remote_listen_address in [None, Some("0.0.0.0:7777".to_string())] {
        for standing_grant_count in [Ok(0), Ok(1), Ok(2), Err("unreadable".to_string())] {
            let mut doctor_context = build_doctor_context(test_directory.path());
            doctor_context.remote_listen = remote_listen_address.clone();
            let expected_verdict = if standing_grant_count.is_ok() {
                Verdict::Ok
            } else {
                Verdict::Warn
            };
            doctor_context.standing_grant_count = standing_grant_count;
            assert_eq!(
                check_remote_access(&doctor_context).verdict,
                expected_verdict
            );
        }
    }

    for router_connections in [
        RemoteConnections::Answered(Some(3)),
        RemoteConnections::Answered(None),
        RemoteConnections::NotRunning,
        RemoteConnections::OlderBuild,
        RemoteConnections::NoAnswer {
            error_detail: "connection refused".to_string(),
        },
    ] {
        let mut doctor_context = build_doctor_context(test_directory.path());
        doctor_context.router_connections = router_connections;
        assert_eq!(
            check_remote_connections(&doctor_context).verdict,
            Verdict::Ok
        );
    }
}

// --------------------------------------------------------------- helpers

#[test]
fn count_standing_grants_counts_only_the_grants_that_still_stand() {
    let current_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let create_token_record =
        |identity: &str, expires_at: Option<SystemTime>, revoked_at: Option<SystemTime>| {
            TokenRecord {
                identity: identity.to_string(),
                token_hash: "0".repeat(64),
                scope: TokenScope::HostWide,
                issued_at: SystemTime::UNIX_EPOCH,
                expires_at,
                last_used_at: None,
                revoked_at,
            }
        };
    let token_store = TokenStore {
        store_format: TokenStore::new().store_format,
        token_records: vec![
            create_token_record("live", None, None),
            create_token_record("revoked", None, Some(current_time)),
            create_token_record(
                "expired",
                Some(SystemTime::UNIX_EPOCH + Duration::from_secs(999)),
                None,
            ),
            create_token_record(
                "still-good",
                Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_001)),
                None,
            ),
        ],
    };

    assert_eq!(count_standing_grants(&token_store, current_time), 2);
}

#[test]
fn format_counted_noun_puts_an_s_on_every_count_but_one() {
    assert_eq!(format_counted_noun(0, "grant"), "0 grants");
    assert_eq!(format_counted_noun(1, "grant"), "1 grant");
    assert_eq!(format_counted_noun(2, "grant"), "2 grants");
}

#[test]
fn read_directory_mode_reads_the_permission_bits_on_unix_and_nothing_elsewhere() {
    let test_directory = TempDir::new().unwrap();
    let private_directory_path = test_directory.path().join("private");
    fs::create_dir(&private_directory_path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&private_directory_path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    let directory_permission_mode = read_directory_mode(&private_directory_path);

    #[cfg(unix)]
    assert_eq!(directory_permission_mode, Some(0o700));
    #[cfg(not(unix))]
    assert_eq!(directory_permission_mode, None);
}

#[test]
fn read_directory_mode_leaves_out_the_sticky_setgid_and_setuid_bits() {
    let test_directory = TempDir::new().unwrap();
    let sticky_directory_path = test_directory.path().join("sticky");
    fs::create_dir(&sticky_directory_path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&sticky_directory_path, fs::Permissions::from_mode(0o1700)).unwrap();
    }

    let directory_permission_mode = read_directory_mode(&sticky_directory_path);

    #[cfg(unix)]
    assert_eq!(directory_permission_mode, Some(0o700));
    #[cfg(not(unix))]
    assert_eq!(directory_permission_mode, None);
}

#[test]
fn a_sticky_runtime_directory_passes_the_same_check_the_socket_bind_applies() {
    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    let runtime_directory_path = build_runtime_directory(&doctor_context);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&runtime_directory_path, fs::Permissions::from_mode(0o1700)).unwrap();
    }
    doctor_context.runtime_directory_mode = read_directory_mode(&runtime_directory_path);

    assert_eq!(
        check_runtime_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: format!(
                "{} is ready; KOSHI_RUNTIME_DIR names it",
                runtime_directory_path.display()
            ),
            help: None,
            detail: None,
        }
    );
}

// ---------------------------------------------------------- check rows and exit

#[test]
fn build_doctor_check_rows_runs_every_check_in_print_order() {
    let test_directory = TempDir::new().unwrap();
    let doctor_context = build_doctor_context(test_directory.path());

    let check_names: Vec<&str> = build_doctor_check_rows(&doctor_context)
        .iter()
        .map(|check_row| check_row.check_name)
        .collect();

    assert_eq!(
        check_names,
        vec![
            "config",
            "shell",
            "terminal",
            "runtime directory",
            "log directory",
            "plugins directory",
            "router",
            "session directory",
            "remote access",
            "remote connections",
        ]
    );
}

/// One check row carrying `verdict`, named after it.
fn build_check_row(check_name: &'static str, verdict: Verdict) -> DoctorCheckRow {
    DoctorCheckRow {
        check_name,
        outcome: DoctorOutcome {
            verdict,
            reason: "a fact".to_string(),
            help: None,
            detail: None,
        },
    }
}

#[test]
fn a_run_with_no_failed_row_ends_well() {
    let check_rows = vec![
        build_check_row("config", Verdict::Ok),
        build_check_row("shell", Verdict::Ok),
    ];

    assert!(build_failure_outcome_from_check_rows(&check_rows).is_none());
}

#[test]
fn a_run_with_only_a_warning_ends_well() {
    let check_rows = vec![
        build_check_row("config", Verdict::Ok),
        build_check_row("terminal", Verdict::Warn),
    ];

    assert!(build_failure_outcome_from_check_rows(&check_rows).is_none());
}

#[test]
fn a_run_with_two_failed_rows_names_both() {
    let check_rows = vec![
        build_check_row("config", Verdict::Fail),
        build_check_row("terminal", Verdict::Warn),
        build_check_row("shell", Verdict::Fail),
    ];

    assert_eq!(
        build_failure_outcome_from_check_rows(&check_rows)
            .unwrap()
            .to_string(),
        "2 checks failed"
    );
}

#[test]
fn a_run_with_one_failed_row_names_one() {
    let check_rows = vec![
        build_check_row("config", Verdict::Fail),
        build_check_row("shell", Verdict::Ok),
    ];

    assert_eq!(
        build_failure_outcome_from_check_rows(&check_rows)
            .unwrap()
            .to_string(),
        "1 check failed"
    );
}

// ---------------------------------------------------------------- one line

#[test]
fn format_single_line_leaves_text_that_holds_no_break_untouched() {
    assert_eq!(format_single_line("bad file".to_string()), "bad file");
}

#[test]
fn format_single_line_replaces_every_newline_and_carriage_return_with_a_space() {
    assert_eq!(
        format_single_line("bad\nfile\r\nhere".to_string()),
        "bad file  here"
    );
}

#[test]
fn a_reason_carrying_a_newline_reaches_the_row_on_one_line() {
    let check_outcome =
        DoctorOutcome::build_failure_outcome("first\nsecond".to_string(), "do the thing");

    assert_eq!(
        check_outcome,
        DoctorOutcome {
            verdict: Verdict::Fail,
            reason: "first second".to_string(),
            help: Some("do the thing".to_string()),
            detail: None,
        }
    );
}

#[cfg(unix)]
#[test]
fn shell_fails_when_the_named_shell_carries_no_execute_bit() {
    use std::os::unix::fs::PermissionsExt;

    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.shell_source = ShellSource::Config;
    fs::write(&doctor_context.shell, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&doctor_context.shell, fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(
        check_shell(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Fail,
            reason: format!(
                "{} is on this machine and carries no execute bit",
                doctor_context.shell.display()
            ),
            help: Some(format!("run chmod +x {}", doctor_context.shell.display())),
            detail: None,
        }
    );
}

#[cfg(unix)]
#[test]
fn a_runnable_shell_subsequently_in_path_wins_over_one_that_carries_no_execute_bit() {
    use std::os::unix::fs::PermissionsExt;

    let test_directory = TempDir::new().unwrap();
    let first_path_entry = test_directory.path().join("first");
    let second_path_entry = test_directory.path().join("second");
    fs::create_dir_all(&first_path_entry).unwrap();
    fs::create_dir_all(&second_path_entry).unwrap();
    fs::write(first_path_entry.join("bare-shell"), "").unwrap();
    fs::set_permissions(
        first_path_entry.join("bare-shell"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    write_runnable(&second_path_entry.join("bare-shell"));

    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.shell = PathBuf::from("bare-shell");
    doctor_context.path_entries =
        Some(std::env::join_paths([&first_path_entry, &second_path_entry]).unwrap());

    assert_eq!(
        check_shell(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: "a new pane runs bare-shell".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[cfg(unix)]
#[test]
fn runtime_directory_fails_when_nothing_can_be_created_above_it() {
    use std::os::unix::fs::PermissionsExt;

    let test_directory = TempDir::new().unwrap();
    let sealed_directory = test_directory.path().join("sealed");
    fs::create_dir_all(&sealed_directory).unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.runtime_directory = Some(sealed_directory.join("run"));
    fs::set_permissions(&sealed_directory, fs::Permissions::from_mode(0o000)).unwrap();

    let check_outcome = check_runtime_directory(&doctor_context);

    fs::set_permissions(&sealed_directory, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        check_outcome,
        DoctorOutcome {
            verdict: Verdict::Fail,
            reason: format!(
                "{} does not exist and koshi cannot create it: nothing new can be written in {}; KOSHI_RUNTIME_DIR names it",
                sealed_directory.join("run").display(),
                sealed_directory.display()
            ),
            help: Some(format!(
                "make sure you can write in {}",
                sealed_directory.display()
            )),
            detail: None,
        }
    );
}

#[cfg(unix)]
#[test]
fn runtime_directory_fails_when_a_name_above_it_points_nowhere() {
    let test_directory = TempDir::new().unwrap();
    let parent_directory = test_directory.path().join("parent");
    fs::create_dir_all(&parent_directory).unwrap();
    let symlink_path = parent_directory.join("link");
    std::os::unix::fs::symlink(test_directory.path().join("nowhere"), &symlink_path).unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.runtime_directory = Some(symlink_path.join("koshi"));

    assert_eq!(
        check_runtime_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Fail,
            reason: format!(
                "{} does not exist and koshi cannot create it: nothing new can be written in {}; KOSHI_RUNTIME_DIR names it",
                symlink_path.join("koshi").display(),
                symlink_path.display()
            ),
            help: Some(format!(
                "make sure you can write in {}",
                symlink_path.display()
            )),
            detail: None,
        }
    );
}

#[cfg(unix)]
#[test]
fn runtime_directory_is_ok_under_a_symlink_that_points_at_a_real_directory() {
    let test_directory = TempDir::new().unwrap();
    let real_directory = test_directory.path().join("real");
    let parent_directory = test_directory.path().join("parent");
    fs::create_dir_all(&real_directory).unwrap();
    fs::create_dir_all(&parent_directory).unwrap();
    let symlink_path = parent_directory.join("link");
    std::os::unix::fs::symlink(&real_directory, &symlink_path).unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.runtime_directory = Some(symlink_path.join("koshi"));

    assert_eq!(
        check_runtime_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: format!(
                "{} does not exist yet; koshi creates it under {} when a session starts; KOSHI_RUNTIME_DIR names it",
                symlink_path.join("koshi").display(),
                symlink_path.display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[cfg(unix)]
#[test]
fn runtime_directory_fails_when_the_directory_itself_points_nowhere() {
    let test_directory = TempDir::new().unwrap();
    let runtime_directory_path = test_directory.path().join("koshi");
    std::os::unix::fs::symlink(
        test_directory.path().join("nowhere"),
        &runtime_directory_path,
    )
    .unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.runtime_directory = Some(runtime_directory_path.clone());

    assert_eq!(
        check_runtime_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Fail,
            reason: format!(
                "{} is a name koshi cannot make a directory at; KOSHI_RUNTIME_DIR names it",
                runtime_directory_path.display()
            ),
            help: Some(format!(
                "remove {}, or point it at a directory",
                runtime_directory_path.display()
            )),
            detail: None,
        }
    );
}

#[cfg(unix)]
#[test]
fn runtime_directory_fails_when_the_directory_points_at_a_regular_file() {
    let test_directory = TempDir::new().unwrap();
    let regular_file_path = test_directory.path().join("afile");
    fs::write(&regular_file_path, "").unwrap();
    let runtime_directory_path = test_directory.path().join("koshi");
    std::os::unix::fs::symlink(&regular_file_path, &runtime_directory_path).unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.runtime_directory = Some(runtime_directory_path.join("run"));

    assert_eq!(
        check_runtime_directory(&doctor_context).verdict,
        Verdict::Fail,
        "a runtime directory under a link to a regular file must not report ok"
    );
}

#[cfg(unix)]
#[test]
fn runtime_directory_fails_on_a_symlink_loop() {
    let test_directory = TempDir::new().unwrap();
    let first_symlink_path = test_directory.path().join("one");
    let second_symlink_path = test_directory.path().join("two");
    std::os::unix::fs::symlink(&second_symlink_path, &first_symlink_path).unwrap();
    std::os::unix::fs::symlink(&first_symlink_path, &second_symlink_path).unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.runtime_directory = Some(first_symlink_path.join("koshi"));

    assert_eq!(
        check_runtime_directory(&doctor_context).verdict,
        Verdict::Fail,
        "a runtime directory under a symlink loop must not report ok"
    );
}

#[cfg(unix)]
#[test]
fn runtime_directory_fails_when_a_name_above_it_is_a_regular_file() {
    let test_directory = TempDir::new().unwrap();
    let regular_file_path = test_directory.path().join("afile");
    fs::write(&regular_file_path, "").unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.runtime_directory = Some(regular_file_path.join("koshi"));

    assert_eq!(
        check_runtime_directory(&doctor_context).verdict,
        Verdict::Fail,
        "a runtime directory under a regular file must not report ok"
    );
}

#[cfg(unix)]
#[test]
fn shell_fails_when_the_execute_bits_do_not_apply_to_this_user() {
    use std::os::unix::fs::PermissionsExt;

    let test_directory = TempDir::new().unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.shell_source = ShellSource::Config;
    fs::write(&doctor_context.shell, "#!/bin/sh\n").unwrap();
    // Mode 011 on a file this user owns: the execute bits sit on group and
    // other, and the owner bits are the ones that apply.
    fs::set_permissions(&doctor_context.shell, fs::Permissions::from_mode(0o011)).unwrap();

    assert_eq!(
        check_shell(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Fail,
            reason: format!(
                "{} is on this machine and carries no execute bit",
                doctor_context.shell.display()
            ),
            help: Some(format!("run chmod +x {}", doctor_context.shell.display())),
            detail: None,
        }
    );
}

#[cfg(unix)]
#[test]
fn log_directory_fails_when_it_points_nowhere() {
    let test_directory = TempDir::new().unwrap();
    let log_directory_path = test_directory.path().join("logs");
    std::os::unix::fs::symlink(test_directory.path().join("nowhere"), &log_directory_path).unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.log_directory = Some(log_directory_path.clone());

    assert_eq!(
        check_log_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Fail,
            reason: format!(
                "{} is a name koshi cannot make a directory at",
                log_directory_path.display()
            ),
            help: Some(format!(
                "remove {}, or point it at a directory",
                log_directory_path.display()
            )),
            detail: None,
        }
    );
}

#[cfg(unix)]
#[test]
fn plugins_directory_fails_when_it_points_nowhere() {
    let test_directory = TempDir::new().unwrap();
    let plugins_directory_path = test_directory.path().join("plugins");
    std::os::unix::fs::symlink(
        test_directory.path().join("nowhere"),
        &plugins_directory_path,
    )
    .unwrap();
    let mut doctor_context = build_doctor_context(test_directory.path());
    doctor_context.plugins_directory = Some(plugins_directory_path.clone());

    assert_eq!(
        check_plugins_directory(&doctor_context),
        DoctorOutcome {
            verdict: Verdict::Fail,
            reason: format!(
                "{} is there and koshi cannot read it as a directory",
                plugins_directory_path.display()
            ),
            help: Some(format!(
                "remove {}, or point it at a directory",
                plugins_directory_path.display()
            )),
            detail: None,
        }
    );
}
