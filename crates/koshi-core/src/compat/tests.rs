//! Tests for the versioned-surface table: that every surface in it follows the
//! rule, that the check catches each way of breaking it, and that the table
//! names every surface exactly once.

use super::*;

use std::collections::HashSet;

#[test]
fn every_surface_follows_the_version_rule() {
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
fn a_surface_breaking_both_rules_is_reported_on_its_floor_first() {
    // The floor is above the ceiling AND the ceiling is two steps above the
    // released value; the floor check runs first and names the message.
    let doubly_broken = Surface {
        name: "sample",
        min: 5,
        max: 4,
        released: Some(1),
    };

    assert_eq!(
        doubly_broken.version_problem(),
        Some(
            "the sample accepts 5 at the lowest and 4 at the highest, which is no version at all"
                .to_string()
        )
    );
}

#[test]
fn two_steps_above_the_released_value_is_a_problem() {
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
    // Any ceiling is allowed; a floor above that ceiling is still no version
    // at all.
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
fn the_session_protocol_speaks_three_and_accepts_nothing_older() {
    assert_eq!(SESSION_PROTOCOL.min, 2);
    assert_eq!(SESSION_PROTOCOL.max, 3);
    assert_eq!(SESSION_PROTOCOL.released, Some(2));
}

#[test]
fn the_control_plane_speaks_two_and_still_serves_the_released_one() {
    assert_eq!(CONTROL_PROTOCOL.min, 1);
    assert_eq!(CONTROL_PROTOCOL.max, 2);
    assert_eq!(CONTROL_PROTOCOL.released, Some(1));
}

#[test]
fn every_surface_no_release_carries_is_one_born_after_the_last_tag() {
    // A surface added to the table with `released: None` lands in this list.
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

#[test]
fn the_table_pins_every_surface_by_name_and_numbers() {
    let rows: Vec<(&str, u32, u32, Option<u32>)> = SURFACES
        .iter()
        .map(|surface| (surface.name, surface.min, surface.max, surface.released))
        .collect();

    assert_eq!(
        rows,
        [
            ("session protocol", 2, 3, Some(2)),
            ("control plane", 1, 2, Some(1)),
            ("supervisor link", 1, 1, None),
            ("token store format", 1, 1, None),
            ("remote doorway", 1, 1, None),
            ("saved server file format", 1, 1, None),
            ("remote certificate file format", 1, 1, None),
            ("remote access record format", 1, 1, None),
            ("resume file format", 1, 3, None),
            ("config schema", 1, 1, Some(1)),
        ]
    );
}

#[test]
fn a_floor_raised_above_the_released_value_is_allowed() {
    // Only the ceiling is held to the released value; the floor may move past it.
    let floor_raised = Surface {
        name: "sample",
        min: 3,
        max: 3,
        released: Some(2),
    };

    assert_eq!(floor_raised.version_problem(), None);
}

#[test]
fn a_floor_equal_to_the_ceiling_is_one_version() {
    let single = Surface {
        name: "sample",
        min: 0,
        max: 0,
        released: Some(0),
    };

    assert_eq!(single.version_problem(), None);
}

#[test]
fn two_steps_above_a_released_zero_is_a_problem() {
    let over_bumped = Surface {
        name: "sample",
        min: 0,
        max: 2,
        released: Some(0),
    };

    assert_eq!(
        over_bumped.version_problem(),
        Some(
            "the sample speaks 2, which is more than one step above the 0 the last release spoke"
                .to_string()
        )
    );
}

#[test]
fn a_ceiling_below_the_released_value_is_reported_before_an_over_bump_can_be() {
    // `max` is below `released`, so the "below" check fires and the "more than
    // one step above" check is never reached.
    let regressed = Surface {
        name: "sample",
        min: 0,
        max: 3,
        released: Some(9),
    };

    assert_eq!(
        regressed.version_problem(),
        Some("the sample speaks 3, which is below the 9 the last release spoke".to_string())
    );
}

#[test]
fn the_problem_message_carries_the_surface_name() {
    let inverted = Surface {
        name: "remote doorway",
        min: 2,
        max: 1,
        released: None,
    };

    assert_eq!(
        inverted.version_problem(),
        Some(
            "the remote doorway accepts 2 at the lowest and 1 at the highest, which is no version at all"
                .to_string()
        )
    );
}

#[test]
fn a_released_value_at_the_top_of_the_range_is_held_without_overflow() {
    let maxed = Surface {
        name: "sample",
        min: u32::MAX,
        max: u32::MAX,
        released: Some(u32::MAX),
    };

    assert_eq!(maxed.version_problem(), None);
}
