//! Tests for the versioned-surface table: that every surface in it follows the
//! rule, that the check catches each way of breaking it, and that the table
//! names every surface exactly once.

use super::*;

use std::collections::HashSet;

#[test]
fn every_surface_follows_the_version_rule() {
    let version_problems: Vec<String> = SURFACES
        .iter()
        .filter_map(Surface::find_version_problem)
        .collect();

    assert_eq!(version_problems, Vec::<String>::new());
}

#[test]
fn the_table_names_every_surface_once() {
    let mut surface_names = HashSet::new();
    for surface in SURFACES {
        assert!(
            surface_names.insert(surface.surface_name),
            "{} appears in the table twice",
            surface.surface_name
        );
    }

    assert_eq!(surface_names.len(), SURFACES.len());
}

#[test]
fn every_surface_is_named() {
    for surface in SURFACES {
        assert!(
            !surface.surface_name.is_empty(),
            "a surface in the table has no name"
        );
    }
}

#[test]
fn a_floor_above_the_ceiling_is_no_version_at_all() {
    let inverted_surface = Surface {
        surface_name: "sample",
        minimum_version: 3,
        maximum_version: 2,
        released_version: Some(2),
    };

    assert_eq!(
        inverted_surface.find_version_problem(),
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
    let doubly_broken_surface = Surface {
        surface_name: "sample",
        minimum_version: 5,
        maximum_version: 4,
        released_version: Some(1),
    };

    assert_eq!(
        doubly_broken_surface.find_version_problem(),
        Some(
            "the sample accepts 5 at the lowest and 4 at the highest, which is no version at all"
                .to_string()
        )
    );
}

#[test]
fn two_steps_above_the_released_value_is_a_problem() {
    let over_bumped_surface = Surface {
        surface_name: "sample",
        minimum_version: 1,
        maximum_version: 3,
        released_version: Some(1),
    };

    assert_eq!(
        over_bumped_surface.find_version_problem(),
        Some(
            "the sample speaks 3, which is more than one step above the 1 the last release spoke"
                .to_string()
        )
    );
}

#[test]
fn dropping_below_the_released_value_is_a_problem() {
    let regressed_surface = Surface {
        surface_name: "sample",
        minimum_version: 1,
        maximum_version: 1,
        released_version: Some(2),
    };

    assert_eq!(
        regressed_surface.find_version_problem(),
        Some("the sample speaks 1, which is below the 2 the last release spoke".to_string())
    );
}

#[test]
fn one_step_above_the_released_value_is_the_allowed_move() {
    let bumped_once_surface = Surface {
        surface_name: "sample",
        minimum_version: 1,
        maximum_version: 2,
        released_version: Some(1),
    };

    assert_eq!(bumped_once_surface.find_version_problem(), None);
}

#[test]
fn holding_at_the_released_value_is_allowed() {
    let unmoved_surface = Surface {
        surface_name: "sample",
        minimum_version: 2,
        maximum_version: 2,
        released_version: Some(2),
    };

    assert_eq!(unmoved_surface.find_version_problem(), None);
}

#[test]
fn a_surface_no_release_carries_is_checked_on_the_floor_alone() {
    // Any ceiling is allowed; a floor above that ceiling is still no version
    // at all.
    let unreleased_surface = Surface {
        surface_name: "sample",
        minimum_version: 1,
        maximum_version: 9,
        released_version: None,
    };

    assert_eq!(unreleased_surface.find_version_problem(), None);

    let inverted_surface = Surface {
        minimum_version: 4,
        maximum_version: 3,
        ..unreleased_surface
    };

    assert_eq!(
        inverted_surface.find_version_problem(),
        Some(
            "the sample accepts 4 at the lowest and 3 at the highest, which is no version at all"
                .to_string()
        )
    );
}

#[test]
fn the_session_protocol_speaks_four_and_accepts_nothing_older() {
    assert_eq!(SESSION_PROTOCOL.minimum_version, 4);
    assert_eq!(SESSION_PROTOCOL.maximum_version, 4);
    assert_eq!(SESSION_PROTOCOL.released_version, Some(3));
}

#[test]
fn the_session_protocol_stands_one_step_above_its_release_anchor() {
    assert_eq!(
        SESSION_PROTOCOL.released_version,
        Some(SESSION_PROTOCOL.maximum_version - 1)
    );
    assert_eq!(SESSION_PROTOCOL.find_version_problem(), None);
}

#[test]
fn the_resume_format_speaks_four_and_accepts_nothing_older() {
    assert_eq!(RESUME_FORMAT.minimum_version, 4);
    assert_eq!(RESUME_FORMAT.maximum_version, 4);
    assert_eq!(RESUME_FORMAT.released_version, Some(3));
}

#[test]
fn the_control_plane_speaks_three_and_accepts_nothing_older() {
    assert_eq!(CONTROL_PROTOCOL.minimum_version, 3);
    assert_eq!(CONTROL_PROTOCOL.maximum_version, 3);
    assert_eq!(CONTROL_PROTOCOL.released_version, Some(2));
}

/// The `max` each surface reads in the `v0.4.0` tag, which is the last
/// release. A surface that tag does not carry reads `None`.
const WHAT_V0_4_0_SPEAKS: [(&str, Option<u32>); 10] = [
    ("session protocol", Some(3)),
    ("control plane", Some(2)),
    ("supervisor link", Some(1)),
    ("token store format", Some(1)),
    ("remote doorway", Some(1)),
    ("saved server file format", Some(1)),
    ("remote certificate file format", Some(1)),
    ("remote access record format", Some(1)),
    ("resume file format", Some(3)),
    ("config schema", Some(1)),
];

#[test]
fn every_anchor_holds_the_version_the_v0_4_0_tag_speaks() {
    let released_versions: Vec<(&str, Option<u32>)> = SURFACES
        .iter()
        .map(|surface| (surface.surface_name, surface.released_version))
        .collect();

    assert_eq!(released_versions, WHAT_V0_4_0_SPEAKS);
}

#[test]
fn the_table_pins_every_surface_by_name_and_numbers() {
    let surface_versions: Vec<(&str, u32, u32, Option<u32>)> = SURFACES
        .iter()
        .map(|surface| {
            (
                surface.surface_name,
                surface.minimum_version,
                surface.maximum_version,
                surface.released_version,
            )
        })
        .collect();

    assert_eq!(
        surface_versions,
        [
            ("session protocol", 4, 4, Some(3)),
            ("control plane", 3, 3, Some(2)),
            ("supervisor link", 2, 2, Some(1)),
            ("token store format", 2, 2, Some(1)),
            ("remote doorway", 2, 2, Some(1)),
            ("saved server file format", 2, 2, Some(1)),
            ("remote certificate file format", 2, 2, Some(1)),
            ("remote access record format", 2, 2, Some(1)),
            ("resume file format", 4, 4, Some(3)),
            ("config schema", 2, 2, Some(1)),
        ]
    );
}

#[test]
fn a_floor_raised_above_the_released_value_is_allowed() {
    // Only the ceiling is held to the released value; the floor may move past it.
    let floor_raised = Surface {
        surface_name: "sample",
        minimum_version: 3,
        maximum_version: 3,
        released_version: Some(2),
    };

    assert_eq!(floor_raised.find_version_problem(), None);
}

#[test]
fn a_floor_equal_to_the_ceiling_is_one_version() {
    let single_version_surface = Surface {
        surface_name: "sample",
        minimum_version: 0,
        maximum_version: 0,
        released_version: Some(0),
    };

    assert_eq!(single_version_surface.find_version_problem(), None);
}

#[test]
fn two_steps_above_a_released_zero_is_a_problem() {
    let over_bumped_surface = Surface {
        surface_name: "sample",
        minimum_version: 0,
        maximum_version: 2,
        released_version: Some(0),
    };

    assert_eq!(
        over_bumped_surface.find_version_problem(),
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
    let regressed_surface = Surface {
        surface_name: "sample",
        minimum_version: 0,
        maximum_version: 3,
        released_version: Some(9),
    };

    assert_eq!(
        regressed_surface.find_version_problem(),
        Some("the sample speaks 3, which is below the 9 the last release spoke".to_string())
    );
}

#[test]
fn the_problem_message_carries_the_surface_name() {
    let inverted_surface = Surface {
        surface_name: "remote doorway",
        minimum_version: 2,
        maximum_version: 1,
        released_version: None,
    };

    assert_eq!(
        inverted_surface.find_version_problem(),
        Some(
            "the remote doorway accepts 2 at the lowest and 1 at the highest, which is no version at all"
                .to_string()
        )
    );
}

#[test]
fn a_released_value_at_the_top_of_the_range_is_held_without_overflow() {
    let maxed_surface = Surface {
        surface_name: "sample",
        minimum_version: u32::MAX,
        maximum_version: u32::MAX,
        released_version: Some(u32::MAX),
    };

    assert_eq!(maxed_surface.find_version_problem(), None);
}
