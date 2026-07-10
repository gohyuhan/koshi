//! Tests for process lifecycle and spawn types.

use super::*;
use std::ffi::OsString;
use std::path::Path;

#[test]
fn shell_program_uses_a_set_nonempty_value() {
    assert_eq!(
        shell_program(Some(OsString::from("/usr/bin/fish")), "/bin/sh"),
        PathBuf::from("/usr/bin/fish"),
    );
}

#[test]
fn shell_program_falls_back_when_unset() {
    assert_eq!(shell_program(None, "/bin/sh"), PathBuf::from("/bin/sh"));
}

#[test]
fn shell_program_treats_a_set_but_empty_value_as_unset() {
    assert_eq!(
        shell_program(Some(OsString::new()), "/bin/sh"),
        PathBuf::from("/bin/sh"),
    );
}

#[test]
fn kill_policy_serializes_timeout_as_seconds() {
    let policy = KillPolicy::Graceful {
        timeout: Duration::from_secs(3),
    };
    let json = serde_json::to_string(&policy).expect("serialize");
    // Timeout is a bare integer count of seconds, not a struct.
    assert_eq!(json, r#"{"Graceful":{"timeout":3}}"#);
}

#[test]
fn kill_policy_graceful_tree_serializes_timeout_as_seconds() {
    let policy = KillPolicy::GracefulTree {
        timeout: Duration::from_secs(3),
    };
    let json = serde_json::to_string(&policy).expect("serialize");
    // Timeout is a bare integer count of seconds, not a struct.
    assert_eq!(json, r#"{"GracefulTree":{"timeout":3}}"#);
}

#[test]
fn kill_policy_roundtrips() {
    for policy in [
        KillPolicy::Graceful {
            timeout: Duration::from_secs(5),
        },
        KillPolicy::Force,
        KillPolicy::Tree,
        KillPolicy::GracefulTree {
            timeout: Duration::from_secs(5),
        },
    ] {
        let json = serde_json::to_string(&policy).expect("serialize");
        let back: KillPolicy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(policy, back);
    }
}

#[test]
fn kill_policy_drops_subsecond_part() {
    let policy = KillPolicy::Graceful {
        timeout: Duration::from_millis(3_750),
    };
    let json = serde_json::to_string(&policy).expect("serialize");
    let back: KillPolicy = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(
        back,
        KillPolicy::Graceful {
            timeout: Duration::from_secs(3),
        }
    );
}

#[test]
fn shell_kind_detects_known_shells() {
    assert_eq!(
        ShellKind::from_program(Path::new("/bin/zsh")),
        ShellKind::Zsh
    );
    assert_eq!(
        ShellKind::from_program(Path::new("/usr/bin/bash")),
        ShellKind::Bash
    );
    assert_eq!(
        ShellKind::from_program(Path::new("/usr/local/bin/fish")),
        ShellKind::Fish
    );
    assert_eq!(ShellKind::from_program(Path::new("nu")), ShellKind::Nu);
}

#[test]
fn shell_kind_detects_powershell_variants() {
    assert_eq!(
        ShellKind::from_program(Path::new("pwsh")),
        ShellKind::PowerShell
    );
    // `.exe` suffix is stripped by `file_stem`, and matching is case-insensitive.
    // Use a bare filename so the assertion is host-portable (a Windows
    // backslash path is a single opaque component on Unix).
    assert_eq!(
        ShellKind::from_program(Path::new("PowerShell.exe")),
        ShellKind::PowerShell
    );
}

#[test]
fn shell_kind_unknown_becomes_other() {
    assert_eq!(
        ShellKind::from_program(Path::new("/usr/bin/elvish")),
        ShellKind::Other("elvish".to_string())
    );
}

#[test]
fn spawn_spec_roundtrips() {
    let mut env = BTreeMap::new();
    env.insert("TERM".to_string(), "xterm-256color".to_string());
    env.insert("LANG".to_string(), "en_US.UTF-8".to_string());
    let spec = SpawnSpec {
        program: PathBuf::from("/bin/zsh"),
        args: vec!["-l".to_string()],
        cwd: Some(PathBuf::from("/home/u")),
        env,
        shell_kind: ShellKind::Zsh,
    };
    let json = serde_json::to_string(&spec).expect("serialize");
    let back: SpawnSpec = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(spec, back);
}

#[test]
fn pty_size_roundtrips() {
    let size = PtySize { cols: 80, rows: 24 };
    let json = serde_json::to_string(&size).expect("serialize");
    let back: PtySize = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(size, back);
}

#[test]
fn exit_status_roundtrips() {
    for status in [
        ExitStatus::ExitCode(0),
        ExitStatus::ExitCode(1),
        ExitStatus::Signaled(9),
    ] {
        let json = serde_json::to_string(&status).expect("serialize");
        let back: ExitStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(status, back);
    }
}

#[test]
fn tree_scoped_widens_each_policy_to_its_group_flavor() {
    let timeout = Duration::from_secs(3);
    let cases = [
        (
            KillPolicy::Graceful { timeout },
            KillPolicy::GracefulTree { timeout },
        ),
        (KillPolicy::Force, KillPolicy::Tree),
        (KillPolicy::Tree, KillPolicy::Tree),
        (
            KillPolicy::GracefulTree { timeout },
            KillPolicy::GracefulTree { timeout },
        ),
    ];
    for (policy, widened) in cases {
        assert_eq!(policy.tree_scoped(), widened, "{policy:?}");
    }
}
