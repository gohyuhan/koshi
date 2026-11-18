//! Tests for `build_presence_notice`: the text of each change for each
//! origin, and the host-account rule, which counts no client whose arrival
//! never posted.

use std::time::SystemTime;

use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, SessionId, TabId};
use koshi_session::client::ArrivalNotice;

use super::*;

/// A client record of origin `client_origin`, labelled `label`, holding
/// `color_index`.
fn build_client(client_origin: ClientOrigin, label: &str, color_index: u32) -> Client {
    Client::from_attachment(
        ClientId::new(),
        SessionId::new(),
        SystemTime::UNIX_EPOCH,
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        TabId::new(),
        client_origin,
        label.to_string(),
        color_index,
    )
}

const HOST_ACCOUNT: ClientOrigin = ClientOrigin::Local { is_same_user: true };
const OTHER_ACCOUNT: ClientOrigin = ClientOrigin::Local {
    is_same_user: false,
};

#[test]
fn a_remote_client_gets_each_notice_with_its_label_and_number() {
    let bob = build_client(ClientOrigin::Remote, "bob", 1);

    let notices: Vec<Option<String>> = [
        PresenceChange::Joined,
        PresenceChange::Reconnected,
        PresenceChange::Left,
        PresenceChange::Disconnected,
    ]
    .into_iter()
    .map(|presence_change| build_presence_notice(&bob, [], presence_change))
    .collect();

    assert_eq!(
        notices,
        vec![
            Some("bob (2) joined remotely".to_string()),
            Some("bob (2) reconnected".to_string()),
            Some("bob (2) left".to_string()),
            Some("bob (2) disconnected".to_string()),
        ]
    );
}

#[test]
fn another_account_joins_from_another_account_on_this_machine_with_nobody_present() {
    let otter = build_client(OTHER_ACCOUNT, "C-swift-otter", 2);

    assert_eq!(
        build_presence_notice(&otter, [], PresenceChange::Joined),
        Some("C-swift-otter (3) joined from another account on this machine".to_string())
    );
    assert_eq!(
        build_presence_notice(&otter, [], PresenceChange::Left),
        Some("C-swift-otter (3) left".to_string())
    );
}

#[test]
fn a_host_terminal_gets_a_notice_only_as_the_only_host_terminal_beside_another_person() {
    let host = build_client(HOST_ACCOUNT, "host", 0);
    let second_host = build_client(HOST_ACCOUNT, "host-2", 3);
    let bob = build_client(ClientOrigin::Remote, "bob", 1);
    let otter = build_client(OTHER_ACCOUNT, "C-swift-otter", 2);

    let cases: [(&str, Vec<&Client>, Option<&str>); 6] = [
        ("bob present", vec![&bob], Some("host (1) joined")),
        (
            "another account present",
            vec![&otter],
            Some("host (1) joined"),
        ),
        (
            "itself and bob present",
            vec![&host, &bob],
            Some("host (1) joined"),
        ),
        (
            "bob and a second host terminal present",
            vec![&bob, &second_host],
            None,
        ),
        ("only itself present", vec![&host], None),
        ("nobody present", vec![], None),
    ];
    for (case_name, present_clients, expected_notice) in cases {
        assert_eq!(
            build_presence_notice(&host, present_clients, PresenceChange::Joined).as_deref(),
            expected_notice,
            "{case_name}"
        );
    }
    assert_eq!(
        build_presence_notice(&host, [&bob], PresenceChange::Left),
        Some("host (1) left".to_string())
    );
    assert_eq!(
        build_presence_notice(&host, [&bob], PresenceChange::Disconnected),
        Some("host (1) disconnected".to_string())
    );
}

#[test]
fn a_remote_or_other_account_notice_ignores_who_is_present() {
    let bob = build_client(ClientOrigin::Remote, "bob", 1);
    let otter = build_client(OTHER_ACCOUNT, "C-swift-otter", 2);
    let host = build_client(HOST_ACCOUNT, "host", 0);

    assert_eq!(
        build_presence_notice(&bob, [&host], PresenceChange::Left),
        Some("bob (2) left".to_string())
    );
    assert_eq!(
        build_presence_notice(&otter, [&host, &bob], PresenceChange::Disconnected),
        Some("C-swift-otter (3) disconnected".to_string())
    );
}

#[test]
fn a_host_terminal_counts_no_present_client_whose_arrival_never_posted() {
    let host = build_client(HOST_ACCOUNT, "host", 0);
    let bob = build_client(ClientOrigin::Remote, "bob", 1);
    let mut unannounced_carol = build_client(ClientOrigin::Remote, "carol", 4);
    unannounced_carol.set_unannounced_arrival(Some(ArrivalNotice::Joined));
    let mut unannounced_second_host = build_client(HOST_ACCOUNT, "host-2", 3);
    unannounced_second_host.set_unannounced_arrival(Some(ArrivalNotice::Reconnected));

    assert_eq!(
        build_presence_notice(&host, [&unannounced_carol], PresenceChange::Left),
        None,
        "the only other person never arrived"
    );
    assert_eq!(
        build_presence_notice(
            &host,
            [&bob, &unannounced_second_host],
            PresenceChange::Left
        ),
        Some("host (1) left".to_string()),
        "the second host terminal never arrived"
    );
}
