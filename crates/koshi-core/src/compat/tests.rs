//! Tests for the versioned-surface table: that every surface in it follows the
//! rule, that the check catches each way of breaking it, and that the table
//! names every surface exactly once.

use super::*;

use std::collections::HashSet;

#[test]
fn every_surface_follows_the_version_rule() {
    // The rule this asserts used to live only in prose above six constants,
    // and two of them carried a wrong number until a person re-read it.
    let problems: Vec<String> = SURFACES
        .iter()
        .filter_map(Surface::version_problem)
        .collect();

    assert_eq!(problems, Vec::<String>::new());
}

#[test]
fn the_table_names_every_surface_once() {
    let mut seen = HashSet::new();
    for surface in SURFACES {
        assert!(
            seen.insert(surface.name),
            "{} appears in the table twice",
            surface.name
        );
    }

    assert_eq!(seen.len(), SURFACES.len());
}

#[test]
fn every_surface_is_named() {
    for surface in SURFACES {
        assert!(
            !surface.name.is_empty(),
            "a surface in the table has no name"
        );
    }
}

#[test]
fn a_floor_above_the_ceiling_is_no_version_at_all() {
    let inverted = Surface {
        name: "sample",
        min: 3,
        max: 2,
        released: Some(2),
    };

    assert_eq!(
        inverted.version_problem(),
        Some(
            "the sample accepts 3 at the lowest and 2 at the highest, which is no version at all"
                .to_string()
        )
    );
}

#[test]
fn two_steps_above_the_released_value_is_a_problem() {
    // The real case this models: the control plane reached 3 on two bumps that
    // the rule does not allow, against a released value of 1.
    let over_bumped = Surface {
        name: "sample",
        min: 1,
        max: 3,
        released: Some(1),
    };

    assert_eq!(
        over_bumped.version_problem(),
        Some(
            "the sample speaks 3, which is more than one step above the 1 the last release spoke"
                .to_string()
        )
    );
}

#[test]
fn dropping_below_the_released_value_is_a_problem() {
    let regressed = Surface {
        name: "sample",
        min: 1,
        max: 1,
        released: Some(2),
    };

    assert_eq!(
        regressed.version_problem(),
        Some("the sample speaks 1, which is below the 2 the last release spoke".to_string())
    );
}

#[test]
fn one_step_above_the_released_value_is_the_allowed_move() {
    let bumped_once = Surface {
        name: "sample",
        min: 1,
        max: 2,
        released: Some(1),
    };

    assert_eq!(bumped_once.version_problem(), None);
}

#[test]
fn holding_at_the_released_value_is_allowed() {
    let unmoved = Surface {
        name: "sample",
        min: 2,
        max: 2,
        released: Some(2),
    };

    assert_eq!(unmoved.version_problem(), None);
}

#[test]
fn a_surface_no_release_carries_is_checked_on_the_floor_alone() {
    // Nothing released can disagree with it, so any ceiling is allowed — but a
    // floor above that ceiling is still no version at all.
    let unreleased = Surface {
        name: "sample",
        min: 1,
        max: 9,
        released: None,
    };

    assert_eq!(unreleased.version_problem(), None);

    let inverted = Surface {
        min: 4,
        max: 3,
        ..unreleased
    };

    assert_eq!(
        inverted.version_problem(),
        Some(
            "the sample accepts 4 at the lowest and 3 at the highest, which is no version at all"
                .to_string()
        )
    );
}

#[test]
fn the_session_protocol_speaks_two_and_accepts_nothing_older() {
    // Version 1 is v0.1.0, which has no attach, so a version-1 peer has
    // nothing to ask a session server for.
    assert_eq!(SESSION_PROTOCOL.min, 2);
    assert_eq!(SESSION_PROTOCOL.max, 2);
    assert_eq!(SESSION_PROTOCOL.released, Some(2));
}

#[test]
fn the_control_plane_speaks_two_and_still_serves_the_released_one() {
    // 0.2.0 speaks 1 and is still served; 2 is this build's own, one step up
    // for the refusal code a caller can branch on.
    assert_eq!(CONTROL_PROTOCOL.min, 1);
    assert_eq!(CONTROL_PROTOCOL.max, 2);
    assert_eq!(CONTROL_PROTOCOL.released, Some(1));
}

#[test]
fn every_surface_no_release_carries_is_one_born_after_the_last_tag() {
    // Naming them one by one is the point: a surface added to the table lands
    // here too, so somebody states in this list that the new one really has
    // never shipped.
    let unreleased: Vec<&str> = SURFACES
        .iter()
        .filter(|surface| surface.released.is_none())
        .map(|surface| surface.name)
        .collect();

    assert_eq!(
        unreleased,
        [
            "supervisor link",
            "token store format",
            "remote doorway",
            "saved server file format",
            "remote certificate file format",
            "remote access record format",
            "resume file format"
        ]
    );
}
