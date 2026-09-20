//! `koshi doctor`: check this machine's koshi installation and print one row
//! per check.
//!
//! Every fact the checks read is gathered once into a
//! [`crate::doctor::DoctorContext`] — the platform directories, `koshi.kdl`, the
//! environment variables a pane inherits, the remote access grant file, and
//! the running router. The checks themselves read only that context and touch
//! the filesystem only where a check names a file operation of its own.
//!
//! The checks run in print order, each one a name and a function. A check
//! answers [`crate::doctor::Verdict::Ok`], [`crate::doctor::Verdict::Warn`] or
//! [`crate::doctor::Verdict::Fail`] with the fact behind it and what to do
//! about it. The whole table reaches stdout before [`crate::doctor::run_doctor_checks`]
//! returns, and a run with any [`crate::doctor::Verdict::Fail`] row ends in
//! [`koshi_link::error::CliError::Runtime`].

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use koshi_core::process::SpawnSpec;
use koshi_ipc::remote_tokens::{resolve_token_store_path, TokenStore};
use serde::Serialize;

use crate::cli::OutputFormat;
use crate::output;
use koshi_link::error::CliError;
use koshi_link::in_session::InSessionContext;
use koshi_link::router_client::{query_running_router_remote_connections, RemoteConnections};
use koshi_observability::logging::session_log_path;
use koshi_paths::RuntimeDirectoryRule;

/// What one check concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Verdict {
    /// The check found what it looks for.
    Ok,
    /// The check found something that still works and is worth reading.
    Warn,
    /// The check found something koshi cannot work through.
    Fail,
}

/// One check's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorOutcome {
    /// What the check concluded.
    pub verdict: Verdict,
    /// The fact behind the verdict, on one line and holding no newline.
    pub reason: String,
    /// What to do about it, or `None` when there is nothing to do.
    pub help: Option<String>,
    /// The full text behind a shortened `reason`, or `None` when `reason`
    /// already says the whole thing. `--format json` prints it and the table
    /// leaves it out.
    pub detail: Option<String>,
}

impl DoctorOutcome {
    /// A [`Verdict::Ok`] answer carrying `reason`, no help and no detail.
    fn build_success_outcome(reason: String) -> DoctorOutcome {
        DoctorOutcome {
            verdict: Verdict::Ok,
            reason: format_single_line(reason),
            help: None,
            detail: None,
        }
    }

    /// A [`Verdict::Warn`] answer carrying `reason` and `help`, and no detail.
    fn build_warning_outcome(reason: String, help: &str) -> DoctorOutcome {
        DoctorOutcome {
            verdict: Verdict::Warn,
            reason: format_single_line(reason),
            help: Some(help.to_string()),
            detail: None,
        }
    }

    /// A [`Verdict::Fail`] answer carrying `reason` and `help`, and no detail.
    fn build_failure_outcome(reason: String, help: &str) -> DoctorOutcome {
        DoctorOutcome {
            verdict: Verdict::Fail,
            reason: format_single_line(reason),
            help: Some(help.to_string()),
            detail: None,
        }
    }

    /// The same answer carrying `detail` as the full text behind its `reason`.
    fn with_detail(mut self, detail: String) -> DoctorOutcome {
        self.detail = Some(detail);
        self
    }
}

/// One check: what it is called, and the function that runs it.
struct DoctorCheck {
    /// The name printed in the `check` column, e.g. `"runtime directory"`.
    check_name: &'static str,
    /// Runs the check against the gathered context.
    run_check: fn(&DoctorContext) -> DoctorOutcome,
}

/// Every check `koshi doctor` runs, in print order.
const DOCTOR_CHECKS: &[DoctorCheck] = &[
    DoctorCheck {
        check_name: "config",
        run_check: check_config,
    },
    DoctorCheck {
        check_name: "shell",
        run_check: check_shell,
    },
    DoctorCheck {
        check_name: "terminal",
        run_check: check_terminal,
    },
    DoctorCheck {
        check_name: "runtime directory",
        run_check: check_runtime_directory,
    },
    DoctorCheck {
        check_name: "log directory",
        run_check: check_log_directory,
    },
    DoctorCheck {
        check_name: "session log file",
        run_check: check_session_log_file,
    },
    DoctorCheck {
        check_name: "plugins directory",
        run_check: check_plugins_directory,
    },
    DoctorCheck {
        check_name: "router",
        run_check: check_router,
    },
    DoctorCheck {
        check_name: "session directory",
        run_check: check_session_directory,
    },
    DoctorCheck {
        check_name: "remote access",
        run_check: check_remote_access,
    },
    DoctorCheck {
        check_name: "remote connections",
        run_check: check_remote_connections,
    },
];

/// The environment variable naming the program a new pane runs: `"SHELL"` on
/// Unix, `"COMSPEC"` on Windows.
#[cfg(not(windows))]
const SHELL_ENVIRONMENT_VARIABLE: &str = "SHELL";
#[cfg(windows)]
const SHELL_ENVIRONMENT_VARIABLE: &str = "COMSPEC";

/// Where the program a new pane runs was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellSource {
    /// `koshi.kdl`'s `terminal.default-shell`.
    Config,
    /// `SHELL` (Unix) or `COMSPEC` (Windows), set and not empty.
    Environment,
    /// The built-in default, `/bin/sh` on Unix and `cmd.exe` on Windows.
    Fallback,
}

/// Everything the checks read, gathered once.
pub struct DoctorContext {
    /// The directory `koshi.kdl` lives in, or `None` when this machine
    /// reports no home directory.
    pub config_directory: Option<PathBuf>,
    /// The private runtime directory holding the endpoint files, or `None`
    /// when this machine reports no home directory.
    /// [`DoctorContext::runtime_directory_rule`] names the rule that produced it.
    pub runtime_directory: Option<PathBuf>,
    /// The rule that produced [`DoctorContext::runtime_directory`], or `None` when this
    /// machine reports no home directory.
    pub runtime_directory_rule: Option<RuntimeDirectoryRule>,
    /// The runtime directory's permission bits, `None` on Windows, when this
    /// machine reports no home directory, and when the directory could not be
    /// read.
    pub runtime_directory_mode: Option<u32>,
    /// The directory koshi writes its log files in, or `None` when this
    /// machine reports no home directory.
    pub log_directory: Option<PathBuf>,
    /// The log file of the session this command runs inside, read from
    /// `KOSHI_SESSION_ID`; `Ok(None)` when the command runs outside a pane;
    /// the message naming what is wrong when `KOSHI` is set and the rest of
    /// the in-session identity is missing or malformed.
    pub session_log_file: Result<Option<PathBuf>, String>,
    /// `plugins` under the config directory, or `None` when this machine
    /// reports no home directory.
    pub plugins_directory: Option<PathBuf>,
    /// The machine-wide directory the shared session sockets live in:
    /// `koshi.kdl`'s `shared-sessions-dir` when it names one, else this
    /// platform's own. `None` when neither names one.
    pub shared_directory: Option<PathBuf>,
    /// The program a new pane runs.
    pub shell: PathBuf,
    /// Where [`DoctorContext::shell`] was read from.
    pub shell_source: ShellSource,
    /// `PATH`, used to find a shell named without a directory.
    pub path_entries: Option<OsString>,
    /// `TERM`, or `None` when it is unset or empty.
    pub term: Option<String>,
    /// `COLORTERM`, or `None` when it is unset or empty.
    pub colorterm: Option<String>,
    /// `koshi.kdl`'s `allow-other-users`.
    pub is_other_user_access_allowed: bool,
    /// `koshi.kdl`'s `remote-listen` address, or `None` when it names none.
    pub remote_listen: Option<String>,
    /// `koshi.kdl`'s `logging.enabled`.
    pub is_logging_enabled: bool,
    /// How many remote access grants still stand, or the message naming why
    /// they could not be read.
    pub standing_grant_count: Result<usize, String>,
    /// What asking the running router produced.
    pub router_connections: RemoteConnections,
}

impl DoctorContext {
    /// Read every fact the checks need from this machine: the platform
    /// directories, the rule that produced the runtime directory, `koshi.kdl`,
    /// the environment, the grant file, and the running router. Creates
    /// nothing and starts no router.
    #[must_use]
    pub fn from_current_machine() -> DoctorContext {
        let config_directory = koshi_paths::resolve_config_directory();
        let (runtime_directory, runtime_directory_rule) =
            koshi_paths::resolve_runtime_directory_with_rule().unzip();
        let runtime_directory_mode = runtime_directory.as_deref().and_then(read_directory_mode);
        let plugins_directory = config_directory
            .as_ref()
            .map(|config_directory| config_directory.join("plugins"));
        let server_config = koshi_link::config::load_current_server_config();
        let shared_directory = server_config
            .shared_sessions_directory
            .clone()
            .or_else(koshi_paths::resolve_shared_sessions_directory);
        let standing_grant_count = match koshi_paths::resolve_data_directory() {
            Some(data_directory) => {
                match TokenStore::load_token_store_from_path(&resolve_token_store_path(
                    &data_directory,
                )) {
                    Ok(token_store) => Ok(count_standing_grants(&token_store, SystemTime::now())),
                    Err(store_error) => Err(store_error.to_string()),
                }
            }
            None => Err("this machine reports no home directory".to_string()),
        };
        let router_connections = match runtime_directory.as_deref() {
            Some(runtime_directory) => query_running_router_remote_connections(runtime_directory),
            None => RemoteConnections::NotRunning,
        };
        DoctorContext {
            config_directory,
            runtime_directory,
            runtime_directory_rule,
            runtime_directory_mode,
            log_directory: koshi_observability::logging::resolve_log_directory(),
            session_log_file: InSessionContext::from_env()
                .map(|in_session_context| {
                    in_session_context
                        .map(|in_session_context| session_log_path(in_session_context.session_id))
                })
                .map_err(|in_session_error| in_session_error.to_string()),
            plugins_directory,
            shared_directory,
            shell: match &server_config.terminal.default_shell {
                Some(program) => PathBuf::from(program),
                None => SpawnSpec::default_shell(None, BTreeMap::new()).program,
            },
            shell_source: match &server_config.terminal.default_shell {
                Some(_) => ShellSource::Config,
                None if std::env::var_os(SHELL_ENVIRONMENT_VARIABLE)
                    .is_some_and(|environment_value| !environment_value.is_empty()) =>
                {
                    ShellSource::Environment
                }
                None => ShellSource::Fallback,
            },
            path_entries: std::env::var_os("PATH"),
            term: std::env::var_os("TERM")
                .and_then(|environment_value| environment_value.into_string().ok())
                .filter(|environment_value| !environment_value.is_empty()),
            colorterm: std::env::var_os("COLORTERM")
                .and_then(|environment_value| environment_value.into_string().ok())
                .filter(|environment_value| !environment_value.is_empty()),
            is_other_user_access_allowed: server_config.should_allow_other_users,
            remote_listen: server_config.remote_listen,
            is_logging_enabled: server_config.logging.is_enabled,
            standing_grant_count,
            router_connections,
        }
    }
}

/// One row of the answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorCheckRow {
    /// The check's name.
    pub check_name: &'static str,
    /// What the check concluded. `--format json` prints its fields beside
    /// `check_name` in the same object.
    #[serde(flatten)]
    pub outcome: DoctorOutcome,
}

/// Run every check against `doctor_context`, in print order.
#[must_use]
pub fn build_doctor_check_rows(doctor_context: &DoctorContext) -> Vec<DoctorCheckRow> {
    DOCTOR_CHECKS
        .iter()
        .map(|doctor_check| DoctorCheckRow {
            check_name: doctor_check.check_name,
            outcome: (doctor_check.run_check)(doctor_context),
        })
        .collect()
}

/// Check this machine's koshi installation and print one row per check.
///
/// # Errors
/// [`CliError::Runtime`] when any row is [`Verdict::Fail`], after the whole
/// answer has been printed. A [`Verdict::Warn`] row never fails the run.
pub fn run_doctor_checks(output_format: OutputFormat) -> Result<(), CliError> {
    let doctor_context = DoctorContext::from_current_machine();
    let check_rows = build_doctor_check_rows(&doctor_context);
    print!("{}", output::render_doctor(&check_rows, output_format));
    build_failure_outcome_from_check_rows(&check_rows).map_or(Ok(()), Err)
}

/// The failure a run ends with when any row failed, naming how many did, or
/// `None` when every row is ok or warn.
fn build_failure_outcome_from_check_rows(check_rows: &[DoctorCheckRow]) -> Option<CliError> {
    let failed_check_count = check_rows
        .iter()
        .filter(|check_row| check_row.outcome.verdict == Verdict::Fail)
        .count();
    if failed_check_count == 0 {
        return None;
    }
    Some(CliError::Runtime {
        detail: format!(
            "{} failed",
            format_counted_noun(failed_check_count, "check")
        ),
    })
}

fn check_config(doctor_context: &DoctorContext) -> DoctorOutcome {
    let Some(config_directory) = doctor_context.config_directory.as_deref() else {
        return build_no_home_directory_outcome("config");
    };
    let config_report = crate::config_command::validate_config_directory(config_directory);
    if !config_report.validation_errors.is_empty() {
        return DoctorOutcome::build_failure_outcome(
            config_report.validation_errors.join("; "),
            "run koshi config check to see each file",
        );
    }
    if config_report.report_lines.is_empty() {
        return DoctorOutcome::build_success_outcome(format!(
            "no config file is present in {}",
            config_directory.display()
        ));
    }
    DoctorOutcome::build_success_outcome(format!(
        "{} validated",
        format_counted_noun(config_report.report_lines.len(), "config file")
    ))
}

fn check_shell(doctor_context: &DoctorContext) -> DoctorOutcome {
    let shell_path = doctor_context.shell.display();
    match inspect_shell_program(&doctor_context.shell, doctor_context.path_entries.as_ref()) {
        ShellAvailability::NotExecutable => DoctorOutcome::build_failure_outcome(
            format!("{shell_path} is on this machine and carries no execute bit"),
            &format!("run chmod +x {shell_path}"),
        ),
        ShellAvailability::Missing => match doctor_context.shell_source {
            ShellSource::Config => DoctorOutcome::build_failure_outcome(
                format!("koshi.kdl names {shell_path}, which is not on this machine"),
                "set terminal.default-shell in koshi.kdl to a shell that exists",
            ),
            ShellSource::Environment | ShellSource::Fallback => {
                DoctorOutcome::build_failure_outcome(
                    format!("a new pane would run {shell_path}, which is not on this machine"),
                    &format!("set {SHELL_ENVIRONMENT_VARIABLE} to a shell that exists"),
                )
            }
        },
        ShellAvailability::Runnable => match doctor_context.shell_source {
            ShellSource::Config => DoctorOutcome::build_success_outcome(format!(
                "koshi.kdl names {shell_path}, which a new pane runs"
            )),
            ShellSource::Environment => {
                DoctorOutcome::build_success_outcome(format!("a new pane runs {shell_path}"))
            }
            ShellSource::Fallback => DoctorOutcome::build_warning_outcome(
                format!("{SHELL_ENVIRONMENT_VARIABLE} is not set, so a new pane runs {shell_path}"),
                &format!(
                    "set {SHELL_ENVIRONMENT_VARIABLE} to the shell you want a new pane to run"
                ),
            ),
        },
    }
}

fn check_terminal(doctor_context: &DoctorContext) -> DoctorOutcome {
    let help = "set TERM before running koshi, for example TERM=xterm-256color";
    match doctor_context.term.as_deref() {
        None => DoctorOutcome::build_warning_outcome("TERM is not set".to_string(), help),
        Some("dumb") => DoctorOutcome::build_warning_outcome(
            "TERM is dumb, which names a terminal with no cursor control".to_string(),
            help,
        ),
        Some(term) => DoctorOutcome::build_success_outcome(format!(
            "TERM is {term}, COLORTERM is {}",
            doctor_context.colorterm.as_deref().unwrap_or("not set")
        )),
    }
}

/// The runtime directory check: what state the directory is in, and after a
/// `; ` the rule that produced its path.
///
/// `/tmp/koshi-501` ready, named by `KOSHI_RUNTIME_DIR`, gives the reason
/// `"/tmp/koshi-501 is ready; KOSHI_RUNTIME_DIR names it"`.
fn check_runtime_directory(doctor_context: &DoctorContext) -> DoctorOutcome {
    let (Some(runtime_directory), Some(runtime_directory_rule)) = (
        doctor_context.runtime_directory.as_deref(),
        doctor_context.runtime_directory_rule,
    ) else {
        return build_no_home_directory_outcome("runtime");
    };
    let mut check_outcome =
        inspect_runtime_directory(runtime_directory, doctor_context.runtime_directory_mode);
    check_outcome.reason = format_single_line(format!(
        "{}; {}",
        check_outcome.reason,
        format_runtime_directory_rule(runtime_directory_rule)
    ));
    check_outcome
}

fn check_router(doctor_context: &DoctorContext) -> DoctorOutcome {
    match &doctor_context.router_connections {
        RemoteConnections::Answered(_) => DoctorOutcome::build_success_outcome(
            "a router answers on its control socket".to_string(),
        ),
        RemoteConnections::NotRunning => {
            DoctorOutcome::build_success_outcome("no koshi is running".to_string())
        }
        RemoteConnections::OlderBuild => DoctorOutcome::build_warning_outcome(
            "the running router is an older koshi build".to_string(),
            "end every koshi process on this machine and start one again",
        ),
        RemoteConnections::NoAnswer {
            error_detail: router_error_detail,
        } => DoctorOutcome::build_failure_outcome(
            "a router is listening and did not answer".to_string(),
            "end every koshi process on this machine and start one again",
        )
        .with_detail(router_error_detail.clone()),
    }
}

fn check_log_directory(doctor_context: &DoctorContext) -> DoctorOutcome {
    let Some(log_directory) = doctor_context.log_directory.as_deref() else {
        return DoctorOutcome::build_warning_outcome(
            "this machine reports no home directory, so a log file lands in whichever directory koshi is started from"
                .to_string(),
            "give this user a home directory",
        );
    };
    let displayed_path = log_directory.display();
    if !log_directory.exists() {
        return build_absent_directory_outcome(log_directory, "when logging is on");
    }
    match tempfile::NamedTempFile::new_in(log_directory) {
        Ok(writable_file_probe) => {
            drop(writable_file_probe);
            DoctorOutcome::build_success_outcome(format!(
                "{displayed_path} is writable and logging is {}",
                if doctor_context.is_logging_enabled {
                    "on"
                } else {
                    "off"
                }
            ))
        }
        Err(log_directory_error) => DoctorOutcome::build_failure_outcome(
            format!("{displayed_path} cannot be written: {log_directory_error}"),
            &format!("make sure you own {displayed_path}"),
        ),
    }
}

/// Name the log file this session writes. Inside a pane the exact path;
/// outside one the `koshi-log-<session-id>.log` shape; a `Warn` when `KOSHI`
/// is set and the rest of the in-session identity is missing or malformed.
fn check_session_log_file(doctor_context: &DoctorContext) -> DoctorOutcome {
    match &doctor_context.session_log_file {
        Ok(Some(session_log_file)) => DoctorOutcome::build_success_outcome(format!(
            "this session writes {}",
            session_log_file.display()
        )),
        Ok(None) => DoctorOutcome::build_success_outcome(
            "not inside a koshi pane; each session writes koshi-log-<session-id>.log in the log directory, and koshi list-sessions names the id"
                .to_string(),
        ),
        Err(in_session_error) => DoctorOutcome::build_warning_outcome(
            format!("the session's log file cannot be named: {in_session_error}"),
            "run koshi doctor from a shell koshi started, or unset KOSHI",
        ),
    }
}

fn check_plugins_directory(doctor_context: &DoctorContext) -> DoctorOutcome {
    let Some(plugins_directory) = doctor_context.plugins_directory.as_deref() else {
        return build_no_home_directory_outcome("plugins");
    };
    let displayed_path = plugins_directory.display();
    if !plugins_directory.exists() {
        return match std::fs::symlink_metadata(plugins_directory) {
            Ok(_) => DoctorOutcome::build_failure_outcome(
                format!("{displayed_path} is there and koshi cannot read it as a directory"),
                &format!("remove {displayed_path}, or point it at a directory"),
            ),
            Err(_) => {
                DoctorOutcome::build_success_outcome(format!("{displayed_path} does not exist"))
            }
        };
    }
    if let Err(plugins_directory_error) = std::fs::read_dir(plugins_directory) {
        return DoctorOutcome::build_failure_outcome(
            format!("{displayed_path} cannot be read: {plugins_directory_error}"),
            &format!("make sure you own {displayed_path}"),
        );
    }
    DoctorOutcome::build_success_outcome(format!("{displayed_path} is readable"))
}

fn check_session_directory(doctor_context: &DoctorContext) -> DoctorOutcome {
    if !doctor_context.is_other_user_access_allowed {
        let Some(runtime_directory) = doctor_context.runtime_directory.as_deref() else {
            return DoctorOutcome::build_success_outcome(
                "allow-other-users is off, so only you may reach your sessions".to_string(),
            );
        };
        let runtime_directory_description = match doctor_context.runtime_directory_mode {
            Some(directory_mode) => format!(
                "{} (mode {directory_mode:03o})",
                runtime_directory.display()
            ),
            None => runtime_directory.display().to_string(),
        };
        return DoctorOutcome::build_success_outcome(format!(
            "sessions are advertised in {runtime_directory_description}, which only you may reach"
        ));
    }
    match doctor_context.shared_directory.as_deref() {
        Some(shared_directory) => DoctorOutcome::build_success_outcome(format!(
            "allow-other-users is on: sessions are also advertised in {}, which every user of this machine may reach",
            shared_directory.display()
        )),
        None => DoctorOutcome::build_success_outcome(
            "allow-other-users is on, and this machine names no shared session directory, so no other user reaches your sessions"
                .to_string(),
        ),
    }
}

fn check_remote_access(doctor_context: &DoctorContext) -> DoctorOutcome {
    let remote_listen_description = match doctor_context.remote_listen.as_deref() {
        Some(remote_listen_address) => {
            format!("koshi.kdl names the remote listen address {remote_listen_address}")
        }
        None => "koshi.kdl names no remote listen address".to_string(),
    };
    match &doctor_context.standing_grant_count {
        Ok(standing_grant_count) => DoctorOutcome::build_success_outcome(format!(
            "{remote_listen_description}, and this machine holds {}",
            format_counted_noun(*standing_grant_count, "standing grant")
        )),
        Err(grant_read_error) => DoctorOutcome::build_warning_outcome(
            format!(
                "{remote_listen_description}, and the grants could not be read: {grant_read_error}"
            ),
            "make sure you own the koshi data directory",
        ),
    }
}

fn check_remote_connections(doctor_context: &DoctorContext) -> DoctorOutcome {
    match &doctor_context.router_connections {
        RemoteConnections::Answered(Some(remote_connection_count)) => {
            DoctorOutcome::build_success_outcome(format!(
                "this machine holds {} from another machine",
                format_counted_noun(*remote_connection_count, "open connection")
            ))
        }
        RemoteConnections::Answered(None) => DoctorOutcome::build_success_outcome(
            "the running router reports no count, so this is not known".to_string(),
        ),
        RemoteConnections::NotRunning => DoctorOutcome::build_success_outcome(
            "no koshi is running, so nothing from another machine is connected".to_string(),
        ),
        RemoteConnections::OlderBuild | RemoteConnections::NoAnswer { .. } => {
            DoctorOutcome::build_success_outcome(
                "the running router did not answer, so this is not known".to_string(),
            )
        }
    }
}

/// What state the runtime directory `runtime_directory` is in, with
/// `runtime_directory_mode` its permission bits and `None` where they are not
/// known.
///
/// Ok when `runtime_directory` holds mode 700, when `runtime_directory_mode` is
/// `None`, and when `runtime_directory` is not there yet and koshi can create
/// it. Fail when `runtime_directory` cannot be read, when
/// `runtime_directory_mode` is anything other than 700, and when
/// `runtime_directory` is not there and koshi cannot create it.
fn inspect_runtime_directory(
    runtime_directory: &Path,
    runtime_directory_mode: Option<u32>,
) -> DoctorOutcome {
    let displayed_path = runtime_directory.display();
    if !runtime_directory.exists() {
        return build_absent_directory_outcome(runtime_directory, "when a session starts");
    }
    if let Err(runtime_directory_error) = std::fs::read_dir(runtime_directory) {
        return DoctorOutcome::build_failure_outcome(
            format!("{displayed_path} cannot be read: {runtime_directory_error}"),
            &format!("make sure you own {displayed_path}"),
        );
    }
    if let Some(permission_mode) = runtime_directory_mode {
        if permission_mode != 0o700 {
            return DoctorOutcome::build_failure_outcome(
                format!(
                    "{displayed_path} has mode {permission_mode:03o}; koshi serves a session socket only from a directory with mode 700"
                ),
                &format!("run chmod 700 {displayed_path}"),
            );
        }
    }
    DoctorOutcome::build_success_outcome(format!("{displayed_path} is ready"))
}

/// `runtime_directory_rule` in words, holding no newline:
/// [`RuntimeDirectoryRule::EnvironmentVariable`] gives `"KOSHI_RUNTIME_DIR names it"`.
fn format_runtime_directory_rule(runtime_directory_rule: RuntimeDirectoryRule) -> &'static str {
    match runtime_directory_rule {
        RuntimeDirectoryRule::EnvironmentVariable => "KOSHI_RUNTIME_DIR names it",
        RuntimeDirectoryRule::UserId => "koshi names it after your user id",
        RuntimeDirectoryRule::DataDirectory => {
            "koshi puts it under your application data directory"
        }
    }
}

/// The answer for a directory koshi makes for itself that is not there yet.
///
/// `creation_event` names the moment koshi makes it, such as `"when a session
/// starts"`.
///
/// Ok when a directory can be made at `directory_path`, naming what it goes under. Fail
/// naming what stops it: `directory_path` itself when that name is taken, else
/// the closest name above it that takes nothing new.
fn build_absent_directory_outcome(directory_path: &Path, creation_event: &str) -> DoctorOutcome {
    let displayed_path = directory_path.display();
    match find_nearest_existing_path(directory_path) {
        Some((existing_path, true)) => DoctorOutcome::build_success_outcome(format!(
            "{displayed_path} does not exist yet; koshi creates it under {} {creation_event}",
            existing_path.display()
        )),
        Some((existing_path, false)) if existing_path == directory_path => DoctorOutcome::build_failure_outcome(
            format!("{displayed_path} is a name koshi cannot make a directory at"),
            &format!("remove {displayed_path}, or point it at a directory"),
        ),
        Some((existing_path, false)) => DoctorOutcome::build_failure_outcome(
            format!(
                "{displayed_path} does not exist and koshi cannot create it: nothing new can be written in {}",
                existing_path.display()
            ),
            &format!("make sure you can write in {}", existing_path.display()),
        ),
        None => DoctorOutcome::build_failure_outcome(
            format!("{displayed_path} does not exist and no name above it does either"),
            &format!("make sure a directory above {displayed_path} exists"),
        ),
    }
}

/// The closest name at or above `directory_path` that is already there, and whether a
/// new directory can be made inside it. `None` when neither `directory_path` nor
/// anything above `directory_path` is there.
///
/// Each name is read without following it, so a symbolic link pointing
/// nowhere counts as being there, and `directory_path` itself is read first. The second
/// value comes from making a directory inside that name and removing it
/// again.
///
/// `/tmp/koshi-501` with `/tmp` present and writable gives
/// `Some(("/tmp", true))`. `/tmp/parent/koshi` where `koshi` points nowhere
/// gives `Some(("/tmp/parent/koshi", false))`.
fn find_nearest_existing_path(directory_path: &Path) -> Option<(PathBuf, bool)> {
    let existing_path = directory_path
        .ancestors()
        .find(|ancestor_path| std::fs::symlink_metadata(ancestor_path).is_ok())?;
    let can_create_child_directory = tempfile::TempDir::new_in(existing_path).is_ok();
    Some((existing_path.to_path_buf(), can_create_child_directory))
}

/// What this machine can do with the program a new pane would run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellAvailability {
    /// A regular file this machine can run.
    Runnable,
    /// A regular file carrying no execute bit. Unix only: on Windows a
    /// regular file is always [`ShellAvailability::Runnable`].
    NotExecutable,
    /// No regular file of that name.
    Missing,
}

/// Whether this user may run `shell_path`, asked of the kernel with `access(X_OK)`.
///
/// The answer covers the owner, group and other bits, any access control
/// list, and a filesystem mounted without execute permission. It is made
/// against this process's real user and group.
#[cfg(unix)]
fn user_may_execute(shell_path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(shell_path_c_string) = CString::new(shell_path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `shell_path_c_string` holds a NUL-terminated C string that outlives the call,
    // and `access` only reads it.
    unsafe { libc::access(shell_path_c_string.as_ptr(), libc::X_OK) == 0 }
}

/// What `shell_path` is: [`ShellAvailability::Missing`] when it is absent or is not
/// a regular file, [`ShellAvailability::NotExecutable`] when it is a regular
/// file this user may not run, else [`ShellAvailability::Runnable`].
///
/// `/bin/zsh` at mode `755` is `Runnable`. The same file at mode `644` is
/// `NotExecutable`, and so is a file at mode `011` this user owns, whose
/// group and other bits do not apply to its owner.
fn inspect_shell_file(shell_path: &Path) -> ShellAvailability {
    let Ok(shell_file_metadata) = std::fs::metadata(shell_path) else {
        return ShellAvailability::Missing;
    };
    if !shell_file_metadata.is_file() {
        return ShellAvailability::Missing;
    }
    #[cfg(unix)]
    if !user_may_execute(shell_path) {
        return ShellAvailability::NotExecutable;
    }
    ShellAvailability::Runnable
}

/// What this machine can do with `shell_program`: the path itself when it holds
/// a directory, else each `PATH` entry in order.
///
/// A `PATH` search answers [`ShellAvailability::Runnable`] on the first runnable
/// match, and [`ShellAvailability::NotExecutable`] only when some entry held a
/// regular file and none held a runnable one. `("/bin/zsh", _)` reads `/bin/zsh`;
/// `("cmd.exe", "C:\\Windows\\System32")` reads
/// `C:\Windows\System32\cmd.exe`.
fn inspect_shell_program(
    shell_program: &Path,
    path_entries: Option<&OsString>,
) -> ShellAvailability {
    if shell_program
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty())
    {
        return inspect_shell_file(shell_program);
    }
    let Some(path_entries) = path_entries else {
        return ShellAvailability::Missing;
    };
    let mut has_non_executable_file = false;
    for path_directory in std::env::split_paths(path_entries) {
        match inspect_shell_file(&path_directory.join(shell_program)) {
            ShellAvailability::Runnable => return ShellAvailability::Runnable,
            ShellAvailability::NotExecutable => has_non_executable_file = true,
            ShellAvailability::Missing => {}
        }
    }
    if has_non_executable_file {
        ShellAvailability::NotExecutable
    } else {
        ShellAvailability::Missing
    }
}

/// The permission bits of `directory_path` on Unix, `None` on Windows and when `directory_path`
/// cannot be read.
///
/// Masks with `0o777`, the same mask
/// [`koshi_ipc::validate::validate_socket_address`] applies before a socket
/// binds, so the setuid, setgid and sticky bits are left out. `0o755` reads
/// back as `0o755`; a sticky `0o1700` reads back as `0o700`.
fn read_directory_mode(directory_path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::metadata(directory_path)
            .ok()
            .map(|directory_metadata| directory_metadata.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = directory_path;
        None
    }
}

/// How many grants in `token_store` still stand at `current_time`: not revoked,
/// and either never expiring or expiring after `current_time`.
fn count_standing_grants(token_store: &TokenStore, current_time: SystemTime) -> usize {
    token_store
        .token_records
        .iter()
        .filter(|token_record| {
            token_record.revoked_at.is_none()
                && token_record
                    .expires_at
                    .is_none_or(|expiration_time| expiration_time > current_time)
        })
        .count()
}

/// `message_text` with every newline and carriage return replaced by one space.
///
/// `"bad\nfile"` gives `"bad file"`; `"bad\r\nfile"` gives `"bad  file"`.
fn format_single_line(message_text: String) -> String {
    if !message_text.contains(['\n', '\r']) {
        return message_text;
    }
    message_text.replace(['\n', '\r'], " ")
}

/// The answer a check gives when this machine reports no home directory, so
/// the directory it is about has no location at all. `"config"` gives the
/// reason `"this machine reports no home directory, so koshi finds no config
/// directory"`.
fn build_no_home_directory_outcome(directory_kind: &str) -> DoctorOutcome {
    DoctorOutcome::build_failure_outcome(
        format!(
            "this machine reports no home directory, so koshi finds no {directory_kind} directory"
        ),
        "give this user a home directory",
    )
}

/// `quantity` and `singular_noun`, with an `s` on the noun when `quantity` is not 1:
/// `(2, "grant")` gives `"2 grants"`, `(1, "grant")` gives `"1 grant"`.
fn format_counted_noun(quantity: usize, singular_noun: &str) -> String {
    if quantity == 1 {
        format!("{quantity} {singular_noun}")
    } else {
        format!("{quantity} {singular_noun}s")
    }
}

#[cfg(test)]
mod tests;
