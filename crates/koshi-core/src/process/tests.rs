//! Tests for process lifecycle and spawn types.

use super::*;
use std::ffi::OsString;
use std::path::Path;

#[test]
fn shell_program_uses_a_set_nonempty_value() {
    assert_eq!(
        resolve_shell_program(Some(OsString::from("/usr/bin/fish")), "/bin/sh"),
        PathBuf::from("/usr/bin/fish"),
    );
}

#[test]
fn shell_program_falls_back_when_unset() {
    assert_eq!(
        resolve_shell_program(None, "/bin/sh"),
        PathBuf::from("/bin/sh")
    );
}

#[test]
fn shell_program_treats_a_set_but_empty_value_as_unset() {
    assert_eq!(
        resolve_shell_program(Some(OsString::new()), "/bin/sh"),
        PathBuf::from("/bin/sh"),
    );
}

#[test]
fn kill_policy_serializes_timeout_as_seconds() {
    let policy = KillPolicy::Graceful {
        timeout_duration: Duration::from_secs(3),
    };
    let policy_json = serde_json::to_string(&policy).expect("serialize");
    // Timeout is a bare integer count of seconds, not a struct.
    assert_eq!(policy_json, r#"{"Graceful":{"timeout_duration":3}}"#);
}

#[test]
fn kill_policy_graceful_tree_serializes_timeout_as_seconds() {
    let policy = KillPolicy::GracefulTree {
        timeout_duration: Duration::from_secs(3),
    };
    let policy_json = serde_json::to_string(&policy).expect("serialize");
    // Timeout is a bare integer count of seconds, not a struct.
    assert_eq!(policy_json, r#"{"GracefulTree":{"timeout_duration":3}}"#);
}

#[test]
fn kill_policy_roundtrips() {
    for policy in [
        KillPolicy::Graceful {
            timeout_duration: Duration::from_secs(5),
        },
        KillPolicy::Force,
        KillPolicy::Tree,
        KillPolicy::GracefulTree {
            timeout_duration: Duration::from_secs(5),
        },
    ] {
        let policy_json = serde_json::to_string(&policy).expect("serialize");
        let deserialized_policy: KillPolicy =
            serde_json::from_str(&policy_json).expect("deserialize");
        assert_eq!(policy, deserialized_policy);
    }
}

#[test]
fn kill_policy_drops_subsecond_part() {
    let policy = KillPolicy::Graceful {
        timeout_duration: Duration::from_millis(3_750),
    };
    let policy_json = serde_json::to_string(&policy).expect("serialize");
    let deserialized_policy: KillPolicy = serde_json::from_str(&policy_json).expect("deserialize");
    assert_eq!(
        deserialized_policy,
        KillPolicy::Graceful {
            timeout_duration: Duration::from_secs(3),
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
    let mut environment_variables = BTreeMap::new();
    environment_variables.insert("TERM".to_string(), "xterm-256color".to_string());
    environment_variables.insert("LANG".to_string(), "en_US.UTF-8".to_string());
    let spawn_spec = SpawnSpec {
        program: PathBuf::from("/bin/zsh"),
        arguments: vec!["-l".to_string()],
        working_directory: Some(PathBuf::from("/home/u")),
        environment_variables,
        shell_kind: ShellKind::Zsh,
    };
    let spawn_spec_json = serde_json::to_string(&spawn_spec).expect("serialize");
    let deserialized_spawn_spec: SpawnSpec =
        serde_json::from_str(&spawn_spec_json).expect("deserialize");
    assert_eq!(spawn_spec, deserialized_spawn_spec);
}

#[test]
fn pty_size_roundtrips() {
    let pty_size = PtySize {
        column_count: 80,
        row_count: 24,
    };
    let pty_size_json = serde_json::to_string(&pty_size).expect("serialize");
    let decoded_pty_size: PtySize = serde_json::from_str(&pty_size_json).expect("deserialize");
    assert_eq!(pty_size, decoded_pty_size);
}

#[test]
fn exit_status_roundtrips() {
    for exit_status in [
        ExitStatus::ExitCode(0),
        ExitStatus::ExitCode(1),
        ExitStatus::Signaled(9),
    ] {
        let exit_status_json = serde_json::to_string(&exit_status).expect("serialize");
        let deserialized_exit_status: ExitStatus =
            serde_json::from_str(&exit_status_json).expect("deserialize");
        assert_eq!(exit_status, deserialized_exit_status);
    }
}

#[test]
fn tree_scoped_widens_each_policy_to_its_group_flavor() {
    let timeout_duration = Duration::from_secs(3);
    let policy_cases = [
        (
            KillPolicy::Graceful { timeout_duration },
            KillPolicy::GracefulTree { timeout_duration },
        ),
        (KillPolicy::Force, KillPolicy::Tree),
        (KillPolicy::Tree, KillPolicy::Tree),
        (
            KillPolicy::GracefulTree { timeout_duration },
            KillPolicy::GracefulTree { timeout_duration },
        ),
    ];
    for (kill_policy, expected_group_policy) in policy_cases {
        assert_eq!(
            kill_policy.apply_tree_scope(),
            expected_group_policy,
            "{kill_policy:?}"
        );
    }
}

#[test]
fn the_default_shell_spec_passes_the_callers_working_directory_and_environment_variables_through_and_takes_no_arguments(
) {
    let working_directory = PathBuf::from("/tmp/koshi-default-shell");
    let mut environment_variables = BTreeMap::new();
    environment_variables.insert("KOSHI_SESSION_ID".to_string(), "abc".to_string());

    let spawn_spec = SpawnSpec::default_shell(
        Some(working_directory.clone()),
        environment_variables.clone(),
    );

    assert_eq!(spawn_spec.working_directory, Some(working_directory));
    assert_eq!(spawn_spec.environment_variables, environment_variables);
    assert_eq!(spawn_spec.arguments, Vec::<String>::new());
}

#[test]
fn the_default_shell_program_is_never_empty_and_its_kind_matches_that_program() {
    let spawn_spec = SpawnSpec::default_shell(None, BTreeMap::new());

    assert_ne!(spawn_spec.program, PathBuf::new());
    assert_eq!(
        spawn_spec.shell_kind,
        ShellKind::from_program(&spawn_spec.program)
    );
    assert_eq!(spawn_spec.working_directory, None);
    assert_eq!(spawn_spec.environment_variables, BTreeMap::new());
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
    let negative_timeout_parse_error =
        serde_json::from_str::<KillPolicy>(r#"{"Graceful":{"timeout_duration":-1}}"#)
            .expect_err("a negative second count is refused");
    assert!(
        negative_timeout_parse_error.to_string().contains("u64"),
        "{negative_timeout_parse_error}"
    );
}

#[test]
fn kill_policy_refuses_a_fractional_timeout() {
    let fractional_timeout_parse_error =
        serde_json::from_str::<KillPolicy>(r#"{"GracefulTree":{"timeout_duration":3.5}}"#)
            .expect_err("a fractional second count is refused");
    assert!(
        fractional_timeout_parse_error.to_string().contains("u64"),
        "{fractional_timeout_parse_error}"
    );
}

#[test]
fn kill_policy_zero_and_max_timeouts_roundtrip() {
    for timeout_duration in [Duration::ZERO, Duration::from_secs(u64::MAX)] {
        let policy = KillPolicy::Graceful { timeout_duration };
        let policy_json = serde_json::to_string(&policy).expect("serialize");
        let deserialized_policy: KillPolicy =
            serde_json::from_str(&policy_json).expect("deserialize");
        assert_eq!(deserialized_policy, policy);
    }
    assert_eq!(
        serde_json::to_string(&KillPolicy::Graceful {
            timeout_duration: Duration::ZERO
        })
        .expect("serialize"),
        r#"{"Graceful":{"timeout_duration":0}}"#
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
    let exit_status_json = serde_json::to_string(&ExitStatus::ExitCode(-1)).expect("serialize");
    assert_eq!(exit_status_json, r#"{"ExitCode":-1}"#);
    let deserialized_exit_status: ExitStatus =
        serde_json::from_str(&exit_status_json).expect("deserialize");
    assert_eq!(deserialized_exit_status, ExitStatus::ExitCode(-1));
}

#[test]
fn pty_size_serializes_cols_then_rows() {
    assert_eq!(
        serde_json::to_string(&PtySize {
            column_count: 80,
            row_count: 24,
        })
        .expect("serialize"),
        r#"{"column_count":80,"row_count":24}"#
    );
}

#[test]
fn pty_size_zero_and_max_roundtrip() {
    for pty_size in [
        PtySize {
            column_count: 0,
            row_count: 0,
        },
        PtySize {
            column_count: u16::MAX,
            row_count: u16::MAX,
        },
    ] {
        let pty_size_json = serde_json::to_string(&pty_size).expect("serialize");
        let deserialized_pty_size: PtySize =
            serde_json::from_str(&pty_size_json).expect("deserialize");
        assert_eq!(deserialized_pty_size, pty_size);
    }
}

#[test]
fn pty_size_refuses_a_dimension_past_u16() {
    let pty_size_parse_error =
        serde_json::from_str::<PtySize>(r#"{"column_count":65536,"row_count":24}"#)
            .expect_err("a column count past u16 is refused");
    assert!(
        pty_size_parse_error.to_string().contains("u16"),
        "{pty_size_parse_error}"
    );
}

#[test]
fn spawn_spec_serializes_with_its_field_names_and_sorted_env() {
    let mut environment_variables = BTreeMap::new();
    environment_variables.insert("TERM".to_string(), "xterm-256color".to_string());
    environment_variables.insert("LANG".to_string(), "en_US.UTF-8".to_string());
    let spawn_spec = SpawnSpec {
        program: PathBuf::from("/bin/zsh"),
        arguments: vec!["-l".to_string()],
        working_directory: Some(PathBuf::from("/home/u")),
        environment_variables,
        shell_kind: ShellKind::Zsh,
    };
    assert_eq!(
        serde_json::to_string(&spawn_spec).expect("serialize"),
        r#"{"program":"/bin/zsh","arguments":["-l"],"working_directory":"/home/u","environment_variables":{"LANG":"en_US.UTF-8","TERM":"xterm-256color"},"shell_kind":"Zsh"}"#
    );
}

#[test]
fn spawn_spec_with_no_cwd_serializes_cwd_as_null() {
    let spawn_spec = SpawnSpec::from_shell_program(PathBuf::from("/bin/sh"), None, BTreeMap::new());
    assert_eq!(
        serde_json::to_string(&spawn_spec).expect("serialize"),
        r#"{"program":"/bin/sh","arguments":[],"working_directory":null,"environment_variables":{},"shell_kind":{"Other":"sh"}}"#
    );
}

#[test]
fn spawn_spec_shell_derives_the_kind_from_the_program_and_takes_no_arguments() {
    let working_directory = PathBuf::from("/tmp/koshi-shell");
    let mut environment_variables = BTreeMap::new();
    environment_variables.insert("KOSHI_SESSION_ID".to_string(), "abc".to_string());

    let spawn_spec = SpawnSpec::from_shell_program(
        PathBuf::from("/usr/bin/fish"),
        Some(working_directory.clone()),
        environment_variables.clone(),
    );

    assert_eq!(
        spawn_spec,
        SpawnSpec {
            program: PathBuf::from("/usr/bin/fish"),
            arguments: Vec::new(),
            working_directory: Some(working_directory),
            environment_variables,
            shell_kind: ShellKind::Fish,
        }
    );
}
