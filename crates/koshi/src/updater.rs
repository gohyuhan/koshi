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
    self, restart_running_session, running_session_version, SessionRestart,
};
use koshi_link::router_client::{restart_running_router, running_router_version};

/// This build's version, from the crate version bumped before each release.
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The GitHub `owner/repo` the release archives live under.
const REPO: &str = "gohyuhan/koshi";

/// How long the GitHub API check may run before it is abandoned. Bounds the
/// whole call, connection through JSON body.
const API_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a binary download may run before it is abandoned. Bounds the whole
/// call, connection through the streamed archive body.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

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
    let allow_prerelease = load_update_config().allow_prerelease;
    let newer = check_for_update(allow_prerelease).map_err(update_err)?;
    // A completed check counts toward the interval whether or not it found a
    // newer release, so the next startup check waits the full interval.
    persist_last_check();
    let Some(tag) = newer else {
        println!("koshi {APP_VERSION} is already the latest version");
        return Ok(());
    };
    install_release(&tag).map_err(update_err)?;
    println!("updated to koshi {}", strip_v(&tag));
    restart_sessions_after_install(strip_v(&tag));
    restart_router_after_install(strip_v(&tag));
    Ok(())
}

/// On an interactive launch, when auto-check is enabled and a check is due,
/// look for a newer release and offer to install it. Every failure is
/// swallowed and the launch continues. Runs before the terminal enters raw
/// mode, and reads the answer from plain standard input.
pub fn maybe_prompt_startup_update() {
    remove_stale_backup();
    let config = load_update_config();
    if !config.auto_check {
        return;
    }
    let mut state = load_state();
    if !is_due(&state, config.check_interval_days) {
        return;
    }
    // The attempt is recorded before the network call, whatever that call
    // answers, so a failing or slow check waits a full interval before the
    // next launch tries again.
    state.last_check = Some(now_secs());
    let _ = save_state(&state);
    let tag = match check_for_update(config.allow_prerelease) {
        Ok(Some(tag)) => tag,
        Ok(None) | Err(_) => return,
    };

    let prompt = format!(
        "koshi {} is available (you have {APP_VERSION}). Update now? [y/N] ",
        strip_v(&tag)
    );
    if !crate::prompt::yes(&prompt) {
        return;
    }
    match install_release(&tag) {
        Ok(()) => {
            restart_sessions_after_install(strip_v(&tag));
            restart_router_after_install(strip_v(&tag));
            println!("updated to koshi {} — relaunch to use it", strip_v(&tag));
            std::process::exit(0);
        }
        Err(err) => eprintln!("koshi: update failed: {err}"),
    }
}

/// How long a restarted router or session has to come back answering Hello
/// with the installed version.
const RESTART_CONFIRM_WAIT: Duration = Duration::from_secs(10);

/// The pause between two Hello probes while waiting for the confirmation.
const RESTART_CONFIRM_POLL: Duration = Duration::from_millis(200);

/// The hard time bound on one Hello probe. A probe that reaches a half-closed
/// named pipe on Windows reads as no answer once the bound runs out.
const RESTART_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Ask the running router to restart into the binary just installed, confirm
/// the router now reports the `installed` version, and say what happened.
///
/// Prints nothing when no router is running. Success is printed only after
/// the router's Hello reports `installed`. A refusal, a router still on the
/// previous build, or no answer within [`RESTART_CONFIRM_WAIT`] prints a note
/// on standard error; the install itself stands.
fn restart_router_after_install(installed: &str) {
    let dir = match ipc_client::runtime_dir() {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("koshi: the running router could not be reached: {err}");
            return;
        }
    };
    match restart_running_router(&dir) {
        Ok(false) => {}
        Ok(true) => match wait_for_version(installed, RESTART_CONFIRM_WAIT, || {
            probe_router_version(&dir)
        }) {
            VersionAnswer::Installed => println!(
                "the running router restarted into the new binary; every session keeps running"
            ),
            VersionAnswer::Other(version) => eprintln!(
                "koshi: the running router still reports {version} after the restart; it keeps \
                 serving that build; every session keeps running"
            ),
            VersionAnswer::Silent => eprintln!(
                "koshi: the router restart was not confirmed: no router answered within \
                 {} seconds; every session keeps running",
                RESTART_CONFIRM_WAIT.as_secs()
            ),
        },
        Err(err) => eprintln!(
            "koshi: the running router could not be restarted: {err}; it keeps serving the old \
             build until it exits"
        ),
    }
}

/// How the wait for a restarted router or session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum VersionAnswer {
    /// It answered with the version the wait was for.
    Installed,
    /// The last version it answered with, which was another one.
    Other(String),
    /// It answered nothing before the wait ran out.
    Silent,
}

/// Poll `probe` until it reports `want`, for up to `wait`.
///
/// A probe that gives no answer counts as none and the poll continues: the peer
/// is mid-restart.
fn wait_for_version(
    want: &str,
    wait: Duration,
    probe: impl Fn() -> Option<String>,
) -> VersionAnswer {
    let deadline = Instant::now() + wait;
    let mut last_answer = None;
    loop {
        match probe() {
            Some(version) if version == want => return VersionAnswer::Installed,
            Some(version) => last_answer = Some(version),
            None => {}
        }
        if Instant::now() >= deadline {
            return last_answer.map_or(VersionAnswer::Silent, VersionAnswer::Other);
        }
        std::thread::sleep(RESTART_CONFIRM_POLL);
    }
}

/// One version probe, bounded by [`RESTART_PROBE_TIMEOUT`].
///
/// `ask` runs on its own thread. A probe that runs out the bound reads as no
/// answer; its thread is left behind and ends with this process.
fn probe_version(ask: impl FnOnce() -> Option<String> + Send + 'static) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(ask());
    });
    rx.recv_timeout(RESTART_PROBE_TIMEOUT).unwrap_or(None)
}

/// One probe of the running router's version.
fn probe_router_version(runtime_dir: &Path) -> Option<String> {
    let dir = runtime_dir.to_path_buf();
    probe_version(move || running_router_version(&dir).ok().flatten())
}

/// One probe of the running session `session_id`'s version.
fn probe_session_version(runtime_dir: &Path, session_id: SessionId) -> Option<String> {
    let dir = runtime_dir.to_path_buf();
    probe_version(move || running_session_version(&dir, session_id).ok().flatten())
}

/// The result of asking one running session to restart.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionOutcome {
    /// The session restarted and now reports the installed version.
    Confirmed,
    /// The session restarted and still reports the version named here.
    StillOn(String),
    /// The session restarted and answered nothing within the wait.
    Unconfirmed,
    /// The session runs a koshi build that has no restart request.
    TooOld,
    /// The session refused the restart or could not be reached. Carries the
    /// sentence naming what went wrong.
    Failed(String),
}

/// Ask every session `runtime_dir` advertises to restart into the binary just
/// installed, and print one line per session.
///
/// Prints nothing for a session that is no longer listening. Success is printed
/// only after that session's Hello reports `installed`. Every other result
/// prints a note on standard error, and the install itself stands.
fn restart_sessions_after_install(installed: &str) {
    let dir = match ipc_client::runtime_dir() {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("koshi: the running sessions could not be reached: {err}");
            return;
        }
    };
    for (session_id, outcome) in restart_advertised_sessions(&dir, installed, RESTART_CONFIRM_WAIT)
    {
        match outcome {
            SessionOutcome::Confirmed => {
                println!("{session_id} restarted into the new binary; its panes keep running")
            }
            SessionOutcome::StillOn(version) => eprintln!(
                "koshi: {session_id} still reports {version} after the restart; it keeps serving \
                 that build; its panes keep running"
            ),
            SessionOutcome::Unconfirmed => eprintln!(
                "koshi: the restart of {session_id} was not confirmed: it answered nothing \
                 within {} seconds; its panes keep running",
                RESTART_CONFIRM_WAIT.as_secs()
            ),
            SessionOutcome::TooOld => eprintln!(
                "koshi: {session_id} runs a koshi that cannot replace its own binary; end that \
                 session and start it again to run the new build"
            ),
            SessionOutcome::Failed(detail) => eprintln!(
                "koshi: {session_id} could not be restarted: {detail}; it keeps serving the old \
                 build until you end that session and start it again"
            ),
        }
    }
}

/// Ask every session `runtime_dir` advertises to restart, waiting up to `wait`
/// on each for a Hello reporting `installed`, and hand back each result.
///
/// A session no longer listening is left out. One session's failure never ends
/// the walk: every advertised session is asked, whatever the one before it
/// answered.
fn restart_advertised_sessions(
    runtime_dir: &Path,
    installed: &str,
    wait: Duration,
) -> Vec<(SessionId, SessionOutcome)> {
    let mut outcomes = Vec::new();
    for session_id in ipc_client::advertised_sessions(runtime_dir) {
        let outcome = match restart_running_session(runtime_dir, session_id) {
            Ok(SessionRestart::NotRunning) => continue,
            Ok(SessionRestart::TooOld) => SessionOutcome::TooOld,
            Ok(SessionRestart::Restarting) => {
                match wait_for_version(installed, wait, || {
                    probe_session_version(runtime_dir, session_id)
                }) {
                    VersionAnswer::Installed => SessionOutcome::Confirmed,
                    VersionAnswer::Other(version) => SessionOutcome::StillOn(version),
                    VersionAnswer::Silent => SessionOutcome::Unconfirmed,
                }
            }
            Err(err) => SessionOutcome::Failed(err.to_string()),
        };
        outcomes.push((session_id, outcome));
    }
    outcomes
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
fn check_for_update(allow_prerelease: bool) -> Result<Option<String>, String> {
    let tag = latest_release(allow_prerelease)?;
    Ok(is_newer(&tag).then_some(tag))
}

/// Fetches the newest eligible release tag. With pre-releases allowed it reads
/// the release list (pre-releases included) and picks the highest version by
/// semver, not the newest by date. Otherwise it reads the `latest` endpoint,
/// which GitHub limits to stable releases.
fn latest_release(allow_prerelease: bool) -> Result<String, String> {
    if allow_prerelease {
        let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=25");
        highest_version(get_json(&url)?)
    } else {
        let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
        let release: Release = get_json(&url)?;
        Ok(release.tag_name)
    }
}

/// The tag of the highest version among `releases` by semver order. A tag
/// that does not parse as a version is skipped; an empty or all-unparsable
/// list is `no releases found`.
///
/// `["v0.3.0-rc.2", "v0.3.0-rc.10", "v0.2.0"]` gives `v0.3.0-rc.10` —
/// publish dates play no part.
fn highest_version(releases: Vec<Release>) -> Result<String, String> {
    releases
        .into_iter()
        .filter_map(|release| {
            Version::parse(strip_v(&release.tag_name))
                .ok()
                .map(|version| (version, release.tag_name))
        })
        .max_by(|left, right| left.0.cmp(&right.0))
        .map(|(_, tag)| tag)
        .ok_or_else(|| "no releases found".to_string())
}

/// True when `tag` names a version strictly newer than this build. A tag or
/// build version that does not parse as semver reads as not newer.
fn is_newer(tag: &str) -> bool {
    match (
        Version::parse(strip_v(tag)),
        Version::parse(strip_v(APP_VERSION)),
    ) {
        (Ok(latest), Ok(current)) => latest > current,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Download, extract, install
// ---------------------------------------------------------------------------

/// Checks for a newer release, downloads its archive, unpacks the binary, and
/// swaps it for the running executable. Both temp files are securely created
/// and auto-removed when their [`TempPath`] drops at the end of this function,
/// whichever way it ends.
fn install_release(tag: &str) -> Result<(), String> {
    let url = binary_url(tag).ok_or_else(|| {
        format!(
            "no koshi release binary for {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    println!("downloading koshi {} …", strip_v(tag));
    let archive = download(&url)?;
    let binary = extract(archive.as_ref(), &url)?;
    install_binary(binary.as_ref())
}

/// The download URL for this platform's release archive at `tag`, or `None`
/// when koshi ships no binary for this OS/arch. Archive name matches the
/// release convention `koshi-v{version}-{os}-{arch}.{ext}`.
fn binary_url(tag: &str) -> Option<String> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        _ => return None,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        _ => return None,
    };
    let ext = if os == "windows" { "zip" } else { "tar.gz" };
    let file = format!("koshi-v{}-{os}-{arch}.{ext}", strip_v(tag));
    Some(format!(
        "https://github.com/{REPO}/releases/download/{tag}/{file}"
    ))
}

/// Downloads `url` into a temp file and returns its path. The file is created
/// exclusively under a random name: it follows and truncates no existing file
/// or symbolic link.
fn download(url: &str) -> Result<TempPath, String> {
    let mut response = agent(DOWNLOAD_TIMEOUT)
        .get(url)
        .header("User-Agent", "koshi")
        .call()
        .map_err(|err| err.to_string())?;
    let mut file = Builder::new()
        .prefix("koshi-update-")
        .tempfile()
        .map_err(|err| err.to_string())?;
    let mut reader = response.body_mut().as_reader();
    io::copy(&mut reader, file.as_file_mut()).map_err(|err| err.to_string())?;
    Ok(file.into_temp_path())
}

/// Unpacks the koshi binary out of the downloaded archive to a temp file,
/// choosing the tar.gz or zip reader from the URL suffix.
fn extract(archive: &Path, url: &str) -> Result<TempPath, String> {
    if url.ends_with(".zip") {
        extract_zip(archive)
    } else {
        extract_tar_gz(archive)
    }
}

/// Unpacks the binary from a gzip-compressed tar archive.
fn extract_tar_gz(archive: &Path) -> Result<TempPath, String> {
    let file = fs::File::open(archive).map_err(|err| err.to_string())?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    for entry in tar.entries().map_err(|err| err.to_string())? {
        let mut entry = entry.map_err(|err| err.to_string())?;
        // Only a regular file counts: a directory or symlink named `koshi`
        // would otherwise be "saved" as an empty or wrong binary.
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let is_binary = entry
            .path()
            .ok()
            .and_then(|path| path.file_name().map(|name| name == binary_name()))
            .unwrap_or(false);
        if is_binary {
            return save_binary(&mut entry);
        }
    }
    Err("binary not found in archive".to_string())
}

/// Unpacks the binary from a zip archive.
fn extract_zip(archive: &Path) -> Result<TempPath, String> {
    let file = fs::File::open(archive).map_err(|err| err.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|err| err.to_string())?;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|err| err.to_string())?;
        let is_binary = Path::new(entry.name())
            .file_name()
            .map(|name| name == binary_name())
            .unwrap_or(false);
        if is_binary {
            return save_binary(&mut entry);
        }
    }
    Err("binary not found in archive".to_string())
}

/// Copies an extracted binary stream to a temp file, made executable on Unix.
/// The file is created exclusively under a random name: it follows and
/// truncates no existing file or symbolic link.
fn save_binary(source: &mut impl Read) -> Result<TempPath, String> {
    let mut file = Builder::new()
        .prefix("koshi-update-")
        .tempfile()
        .map_err(|err| err.to_string())?;
    io::copy(source, file.as_file_mut()).map_err(|err| err.to_string())?;
    #[cfg(unix)]
    make_executable(file.path())?;
    Ok(file.into_temp_path())
}

/// The binary's file name inside a release archive on this platform.
fn binary_name() -> &'static str {
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
    if let Ok(exe) = std::env::current_exe() {
        let _ = fs::remove_file(exe.with_extension("old"));
    }
}

/// Swaps the running executable for `new_binary`.
fn install_binary(new_binary: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|err| err.to_string())?;
    swap_exe(new_binary, &exe)
}

/// Replaces the executable on Unix atomically. The new binary is staged as a
/// sibling of `exe` — same directory, so the same filesystem — then renamed
/// over `exe`. Renaming a running binary is safe on Unix: the live process
/// keeps the old inode. The swap is a single rename: an interrupted copy never
/// touches the running binary, and the replacement either fully happens or not
/// at all. A permission error on the staging directory escalates
/// to sudo.
#[cfg(unix)]
fn swap_exe(new_binary: &Path, exe: &Path) -> Result<(), String> {
    let staged = exe.with_file_name(format!(
        "{}.koshi-update-{}",
        exe.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("koshi"),
        std::process::id()
    ));
    if let Err(err) = fs::copy(new_binary, &staged) {
        // A copy into the exe's own directory fails when that directory is not
        // writable (e.g. a root-owned /usr/local/bin) — escalate to sudo.
        if err.kind() == io::ErrorKind::PermissionDenied {
            return replace_with_sudo(new_binary, exe);
        }
        return Err(err.to_string());
    }
    if let Err(err) = make_executable(&staged) {
        let _ = fs::remove_file(&staged);
        return Err(err);
    }
    match fs::rename(&staged, exe) {
        Ok(()) => {
            let _ = fs::remove_file(new_binary);
            Ok(())
        }
        Err(err) => {
            let _ = fs::remove_file(&staged);
            Err(err.to_string())
        }
    }
}

/// Replaces the executable on Windows: a running binary cannot be overwritten,
/// so stage the new one beside the exe (a copy crosses drives, so both later
/// renames stay on the exe's own volume), rename the running exe aside, move the
/// staged one into place, and restore the old one if that final move fails.
#[cfg(windows)]
fn swap_exe(new_binary: &Path, exe: &Path) -> Result<(), String> {
    // The staging name sits beside the exe, on the exe's own volume, so both
    // renames below stay within one volume.
    let staged = exe.with_file_name(format!("koshi-update-{}.exe", std::process::id()));
    fs::copy(new_binary, &staged).map_err(|err| err.to_string())?;
    let backup = exe.with_extension("old");
    if let Err(err) = fs::rename(exe, &backup) {
        let _ = fs::remove_file(&staged);
        return Err(err.to_string());
    }
    if let Err(err) = fs::rename(&staged, exe) {
        let _ = fs::rename(&backup, exe);
        let _ = fs::remove_file(&staged);
        return Err(err.to_string());
    }
    // The old image is locked against deletion while it runs; `remove_stale_backup`
    // clears it on the next launch.
    let _ = fs::remove_file(&backup);
    Ok(())
}

/// Installs `new_binary` over `exe` with `sudo`, for a binary in a root-owned
/// directory. `install -m 755` writes the file and sets its mode in one step.
#[cfg(unix)]
fn replace_with_sudo(new_binary: &Path, exe: &Path) -> Result<(), String> {
    eprintln!(
        "koshi: updating {} needs elevated permissions",
        exe.display()
    );
    let status = std::process::Command::new("sudo")
        .arg("install")
        .arg("-m")
        .arg("755")
        .arg(new_binary)
        .arg(exe)
        .status()
        .map_err(|err| err.to_string())?;
    if !status.success() {
        return Err("`sudo install` failed".to_string());
    }
    let _ = fs::remove_file(new_binary);
    Ok(())
}

/// Sets the Unix executable bit (`0755`) on `path`.
#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|err| err.to_string())
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
    last_check: Option<u64>,
}

/// The path of the koshi-owned update state file, if a state directory exists.
fn state_path() -> Option<PathBuf> {
    koshi_paths::state_dir().map(|dir| dir.join("update.json"))
}

/// Reads the update state, defaulting on a missing or unreadable file.
fn load_state() -> UpdateState {
    let Some(path) = state_path() else {
        return UpdateState::default();
    };
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => UpdateState::default(),
    }
}

/// Writes the update state, creating the state directory if needed.
///
/// ponytail: plain write, not atomic — `update.json` is disposable, a torn
/// write just forces a re-check next launch. Atomic write is for session data.
fn save_state(state: &UpdateState) -> io::Result<()> {
    let path = state_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no state directory"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(state).map_err(io::Error::other)?;
    fs::write(&path, text)
}

/// Records the current time as the last check, ignoring a write failure.
fn persist_last_check() {
    let mut state = load_state();
    state.last_check = Some(now_secs());
    let _ = save_state(&state);
}

/// True when the interval has elapsed since the last check, or none has run.
fn is_due(state: &UpdateState, interval_days: u32) -> bool {
    match state.last_check {
        None => true,
        Some(last) => now_secs().saturating_sub(last) >= u64::from(interval_days) * SECONDS_PER_DAY,
    }
}

// ---------------------------------------------------------------------------
// Config (user-owned): koshi.kdl `update` section
// ---------------------------------------------------------------------------

/// Reads the `update` section of `koshi.kdl`. A missing or unreadable file
/// gives the defaults, auto-check on. A file that is present and does not
/// parse gives auto-check off.
fn load_update_config() -> UpdateConfig {
    let Some(path) = koshi_paths::config_dir().map(|dir| dir.join("koshi.kdl")) else {
        return UpdateConfig::default();
    };
    let Ok(source) = fs::read_to_string(&path) else {
        return UpdateConfig::default();
    };
    match parse_app_config(&path, &source) {
        // Only the strict `update` section matters here; a bad field there is
        // still an `Err` (fail closed), so field-partial warnings are ignored.
        Ok(file) => merge_client(ClientConfig::default(), vec![file.layer]).update,
        Err(err) => {
            tracing::warn!(%err, "koshi.kdl did not parse; disabling auto update check");
            UpdateConfig {
                auto_check: false,
                ..UpdateConfig::default()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// A configured HTTP agent whose whole call — connection through body — is
/// bounded by `timeout`. The API check passes [`API_TIMEOUT`]; the binary
/// download passes [`DOWNLOAD_TIMEOUT`].
///
/// The agent encrypts with [`koshi_ipc::tls::crypto_provider`], the provider
/// koshi's own TLS streams use. `ureq` is built with no provider feature and
/// takes the one it is given here.
fn agent(timeout: Duration) -> Agent {
    Agent::new_with_config(
        Agent::config_builder()
            .timeout_global(Some(timeout))
            .tls_config(
                TlsConfig::builder()
                    .unversioned_rustls_crypto_provider(koshi_ipc::tls::crypto_provider())
                    .build(),
            )
            .build(),
    )
}

/// Fetches `url` and decodes the JSON body, sending the User-Agent and Accept
/// headers GitHub's API requires.
fn get_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, String> {
    let body = agent(API_TIMEOUT)
        .get(url)
        .header("User-Agent", "koshi")
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|err| err.to_string())?
        .body_mut()
        .read_to_string()
        .map_err(|err| err.to_string())?;
    serde_json::from_str(&body).map_err(|err| err.to_string())
}

/// Drops a leading `v` from a tag or version string.
fn strip_v(version: &str) -> &str {
    version.strip_prefix('v').unwrap_or(version)
}

/// Current Unix time in whole seconds, or `0` if the clock is before the epoch.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Builds a [`CliError::Update`] from a failure detail.
fn update_err(detail: impl Into<String>) -> CliError {
    CliError::Update {
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests;
