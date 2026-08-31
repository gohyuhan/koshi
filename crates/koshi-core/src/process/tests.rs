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
fn shell_kind_of_an_empty_program_path_is_other_with_an_empty_name() {
    // An empty path has no file stem, so `unwrap_or_default()` yields "" —
    // must not panic and must not match any known shell.
    assert_eq!(
        ShellKind::from_program(Path::new("")),
        ShellKind::Other(String::new())
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

#[test]
fn the_default_shell_spec_passes_the_callers_cwd_and_env_through_and_takes_no_arguments() {
    let cwd = PathBuf::from("/tmp/koshi-default-shell");
    let mut env = BTreeMap::new();
    env.insert("KOSHI_SESSION_ID".to_string(), "abc".to_string());

    let spec = SpawnSpec::default_shell(Some(cwd.clone()), env.clone());

    assert_eq!(spec.cwd, Some(cwd));
    assert_eq!(spec.env, env);
    assert_eq!(spec.args, Vec::<String>::new());
}

#[test]
fn the_default_shell_program_is_never_empty_and_its_kind_matches_that_program() {
    let spec = SpawnSpec::default_shell(None, BTreeMap::new());

    assert_ne!(spec.program, PathBuf::new());
    assert_eq!(spec.shell_kind, ShellKind::from_program(&spec.program));
    assert_eq!(spec.cwd, None);
    assert_eq!(spec.env, BTreeMap::new());
}

#[test]
fn shell_kind_matching_ignores_ascii_case() {
    assert_eq!(ShellKind::from_program(Path::new("ZSH")), ShellKind::Zsh);
    assert_eq!(ShellKind::from_program(Path::new("Bash")), ShellKind::Bash);
    assert_eq!(
        ShellKind::from_program(Path::new("/usr/bin/FISH")),
        ShellKind::Fish
    );
}

#[test]
fn shell_kind_of_a_versioned_program_name_is_other_with_the_stem_before_the_last_dot() {
    // `file_stem` cuts at the last `.`, so `bash-5.2` leaves `bash-5`.
    assert_eq!(
        ShellKind::from_program(Path::new("/usr/bin/bash-5.2")),
        ShellKind::Other("bash-5".to_string())
    );
}

#[test]
fn shell_kind_of_an_uppercase_unknown_program_is_other_with_the_lowercased_stem() {
    assert_eq!(
        ShellKind::from_program(Path::new("Elvish.EXE")),
        ShellKind::Other("elvish".to_string())
    );
}

#[cfg(unix)]
#[test]
fn shell_kind_of_a_non_utf8_program_name_is_other_with_an_empty_name() {
    use std::os::unix::ffi::OsStrExt;
    let program = Path::new(std::ffi::OsStr::from_bytes(b"/bin/z\xffsh"));
    assert_eq!(
        ShellKind::from_program(program),
        ShellKind::Other(String::new())
    );
}

#[test]
fn shell_kind_serializes_known_shells_as_bare_names_and_other_with_its_program() {
    assert_eq!(
        serde_json::to_string(&ShellKind::Zsh).expect("serialize"),
        r#""Zsh""#
    );
    assert_eq!(
        serde_json::to_string(&ShellKind::PowerShell).expect("serialize"),
        r#""PowerShell""#
    );
    assert_eq!(
        serde_json::to_string(&ShellKind::Other("elvish".to_string())).expect("serialize"),
        r#"{"Other":"elvish"}"#
    );
}

#[test]
fn kill_policy_force_and_tree_serialize_as_bare_names() {
    assert_eq!(
        serde_json::to_string(&KillPolicy::Force).expect("serialize"),
        r#""Force""#
    );
    assert_eq!(
        serde_json::to_string(&KillPolicy::Tree).expect("serialize"),
        r#""Tree""#
    );
}

#[test]
fn kill_policy_refuses_a_negative_timeout() {
    let refusal = serde_json::from_str::<KillPolicy>(r#"{"Graceful":{"timeout":-1}}"#)
        .expect_err("a negative second count is refused");
    assert!(refusal.to_string().contains("u64"), "{refusal}");
}

#[test]
fn kill_policy_refuses_a_fractional_timeout() {
    let refusal = serde_json::from_str::<KillPolicy>(r#"{"GracefulTree":{"timeout":3.5}}"#)
        .expect_err("a fractional second count is refused");
    assert!(refusal.to_string().contains("u64"), "{refusal}");
}

#[test]
fn kill_policy_zero_and_max_timeouts_roundtrip() {
    for timeout in [Duration::ZERO, Duration::from_secs(u64::MAX)] {
        let policy = KillPolicy::Graceful { timeout };
        let json = serde_json::to_string(&policy).expect("serialize");
        let back: KillPolicy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, policy);
    }
    assert_eq!(
        serde_json::to_string(&KillPolicy::Graceful {
            timeout: Duration::ZERO
        })
        .expect("serialize"),
        r#"{"Graceful":{"timeout":0}}"#
    );
}

#[test]
fn exit_status_serializes_as_a_tagged_integer() {
    assert_eq!(
        serde_json::to_string(&ExitStatus::ExitCode(0)).expect("serialize"),
        r#"{"ExitCode":0}"#
    );
    assert_eq!(
        serde_json::to_string(&ExitStatus::Signaled(9)).expect("serialize"),
        r#"{"Signaled":9}"#
    );
}

#[test]
fn exit_status_keeps_a_negative_exit_code() {
    let json = serde_json::to_string(&ExitStatus::ExitCode(-1)).expect("serialize");
    assert_eq!(json, r#"{"ExitCode":-1}"#);
    let back: ExitStatus = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, ExitStatus::ExitCode(-1));
}

#[test]
fn pty_size_serializes_cols_then_rows() {
    assert_eq!(
        serde_json::to_string(&PtySize { cols: 80, rows: 24 }).expect("serialize"),
        r#"{"cols":80,"rows":24}"#
    );
}

#[test]
fn pty_size_zero_and_max_roundtrip() {
    for size in [
        PtySize { cols: 0, rows: 0 },
        PtySize {
            cols: u16::MAX,
            rows: u16::MAX,
        },
    ] {
        let json = serde_json::to_string(&size).expect("serialize");
        let back: PtySize = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, size);
    }
}

#[test]
fn pty_size_refuses_a_dimension_past_u16() {
    let refusal = serde_json::from_str::<PtySize>(r#"{"cols":65536,"rows":24}"#)
        .expect_err("a column count past u16 is refused");
    assert!(refusal.to_string().contains("u16"), "{refusal}");
}

#[test]
fn spawn_spec_serializes_with_its_field_names_and_sorted_env() {
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
    assert_eq!(
        serde_json::to_string(&spec).expect("serialize"),
        r#"{"program":"/bin/zsh","args":["-l"],"cwd":"/home/u","env":{"LANG":"en_US.UTF-8","TERM":"xterm-256color"},"shell_kind":"Zsh"}"#
    );
}

#[test]
fn spawn_spec_with_no_cwd_serializes_cwd_as_null() {
    let spec = SpawnSpec::shell(PathBuf::from("/bin/sh"), None, BTreeMap::new());
    assert_eq!(
        serde_json::to_string(&spec).expect("serialize"),
        r#"{"program":"/bin/sh","args":[],"cwd":null,"env":{},"shell_kind":{"Other":"sh"}}"#
    );
}

#[test]
fn spawn_spec_shell_derives_the_kind_from_the_program_and_takes_no_arguments() {
    let cwd = PathBuf::from("/tmp/koshi-shell");
    let mut env = BTreeMap::new();
    env.insert("KOSHI_SESSION_ID".to_string(), "abc".to_string());

    let spec = SpawnSpec::shell(
        PathBuf::from("/usr/bin/fish"),
        Some(cwd.clone()),
        env.clone(),
    );

    assert_eq!(
        spec,
        SpawnSpec {
            program: PathBuf::from("/usr/bin/fish"),
            args: Vec::new(),
            cwd: Some(cwd),
            env,
            shell_kind: ShellKind::Fish,
        }
    );
}
