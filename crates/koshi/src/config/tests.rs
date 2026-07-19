//! Tests for config file loading — profile-name validation.

use super::is_plain_profile_name;

#[test]
fn a_plain_profile_name_is_accepted() {
    assert!(is_plain_profile_name("dev"));
    assert!(is_plain_profile_name("work.2"));
    assert!(is_plain_profile_name("my-profile"));
}

#[test]
fn a_path_traversing_or_absolute_profile_name_is_rejected() {
    // Each of these would join to a `.kdl` outside `profile/`.
    assert!(!is_plain_profile_name("../secret"));
    assert!(!is_plain_profile_name("a/b"));
    assert!(!is_plain_profile_name("/etc/passwd"));
    assert!(!is_plain_profile_name(".."));
    assert!(!is_plain_profile_name("."));
    assert!(!is_plain_profile_name(""));
}
