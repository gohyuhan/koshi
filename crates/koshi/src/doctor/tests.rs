//! Tests for the `koshi doctor` checks: every branch of every check, the row
//! order, and the failure a run ends with.

use std::fs;
use std::time::Duration;

use koshi_ipc::remote_tokens::{TokenRecord, TokenScope};
use tempfile::TempDir;

use super::*;

/// A context whose every fact is known, rooted at `dir`. A test changes the
/// one field it is about and leaves the rest.
///
/// Creates no file and reads no environment variable. A test that needs
/// `<dir>/runtime`, `<dir>/config`, `<dir>/log`, `<dir>/config/plugins` or
/// `<dir>/shell` to be there creates it itself.
fn context(dir: &Path) -> Context {
    Context {
        config_dir: Some(dir.join("config")),
        runtime_dir: Some(dir.join("runtime")),
        runtime_mode: Some(0o700),
        log_dir: Some(dir.join("log")),
        plugins_dir: Some(dir.join("config").join("plugins")),
        shared_dir: Some(dir.join("shared")),
        shell: dir.join("shell"),
        shell_source: ShellSource::Environment,
        path: Some(dir.as_os_str().to_os_string()),
        term: Some("xterm-256color".to_string()),
        colorterm: None,
        allow_other_users: false,
        remote_listen: None,
        logging_on: false,
        grants: Ok(0),
        router: RemoteConnections::NotRunning,
    }
}

/// Create `path` as a file this machine can run: an empty file, mode `0755`
/// on Unix and the bare write on Windows.
fn write_runnable(path: &Path) {
    fs::write(path, "").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The config directory of `context`, created and empty.
fn config_dir(context: &Context) -> PathBuf {
    let dir = context.config_dir.clone().unwrap();
    fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------- config

#[test]
fn config_fails_when_this_machine_reports_no_home_directory() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.config_dir = None;

    assert_eq!(
        check_config(&context),
        Outcome {
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
    let temp = TempDir::new().unwrap();
    let context = context(temp.path());
    let dir = config_dir(&context);
    fs::write(dir.join("koshi.kdl"), "pane {}\n").unwrap();

    assert_eq!(
        check_config(&context),
        Outcome {
            verdict: Verdict::Fail,
            reason: format!(
                "invalid config version in {}: file must declare `version`",
                dir.join("koshi.kdl").display()
            ),
            help: Some("run koshi config check to see each file".to_string()),
            detail: None,
        }
    );
}

#[test]
fn config_is_ok_when_the_directory_holds_no_config_file() {
    let temp = TempDir::new().unwrap();
    let context = context(temp.path());
    let dir = config_dir(&context);

    assert_eq!(
        check_config(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: format!("no config file is present in {}", dir.display()),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn config_counts_one_validated_file_in_the_singular() {
    let temp = TempDir::new().unwrap();
    let context = context(temp.path());
    let dir = config_dir(&context);
    fs::write(dir.join("koshi.kdl"), "version 1\n").unwrap();

    assert_eq!(
        check_config(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: "1 config file validated".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn config_counts_two_validated_files_in_the_plural() {
    let temp = TempDir::new().unwrap();
    let context = context(temp.path());
    let dir = config_dir(&context);
    fs::write(dir.join("koshi.kdl"), "version 1\n").unwrap();
    fs::write(
        dir.join("keybinding.kdl"),
        "version 1\nmode \"normal\" {}\n",
    )
    .unwrap();

    assert_eq!(
        check_config(&context),
        Outcome {
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
    let temp = TempDir::new().unwrap();
    let context = context(temp.path());
    write_runnable(&context.shell);

    assert_eq!(
        check_shell(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: format!("a new pane runs {}", context.shell.display()),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn shell_warns_when_the_shell_comes_from_the_fallback() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.shell_source = ShellSource::Fallback;
    write_runnable(&context.shell);

    assert_eq!(
        check_shell(&context),
        Outcome {
            verdict: Verdict::Warn,
            reason: format!(
                "{SHELL_VAR} is not set, so a new pane runs {}",
                context.shell.display()
            ),
            help: Some(format!(
                "set {SHELL_VAR} to the shell you want a new pane to run"
            )),
            detail: None,
        }
    );
}

#[test]
fn shell_fails_when_the_named_shell_is_not_on_this_machine() {
    let temp = TempDir::new().unwrap();
    let context = context(temp.path());

    assert_eq!(
        check_shell(&context),
        Outcome {
            verdict: Verdict::Fail,
            reason: format!(
                "a new pane would run {}, which is not on this machine",
                context.shell.display()
            ),
            help: Some(format!("set {SHELL_VAR} to a shell that exists")),
            detail: None,
        }
    );
}

#[test]
fn shell_is_ok_when_koshi_kdl_names_a_shell_on_this_machine() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.shell_source = ShellSource::Config;
    write_runnable(&context.shell);

    assert_eq!(
        check_shell(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: format!(
                "koshi.kdl names {}, which a new pane runs",
                context.shell.display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn shell_fails_when_koshi_kdl_names_a_shell_that_is_not_on_this_machine() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.shell_source = ShellSource::Config;

    assert_eq!(
        check_shell(&context),
        Outcome {
            verdict: Verdict::Fail,
            reason: format!(
                "koshi.kdl names {}, which is not on this machine",
                context.shell.display()
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
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.shell = PathBuf::from("bare-shell");
    write_runnable(&temp.path().join("bare-shell"));

    assert_eq!(
        check_shell(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: "a new pane runs bare-shell".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn shell_named_without_a_directory_fails_when_path_is_unset() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.shell = PathBuf::from("bare-shell");
    context.path = None;
    write_runnable(&temp.path().join("bare-shell"));

    assert_eq!(
        check_shell(&context),
        Outcome {
            verdict: Verdict::Fail,
            reason: "a new pane would run bare-shell, which is not on this machine".to_string(),
            help: Some(format!("set {SHELL_VAR} to a shell that exists")),
            detail: None,
        }
    );
}

// -------------------------------------------------------------- terminal

/// The one help line every terminal branch carries.
const TERMINAL_HELP: &str = "set TERM before running koshi, for example TERM=xterm-256color";

#[test]
fn terminal_warns_when_term_is_unset() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.term = None;

    assert_eq!(
        check_terminal(&context),
        Outcome {
            verdict: Verdict::Warn,
            reason: "TERM is not set".to_string(),
            help: Some(TERMINAL_HELP.to_string()),
            detail: None,
        }
    );
}

#[test]
fn terminal_warns_when_term_is_dumb() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.term = Some("dumb".to_string());

    assert_eq!(
        check_terminal(&context),
        Outcome {
            verdict: Verdict::Warn,
            reason: "TERM is dumb, which names a terminal with no cursor control".to_string(),
            help: Some(TERMINAL_HELP.to_string()),
            detail: None,
        }
    );
}

#[test]
fn terminal_reports_term_without_colorterm() {
    let temp = TempDir::new().unwrap();
    let context = context(temp.path());

    assert_eq!(
        check_terminal(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: "TERM is xterm-256color, COLORTERM is not set".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn terminal_reports_term_with_colorterm() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.colorterm = Some("truecolor".to_string());

    assert_eq!(
        check_terminal(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: "TERM is xterm-256color, COLORTERM is truecolor".to_string(),
            help: None,
            detail: None,
        }
    );
}

// ----------------------------------------------------- runtime directory

/// The runtime directory of `context`, created and empty.
fn runtime_dir(context: &Context) -> PathBuf {
    let dir = context.runtime_dir.clone().unwrap();
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn runtime_directory_fails_when_this_machine_reports_no_home_directory() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.runtime_dir = None;

    assert_eq!(
        check_runtime_dir(&context),
        Outcome {
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
    let temp = TempDir::new().unwrap();
    let context = context(temp.path());

    assert_eq!(
        check_runtime_dir(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: format!(
                "{} does not exist yet; koshi creates it under {} when a session starts",
                context.runtime_dir.as_deref().unwrap().display(),
                temp.path().display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn runtime_directory_fails_when_it_cannot_be_read() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    let path = temp.path().join("runtime-file");
    fs::write(&path, "").unwrap();
    context.runtime_dir = Some(path.clone());
    let error = fs::read_dir(&path).unwrap_err();

    assert_eq!(
        check_runtime_dir(&context),
        Outcome {
            verdict: Verdict::Fail,
            reason: format!("{} cannot be read: {error}", path.display()),
            help: Some(format!("make sure you own {}", path.display())),
            detail: None,
        }
    );
}

#[test]
fn runtime_directory_fails_on_a_mode_other_than_700() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    let dir = runtime_dir(&context);
    context.runtime_mode = Some(0o755);

    assert_eq!(
        check_runtime_dir(&context),
        Outcome {
            verdict: Verdict::Fail,
            reason: format!(
                "{} has mode 755; koshi serves a session socket only from a directory with mode 700",
                dir.display()
            ),
            help: Some(format!("run chmod 700 {}", dir.display())),
            detail: None,
        }
    );
}

#[test]
fn runtime_directory_with_mode_700_is_ok() {
    let temp = TempDir::new().unwrap();
    let context = context(temp.path());
    let dir = runtime_dir(&context);

    assert_eq!(
        check_runtime_dir(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: format!("{} is ready", dir.display()),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn runtime_directory_says_nothing_about_the_router() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    let dir = runtime_dir(&context);
    let ready = Outcome {
        verdict: Verdict::Ok,
        reason: format!("{} is ready", dir.display()),
        help: None,
        detail: None,
    };

    for router in [
        RemoteConnections::Answered(Some(0)),
        RemoteConnections::Answered(None),
        RemoteConnections::NotRunning,
        RemoteConnections::OlderBuild,
        RemoteConnections::NoAnswer {
            detail: "connection refused".to_string(),
        },
    ] {
        context.router = router;
        assert_eq!(check_runtime_dir(&context), ready);
    }
}

// ---------------------------------------------------------------- router

#[test]
fn router_is_ok_when_one_answered() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.router = RemoteConnections::Answered(Some(2));

    assert_eq!(
        check_router(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: "a router answers on its control socket".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn router_is_ok_when_one_answered_without_a_count() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.router = RemoteConnections::Answered(None);

    assert_eq!(
        check_router(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: "a router answers on its control socket".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn router_is_ok_when_none_runs() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.router = RemoteConnections::NotRunning;

    assert_eq!(
        check_router(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: "no koshi is running".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn router_warns_on_an_older_build_and_does_not_fail_the_run() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.router = RemoteConnections::OlderBuild;

    assert_eq!(
        check_router(&context),
        Outcome {
            verdict: Verdict::Warn,
            reason: "the running router is an older koshi build".to_string(),
            help: Some("end every koshi process on this machine and start one again".to_string()),
            detail: None,
        }
    );
}

#[test]
fn router_fails_when_a_listening_router_did_not_answer_and_keeps_the_full_text() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.router = RemoteConnections::NoAnswer {
        detail: "connection refused".to_string(),
    };

    assert_eq!(
        check_router(&context),
        Outcome {
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
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.log_dir = None;

    assert_eq!(
        check_log_dir(&context),
        Outcome {
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
    let temp = TempDir::new().unwrap();
    let context = context(temp.path());

    assert_eq!(
        check_log_dir(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: format!(
                "{} does not exist yet; koshi creates it under {} when logging is on",
                context.log_dir.as_deref().unwrap().display(),
                temp.path().display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn log_directory_reports_logging_off_when_it_is_writable() {
    let temp = TempDir::new().unwrap();
    let context = context(temp.path());
    let dir = context.log_dir.clone().unwrap();
    fs::create_dir_all(&dir).unwrap();

    assert_eq!(
        check_log_dir(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: format!("{} is writable and logging is off", dir.display()),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn log_directory_reports_logging_on_when_it_is_writable() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    let dir = context.log_dir.clone().unwrap();
    fs::create_dir_all(&dir).unwrap();
    context.logging_on = true;

    assert_eq!(
        check_log_dir(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: format!("{} is writable and logging is on", dir.display()),
            help: None,
            detail: None,
        }
    );
}

/// The reason ends in ` at path "<name>"`, where `<name>` is the random file
/// name `tempfile` tried. The assertion covers everything before it.
#[test]
fn log_directory_fails_when_it_cannot_be_written() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    let path = temp.path().join("log-file");
    fs::write(&path, "").unwrap();
    context.log_dir = Some(path.clone());
    let probe = tempfile::NamedTempFile::new_in(&path)
        .unwrap_err()
        .to_string();
    let error = probe.split_once(" at path ").unwrap().0.to_string();

    let outcome = check_log_dir(&context);

    assert_eq!(outcome.verdict, Verdict::Fail);
    assert_eq!(
        outcome.help,
        Some(format!("make sure you own {}", path.display()))
    );
    assert_eq!(
        outcome.reason.split_once(" at path ").unwrap().0,
        format!("{} cannot be written: {error}", path.display())
    );
}

// ----------------------------------------------------- plugins directory

#[test]
fn plugins_directory_fails_when_this_machine_reports_no_home_directory() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.plugins_dir = None;

    assert_eq!(
        check_plugins_dir(&context),
        Outcome {
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
    let temp = TempDir::new().unwrap();
    let context = context(temp.path());

    assert_eq!(
        check_plugins_dir(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: format!(
                "{} does not exist",
                context.plugins_dir.as_deref().unwrap().display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn plugins_directory_is_ok_when_it_is_readable() {
    let temp = TempDir::new().unwrap();
    let context = context(temp.path());
    let dir = context.plugins_dir.clone().unwrap();
    fs::create_dir_all(&dir).unwrap();

    assert_eq!(
        check_plugins_dir(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: format!("{} is readable", dir.display()),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn plugins_directory_fails_when_it_cannot_be_read() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    let path = temp.path().join("plugins-file");
    fs::write(&path, "").unwrap();
    context.plugins_dir = Some(path.clone());
    let error = fs::read_dir(&path).unwrap_err();

    assert_eq!(
        check_plugins_dir(&context),
        Outcome {
            verdict: Verdict::Fail,
            reason: format!("{} cannot be read: {error}", path.display()),
            help: Some(format!("make sure you own {}", path.display())),
            detail: None,
        }
    );
}

// ----------------------------------------------------- session directory

#[test]
fn session_directory_without_other_users_and_without_a_home_directory() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.runtime_dir = None;

    assert_eq!(
        check_session_directory(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: "allow-other-users is off, so only you may reach your sessions".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn session_directory_without_other_users_names_the_runtime_directory_and_its_mode() {
    let temp = TempDir::new().unwrap();
    let context = context(temp.path());

    assert_eq!(
        check_session_directory(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: format!(
                "sessions are advertised in {} (mode 700), which only you may reach",
                context.runtime_dir.as_deref().unwrap().display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn session_directory_without_other_users_leaves_out_a_mode_it_does_not_know() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.runtime_mode = None;

    assert_eq!(
        check_session_directory(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: format!(
                "sessions are advertised in {}, which only you may reach",
                context.runtime_dir.as_deref().unwrap().display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn session_directory_with_other_users_names_the_shared_directory() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.allow_other_users = true;

    assert_eq!(
        check_session_directory(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: format!(
                "allow-other-users is on: sessions are also advertised in {}, which every user of this machine may reach",
                context.shared_dir.as_deref().unwrap().display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn session_directory_with_other_users_and_no_shared_directory() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.allow_other_users = true;
    context.shared_dir = None;

    assert_eq!(
        check_session_directory(&context),
        Outcome {
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
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());

    assert_eq!(
        check_remote_access(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason:
                "koshi.kdl names no remote listen address, and this machine holds 0 standing grants"
                    .to_string(),
            help: None,
            detail: None,
        }
    );

    context.grants = Ok(1);
    assert_eq!(
        check_remote_access(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason:
                "koshi.kdl names no remote listen address, and this machine holds 1 standing grant"
                    .to_string(),
            help: None,
            detail: None,
        }
    );

    context.grants = Ok(2);
    assert_eq!(
        check_remote_access(&context),
        Outcome {
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
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.remote_listen = Some("0.0.0.0:7777".to_string());

    assert_eq!(
        check_remote_access(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: "koshi.kdl names the remote listen address 0.0.0.0:7777, and this machine holds 0 standing grants"
                .to_string(),
            help: None,
            detail: None,
        }
    );

    context.grants = Ok(1);
    assert_eq!(
        check_remote_access(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: "koshi.kdl names the remote listen address 0.0.0.0:7777, and this machine holds 1 standing grant"
                .to_string(),
            help: None,
            detail: None,
        }
    );

    context.grants = Ok(2);
    assert_eq!(
        check_remote_access(&context),
        Outcome {
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
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.grants = Err("grants.json: unexpected end of file".to_string());

    assert_eq!(
        check_remote_access(&context),
        Outcome {
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
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.router = RemoteConnections::Answered(Some(0));

    assert_eq!(
        check_remote_connections(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: "this machine holds 0 open connections from another machine".to_string(),
            help: None,
            detail: None,
        }
    );

    context.router = RemoteConnections::Answered(Some(1));
    assert_eq!(
        check_remote_connections(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: "this machine holds 1 open connection from another machine".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn remote_connections_is_ok_when_no_router_runs() {
    let temp = TempDir::new().unwrap();
    let context = context(temp.path());

    assert_eq!(
        check_remote_connections(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: "no koshi is running, so nothing from another machine is connected".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn remote_connections_never_reads_a_missing_count_as_zero() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.router = RemoteConnections::Answered(None);

    assert_eq!(
        check_remote_connections(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: "the running router reports no count, so this is not known".to_string(),
            help: None,
            detail: None,
        }
    );
}

#[test]
fn remote_connections_reports_not_known_and_never_rates_a_router_that_did_not_answer() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    let not_known = Outcome {
        verdict: Verdict::Ok,
        reason: "the running router did not answer, so this is not known".to_string(),
        help: None,
        detail: None,
    };

    for router in [
        RemoteConnections::OlderBuild,
        RemoteConnections::NoAnswer {
            detail: "connection refused".to_string(),
        },
    ] {
        context.router = router;
        assert_eq!(check_remote_connections(&context), not_known);
    }
}

// -------------------------------------------------------------- postures

#[test]
fn no_posture_row_ever_fails() {
    let temp = TempDir::new().unwrap();

    let mut sessions = Vec::new();
    for allow_other_users in [false, true] {
        for shared in [None, Some(temp.path().join("shared"))] {
            for runtime in [None, Some(temp.path().join("runtime"))] {
                for runtime_mode in [None, Some(0o700), Some(0o755)] {
                    let mut context = context(temp.path());
                    context.allow_other_users = allow_other_users;
                    context.shared_dir = shared.clone();
                    context.runtime_dir = runtime.clone();
                    context.runtime_mode = runtime_mode;
                    sessions.push(context);
                }
            }
        }
    }
    for context in &sessions {
        assert_eq!(check_session_directory(context).verdict, Verdict::Ok);
    }

    for remote_listen in [None, Some("0.0.0.0:7777".to_string())] {
        for grants in [Ok(0), Ok(1), Ok(2), Err("unreadable".to_string())] {
            let mut context = context(temp.path());
            context.remote_listen = remote_listen.clone();
            let expected = if grants.is_ok() {
                Verdict::Ok
            } else {
                Verdict::Warn
            };
            context.grants = grants;
            assert_eq!(check_remote_access(&context).verdict, expected);
        }
    }

    for router in [
        RemoteConnections::Answered(Some(3)),
        RemoteConnections::Answered(None),
        RemoteConnections::NotRunning,
        RemoteConnections::OlderBuild,
        RemoteConnections::NoAnswer {
            detail: "connection refused".to_string(),
        },
    ] {
        let mut context = context(temp.path());
        context.router = router;
        assert_eq!(check_remote_connections(&context).verdict, Verdict::Ok);
    }
}

// --------------------------------------------------------------- helpers

#[test]
fn standing_grants_counts_only_the_grants_that_still_stand() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let record =
        |identity: &str, expires_at: Option<SystemTime>, revoked_at: Option<SystemTime>| {
            TokenRecord {
                identity: identity.to_string(),
                hash: "0".repeat(64),
                scope: TokenScope::HostWide,
                issued_at: SystemTime::UNIX_EPOCH,
                expires_at,
                last_used_at: None,
                revoked_at,
            }
        };
    let store = TokenStore {
        format: TokenStore::new().format,
        records: vec![
            record("live", None, None),
            record("revoked", None, Some(now)),
            record(
                "expired",
                Some(SystemTime::UNIX_EPOCH + Duration::from_secs(999)),
                None,
            ),
            record(
                "still-good",
                Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_001)),
                None,
            ),
        ],
    };

    assert_eq!(standing_grants(&store, now), 2);
}

#[test]
fn counted_puts_an_s_on_every_count_but_one() {
    assert_eq!(counted(0, "grant"), "0 grants");
    assert_eq!(counted(1, "grant"), "1 grant");
    assert_eq!(counted(2, "grant"), "2 grants");
}

#[test]
fn directory_mode_reads_the_permission_bits_on_unix_and_nothing_elsewhere() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path().join("private");
    fs::create_dir(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    }

    let mode = directory_mode(&dir);

    #[cfg(unix)]
    assert_eq!(mode, Some(0o700));
    #[cfg(not(unix))]
    assert_eq!(mode, None);
}

#[test]
fn directory_mode_leaves_out_the_sticky_setgid_and_setuid_bits() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path().join("sticky");
    fs::create_dir(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o1700)).unwrap();
    }

    let mode = directory_mode(&dir);

    #[cfg(unix)]
    assert_eq!(mode, Some(0o700));
    #[cfg(not(unix))]
    assert_eq!(mode, None);
}

#[test]
fn a_sticky_runtime_directory_passes_the_same_check_the_socket_bind_applies() {
    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    let dir = runtime_dir(&context);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o1700)).unwrap();
    }
    context.runtime_mode = directory_mode(&dir);

    assert_eq!(
        check_runtime_dir(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: format!("{} is ready", dir.display()),
            help: None,
            detail: None,
        }
    );
}

// ------------------------------------------------------------ rows and exit

#[test]
fn rows_run_every_check_in_print_order() {
    let temp = TempDir::new().unwrap();
    let context = context(temp.path());

    let names: Vec<&str> = rows(&context).iter().map(|row| row.name).collect();

    assert_eq!(
        names,
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

/// One row carrying `verdict`, named after it.
fn row(name: &'static str, verdict: Verdict) -> CheckRow {
    CheckRow {
        name,
        verdict,
        reason: "a fact".to_string(),
        help: None,
        detail: None,
    }
}

#[test]
fn a_run_with_no_failed_row_ends_well() {
    let rows = vec![row("config", Verdict::Ok), row("shell", Verdict::Ok)];

    assert!(failed(&rows).is_none());
}

#[test]
fn a_run_with_only_a_warning_ends_well() {
    let rows = vec![row("config", Verdict::Ok), row("terminal", Verdict::Warn)];

    assert!(failed(&rows).is_none());
}

#[test]
fn a_run_with_two_failed_rows_names_both() {
    let rows = vec![
        row("config", Verdict::Fail),
        row("terminal", Verdict::Warn),
        row("shell", Verdict::Fail),
    ];

    assert_eq!(failed(&rows).unwrap().to_string(), "2 checks failed");
}

#[test]
fn a_run_with_one_failed_row_names_one() {
    let rows = vec![row("config", Verdict::Fail), row("shell", Verdict::Ok)];

    assert_eq!(failed(&rows).unwrap().to_string(), "1 check failed");
}

// ---------------------------------------------------------------- one line

#[test]
fn one_line_leaves_text_that_holds_no_break_untouched() {
    assert_eq!(one_line("bad file".to_string()), "bad file");
}

#[test]
fn one_line_replaces_every_newline_and_carriage_return_with_a_space() {
    assert_eq!(one_line("bad\nfile\r\nhere".to_string()), "bad file  here");
}

#[test]
fn a_reason_carrying_a_newline_reaches_the_row_on_one_line() {
    let outcome = Outcome::fail("first\nsecond".to_string(), "do the thing");

    assert_eq!(
        outcome,
        Outcome {
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

    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.shell_source = ShellSource::Config;
    fs::write(&context.shell, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&context.shell, fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(
        check_shell(&context),
        Outcome {
            verdict: Verdict::Fail,
            reason: format!(
                "{} is on this machine and carries no execute bit",
                context.shell.display()
            ),
            help: Some(format!("run chmod +x {}", context.shell.display())),
            detail: None,
        }
    );
}

#[cfg(unix)]
#[test]
fn a_runnable_shell_later_in_path_wins_over_one_that_carries_no_execute_bit() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    fs::write(first.join("bare-shell"), "").unwrap();
    fs::set_permissions(first.join("bare-shell"), fs::Permissions::from_mode(0o644)).unwrap();
    write_runnable(&second.join("bare-shell"));

    let mut context = context(temp.path());
    context.shell = PathBuf::from("bare-shell");
    context.path = Some(std::env::join_paths([&first, &second]).unwrap());

    assert_eq!(
        check_shell(&context),
        Outcome {
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

    let temp = TempDir::new().unwrap();
    let sealed = temp.path().join("sealed");
    fs::create_dir_all(&sealed).unwrap();
    let mut context = context(temp.path());
    context.runtime_dir = Some(sealed.join("run"));
    fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000)).unwrap();

    let outcome = check_runtime_dir(&context);

    fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        outcome,
        Outcome {
            verdict: Verdict::Fail,
            reason: format!(
                "{} does not exist and koshi cannot create it: nothing new can be written in {}",
                sealed.join("run").display(),
                sealed.display()
            ),
            help: Some(format!("make sure you can write in {}", sealed.display())),
            detail: None,
        }
    );
}

#[cfg(unix)]
#[test]
fn runtime_directory_fails_when_a_name_above_it_points_nowhere() {
    let temp = TempDir::new().unwrap();
    let parent = temp.path().join("parent");
    fs::create_dir_all(&parent).unwrap();
    let link = parent.join("link");
    std::os::unix::fs::symlink(temp.path().join("nowhere"), &link).unwrap();
    let mut context = context(temp.path());
    context.runtime_dir = Some(link.join("koshi"));

    assert_eq!(
        check_runtime_dir(&context),
        Outcome {
            verdict: Verdict::Fail,
            reason: format!(
                "{} does not exist and koshi cannot create it: nothing new can be written in {}",
                link.join("koshi").display(),
                link.display()
            ),
            help: Some(format!("make sure you can write in {}", link.display())),
            detail: None,
        }
    );
}

#[cfg(unix)]
#[test]
fn runtime_directory_is_ok_under_a_symlink_that_points_at_a_real_directory() {
    let temp = TempDir::new().unwrap();
    let real = temp.path().join("real");
    let parent = temp.path().join("parent");
    fs::create_dir_all(&real).unwrap();
    fs::create_dir_all(&parent).unwrap();
    let link = parent.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let mut context = context(temp.path());
    context.runtime_dir = Some(link.join("koshi"));

    assert_eq!(
        check_runtime_dir(&context),
        Outcome {
            verdict: Verdict::Ok,
            reason: format!(
                "{} does not exist yet; koshi creates it under {} when a session starts",
                link.join("koshi").display(),
                link.display()
            ),
            help: None,
            detail: None,
        }
    );
}

#[cfg(unix)]
#[test]
fn runtime_directory_fails_when_the_directory_itself_points_nowhere() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path().join("koshi");
    std::os::unix::fs::symlink(temp.path().join("nowhere"), &dir).unwrap();
    let mut context = context(temp.path());
    context.runtime_dir = Some(dir.clone());

    assert_eq!(
        check_runtime_dir(&context),
        Outcome {
            verdict: Verdict::Fail,
            reason: format!(
                "{} is a name koshi cannot make a directory at",
                dir.display()
            ),
            help: Some(format!(
                "remove {}, or point it at a directory",
                dir.display()
            )),
            detail: None,
        }
    );
}

#[cfg(unix)]
#[test]
fn runtime_directory_fails_when_the_directory_points_at_a_regular_file() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("afile");
    fs::write(&file, "").unwrap();
    let dir = temp.path().join("koshi");
    std::os::unix::fs::symlink(&file, &dir).unwrap();
    let mut context = context(temp.path());
    context.runtime_dir = Some(dir.join("run"));

    assert_eq!(
        check_runtime_dir(&context).verdict,
        Verdict::Fail,
        "a runtime directory under a link to a regular file must not report ok"
    );
}

#[cfg(unix)]
#[test]
fn runtime_directory_fails_on_a_symlink_loop() {
    let temp = TempDir::new().unwrap();
    let one = temp.path().join("one");
    let two = temp.path().join("two");
    std::os::unix::fs::symlink(&two, &one).unwrap();
    std::os::unix::fs::symlink(&one, &two).unwrap();
    let mut context = context(temp.path());
    context.runtime_dir = Some(one.join("koshi"));

    assert_eq!(
        check_runtime_dir(&context).verdict,
        Verdict::Fail,
        "a runtime directory under a symlink loop must not report ok"
    );
}

#[cfg(unix)]
#[test]
fn runtime_directory_fails_when_a_name_above_it_is_a_regular_file() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("afile");
    fs::write(&file, "").unwrap();
    let mut context = context(temp.path());
    context.runtime_dir = Some(file.join("koshi"));

    assert_eq!(
        check_runtime_dir(&context).verdict,
        Verdict::Fail,
        "a runtime directory under a regular file must not report ok"
    );
}

#[cfg(unix)]
#[test]
fn shell_fails_when_the_execute_bits_do_not_apply_to_this_user() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    let mut context = context(temp.path());
    context.shell_source = ShellSource::Config;
    fs::write(&context.shell, "#!/bin/sh\n").unwrap();
    // Mode 011 on a file this user owns: the execute bits sit on group and
    // other, and the owner bits are the ones that apply.
    fs::set_permissions(&context.shell, fs::Permissions::from_mode(0o011)).unwrap();

    assert_eq!(
        check_shell(&context),
        Outcome {
            verdict: Verdict::Fail,
            reason: format!(
                "{} is on this machine and carries no execute bit",
                context.shell.display()
            ),
            help: Some(format!("run chmod +x {}", context.shell.display())),
            detail: None,
        }
    );
}

#[cfg(unix)]
#[test]
fn log_directory_fails_when_it_points_nowhere() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path().join("logs");
    std::os::unix::fs::symlink(temp.path().join("nowhere"), &dir).unwrap();
    let mut context = context(temp.path());
    context.log_dir = Some(dir.clone());

    assert_eq!(
        check_log_dir(&context),
        Outcome {
            verdict: Verdict::Fail,
            reason: format!(
                "{} is a name koshi cannot make a directory at",
                dir.display()
            ),
            help: Some(format!(
                "remove {}, or point it at a directory",
                dir.display()
            )),
            detail: None,
        }
    );
}

#[cfg(unix)]
#[test]
fn plugins_directory_fails_when_it_points_nowhere() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path().join("plugins");
    std::os::unix::fs::symlink(temp.path().join("nowhere"), &dir).unwrap();
    let mut context = context(temp.path());
    context.plugins_dir = Some(dir.clone());

    assert_eq!(
        check_plugins_dir(&context),
        Outcome {
            verdict: Verdict::Fail,
            reason: format!(
                "{} is there and koshi cannot read it as a directory",
                dir.display()
            ),
            help: Some(format!(
                "remove {}, or point it at a directory",
                dir.display()
            )),
            detail: None,
        }
    );
}
