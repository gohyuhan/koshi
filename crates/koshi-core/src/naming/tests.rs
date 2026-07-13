//! Tests for generated default names: list integrity, same-language pairing,
//! deterministic walks from a fixed start, taken-name skipping, and the wrap
//! suffix once every combination is claimed.

use std::collections::BTreeSet;

use super::*;

/// Split a generated `<TYPE>-<adjective>-<noun>` name into its three parts.
/// Words never contain `-` (asserted separately), so a plain split is exact.
fn parts(name: &str) -> (String, String, String) {
    let mut pieces = name.splitn(3, '-');
    let tag = pieces.next().expect("type tag").to_string();
    let adjective = pieces.next().expect("adjective").to_string();
    let noun = pieces.next().expect("noun").to_string();
    (tag, adjective, noun)
}

#[test]
fn prefix_tags_are_s_t_p() {
    assert_eq!(NameKind::Session.prefix(), "S");
    assert_eq!(NameKind::Tab.prefix(), "T");
    assert_eq!(NameKind::Pane.prefix(), "P");
}

#[test]
fn every_list_has_fifty_unique_words_without_hyphens() {
    for list in [
        &EN_ADJECTIVES,
        &EN_NOUNS,
        &JA_ADJECTIVES,
        &JA_NOUNS,
        &ZH_HANT_ADJECTIVES,
        &ZH_HANT_NOUNS,
    ] {
        let unique: BTreeSet<&str> = list.iter().copied().collect();
        assert_eq!(unique.len(), 50);
        assert!(list.iter().all(|word| !word.contains('-')));
        assert!(list.iter().all(|word| !word.is_empty()));
    }
}

#[test]
fn start_zero_yields_the_first_combination() {
    let name = generate_name_from(NameKind::Tab, |_| false, 0);
    assert_eq!(name, "T-swift-otter");
}

#[test]
fn session_and_pane_kinds_tag_the_same_combination() {
    assert_eq!(
        generate_name_from(NameKind::Session, |_| false, 0),
        "S-swift-otter"
    );
    assert_eq!(
        generate_name_from(NameKind::Pane, |_| false, 0),
        "P-swift-otter"
    );
}

#[test]
fn a_taken_name_is_skipped_for_the_next_stride_candidate() {
    // Start 0 is `T-swift-otter`; one stride step lands on index 73 —
    // language 1 (Japanese), pair 24 — which is adjective 0 / noun 24.
    let name = generate_name_from(NameKind::Tab, |name| name == "T-swift-otter", 0);
    assert_eq!(name, "T-しずか-りす");
}

#[test]
fn adjective_and_noun_always_come_from_the_same_language() {
    let per_language = WORDS_PER_LIST * WORDS_PER_LIST;
    for start in 0..LANGUAGES.len() * per_language {
        let name = generate_name_from(NameKind::Tab, |_| false, start);
        let (tag, adjective, noun) = parts(&name);
        assert_eq!(tag, "T");
        let language = LANGUAGES
            .iter()
            .position(|(adjectives, _)| adjectives.contains(&adjective.as_str()))
            .expect("adjective from a known language");
        assert!(LANGUAGES[language].1.contains(&noun.as_str()));
    }
}

#[test]
fn every_start_yields_a_distinct_combination() {
    let per_language = WORDS_PER_LIST * WORDS_PER_LIST;
    let combos = LANGUAGES.len() * per_language;
    let names: BTreeSet<String> = (0..combos)
        .map(|start| generate_name_from(NameKind::Tab, |_| false, start))
        .collect();
    assert_eq!(names.len(), combos);
}

#[test]
fn exhausted_combinations_wrap_with_a_numeric_suffix() {
    // Every unsuffixed name is taken, so the walk wraps into round two and
    // starts over from the same stride order with `-2` appended.
    let name = generate_name_from(NameKind::Tab, |name| !name.ends_with("-2"), 0);
    assert_eq!(name, "T-swift-otter-2");
}

#[test]
fn wrap_suffix_numbering_matches_the_actual_wrap_round() {
    // `is_taken` rejects by STRUCTURE — a round-0 name has exactly two
    // hyphens (`T-word-word`), a wrapped one has three (`T-word-word-N`) —
    // never by the specific number the assertion expects. A predicate like
    // `!name.ends_with("-2")` instead describes the accepted *shape*, so a
    // walk that reaches that shape at the wrong round (e.g. round 1 emitting
    // "-1" instead of "-2") still satisfies it; this one does not.
    let round0_shape = |name: &str| name.matches('-').count() == 2;

    let first_wrap = generate_name_from(NameKind::Tab, round0_shape, 0);
    assert_eq!(first_wrap, "T-swift-otter-2");

    let round0_or_first_wrap_shape = |name: &str| round0_shape(name) || name.ends_with("-2");
    let second_wrap = generate_name_from(NameKind::Tab, round0_or_first_wrap_shape, 0);
    assert_eq!(second_wrap, "T-swift-otter-3");
}

#[test]
fn same_start_and_taken_set_always_yield_the_same_name() {
    let first = generate_name_from(NameKind::Tab, |name| name.starts_with("T-s"), 4242);
    let second = generate_name_from(NameKind::Tab, |name| name.starts_with("T-s"), 4242);
    assert_eq!(first, second);
}

#[test]
fn random_start_generates_a_well_formed_free_name() {
    let name = generate_name(NameKind::Tab, |_| false);
    let (tag, adjective, noun) = parts(&name);
    assert_eq!(tag, "T");
    let language = LANGUAGES
        .iter()
        .position(|(adjectives, _)| adjectives.contains(&adjective.as_str()))
        .expect("adjective from a known language");
    assert!(LANGUAGES[language].1.contains(&noun.as_str()));
}
