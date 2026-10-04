//! The network addresses of this machine that another machine can connect
//! to.
//!
//! [`list_host_addresses`] reads every address of every interface that is up
//! and is not loopback, leaves out the link-local ones, and puts the address
//! the system sends to the internet from first.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

#[cfg(test)]
mod tests;

/// An IPv4 address in the documentation range of RFC 5737. No packet goes to
/// it: [`find_route_address`] only asks the system which address it would
/// send from.
const IPV4_ROUTE_PROBE_ADDRESS: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 9);

/// An IPv6 address in the documentation range of RFC 3849, used the same way
/// as [`IPV4_ROUTE_PROBE_ADDRESS`].
const IPV6_ROUTE_PROBE_ADDRESS: SocketAddr = SocketAddr::new(
    IpAddr::V6(Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1)),
    9,
);

/// One address of this machine, and the interface that carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAddress {
    /// The address, such as `192.168.1.20` or `2001:db8::20`.
    pub ip_address: IpAddr,
    /// The name of the interface, such as `en0`, `eth0` or `Wi-Fi`.
    pub interface_name: String,
}

/// Every address of this machine that another machine can connect to.
///
/// Each address of an interface that is up and is not a loopback interface
/// is kept when [`is_reachable_from_another_machine`] accepts it. The
/// addresses the system sends from toward `192.0.2.1` and toward
/// `2001:db8::1` come first, and the rest keep the order the system lists
/// them in. Nothing is sent to either address. Empty when the system does not
/// list the addresses.
///
/// Example: `en0` with `192.168.1.20` and `fe80::1`, `utun3` with
/// `100.64.0.2`, and a default route through `en0` give `192.168.1.20` on
/// `en0`, then `100.64.0.2` on `utun3`.
#[must_use]
pub fn list_host_addresses() -> Vec<HostAddress> {
    let host_addresses: Vec<HostAddress> = platform::list_interface_addresses()
        .into_iter()
        .filter(|host_address| is_reachable_from_another_machine(host_address.ip_address))
        .collect();
    let route_addresses: Vec<IpAddr> = [IPV4_ROUTE_PROBE_ADDRESS, IPV6_ROUTE_PROBE_ADDRESS]
        .into_iter()
        .filter_map(find_route_address)
        .collect();
    order_route_addresses_first(host_addresses, &route_addresses)
}

/// Whether another machine can connect to `ip_address`: it is not
/// unspecified (`0.0.0.0`, `::`), not loopback (`127.0.0.0/8`, `::1`), and
/// not link-local (`169.254.0.0/16`, `fe80::/10`).
#[must_use]
pub fn is_reachable_from_another_machine(ip_address: IpAddr) -> bool {
    match ip_address {
        IpAddr::V4(ipv4_address) => {
            !ipv4_address.is_unspecified()
                && !ipv4_address.is_loopback()
                && !ipv4_address.is_link_local()
        }
        IpAddr::V6(ipv6_address) => {
            !ipv6_address.is_unspecified()
                && !ipv6_address.is_loopback()
                && !ipv6_address.is_unicast_link_local()
        }
    }
}

/// `host_addresses` with every address in `route_addresses` moved to the
/// front. Both groups keep their order.
fn order_route_addresses_first(
    host_addresses: Vec<HostAddress>,
    route_addresses: &[IpAddr],
) -> Vec<HostAddress> {
    let (mut ordered_addresses, other_addresses): (Vec<HostAddress>, Vec<HostAddress>) =
        host_addresses
            .into_iter()
            .partition(|host_address| route_addresses.contains(&host_address.ip_address));
    ordered_addresses.extend(other_addresses);
    ordered_addresses
}

/// The address of this machine the system sends from toward
/// `probe_address`, read from a UDP socket connected to it. Connecting a UDP
/// socket sends nothing. `None` when the system has no route there.
fn find_route_address(probe_address: SocketAddr) -> Option<IpAddr> {
    let unspecified_address = match probe_address {
        SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
    };
    let probe_socket = UdpSocket::bind(unspecified_address).ok()?;
    probe_socket.connect(probe_address).ok()?;
    let route_address = probe_socket.local_addr().ok()?.ip();
    (!route_address.is_unspecified()).then_some(route_address)
}

#[cfg(unix)]
mod platform {
    use std::net::IpAddr;

    use nix::ifaddrs::getifaddrs;
    use nix::net::if_::InterfaceFlags;

    use super::HostAddress;

    /// Every IPv4 and IPv6 address `getifaddrs` gives for an interface that
    /// carries `IFF_UP` and not `IFF_LOOPBACK`.
    pub(super) fn list_interface_addresses() -> Vec<HostAddress> {
        let Ok(interface_addresses) = getifaddrs() else {
            return Vec::new();
        };
        interface_addresses
            .filter(|interface_address| {
                interface_address.flags.contains(InterfaceFlags::IFF_UP)
                    && !interface_address
                        .flags
                        .contains(InterfaceFlags::IFF_LOOPBACK)
            })
            .filter_map(|interface_address| {
                let socket_address = interface_address.address?;
                let ip_address = match (
                    socket_address.as_sockaddr_in(),
                    socket_address.as_sockaddr_in6(),
                ) {
                    (Some(ipv4_socket_address), _) => IpAddr::V4(ipv4_socket_address.ip()),
                    (None, Some(ipv6_socket_address)) => IpAddr::V6(ipv6_socket_address.ip()),
                    (None, None) => return None,
                };
                Some(HostAddress {
                    ip_address,
                    interface_name: interface_address.interface_name,
                })
            })
            .collect()
    }
}

#[cfg(windows)]
mod platform {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, IF_TYPE_SOFTWARE_LOOPBACK, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6,
    };

    use super::HostAddress;

    /// The byte count of the first buffer `GetAdaptersAddresses` is given.
    const FIRST_ADAPTER_BUFFER_BYTE_COUNT: u32 = 16 * 1024;

    /// How many times `GetAdaptersAddresses` is called at most: the first call,
    /// and up to 3 more, each with a buffer grown to the size the call before
    /// named.
    const MAX_ADAPTER_READ_ATTEMPT_COUNT: usize = 4;

    /// Every unicast IPv4 and IPv6 address `GetAdaptersAddresses` gives for an
    /// adapter whose state is up and whose type is not software loopback,
    /// named by the adapter's friendly name.
    pub(super) fn list_interface_addresses() -> Vec<HostAddress> {
        let Some(adapter_buffer) = read_adapter_buffer() else {
            return Vec::new();
        };
        let mut host_addresses = Vec::new();
        let mut adapter_pointer = adapter_buffer.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        while !adapter_pointer.is_null() {
            // SAFETY: `adapter_pointer` is the first adapter in
            // `adapter_buffer` or the `Next` of one, and `adapter_buffer`
            // lives until this function returns.
            let adapter = unsafe { &*adapter_pointer };
            adapter_pointer = adapter.Next;
            if adapter.OperStatus != IfOperStatusUp || adapter.IfType == IF_TYPE_SOFTWARE_LOOPBACK {
                continue;
            }
            // SAFETY: `FriendlyName` is a 0-ended UTF-16 string inside
            // `adapter_buffer`.
            let interface_name = unsafe { decode_wide_text_pointer(adapter.FriendlyName) };
            let mut unicast_pointer = adapter.FirstUnicastAddress;
            while !unicast_pointer.is_null() {
                // SAFETY: `unicast_pointer` is the adapter's first unicast
                // address or the `Next` of one, inside `adapter_buffer`.
                let unicast_address = unsafe { &*unicast_pointer };
                unicast_pointer = unicast_address.Next;
                // SAFETY: `lpSockaddr` points at a socket address of
                // `iSockaddrLength` bytes inside `adapter_buffer`.
                if let Some(ip_address) = unsafe {
                    decode_socket_address(
                        unicast_address.Address.lpSockaddr,
                        unicast_address.Address.iSockaddrLength,
                    )
                } {
                    host_addresses.push(HostAddress {
                        ip_address,
                        interface_name: interface_name.clone(),
                    });
                }
            }
        }
        host_addresses
    }

    /// The adapter list `GetAdaptersAddresses` writes, in a buffer aligned for
    /// `IP_ADAPTER_ADDRESSES_LH`. A buffer that was too small is grown to the
    /// size the call names, up to [`MAX_ADAPTER_READ_ATTEMPT_COUNT`] calls.
    /// `None` when the call fails.
    fn read_adapter_buffer() -> Option<Vec<u64>> {
        let mut buffer_byte_count = FIRST_ADAPTER_BUFFER_BYTE_COUNT;
        for _ in 0..MAX_ADAPTER_READ_ATTEMPT_COUNT {
            let mut adapter_buffer: Vec<u64> =
                vec![0; (buffer_byte_count as usize).div_ceil(std::mem::size_of::<u64>())];
            // SAFETY: `adapter_buffer` holds at least `buffer_byte_count`
            // bytes and lives for the call.
            let read_answer = unsafe {
                GetAdaptersAddresses(
                    u32::from(AF_UNSPEC),
                    GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER,
                    std::ptr::null(),
                    adapter_buffer.as_mut_ptr().cast(),
                    &mut buffer_byte_count,
                )
            };
            match read_answer {
                ERROR_SUCCESS => return Some(adapter_buffer),
                ERROR_BUFFER_OVERFLOW => continue,
                _ => return None,
            }
        }
        None
    }

    /// The IP address of the socket address at `socket_address_pointer`, of
    /// `socket_address_byte_count` bytes. `None` for a null pointer, for a
    /// family other than `AF_INET` and `AF_INET6`, and for a length too short
    /// for that family.
    ///
    /// # Safety
    /// `socket_address_pointer` is null or points at
    /// `socket_address_byte_count` readable bytes.
    unsafe fn decode_socket_address(
        socket_address_pointer: *const SOCKADDR,
        socket_address_byte_count: i32,
    ) -> Option<IpAddr> {
        if socket_address_pointer.is_null() {
            return None;
        }
        let socket_address_byte_count = usize::try_from(socket_address_byte_count).ok()?;
        // SAFETY: the caller promises the pointer reads; every family starts
        // with the `sa_family` field.
        let address_family = unsafe { (*socket_address_pointer).sa_family };
        if address_family == AF_INET
            && socket_address_byte_count >= std::mem::size_of::<SOCKADDR_IN>()
        {
            // SAFETY: the family is `AF_INET` and the length covers
            // `SOCKADDR_IN`.
            let ipv4_socket_address = unsafe { &*socket_address_pointer.cast::<SOCKADDR_IN>() };
            // SAFETY: every field of the `IN_ADDR` union is plain bytes.
            let ipv4_bytes = unsafe { ipv4_socket_address.sin_addr.S_un.S_addr }.to_ne_bytes();
            return Some(IpAddr::V4(Ipv4Addr::from(ipv4_bytes)));
        }
        if address_family == AF_INET6
            && socket_address_byte_count >= std::mem::size_of::<SOCKADDR_IN6>()
        {
            // SAFETY: the family is `AF_INET6` and the length covers
            // `SOCKADDR_IN6`.
            let ipv6_socket_address = unsafe { &*socket_address_pointer.cast::<SOCKADDR_IN6>() };
            // SAFETY: every field of the `IN6_ADDR` union is plain bytes.
            let ipv6_bytes = unsafe { ipv6_socket_address.sin6_addr.u.Byte };
            return Some(IpAddr::V6(Ipv6Addr::from(ipv6_bytes)));
        }
        None
    }

    /// The UTF-16 text at `wide_text_pointer` up to its first 0. Empty for a
    /// null pointer.
    ///
    /// # Safety
    /// `wide_text_pointer` is null or points at a 0-ended UTF-16 string.
    unsafe fn decode_wide_text_pointer(wide_text_pointer: *const u16) -> String {
        if wide_text_pointer.is_null() {
            return String::new();
        }
        let mut text_length = 0;
        // SAFETY: the caller promises a 0 ends the string.
        while unsafe { *wide_text_pointer.add(text_length) } != 0 {
            text_length += 1;
        }
        // SAFETY: the `text_length` code units before the 0 are readable.
        String::from_utf16_lossy(unsafe {
            std::slice::from_raw_parts(wide_text_pointer, text_length)
        })
    }
}
