//! Self-update: install a newer koshi release the way this koshi was
//! installed, then restart the running servers into it.
//!
//! `koshi update` (`run_update_command`) first reads where this koshi came
//! from, as `install_source` states. A Homebrew install upgrades through
//! `brew`. A Scoop install and a file no package manager tracks check the
//! project's GitHub releases and, when a newer one exists, download the
//! prebuilt archive for this OS/arch, verify its SHA-256 checksum, unpack the
//! `koshi` binary, and swap it for the program file in place. A build from
//! source downloads nothing. An interactive launch also calls
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

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use koshi_config::app_config::parse_app_config;
use koshi_config::layer::merge_client;
use koshi_config::types::{ClientConfig, UpdateConfig};
use koshi_core::ids::SessionId;
use koshi_host::process_tree;
use koshi_ipc::endpoint::{resolve_update_lock_path, EndpointFile, ServerProgramFile};
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
use koshi_link::ipc_client::{
    self, find_running_session_version, restart_running_session, SessionRestart, UnreadPath,
};
use koshi_link::router_client::{find_running_router_version, restart_running_router};
use koshi_link::server_build::{
    compare_program_files, find_advertised_server_program_file, find_server_program_file,
    ProgramFileMatch,
};

use install_source::{
    find_install_source, InstallSource, ReleaseInstall, HOMEBREW_FORMULA_NAME,
    HOMEBREW_TAPPED_FORMULA_NAME,
};

use crate::session_end::{
    find_server_process_record, format_process_kill_command, PROCESS_STOP_GRACE_DURATION,
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

/// On an interactive launch, when auto-check is enabled and a check is due,
/// look for a newer release and offer to install it. Every failure is
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
    remove_stale_backup();
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
/// <failure>` and restarts nothing.
fn restart_servers_into_program_file(program_path: &Path, restart_scope: RestartScope) {
    match read_installed_version(program_path) {
        Ok(installed_version) => {
            let _ = restart_servers_into_version(&installed_version, restart_scope);
        }
        Err(version_read_error) => {
            eprintln!("koshi: the running servers keep the builds they run: {version_read_error}");
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
/// restarts without it.
///
/// `true` when both give `true`: every server that ran now runs
/// `expected_version`, or ended as [`restart_router_into_version`] states. A
/// runtime directory that cannot be resolved restarts nothing, prints `koshi:
/// the running sessions and the router could not be reached: <failure>`, and
/// gives `false`.
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
    let has_every_session_restarted =
        restart_sessions_into_version(&runtime_directory, expected_version, restart_scope);
    let has_router_restarted =
        restart_router_into_version(&runtime_directory, expected_version, restart_scope);
    drop(update_lock_file);
    has_every_session_restarted && has_router_restarted
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
    let mut update_lock_options = fs::File::options();
    update_lock_options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        update_lock_options.mode(0o600);
    }
    let update_lock_file = match update_lock_options.open(&update_lock_path) {
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

/// Ask the router `runtime_directory` advertises to restart into the koshi
/// program it runs from, confirm the router now reports `expected_version`,
/// and say what happened.
///
/// Prints nothing when no router is running. With
/// [`RestartScope::ServersNotOnVersion`], a router whose program file says it
/// runs `expected_version` is not asked, and prints `the running router
/// already runs koshi <version>; every session keeps running`. Success is
/// printed only after the router's Hello reports `expected_version`. A router
/// that refuses this build's protocol version goes to
/// [`stop_incompatible_router`]. Any other refusal, a router still on another
/// build, or no answer within [`RESTART_CONFIRM_WAIT_DURATION`] prints a note
/// on standard error.
///
/// `true` when no router runs, when the router runs `expected_version`, and
/// when [`stop_incompatible_router`] gives `true`.
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
        Err(CliError::ProtocolVersionRefused {
            detail: refusal_detail,
        }) => stop_incompatible_router(runtime_directory, expected_version, &refusal_detail),
        Err(router_restart_error) => {
            eprintln!(
                "koshi: the running router could not be restarted: {router_restart_error}; it keeps serving the old \
                 build until it exits"
            );
            false
        }
    }
}

/// End the router `runtime_directory` advertises, which refused this build's
/// protocol version with `refusal_detail`, once
/// [`find_server_process_record`] confirms its process. Only the router's own
/// process ends: every session keeps running, and the next koshi command that
/// needs a router starts a new one. Prints what happened.
///
/// A router whose program file, as [`find_server_program_file`] reads it,
/// names a version newer than this build keeps running:
///
/// - `expected_version` prints `the running router already runs koshi
///   <version>; every session keeps running`, and gives `true`.
/// - Any other newer version prints `koshi: the running router runs koshi
///   <version> from <path>, which is newer than this koshi <this version>; it
///   keeps running; every session keeps running`, and gives `false`.
///
/// `true` once the router's process has ended. `false`, with a note on
/// standard error, for an endpoint file that cannot be read, for a process the
/// proof does not confirm, which is left running, and for a process that still
/// runs [`PROCESS_STOP_GRACE_DURATION`] after koshi ended it.
fn stop_incompatible_router(
    runtime_directory: &Path,
    expected_version: &str,
    refusal_detail: &str,
) -> bool {
    let router_endpoint_path = resolve_router_endpoint_path(runtime_directory);
    let router_endpoint_file = match EndpointFile::load_from_path(&router_endpoint_path) {
        Ok(router_endpoint_file) => router_endpoint_file,
        Err(endpoint_file_error) => {
            eprintln!(
                "koshi: the running router runs a koshi version this one cannot talk to \
                 ({refusal_detail}), and its endpoint file could not be read: {endpoint_file_error}"
            );
            return false;
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
            return true;
        }
        Some(router_program_file) => {
            eprintln!(
                "koshi: the running router runs koshi {} from {}, which is newer than this koshi \
                 {APP_VERSION}; it keeps running; every session keeps running",
                sanitize_reported_text(&router_program_file.build_version),
                sanitize_reported_text(&router_program_file.program_path)
            );
            return false;
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
        return false;
    };
    let router_records = std::slice::from_ref(&router_record);
    process_tree::stop_processes(router_records, PROCESS_STOP_GRACE_DURATION);
    if !process_tree::wait_for_processes_to_end(router_records, PROCESS_STOP_GRACE_DURATION) {
        eprintln!(
            "koshi: the running router (process {router_process_id}) runs a koshi version this \
             one cannot talk to ({refusal_detail}), and still runs after koshi ended it"
        );
        return false;
    }
    println!(
        "koshi ended the running router (process {router_process_id}): it ran a koshi version \
         this one cannot talk to; the next koshi command starts a new router; every session \
         keeps running"
    );
    true
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
/// [`find_advertised_server_program_file`] reads it.
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
}

/// Ask every session `runtime_directory` advertises that `restart_scope`
/// names to restart into the koshi program it runs from, and print one line
/// per session.
///
/// Prints nothing for a session that is no longer listening. Success is printed
/// only after that session's Hello reports `expected_version`. A session
/// already on `expected_version` prints `<session id> already runs koshi
/// <version>; its panes keep running`. Every other result prints a note on
/// standard error. A session that refused this build's protocol version, and
/// did not restart by itself within the wait [`restart_running_session`]
/// makes, is not ended: its note names the command that ends it,
/// `koshi kill-session <session id>`. A runtime directory that cannot be read
/// prints `koshi: the running sessions could not be listed: <path> could not
/// be read: <failure>; each keeps serving the old build until it is ended and
/// started again`, and restarts nothing.
///
/// `true` when every session now runs `expected_version`, and when none was
/// running.
fn restart_sessions_into_version(
    runtime_directory: &Path,
    expected_version: &str,
    restart_scope: RestartScope,
) -> bool {
    let session_outcomes = match restart_advertised_sessions(
        runtime_directory,
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
            return false;
        }
    };
    let mut has_every_session_restarted = true;
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
                let running_version = sanitize_reported_text(&session_program_file.build_version);
                eprintln!(
                    "koshi: {session_id} runs koshi {running_version} from {}, a program file \
                     this koshi does not replace; it keeps running koshi {running_version}",
                    sanitize_reported_text(&session_program_file.program_path)
                );
                has_every_session_restarted = false;
            }
        }
    }
    has_every_session_restarted
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
/// than this build, and [`SessionOutcome::Incompatible`] otherwise. One
/// session's failure never ends the walk: every advertised session is asked,
/// whatever the one before it answered.
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
/// and swaps it for the running executable, as [`swap_executable`] states. The temp files are
/// securely created and auto-removed when their [`TempPath`] drops at the end of this function,
/// whichever way it ends.
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
    swap_executable(release_binary.as_ref(), &executable_path)
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

/// Removes the `<exe>.old` a prior Windows self-update left beside the file
/// the running executable's path names once every symbolic link and junction
/// in it is followed. The swap leaves that file while the old image still
/// runs from it, and the next launch removes it. A no-op on other platforms,
/// where the swap leaves no file behind.
fn remove_stale_backup() {
    #[cfg(windows)]
    if let Ok(executable_path) = std::env::current_exe().and_then(fs::canonicalize) {
        let _ = fs::remove_file(executable_path.with_extension("old"));
    }
}

/// Replace the program file on Unix: the file `executable_path` names once
/// every symbolic link in it is followed. A symbolic link at
/// `executable_path` keeps naming that file.
///
/// Copies `new_binary` beside that file as `<name>.koshi-update-<pid>`, sets
/// mode `0755` on the copy, and renames the copy over the file in one step. A
/// process that runs the old file keeps running it. `new_binary` is removed
/// once the rename succeeds. A copy refused with a permission error installs
/// through `sudo` instead, as [`replace_with_sudo`] states. Every other
/// failure removes the copy and leaves the program file as it was.
///
/// # Errors
/// The failure of the path lookup, the copy, the mode change, or the rename,
/// as text.
#[cfg(unix)]
fn swap_executable(new_binary: &Path, executable_path: &Path) -> Result<(), String> {
    let executable_path = &fs::canonicalize(executable_path)
        .map_err(|executable_path_error| executable_path_error.to_string())?;
    let staged_binary_path = executable_path.with_file_name(format!(
        "{}.koshi-update-{}",
        executable_path
            .file_name()
            .and_then(|file_name| file_name.to_str())
            .unwrap_or("koshi"),
        std::process::id()
    ));
    if let Err(copy_error) = fs::copy(new_binary, &staged_binary_path) {
        // A copy refused for permission, such as into a root-owned
        // `/usr/local/bin`, installs through `sudo`.
        if copy_error.kind() == io::ErrorKind::PermissionDenied {
            return replace_with_sudo(new_binary, executable_path);
        }
        return Err(copy_error.to_string());
    }
    if let Err(permission_error) = set_executable_permissions(&staged_binary_path) {
        let _ = fs::remove_file(&staged_binary_path);
        return Err(permission_error);
    }
    match fs::rename(&staged_binary_path, executable_path) {
        Ok(()) => {
            let _ = fs::remove_file(new_binary);
            Ok(())
        }
        Err(rename_error) => {
            let _ = fs::remove_file(&staged_binary_path);
            Err(rename_error.to_string())
        }
    }
}

/// Replace the program file on Windows: the file `executable_path` names once
/// every symbolic link and junction in it is followed. A symbolic link at
/// `executable_path` keeps naming that file.
///
/// Copies `new_binary` beside that file as `koshi-update-<pid>.exe`, renames
/// the running file to `<name>.old`, and renames the copy into its place. When
/// that last rename fails, the `.old` file is renamed back, and a restore that
/// fails too returns both errors and the `.old` path. A `.old` file that
/// cannot be removed while it runs is removed at the next launch by
/// [`remove_stale_backup`].
///
/// # Errors
/// The failure of the path lookup, the copy, or a rename, as text.
#[cfg(windows)]
fn swap_executable(new_binary: &Path, executable_path: &Path) -> Result<(), String> {
    let executable_path = &fs::canonicalize(executable_path)
        .map_err(|executable_path_error| executable_path_error.to_string())?;
    // The copy sits beside the program file, on the same volume.
    let staged_binary_path =
        executable_path.with_file_name(format!("koshi-update-{}.exe", std::process::id()));
    fs::copy(new_binary, &staged_binary_path)
        .map_err(|staged_copy_error| staged_copy_error.to_string())?;
    let backup_executable_path = executable_path.with_extension("old");
    if let Err(backup_rename_error) = fs::rename(executable_path, &backup_executable_path) {
        let _ = fs::remove_file(&staged_binary_path);
        return Err(backup_rename_error.to_string());
    }
    if let Err(staged_rename_error) = fs::rename(&staged_binary_path, executable_path) {
        let rollback_error = fs::rename(&backup_executable_path, executable_path).err();
        let _ = fs::remove_file(&staged_binary_path);
        if let Some(rollback_error) = rollback_error {
            return Err(format!(
                "could not install replacement at {}: {staged_rename_error}; could not restore the original executable: {rollback_error}; manual recovery: restore {} as {}",
                executable_path.display(),
                backup_executable_path.display(),
                executable_path.display()
            ));
        }
        return Err(staged_rename_error.to_string());
    }
    // A `.old` file that still runs is not removed here; `remove_stale_backup`
    // removes it at the next launch.
    let _ = fs::remove_file(&backup_executable_path);
    Ok(())
}

/// Installs `new_binary` over `executable_path` with `sudo`, for a binary in a root-owned
/// directory. `install -m 755` writes the file and sets its mode in one step.
#[cfg(unix)]
fn replace_with_sudo(new_binary: &Path, executable_path: &Path) -> Result<(), String> {
    eprintln!(
        "koshi: updating {} needs elevated permissions",
        executable_path.display()
    );
    let install_command_status = std::process::Command::new("sudo")
        .arg("install")
        .arg("-m")
        .arg("755")
        .arg(new_binary)
        .arg(executable_path)
        .status()
        .map_err(|sudo_spawn_error| sudo_spawn_error.to_string())?;
    if !install_command_status.success() {
        return Err("`sudo install` failed".to_string());
    }
    let _ = fs::remove_file(new_binary);
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

/// Reads the update state, defaulting on a missing or unreadable file.
fn load_update_state() -> UpdateState {
    let Some(update_state_file_path) = resolve_update_state_path() else {
        return UpdateState::default();
    };
    match fs::read_to_string(&update_state_file_path) {
        Ok(serialized_update_state) => {
            serde_json::from_str(&serialized_update_state).unwrap_or_default()
        }
        Err(_) => UpdateState::default(),
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
