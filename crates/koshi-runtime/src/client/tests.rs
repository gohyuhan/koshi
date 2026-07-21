//! Tests for the client half: construction, viewport updates, and draining
//! the subscribed event feed.

use std::sync::mpsc;

use koshi_core::event::{LayoutChanged, TabCreated};
use koshi_core::ids::TabId;
use koshi_observability::cleanup::TerminalCleanupGuard;

use super::*;

fn new_client() -> (Client, mpsc::SyncSender<Event>) {
    let (tx, rx) = mpsc::sync_channel(8);
    let client = Client::new(
        ClientId::new(),
        Size { cols: 80, rows: 24 },
        rx,
        TerminalCleanupGuard::new(),
    );
    (client, tx)
}

#[test]
fn a_new_client_holds_its_id_viewport_and_guard() {
    let (client, _tx) = new_client();
    assert_eq!(client.viewport(), Size { cols: 80, rows: 24 });
    let _ = client.cleanup_guard();
}

#[test]
fn set_viewport_records_the_new_size() {
    let (mut client, _tx) = new_client();
    client.set_viewport(Size {
        cols: 120,
        rows: 40,
    });
    assert_eq!(
        client.viewport(),
        Size {
            cols: 120,
            rows: 40,
        }
    );
}

#[test]
fn drain_events_takes_everything_delivered_in_order() {
    let (mut client, tx) = new_client();
    let tab = TabId::new();
    tx.send(Event::TabCreated(TabCreated { tab_id: tab }))
        .expect("send into the subscription");
    tx.send(Event::LayoutChanged(LayoutChanged { tab_id: tab }))
        .expect("send into the subscription");

    assert_eq!(
        client.drain_events(),
        vec![
            Event::TabCreated(TabCreated { tab_id: tab }),
            Event::LayoutChanged(LayoutChanged { tab_id: tab }),
        ]
    );
    assert_eq!(client.drain_events(), Vec::new());
}
