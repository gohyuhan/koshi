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
//! Nothing here is used by the daemon or the running session: it is a
//! CLI-side, one-shot flow, so it reads the clock and the network directly
//! rather than through the runtime's injected services.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use koshi_config::app_config::parse_app_config;
use koshi_config::layer::merge;
use koshi_config::types::{KoshiConfig, UpdateConfig};
use semver::Version;
use serde::{Deserialize, Serialize};
use tempfile::{Builder, TempPath};
use ureq::Agent;

use crate::error::CliError;

/// This build's version, from the crate version bumped before each release.
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The GitHub `owner/repo` the release archives live under.
const REPO: &str = "gohyuhan/koshi";

/// How long the GitHub API check may run before it is abandoned, so a slow or
/// hung endpoint never stalls a launch. Bounds a small JSON reply, so it is
/// short.
const API_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a binary download may run before it is abandoned. Generous, because
/// the timeout covers streaming a multi-megabyte binary, which over a slow link
/// takes far longer than an API reply — while still bounding a stuck transfer.
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
    Ok(())
}

/// On an interactive launch, when auto-check is enabled and a check is due,
/// look for a newer release and offer to install it. Every failure is
/// swallowed: a startup update check must never block or crash a normal
/// launch. Runs before the terminal enters raw mode, so the prompt is a plain
/// stdin read.
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
    // Record this attempt before the network call, for every outcome, so a
    // failing or slow check waits a full interval instead of repeating — and
    // stalling on the timeout — on every launch while offline or firewalled.
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
    if !prompt_yes(&prompt) {
        return;
    }
    match install_release(&tag) {
        Ok(()) => {
            println!("updated to koshi {} — relaunch to use it", strip_v(&tag));
            std::process::exit(0);
        }
        Err(err) => eprintln!("koshi: update failed: {err}"),
    }
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
/// semver — not the newest by date, which a backport to an older line could
/// otherwise win. Otherwise it reads the `latest` endpoint, which GitHub limits
/// to stable releases.
fn latest_release(allow_prerelease: bool) -> Result<String, String> {
    if allow_prerelease {
        let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=25");
        let releases: Vec<Release> = get_json(&url)?;
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
    } else {
        let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
        let release: Release = get_json(&url)?;
        Ok(release.tag_name)
    }
}

/// True when `tag` names a version strictly newer than this build. A tag or
/// build version that does not parse as semver is treated as not-newer, so a
/// malformed tag never triggers a download.
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

/// Downloads `url` into a securely-created temp file and returns its path. The
/// file has a random name and is created exclusively, so it never follows or
/// truncates a pre-existing file or symlink of a guessable name.
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

/// Copies an extracted binary stream to a securely-created temp file, made
/// executable on Unix. The random-named, exclusively-created temp file never
/// follows or truncates an existing file or symlink of a guessable name.
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
/// keeps the old inode. Because the swap is a single rename, an interrupted
/// copy never touches the running binary, and the replacement either fully
/// happens or not at all. A permission error on the staging directory escalates
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
    // Stage on the exe's own volume so the final rename can't fail cross-drive
    // (the download temp dir may be on a different drive than a portable exe).
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
/// falls back to defaults (auto-check on), since no opt-out was expressed. A
/// file that is present but fails to parse fails **closed** — auto-check off —
/// because it may carry an `auto-check #false` we could not read, and a network
/// check should never be silently re-enabled by an unrelated typo.
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
        Ok((layer, _warnings)) => merge(KoshiConfig::default(), vec![layer]).update,
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
/// bounded by `timeout`. The API check uses a short bound; the binary download
/// uses a long one, since the timeout also covers streaming the body and a
/// multi-megabyte binary over a slow link needs far longer than a JSON reply.
fn agent(timeout: Duration) -> Agent {
    Agent::new_with_config(
        Agent::config_builder()
            .timeout_global(Some(timeout))
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

/// Prints `prompt`, reads a line from stdin, and returns whether it is a yes.
fn prompt_yes(prompt: &str) -> bool {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes" | "Yes")
}

/// Builds a [`CliError::Update`] from a failure detail.
fn update_err(detail: impl Into<String>) -> CliError {
    CliError::Update {
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests;
