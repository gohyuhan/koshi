//! Tests for generated default names: list integrity, same-language pairing,
//! deterministic walks from a fixed start, taken-name skipping, and the wrap
//! suffix once every combination is claimed.

use std::collections::BTreeSet;

use super::*;

/// Split a generated `<TYPE>-<adjective>-<noun>` name into its three parts.
/// Words never contain `-` (asserted separately), so a plain split is exact.
fn split_generated_name(generated_name: &str) -> (String, String, String) {
    let mut name_parts = generated_name.splitn(3, '-');
    let name_tag = name_parts.next().expect("type tag").to_string();
    let adjective = name_parts.next().expect("adjective").to_string();
    let noun = name_parts.next().expect("noun").to_string();
    (name_tag, adjective, noun)
}

#[test]
fn prefix_tags_are_s_and_t_and_c() {
    assert_eq!(NameKind::Session.get_type_tag(), "S");
    assert_eq!(NameKind::Tab.get_type_tag(), "T");
    assert_eq!(NameKind::Client.get_type_tag(), "C");
}

#[test]
fn every_list_has_fifty_unique_words_without_hyphens() {
    for word_list in [
        &EN_ADJECTIVES,
        &EN_NOUNS,
        &JA_ADJECTIVES,
        &JA_NOUNS,
        &ZH_HANT_ADJECTIVES,
        &ZH_HANT_NOUNS,
    ] {
        let unique_words: BTreeSet<&str> = word_list.iter().copied().collect();
        assert_eq!(unique_words.len(), 50);
        assert!(word_list.iter().all(|word| !word.contains('-')));
        assert!(word_list.iter().all(|word| !word.is_empty()));
    }
}

#[test]
fn start_zero_yields_the_first_combination() {
    let generated_name = generate_name_from_start(NameKind::Tab, |_| false, 0);
    assert_eq!(generated_name, "T-swift-otter");
}

#[test]
fn session_and_tab_kinds_tag_the_same_combination() {
    assert_eq!(
        generate_name_from_start(NameKind::Session, |_| false, 0),
        "S-swift-otter"
    );
    assert_eq!(
        generate_name_from_start(NameKind::Tab, |_| false, 0),
        "T-swift-otter"
    );
}

#[test]
fn a_taken_name_is_skipped_for_the_next_stride_candidate() {
    // Start 0 is `T-swift-otter`; one stride step lands on index 73 —
    // language 1 (Japanese), pair 24 — which is adjective 0 / noun 24.
    let generated_name = generate_name_from_start(
        NameKind::Tab,
        |candidate_name| candidate_name == "T-swift-otter",
        0,
    );
    assert_eq!(generated_name, "T-しずか-りす");
}

#[test]
fn adjective_and_noun_always_come_from_the_same_language() {
    let combinations_per_language = WORDS_PER_LIST * WORDS_PER_LIST;
    for starting_combination_index in 0..LANGUAGES.len() * combinations_per_language {
        let generated_name =
            generate_name_from_start(NameKind::Tab, |_| false, starting_combination_index);
        let (name_tag, adjective, noun) = split_generated_name(&generated_name);
        assert_eq!(name_tag, "T");
        let language_index = LANGUAGES
            .iter()
            .position(|(adjectives, _)| adjectives.contains(&adjective.as_str()))
            .expect("adjective from a known language");
        assert!(LANGUAGES[language_index].1.contains(&noun.as_str()));
    }
}

#[test]
fn every_start_yields_a_distinct_combination() {
    let combinations_per_language = WORDS_PER_LIST * WORDS_PER_LIST;
    let combination_count = LANGUAGES.len() * combinations_per_language;
    let generated_names: BTreeSet<String> = (0..combination_count)
        .map(|starting_combination_index| {
            generate_name_from_start(NameKind::Tab, |_| false, starting_combination_index)
        })
        .collect();
    assert_eq!(generated_names.len(), combination_count);
}

#[test]
fn exhausted_combinations_wrap_with_a_numeric_suffix() {
    // Every unsuffixed name is taken, so the walk wraps into round two and
    // starts over from the same stride order with `-2` appended.
    let generated_name = generate_name_from_start(
        NameKind::Tab,
        |candidate_name| !candidate_name.ends_with("-2"),
        0,
    );
    assert_eq!(generated_name, "T-swift-otter-2");
}

#[test]
fn wrap_suffix_numbering_matches_the_actual_wrap_round() {
    // `is_taken` rejects by STRUCTURE — a round-0 name has exactly two
    // hyphens (`T-word-word`), a wrapped one has three (`T-word-word-N`) —
    // never by the specific number the assertion expects. A predicate like
    // `!name.ends_with("-2")` instead describes the accepted *shape*, so a
    // walk that reaches that shape at the wrong round (e.g. round 1 emitting
    // "-1" instead of "-2") still satisfies it; this one does not.
    let round_zero_shape = |candidate_name: &str| candidate_name.matches('-').count() == 2;

    let first_wrap = generate_name_from_start(NameKind::Tab, round_zero_shape, 0);
    assert_eq!(first_wrap, "T-swift-otter-2");

    let round_zero_or_first_wrap_shape =
        |candidate_name: &str| round_zero_shape(candidate_name) || candidate_name.ends_with("-2");
    let second_wrap = generate_name_from_start(NameKind::Tab, round_zero_or_first_wrap_shape, 0);
    assert_eq!(second_wrap, "T-swift-otter-3");
}

#[test]
fn same_start_and_taken_set_always_yield_the_same_name() {
    let first_generated_name = generate_name_from_start(
        NameKind::Tab,
        |candidate_name| candidate_name.starts_with("T-s"),
        4242,
    );
    let second_generated_name = generate_name_from_start(
        NameKind::Tab,
        |candidate_name| candidate_name.starts_with("T-s"),
        4242,
    );
    assert_eq!(first_generated_name, second_generated_name);
}

#[test]
fn random_start_generates_a_well_formed_free_name() {
    let generated_name = generate_name(NameKind::Tab, |_| false);
    let (name_tag, adjective, noun) = split_generated_name(&generated_name);
    assert_eq!(name_tag, "T");
    let language_index = LANGUAGES
        .iter()
        .position(|(adjectives, _)| adjectives.contains(&adjective.as_str()))
        .expect("adjective from a known language");
    assert!(LANGUAGES[language_index].1.contains(&noun.as_str()));
}

#[test]
fn the_one_free_name_next_to_the_start_is_found_in_the_same_round() {
    // Combination 1 sits one place after the start; the coprime stride reaches
    // it within the first round. A stride that walks a smaller orbit skips it
    // and wraps to a `-2` name instead.
    let only_free_name = generate_name_from_start(NameKind::Tab, |_| false, 1);
    let found_name = generate_name_from_start(
        NameKind::Tab,
        |candidate_name| candidate_name != only_free_name,
        0,
    );
    assert_eq!(found_name, only_free_name);
}

#[test]
fn the_one_free_name_on_the_last_step_of_a_round_is_found_before_the_wrap() {
    // The walk from 0 reaches this combination on its final step. A round that
    // stops any earlier returns a wrapped `-2` name while a plain name is free.
    let last_combination_index =
        (TOTAL_NAME_COMBINATIONS - 1) * NAME_COMBINATION_STRIDE % TOTAL_NAME_COMBINATIONS;
    let only_free_name = generate_name_from_start(NameKind::Tab, |_| false, last_combination_index);
    let found_name = generate_name_from_start(
        NameKind::Tab,
        |candidate_name| candidate_name != only_free_name,
        0,
    );
    assert_eq!(found_name, only_free_name);
    assert_eq!(
        found_name.matches('-').count(),
        2,
        "{found_name} carries no wrap number"
    );
}

#[test]
fn stride_is_coprime_with_the_combination_count() {
    let mut greatest_common_divisor = NAME_COMBINATION_STRIDE;
    let mut remainder = TOTAL_NAME_COMBINATIONS;
    while remainder != 0 {
        (greatest_common_divisor, remainder) = (remainder, greatest_common_divisor % remainder);
    }
    assert_eq!(
        greatest_common_divisor, 1,
        "gcd({NAME_COMBINATION_STRIDE}, {TOTAL_NAME_COMBINATIONS})"
    );
}

#[test]
fn client_kind_tags_the_same_combination_with_c() {
    assert_eq!(
        generate_name_from_start(NameKind::Client, |_| false, 0),
        "C-swift-otter"
    );
}

#[test]
fn the_last_index_yields_the_last_traditional_chinese_combination() {
    // Index 7499: language 7499 % 3 == 2, pair 2499 == adjective 49 / noun 49.
    let generated_name =
        generate_name_from_start(NameKind::Tab, |_| false, TOTAL_NAME_COMBINATIONS - 1);
    assert_eq!(generated_name, "T-自在-茶館");
}

#[test]
fn a_start_equal_to_the_combination_count_wraps_to_the_first_combination() {
    let generated_name =
        generate_name_from_start(NameKind::Tab, |_| false, TOTAL_NAME_COMBINATIONS);
    assert_eq!(generated_name, "T-swift-otter");
}

#[test]
fn a_taken_name_in_the_wrap_round_is_skipped_for_the_next_stride_candidate() {
    // Every plain name is taken, and so is the first `-2` candidate; the walk
    // moves one stride step inside the wrap round.
    let is_taken = |candidate_name: &str| {
        candidate_name.matches('-').count() == 2 || candidate_name == "T-swift-otter-2"
    };
    let generated_name = generate_name_from_start(NameKind::Tab, is_taken, 0);
    assert_eq!(generated_name, "T-しずか-りす-2");
}

#[test]
fn a_random_index_with_bound_one_is_zero() {
    assert_eq!(generate_random_index(1), 0);
}

#[test]
fn a_random_index_stays_below_its_bound() {
    for _ in 0..1_000 {
        let random_index = generate_random_index(7);
        assert!(random_index < 7, "{random_index}");
    }
}
