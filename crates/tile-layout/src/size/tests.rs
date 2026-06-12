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
