use tile_core::ids::SessionId;
use tile_pane::registry::PaneRegistry;

use super::state::Session;
use crate::client::ClientRegistry;

#[test]
fn a_new_session_starts_empty() {
    let id = SessionId::new();
    let session = Session::new(id, "main".to_owned());

    assert_eq!(session.id, id);
    assert_eq!(session.name, "main");
    assert!(session.tabs.is_empty());
    assert!(session.plugin_runtime_ref.is_none());
    // The registries are part of the public shape, reachable as fields.
    let _: &PaneRegistry = &session.panes;
    let _: &ClientRegistry = &session.clients;
}
