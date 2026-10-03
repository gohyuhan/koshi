//! Tests for which addresses of this machine another machine can connect to.

use super::*;

/// A [`HostAddress`] on `interface_name` for the address `ip_address_text`.
fn build_host_address(ip_address_text: &str, interface_name: &str) -> HostAddress {
    HostAddress {
        ip_address: ip_address_text.parse().expect("an address literal"),
        interface_name: interface_name.to_string(),
    }
}

#[test]
fn is_reachable_from_another_machine_leaves_out_unspecified_loopback_and_link_local() {
    let address_checks = [
        ("192.168.1.20", true),
        ("10.0.0.5", true),
        ("100.64.0.2", true),
        ("2001:db8::20", true),
        ("fd00::20", true),
        ("0.0.0.0", false),
        ("127.0.0.1", false),
        ("127.1.2.3", false),
        ("169.254.10.20", false),
        ("::", false),
        ("::1", false),
        ("fe80::1", false),
        ("febf::1", false),
    ];
    for (ip_address_text, is_expected_reachable) in address_checks {
        assert_eq!(
            is_reachable_from_another_machine(ip_address_text.parse().expect("an address literal")),
            is_expected_reachable,
            "{ip_address_text}"
        );
    }
}

#[test]
fn order_route_addresses_first_moves_each_route_address_ahead_and_keeps_both_orders() {
    let host_addresses = vec![
        build_host_address("172.17.0.1", "docker0"),
        build_host_address("192.168.1.20", "en0"),
        build_host_address("100.64.0.2", "utun3"),
        build_host_address("2001:db8::20", "en0"),
    ];
    let route_addresses: Vec<IpAddr> = vec![
        "2001:db8::20".parse().expect("an address literal"),
        "192.168.1.20".parse().expect("an address literal"),
    ];

    assert_eq!(
        order_route_addresses_first(host_addresses, &route_addresses),
        vec![
            build_host_address("192.168.1.20", "en0"),
            build_host_address("2001:db8::20", "en0"),
            build_host_address("172.17.0.1", "docker0"),
            build_host_address("100.64.0.2", "utun3"),
        ]
    );
}

#[test]
fn order_route_addresses_first_keeps_the_order_when_no_route_address_is_listed() {
    let host_addresses = vec![
        build_host_address("172.17.0.1", "docker0"),
        build_host_address("192.168.1.20", "en0"),
    ];

    assert_eq!(
        order_route_addresses_first(host_addresses.clone(), &[]),
        host_addresses
    );
}

#[test]
fn list_host_addresses_gives_only_addresses_another_machine_can_reach() {
    for host_address in list_host_addresses() {
        assert!(
            is_reachable_from_another_machine(host_address.ip_address),
            "{host_address:?}"
        );
        assert!(!host_address.interface_name.is_empty(), "{host_address:?}");
    }
}

#[test]
fn find_route_address_gives_an_address_of_this_machine_or_none() {
    let host_ip_addresses: Vec<IpAddr> = platform::list_interface_addresses()
        .into_iter()
        .map(|host_address| host_address.ip_address)
        .collect();

    for probe_address in [IPV4_ROUTE_PROBE_ADDRESS, IPV6_ROUTE_PROBE_ADDRESS] {
        if let Some(route_address) = find_route_address(probe_address) {
            assert!(
                host_ip_addresses.contains(&route_address),
                "{route_address} is not among {host_ip_addresses:?}"
            );
        }
    }
}
