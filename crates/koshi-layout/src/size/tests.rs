//! Tests for size constraints and weights.

use super::*;

fn assert_size_weight_round_trip(weight: &SizeWeight) {
    let serialized_weight_json = serde_json::to_string(weight).expect("serialize");
    let deserialized_weight: SizeWeight =
        serde_json::from_str(&serialized_weight_json).expect("deserialize");
    assert_eq!(*weight, deserialized_weight);
}

#[test]
fn default_weight_is_one_flex_share() {
    let weight = SizeWeight::default();
    assert_eq!(weight.primary_constraint, SizeConstraint::Flex(1));
    assert_eq!(weight.minimum_cell_count, None);
    assert_eq!(weight.preferred_cell_count, None);
    assert_eq!(weight.resize_delta, 0);
}

#[test]
fn every_constraint_kind_assert_size_weight_round_trips() {
    let size_constraints = [
        SizeConstraint::Flex(3),
        SizeConstraint::Percent(40),
        SizeConstraint::Fixed(80),
        SizeConstraint::Minimum(10),
        SizeConstraint::Preferred(120),
    ];
    for size_constraint in size_constraints {
        assert_size_weight_round_trip(&SizeWeight {
            primary_constraint: size_constraint,
            minimum_cell_count: None,
            preferred_cell_count: None,
            resize_delta: 0,
        });
    }
}

#[test]
fn combined_flex_with_overlays_assert_size_weight_round_trips() {
    assert_size_weight_round_trip(&SizeWeight {
        primary_constraint: SizeConstraint::Flex(2),
        minimum_cell_count: Some(20),
        preferred_cell_count: Some(50),
        resize_delta: -3,
    });
}

#[test]
fn constructors_accept_valid_values() {
    assert_eq!(
        SizeConstraint::from_flex_weight(1),
        Ok(SizeConstraint::Flex(1))
    );
    assert_eq!(
        SizeConstraint::from_percent(1),
        Ok(SizeConstraint::Percent(1))
    );
    assert_eq!(
        SizeConstraint::from_percent(100),
        Ok(SizeConstraint::Percent(100))
    );
    assert_eq!(
        SizeConstraint::from_fixed_cell_count(80),
        Ok(SizeConstraint::Fixed(80))
    );
    assert_eq!(
        SizeConstraint::from_minimum_cell_count(2),
        Ok(SizeConstraint::Minimum(2))
    );
    assert_eq!(
        SizeConstraint::from_preferred_cell_count(120),
        Ok(SizeConstraint::Preferred(120))
    );
}

#[test]
fn constructors_reject_invalid_values() {
    assert_eq!(
        SizeConstraint::from_flex_weight(0),
        Err(ConstraintError::ZeroFlexWeight)
    );
    assert_eq!(
        SizeConstraint::from_percent(0),
        Err(ConstraintError::PercentOutOfRange {
            received_percent: 0
        })
    );
    assert_eq!(
        SizeConstraint::from_percent(101),
        Err(ConstraintError::PercentOutOfRange {
            received_percent: 101,
        })
    );
    assert_eq!(
        SizeConstraint::from_fixed_cell_count(0),
        Err(ConstraintError::ZeroFixedCellCount)
    );
    assert_eq!(
        SizeConstraint::from_minimum_cell_count(0),
        Err(ConstraintError::ZeroMinimumCellCount)
    );
    assert_eq!(
        SizeConstraint::from_preferred_cell_count(0),
        Err(ConstraintError::ZeroPreferredCellCount)
    );
}

#[test]
fn constructors_accept_their_maximum_values() {
    assert_eq!(
        SizeConstraint::from_flex_weight(u32::MAX),
        Ok(SizeConstraint::Flex(u32::MAX))
    );
    assert_eq!(
        SizeConstraint::from_fixed_cell_count(u16::MAX),
        Ok(SizeConstraint::Fixed(u16::MAX))
    );
    assert_eq!(
        SizeConstraint::from_minimum_cell_count(u16::MAX),
        Ok(SizeConstraint::Minimum(u16::MAX))
    );
    assert_eq!(
        SizeConstraint::from_preferred_cell_count(u16::MAX),
        Ok(SizeConstraint::Preferred(u16::MAX))
    );
}

#[test]
fn percent_rejects_its_type_maximum() {
    assert_eq!(
        SizeConstraint::from_percent(u8::MAX),
        Err(ConstraintError::PercentOutOfRange {
            received_percent: u8::MAX,
        })
    );
}

#[test]
fn both_overlays_apply_to_any_primary() {
    let weight = SizeWeight {
        minimum_cell_count: Some(9),
        preferred_cell_count: Some(6),
        ..SizeWeight::from_primary_constraint(SizeConstraint::Percent(40))
    };
    assert_eq!(
        weight,
        SizeWeight {
            primary_constraint: SizeConstraint::Percent(40),
            minimum_cell_count: Some(9),
            preferred_cell_count: Some(6),
            resize_delta: 0,
        }
    );
}

#[test]
fn constraint_errors_display_their_exact_messages() {
    assert_eq!(
        ConstraintError::ZeroFlexWeight.to_string(),
        "flex weight must be at least 1"
    );
    assert_eq!(
        ConstraintError::PercentOutOfRange {
            received_percent: 101,
        }
        .to_string(),
        "percent must be between 1 and 100, got 101"
    );
    assert_eq!(
        ConstraintError::ZeroFixedCellCount.to_string(),
        "fixed size must be at least one cell"
    );
    assert_eq!(
        ConstraintError::ZeroMinimumCellCount.to_string(),
        "minimum size must be at least one cell"
    );
    assert_eq!(
        ConstraintError::ZeroPreferredCellCount.to_string(),
        "preferred size must be at least one cell"
    );
}

#[test]
fn constraint_errors_are_recoverable_layout_errors() {
    let constraint_errors = [
        ConstraintError::ZeroFlexWeight,
        ConstraintError::PercentOutOfRange {
            received_percent: 0,
        },
        ConstraintError::ZeroFixedCellCount,
        ConstraintError::ZeroMinimumCellCount,
        ConstraintError::ZeroPreferredCellCount,
    ];
    for constraint_error in constraint_errors {
        assert_eq!(constraint_error.category(), DomainCategory::Layout);
        assert_eq!(constraint_error.get_severity(), Severity::Recoverable);
    }
}

#[test]
fn a_weight_serializes_to_its_exact_json_shape() {
    let weight = SizeWeight {
        primary_constraint: SizeConstraint::Flex(2),
        minimum_cell_count: Some(20),
        preferred_cell_count: None,
        resize_delta: -3,
    };
    assert_eq!(
        serde_json::to_string(&weight).unwrap(),
        r#"{"primary_constraint":{"Flex":2},"minimum_cell_count":20,"preferred_cell_count":null,"resize_delta":-3}"#
    );
}

#[test]
fn every_constraint_kind_serializes_as_a_tagged_object() {
    assert_eq!(
        serde_json::to_string(&SizeConstraint::Flex(3)).unwrap(),
        r#"{"Flex":3}"#
    );
    assert_eq!(
        serde_json::to_string(&SizeConstraint::Percent(40)).unwrap(),
        r#"{"Percent":40}"#
    );
    assert_eq!(
        serde_json::to_string(&SizeConstraint::Fixed(80)).unwrap(),
        r#"{"Fixed":80}"#
    );
    assert_eq!(
        serde_json::to_string(&SizeConstraint::Minimum(10)).unwrap(),
        r#"{"Minimum":10}"#
    );
    assert_eq!(
        serde_json::to_string(&SizeConstraint::Preferred(120)).unwrap(),
        r#"{"Preferred":120}"#
    );
}

#[test]
fn absent_overlays_deserialize_as_none_and_resize_delta_is_required() {
    let weight: SizeWeight =
        serde_json::from_str(r#"{"primary_constraint":{"Flex":1},"resize_delta":0}"#).unwrap();
    assert_eq!(weight, SizeWeight::default());

    let deserialization_error =
        serde_json::from_str::<SizeWeight>(r#"{"primary_constraint":{"Flex":1}}"#).unwrap_err();
    assert_eq!(
        deserialization_error.to_string(),
        "missing field `resize_delta` at line 1 column 33"
    );
}

#[test]
fn deserialization_keeps_out_of_range_values_as_stored() {
    let weight: SizeWeight = serde_json::from_str(
        r#"{"primary_constraint":{"Percent":250},"minimum_cell_count":0,"preferred_cell_count":0,"resize_delta":0}"#,
    )
    .unwrap();
    assert_eq!(
        weight,
        SizeWeight {
            primary_constraint: SizeConstraint::Percent(250),
            minimum_cell_count: Some(0),
            preferred_cell_count: Some(0),
            resize_delta: 0,
        }
    );
}
