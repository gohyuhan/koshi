//! `koshi doctor`: check this machine's koshi installation and print one row
//! per check.
//!
//! Every fact the checks read is gathered once into a
//! [`crate::doctor::Context`] — the platform directories, `koshi.kdl`, the
//! environment variables a pane inherits, the remote access grant file, and
//! the running router. The checks themselves read only that context and touch
//! the filesystem only where a check names a file operation of its own.
//!
//! The checks run in print order, each one a name and a function. A check
//! answers [`crate::doctor::Verdict::Ok`], [`crate::doctor::Verdict::Warn`] or
//! [`crate::doctor::Verdict::Fail`] with the fact behind it and what to do
//! about it. The whole table reaches stdout before [`crate::doctor::run`]
//! returns, and a run with any [`crate::doctor::Verdict::Fail`] row ends in
//! [`koshi_link::error::CliError::Runtime`].

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use koshi_core::process::SpawnSpec;
use koshi_ipc::remote_tokens::{store_path, TokenStore};
use serde::Serialize;

use crate::cli::FormatArg;
use crate::output;
use koshi_link::error::CliError;
use koshi_link::router_client::{running_router_remote_connections, RemoteConnections};
use koshi_paths::RuntimeDirRule;

/// What one check concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The check found what it looks for.
    Ok,
    /// The check found something that still works and is worth reading.
    Warn,
    /// The check found something koshi cannot work through.
    Fail,
}

/// One check's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
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

impl Outcome {
    /// A [`Verdict::Ok`] answer carrying `reason`, no help and no detail.
    fn ok(reason: String) -> Outcome {
        Outcome {
            verdict: Verdict::Ok,
            reason: one_line(reason),
            help: None,
            detail: None,
        }
    }

    /// A [`Verdict::Warn`] answer carrying `reason` and `help`, and no detail.
    fn warn(reason: String, help: &str) -> Outcome {
        Outcome {
            verdict: Verdict::Warn,
            reason: one_line(reason),
            help: Some(help.to_string()),
            detail: None,
        }
    }

    /// A [`Verdict::Fail`] answer carrying `reason` and `help`, and no detail.
    fn fail(reason: String, help: &str) -> Outcome {
        Outcome {
            verdict: Verdict::Fail,
            reason: one_line(reason),
            help: Some(help.to_string()),
            detail: None,
        }
    }

    /// The same answer carrying `detail` as the full text behind its `reason`.
    fn with_detail(mut self, detail: String) -> Outcome {
        self.detail = Some(detail);
        self
    }
}

/// One check: what it is called, and the function that runs it.
struct Check {
    /// The name printed in the `check` column, e.g. `"runtime directory"`.
    name: &'static str,
    /// Runs the check against the gathered context.
    run: fn(&Context) -> Outcome,
}

/// Every check `koshi doctor` runs, in print order.
const CHECKS: &[Check] = &[
    Check {
        name: "config",
        run: check_config,
    },
    Check {
        name: "shell",
        run: check_shell,
    },
    Check {
        name: "terminal",
        run: check_terminal,
    },
    Check {
        name: "runtime directory",
        run: check_runtime_dir,
    },
    Check {
        name: "log directory",
        run: check_log_dir,
    },
    Check {
        name: "plugins directory",
        run: check_plugins_dir,
    },
    Check {
        name: "router",
        run: check_router,
    },
    Check {
        name: "session directory",
        run: check_session_directory,
    },
    Check {
        name: "remote access",
        run: check_remote_access,
    },
    Check {
        name: "remote connections",
        run: check_remote_connections,
    },
];

/// The environment variable naming the program a new pane runs: `"SHELL"` on
/// Unix, `"COMSPEC"` on Windows.
#[cfg(not(windows))]
const SHELL_VAR: &str = "SHELL";
#[cfg(windows)]
const SHELL_VAR: &str = "COMSPEC";

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
pub struct Context {
    /// The directory `koshi.kdl` lives in, or `None` when this machine
    /// reports no home directory.
    pub config_dir: Option<PathBuf>,
    /// The private runtime directory holding the endpoint files, or `None`
    /// when this machine reports no home directory.
    /// [`Context::runtime_dir_rule`] names the rule that produced it.
    pub runtime_dir: Option<PathBuf>,
    /// The rule that produced [`Context::runtime_dir`], or `None` when this
    /// machine reports no home directory.
    pub runtime_dir_rule: Option<RuntimeDirRule>,
    /// The runtime directory's permission bits, `None` on Windows, when this
    /// machine reports no home directory, and when the directory could not be
    /// read.
    pub runtime_mode: Option<u32>,
    /// The directory koshi writes its log files in, or `None` when this
    /// machine reports no home directory.
    pub log_dir: Option<PathBuf>,
    /// `plugins` under the config directory, or `None` when this machine
    /// reports no home directory.
    pub plugins_dir: Option<PathBuf>,
    /// The machine-wide directory the shared session sockets live in:
    /// `koshi.kdl`'s `shared-sessions-dir` when it names one, else this
    /// platform's own. `None` when neither names one.
    pub shared_dir: Option<PathBuf>,
    /// The program a new pane runs.
    pub shell: PathBuf,
    /// Where [`Context::shell`] was read from.
    pub shell_source: ShellSource,
    /// `PATH`, used to find a shell named without a directory.
    pub path: Option<OsString>,
    /// `TERM`, or `None` when it is unset or empty.
    pub term: Option<String>,
    /// `COLORTERM`, or `None` when it is unset or empty.
    pub colorterm: Option<String>,
    /// `koshi.kdl`'s `allow-other-users`.
    pub allow_other_users: bool,
    /// `koshi.kdl`'s `remote-listen` address, or `None` when it names none.
    pub remote_listen: Option<String>,
    /// `koshi.kdl`'s `logging.enabled`.
    pub logging_on: bool,
    /// How many remote access grants still stand, or the message naming why
    /// they could not be read.
    pub grants: Result<usize, String>,
    /// What asking the running router produced.
    pub router: RemoteConnections,
}

impl Context {
    /// Read every fact the checks need from this machine: the platform
    /// directories, the rule that produced the runtime directory, `koshi.kdl`,
    /// the environment, the grant file, and the running router. Creates
    /// nothing and starts no router.
    #[must_use]
    pub fn of_this_machine() -> Context {
        let config_dir = koshi_paths::config_dir();
        let (runtime_dir, runtime_dir_rule) = koshi_paths::runtime_dir_with_rule().unzip();
        let runtime_mode = runtime_dir.as_deref().and_then(directory_mode);
        let plugins_dir = config_dir.as_ref().map(|dir| dir.join("plugins"));
        let server = koshi_link::config::server_config_now();
        let shared_dir = server
            .shared_sessions_dir
            .clone()
            .or_else(koshi_paths::shared_sessions_dir);
        let grants = match koshi_paths::data_dir() {
            Some(data_dir) => match TokenStore::read(&store_path(&data_dir)) {
                Ok(store) => Ok(standing_grants(&store, SystemTime::now())),
                Err(error) => Err(error.to_string()),
            },
            None => Err("this machine reports no home directory".to_string()),
        };
        let router = match runtime_dir.as_deref() {
            Some(dir) => running_router_remote_connections(dir),
            None => RemoteConnections::NotRunning,
        };
        Context {
            config_dir,
            runtime_dir,
            runtime_dir_rule,
            runtime_mode,
            log_dir: koshi_observability::logging::log_dir(),
            plugins_dir,
            shared_dir,
            shell: match &server.terminal.default_shell {
                Some(program) => PathBuf::from(program),
                None => SpawnSpec::default_shell(None, BTreeMap::new()).program,
            },
            shell_source: match &server.terminal.default_shell {
                Some(_) => ShellSource::Config,
                None if std::env::var_os(SHELL_VAR).is_some_and(|value| !value.is_empty()) => {
                    ShellSource::Environment
                }
                None => ShellSource::Fallback,
            },
            path: std::env::var_os("PATH"),
            term: std::env::var_os("TERM")
                .and_then(|value| value.into_string().ok())
                .filter(|value| !value.is_empty()),
            colorterm: std::env::var_os("COLORTERM")
                .and_then(|value| value.into_string().ok())
                .filter(|value| !value.is_empty()),
            allow_other_users: server.allow_other_users,
            remote_listen: server.remote_listen,
            logging_on: server.logging.enabled,
            grants,
            router,
        }
    }
}

/// One row of the answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckRow {
    /// The check's name.
    pub name: &'static str,
    /// What the check concluded.
    pub verdict: Verdict,
    /// The fact behind the verdict.
    pub reason: String,
    /// What to do about it, or `None` when there is nothing to do.
    pub help: Option<String>,
    /// The full text behind a shortened `reason`, or `None` when `reason`
    /// already says the whole thing. `--format json` prints it and the table
    /// leaves it out.
    pub detail: Option<String>,
}

/// Run every check against `context`, in print order.
#[must_use]
pub fn rows(context: &Context) -> Vec<CheckRow> {
    CHECKS
        .iter()
        .map(|check| {
            let outcome = (check.run)(context);
            CheckRow {
                name: check.name,
                verdict: outcome.verdict,
                reason: outcome.reason,
                help: outcome.help,
                detail: outcome.detail,
            }
        })
        .collect()
}

/// Check this machine's koshi installation and print one row per check.
///
/// # Errors
/// [`CliError::Runtime`] when any row is [`Verdict::Fail`], after the whole
/// answer has been printed. A [`Verdict::Warn`] row never fails the run.
pub fn run(format: FormatArg) -> Result<(), CliError> {
    let context = Context::of_this_machine();
    let rows = rows(&context);
    print!("{}", output::render_doctor(&rows, format));
    failed(&rows).map_or(Ok(()), Err)
}

/// The failure a run ends with when any row failed, naming how many did, or
/// `None` when every row is ok or warn.
fn failed(rows: &[CheckRow]) -> Option<CliError> {
    let count = rows
        .iter()
        .filter(|row| row.verdict == Verdict::Fail)
        .count();
    if count == 0 {
        return None;
    }
    Some(CliError::Runtime {
        detail: format!("{} failed", counted(count, "check")),
    })
}

fn check_config(context: &Context) -> Outcome {
    let Some(dir) = context.config_dir.as_deref() else {
        return no_home_directory("config");
    };
    let report = crate::config_command::validate_dir(dir);
    if !report.errors.is_empty() {
        return Outcome::fail(
            report.errors.join("; "),
            "run koshi config check to see each file",
        );
    }
    if report.lines.is_empty() {
        return Outcome::ok(format!("no config file is present in {}", dir.display()));
    }
    Outcome::ok(format!(
        "{} validated",
        counted(report.lines.len(), "config file")
    ))
}

fn check_shell(context: &Context) -> Outcome {
    let shell = context.shell.display();
    match shell_state(&context.shell, context.path.as_ref()) {
        ShellState::NotExecutable => Outcome::fail(
            format!("{shell} is on this machine and carries no execute bit"),
            &format!("run chmod +x {shell}"),
        ),
        ShellState::Missing => match context.shell_source {
            ShellSource::Config => Outcome::fail(
                format!("koshi.kdl names {shell}, which is not on this machine"),
                "set terminal.default-shell in koshi.kdl to a shell that exists",
            ),
            ShellSource::Environment | ShellSource::Fallback => Outcome::fail(
                format!("a new pane would run {shell}, which is not on this machine"),
                &format!("set {SHELL_VAR} to a shell that exists"),
            ),
        },
        ShellState::Runnable => match context.shell_source {
            ShellSource::Config => {
                Outcome::ok(format!("koshi.kdl names {shell}, which a new pane runs"))
            }
            ShellSource::Environment => Outcome::ok(format!("a new pane runs {shell}")),
            ShellSource::Fallback => Outcome::warn(
                format!("{SHELL_VAR} is not set, so a new pane runs {shell}"),
                &format!("set {SHELL_VAR} to the shell you want a new pane to run"),
            ),
        },
    }
}

fn check_terminal(context: &Context) -> Outcome {
    let help = "set TERM before running koshi, for example TERM=xterm-256color";
    match context.term.as_deref() {
        None => Outcome::warn("TERM is not set".to_string(), help),
        Some("dumb") => Outcome::warn(
            "TERM is dumb, which names a terminal with no cursor control".to_string(),
            help,
        ),
        Some(term) => Outcome::ok(format!(
            "TERM is {term}, COLORTERM is {}",
            context.colorterm.as_deref().unwrap_or("not set")
        )),
    }
}

/// The runtime directory check: what state the directory is in, and after a
/// `; ` the rule that produced its path.
///
/// `/tmp/koshi-501` ready, named by `KOSHI_RUNTIME_DIR`, gives the reason
/// `"/tmp/koshi-501 is ready; KOSHI_RUNTIME_DIR names it"`.
fn check_runtime_dir(context: &Context) -> Outcome {
    let (Some(dir), Some(rule)) = (context.runtime_dir.as_deref(), context.runtime_dir_rule) else {
        return no_home_directory("runtime");
    };
    let mut outcome = runtime_dir_state(dir, context.runtime_mode);
    outcome.reason = format!("{}; {}", outcome.reason, runtime_dir_rule_phrase(rule));
    outcome
}

fn check_router(context: &Context) -> Outcome {
    match &context.router {
        RemoteConnections::Answered(_) => {
            Outcome::ok("a router answers on its control socket".to_string())
        }
        RemoteConnections::NotRunning => Outcome::ok("no koshi is running".to_string()),
        RemoteConnections::OlderBuild => Outcome::warn(
            "the running router is an older koshi build".to_string(),
            "end every koshi process on this machine and start one again",
        ),
        RemoteConnections::NoAnswer { detail } => Outcome::fail(
            "a router is listening and did not answer".to_string(),
            "end every koshi process on this machine and start one again",
        )
        .with_detail(detail.clone()),
    }
}

fn check_log_dir(context: &Context) -> Outcome {
    let Some(dir) = context.log_dir.as_deref() else {
        return Outcome::warn(
            "this machine reports no home directory, so a log file lands in whichever directory koshi is started from"
                .to_string(),
            "give this user a home directory",
        );
    };
    let path = dir.display();
    if !dir.exists() {
        return absent_directory(dir, "when logging is on");
    }
    match tempfile::NamedTempFile::new_in(dir) {
        Ok(probe) => {
            drop(probe);
            Outcome::ok(format!(
                "{path} is writable and logging is {}",
                if context.logging_on { "on" } else { "off" }
            ))
        }
        Err(error) => Outcome::fail(
            format!("{path} cannot be written: {error}"),
            &format!("make sure you own {path}"),
        ),
    }
}

fn check_plugins_dir(context: &Context) -> Outcome {
    let Some(dir) = context.plugins_dir.as_deref() else {
        return no_home_directory("plugins");
    };
    let path = dir.display();
    if !dir.exists() {
        return match std::fs::symlink_metadata(dir) {
            Ok(_) => Outcome::fail(
                format!("{path} is there and koshi cannot read it as a directory"),
                &format!("remove {path}, or point it at a directory"),
            ),
            Err(_) => Outcome::ok(format!("{path} does not exist")),
        };
    }
    if let Err(error) = std::fs::read_dir(dir) {
        return Outcome::fail(
            format!("{path} cannot be read: {error}"),
            &format!("make sure you own {path}"),
        );
    }
    Outcome::ok(format!("{path} is readable"))
}

fn check_session_directory(context: &Context) -> Outcome {
    if !context.allow_other_users {
        let Some(dir) = context.runtime_dir.as_deref() else {
            return Outcome::ok(
                "allow-other-users is off, so only you may reach your sessions".to_string(),
            );
        };
        let where_they_live = match context.runtime_mode {
            Some(mode) => format!("{} (mode {mode:03o})", dir.display()),
            None => dir.display().to_string(),
        };
        return Outcome::ok(format!(
            "sessions are advertised in {where_they_live}, which only you may reach"
        ));
    }
    match context.shared_dir.as_deref() {
        Some(dir) => Outcome::ok(format!(
            "allow-other-users is on: sessions are also advertised in {}, which every user of this machine may reach",
            dir.display()
        )),
        None => Outcome::ok(
            "allow-other-users is on, and this machine names no shared session directory, so no other user reaches your sessions"
                .to_string(),
        ),
    }
}

fn check_remote_access(context: &Context) -> Outcome {
    let address = match context.remote_listen.as_deref() {
        Some(address) => format!("koshi.kdl names the remote listen address {address}"),
        None => "koshi.kdl names no remote listen address".to_string(),
    };
    match &context.grants {
        Ok(count) => Outcome::ok(format!(
            "{address}, and this machine holds {}",
            counted(*count, "standing grant")
        )),
        Err(detail) => Outcome::warn(
            format!("{address}, and the grants could not be read: {detail}"),
            "make sure you own the koshi data directory",
        ),
    }
}

fn check_remote_connections(context: &Context) -> Outcome {
    match &context.router {
        RemoteConnections::Answered(Some(remote_connections)) => Outcome::ok(format!(
            "this machine holds {} from another machine",
            counted(*remote_connections, "open connection")
        )),
        RemoteConnections::Answered(None) => {
            Outcome::ok("the running router reports no count, so this is not known".to_string())
        }
        RemoteConnections::NotRunning => Outcome::ok(
            "no koshi is running, so nothing from another machine is connected".to_string(),
        ),
        RemoteConnections::OlderBuild | RemoteConnections::NoAnswer { .. } => {
            Outcome::ok("the running router did not answer, so this is not known".to_string())
        }
    }
}

/// What state the runtime directory `dir` is in, with `mode` its permission
/// bits and `None` where they are not known.
///
/// Ok when `dir` holds mode 700, when `mode` is `None`, and when `dir` is not
/// there yet and koshi can create it. Fail when `dir` cannot be read, when
/// `mode` is anything other than 700, and when `dir` is not there and koshi
/// cannot create it.
fn runtime_dir_state(dir: &Path, mode: Option<u32>) -> Outcome {
    let path = dir.display();
    if !dir.exists() {
        return absent_directory(dir, "when a session starts");
    }
    if let Err(error) = std::fs::read_dir(dir) {
        return Outcome::fail(
            format!("{path} cannot be read: {error}"),
            &format!("make sure you own {path}"),
        );
    }
    if let Some(mode) = mode {
        if mode != 0o700 {
            return Outcome::fail(
                format!(
                    "{path} has mode {mode:03o}; koshi serves a session socket only from a directory with mode 700"
                ),
                &format!("run chmod 700 {path}"),
            );
        }
    }
    Outcome::ok(format!("{path} is ready"))
}

/// `rule` in words, holding no newline:
/// [`RuntimeDirRule::Variable`] gives `"KOSHI_RUNTIME_DIR names it"`.
fn runtime_dir_rule_phrase(rule: RuntimeDirRule) -> &'static str {
    match rule {
        RuntimeDirRule::Variable => "KOSHI_RUNTIME_DIR names it",
        RuntimeDirRule::UserId => "koshi names it after your user id",
        RuntimeDirRule::DataDir => "koshi puts it under your application data directory",
    }
}

/// The answer for a directory koshi makes for itself that is not there yet.
///
/// `created` names the moment koshi makes it, such as `"when a session
/// starts"`.
///
/// Ok when a directory can be made at `dir`, naming what it goes under. Fail
/// naming what stops it: `dir` itself when that name is taken, else the
/// closest name above it that takes nothing new.
fn absent_directory(dir: &Path, created: &str) -> Outcome {
    let path = dir.display();
    match nearest_existing_name(dir) {
        Some((holder, true)) => Outcome::ok(format!(
            "{path} does not exist yet; koshi creates it under {} {created}",
            holder.display()
        )),
        Some((holder, false)) if holder == dir => Outcome::fail(
            format!("{path} is a name koshi cannot make a directory at"),
            &format!("remove {path}, or point it at a directory"),
        ),
        Some((holder, false)) => Outcome::fail(
            format!(
                "{path} does not exist and koshi cannot create it: nothing new can be written in {}",
                holder.display()
            ),
            &format!("make sure you can write in {}", holder.display()),
        ),
        None => Outcome::fail(
            format!("{path} does not exist and no name above it does either"),
            &format!("make sure a directory above {path} exists"),
        ),
    }
}

/// The closest name at or above `path` that is already there, and whether a
/// new directory can be made inside it. `None` when neither `path` nor
/// anything above it is there.
///
/// Each name is read without following it, so a symbolic link pointing
/// nowhere counts as being there, and `path` itself is read first. The second
/// value comes from making a directory inside that name and removing it
/// again.
///
/// `/tmp/koshi-501` with `/tmp` present and writable gives
/// `Some(("/tmp", true))`. `/tmp/parent/koshi` where `koshi` points nowhere
/// gives `Some(("/tmp/parent/koshi", false))`.
fn nearest_existing_name(path: &Path) -> Option<(PathBuf, bool)> {
    let name = path
        .ancestors()
        .find(|above| std::fs::symlink_metadata(above).is_ok())?;
    let takes_a_new_directory = tempfile::TempDir::new_in(name).is_ok();
    Some((name.to_path_buf(), takes_a_new_directory))
}

/// What this machine can do with the program a new pane would run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellState {
    /// A regular file this machine can run.
    Runnable,
    /// A regular file carrying no execute bit. Unix only: on Windows a
    /// regular file is always [`ShellState::Runnable`].
    NotExecutable,
    /// No regular file of that name.
    Missing,
}

/// Whether this user may run `path`, asked of the kernel with `access(X_OK)`.
///
/// The answer covers the owner, group and other bits, any access control
/// list, and a filesystem mounted without execute permission. It is made
/// against this process's real user and group.
#[cfg(unix)]
fn user_may_execute(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(name) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `name` holds a NUL-terminated C string that outlives the call,
    // and `access` only reads it.
    unsafe { libc::access(name.as_ptr(), libc::X_OK) == 0 }
}

/// What `path` is: [`ShellState::Missing`] when it is absent or is not a
/// regular file, [`ShellState::NotExecutable`] when it is a regular file this
/// user may not run, else [`ShellState::Runnable`].
///
/// `/bin/zsh` at mode `755` is `Runnable`. The same file at mode `644` is
/// `NotExecutable`, and so is a file at mode `011` this user owns, whose
/// group and other bits do not apply to its owner.
fn file_state(path: &Path) -> ShellState {
    let Ok(metadata) = std::fs::metadata(path) else {
        return ShellState::Missing;
    };
    if !metadata.is_file() {
        return ShellState::Missing;
    }
    #[cfg(unix)]
    if !user_may_execute(path) {
        return ShellState::NotExecutable;
    }
    ShellState::Runnable
}

/// What this machine can do with `program`: the path itself when it holds a
/// directory, else each `path` entry in order.
///
/// A `path` search answers [`ShellState::Runnable`] on the first runnable
/// match, and [`ShellState::NotExecutable`] only when some entry held a
/// regular file and none held a runnable one. `("/bin/zsh", _)` reads `/bin/zsh`;
/// `("cmd.exe", "C:\\Windows\\System32")` reads
/// `C:\Windows\System32\cmd.exe`.
fn shell_state(program: &Path, path: Option<&OsString>) -> ShellState {
    if program
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty())
    {
        return file_state(program);
    }
    let Some(path) = path else {
        return ShellState::Missing;
    };
    let mut found_a_file = false;
    for dir in std::env::split_paths(path) {
        match file_state(&dir.join(program)) {
            ShellState::Runnable => return ShellState::Runnable,
            ShellState::NotExecutable => found_a_file = true,
            ShellState::Missing => {}
        }
    }
    if found_a_file {
        ShellState::NotExecutable
    } else {
        ShellState::Missing
    }
}

/// The permission bits of `path` on Unix, `None` on Windows and when `path`
/// cannot be read.
///
/// Masks with `0o777`, the same mask
/// [`koshi_ipc::validate::validate_socket_addr`] applies before a socket
/// binds, so the setuid, setgid and sticky bits are left out. `0o755` reads
/// back as `0o755`; a sticky `0o1700` reads back as `0o700`.
fn directory_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::metadata(path)
            .ok()
            .map(|metadata| metadata.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// How many grants in `store` still stand at `now`: not revoked, and either
/// never expiring or expiring after `now`.
fn standing_grants(store: &TokenStore, now: SystemTime) -> usize {
    store
        .records
        .iter()
        .filter(|record| {
            record.revoked_at.is_none() && record.expires_at.is_none_or(|expiry| expiry > now)
        })
        .count()
}

/// `text` with every newline and carriage return replaced by one space.
///
/// `"bad\nfile"` gives `"bad file"`; `"bad\r\nfile"` gives `"bad  file"`.
fn one_line(text: String) -> String {
    if !text.contains(['\n', '\r']) {
        return text;
    }
    text.replace(['\n', '\r'], " ")
}

/// The answer a check gives when this machine reports no home directory, so
/// the directory it is about has no location at all. `"config"` gives the
/// reason `"this machine reports no home directory, so koshi finds no config
/// directory"`.
fn no_home_directory(which: &str) -> Outcome {
    Outcome::fail(
        format!("this machine reports no home directory, so koshi finds no {which} directory"),
        "give this user a home directory",
    )
}

/// `count` and `noun`, with an `s` on the noun when `count` is not 1:
/// `(2, "grant")` gives `"2 grants"`, `(1, "grant")` gives `"1 grant"`.
fn counted(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

#[cfg(test)]
mod tests;
