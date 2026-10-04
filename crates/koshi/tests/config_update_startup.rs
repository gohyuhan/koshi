//! Config migration before the updated binary starts its service commands.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use clap::Parser;
use koshi::cli::Cli;
use koshi::config_command::migrate_config_for_service_command;
use koshi_core::ids::SessionId;
use koshi_daemon::session_server::ResumeSupport;
use koshi_ipc::router::resolve_router_endpoint_path;
use koshi_test_support::fixtures::build_test_runtime_directory;
use tempfile::TempDir;

mod common;

use common::{
    build_koshi_command_under_home, resolve_config_directory_under_home, SESSION_SERVER_NAME,
    WAIT_DURATION,
};

const STARTUP_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(20);

fn build_service_command_arguments(session_id: SessionId) -> [Vec<String>; 3] {
    [
        vec!["koshi".to_string(), "resume-support".to_string()],
        vec!["koshi".to_string(), "serve-router".to_string()],
        vec![
            "koshi".to_string(),
            "serve-session".to_string(),
            session_id.to_string(),
            "workspace".to_string(),
        ],
    ]
}

/// Writes `app_config_source_text` to `<config_directory>/koshi.kdl`, creating
/// `config_directory` first. Returns the `koshi.kdl` path.
fn write_app_config(config_directory: &Path, app_config_source_text: &str) -> PathBuf {
    fs::create_dir_all(config_directory).expect("create config directory");
    let app_config_path = config_directory.join("koshi.kdl");
    fs::write(&app_config_path, app_config_source_text).expect("write app config");
    app_config_path
}

/// Creates `<config_directory>/.migration.lock` as a directory, which no
/// migration can open as its lock file. Returns the lock path and the error
/// that opening it as the lock file gives.
fn block_migration_lock(config_directory: &Path) -> (PathBuf, std::io::Error) {
    let migration_lock_path = config_directory.join(".migration.lock");
    fs::create_dir(&migration_lock_path).expect("block migration lock");
    let lock_open_error = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&migration_lock_path)
        .expect_err("a directory cannot be opened as a migration lock");
    (migration_lock_path, lock_open_error)
}

/// Parses `cli_arguments` and runs the config migration step for the command
/// they name against `config_directory`.
fn run_config_migration_step(
    cli_arguments: Vec<String>,
    config_directory: &Path,
) -> Result<(), koshi_link::error::CliError> {
    let parsed_cli = Cli::try_parse_from(cli_arguments).expect("parse command");
    migrate_config_for_service_command(parsed_cli.command.as_ref(), Some(config_directory))
}

#[test]
fn service_commands_migrate_released_config_before_starting_on_every_platform() {
    for cli_arguments in build_service_command_arguments(SessionId::new()) {
        let test_directory = TempDir::new().expect("create test directory");
        let config_directory = test_directory.path().join("config");
        let app_config_path = write_app_config(&config_directory, "version 1\n");

        run_config_migration_step(cli_arguments, &config_directory)
            .expect("migrate released config");

        assert_eq!(
            fs::read_to_string(app_config_path).expect("read migrated config"),
            "version 2\n"
        );
    }
}

#[test]
fn service_commands_migrate_a_released_config_with_an_unknown_key_on_every_platform() {
    for cli_arguments in build_service_command_arguments(SessionId::new()) {
        let test_directory = TempDir::new().expect("create test directory");
        let config_directory = test_directory.path().join("config");
        let app_config_path = write_app_config(&config_directory, "version 1\nmade-up-key \"x\"\n");

        run_config_migration_step(cli_arguments, &config_directory)
            .expect("migrate released config with an unknown key");

        assert_eq!(
            fs::read_to_string(app_config_path).expect("read migrated config"),
            "version 2\nmade-up-key \"x\"\n"
        );
    }
}

#[test]
fn service_commands_accept_a_current_config_with_an_unknown_key_on_every_platform() {
    for cli_arguments in build_service_command_arguments(SessionId::new()) {
        let test_directory = TempDir::new().expect("create test directory");
        let config_directory = test_directory.path().join("config");
        let app_config_path = write_app_config(&config_directory, "version 2\nmade-up-key \"x\"\n");
        block_migration_lock(&config_directory);

        run_config_migration_step(cli_arguments, &config_directory)
            .expect("current config with an unknown key needs no migration");

        assert_eq!(
            fs::read_to_string(app_config_path).expect("read unchanged config"),
            "version 2\nmade-up-key \"x\"\n"
        );
    }
}

#[test]
fn the_migration_step_reports_config_kdl_that_does_not_parse_on_every_platform() {
    for cli_arguments in build_service_command_arguments(SessionId::new()) {
        let test_directory = TempDir::new().expect("create test directory");
        let config_directory = test_directory.path().join("config");
        let app_config_path = write_app_config(&config_directory, "version 1\npane {");

        let migration_error = run_config_migration_step(cli_arguments, &config_directory)
            .expect_err("reject config that does not parse");

        assert_eq!(
            migration_error.to_string(),
            format!(
                "config failed: config parse error in {}: No closing '}}' for child block",
                app_config_path.display()
            )
        );
        assert_eq!(
            fs::read_to_string(app_config_path).expect("read unchanged config"),
            "version 1\npane {"
        );
    }
}

#[test]
fn the_migration_step_reports_a_blocked_migration_lock_on_every_platform() {
    for cli_arguments in build_service_command_arguments(SessionId::new()) {
        let test_directory = TempDir::new().expect("create test directory");
        let config_directory = test_directory.path().join("config");
        let app_config_path = write_app_config(&config_directory, "version 1\n");
        let (migration_lock_path, lock_open_error) = block_migration_lock(&config_directory);

        let migration_error = run_config_migration_step(cli_arguments, &config_directory)
            .expect_err("reject failed migration");

        assert_eq!(
            migration_error.to_string(),
            format!(
                "config failed: open {}: {lock_open_error}",
                migration_lock_path.display()
            )
        );
        assert_eq!(
            fs::read_to_string(app_config_path).expect("read unchanged config"),
            "version 1\n"
        );
    }
}

#[test]
fn service_commands_accept_current_config_without_a_migration_lock_on_every_platform() {
    for cli_arguments in build_service_command_arguments(SessionId::new()) {
        let test_directory = TempDir::new().expect("create test directory");
        let config_directory = test_directory.path().join("config");
        let app_config_path = write_app_config(&config_directory, "version 2\n");
        block_migration_lock(&config_directory);

        run_config_migration_step(cli_arguments, &config_directory)
            .expect("current config needs no migration lock");

        assert_eq!(
            fs::read_to_string(app_config_path).expect("read unchanged config"),
            "version 2\n"
        );
    }
}

#[test]
fn non_service_commands_leave_released_config_unchanged_on_every_platform() {
    let command_arguments = [
        vec!["koshi".to_string(), "version".to_string()],
        vec![
            "koshi".to_string(),
            "config".to_string(),
            "check".to_string(),
        ],
        vec!["koshi".to_string()],
    ];
    for cli_arguments in command_arguments {
        let test_directory = TempDir::new().expect("create test directory");
        let config_directory = test_directory.path().join("config");
        let app_config_path = write_app_config(&config_directory, "version 1\n");
        block_migration_lock(&config_directory);

        run_config_migration_step(cli_arguments, &config_directory)
            .expect("non-service command does not migrate config");

        assert_eq!(
            fs::read_to_string(app_config_path).expect("read unchanged config"),
            "version 1\n"
        );
    }
}

fn run_test_koshi_command(process_command: &mut Command) -> Output {
    let mut started_process = process_command.spawn().expect("run updated binary");
    let startup_deadline = Instant::now() + WAIT_DURATION;
    loop {
        match started_process.try_wait() {
            Ok(Some(_)) => {
                return started_process
                    .wait_with_output()
                    .expect("read process output")
            }
            Ok(None) if Instant::now() < startup_deadline => {
                std::thread::sleep(STARTUP_POLL_INTERVAL_DURATION);
            }
            Ok(None) => {
                let _ = started_process.kill();
                let _ = started_process.wait();
                panic!("updated binary did not exit within {WAIT_DURATION:?}");
            }
            Err(wait_error) => {
                let _ = started_process.kill();
                let _ = started_process.wait();
                panic!("wait for updated binary: {wait_error}");
            }
        }
    }
}

/// Runs `koshi resume-support` with `home_directory` as the home directory, and
/// asserts it exits `0`, writes nothing to stderr, and prints this build's
/// resume-format bounds under both key pairs.
fn assert_resume_support_answers(home_directory: &Path) {
    let process_output = run_test_koshi_command(
        build_koshi_command_under_home(home_directory).arg("resume-support"),
    );

    assert_eq!(process_output.status.code(), Some(0));
    assert_eq!(process_output.stderr, b"");
    let resume_support = ResumeSupport::from_current_build();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&process_output.stdout)
            .expect("decode resume support"),
        serde_json::json!({
            "minimum_resume_format": resume_support.minimum_resume_format,
            "maximum_resume_format": resume_support.maximum_resume_format,
            "min": resume_support.minimum_resume_format,
            "max": resume_support.maximum_resume_format,
        })
    );
}

#[test]
fn resume_support_migrates_released_config_before_advertising_a_session_swap() {
    let test_directory = TempDir::new().expect("create test directory");
    let app_config_path = write_app_config(
        &resolve_config_directory_under_home(test_directory.path()),
        "version 1\n",
    );

    assert_resume_support_answers(test_directory.path());

    assert_eq!(
        fs::read_to_string(app_config_path).expect("read migrated config"),
        "version 2\n"
    );
}

#[test]
fn resume_support_answers_for_a_released_config_with_an_unknown_key() {
    let test_directory = TempDir::new().expect("create test directory");
    let app_config_path = write_app_config(
        &resolve_config_directory_under_home(test_directory.path()),
        "version 1\nmade-up-key \"x\"\n",
    );

    assert_resume_support_answers(test_directory.path());

    assert_eq!(
        fs::read_to_string(app_config_path).expect("read migrated config"),
        "version 2\nmade-up-key \"x\"\n"
    );
}

#[test]
fn resume_support_accepts_current_config_without_a_migration_lock() {
    let test_directory = TempDir::new().expect("create test directory");
    let config_directory = resolve_config_directory_under_home(test_directory.path());
    let app_config_path = write_app_config(&config_directory, "version 2\n");
    block_migration_lock(&config_directory);

    assert_resume_support_answers(test_directory.path());

    assert_eq!(
        fs::read_to_string(app_config_path).expect("read unchanged config"),
        "version 2\n"
    );
}

#[test]
fn resume_support_answers_when_its_config_migration_fails() {
    let test_directory = TempDir::new().expect("create test directory");
    let config_directory = resolve_config_directory_under_home(test_directory.path());
    let app_config_path = write_app_config(&config_directory, "version 1\n");
    let (migration_lock_path, lock_open_error) = block_migration_lock(&config_directory);

    let process_output = run_test_koshi_command(
        build_koshi_command_under_home(test_directory.path()).arg("resume-support"),
    );

    assert_eq!(process_output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&process_output.stderr),
        format!(
            "koshi: config failed: open {}: {lock_open_error}; starting with the config files \
             as they are\n",
            migration_lock_path.display()
        )
    );
    let resume_support = ResumeSupport::from_current_build();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&process_output.stdout)
            .expect("decode resume support"),
        serde_json::json!({
            "minimum_resume_format": resume_support.minimum_resume_format,
            "maximum_resume_format": resume_support.maximum_resume_format,
            "min": resume_support.minimum_resume_format,
            "max": resume_support.maximum_resume_format,
        })
    );
    assert_eq!(
        fs::read_to_string(&app_config_path).expect("read unchanged config"),
        "version 1\n"
    );
}

#[test]
fn a_router_whose_config_migration_fails_warns_and_serves() {
    let test_directory = TempDir::new().expect("create test directory");
    let config_directory = resolve_config_directory_under_home(test_directory.path());
    let app_config_path = write_app_config(&config_directory, "version 1\npane {");
    let runtime_directory = build_test_runtime_directory();
    let router_endpoint_path = resolve_router_endpoint_path(runtime_directory.path());

    let mut router_process = build_koshi_command_under_home(test_directory.path())
        .arg("serve-router")
        .arg("--runtime-dir")
        .arg(runtime_directory.path())
        .stdout(Stdio::null())
        .spawn()
        .expect("start the router");
    let startup_deadline = Instant::now() + WAIT_DURATION;
    while !router_endpoint_path.exists() && Instant::now() < startup_deadline {
        std::thread::sleep(STARTUP_POLL_INTERVAL_DURATION);
    }
    let is_router_serving = router_endpoint_path.exists();
    let _ = router_process.kill();
    let router_output = router_process
        .wait_with_output()
        .expect("read router output");

    assert!(
        is_router_serving,
        "the router wrote no endpoint file within {WAIT_DURATION:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&router_output.stderr),
        format!(
            "koshi: config failed: config parse error in {}: No closing '}}' for child block; \
             starting with the config files as they are\n",
            app_config_path.display()
        )
    );
    assert_eq!(
        fs::read_to_string(&app_config_path).expect("read unchanged config"),
        "version 1\npane {"
    );
}

#[cfg(target_os = "macos")]
fn resolve_test_log_directory(home_directory: &Path) -> PathBuf {
    home_directory.join("Library/Application Support/koshi/logs")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn resolve_test_log_directory(home_directory: &Path) -> PathBuf {
    home_directory.join(".local/state/koshi/logs")
}

#[cfg(windows)]
fn resolve_test_log_directory(home_directory: &Path) -> PathBuf {
    home_directory
        .join("AppData")
        .join("Local")
        .join("koshi")
        .join("data")
        .join("logs")
}

/// `koshi.kdl` turns logging on, and `keybinding.kdl` holds KDL that does not
/// parse: migration fails on `keybinding.kdl`, and the session server writes
/// that failure to its log file at warn level.
#[test]
fn a_session_server_whose_config_migration_fails_writes_the_failure_to_its_log() {
    let test_directory = TempDir::new().expect("create test directory");
    let config_directory = resolve_config_directory_under_home(test_directory.path());
    write_app_config(
        &config_directory,
        "version 2\nlogging {\n    enabled #true\n    level \"warning\"\n}\n",
    );
    let keybinding_config_path = config_directory.join("keybinding.kdl");
    fs::write(&keybinding_config_path, "version 1\nnormal {").expect("write keybinding config");
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let session_log_path = resolve_test_log_directory(test_directory.path())
        .join(format!("koshi-log-{}.log", session_id.get_uuid()));

    let mut session_server_process = build_koshi_command_under_home(test_directory.path())
        .arg("serve-session")
        .arg(session_id.to_string())
        .arg(SESSION_SERVER_NAME)
        .arg("--runtime-dir")
        .arg(runtime_directory.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the session server");
    let expected_warning = format!(
        "config failed: config parse error in {}: No closing '}}' for child block",
        keybinding_config_path.display()
    );
    let startup_deadline = Instant::now() + WAIT_DURATION;
    let mut session_log_text = String::new();
    while !session_log_text.contains(&expected_warning) && Instant::now() < startup_deadline {
        std::thread::sleep(STARTUP_POLL_INTERVAL_DURATION);
        session_log_text = fs::read_to_string(&session_log_path).unwrap_or_default();
    }
    let _ = session_server_process.kill();
    let _ = session_server_process.wait();

    let warning_line = session_log_text
        .lines()
        .find(|session_log_line| session_log_line.contains(&expected_warning))
        .unwrap_or_else(|| {
            panic!("the session log holds no migration warning:\n{session_log_text}")
        });
    let (_log_timestamp, logged_warning) = warning_line
        .split_once("Z  ")
        .unwrap_or_else(|| panic!("the warning line starts with a timestamp: {warning_line}"));
    assert_eq!(
        logged_warning,
        format!(
            "WARN koshi_daemon::session_server: the config files could not be migrated; the \
             session starts with them as they are, config_migration_error: {expected_warning}"
        )
    );
}
