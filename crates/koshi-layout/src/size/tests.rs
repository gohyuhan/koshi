//! Tests for size constraints and weights.

use super::*;

fn roundtrip(weight: &SizeWeight) {
    let json = serde_json::to_string(weight).expect("serialize");
    let back: SizeWeight = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(*weight, back);
}

#[test]
fn default_weight_is_one_flex_share() {
    let weight = SizeWeight::default();
    assert_eq!(weight.primary, SizeConstraint::Flex(1));
    assert_eq!(weight.min, None);
    assert_eq!(weight.preferred, None);
    assert_eq!(weight.resize_delta, 0);
}

#[test]
fn every_constraint_kind_roundtrips() {
    let kinds = [
        SizeConstraint::Flex(3),
        SizeConstraint::Percent(40),
        SizeConstraint::Fixed(80),
        SizeConstraint::Min(10),
        SizeConstraint::Preferred(120),
    ];
    for primary in kinds {
        roundtrip(&SizeWeight {
            primary,
            min: None,
            preferred: None,
            resize_delta: 0,
        });
    }
}

#[test]
fn combined_flex_with_overlays_roundtrips() {
    roundtrip(&SizeWeight {
        primary: SizeConstraint::Flex(2),
        min: Some(20),
        preferred: Some(50),
        resize_delta: -3,
    });
}

#[test]
fn constructors_accept_valid_values() {
    assert_eq!(SizeConstraint::flex(1), Ok(SizeConstraint::Flex(1)));
    assert_eq!(SizeConstraint::percent(1), Ok(SizeConstraint::Percent(1)));
    assert_eq!(
        SizeConstraint::percent(100),
        Ok(SizeConstraint::Percent(100))
    );
    assert_eq!(SizeConstraint::fixed(80), Ok(SizeConstraint::Fixed(80)));
    assert_eq!(SizeConstraint::min(2), Ok(SizeConstraint::Min(2)));
    assert_eq!(
        SizeConstraint::preferred(120),
        Ok(SizeConstraint::Preferred(120))
    );
}

#[test]
fn constructors_reject_invalid_values() {
    assert_eq!(
        SizeConstraint::flex(0),
        Err(ConstraintError::ZeroFlexWeight)
    );
    assert_eq!(
        SizeConstraint::percent(0),
        Err(ConstraintError::PercentOutOfRange { got: 0 })
    );
    assert_eq!(
        SizeConstraint::percent(101),
        Err(ConstraintError::PercentOutOfRange { got: 101 })
    );
    assert_eq!(SizeConstraint::fixed(0), Err(ConstraintError::ZeroFixed));
    assert_eq!(SizeConstraint::min(0), Err(ConstraintError::ZeroMin));
    assert_eq!(
        SizeConstraint::preferred(0),
        Err(ConstraintError::ZeroPreferred)
    );
}

#[test]
fn weight_overlays_validate_and_compose() {
    let weight = SizeWeight::new(SizeConstraint::Flex(2))
        .with_min(20)
        .unwrap()
        .with_preferred(50)
        .unwrap();
    assert_eq!(weight.primary, SizeConstraint::Flex(2));
    assert_eq!(weight.min, Some(20));
    assert_eq!(weight.preferred, Some(50));
    assert_eq!(weight.resize_delta, 0);

    let base = SizeWeight::new(SizeConstraint::Flex(1));
    assert_eq!(base.with_min(0), Err(ConstraintError::ZeroMin));
    assert_eq!(base.with_preferred(0), Err(ConstraintError::ZeroPreferred));
}

#[test]
fn constructors_accept_their_maximum_values() {
    assert_eq!(
        SizeConstraint::flex(u32::MAX),
        Ok(SizeConstraint::Flex(u32::MAX))
    );
    assert_eq!(
        SizeConstraint::fixed(u16::MAX),
        Ok(SizeConstraint::Fixed(u16::MAX))
    );
    assert_eq!(
        SizeConstraint::min(u16::MAX),
        Ok(SizeConstraint::Min(u16::MAX))
    );
    assert_eq!(
        SizeConstraint::preferred(u16::MAX),
        Ok(SizeConstraint::Preferred(u16::MAX))
    );
}

#[test]
fn percent_rejects_its_type_maximum() {
    assert_eq!(
        SizeConstraint::percent(u8::MAX),
        Err(ConstraintError::PercentOutOfRange { got: u8::MAX })
    );
}

#[test]
fn an_overlay_applies_to_any_primary_and_the_last_call_wins() {
    let weight = SizeWeight::new(SizeConstraint::Percent(40))
        .with_min(3)
        .unwrap()
        .with_min(9)
        .unwrap()
        .with_preferred(5)
        .unwrap()
        .with_preferred(6)
        .unwrap();
    assert_eq!(
        weight,
        SizeWeight {
            primary: SizeConstraint::Percent(40),
            min: Some(9),
            preferred: Some(6),
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
        ConstraintError::PercentOutOfRange { got: 101 }.to_string(),
        "percent must be between 1 and 100, got 101"
    );
    assert_eq!(
        ConstraintError::ZeroFixed.to_string(),
        "fixed size must be at least one cell"
    );
    assert_eq!(
        ConstraintError::ZeroMin.to_string(),
        "minimum size must be at least one cell"
    );
    assert_eq!(
        ConstraintError::ZeroPreferred.to_string(),
        "preferred size must be at least one cell"
    );
}

#[test]
fn constraint_errors_are_recoverable_layout_errors() {
    let errors = [
        ConstraintError::ZeroFlexWeight,
        ConstraintError::PercentOutOfRange { got: 0 },
        ConstraintError::ZeroFixed,
        ConstraintError::ZeroMin,
        ConstraintError::ZeroPreferred,
    ];
    for error in errors {
        assert_eq!(error.category(), DomainCategory::Layout);
        assert_eq!(error.severity(), Severity::Recoverable);
    }
}

#[test]
fn a_weight_serializes_to_its_exact_json_shape() {
    let weight = SizeWeight {
        primary: SizeConstraint::Flex(2),
        min: Some(20),
        preferred: None,
        resize_delta: -3,
    };
    assert_eq!(
        serde_json::to_string(&weight).unwrap(),
        r#"{"primary":{"Flex":2},"min":20,"preferred":null,"resize_delta":-3}"#
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
        serde_json::to_string(&SizeConstraint::Min(10)).unwrap(),
        r#"{"Min":10}"#
    );
    assert_eq!(
        serde_json::to_string(&SizeConstraint::Preferred(120)).unwrap(),
        r#"{"Preferred":120}"#
    );
}

#[test]
fn absent_overlays_deserialize_as_none_and_resize_delta_is_required() {
    let weight: SizeWeight =
        serde_json::from_str(r#"{"primary":{"Flex":1},"resize_delta":0}"#).unwrap();
    assert_eq!(weight, SizeWeight::default());

    let err = serde_json::from_str::<SizeWeight>(r#"{"primary":{"Flex":1}}"#).unwrap_err();
    assert_eq!(
        err.to_string(),
        "missing field `resize_delta` at line 1 column 22"
    );
}

#[test]
fn deserialization_keeps_out_of_range_values_as_stored() {
    let weight: SizeWeight = serde_json::from_str(
        r#"{"primary":{"Percent":250},"min":0,"preferred":0,"resize_delta":0}"#,
    )
    .unwrap();
    assert_eq!(
        weight,
        SizeWeight {
            primary: SizeConstraint::Percent(250),
            min: Some(0),
            preferred: Some(0),
            resize_delta: 0,
        }
    );
}
