//! Config migration before the updated binary starts its service commands.

use std::fs;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Output, Stdio};
#[cfg(unix)]
use std::time::{Duration, Instant};

use clap::Parser;
use koshi::cli::Cli;
use koshi::config_command::migrate_config_for_service_command;
#[cfg(unix)]
use koshi_core::command::CliExitCode;
use koshi_core::ids::SessionId;
#[cfg(unix)]
use koshi_daemon::session_server::ResumeSupport;
use koshi_link::error::CliError;
use tempfile::TempDir;

#[cfg(unix)]
const STARTUP_TEST_TIMEOUT_DURATION: Duration = Duration::from_secs(20);

#[cfg(unix)]
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

#[test]
fn service_commands_migrate_released_config_before_starting_on_every_platform() {
    for cli_arguments in build_service_command_arguments(SessionId::new()) {
        let test_directory = TempDir::new().expect("create test directory");
        let config_directory = test_directory.path().join("config");
        fs::create_dir(&config_directory).expect("create config directory");
        let app_config_path = config_directory.join("koshi.kdl");
        fs::write(&app_config_path, "version 1\n").expect("write released config");
        let cli = Cli::try_parse_from(cli_arguments).expect("parse service command");

        migrate_config_for_service_command(cli.command.as_ref(), Some(&config_directory))
            .expect("migrate released config");

        assert_eq!(
            fs::read_to_string(app_config_path).expect("read migrated config"),
            "version 2\n"
        );
    }
}

#[test]
fn service_commands_reject_failed_config_migration_on_every_platform() {
    for cli_arguments in build_service_command_arguments(SessionId::new()) {
        let test_directory = TempDir::new().expect("create test directory");
        let config_directory = test_directory.path().join("config");
        fs::create_dir(&config_directory).expect("create config directory");
        let app_config_path = config_directory.join("koshi.kdl");
        fs::write(&app_config_path, "version 1\n").expect("write released config");
        let migration_lock_path = config_directory.join(".migration.lock");
        fs::create_dir(&migration_lock_path).expect("block migration lock");
        let expected_open_error = fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&migration_lock_path)
            .expect_err("a directory cannot be opened as a migration lock");
        let cli = Cli::try_parse_from(cli_arguments).expect("parse service command");

        let migration_error =
            migrate_config_for_service_command(cli.command.as_ref(), Some(&config_directory))
                .expect_err("reject failed migration");

        let CliError::Config {
            detail: migration_error_detail,
        } = migration_error
        else {
            panic!("expected a config error");
        };
        assert_eq!(
            migration_error_detail,
            format!(
                "open {}: {expected_open_error}",
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
        fs::create_dir(&config_directory).expect("create config directory");
        let app_config_path = config_directory.join("koshi.kdl");
        fs::write(&app_config_path, "version 2\n").expect("write current config");
        fs::create_dir(config_directory.join(".migration.lock")).expect("block migration lock");
        let cli = Cli::try_parse_from(cli_arguments).expect("parse service command");

        migrate_config_for_service_command(cli.command.as_ref(), Some(&config_directory))
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
        vec!["koshi", "version"],
        vec!["koshi", "config", "check"],
        vec!["koshi"],
    ];
    for cli_arguments in command_arguments {
        let test_directory = TempDir::new().expect("create test directory");
        let config_directory = test_directory.path().join("config");
        fs::create_dir(&config_directory).expect("create config directory");
        let app_config_path = config_directory.join("koshi.kdl");
        fs::write(&app_config_path, "version 1\n").expect("write released config");
        fs::create_dir(config_directory.join(".migration.lock")).expect("block migration lock");
        let cli = Cli::try_parse_from(cli_arguments).expect("parse non-service command");

        migrate_config_for_service_command(cli.command.as_ref(), Some(&config_directory))
            .expect("non-service command does not migrate config");

        assert_eq!(
            fs::read_to_string(app_config_path).expect("read unchanged config"),
            "version 1\n"
        );
    }
}

#[cfg(target_os = "macos")]
fn resolve_test_config_directory(home_directory: &Path) -> PathBuf {
    home_directory.join("Library/Application Support/koshi")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn resolve_test_config_directory(home_directory: &Path) -> PathBuf {
    home_directory.join(".config/koshi")
}

#[cfg(unix)]
fn build_test_koshi_command(home_directory: &Path) -> Command {
    let mut process_command = Command::new(env!("CARGO_BIN_EXE_koshi"));
    process_command
        .env("HOME", home_directory)
        .env("XDG_CONFIG_HOME", home_directory.join(".config"));
    process_command
}

#[cfg(unix)]
fn run_test_koshi_command(process_command: &mut Command) -> Output {
    let mut started_process = process_command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run updated binary");
    let startup_deadline = Instant::now() + STARTUP_TEST_TIMEOUT_DURATION;
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
                panic!("updated binary did not exit within {STARTUP_TEST_TIMEOUT_DURATION:?}");
            }
            Err(wait_error) => {
                let _ = started_process.kill();
                let _ = started_process.wait();
                panic!("wait for updated binary: {wait_error}");
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn resume_support_migrates_released_config_before_advertising_a_session_swap() {
    let test_directory = TempDir::new().expect("create test directory");
    let config_directory = resolve_test_config_directory(test_directory.path());
    fs::create_dir_all(&config_directory).expect("create config directory");
    let app_config_path = config_directory.join("koshi.kdl");
    fs::write(&app_config_path, "version 1\n").expect("write released config");

    let process_output = run_test_koshi_command(
        build_test_koshi_command(test_directory.path()).arg("resume-support"),
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
    assert_eq!(
        fs::read_to_string(app_config_path).expect("read migrated config"),
        "version 2\n"
    );
}

#[cfg(unix)]
#[test]
fn resume_support_accepts_current_config_without_a_migration_lock() {
    let test_directory = TempDir::new().expect("create test directory");
    let config_directory = resolve_test_config_directory(test_directory.path());
    fs::create_dir_all(&config_directory).expect("create config directory");
    let app_config_path = config_directory.join("koshi.kdl");
    fs::write(&app_config_path, "version 2\n").expect("write current config");
    fs::create_dir(config_directory.join(".migration.lock")).expect("block migration lock");

    let process_output = run_test_koshi_command(
        build_test_koshi_command(test_directory.path()).arg("resume-support"),
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
    assert_eq!(
        fs::read_to_string(app_config_path).expect("read unchanged config"),
        "version 2\n"
    );
}

#[cfg(unix)]
#[test]
fn process_entrypoints_refuse_a_failed_config_migration_before_serving() {
    let test_directory = TempDir::new().expect("create test directory");
    let config_directory = resolve_test_config_directory(test_directory.path());
    fs::create_dir_all(&config_directory).expect("create config directory");
    let app_config_path = config_directory.join("koshi.kdl");
    fs::write(&app_config_path, "version 1\n").expect("write released config");
    let migration_lock_path = config_directory.join(".migration.lock");
    fs::create_dir(&migration_lock_path).expect("block migration lock");
    let expected_open_error = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&migration_lock_path)
        .expect_err("a directory cannot be opened as a migration lock");
    let expected_standard_error = format!(
        "koshi: config failed: open {}: {expected_open_error}\n",
        migration_lock_path.display()
    );
    let runtime_directory = test_directory.path().join("runtime");
    let session_id = SessionId::new();
    let cli_arguments_by_process = [
        vec!["resume-support".to_string()],
        vec![
            "serve-router".to_string(),
            "--runtime-dir".to_string(),
            runtime_directory.display().to_string(),
        ],
        vec![
            "serve-session".to_string(),
            session_id.to_string(),
            "workspace".to_string(),
            "--runtime-dir".to_string(),
            runtime_directory.display().to_string(),
        ],
    ];

    for cli_arguments in cli_arguments_by_process {
        let process_output = run_test_koshi_command(
            build_test_koshi_command(test_directory.path()).args(cli_arguments),
        );
        assert_eq!(
            process_output.status.code(),
            Some(CliExitCode::UsageOrConfig.get_exit_code())
        );
        assert_eq!(process_output.stdout, b"");
        assert_eq!(process_output.stderr, expected_standard_error.as_bytes());
        assert_eq!(
            fs::read_to_string(&app_config_path).expect("read unchanged config"),
            "version 1\n"
        );
        assert!(!runtime_directory.exists());
    }
}
