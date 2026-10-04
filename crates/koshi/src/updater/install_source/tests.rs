//! Tests for telling where a koshi program file came from: a build from
//! source, a Homebrew keg, a Scoop app folder, or a release binary no package
//! manager tracks.

use tempfile::TempDir;

use super::*;

/// An empty file at `file_path`, with its folders made, and its path.
fn write_empty_file(file_path: PathBuf) -> PathBuf {
    std::fs::create_dir_all(file_path.parent().expect("the file has a folder"))
        .expect("the folders are made");
    std::fs::write(&file_path, b"").expect("the file is written");
    file_path
}

/// A test directory, and its path with every symbolic link followed.
fn build_install_root() -> (TempDir, PathBuf) {
    let install_root = TempDir::new().expect("a test directory");
    let install_root_path =
        std::fs::canonicalize(install_root.path()).expect("the test directory resolves");
    (install_root, install_root_path)
}

#[test]
fn a_build_without_the_release_marker_is_a_source_build_wherever_it_sits() {
    let (_install_root, install_root_path) = build_install_root();
    let program_path = write_empty_file(install_root_path.join("Cellar/koshi/0.5.0/bin/koshi"));
    write_empty_file(install_root_path.join("Cellar/koshi/0.5.0/INSTALL_RECEIPT.json"));

    assert_eq!(
        find_install_source(&program_path, false),
        InstallSource::SourceBuild
    );
}

#[test]
fn a_release_binary_in_a_homebrew_keg_is_homebrew_with_the_brew_beside_the_cellar() {
    let (_install_root, install_root_path) = build_install_root();
    let program_path = write_empty_file(install_root_path.join("Cellar/koshi/0.5.0/bin/koshi"));
    write_empty_file(install_root_path.join("Cellar/koshi/0.5.0/INSTALL_RECEIPT.json"));

    assert_eq!(
        find_install_source(&program_path, true),
        InstallSource::Release(ReleaseInstall::Homebrew {
            brew_path: install_root_path.join("bin/brew"),
            formula_name: "koshi".to_string(),
        })
    );
}

#[cfg(unix)]
#[test]
fn a_homebrew_link_to_a_keg_is_homebrew() {
    let (_install_root, install_root_path) = build_install_root();
    let keg_program_path = write_empty_file(install_root_path.join("Cellar/koshi/0.5.0/bin/koshi"));
    write_empty_file(install_root_path.join("Cellar/koshi/0.5.0/INSTALL_RECEIPT.json"));
    let link_path = install_root_path.join("bin/koshi");
    std::fs::create_dir_all(install_root_path.join("bin")).expect("the bin folder is made");
    std::os::unix::fs::symlink(&keg_program_path, &link_path).expect("the link is made");

    assert_eq!(
        find_install_source(&link_path, true),
        InstallSource::Release(ReleaseInstall::Homebrew {
            brew_path: install_root_path.join("bin/brew"),
            formula_name: "koshi".to_string(),
        })
    );
}

#[test]
fn a_release_binary_from_a_pinned_formula_names_that_formula() {
    let (_install_root, install_root_path) = build_install_root();
    let program_path =
        write_empty_file(install_root_path.join("Cellar/koshi@0.5.0/0.5.0/bin/koshi"));
    write_empty_file(install_root_path.join("Cellar/koshi@0.5.0/0.5.0/INSTALL_RECEIPT.json"));

    assert_eq!(
        find_install_source(&program_path, true),
        InstallSource::Release(ReleaseInstall::Homebrew {
            brew_path: install_root_path.join("bin/brew"),
            formula_name: "koshi@0.5.0".to_string(),
        })
    );
}

#[test]
fn a_cellar_shaped_folder_without_a_homebrew_receipt_is_standalone() {
    let (_install_root, install_root_path) = build_install_root();
    let program_path = write_empty_file(install_root_path.join("Cellar/koshi/0.5.0/bin/koshi"));

    assert_eq!(
        find_install_source(&program_path, true),
        InstallSource::Release(ReleaseInstall::Standalone)
    );
}

#[test]
fn a_release_binary_in_a_scoop_app_folder_is_scoop_under_either_receipt_name() {
    let (_install_root, install_root_path) = build_install_root();
    let current_receipt_program_path =
        write_empty_file(install_root_path.join("scoop/apps/koshi/0.5.0/koshi.exe"));
    write_empty_file(install_root_path.join("scoop/apps/koshi/0.5.0/scoop-install.json"));
    let older_receipt_program_path =
        write_empty_file(install_root_path.join("old-scoop/apps/koshi/0.4.0/koshi.exe"));
    write_empty_file(install_root_path.join("old-scoop/apps/koshi/0.4.0/install.json"));

    assert_eq!(
        [
            find_install_source(&current_receipt_program_path, true),
            find_install_source(&older_receipt_program_path, true),
        ],
        [
            InstallSource::Release(ReleaseInstall::Scoop),
            InstallSource::Release(ReleaseInstall::Scoop),
        ]
    );
}

#[test]
fn a_scoop_shaped_folder_without_a_scoop_receipt_is_standalone() {
    let (_install_root, install_root_path) = build_install_root();
    let program_path = write_empty_file(install_root_path.join("scoop/apps/koshi/0.5.0/koshi.exe"));

    assert_eq!(
        find_install_source(&program_path, true),
        InstallSource::Release(ReleaseInstall::Standalone)
    );
}

#[test]
fn a_release_binary_placed_by_the_install_script_or_by_hand_is_standalone() {
    let (_install_root, install_root_path) = build_install_root();
    let program_path = write_empty_file(install_root_path.join("usr/local/bin/koshi"));

    assert_eq!(
        [
            find_install_source(&program_path, true),
            find_install_source(&install_root_path.join("missing/koshi"), true),
        ],
        [
            InstallSource::Release(ReleaseInstall::Standalone),
            InstallSource::Release(ReleaseInstall::Standalone),
        ]
    );
}
