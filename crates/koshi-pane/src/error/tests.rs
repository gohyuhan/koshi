//! Tests for the pane error types: their `Display` text and value equality.

use super::*;

use koshi_core::ids::PaneId;

#[test]
fn a_duplicate_pane_id_error_names_the_pane_in_its_message() {
    let pane_id = PaneId::new();
    let pane_registry_error = PaneRegistryError::DuplicateId { pane_id };

    assert_eq!(
        pane_registry_error.to_string(),
        format!("pane-{} is already registered", pane_id.get_uuid())
    );
}

#[test]
fn two_duplicate_pane_id_errors_are_equal_only_when_the_id_matches() {
    let pane_id = PaneId::new();
    let duplicate_pane_id_error = PaneRegistryError::DuplicateId { pane_id };

    assert_eq!(
        duplicate_pane_id_error,
        PaneRegistryError::DuplicateId { pane_id }
    );
    assert_ne!(
        duplicate_pane_id_error,
        PaneRegistryError::DuplicateId {
            pane_id: PaneId::new(),
        }
    );
}
