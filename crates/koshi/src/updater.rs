//! Self-update: check GitHub for a newer koshi release and install it.
//!
//! `koshi update` (`run_update_command`) checks the project's GitHub releases
//! and, when a newer one exists, downloads the prebuilt archive for this
//! OS/arch, unpacks the `koshi` binary, and swaps it for the running executable
//! in place. An interactive launch also calls `maybe_prompt_startup_update`,
//! which does the same check on a timer and offers to install.
//!
//! Two small files back this. The user's hand-authored `koshi.kdl` holds every
//! preference koshi only reads — `update.auto-check`,
//! `update.check-interval-days`, and `update.allow-prerelease`. A koshi-owned
//! `update.json` in the state directory holds the one thing koshi writes — the
//! last-check time — so koshi never rewrites the user's config file.
//!
//! This is a CLI-side, one-shot flow. No session runs it, so it reads the
//! clock and the network directly rather than through the runtime's injected
//! services. After an install it asks every running session, and then the
//! running router, to restart into the binary just installed.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use koshi_config::app_config::parse_app_config;
use koshi_config::layer::merge_client;
use koshi_config::types::{ClientConfig, UpdateConfig};
use koshi_core::ids::SessionId;
use semver::Version;
use serde::{Deserialize, Serialize};
use tempfile::{Builder, TempPath};
use ureq::tls::TlsConfig;
use ureq::Agent;

use koshi_link::error::CliError;
use koshi_link::ipc_client::{
    self, get_running_session_version, restart_running_session, SessionRestart,
};
use koshi_link::router_client::{get_running_router_version, restart_running_router};

/// This build's version, from the crate version bumped before each release.
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The GitHub `owner/repo` the release archives live under.
const RELEASE_REPOSITORY: &str = "gohyuhan/koshi";

/// How long the GitHub API check may run before it is abandoned. Bounds the
/// whole call, connection through JSON body.
const UPDATE_API_TIMEOUT_DURATION: Duration = Duration::from_secs(15);

/// How long a binary download may run before it is abandoned. Bounds the whole
/// call, connection through the streamed archive body.
const UPDATE_DOWNLOAD_TIMEOUT_DURATION: Duration = Duration::from_secs(600);

/// Seconds in a day, for turning the check interval into a duration.
const SECONDS_PER_DAY: u64 = 86_400;

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Runs `koshi update`: check for a newer release and, if one exists, download
/// and install it in place. Prints an "already latest" note when up to date.
///
/// # Errors
/// Returns [`CliError::Update`] when the network check fails, no release
/// binary exists for this platform, or the download/install step fails.
pub fn run_update_command() -> Result<(), CliError> {
    let should_allow_prerelease_updates = load_update_config().should_allow_prerelease_updates;
    let available_release_tag =
        check_for_update(should_allow_prerelease_updates).map_err(build_update_error)?;
    // A completed check counts toward the interval whether or not it found a
    // newer release, so the next startup check waits the full interval.
    persist_last_check();
    let Some(release_tag) = available_release_tag else {
        println!("koshi {APP_VERSION} is already the latest version");
        return Ok(());
    };
    install_release(&release_tag).map_err(build_update_error)?;
    println!("updated to koshi {}", strip_version_prefix(&release_tag));
    restart_sessions_after_install(strip_version_prefix(&release_tag));
    restart_router_after_install(strip_version_prefix(&release_tag));
    Ok(())
}

/// On an interactive launch, when auto-check is enabled and a check is due,
/// look for a newer release and offer to install it. Every failure is
/// swallowed and the launch continues. Runs before the terminal enters raw
/// mode, and reads the answer from plain standard input.
pub fn maybe_prompt_startup_update() {
    remove_stale_backup();
    let update_config = load_update_config();
    if !update_config.should_auto_check_for_updates {
        return;
    }
    let mut update_state = load_update_state();
    if !is_update_due(&update_state, update_config.check_interval_days) {
        return;
    }
    // The attempt is recorded before the network call, whatever that call
    // answers, so a failing or slow check waits a full interval before the
    // next launch tries again.
    update_state.last_check_unix_seconds = Some(get_current_unix_seconds());
    let _ = save_update_state(&update_state);
    let release_tag = match check_for_update(update_config.should_allow_prerelease_updates) {
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
    match install_release(&release_tag) {
        Ok(()) => {
            restart_sessions_after_install(strip_version_prefix(&release_tag));
            restart_router_after_install(strip_version_prefix(&release_tag));
            println!(
                "updated to koshi {} — relaunch to use it",
                strip_version_prefix(&release_tag)
            );
            std::process::exit(0);
        }
        Err(update_install_error) => eprintln!("koshi: update failed: {update_install_error}"),
    }
}

/// How long a restarted router or session has to come back answering Hello
/// with the installed version.
const RESTART_CONFIRM_WAIT_DURATION: Duration = Duration::from_secs(10);

/// The pause between two Hello probes while waiting for the confirmation.
const RESTART_CONFIRM_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(200);

/// The hard time bound on one Hello probe. A probe that reaches a half-closed
/// named pipe on Windows reads as no answer once the bound runs out.
const RESTART_PROBE_TIMEOUT_DURATION: Duration = Duration::from_secs(2);

/// Ask the running router to restart into the binary just installed, confirm
/// the router now reports the `installed` version, and say what happened.
///
/// Prints nothing when no router is running. Success is printed only after
/// the router's Hello reports `installed`. A refusal, a router still on the
/// previous build, or no answer within [`RESTART_CONFIRM_WAIT_DURATION`] prints a note
/// on standard error; the install itself stands.
fn restart_router_after_install(installed_version: &str) {
    let runtime_directory = match ipc_client::resolve_runtime_directory() {
        Ok(runtime_directory) => runtime_directory,
        Err(runtime_directory_error) => {
            eprintln!("koshi: the running router could not be reached: {runtime_directory_error}");
            return;
        }
    };
    match restart_running_router(&runtime_directory) {
        Ok(false) => {}
        Ok(true) => match wait_for_version(installed_version, RESTART_CONFIRM_WAIT_DURATION, || {
            probe_router_version(&runtime_directory)
        }) {
            VersionProbeOutcome::Installed => println!(
                "the running router restarted into the new binary; every session keeps running"
            ),
            VersionProbeOutcome::OtherVersion(reported_version) => eprintln!(
                "koshi: the running router still reports {reported_version} after the restart; it keeps \
                 serving that build; every session keeps running"
            ),
            VersionProbeOutcome::Silent => eprintln!(
                "koshi: the router restart was not confirmed: no router answered within \
                 {} seconds; every session keeps running",
                RESTART_CONFIRM_WAIT_DURATION.as_secs()
            ),
        },
        Err(router_restart_error) => eprintln!(
            "koshi: the running router could not be restarted: {router_restart_error}; it keeps serving the old \
             build until it exits"
        ),
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
/// A probe that gives no answer counts as none and the poll continues: the peer
/// is mid-restart.
fn wait_for_version(
    expected_version: &str,
    wait_duration: Duration,
    version_probe: impl Fn() -> Option<String>,
) -> VersionProbeOutcome {
    let deadline = Instant::now() + wait_duration;
    let mut last_reported_version = None;
    loop {
        match version_probe() {
            Some(version) if version == expected_version => {
                return VersionProbeOutcome::Installed;
            }
            Some(version) => last_reported_version = Some(version),
            None => {}
        }
        if Instant::now() >= deadline {
            return last_reported_version.map_or(
                VersionProbeOutcome::Silent,
                VersionProbeOutcome::OtherVersion,
            );
        }
        std::thread::sleep(RESTART_CONFIRM_POLL_INTERVAL_DURATION);
    }
}

/// One version probe, bounded by [`RESTART_PROBE_TIMEOUT_DURATION`].
///
/// `version_probe` runs on its own thread. A probe that runs out the bound reads as no
/// answer; its thread is left behind and ends with this process.
fn probe_version(
    version_probe: impl FnOnce() -> Option<String> + Send + 'static,
) -> Option<String> {
    let (response_sender, response_receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = response_sender.send(version_probe());
    });
    response_receiver
        .recv_timeout(RESTART_PROBE_TIMEOUT_DURATION)
        .unwrap_or(None)
}

/// One probe of the running router's version.
fn probe_router_version(runtime_directory: &Path) -> Option<String> {
    let runtime_directory = runtime_directory.to_path_buf();
    probe_version(move || {
        get_running_router_version(&runtime_directory)
            .ok()
            .flatten()
    })
}

/// One probe of the running session `session_id`'s version.
fn probe_session_version(runtime_directory: &Path, session_id: SessionId) -> Option<String> {
    let runtime_directory = runtime_directory.to_path_buf();
    probe_version(move || {
        get_running_session_version(&runtime_directory, session_id)
            .ok()
            .flatten()
    })
}

/// The result of asking one running session to restart.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionOutcome {
    /// The session restarted and now reports the installed version.
    Confirmed,
    /// The session restarted and still reports the version named here.
    StillOnVersion(String),
    /// The session restarted and answered nothing within the wait.
    Unconfirmed,
    /// The session runs a koshi build that has no restart request.
    TooOld,
    /// The session refused the restart or could not be reached. Carries the
    /// sentence naming what went wrong.
    Failed(String),
}

/// Ask every session `runtime_directory` advertises to restart into the binary just
/// installed, and print one line per session.
///
/// Prints nothing for a session that is no longer listening. Success is printed
/// only after that session's Hello reports `installed`. Every other result
/// prints a note on standard error, and the install itself stands.
fn restart_sessions_after_install(installed_version: &str) {
    let runtime_directory = match ipc_client::resolve_runtime_directory() {
        Ok(runtime_directory) => runtime_directory,
        Err(runtime_directory_error) => {
            eprintln!(
                "koshi: the running sessions could not be reached: {runtime_directory_error}"
            );
            return;
        }
    };
    for (session_id, session_outcome) in restart_advertised_sessions(
        &runtime_directory,
        installed_version,
        RESTART_CONFIRM_WAIT_DURATION,
    ) {
        match session_outcome {
            SessionOutcome::Confirmed => {
                println!("{session_id} restarted into the new binary; its panes keep running")
            }
            SessionOutcome::StillOnVersion(reported_version) => eprintln!(
                "koshi: {session_id} still reports {reported_version} after the restart; it keeps serving \
                 that build; its panes keep running"
            ),
            SessionOutcome::Unconfirmed => eprintln!(
                "koshi: the restart of {session_id} was not confirmed: it answered nothing \
                 within {} seconds; its panes keep running",
                RESTART_CONFIRM_WAIT_DURATION.as_secs()
            ),
            SessionOutcome::TooOld => eprintln!(
                "koshi: {session_id} runs a koshi that cannot replace its own binary; end that \
                 session and start it again to run the new build"
            ),
            SessionOutcome::Failed(error_detail) => eprintln!(
                "koshi: {session_id} could not be restarted: {error_detail}; it keeps serving the old \
                 build until you end that session and start it again"
            ),
        }
    }
}

/// Ask every session `runtime_directory` advertises to restart, waiting up to
/// `wait_duration`
/// on each for a Hello reporting `installed`, and hand back each result.
///
/// A session no longer listening is left out. One session's failure never ends
/// the walk: every advertised session is asked, whatever the one before it
/// answered.
fn restart_advertised_sessions(
    runtime_directory: &Path,
    installed_version: &str,
    wait_duration: Duration,
) -> Vec<(SessionId, SessionOutcome)> {
    let mut session_outcomes = Vec::new();
    for session_id in ipc_client::list_advertised_sessions(runtime_directory) {
        let session_outcome = match restart_running_session(runtime_directory, session_id) {
            Ok(SessionRestart::NotRunning) => continue,
            Ok(SessionRestart::TooOld) => SessionOutcome::TooOld,
            Ok(SessionRestart::Restarting) => {
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
            Err(session_restart_error) => SessionOutcome::Failed(session_restart_error.to_string()),
        };
        session_outcomes.push((session_id, session_outcome));
    }
    session_outcomes
}

// ---------------------------------------------------------------------------
// Version check
// ---------------------------------------------------------------------------

/// One GitHub release, cut down to the fields the update check reads.
#[derive(Debug, Deserialize)]
struct Release {
    /// The git tag the release was cut from, e.g. `v0.2.0`.
    #[serde(rename = "tag_name")]
    release_tag: String,
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
        Ok(release.release_tag)
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
            Version::parse(strip_version_prefix(&release.release_tag))
                .ok()
                .map(|parsed_version| (parsed_version, release.release_tag))
        })
        .max_by(|left_release, right_release| left_release.0.cmp(&right_release.0))
        .map(|(_, release_tag)| release_tag)
        .ok_or_else(|| "no releases found".to_string())
}

/// True when `release_tag` names a version strictly newer than this build. A tag or
/// build version that does not parse as semver reads as not newer.
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

/// Downloads the release archive `release_tag` names, unpacks the binary, and swaps it
/// for the running executable. Both temp files are securely created and
/// auto-removed when their [`TempPath`] drops at the end of this function,
/// whichever way it ends.
fn install_release(release_tag: &str) -> Result<(), String> {
    let archive_url = compute_binary_url(release_tag).ok_or_else(|| {
        format!(
            "no koshi release binary for {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    println!("downloading koshi {} …", strip_version_prefix(release_tag));
    let release_archive = download_release_archive(&archive_url)?;
    let release_binary = extract_release_binary(release_archive.as_ref(), &archive_url)?;
    install_binary(release_binary.as_ref())
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
/// or symbolic link.
fn download_release_archive(archive_url: &str) -> Result<TempPath, String> {
    let mut http_response = build_http_agent(UPDATE_DOWNLOAD_TIMEOUT_DURATION)
        .get(archive_url)
        .header("User-Agent", "koshi")
        .call()
        .map_err(|download_error| download_error.to_string())?;
    let mut archive_file = Builder::new()
        .prefix("koshi-update-")
        .tempfile()
        .map_err(|temporary_archive_error| temporary_archive_error.to_string())?;
    let mut archive_reader = http_response.body_mut().as_reader();
    io::copy(&mut archive_reader, archive_file.as_file_mut())
        .map_err(|archive_copy_error| archive_copy_error.to_string())?;
    Ok(archive_file.into_temp_path())
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
        // Only a regular file counts: a directory or symlink named `koshi`
        // would otherwise be "saved" as an empty or wrong binary.
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
        .map_err(|temporary_binary_error| temporary_binary_error.to_string())?;
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

/// Removes a `<exe>.old` left by a prior Windows self-update. The old image is
/// locked against deletion while it is the running process, so the rename-aside
/// swap cannot delete it then; the next launch runs the new binary and clears
/// it. A no-op on other platforms, where the swap deletes nothing behind.
fn remove_stale_backup() {
    #[cfg(windows)]
    if let Ok(executable_path) = std::env::current_exe() {
        let _ = fs::remove_file(executable_path.with_extension("old"));
    }
}

/// Swaps the running executable for `new_binary`.
fn install_binary(new_binary: &Path) -> Result<(), String> {
    let executable_path = std::env::current_exe()
        .map_err(|executable_path_error| executable_path_error.to_string())?;
    swap_executable(new_binary, &executable_path)
}

/// Replaces the executable on Unix atomically. The new binary is staged as a
/// sibling of `exe` — same directory, so the same filesystem — then renamed
/// over `exe`. Renaming a running binary is safe on Unix: the live process
/// keeps the old inode. The swap is a single rename: an interrupted copy never
/// touches the running binary, and the replacement either fully happens or not
/// at all. A permission error on the staging directory escalates
/// to sudo.
#[cfg(unix)]
fn swap_executable(new_binary: &Path, executable_path: &Path) -> Result<(), String> {
    let staged_binary_path = executable_path.with_file_name(format!(
        "{}.koshi-update-{}",
        executable_path
            .file_name()
            .and_then(|file_name| file_name.to_str())
            .unwrap_or("koshi"),
        std::process::id()
    ));
    if let Err(copy_error) = fs::copy(new_binary, &staged_binary_path) {
        // A copy into the exe's own directory fails when that directory is not
        // writable (e.g. a root-owned /usr/local/bin) — escalate to sudo.
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

/// Replaces the executable on Windows: a running binary cannot be overwritten,
/// so stage the new one beside the exe (a copy crosses drives, so both following
/// renames stay on the exe's own volume), rename the running exe aside, move the
/// staged one into place, and restore the old one if that final move fails.
#[cfg(windows)]
fn swap_executable(new_binary: &Path, executable_path: &Path) -> Result<(), String> {
    // The staging name sits beside the exe, on the exe's own volume, so both
    // renames below stay within one volume.
    let staged_binary_path =
        executable_path.with_file_name(format!("koshi-update-{}.exe", std::process::id()));
    fs::copy(new_binary, &staged_binary_path).map_err(|error| error.to_string())?;
    let backup_executable_path = executable_path.with_extension("old");
    if let Err(error) = fs::rename(executable_path, &backup_executable_path) {
        let _ = fs::remove_file(&staged_binary_path);
        return Err(error.to_string());
    }
    if let Err(error) = fs::rename(&staged_binary_path, executable_path) {
        let _ = fs::rename(&backup_executable_path, executable_path);
        let _ = fs::remove_file(&staged_binary_path);
        return Err(error.to_string());
    }
    // The old image is locked against deletion while it runs; `remove_stale_backup`
    // clears it on the next launch.
    let _ = fs::remove_file(&backup_executable_path);
    Ok(())
}

/// Installs `new_binary` over `exe` with `sudo`, for a binary in a root-owned
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

/// Sets the Unix executable bit (`0755`) on `path`.
#[cfg(unix)]
fn set_executable_permissions(executable_path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(executable_path, fs::Permissions::from_mode(0o755))
        .map_err(|permission_error| permission_error.to_string())
}

// ---------------------------------------------------------------------------
// State file (koshi-owned): last check time + pre-release opt-in
// ---------------------------------------------------------------------------

/// The update state koshi owns and rewrites, stored as `update.json` in the
/// state directory. Holds only the last-check time — the one update fact koshi
/// writes; every user preference lives in `koshi.kdl`, which koshi never
/// rewrites.
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
/// ponytail: plain write, not atomic — `update.json` is disposable, a torn
/// write just forces a re-check next launch. Atomic write is for session data.
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

/// Records the current time as the last check, ignoring a write failure.
fn persist_last_check() {
    let mut update_state = load_update_state();
    update_state.last_check_unix_seconds = Some(get_current_unix_seconds());
    let _ = save_update_state(&update_state);
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
        // Only the strict `update` section matters here; a bad field there is
        // still an `Err` (fail closed), so field-partial warnings are ignored.
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

/// Fetches `url` and decodes the JSON body, sending the User-Agent and Accept
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

#[cfg(test)]
mod tests;
