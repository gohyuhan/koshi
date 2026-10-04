//! Where the koshi program file `koshi update` updates came from: a build
//! from source, a Homebrew keg, a Scoop app folder, or a release binary no
//! package manager tracks.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests;

/// The Homebrew formula that tracks the newest koshi release.
pub(super) const HOMEBREW_FORMULA_NAME: &str = "koshi";

/// The tap-qualified name of [`HOMEBREW_FORMULA_NAME`]: `gohyuhan/koshi/koshi`.
pub(super) const HOMEBREW_TAPPED_FORMULA_NAME: &str = "gohyuhan/koshi/koshi";

/// Where the koshi program file came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum InstallSource {
    /// A build from source: this build carries no release marker.
    SourceBuild,
    /// A release binary, and what placed it.
    Release(ReleaseInstall),
}

/// What placed a release binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReleaseInstall {
    /// A Homebrew keg. `brew_path` is the `brew` program of that Homebrew, and
    /// `formula_name` the formula, such as `koshi` or `koshi@0.5.0`.
    Homebrew {
        brew_path: PathBuf,
        formula_name: String,
    },
    /// A Scoop app folder.
    Scoop,
    /// A file no package manager tracks: placed by `install.sh`,
    /// `install.ps1`, or by hand.
    Standalone,
}

/// Where the program file at `program_path` came from.
///
/// `is_release_build` `false` is [`InstallSource::SourceBuild`], wherever the
/// file sits. A release build is read from the file `program_path` names once
/// every symbolic link is followed:
///
/// - `<prefix>/Cellar/<formula>/<version>/bin/koshi`, with
///   `INSTALL_RECEIPT.json` in `<prefix>/Cellar/<formula>/<version>`, is
///   [`ReleaseInstall::Homebrew`] with the `brew` program
///   `<prefix>/bin/brew`.
/// - `<root>/apps/koshi/<version>/koshi.exe`, with `scoop-install.json` or
///   `install.json` beside it, is [`ReleaseInstall::Scoop`].
/// - Every other file, and a path whose links cannot be followed, is
///   [`ReleaseInstall::Standalone`].
///
/// Example: `/opt/homebrew/bin/koshi`, a link to
/// `/opt/homebrew/Cellar/koshi/0.5.0/bin/koshi`, gives Homebrew with
/// `/opt/homebrew/bin/brew` and the formula `koshi`.
pub(super) fn find_install_source(program_path: &Path, is_release_build: bool) -> InstallSource {
    if !is_release_build {
        return InstallSource::SourceBuild;
    }
    let Ok(program_file_path) = std::fs::canonicalize(program_path) else {
        return InstallSource::Release(ReleaseInstall::Standalone);
    };
    if let Some(homebrew_install) = find_homebrew_install(&program_file_path) {
        return InstallSource::Release(homebrew_install);
    }
    if is_scoop_install(&program_file_path) {
        return InstallSource::Release(ReleaseInstall::Scoop);
    }
    InstallSource::Release(ReleaseInstall::Standalone)
}

/// The Homebrew keg the program file at `program_file_path` sits in, as
/// [`find_install_source`] states, or `None` when it sits in none.
fn find_homebrew_install(program_file_path: &Path) -> Option<ReleaseInstall> {
    let bin_directory = program_file_path.parent()?;
    let keg_directory = bin_directory.parent()?;
    let formula_directory = keg_directory.parent()?;
    let cellar_directory = formula_directory.parent()?;
    if bin_directory.file_name()? != "bin" || cellar_directory.file_name()? != "Cellar" {
        return None;
    }
    if !keg_directory.join("INSTALL_RECEIPT.json").is_file() {
        return None;
    }
    Some(ReleaseInstall::Homebrew {
        brew_path: cellar_directory.parent()?.join("bin").join("brew"),
        formula_name: formula_directory
            .file_name()?
            .to_string_lossy()
            .into_owned(),
    })
}

/// Whether the program file at `program_file_path` sits in a Scoop app
/// folder, as [`find_install_source`] states.
fn is_scoop_install(program_file_path: &Path) -> bool {
    let Some(version_directory) = program_file_path.parent() else {
        return false;
    };
    let app_directory = version_directory.parent();
    let apps_directory = app_directory.and_then(Path::parent);
    app_directory.and_then(Path::file_name) == Some(OsStr::new("koshi"))
        && apps_directory.and_then(Path::file_name) == Some(OsStr::new("apps"))
        && (version_directory.join("scoop-install.json").is_file()
            || version_directory.join("install.json").is_file())
}
