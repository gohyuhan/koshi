//! Tests for the label a client takes and the number its `color_index` prints
//! as.

use std::collections::HashSet;

use super::*;

/// Whether `label` follows the `C-<adjective>-<noun>` shape of a generated
/// client name.
fn is_generated_client_label(label: &str) -> bool {
    let label_parts: Vec<&str> = label.split('-').collect();
    label_parts.len() == 3 && label_parts[0] == "C"
}

#[test]
fn every_color_index_prints_as_one_more_than_itself() {
    for (color_index, expected_client_number) in
        [(0, 1), (1, 2), (11, 12), (u32::MAX, 4_294_967_296)]
    {
        assert_eq!(
            compute_client_number(color_index),
            expected_client_number,
            "number of color index {color_index}"
        );
    }
}

#[test]
fn sanitize_client_label_folds_whitespace_drops_refused_characters_and_cuts_to_32_cells() {
    let forty_ascii_characters = "a".repeat(40);
    let twenty_wide_characters = "日".repeat(20);
    let thirty_one_ascii_then_wide = format!("{}日", "a".repeat(31));
    let thirty_one_ascii_then_word = format!("{} bob", "a".repeat(31));
    for (raw_label, expected_label) in [
        ("bob", "bob".to_string()),
        ("Bob\tSmith\n", "Bob Smith".to_string()),
        ("  bob \u{2028}\u{85} smith  ", "bob smith".to_string()),
        ("\u{1b}[31mbob", "[31mbob".to_string()),
        ("ali\u{202E}nimda", "alinimda".to_string()),
        ("a \u{202E} b", "a b".to_string()),
        ("bo\u{7f}b\u{9b}", "bob".to_string()),
        ("   ", String::new()),
        ("", String::new()),
        ("\u{1b}\u{7}", String::new()),
        (forty_ascii_characters.as_str(), "a".repeat(32)),
        (twenty_wide_characters.as_str(), "日".repeat(16)),
        (thirty_one_ascii_then_wide.as_str(), "a".repeat(31)),
        (thirty_one_ascii_then_word.as_str(), "a".repeat(31)),
    ] {
        assert_eq!(
            sanitize_client_label(raw_label),
            expected_label,
            "label of {raw_label:?}"
        );
    }
}

#[test]
fn sanitize_client_label_keeps_at_most_512_bytes_of_zero_width_characters() {
    let combining_mark_label = format!("b{}", "\u{301}".repeat(1000));

    let sanitized_label = sanitize_client_label(&combining_mark_label);

    assert_eq!(sanitized_label, format!("b{}", "\u{301}".repeat(255)));
    assert_eq!(sanitized_label.len(), 511);
}

#[test]
fn sanitize_client_label_reads_a_label_of_many_megabytes_into_at_most_32_cells() {
    let long_label = format!("{}bob", " ".repeat(4 * 1024 * 1024));

    assert_eq!(sanitize_client_label(&long_label), "bob");
    assert_eq!(
        sanitize_client_label(&"x".repeat(16 * 1024 * 1024)),
        "x".repeat(32)
    );
}

#[test]
fn resolve_client_label_names_a_remote_client_by_its_grant_identity_and_suffixes_a_held_one() {
    for (held_labels, expected_label) in [
        (vec![], "bob"),
        (vec!["bob"], "bob-2"),
        (vec!["bob", "bob-2"], "bob-3"),
        (vec!["bob-2"], "bob"),
        (vec!["host"], "bob"),
    ] {
        let held_labels: HashSet<&str> = held_labels.into_iter().collect();
        assert_eq!(
            resolve_client_label(ClientOrigin::Remote, Some("bob"), |candidate_label| {
                held_labels.contains(candidate_label)
            }),
            expected_label,
            "held labels {held_labels:?}"
        );
    }
}

#[test]
fn resolve_client_label_names_the_host_account_host_whatever_identity_it_carries() {
    let no_label_held = |_: &str| false;
    assert_eq!(
        resolve_client_label(
            ClientOrigin::Local { is_same_user: true },
            None,
            no_label_held
        ),
        "host"
    );
    assert_eq!(
        resolve_client_label(
            ClientOrigin::Local { is_same_user: true },
            Some("alice"),
            no_label_held
        ),
        "host"
    );
    assert_eq!(
        resolve_client_label(
            ClientOrigin::Local { is_same_user: true },
            None,
            |candidate_label| candidate_label == "host"
        ),
        "host-2"
    );
}

#[test]
fn resolve_client_label_generates_a_name_for_another_account_and_for_a_remote_client_with_no_usable_identity(
) {
    let held_labels: HashSet<&str> = ["host", "bob"].into_iter().collect();
    for (client_origin, granted_identity) in [
        (
            ClientOrigin::Local {
                is_same_user: false,
            },
            None,
        ),
        (
            ClientOrigin::Local {
                is_same_user: false,
            },
            Some("alice"),
        ),
        (ClientOrigin::Remote, None),
        (ClientOrigin::Remote, Some("   ")),
        (ClientOrigin::Remote, Some("\u{1b}\u{202E}")),
    ] {
        let resolved_label =
            resolve_client_label(client_origin, granted_identity, |candidate_label| {
                held_labels.contains(candidate_label)
            });
        assert!(
            is_generated_client_label(&resolved_label),
            "{client_origin:?} with {granted_identity:?} gave {resolved_label}"
        );
    }
}

#[test]
fn resolve_client_label_names_a_remote_client_whose_grant_is_host_host() {
    assert_eq!(
        resolve_client_label(ClientOrigin::Remote, Some("host"), |_| false),
        "host"
    );
    assert_eq!(
        resolve_client_label(ClientOrigin::Remote, Some("host"), |candidate_label| {
            candidate_label == "host"
        }),
        "host-2"
    );
}

#[test]
fn resolve_client_label_suffixes_an_identity_that_matches_a_generated_label() {
    assert_eq!(
        resolve_client_label(
            ClientOrigin::Remote,
            Some("C-swift-otter"),
            |candidate_label| { candidate_label == "C-swift-otter" }
        ),
        "C-swift-otter-2"
    );
}

#[test]
fn resolve_client_label_cuts_a_held_base_so_base_and_suffix_fit_32_cells() {
    let thirty_two_ascii_characters = "a".repeat(32);
    let sixteen_wide_characters = "日".repeat(16);
    let twenty_nine_ascii_then_space_word = format!("{} b", "a".repeat(29));
    for (granted_identity, expected_label) in [
        (
            thirty_two_ascii_characters.as_str(),
            format!("{}-2", "a".repeat(30)),
        ),
        (
            sixteen_wide_characters.as_str(),
            format!("{}-2", "日".repeat(15)),
        ),
        (
            twenty_nine_ascii_then_space_word.as_str(),
            format!("{}-2", "a".repeat(29)),
        ),
    ] {
        let held_label = sanitize_client_label(granted_identity);
        let resolved_label = resolve_client_label(
            ClientOrigin::Remote,
            Some(granted_identity),
            |candidate_label| candidate_label == held_label,
        );
        assert_eq!(
            resolved_label, expected_label,
            "identity {granted_identity:?}"
        );
        assert!(compute_text_display_width(&resolved_label) <= MAX_CLIENT_LABEL_CELL_COUNT);
    }
}

#[test]
fn resolve_client_label_cuts_further_for_a_two_digit_suffix() {
    let thirty_two_ascii_characters = "a".repeat(32);
    let held_labels: HashSet<String> = std::iter::once(thirty_two_ascii_characters.clone())
        .chain((2..=9).map(|suffix_number| format!("{}-{suffix_number}", "a".repeat(30))))
        .collect();

    let resolved_label = resolve_client_label(
        ClientOrigin::Remote,
        Some(&thirty_two_ascii_characters),
        |candidate_label| held_labels.contains(candidate_label),
    );

    assert_eq!(resolved_label, format!("{}-10", "a".repeat(29)));
}

#[test]
fn client_origin_encodes_local_with_its_account_and_remote_as_a_word() {
    for (client_origin, expected_json) in [
        (
            ClientOrigin::Local { is_same_user: true },
            serde_json::json!({"Local": {"is_same_user": true}}),
        ),
        (
            ClientOrigin::Local {
                is_same_user: false,
            },
            serde_json::json!({"Local": {"is_same_user": false}}),
        ),
        (ClientOrigin::Remote, serde_json::json!("Remote")),
    ] {
        let encoded_origin = serde_json::to_value(client_origin).expect("origin encodes");
        assert_eq!(encoded_origin, expected_json);
        assert_eq!(
            serde_json::from_value::<ClientOrigin>(encoded_origin).expect("origin decodes"),
            client_origin
        );
    }
}

#[test]
fn client_origin_refuses_the_bare_local_word() {
    let decode_error = serde_json::from_value::<ClientOrigin>(serde_json::json!("Local"))
        .expect_err("a bare Local names no account");
    assert_eq!(
        decode_error.to_string(),
        "invalid type: unit variant, expected struct variant"
    );
}
