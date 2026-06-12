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
