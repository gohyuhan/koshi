//! Self-update: install a newer koshi release the way this koshi was
//! installed, then restart the running servers into it.
//!
//! `koshi update` (`run_update_command`) first reads where this koshi came
//! from, as `install_source` states. A Homebrew install upgrades through
//! `brew`. A Scoop install and a file no package manager tracks check the
//! project's GitHub releases and, when a newer one exists, download the
//! prebuilt archive for this OS/arch, verify its SHA-256 checksum, unpack the
//! `koshi` binary, copy it beside the program file, run the copy with
//! `--version`, and swap the copy in once it prints the release's version. A
//! build from source downloads nothing. An interactive launch also calls
//! `prompt_startup_update`, which does the same check on a timer and offers to
//! install.
//!
//! Two files back this. `koshi.kdl` holds the preferences koshi reads:
//! `update.auto-check`, `update.check-interval-days`, and
//! `update.allow-prerelease`. `update.json` in the state directory holds the
//! last-check time, the one value this module writes. This module does not
//! write `koshi.kdl`.
//!
//! The CLI process runs this flow once per command, and reads the clock and
//! the network directly. After every update, whatever it did, it asks the
//! running sessions, and then the running router, to restart into the koshi
//! program each one runs from. For a build from source, every running server
//! restarts. For a release install, two kinds of server keep running as they
//! are: a server whose `.program` file in the runtime directory says it
//! already runs the version that the updated `koshi --version` prints, and a
//! session whose `.program` file names another file than the updated `koshi`
//! once every symbolic link is followed. `koshi restart-servers`
//! (`run_restart_servers_command`) asks every running server to restart,
//! without an install.
//!
//! The restart also walks the runtime directories koshi 0.1.0 and 0.2.0 used.
//! A session that koshi 0.2.0 or one of its pre-releases started has no
//! restart request: once every session is asked, the restart names each such
//! session, asks once whether to end them, and ends them or leaves them
//! running.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use koshi_config::app_config::parse_app_config;
use koshi_config::layer::merge_client;
use koshi_config::types::{ClientConfig, UpdateConfig};
use koshi_core::ids::SessionId;
use koshi_host::process_tree;
use koshi_ipc::endpoint::{resolve_update_lock_path, EndpointFile, ServerProgramFile};
use koshi_ipc::error::IpcError;
use koshi_ipc::router::{resolve_router_endpoint_path, resolve_router_program_file_path};
use koshi_runtime::executable_watch::read_installed_version;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::{Builder, TempPath};
use ureq::tls::TlsConfig;
use ureq::Agent;

use koshi_core::text::sanitize_reported_text;
use koshi_link::error::CliError;
use koshi_link::in_session::InSessionContext;
use koshi_link::ipc_client::{
    self, find_previous_release_session_version, find_running_session_version,
    restart_running_session, SessionRestart, UnreadPath,
};
use koshi_link::router_client::{
    find_running_router_version, restart_running_router, start_router,
};
use koshi_link::server_build::{
    compare_program_files, find_advertised_server_program_file, find_server_program_file,
    ProgramFileMatch,
};

use install_source::{
    find_install_source, InstallSource, ReleaseInstall, HOMEBREW_FORMULA_NAME,
    HOMEBREW_TAPPED_FORMULA_NAME,
};

use crate::session_end::{
    end_session, find_server_process_record, format_process_kill_command,
    PROCESS_STOP_GRACE_DURATION,
};

/// This build's version, from the crate version bumped before each release.
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Whether this build is a release build: `true` when the environment variable
/// `KOSHI_RELEASE_BUILD` held any value while it was compiled, as the release
/// workflow sets it. A build from source leaves it unset.
const IS_RELEASE_BUILD: bool = option_env!("KOSHI_RELEASE_BUILD").is_some();

/// The GitHub `owner/repo` the release archives live under.
const RELEASE_REPOSITORY: &str = "gohyuhan/koshi";

/// How long the GitHub API check may run before it is abandoned. Bounds the
/// whole call, connection through JSON body.
const UPDATE_API_TIMEOUT_DURATION: Duration = Duration::from_secs(15);

/// How long a binary download may run before it is abandoned. Bounds the whole
/// call, connection through the streamed archive body.
const UPDATE_DOWNLOAD_TIMEOUT_DURATION: Duration = Duration::from_secs(600);

/// The largest checksum response accepted from the release server.
const MAX_CHECKSUM_FILE_BYTE_COUNT: u64 = 64 * 1024;

/// The largest release archive response accepted from the release server.
const MAX_RELEASE_ARCHIVE_BYTE_COUNT: u64 = 256 * 1024 * 1024;

/// Seconds in a day, for turning the check interval into a duration.
const SECONDS_PER_DAY: u64 = 86_400;

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Runs `koshi update`: update this koshi the way it was installed, then
/// restart each running server that does not run the version its program
/// file now holds.
///
/// Where this koshi came from decides the update, as
/// `find_install_source` reads it from the path
/// [`resolve_program_path`](koshi_host::program_path::resolve_program_path)
/// gives:
///
/// - A release install updates as `update_release_install` states: through
///   `brew` for a Homebrew install, and from the newest GitHub release for
///   every other one.
/// - A build from source downloads nothing, and prints `koshi <version> at
///   <path> was built from source; koshi update downloads nothing for it, and
///   restarts every running server into the koshi at that path`.
///
/// The restart runs whatever the update did, as
/// `restart_servers_into_program_file` states: with
/// `RestartScope::ServersNotOnVersion` for a release install, and with
/// `RestartScope::EveryServer` for a build from source.
///
/// # Errors
/// Returns [`CliError::Update`] when the path of this koshi cannot be read,
/// and when the update fails: the network check, no release binary for this
/// platform, the download or install step, or `brew`.
pub fn run_update_command() -> Result<(), CliError> {
    let program_path =
        koshi_host::program_path::resolve_program_path().map_err(|program_path_error| {
            build_update_error(format!(
                "the path of this koshi could not be read: {program_path_error}"
            ))
        })?;
    let (update_result, restart_scope) = match find_install_source(&program_path, IS_RELEASE_BUILD)
    {
        InstallSource::SourceBuild => {
            println!(
                "koshi {APP_VERSION} at {} was built from source; koshi update downloads \
                     nothing for it, and restarts every running server into the koshi at that \
                     path",
                program_path.display()
            );
            (Ok(()), RestartScope::EveryServer)
        }
        InstallSource::Release(release_install) => (
            update_release_install(&release_install),
            RestartScope::ServersNotOnVersion {
                program_path: &program_path,
            },
        ),
    };
    restart_servers_into_program_file(&program_path, restart_scope);
    update_result.map_err(build_update_error)
}

/// Update the release install `release_install`.
///
/// - Homebrew: [`upgrade_through_homebrew`], with no GitHub check first.
///   `brew` decides whether its formula has a newer version.
/// - Scoop, and a file no package manager tracks: check GitHub for a release
///   newer than this build, and install it as [`install_update`] states.
///   Prints `koshi <version> is already the latest version` when none is
///   newer. The check takes pre-releases when `update.allow-prerelease` is on.
///   A completed check is recorded as the last check, whether or not it found
///   a newer release.
///
/// # Errors
/// The failure of `brew`, of the check, or of [`install_update`].
fn update_release_install(release_install: &ReleaseInstall) -> Result<(), String> {
    if let ReleaseInstall::Homebrew {
        brew_path,
        formula_name,
    } = release_install
    {
        return upgrade_through_homebrew(brew_path, formula_name);
    }
    let available_release_tag =
        check_for_update(load_update_config().should_allow_prerelease_updates)?;
    let mut update_state = load_update_state();
    update_state.last_check_unix_seconds = Some(get_current_unix_seconds());
    let _ = save_update_state(&update_state);
    let Some(release_tag) = available_release_tag else {
        println!("koshi {APP_VERSION} is already the latest version");
        return Ok(());
    };
    install_update(release_install, &release_tag)
}

/// Runs `koshi restart-servers`: restart every running session, then the
/// router, into the koshi program each one runs from, as
/// `restart_servers_into_version` does, and expect each one back on
/// `APP_VERSION`, the version of this build. Prints one line per server.
///
/// # Errors
/// [`CliError::Runtime`] reading `not every running koshi server now runs
/// koshi 0.5.0; see the lines above` when any server did not come back on
/// `APP_VERSION`.
pub fn run_restart_servers_command() -> Result<(), CliError> {
    if restart_servers_into_version(APP_VERSION, RestartScope::EveryServer) {
        Ok(())
    } else {
        Err(CliError::Runtime {
            detail: format!(
                "not every running koshi server now runs koshi {APP_VERSION}; see the lines above"
            ),
        })
    }
}

/// On an interactive launch, delete the stale backups as
/// `delete_stale_backups` states. Then, when auto-check is enabled and a check
/// is due, look for a newer release and offer to install it. Every failure is
/// swallowed and the launch continues. Runs before the terminal enters raw
/// mode, and reads the answer from plain standard input.
///
/// A build from source, and a koshi whose path cannot be read, offer nothing.
/// A Homebrew install is offered stable releases only. A yes installs the
/// release as `install_update` states, then restarts the running servers as
/// `restart_servers_into_program_file` states with
/// `RestartScope::ServersNotOnVersion`, whatever the install did. An
/// install that worked then prints `relaunch koshi to use the new version`
/// and ends this process with status `0`.
pub fn prompt_startup_update() {
    delete_stale_backups();
    let update_config = load_update_config();
    if !update_config.should_auto_check_for_updates {
        return;
    }
    let Ok(program_path) = koshi_host::program_path::resolve_program_path() else {
        return;
    };
    let InstallSource::Release(release_install) =
        find_install_source(&program_path, IS_RELEASE_BUILD)
    else {
        return;
    };
    let mut update_state = load_update_state();
    if !is_update_due(&update_state, update_config.check_interval_days) {
        return;
    }
    // The attempt is recorded before the network call, whatever that call
    // answers: a check that fails or hangs is tried again only once a full
    // interval has passed.
    update_state.last_check_unix_seconds = Some(get_current_unix_seconds());
    let _ = save_update_state(&update_state);
    let should_allow_prerelease_updates = update_config.should_allow_prerelease_updates
        && !matches!(release_install, ReleaseInstall::Homebrew { .. });
    let release_tag = match check_for_update(should_allow_prerelease_updates) {
        Ok(Some(release_tag)) => release_tag,
        Ok(None) | Err(_) => return,
    };

    let update_prompt = format!(
        "koshi {} is available (you have {APP_VERSION}). Update now? [y/N] ",
        strip_version_prefix(&release_tag)
    );
    if !crate::prompt::read_yes_answer(&update_prompt) {
        return;
    }
    let install_result = install_update(&release_install, &release_tag);
    restart_servers_into_program_file(
        &program_path,
        RestartScope::ServersNotOnVersion {
            program_path: &program_path,
        },
    );
    match install_result {
        Ok(()) => {
            println!("relaunch koshi to use the new version");
            std::process::exit(0);
        }
        Err(update_install_error) => eprintln!("koshi: update failed: {update_install_error}"),
    }
}

/// Install the release `release_tag` the way `release_install` takes updates.
///
/// - Homebrew: `brew upgrade`, as [`upgrade_through_homebrew`] states.
/// - Scoop and a file no package manager tracks: the release archive swapped
///   in place, as [`install_release`] states, followed by `updated to koshi
///   <version>`. A Scoop install then prints on standard error `koshi: this
///   koshi came from Scoop, which still lists the version it installed;
///   `scoop update koshi` skips koshi while any koshi from this install runs,
///   and koshi update keeps this install current`.
///
/// # Errors
/// The failure of `brew` or of [`install_release`].
fn install_update(release_install: &ReleaseInstall, release_tag: &str) -> Result<(), String> {
    if let ReleaseInstall::Homebrew {
        brew_path,
        formula_name,
    } = release_install
    {
        return upgrade_through_homebrew(brew_path, formula_name);
    }
    install_release(release_tag)?;
    println!("updated to koshi {}", strip_version_prefix(release_tag));
    if *release_install == ReleaseInstall::Scoop {
        eprintln!(
            "koshi: this koshi came from Scoop, which still lists the version it installed; \
             `scoop update koshi` skips koshi while any koshi from this install runs, and koshi \
             update keeps this install current"
        );
    }
    Ok(())
}

/// Upgrade koshi through the `brew` program at `brew_path`: run `brew upgrade
/// gohyuhan/koshi/koshi` with this process's input and output.
///
/// # Errors
/// - A `formula_name` other than `koshi`, such as the pinned `koshi@0.5.0`:
///   `this koshi comes from the Homebrew formula koshi@0.5.0, which stays on
///   its version; move to the newest koshi with: brew install
///   gohyuhan/koshi/koshi`. Nothing runs.
/// - A `brew` that cannot be started: `<brew_path> could not be run:
///   <failure>; upgrade with: brew upgrade gohyuhan/koshi/koshi`.
/// - A `brew` that exits with a failure: `brew upgrade gohyuhan/koshi/koshi
///   failed: <exit status>`.
fn upgrade_through_homebrew(brew_path: &Path, formula_name: &str) -> Result<(), String> {
    if formula_name != HOMEBREW_FORMULA_NAME {
        return Err(format!(
            "this koshi comes from the Homebrew formula {formula_name}, which stays on its \
             version; move to the newest koshi with: brew install {HOMEBREW_TAPPED_FORMULA_NAME}"
        ));
    }
    let upgrade_status = std::process::Command::new(brew_path)
        .args(["upgrade", HOMEBREW_TAPPED_FORMULA_NAME])
        .status()
        .map_err(|brew_spawn_error| {
            format!(
                "{} could not be run: {brew_spawn_error}; upgrade with: brew upgrade \
                 {HOMEBREW_TAPPED_FORMULA_NAME}",
                brew_path.display()
            )
        })?;
    if !upgrade_status.success() {
        return Err(format!(
            "brew upgrade {HOMEBREW_TAPPED_FORMULA_NAME} failed: {upgrade_status}"
        ));
    }
    Ok(())
}

/// Restart the running servers into the koshi at `program_path`, as
/// [`restart_servers_into_version`] states with `restart_scope`, expecting
/// the version `<program_path> --version` prints. A version that cannot be
/// read prints `koshi: the running servers keep the builds they run:
/// <failure>; run koshi restart-servers to move them` and restarts nothing.
fn restart_servers_into_program_file(program_path: &Path, restart_scope: RestartScope) {
    match read_installed_version(program_path) {
        Ok(installed_version) => {
            let _ = restart_servers_into_version(&installed_version, restart_scope);
        }
        Err(version_read_error) => {
            eprintln!(
                "koshi: the running servers keep the builds they run: {version_read_error}; run \
                 koshi restart-servers to move them"
            );
        }
    }
}

/// Which running servers a restart asks to restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartScope<'program_path> {
    /// Every running server: `koshi restart-servers`, and `koshi update` on a
    /// build from source.
    EveryServer,
    /// Every running server, except a server whose program file says it
    /// already runs the expected version, and a session whose program file
    /// names another file than `program_path` once every symbolic link is
    /// followed: `koshi update` on a release install.
    ServersNotOnVersion { program_path: &'program_path Path },
}

/// How long a router or session has to answer the Restart request, and then
/// to come back answering Hello with the installed version.
const RESTART_CONFIRM_WAIT_DURATION: Duration = Duration::from_secs(20);

/// The pause between two Hello probes while waiting for the confirmation.
const RESTART_CONFIRM_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(200);

/// The hard time bound on one Hello probe. A probe that reaches a half-closed
/// named pipe on Windows reads as no answer once the bound runs out.
const RESTART_PROBE_TIMEOUT_DURATION: Duration = Duration::from_secs(2);

/// Restart the running sessions, then the router, that `restart_scope` names
/// into the koshi program each one runs from, as
/// [`restart_sessions_into_version`] and [`restart_router_into_version`] do,
/// while this process holds the update lock [`take_update_lock`] takes. The
/// lock is released once both return. A lock that cannot be taken runs both
/// restarts without it. Then the sessions of koshi 0.2.0 that the session
/// restart found go to [`end_sessions_without_restart_request`].
///
/// `true` when all three give `true`: every server that ran now runs
/// `expected_version`, or ended as [`restart_router_into_version`] and
/// [`end_sessions_without_restart_request`] state. A runtime directory that
/// cannot be resolved restarts nothing, prints `koshi: the running sessions
/// and the router could not be reached: <failure>`, and gives `false`.
fn restart_servers_into_version(expected_version: &str, restart_scope: RestartScope) -> bool {
    let runtime_directory = match ipc_client::resolve_runtime_directory() {
        Ok(runtime_directory) => runtime_directory,
        Err(runtime_directory_error) => {
            eprintln!(
                "koshi: the running sessions and the router could not be reached: \
                 {runtime_directory_error}"
            );
            return false;
        }
    };
    let update_lock_file = take_update_lock(&runtime_directory);
    let (has_every_session_restarted, sessions_without_restart_request) =
        restart_sessions_into_version(&runtime_directory, expected_version, restart_scope);
    let has_router_restarted =
        restart_router_into_version(&runtime_directory, expected_version, restart_scope);
    drop(update_lock_file);
    let has_every_session_ended =
        end_sessions_without_restart_request(&sessions_without_restart_request, expected_version);
    has_every_session_restarted && has_router_restarted && has_every_session_ended
}

/// Open the lock file at [`resolve_update_lock_path`] in `runtime_directory`,
/// creating it with mode `0600` on Unix, and take its exclusive lock, waiting
/// while another `koshi update` holds it. Hands back the open file, which
/// holds the lock until it is dropped.
///
/// A `runtime_directory` that does not exist gives `None`. Any other failure
/// prints `koshi: the update lock <path> could not be taken: <failure>` on
/// standard error and gives `None`.
fn take_update_lock(runtime_directory: &Path) -> Option<fs::File> {
    let update_lock_path = resolve_update_lock_path(runtime_directory);
    let update_lock_file = match open_lock_file(&update_lock_path) {
        Ok(update_lock_file) => update_lock_file,
        Err(open_error) if open_error.kind() == io::ErrorKind::NotFound => return None,
        Err(open_error) => {
            eprintln!(
                "koshi: the update lock {} could not be taken: {open_error}",
                update_lock_path.display()
            );
            return None;
        }
    };
    if let Err(lock_error) = update_lock_file.lock() {
        eprintln!(
            "koshi: the update lock {} could not be taken: {lock_error}",
            update_lock_path.display()
        );
        return None;
    }
    Some(update_lock_file)
}

/// Open the lock file at `lock_path` for reading and writing, creating it with
/// mode `0600` on Unix when it does not exist. An existing file keeps its
/// content.
///
/// # Errors
/// The failure to open or create the file.
fn open_lock_file(lock_path: &Path) -> io::Result<fs::File> {
    let mut lock_file_options = fs::File::options();
    lock_file_options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        lock_file_options.mode(0o600);
    }
    lock_file_options.open(lock_path)
}

/// Ask the router `runtime_directory` advertises to restart into the koshi
/// program it runs from, confirm the router now reports `expected_version`,
/// and say what happened.
///
/// Prints nothing when no router is running. With
/// [`RestartScope::ServersNotOnVersion`], a router whose program file says it
/// runs `expected_version` is not asked, and prints `the running router
/// already runs koshi <version>; every session keeps running`. Success is
/// printed only after the router's Hello reports `expected_version`. A router
/// that refuses this build's protocol version, and a router that answers in
/// the envelope of koshi 0.1.0 to 0.4.0, go to [`stop_incompatible_router`].
/// Once that router's process ended, a router of the program this process runs
/// starts at once, through [`start_router`], and [`report_started_router`]
/// says what it reports. Any other refusal, a router still on another build,
/// or no answer within [`RESTART_CONFIRM_WAIT_DURATION`] prints a note on
/// standard error.
///
/// `true` when no router runs, when the router runs `expected_version`, when
/// [`stop_incompatible_router`] gives
/// [`IncompatibleRouterStop::AlreadyOnVersion`], and when the router started
/// after it reports `expected_version`.
fn restart_router_into_version(
    runtime_directory: &Path,
    expected_version: &str,
    restart_scope: RestartScope,
) -> bool {
    let router_program_file = find_advertised_server_program_file(
        &resolve_router_program_file_path(runtime_directory),
        &resolve_router_endpoint_path(runtime_directory),
    );
    if matches!(restart_scope, RestartScope::ServersNotOnVersion { .. })
        && router_program_file.is_some_and(|router_program_file| {
            router_program_file.build_version == expected_version
        })
    {
        println!(
            "the running router already runs koshi {expected_version}; every session keeps running"
        );
        return true;
    }
    match restart_advertised_router(
        runtime_directory,
        expected_version,
        RESTART_CONFIRM_WAIT_DURATION,
    ) {
        Ok(None) => true,
        Ok(Some(VersionProbeOutcome::Installed)) => {
            println!(
                "the running router restarted into koshi {expected_version}; every session keeps running"
            );
            true
        }
        Ok(Some(VersionProbeOutcome::OtherVersion(reported_version))) => {
            eprintln!(
                "koshi: the running router still reports {reported_version} after the restart; it keeps \
                 serving that build; every session keeps running"
            );
            false
        }
        Ok(Some(VersionProbeOutcome::Silent)) => {
            eprintln!(
                "koshi: the router restart was not confirmed: no router answered within \
                 {} seconds; every session keeps running",
                RESTART_CONFIRM_WAIT_DURATION.as_secs()
            );
            false
        }
        Err(
            CliError::ProtocolVersionRefused {
                detail: refusal_detail,
            }
            | CliError::PreviousReleaseServer {
                detail: refusal_detail,
            },
        ) => match stop_incompatible_router(runtime_directory, expected_version, &refusal_detail) {
            IncompatibleRouterStop::Stopped { router_process_id } => report_started_router(
                router_process_id,
                expected_version,
                start_router(runtime_directory),
            ),
            IncompatibleRouterStop::AlreadyOnVersion => true,
            IncompatibleRouterStop::NotStopped => false,
        },
        Err(router_restart_error) => {
            eprintln!(
                "koshi: the running router could not be restarted: {router_restart_error}; it keeps serving the old \
                 build until it exits"
            );
            false
        }
    }
}

/// What [`stop_incompatible_router`] did with the router.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncompatibleRouterStop {
    /// The router's process, `router_process_id`, ended.
    Stopped { router_process_id: u32 },
    /// The router runs the expected version, newer than this build, and keeps
    /// running.
    AlreadyOnVersion,
    /// The router was not ended, for the reason printed on standard error.
    NotStopped,
}

/// End the router `runtime_directory` advertises, which this build cannot
/// talk to for the reason `refusal_detail` names, once
/// [`find_server_process_record`] confirms its process. Only the router's own
/// process ends: every session keeps running.
///
/// A router whose program file, as [`find_server_program_file`] reads it,
/// names a version newer than this build keeps running:
///
/// - `expected_version` prints `the running router already runs koshi
///   <version>; every session keeps running`, and gives
///   [`IncompatibleRouterStop::AlreadyOnVersion`].
/// - Any other newer version prints `koshi: the running router runs koshi
///   <version> from <path>, which is newer than this koshi <this version>; it
///   keeps running; every session keeps running`, and gives
///   [`IncompatibleRouterStop::NotStopped`].
///
/// [`IncompatibleRouterStop::Stopped`] once the router's process has ended,
/// with nothing printed. [`IncompatibleRouterStop::NotStopped`], with a note on
/// standard error, for an endpoint file that cannot be read, for a process the
/// proof does not confirm, which is left running, and for a process that still
/// runs [`PROCESS_STOP_GRACE_DURATION`] after koshi ended it.
fn stop_incompatible_router(
    runtime_directory: &Path,
    expected_version: &str,
    refusal_detail: &str,
) -> IncompatibleRouterStop {
    let router_endpoint_path = resolve_router_endpoint_path(runtime_directory);
    let router_endpoint_file = match EndpointFile::load_from_path(&router_endpoint_path) {
        Ok(router_endpoint_file) => router_endpoint_file,
        Err(endpoint_file_error) => {
            eprintln!(
                "koshi: the running router runs a koshi version this one cannot talk to \
                 ({refusal_detail}), and its endpoint file could not be read: {endpoint_file_error}"
            );
            return IncompatibleRouterStop::NotStopped;
        }
    };
    let router_process_id = router_endpoint_file.process_id;
    let router_program_file = find_server_program_file(
        &resolve_router_program_file_path(runtime_directory),
        router_process_id,
    )
    .ok()
    .flatten();
    match router_program_file {
        Some(router_program_file) if !is_release_newer(&router_program_file.build_version) => {}
        Some(router_program_file) if router_program_file.build_version == expected_version => {
            println!(
                "the running router already runs koshi {expected_version}; every session keeps \
                 running"
            );
            return IncompatibleRouterStop::AlreadyOnVersion;
        }
        Some(router_program_file) => {
            eprintln!(
                "koshi: the running router runs koshi {} from {}, which is newer than this koshi \
                 {APP_VERSION}; it keeps running; every session keeps running",
                sanitize_reported_text(&router_program_file.build_version),
                sanitize_reported_text(&router_program_file.program_path)
            );
            return IncompatibleRouterStop::NotStopped;
        }
        None => {}
    }
    let Some(router_record) = find_server_process_record(&router_endpoint_path, router_process_id)
    else {
        eprintln!(
            "koshi: the running router runs a koshi version this one cannot talk to \
             ({refusal_detail}); koshi cannot confirm that process {router_process_id} is the \
             router, and leaves it running. If it is, end it with: {}",
            format_process_kill_command(router_process_id)
        );
        return IncompatibleRouterStop::NotStopped;
    };
    let router_records = std::slice::from_ref(&router_record);
    process_tree::stop_processes(router_records, PROCESS_STOP_GRACE_DURATION);
    if !process_tree::wait_for_processes_to_end(router_records, PROCESS_STOP_GRACE_DURATION) {
        eprintln!(
            "koshi: the running router (process {router_process_id}) runs a koshi version this \
             one cannot talk to ({refusal_detail}), and still runs after koshi ended it"
        );
        return IncompatibleRouterStop::NotStopped;
    }
    IncompatibleRouterStop::Stopped { router_process_id }
}

/// Say what starting a router gave, after koshi ended the router
/// `router_process_id`, which ran a koshi version this one cannot talk to.
/// `started_router_version` is what [`start_router`] gave. `true` when the
/// started router reports `expected_version`.
///
/// - `expected_version` prints `koshi ended the running router (process
///   <id>), which ran a koshi version this one cannot talk to, and started a
///   router on koshi <version>; every session keeps running`.
/// - Another version prints `koshi: koshi ended the running router (process
///   <id>), which ran a koshi version this one cannot talk to; the router
///   started after it reports koshi <version>, not <expected version>; every
///   session keeps running` on standard error.
/// - A failure prints `koshi: koshi ended the running router (process <id>),
///   which ran a koshi version this one cannot talk to, and could not start a
///   new one: <failure>; the next koshi command starts one; every session keeps
///   running` on standard error.
fn report_started_router(
    router_process_id: u32,
    expected_version: &str,
    started_router_version: Result<String, CliError>,
) -> bool {
    let router_end_clause = format!(
        "koshi ended the running router (process {router_process_id}), which ran a koshi version \
         this one cannot talk to"
    );
    match started_router_version {
        Ok(started_router_version) if started_router_version == expected_version => {
            println!(
                "{router_end_clause}, and started a router on koshi {expected_version}; every \
                 session keeps running"
            );
            true
        }
        Ok(started_router_version) => {
            eprintln!(
                "koshi: {router_end_clause}; the router started after it reports koshi {}, not \
                 {expected_version}; every session keeps running",
                sanitize_reported_text(&started_router_version)
            );
            false
        }
        Err(router_start_error) => {
            eprintln!(
                "koshi: {router_end_clause}, and could not start a new one: {router_start_error}; \
                 the next koshi command starts one; every session keeps running"
            );
            false
        }
    }
}

/// Ask the router `runtime_directory` advertises to restart, and wait up to
/// `wait_duration` for its Hello to report `installed_version`.
///
/// `Ok(None)` means no router is running, and nothing restarted. A router that
/// answers nothing to the Restart request within `wait_duration`, and a
/// restarted router whose Hello answers nothing within `wait_duration`, give
/// [`VersionProbeOutcome::Silent`].
///
/// # Errors
/// The [`CliError`] [`restart_running_router`] gives: a router that refuses the
/// restart, or one that cannot be reached.
fn restart_advertised_router(
    runtime_directory: &Path,
    installed_version: &str,
    wait_duration: Duration,
) -> Result<Option<VersionProbeOutcome>, CliError> {
    let router_runtime_directory = runtime_directory.to_path_buf();
    let router_restart = run_peer_call_within(wait_duration, move || {
        restart_running_router(&router_runtime_directory)
    });
    match router_restart {
        Some(Ok(false)) => Ok(None),
        Some(Ok(true)) => Ok(Some(wait_for_version(
            installed_version,
            wait_duration,
            || probe_router_version(runtime_directory),
        ))),
        Some(Err(router_restart_error)) => Err(router_restart_error),
        None => Ok(Some(VersionProbeOutcome::Silent)),
    }
}

/// How the wait for a restarted router or session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum VersionProbeOutcome {
    /// It answered with the version the wait was for.
    Installed,
    /// The last version it answered with, which was another one.
    OtherVersion(String),
    /// It answered nothing before the wait ran out.
    Silent,
}

/// Poll `version_probe` until it reports `expected_version`, for up to
/// `wait_duration`.
///
/// A probe that gives no answer reports no version, and the poll continues.
fn wait_for_version(
    expected_version: &str,
    wait_duration: Duration,
    version_probe: impl Fn() -> Option<String>,
) -> VersionProbeOutcome {
    let wait_deadline = Instant::now() + wait_duration;
    let mut last_reported_version = None;
    loop {
        match version_probe() {
            Some(reported_version) if reported_version == expected_version => {
                return VersionProbeOutcome::Installed;
            }
            Some(reported_version) => last_reported_version = Some(reported_version),
            None => {}
        }
        if Instant::now() >= wait_deadline {
            return last_reported_version.map_or(
                VersionProbeOutcome::Silent,
                VersionProbeOutcome::OtherVersion,
            );
        }
        std::thread::sleep(RESTART_CONFIRM_POLL_INTERVAL_DURATION);
    }
}

/// Run `peer_call` on its own thread and hand back what it returns, or `None`
/// once `answer_wait_duration` runs out first. A call that runs out the wait
/// leaves its thread behind, and that thread ends when `peer_call` returns or
/// with this process.
fn run_peer_call_within<PeerAnswer: Send + 'static>(
    answer_wait_duration: Duration,
    peer_call: impl FnOnce() -> PeerAnswer + Send + 'static,
) -> Option<PeerAnswer> {
    let (peer_answer_sender, peer_answer_receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = peer_answer_sender.send(peer_call());
    });
    peer_answer_receiver.recv_timeout(answer_wait_duration).ok()
}

/// One probe of the running router's version, bounded by
/// [`RESTART_PROBE_TIMEOUT_DURATION`]. A probe that runs out the bound reads as
/// no answer. A router that refuses this build's protocol version reports the
/// version its program file holds, as [`find_advertised_server_program_file`]
/// reads it.
fn probe_router_version(runtime_directory: &Path) -> Option<String> {
    let runtime_directory = runtime_directory.to_path_buf();
    run_peer_call_within(
        RESTART_PROBE_TIMEOUT_DURATION,
        move || match find_running_router_version(&runtime_directory) {
            Ok(reported_version) => reported_version,
            Err(CliError::ProtocolVersionRefused { .. }) => find_advertised_server_program_file(
                &resolve_router_program_file_path(&runtime_directory),
                &resolve_router_endpoint_path(&runtime_directory),
            )
            .map(|router_program_file| router_program_file.build_version),
            Err(_) => None,
        },
    )
    .flatten()
}

/// One probe of the running session `session_id`'s version, bounded by
/// [`RESTART_PROBE_TIMEOUT_DURATION`]. A probe that runs out the bound reads as
/// no answer. A session that refuses this build's protocol version reports
/// the version its program file holds, as
/// [`find_advertised_server_program_file`] reads it. A session that answers in
/// the envelope of koshi 0.1.0 to 0.4.0 reports the build it names in the
/// Hello of that envelope, as [`find_previous_release_session_version`] reads
/// it: `0.4.0` from a koshi 0.4.0 session whose restart did not start.
fn probe_session_version(runtime_directory: &Path, session_id: SessionId) -> Option<String> {
    let runtime_directory = runtime_directory.to_path_buf();
    run_peer_call_within(
        RESTART_PROBE_TIMEOUT_DURATION,
        move || match find_running_session_version(&runtime_directory, None, session_id, None) {
            Ok(reported_version) => reported_version,
            Err(CliError::ProtocolVersionRefused { .. }) => find_advertised_server_program_file(
                &ServerProgramFile::resolve_session_program_file_path(
                    &runtime_directory,
                    session_id,
                ),
                &EndpointFile::resolve_endpoint_file_path(&runtime_directory, session_id),
            )
            .map(|session_program_file| session_program_file.build_version),
            Err(CliError::PreviousReleaseServer { .. }) => {
                find_previous_release_session_version(&runtime_directory, session_id)
                    .ok()
                    .flatten()
            }
            Err(_) => None,
        },
    )
    .flatten()
}

/// The result of asking one running session to restart.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionOutcome {
    /// The session restarted and now reports the installed version.
    Confirmed,
    /// The session restarted and still reports the version named here.
    StillOnVersion(String),
    /// The session answered nothing within the wait: not the Restart request,
    /// or not one Hello after it.
    Unconfirmed,
    /// The session refused the restart or could not be reached. Carries the
    /// sentence naming what went wrong.
    Failed(String),
    /// The session refused this build's protocol version, and did not
    /// restart by itself. Carries the sentence it refused with, followed by
    /// the command that ends it.
    Incompatible(String),
    /// The session's program file says it already runs the installed
    /// version. It was not asked to restart, or it refused this build's
    /// protocol version while running that version, newer than this build.
    AlreadyOnVersion,
    /// The session's program file, carried here, names another file than the
    /// one the update installed into. The session was not asked to restart.
    OnOtherProgramFile(ServerProgramFile),
    /// The session runs koshi 0.2.0 or one of its pre-releases, which has no
    /// restart request.
    WithoutRestartRequest,
}

/// Ask every session that `restart_scope` names to restart into the koshi
/// program it runs from, and print one line per session. The sessions are the
/// ones `runtime_directory` advertises, then the ones each runtime directory
/// of koshi 0.1.0 and 0.2.0 advertises, as [`list_other_runtime_directories`]
/// gives them.
///
/// Prints nothing for a session that is no longer listening. Success is printed
/// only after that session's Hello reports `expected_version`. A session
/// already on `expected_version` prints `<session id> already runs koshi
/// <version>; its panes keep running`. Every other result prints a note on
/// standard error. A session that refused this build's protocol version, and
/// did not restart by itself within the wait [`restart_running_session`]
/// makes, is not ended: its note names the command that ends it,
/// `koshi kill-session <session id>`. A session of koshi 0.2.0, which has no
/// restart request, prints nothing: it is handed back beside the directory
/// that advertises it. A directory that cannot be read prints `koshi: the
/// running sessions could not be listed: <path> could not be read: <failure>;
/// each keeps serving the old build until it is ended and started again`, and
/// restarts nothing in it.
///
/// The `bool` is `true` when every session asked now runs `expected_version`,
/// and when none was running. The sessions of koshi 0.2.0 do not change it.
fn restart_sessions_into_version(
    runtime_directory: &Path,
    expected_version: &str,
    restart_scope: RestartScope,
) -> (bool, Vec<(PathBuf, SessionId)>) {
    let mut has_every_session_restarted = true;
    let mut sessions_without_restart_request = Vec::new();
    let session_runtime_directories =
        std::iter::once(runtime_directory.to_path_buf()).chain(list_other_runtime_directories(
            koshi_paths::resolve_previous_release_runtime_directories(),
            runtime_directory,
        ));
    for session_runtime_directory in session_runtime_directories {
        let session_outcomes = match restart_advertised_sessions(
            &session_runtime_directory,
            expected_version,
            restart_scope,
            RESTART_CONFIRM_WAIT_DURATION,
        ) {
            Ok(session_outcomes) => session_outcomes,
            Err(unread_path) => {
                eprintln!(
                    "koshi: the running sessions could not be listed: {unread_path}; each keeps \
                     serving the old build until it is ended and started again"
                );
                has_every_session_restarted = false;
                continue;
            }
        };
        for (session_id, session_outcome) in session_outcomes {
            match session_outcome {
                SessionOutcome::Confirmed => println!(
                    "{session_id} restarted into koshi {expected_version}; its panes keep running"
                ),
                SessionOutcome::StillOnVersion(reported_version) => {
                    eprintln!(
                        "koshi: {session_id} still reports {reported_version} after the restart; it keeps serving \
                         that build; its panes keep running"
                    );
                    has_every_session_restarted = false;
                }
                SessionOutcome::Unconfirmed => {
                    eprintln!(
                        "koshi: the restart of {session_id} was not confirmed: it answered nothing \
                         within {} seconds; its panes keep running",
                        RESTART_CONFIRM_WAIT_DURATION.as_secs()
                    );
                    has_every_session_restarted = false;
                }
                SessionOutcome::Failed(error_detail) => {
                    eprintln!(
                        "koshi: {session_id} could not be restarted: {error_detail}; it keeps serving the old \
                         build until you end that session and start it again"
                    );
                    has_every_session_restarted = false;
                }
                SessionOutcome::Incompatible(refusal_detail) => {
                    eprintln!(
                        "koshi: {session_id} runs a koshi version this one cannot talk to: \
                         {refusal_detail}"
                    );
                    has_every_session_restarted = false;
                }
                SessionOutcome::AlreadyOnVersion => println!(
                    "{session_id} already runs koshi {expected_version}; its panes keep running"
                ),
                SessionOutcome::OnOtherProgramFile(session_program_file) => {
                    let running_version =
                        sanitize_reported_text(&session_program_file.build_version);
                    eprintln!(
                        "koshi: {session_id} runs koshi {running_version} from {}, a program file \
                         this koshi does not replace; it keeps running koshi {running_version}",
                        sanitize_reported_text(&session_program_file.program_path)
                    );
                    has_every_session_restarted = false;
                }
                SessionOutcome::WithoutRestartRequest => {
                    sessions_without_restart_request
                        .push((session_runtime_directory.clone(), session_id));
                }
            }
        }
    }
    (
        has_every_session_restarted,
        sessions_without_restart_request,
    )
}

/// Name the sessions in `sessions_without_restart_request`, each beside the
/// runtime directory that advertises it, which koshi 0.2.0 or one of its
/// pre-releases started and so cannot restart into `expected_version`. Then
/// ask whether to end them, as [`read_yes_answer`](crate::prompt::read_yes_answer)
/// asks: `koshi 0.6.0 cannot move session-<uuid>, which koshi 0.2.0 started.
/// End it and the programs in its panes? [y/N] `, or `cannot move
/// session-<uuid>, session-<uuid>, which ... End them and the programs in their
/// panes? [y/N] ` for several.
///
/// - Yes: from then on this process ignores `SIGHUP`, `SIGINT` and `SIGQUIT`,
///   or Ctrl+C on Windows, through
///   [`ignore_terminal_signals`](process_tree::ignore_terminal_signals). Each
///   session ends as [`end_session`] ends it, with every process started under
///   it, and prints `<session id> ran koshi 0.2.0; koshi ended it and the
///   programs in its panes`. A session that is already gone prints `<session
///   id> ran koshi 0.2.0 and no longer runs`. A session that cannot be ended
///   prints `koshi: <session id> could not be ended: <failure>` on standard
///   error. The session that `KOSHI_SESSION_ID` names, as
///   [`InSessionContext::from_env`] reads it, which runs the pane this command
///   runs in, ends after every other session. A line that cannot be written,
///   such as one to the terminal of a session that just ended, is dropped,
///   and the next session still ends.
/// - Any other answer, the end of standard input included: each session
///   prints `koshi: <session id> keeps running koshi 0.2.0, and so do its
///   panes; run koshi restart-servers again to end it` on standard error.
///
/// `true` when no session is left running. An empty
/// `sessions_without_restart_request` asks nothing and gives `true`.
fn end_sessions_without_restart_request(
    sessions_without_restart_request: &[(PathBuf, SessionId)],
    expected_version: &str,
) -> bool {
    if sessions_without_restart_request.is_empty() {
        return true;
    }
    let session_id_list = sessions_without_restart_request
        .iter()
        .map(|(_, session_id)| session_id.to_string())
        .collect::<Vec<String>>()
        .join(", ");
    let end_question = match sessions_without_restart_request.len() {
        1 => format!(
            "koshi {expected_version} cannot move {session_id_list}, which koshi 0.2.0 started. \
             End it and the programs in its panes? [y/N] "
        ),
        _ => format!(
            "koshi {expected_version} cannot move {session_id_list}, which koshi 0.2.0 started. \
             End them and the programs in their panes? [y/N] "
        ),
    };
    if !crate::prompt::read_yes_answer(&end_question) {
        for (_, session_id) in sessions_without_restart_request {
            eprintln!(
                "koshi: {session_id} keeps running koshi 0.2.0, and so do its panes; run koshi \
                 restart-servers again to end it"
            );
        }
        return false;
    }
    process_tree::ignore_terminal_signals();
    let own_pane_session_id = InSessionContext::from_env()
        .ok()
        .flatten()
        .map(|in_session_context| in_session_context.session_id);
    let (own_pane_sessions, other_sessions): (Vec<_>, Vec<_>) = sessions_without_restart_request
        .iter()
        .partition(|(_, session_id)| Some(*session_id) == own_pane_session_id);
    let mut has_every_session_ended = true;
    for (session_runtime_directory, session_id) in
        other_sessions.into_iter().chain(own_pane_sessions)
    {
        match end_session(session_runtime_directory, None, *session_id) {
            Ok(_) => {
                let _ = writeln!(
                    io::stdout(),
                    "{session_id} ran koshi 0.2.0; koshi ended it and the programs in its panes"
                );
            }
            Err(CliError::SessionNotFound { .. }) => {
                let _ = writeln!(
                    io::stdout(),
                    "{session_id} ran koshi 0.2.0 and no longer runs"
                );
            }
            Err(end_error) => {
                let _ = writeln!(
                    io::stderr(),
                    "koshi: {session_id} could not be ended: {end_error}"
                );
                has_every_session_ended = false;
            }
        }
    }
    has_every_session_ended
}

/// The directories of `candidate_runtime_directories` that are not
/// `runtime_directory`: neither the same path, nor a path that
/// [`fs::canonicalize`] resolves to the path `runtime_directory` resolves to.
/// A path that cannot be resolved, such as one that does not exist, is
/// compared as it is.
///
/// Example: candidates `/tmp/link` and `/home/user/.local/share/koshi/run`,
/// where `/tmp/link` is a symbolic link to the runtime directory `/tmp/run`,
/// give `/home/user/.local/share/koshi/run`.
#[must_use]
pub fn list_other_runtime_directories(
    candidate_runtime_directories: Vec<PathBuf>,
    runtime_directory: &Path,
) -> Vec<PathBuf> {
    let canonical_runtime_directory = fs::canonicalize(runtime_directory).ok();
    candidate_runtime_directories
        .into_iter()
        .filter(|candidate_runtime_directory| {
            if candidate_runtime_directory == runtime_directory {
                return false;
            }
            match (
                fs::canonicalize(candidate_runtime_directory),
                &canonical_runtime_directory,
            ) {
                (Ok(canonical_candidate_directory), Some(canonical_runtime_directory)) => {
                    canonical_candidate_directory != *canonical_runtime_directory
                }
                _ => true,
            }
        })
        .collect()
}

/// What runs from a runtime directory of koshi 0.1.0 and 0.2.0, as
/// [`count_previous_release_servers`] counts it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreviousReleaseServerCount {
    /// How many sessions run from the directory.
    pub session_count: usize,
    /// How many koshi 0.1.0 windows run from the directory.
    pub open_window_count: usize,
}

/// What runs from `previous_release_runtime_directory`, counted over each
/// endpoint file there, as [`ipc_client::list_advertised_sessions`] lists
/// them:
///
/// - A session is an endpoint file whose process passes the check
///   `koshi kill-session` makes: it runs as this user, runs a program that
///   [`is_koshi_executable_name`](process_tree::is_koshi_executable_name)
///   accepts, such as `koshi` or `koshi.old`, and started in the second the
///   endpoint file was last written or before it.
/// - An open window is the endpoint file of a koshi 0.1.0 window that
///   [`is_koshi_0_1_0_window_closed`](ipc_client::is_koshi_0_1_0_window_closed)
///   does not find closed.
///
/// A directory that cannot be read counts nothing, and so does every other
/// endpoint file.
#[must_use]
pub fn count_previous_release_servers(
    previous_release_runtime_directory: &Path,
) -> PreviousReleaseServerCount {
    let mut server_count = PreviousReleaseServerCount::default();
    let Ok(session_ids) = ipc_client::list_advertised_sessions(previous_release_runtime_directory)
    else {
        return server_count;
    };
    for session_id in session_ids {
        let endpoint_file_path = EndpointFile::resolve_endpoint_file_path(
            previous_release_runtime_directory,
            session_id,
        );
        match EndpointFile::load_from_path(&endpoint_file_path) {
            Ok(endpoint_file)
                if find_server_process_record(&endpoint_file_path, endpoint_file.process_id)
                    .is_some() =>
            {
                server_count.session_count += 1;
            }
            Err(IpcError::Koshi010WindowEndpointFile { .. })
                if !ipc_client::is_koshi_0_1_0_window_closed(
                    previous_release_runtime_directory,
                    session_id,
                ) =>
            {
                server_count.open_window_count += 1;
            }
            _ => {}
        }
    }
    server_count
}

/// The line `koshi list-sessions` prints on standard error for
/// `session_count` sessions that run from `previous_release_runtime_directory`,
/// a runtime directory of koshi 0.1.0 and 0.2.0, and `None` for `0`.
///
/// Example: `1` and `/home/user/.local/share/koshi/run` give `1 session that
/// an older koshi started runs from /home/user/.local/share/koshi/run, which
/// this koshi does not list; run koshi restart-servers to move it or end it`.
/// `2` gives `2 sessions that an older koshi started run from ..., which this
/// koshi does not list; run koshi restart-servers to move them or end them`.
#[must_use]
pub fn format_previous_release_session_note(
    previous_release_runtime_directory: &Path,
    session_count: usize,
) -> Option<String> {
    let previous_release_runtime_directory = previous_release_runtime_directory.display();
    match session_count {
        0 => None,
        1 => Some(format!(
            "1 session that an older koshi started runs from {previous_release_runtime_directory}, \
             which this koshi does not list; run koshi restart-servers to move it or end it"
        )),
        session_count => Some(format!(
            "{session_count} sessions that an older koshi started run from \
             {previous_release_runtime_directory}, which this koshi does not list; run koshi \
             restart-servers to move them or end them"
        )),
    }
}

/// The line `koshi list-sessions` prints on standard error for
/// `open_window_count` koshi 0.1.0 windows that run from
/// `previous_release_runtime_directory`, and `None` for `0`.
///
/// Example: `1` and `/home/user/.local/share/koshi/run` give `1 koshi 0.1.0
/// window runs from /home/user/.local/share/koshi/run; this koshi cannot talk
/// to it, and it ends when its terminal closes`. `2` gives `2 koshi 0.1.0
/// windows run from ...; this koshi cannot talk to them, and each one ends
/// when its terminal closes`.
#[must_use]
pub fn format_koshi_0_1_0_window_note(
    previous_release_runtime_directory: &Path,
    open_window_count: usize,
) -> Option<String> {
    let previous_release_runtime_directory = previous_release_runtime_directory.display();
    match open_window_count {
        0 => None,
        1 => Some(format!(
            "1 koshi 0.1.0 window runs from {previous_release_runtime_directory}; this koshi \
             cannot talk to it, and it ends when its terminal closes"
        )),
        open_window_count => Some(format!(
            "{open_window_count} koshi 0.1.0 windows run from \
             {previous_release_runtime_directory}; this koshi cannot talk to them, and each one \
             ends when its terminal closes"
        )),
    }
}

/// Ask every session `runtime_directory` advertises to restart, waiting up to
/// `wait_duration` on each for a Hello reporting `installed_version`, and hand
/// back each result.
///
/// With [`RestartScope::ServersNotOnVersion`], a session is not asked when
/// its program file, as [`find_advertised_server_program_file`] reads it,
/// says it runs `installed_version`, which gives
/// [`SessionOutcome::AlreadyOnVersion`], or names another file than
/// `program_path`, which gives [`SessionOutcome::OnOtherProgramFile`].
///
/// A session no longer listening is left out. A session that answers nothing
/// to the Restart request within `wait_duration` gives
/// [`SessionOutcome::Unconfirmed`]. A session that refuses this build's
/// protocol version gives [`SessionOutcome::AlreadyOnVersion`] when its
/// program file says it runs `installed_version` and that version is newer
/// than this build, and [`SessionOutcome::Incompatible`] otherwise. A session
/// of koshi 0.2.0 or older gives [`SessionOutcome::WithoutRestartRequest`].
/// One session's failure never ends the walk: every advertised session is
/// asked, whatever the one before it answered.
///
/// # Errors
/// [`UnreadPath`] naming `runtime_directory` when it cannot be read, other than
/// as missing. No session is asked then.
fn restart_advertised_sessions(
    runtime_directory: &Path,
    installed_version: &str,
    restart_scope: RestartScope,
    wait_duration: Duration,
) -> Result<Vec<(SessionId, SessionOutcome)>, UnreadPath> {
    let mut session_outcomes = Vec::new();
    for session_id in ipc_client::list_advertised_sessions(runtime_directory)? {
        let session_program_file_path =
            ServerProgramFile::resolve_session_program_file_path(runtime_directory, session_id);
        let session_endpoint_path =
            EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
        let left_running_outcome = match (
            restart_scope,
            find_advertised_server_program_file(&session_program_file_path, &session_endpoint_path),
        ) {
            (RestartScope::ServersNotOnVersion { .. }, Some(session_program_file))
                if session_program_file.build_version == installed_version =>
            {
                Some(SessionOutcome::AlreadyOnVersion)
            }
            (RestartScope::ServersNotOnVersion { program_path }, Some(session_program_file))
                if compare_program_files(
                    Path::new(&session_program_file.program_path),
                    program_path,
                ) == ProgramFileMatch::Other =>
            {
                Some(SessionOutcome::OnOtherProgramFile(session_program_file))
            }
            _ => None,
        };
        if let Some(left_running_outcome) = left_running_outcome {
            session_outcomes.push((session_id, left_running_outcome));
            continue;
        }
        let session_runtime_directory = runtime_directory.to_path_buf();
        let session_restart = run_peer_call_within(wait_duration, move || {
            restart_running_session(&session_runtime_directory, None, session_id)
        });
        let session_outcome = match session_restart {
            Some(Ok(SessionRestart::NotRunning)) => continue,
            Some(Ok(SessionRestart::WithoutRestartRequest)) => {
                SessionOutcome::WithoutRestartRequest
            }
            Some(Ok(SessionRestart::Restarting)) => {
                match wait_for_version(installed_version, wait_duration, || {
                    probe_session_version(runtime_directory, session_id)
                }) {
                    VersionProbeOutcome::Installed => SessionOutcome::Confirmed,
                    VersionProbeOutcome::OtherVersion(reported_version) => {
                        SessionOutcome::StillOnVersion(reported_version)
                    }
                    VersionProbeOutcome::Silent => SessionOutcome::Unconfirmed,
                }
            }
            Some(Err(CliError::ProtocolVersionRefused { .. }))
                if is_release_newer(installed_version)
                    && find_advertised_server_program_file(
                        &session_program_file_path,
                        &session_endpoint_path,
                    )
                    .is_some_and(|session_program_file| {
                        session_program_file.build_version == installed_version
                    }) =>
            {
                SessionOutcome::AlreadyOnVersion
            }
            Some(Err(CliError::ProtocolVersionRefused {
                detail: refusal_detail,
            })) => SessionOutcome::Incompatible(refusal_detail),
            Some(Err(session_restart_error)) => {
                SessionOutcome::Failed(session_restart_error.to_string())
            }
            None => SessionOutcome::Unconfirmed,
        };
        session_outcomes.push((session_id, session_outcome));
    }
    Ok(session_outcomes)
}

// ---------------------------------------------------------------------------
// Version check
// ---------------------------------------------------------------------------

/// One GitHub release, cut down to the fields the update check reads.
#[derive(Debug, Deserialize)]
struct Release {
    /// The git tag the release was cut from, e.g. `v0.2.0`.
    tag_name: String,
}

/// Returns the newer release tag when one is available, or `None` when this
/// build is already current.
fn check_for_update(should_allow_prerelease_updates: bool) -> Result<Option<String>, String> {
    let release_tag = fetch_latest_release(should_allow_prerelease_updates)?;
    Ok(is_release_newer(&release_tag).then_some(release_tag))
}

/// Fetches the newest eligible release tag. With pre-releases allowed it reads
/// the release list (pre-releases included) and picks the highest version by
/// semver, not the newest by date. Otherwise it reads the `latest` endpoint,
/// which GitHub limits to stable releases.
fn fetch_latest_release(should_allow_prerelease_updates: bool) -> Result<String, String> {
    if should_allow_prerelease_updates {
        let release_list_url =
            format!("https://api.github.com/repos/{RELEASE_REPOSITORY}/releases?per_page=25");
        find_highest_release_version(fetch_json(&release_list_url)?)
    } else {
        let latest_release_url =
            format!("https://api.github.com/repos/{RELEASE_REPOSITORY}/releases/latest");
        let release: Release = fetch_json(&latest_release_url)?;
        Ok(release.tag_name)
    }
}

/// The tag of the highest version among `releases` by semver order. A tag
/// that does not parse as a version is skipped; an empty or all-unparsable
/// list is `no releases found`.
///
/// `["v0.3.0-rc.2", "v0.3.0-rc.10", "v0.2.0"]` gives `v0.3.0-rc.10` —
/// publish dates play no part.
fn find_highest_release_version(releases: Vec<Release>) -> Result<String, String> {
    releases
        .into_iter()
        .filter_map(|release| {
            Version::parse(strip_version_prefix(&release.tag_name))
                .ok()
                .map(|parsed_version| (parsed_version, release.tag_name))
        })
        .max_by(|left_release, right_release| left_release.0.cmp(&right_release.0))
        .map(|(_, release_tag)| release_tag)
        .ok_or_else(|| "no releases found".to_string())
}

/// True when `release_tag`, such as `v0.6.0` or `0.6.0`, names a version
/// strictly newer than this build. A tag or build version that does not parse
/// as semver reads as not newer.
fn is_release_newer(release_tag: &str) -> bool {
    match (
        Version::parse(strip_version_prefix(release_tag)),
        Version::parse(strip_version_prefix(APP_VERSION)),
    ) {
        (Ok(latest_release_version), Ok(current_application_version)) => {
            latest_release_version > current_application_version
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Download, extract, install
// ---------------------------------------------------------------------------

/// Downloads the release archive `release_tag` names, verifies its checksum, unpacks the binary,
/// and swaps it for the running executable once it prints the version of `release_tag`, as
/// [`swap_executable`] states. The temp files are created exclusively under random names, and
/// are removed when their [`TempPath`] drops at the end of this function, whichever way it ends.
fn install_release(release_tag: &str) -> Result<(), String> {
    let archive_url = compute_binary_url(release_tag).ok_or_else(|| {
        format!(
            "no koshi release binary for {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let archive_file_name = archive_url
        .rsplit('/')
        .next()
        .filter(|archive_file_name| !archive_file_name.is_empty())
        .ok_or_else(|| "release archive URL has no file name".to_string())?;
    let checksums_url = format!(
        "https://github.com/{RELEASE_REPOSITORY}/releases/download/{release_tag}/checksums.txt"
    );
    println!("downloading koshi {} …", strip_version_prefix(release_tag));
    let checksums_file = download_release_file(&checksums_url, MAX_CHECKSUM_FILE_BYTE_COUNT)
        .map_err(|download_error| {
            format!(
                "could not download checksums.txt for release archive {archive_file_name}: {download_error}"
            )
        })?;
    let checksums_text = fs::read_to_string(&checksums_file).map_err(|read_error| {
        format!(
            "could not read checksums.txt for release archive {archive_file_name}: {read_error}"
        )
    })?;
    let expected_checksum = find_release_checksum(&checksums_text, archive_file_name)?;
    let release_archive = download_release_file(&archive_url, MAX_RELEASE_ARCHIVE_BYTE_COUNT)
        .map_err(|download_error| {
            format!("could not download release archive {archive_file_name}: {download_error}")
        })?;
    let release_binary = extract_verified_release_binary(
        release_archive.as_ref(),
        &archive_url,
        archive_file_name,
        &expected_checksum,
    )?;
    let executable_path = std::env::current_exe()
        .map_err(|executable_path_error| executable_path_error.to_string())?;
    swap_executable(release_binary.as_ref(), &executable_path, release_tag)
}

/// The download URL for this platform's release archive at `release_tag`, or `None`
/// when koshi ships no binary for this OS/arch. Archive name matches the
/// release convention `koshi-v{version}-{os}-{arch}.{ext}`.
fn compute_binary_url(release_tag: &str) -> Option<String> {
    let target_os_name = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        _ => return None,
    };
    let target_architecture_name = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        _ => return None,
    };
    let archive_extension = if target_os_name == "windows" {
        "zip"
    } else {
        "tar.gz"
    };
    let archive_file_name = format!(
        "koshi-v{}-{target_os_name}-{target_architecture_name}.{archive_extension}",
        strip_version_prefix(release_tag)
    );
    Some(format!(
        "https://github.com/{RELEASE_REPOSITORY}/releases/download/{release_tag}/{archive_file_name}"
    ))
}

/// Downloads `url` into a temp file and returns its path. The file is created
/// exclusively under a random name: it follows and truncates no existing file
/// or symbolic link. The response must not exceed `maximum_byte_count` bytes.
fn download_release_file(url: &str, maximum_byte_count: u64) -> Result<TempPath, String> {
    let mut http_response = build_http_agent(UPDATE_DOWNLOAD_TIMEOUT_DURATION)
        .get(url)
        .header("User-Agent", "koshi")
        .call()
        .map_err(|download_error| download_error.to_string())?;
    let mut release_file = Builder::new()
        .prefix("koshi-update-")
        .tempfile()
        .map_err(|release_file_tempfile_error| release_file_tempfile_error.to_string())?;
    let mut release_file_reader = http_response.body_mut().as_reader();
    copy_stream_with_byte_limit(
        &mut release_file_reader,
        release_file.as_file_mut(),
        maximum_byte_count,
    )
    .map_err(|release_file_copy_error| release_file_copy_error.to_string())?;
    Ok(release_file.into_temp_path())
}

/// Copies at most `maximum_byte_count` bytes and rejects a stream with more.
fn copy_stream_with_byte_limit(
    reader: &mut impl Read,
    writer: &mut impl io::Write,
    maximum_byte_count: u64,
) -> io::Result<()> {
    let copied_byte_count = {
        let mut limited_reader = reader.take(maximum_byte_count);
        io::copy(&mut limited_reader, writer)?
    };
    if copied_byte_count < maximum_byte_count {
        return Ok(());
    }

    let mut extra_byte = [0_u8; 1];
    if reader.read(&mut extra_byte)? == 0 {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("download response exceeds {maximum_byte_count} bytes"),
    ))
}

/// Returns the checksum for `archive_file_name` from a `shasum -a 256` file.
fn find_release_checksum(checksums_text: &str, archive_file_name: &str) -> Result<String, String> {
    let mut matching_checksum = None;
    for checksum_line in checksums_text.lines() {
        let mut checksum_fields = checksum_line.split_whitespace();
        let Some(checksum) = checksum_fields.next() else {
            continue;
        };
        let Some(checksum_file_name) = checksum_fields.next() else {
            continue;
        };
        if checksum_file_name != archive_file_name {
            continue;
        }
        if checksum_fields.next().is_some() {
            return Err(format!(
                "checksums.txt has a malformed row for release archive {archive_file_name}"
            ));
        }
        if checksum.len() != 64 || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!(
                "checksums.txt has an invalid SHA-256 checksum for release archive {archive_file_name}"
            ));
        }
        if matching_checksum.is_some() {
            return Err(format!(
                "checksums.txt has multiple rows for release archive {archive_file_name}"
            ));
        }
        matching_checksum = Some(checksum.to_ascii_lowercase());
    }
    matching_checksum
        .ok_or_else(|| format!("checksums.txt has no row for release archive {archive_file_name}"))
}

/// Hashes `archive_path` and compares it with the checksum from `checksums.txt`.
fn verify_release_archive(
    archive_path: &Path,
    archive_file_name: &str,
    expected_checksum: &str,
) -> Result<(), String> {
    let mut archive_file = fs::File::open(archive_path).map_err(|open_error| {
        format!("could not open release archive {archive_file_name}: {open_error}")
    })?;
    let mut checksum_hasher = Sha256::new();
    let mut archive_read_buffer = [0_u8; 64 * 1024];
    loop {
        let bytes_read = archive_file
            .read(&mut archive_read_buffer)
            .map_err(|read_error| {
                format!("could not hash release archive {archive_file_name}: {read_error}")
            })?;
        if bytes_read == 0 {
            break;
        }
        checksum_hasher.update(&archive_read_buffer[..bytes_read]);
    }
    let computed_checksum = koshi_ipc::bytes::format_hex(&checksum_hasher.finalize());
    if computed_checksum == expected_checksum {
        return Ok(());
    }
    Err(format!(
        "checksum mismatch for release archive {archive_file_name}: expected {expected_checksum}, computed {computed_checksum}"
    ))
}

/// Verifies the downloaded archive before unpacking its binary.
fn extract_verified_release_binary(
    archive_path: &Path,
    archive_url: &str,
    archive_file_name: &str,
    expected_checksum: &str,
) -> Result<TempPath, String> {
    verify_release_archive(archive_path, archive_file_name, expected_checksum)?;
    extract_release_binary(archive_path, archive_url)
}

/// Unpacks the koshi binary out of the downloaded archive to a temp file,
/// choosing the tar.gz or zip reader from the URL suffix.
fn extract_release_binary(archive_path: &Path, archive_url: &str) -> Result<TempPath, String> {
    if archive_url.ends_with(".zip") {
        extract_zip_archive(archive_path)
    } else {
        extract_tar_gz_archive(archive_path)
    }
}

/// Unpacks the binary from a gzip-compressed tar archive.
fn extract_tar_gz_archive(archive_path: &Path) -> Result<TempPath, String> {
    let archive_file = fs::File::open(archive_path)
        .map_err(|archive_open_error| archive_open_error.to_string())?;
    let mut tar_archive = tar::Archive::new(flate2::read::GzDecoder::new(archive_file));
    for archive_entry in tar_archive
        .entries()
        .map_err(|archive_entries_error| archive_entries_error.to_string())?
    {
        let mut archive_entry =
            archive_entry.map_err(|archive_entry_error| archive_entry_error.to_string())?;
        // Only a regular file counts: a directory or symlink named `koshi` is
        // skipped.
        if !archive_entry.header().entry_type().is_file() {
            continue;
        }
        let is_koshi_binary = archive_entry
            .path()
            .ok()
            .and_then(|entry_path| {
                entry_path
                    .file_name()
                    .map(|file_name| file_name == get_binary_file_name())
            })
            .unwrap_or(false);
        if is_koshi_binary {
            return save_extracted_binary(&mut archive_entry);
        }
    }
    Err("binary not found in archive".to_string())
}

/// Unpacks the binary from a zip archive.
fn extract_zip_archive(archive_path: &Path) -> Result<TempPath, String> {
    let archive_file = fs::File::open(archive_path)
        .map_err(|archive_open_error| archive_open_error.to_string())?;
    let mut zip_archive = zip::ZipArchive::new(archive_file)
        .map_err(|zip_archive_error| zip_archive_error.to_string())?;
    for archive_entry_index in 0..zip_archive.len() {
        let mut archive_entry = zip_archive
            .by_index(archive_entry_index)
            .map_err(|archive_entry_error| archive_entry_error.to_string())?;
        let is_koshi_binary = Path::new(archive_entry.name())
            .file_name()
            .map(|file_name| file_name == get_binary_file_name())
            .unwrap_or(false);
        if is_koshi_binary {
            return save_extracted_binary(&mut archive_entry);
        }
    }
    Err("binary not found in archive".to_string())
}

/// Copies an extracted binary stream to a temp file, made executable on Unix.
/// The file is created exclusively under a random name: it follows and
/// truncates no existing file or symbolic link.
fn save_extracted_binary(binary_source: &mut impl Read) -> Result<TempPath, String> {
    let mut binary_file = Builder::new()
        .prefix("koshi-update-")
        .tempfile()
        .map_err(|binary_tempfile_error| binary_tempfile_error.to_string())?;
    io::copy(binary_source, binary_file.as_file_mut())
        .map_err(|binary_copy_error| binary_copy_error.to_string())?;
    #[cfg(unix)]
    set_executable_permissions(binary_file.path())?;
    Ok(binary_file.into_temp_path())
}

/// The binary's file name inside a release archive on this platform.
fn get_binary_file_name() -> &'static str {
    if cfg!(windows) {
        "koshi.exe"
    } else {
        "koshi"
    }
}

/// On Windows, deletes the backups beside the file the running executable's
/// path names once every symbolic link and junction in it is followed, as
/// [`delete_stale_backups_beside`] states. A path that cannot be read or
/// followed deletes nothing. A no-op on other platforms, where the swap leaves
/// no file behind.
fn delete_stale_backups() {
    #[cfg(windows)]
    if let Ok(executable_path) = std::env::current_exe().and_then(fs::canonicalize) {
        delete_stale_backups_beside(&executable_path);
    }
}

/// Deletes each backup that [`list_backup_executable_paths`] lists beside the
/// program file at `executable_path`, while this process holds the install
/// lock. When at least one backup is listed, opens the lock file that
/// [`compute_install_lock_path`] names, creating it when it does not exist,
/// and takes its exclusive lock without waiting. Nothing is deleted when the
/// lock file cannot be opened or locked, such as while another process holds
/// the lock. A backup that a running process still runs from cannot be
/// deleted, and stays.
///
/// Example: beside `koshi.exe`, `koshi.old` and `koshi.2.old` are deleted, and
/// `koshi.lock` is created. While `install.ps1` holds `koshi.lock`, both
/// backups stay.
#[cfg(any(windows, test))]
fn delete_stale_backups_beside(executable_path: &Path) {
    let backup_executable_paths = list_backup_executable_paths(executable_path);
    if backup_executable_paths.is_empty() {
        return;
    }
    let Ok(install_lock_file) = open_lock_file(&compute_install_lock_path(executable_path)) else {
        return;
    };
    if install_lock_file.try_lock().is_err() {
        return;
    }
    for backup_executable_path in backup_executable_paths {
        let _ = fs::remove_file(backup_executable_path);
    }
}

/// The install lock file of the program file at `executable_path`:
/// `<stem>.lock` beside it. `install.ps1`, the Windows swap, and
/// [`delete_stale_backups_beside`] lock this file before they rename the
/// program file or delete a backup.
///
/// Example: `C:\koshi\koshi.exe` gives `C:\koshi\koshi.lock`.
#[cfg(any(windows, test))]
fn compute_install_lock_path(executable_path: &Path) -> PathBuf {
    executable_path.with_extension("lock")
}

/// Open the install lock file that [`compute_install_lock_path`] names for the
/// program file at `executable_path`, creating it when it does not exist, and
/// take its exclusive lock. While another process holds the lock, such as
/// `install.ps1` or another `koshi update`, prints `koshi: waiting while
/// another koshi install holds <path>` on standard error, then waits until
/// the lock is free. Hands back the open file, which holds the lock until it
/// is dropped.
///
/// # Errors
/// `the install lock <path> could not be taken: <failure>` when the lock file
/// cannot be opened or locked.
#[cfg(any(windows, test))]
fn take_install_lock(executable_path: &Path) -> Result<fs::File, String> {
    let install_lock_path = compute_install_lock_path(executable_path);
    let format_install_lock_error = |lock_error: io::Error| {
        format!(
            "the install lock {} could not be taken: {lock_error}",
            install_lock_path.display()
        )
    };
    let install_lock_file =
        open_lock_file(&install_lock_path).map_err(format_install_lock_error)?;
    match install_lock_file.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => {
            eprintln!(
                "koshi: waiting while another koshi install holds {}",
                install_lock_path.display()
            );
            install_lock_file
                .lock()
                .map_err(format_install_lock_error)?;
        }
        Err(fs::TryLockError::Error(lock_error)) => {
            return Err(format_install_lock_error(lock_error));
        }
    }
    Ok(install_lock_file)
}

/// The backup path number `backup_index` of the program file at
/// `executable_path`: `<stem>.old` for `0`, and `<stem>.<backup_index>.old`
/// for every other number.
///
/// Example: `C:\koshi\koshi.exe` and `0` give `C:\koshi\koshi.old`, and `2`
/// gives `C:\koshi\koshi.2.old`.
#[cfg(any(windows, test))]
fn compute_backup_executable_path(executable_path: &Path, backup_index: u64) -> PathBuf {
    match backup_index {
        0 => executable_path.with_extension("old"),
        backup_index => executable_path.with_extension(format!("{backup_index}.old")),
    }
}

/// Delete what stands at the backup path of the program file at
/// `executable_path`, and give that path, which the Windows swap renames the
/// program file to: the path [`compute_backup_executable_path`] gives for the
/// lowest number, from `0` up, where no file is left. A file found at a path
/// is deleted first. A file that cannot be deleted, such as a backup that a
/// running process still runs from, moves the search to the next number.
///
/// Example: a process still runs from `koshi.old`, and `koshi.1.old` does not
/// exist: `koshi.1.old`.
///
/// # Errors
/// The failure to read the metadata of a path, other than a missing path.
#[cfg(any(windows, test))]
fn prepare_backup_executable_path(executable_path: &Path) -> io::Result<PathBuf> {
    let mut backup_index: u64 = 0;
    loop {
        let backup_executable_path = compute_backup_executable_path(executable_path, backup_index);
        match fs::symlink_metadata(&backup_executable_path) {
            Err(metadata_error) if metadata_error.kind() == io::ErrorKind::NotFound => {
                return Ok(backup_executable_path);
            }
            Err(metadata_error) => return Err(metadata_error),
            Ok(_) if fs::remove_file(&backup_executable_path).is_ok() => {
                return Ok(backup_executable_path);
            }
            Ok(_) => backup_index += 1,
        }
    }
}

/// Every backup beside the program file at `executable_path`: each entry of
/// its directory whose name
/// [`is_backup_program_file_name`](koshi_host::program_path::is_backup_program_file_name)
/// accepts with the stem of that file, `<stem>.old` or `<stem>.<n>.old`. A
/// directory that cannot be read lists nothing, and an entry that cannot be
/// read is left out.
///
/// Example: beside `koshi.exe`, the entries `koshi.old`, `koshi.3.old`,
/// `koshi.x.old` and `notes.old` give `koshi.old` and `koshi.3.old`.
#[cfg(any(windows, test))]
fn list_backup_executable_paths(executable_path: &Path) -> Vec<PathBuf> {
    let (Some(program_directory), Some(program_stem)) = (
        executable_path.parent(),
        executable_path
            .file_stem()
            .and_then(|program_stem| program_stem.to_str()),
    ) else {
        return Vec::new();
    };
    let Ok(directory_entries) = fs::read_dir(program_directory) else {
        return Vec::new();
    };
    directory_entries
        .filter_map(Result::ok)
        .map(|directory_entry| directory_entry.path())
        .filter(|entry_path| {
            entry_path
                .file_name()
                .and_then(|entry_name| entry_name.to_str())
                .is_some_and(|entry_name| {
                    koshi_host::program_path::is_backup_program_file_name(entry_name, program_stem)
                })
        })
        .collect()
}

/// The file name of a staged copy beside a program file: `name_prefix`, the
/// id of the process that runs the update, then `name_suffix`.
/// [`swap_executable`], `install.sh`, and `install.ps1` write their copies
/// under this name.
///
/// Example: the process `5000` names its copy of `/usr/local/bin/koshi`
/// `koshi.koshi-update-5000` on Linux and macOS, and its copy of
/// `C:\koshi\koshi.exe` `koshi-staged-5000.exe` on Windows.
struct StagedCopyName {
    /// `<program file name>.koshi-update-` on Linux and macOS, with `koshi` for
    /// a program file name that is not UTF-8. `koshi-staged-` on Windows.
    name_prefix: String,
    /// Empty on Linux and macOS. `.exe` on Windows.
    name_suffix: &'static str,
}

impl StagedCopyName {
    /// The staged copy name of the program file at `executable_path`.
    fn from_program_path(executable_path: &Path) -> StagedCopyName {
        if cfg!(windows) {
            return StagedCopyName {
                name_prefix: "koshi-staged-".to_string(),
                name_suffix: ".exe",
            };
        }
        let program_file_name = executable_path
            .file_name()
            .and_then(|file_name| file_name.to_str())
            .unwrap_or("koshi");
        StagedCopyName {
            name_prefix: format!("{program_file_name}.koshi-update-"),
            name_suffix: "",
        }
    }

    /// The file name that the process `process_id` gives its staged copy.
    fn format_file_name(&self, process_id: u32) -> String {
        format!("{}{process_id}{}", self.name_prefix, self.name_suffix)
    }

    /// The process id in `entry_name` when it is a staged copy name: one or
    /// more ASCII digits that fit a `u32`, between `name_prefix` and
    /// `name_suffix`. `None` for every other name.
    ///
    /// Example: for `koshi`, `koshi.koshi-update-5000` gives `5000`, and
    /// `koshi.koshi-update-+5` and `koshi-dev.koshi-update-5000` give `None`.
    fn parse_process_id(&self, entry_name: &str) -> Option<u32> {
        let process_id_text = entry_name
            .strip_prefix(self.name_prefix.as_str())?
            .strip_suffix(self.name_suffix)?;
        if !process_id_text
            .bytes()
            .all(|process_id_byte| process_id_byte.is_ascii_digit())
        {
            return None;
        }
        process_id_text.parse().ok()
    }
}

/// Lists each staged copy beside the program file at `executable_path` that
/// an update which no longer runs left: each entry of its directory whose name
/// [`StagedCopyName::parse_process_id`] reads a process id from, when
/// [`process_tree::is_process_id_free`] finds no process with that id. On
/// Windows, a name is read as [`StagedCopyName::from_program_path`] gives it,
/// and as `koshi-update-<process id>.exe`, the name `koshi update` of koshi
/// 0.5.0 gives its copy. The copy of a running process is left out, and so is
/// every other entry. A directory that cannot be read lists nothing. A symbolic
/// link with a staged copy name is listed under the same rule.
///
/// Example: beside `koshi`, `koshi.koshi-update-5000` is listed when no
/// process has the id `5000`, and left out while process `5000` runs. Beside
/// `koshi.exe`, `koshi-staged-5000.exe` and `koshi-update-5000.exe` are each
/// listed under the same rule.
fn list_staged_copies_of_ended_updates(executable_path: &Path) -> Vec<PathBuf> {
    let Some(program_directory) = executable_path.parent() else {
        return Vec::new();
    };
    let Ok(directory_entries) = fs::read_dir(program_directory) else {
        return Vec::new();
    };
    let mut staged_copy_names = vec![StagedCopyName::from_program_path(executable_path)];
    if cfg!(windows) {
        staged_copy_names.push(StagedCopyName {
            name_prefix: "koshi-update-".to_string(),
            name_suffix: ".exe",
        });
    }
    let mut ended_staged_copy_paths = Vec::new();
    for directory_entry in directory_entries.filter_map(Result::ok) {
        let entry_path = directory_entry.path();
        let Some(writer_process_id) = entry_path
            .file_name()
            .and_then(|entry_name| entry_name.to_str())
            .and_then(|entry_name| {
                staged_copy_names
                    .iter()
                    .find_map(|staged_copy_name| staged_copy_name.parse_process_id(entry_name))
            })
        else {
            continue;
        };
        if process_tree::is_process_id_free(writer_process_id) {
            ended_staged_copy_paths.push(entry_path);
        }
    }
    ended_staged_copy_paths
}

/// Deletes each staged copy that [`list_staged_copies_of_ended_updates`] lists
/// beside the program file at `executable_path`. A copy that cannot be
/// deleted stays. A symbolic link is deleted, and its target stays.
fn delete_staged_copies_of_ended_updates(executable_path: &Path) {
    for ended_staged_copy_path in list_staged_copies_of_ended_updates(executable_path) {
        let _ = fs::remove_file(ended_staged_copy_path);
    }
}

/// Runs `<binary_path> --version`, as [`read_installed_version`] states, and
/// checks that it prints `koshi <release version>`. The release version is
/// `release_tag` without its leading `v`: `0.6.0` for `v0.6.0`.
///
/// # Errors
/// - A binary whose version cannot be read: `the new koshi <release version>
///   does not run on this system: <failure>`. Example failure: `the binary at
///   /usr/local/bin/koshi.koshi-update-5000 printed "" for --version`.
/// - A binary that prints another version: `the new koshi prints version
///   <printed version>, not <release version>`.
fn validate_release_binary(binary_path: &Path, release_tag: &str) -> Result<(), String> {
    let release_version = strip_version_prefix(release_tag);
    let printed_version = read_installed_version(binary_path).map_err(|version_read_error| {
        format!("the new koshi {release_version} does not run on this system: {version_read_error}")
    })?;
    if printed_version != release_version {
        return Err(format!(
            "the new koshi prints version {printed_version}, not {release_version}"
        ));
    }
    Ok(())
}

/// Replace the program file on Unix: the file `executable_path` names once
/// every symbolic link in it is followed. A symbolic link at
/// `executable_path` keeps naming that file.
///
/// Deletes the staged copies that updates which no longer run left beside that
/// file, as [`delete_staged_copies_of_ended_updates`] states. Then copies
/// `new_binary` beside that file as `<name>.koshi-update-<pid>`, sets mode
/// `0755` on the copy, checks that the copy prints the version of
/// `release_tag`, as [`validate_release_binary`] states, and renames the copy
/// over the file in one step. A process that runs the old file keeps running
/// it. A failed copy, mode change, check, or rename removes the copy. A copy
/// refused with a permission error then installs through `sudo`, as
/// [`replace_with_sudo`] states. Every other failure leaves the program file
/// as it was.
///
/// # Errors
/// The failure of the path lookup, the copy, the mode change, the check, or
/// the rename, as text.
#[cfg(unix)]
fn swap_executable(
    new_binary: &Path,
    executable_path: &Path,
    release_tag: &str,
) -> Result<(), String> {
    let executable_path = &fs::canonicalize(executable_path)
        .map_err(|executable_path_error| executable_path_error.to_string())?;
    delete_staged_copies_of_ended_updates(executable_path);
    let staged_copy_name = StagedCopyName::from_program_path(executable_path);
    let staged_binary_path =
        executable_path.with_file_name(staged_copy_name.format_file_name(std::process::id()));
    if let Err(copy_error) = fs::copy(new_binary, &staged_binary_path) {
        let _ = fs::remove_file(&staged_binary_path);
        // A copy refused for permission, such as into a root-owned
        // `/usr/local/bin`, installs through `sudo`.
        if copy_error.kind() == io::ErrorKind::PermissionDenied {
            return replace_with_sudo(
                new_binary,
                &staged_binary_path,
                executable_path,
                release_tag,
            );
        }
        return Err(copy_error.to_string());
    }
    let replace_result = set_executable_permissions(&staged_binary_path)
        .and_then(|()| validate_release_binary(&staged_binary_path, release_tag))
        .and_then(|()| {
            fs::rename(&staged_binary_path, executable_path)
                .map_err(|rename_error| rename_error.to_string())
        });
    if let Err(replace_error) = replace_result {
        let _ = fs::remove_file(&staged_binary_path);
        return Err(replace_error);
    }
    Ok(())
}

/// Replace the program file on Windows: the file `executable_path` names once
/// every symbolic link and junction in it is followed. A symbolic link at
/// `executable_path` keeps naming that file.
///
/// Deletes the staged copies that updates which no longer run left beside that
/// file, as [`delete_staged_copies_of_ended_updates`] states. Then copies
/// `new_binary` beside that file as `koshi-staged-<pid>.exe`, checks that the
/// copy prints the version of `release_tag`, as [`validate_release_binary`]
/// states, and puts the copy in place of the file, as
/// [`replace_program_file_with_staged_copy`] states. A failed copy, check, or
/// replacement removes the copy.
///
/// # Errors
/// The failure of the path lookup, the copy, the check, or the replacement, as
/// text.
#[cfg(windows)]
fn swap_executable(
    new_binary: &Path,
    executable_path: &Path,
    release_tag: &str,
) -> Result<(), String> {
    let executable_path = &fs::canonicalize(executable_path)
        .map_err(|executable_path_error| executable_path_error.to_string())?;
    delete_staged_copies_of_ended_updates(executable_path);
    // The copy sits beside the program file, on the same volume.
    let staged_copy_name = StagedCopyName::from_program_path(executable_path);
    let staged_binary_path =
        executable_path.with_file_name(staged_copy_name.format_file_name(std::process::id()));
    let replace_result = fs::copy(new_binary, &staged_binary_path)
        .map_err(|staged_copy_error| staged_copy_error.to_string())
        .and_then(|_copied_byte_count| validate_release_binary(&staged_binary_path, release_tag))
        .and_then(|()| replace_program_file_with_staged_copy(&staged_binary_path, executable_path));
    if let Err(replace_error) = replace_result {
        let _ = fs::remove_file(&staged_binary_path);
        return Err(replace_error);
    }
    Ok(())
}

/// Put the staged copy at `staged_binary_path` in place of the program file at
/// `executable_path`, while this process holds the install lock, as
/// [`take_install_lock`] states. Renames the program file to the backup path
/// [`prepare_backup_executable_path`] gives, such as `koshi.old`, renames the
/// copy into its place, then deletes the backup. When the second rename fails,
/// the backup is renamed back, and a restore that fails too returns both errors
/// and the backup path. A backup that a running process still runs from stays,
/// and [`delete_stale_backups`] removes it at the first interactive launch
/// after that process ends. Every failure leaves the copy where it is.
///
/// Example: `C:\koshi\koshi-staged-5000.exe` and `C:\koshi\koshi.exe` leave
/// `C:\koshi\koshi.exe` with the bytes of the copy, and no `koshi.old`.
///
/// # Errors
/// The failure of the install lock, the backup path lookup, or a rename, as
/// text.
#[cfg(any(windows, test))]
fn replace_program_file_with_staged_copy(
    staged_binary_path: &Path,
    executable_path: &Path,
) -> Result<(), String> {
    let _install_lock_file = take_install_lock(executable_path)?;
    let backup_executable_path = prepare_backup_executable_path(executable_path)
        .map_err(|backup_path_error| backup_path_error.to_string())?;
    fs::rename(executable_path, &backup_executable_path)
        .map_err(|backup_rename_error| backup_rename_error.to_string())?;
    if let Err(staged_rename_error) = fs::rename(staged_binary_path, executable_path) {
        if let Err(rollback_error) = fs::rename(&backup_executable_path, executable_path) {
            return Err(format!(
                "could not install replacement at {}: {staged_rename_error}; could not restore the original executable: {rollback_error}; manual recovery: restore {} as {}",
                executable_path.display(),
                backup_executable_path.display(),
                executable_path.display()
            ));
        }
        return Err(staged_rename_error.to_string());
    }
    // A backup that a running process still runs from stays, and
    // `delete_stale_backups` removes it at the first interactive launch after
    // that process ends.
    let _ = fs::remove_file(&backup_executable_path);
    Ok(())
}

/// The `sh` program that [`replace_with_sudo`] runs as root to write the
/// staged copy, with the arguments that [`list_staged_copy_write_arguments`]
/// lists. `$1` is the new release, `$2` the staged copy path, and every
/// further argument a staged copy to delete.
///
/// Deletes each staged copy, copies `$1` to `$2`, and sets mode `0755` on the
/// copy, then exits with status `0`. A failed copy or mode change removes the
/// copy and exits with status `1`. A staged copy that cannot be deleted stays,
/// and the copy is still written.
#[cfg(unix)]
const STAGED_COPY_WRITE_SCRIPT: &str = r#"new_binary=$1
staged_binary=$2
shift 2
rm -f -- "$@"
if cp "$new_binary" "$staged_binary" && chmod 755 "$staged_binary"; then
  exit 0
fi
rm -f "$staged_binary"
exit 1"#;

/// The arguments of `sh` that run [`STAGED_COPY_WRITE_SCRIPT`]: `-c`, the
/// script, `sh` as the name the script runs under, `new_binary`,
/// `staged_binary_path`, then each path of `ended_staged_copy_paths`.
#[cfg(unix)]
fn list_staged_copy_write_arguments(
    new_binary: &Path,
    staged_binary_path: &Path,
    ended_staged_copy_paths: &[PathBuf],
) -> Vec<std::ffi::OsString> {
    let mut write_arguments: Vec<std::ffi::OsString> = vec![
        "-c".into(),
        STAGED_COPY_WRITE_SCRIPT.into(),
        "sh".into(),
        new_binary.into(),
        staged_binary_path.into(),
    ];
    write_arguments.extend(ended_staged_copy_paths.iter().map(Into::into));
    write_arguments
}

/// The `sh` program that [`replace_with_sudo`] runs as root to rename the
/// staged copy, with the arguments that [`list_staged_copy_rename_arguments`]
/// lists. `$1` is the staged copy path, and `$2` the program file.
///
/// Renames `$1` over `$2` in one step and exits with status `0`. A failed
/// rename removes `$1` and exits with status `1`.
#[cfg(unix)]
const STAGED_COPY_RENAME_SCRIPT: &str = r#"if mv -f "$1" "$2"; then
  exit 0
fi
rm -f "$1"
exit 1"#;

/// The arguments of `sh` that run [`STAGED_COPY_RENAME_SCRIPT`]: `-c`, the
/// script, `sh` as the name the script runs under, `staged_binary_path`, and
/// `executable_path`.
#[cfg(unix)]
fn list_staged_copy_rename_arguments(
    staged_binary_path: &Path,
    executable_path: &Path,
) -> Vec<std::ffi::OsString> {
    vec![
        "-c".into(),
        STAGED_COPY_RENAME_SCRIPT.into(),
        "sh".into(),
        staged_binary_path.into(),
        executable_path.into(),
    ]
}

/// Replaces the program file at `executable_path` through `sudo`, for a
/// program file in a directory that only root may write. Prints `koshi:
/// updating <path> needs elevated permissions` on standard error. Then:
///
/// 1. Runs [`STAGED_COPY_WRITE_SCRIPT`] as root in one `sudo sh` command: it
///    deletes each staged copy that [`list_staged_copies_of_ended_updates`]
///    lists, copies `new_binary` to `staged_binary_path`, and sets mode
///    `0755` on the copy.
/// 2. Checks, as this user, that the copy prints the version of
///    `release_tag`, as [`validate_release_binary`] states.
/// 3. Runs [`STAGED_COPY_RENAME_SCRIPT`] as root in a second `sudo sh`
///    command: it renames the copy over `executable_path` in one step.
///
/// A process that runs the old file keeps running it. A failed copy or mode
/// change in step 1 removes the copy there. When step 2 or step 3 fails,
/// including a `sudo` that refuses step 3 or cannot start, and the copy is
/// still there, `sudo rm -f <staged_binary_path>` deletes it. Every failure
/// leaves the program file as it was.
///
/// # Errors
/// The failure to start `sudo`, as text. `` `sudo` could not replace <path> ``
/// when `sudo` or a script exits with a status other than `0`. The failure of
/// the check, as [`validate_release_binary`] states. When the copy is still
/// there after `sudo rm -f`, the error ends with `; the copy <staged path>
/// stays: delete it with sudo rm -f <staged path>`.
///
/// Example: the second `sudo` refuses three wrong passwords, and so does
/// `sudo rm -f`: `` `sudo` could not replace /usr/local/bin/koshi; the copy
/// /usr/local/bin/koshi.koshi-update-5000 stays: delete it with sudo rm -f
/// /usr/local/bin/koshi.koshi-update-5000 ``.
#[cfg(unix)]
fn replace_with_sudo(
    new_binary: &Path,
    staged_binary_path: &Path,
    executable_path: &Path,
    release_tag: &str,
) -> Result<(), String> {
    eprintln!(
        "koshi: updating {} needs elevated permissions",
        executable_path.display()
    );
    let ended_staged_copy_paths = list_staged_copies_of_ended_updates(executable_path);
    let write_command_status = std::process::Command::new("sudo")
        .arg("sh")
        .args(list_staged_copy_write_arguments(
            new_binary,
            staged_binary_path,
            &ended_staged_copy_paths,
        ))
        .status()
        .map_err(|sudo_spawn_error| sudo_spawn_error.to_string())?;
    if !write_command_status.success() {
        return Err(format!(
            "`sudo` could not replace {}",
            executable_path.display()
        ));
    }
    let replace_result = validate_release_binary(staged_binary_path, release_tag).and_then(|()| {
        let rename_command_status = std::process::Command::new("sudo")
            .arg("sh")
            .args(list_staged_copy_rename_arguments(
                staged_binary_path,
                executable_path,
            ))
            .status()
            .map_err(|sudo_spawn_error| sudo_spawn_error.to_string())?;
        if !rename_command_status.success() {
            return Err(format!(
                "`sudo` could not replace {}",
                executable_path.display()
            ));
        }
        Ok(())
    });
    if let Err(replace_error) = replace_result {
        let is_staged_copy_missing = || {
            fs::symlink_metadata(staged_binary_path)
                .is_err_and(|metadata_error| metadata_error.kind() == io::ErrorKind::NotFound)
        };
        if !is_staged_copy_missing() {
            let _ = std::process::Command::new("sudo")
                .arg("rm")
                .arg("-f")
                .arg(staged_binary_path)
                .status();
        }
        if is_staged_copy_missing() {
            return Err(replace_error);
        }
        return Err(format!(
            "{replace_error}; the copy {} stays: delete it with sudo rm -f {}",
            staged_binary_path.display(),
            staged_binary_path.display()
        ));
    }
    Ok(())
}

/// Sets the Unix executable bit (`0755`) on `executable_path`.
#[cfg(unix)]
fn set_executable_permissions(executable_path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(executable_path, fs::Permissions::from_mode(0o755))
        .map_err(|permission_error| permission_error.to_string())
}

// ---------------------------------------------------------------------------
// State file (koshi-owned): last check time
// ---------------------------------------------------------------------------

/// The update state koshi owns and rewrites, stored as `update.json` in the
/// state directory. Holds only the last-check time, the one update fact this
/// module writes. Every user preference lives in `koshi.kdl`, which this module
/// does not write.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct UpdateState {
    /// Unix seconds of the last completed check, or `None` if never checked.
    #[serde(default)]
    last_check_unix_seconds: Option<u64>,
}

/// The path of the koshi-owned update state file, if a state directory exists.
fn resolve_update_state_path() -> Option<PathBuf> {
    koshi_paths::resolve_state_directory()
        .map(|state_directory| state_directory.join("update.json"))
}

/// The update state file koshi 0.2.0 to 0.4.0 write: `{"last_check": <unix seconds>}`.
#[derive(Deserialize)]
struct PreviousReleaseUpdateState {
    /// Unix seconds of the last completed check.
    last_check: Option<u64>,
}

/// Reads the update state, as [`parse_update_state`] parses it, defaulting on
/// a missing or unreadable file.
fn load_update_state() -> UpdateState {
    let Some(update_state_file_path) = resolve_update_state_path() else {
        return UpdateState::default();
    };
    match fs::read_to_string(&update_state_file_path) {
        Ok(serialized_update_state) => parse_update_state(&serialized_update_state),
        Err(_) => UpdateState::default(),
    }
}

/// Parse `serialized_update_state`, the text of `update.json`.
///
/// A file in the shape koshi 0.2.0 to 0.4.0 write gives its last-check time:
/// `{"last_check": 1700000000}` gives `last_check_unix_seconds`
/// `Some(1700000000)`. [`save_update_state`] writes the current shape. Text
/// that parses as neither shape gives no state.
fn parse_update_state(serialized_update_state: &str) -> UpdateState {
    let update_state: UpdateState =
        serde_json::from_str(serialized_update_state).unwrap_or_default();
    if update_state.last_check_unix_seconds.is_some() {
        return update_state;
    }
    match serde_json::from_str::<PreviousReleaseUpdateState>(serialized_update_state) {
        Ok(previous_release_update_state) => UpdateState {
            last_check_unix_seconds: previous_release_update_state.last_check,
        },
        Err(_) => update_state,
    }
}

/// Writes the update state, creating the state directory if needed.
///
/// `update.json` is written in place, not through a temporary file. A torn
/// file reads back as no state through [`load_update_state`], and the next
/// launch checks again.
fn save_update_state(update_state: &UpdateState) -> io::Result<()> {
    let update_state_file_path = resolve_update_state_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no state directory"))?;
    if let Some(state_directory) = update_state_file_path.parent() {
        fs::create_dir_all(state_directory)?;
    }
    let serialized_update_state =
        serde_json::to_string_pretty(update_state).map_err(io::Error::other)?;
    fs::write(&update_state_file_path, serialized_update_state)
}

/// True when the interval has elapsed since the last check, or none has run.
fn is_update_due(update_state: &UpdateState, check_interval_day_count: u32) -> bool {
    match update_state.last_check_unix_seconds {
        None => true,
        Some(last_check_unix_seconds) => {
            get_current_unix_seconds().saturating_sub(last_check_unix_seconds)
                >= u64::from(check_interval_day_count) * SECONDS_PER_DAY
        }
    }
}

// ---------------------------------------------------------------------------
// Config (user-owned): koshi.kdl `update` section
// ---------------------------------------------------------------------------

/// Reads the `update` section of `koshi.kdl`. A missing or unreadable file
/// gives the defaults, auto-check on. A file that is present and does not
/// parse gives auto-check off.
fn load_update_config() -> UpdateConfig {
    let Some(config_file_path) = koshi_paths::resolve_config_directory()
        .map(|config_directory| config_directory.join("koshi.kdl"))
    else {
        return UpdateConfig::default();
    };
    let Ok(config_file_text) = fs::read_to_string(&config_file_path) else {
        return UpdateConfig::default();
    };
    match parse_app_config(&config_file_path, &config_file_text) {
        // Only the `update` section is read, and the field warnings of a file
        // that parsed are ignored. A file that does not parse turns the
        // automatic check off.
        Ok(parsed_config_file) => {
            merge_client(ClientConfig::default(), vec![parsed_config_file.layer]).update
        }
        Err(config_parse_error) => {
            tracing::warn!(%config_parse_error, "koshi.kdl did not parse; disabling auto update check");
            UpdateConfig {
                should_auto_check_for_updates: false,
                ..UpdateConfig::default()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// A configured HTTP agent whose whole call — connection through body — is
/// bounded by `timeout_duration`. The API check passes [`UPDATE_API_TIMEOUT_DURATION`]; the binary
/// download passes [`UPDATE_DOWNLOAD_TIMEOUT_DURATION`].
///
/// The agent encrypts with [`koshi_ipc::tls::build_crypto_provider`], the provider
/// koshi's own TLS streams use. `ureq` is built with no provider feature and
/// takes the one it is given here.
fn build_http_agent(timeout_duration: Duration) -> Agent {
    Agent::new_with_config(
        Agent::config_builder()
            .timeout_global(Some(timeout_duration))
            .tls_config(
                TlsConfig::builder()
                    .unversioned_rustls_crypto_provider(koshi_ipc::tls::build_crypto_provider())
                    .build(),
            )
            .build(),
    )
}

/// Fetches `request_url` and decodes the JSON body, sending the User-Agent and Accept
/// headers GitHub's API requires.
fn fetch_json<ResponseBody: serde::de::DeserializeOwned>(
    request_url: &str,
) -> Result<ResponseBody, String> {
    let response_body_text = build_http_agent(UPDATE_API_TIMEOUT_DURATION)
        .get(request_url)
        .header("User-Agent", "koshi")
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|http_request_error| http_request_error.to_string())?
        .body_mut()
        .read_to_string()
        .map_err(|response_read_error| response_read_error.to_string())?;
    serde_json::from_str(&response_body_text)
        .map_err(|response_parse_error| response_parse_error.to_string())
}

/// Drops a leading `v` from a tag or version string.
fn strip_version_prefix(version_text: &str) -> &str {
    version_text.strip_prefix('v').unwrap_or(version_text)
}

/// Current Unix time in whole seconds, or `0` if the clock is before the epoch.
fn get_current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed_duration| elapsed_duration.as_secs())
        .unwrap_or(0)
}

/// Builds a [`CliError::Update`] from a failure detail.
fn build_update_error(error_detail: impl Into<String>) -> CliError {
    CliError::Update {
        detail: error_detail.into(),
    }
}

mod install_source;

#[cfg(test)]
mod tests;
