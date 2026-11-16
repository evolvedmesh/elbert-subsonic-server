//! Which peers the host answers: the local network only, since the API sends a
//! password over plain HTTP.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Whether `ip` is on a network a LAN listener can be on: loopback, private
/// (RFC 1918), link-local, unique local IPv6, and IPv4-mapped forms of those.
/// Everything else — carrier-grade NAT's shared space included — is refused.
pub fn is_local_peer(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_local_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_local_v4(v4);
            }
            v6.is_loopback() || is_ula(v6) || is_link_local_v6(v6)
        }
    }
}

/// Whether `ip` is a link-local address.
pub fn is_link_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => is_link_local_v6(v6),
    }
}

fn is_local_v4(ip: Ipv4Addr) -> bool {
    ip.is_loopback() || ip.is_private() || ip.is_link_local()
}

fn is_ula(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
}

fn is_link_local_v6(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_peers() {
        for ok in [
            "127.0.0.1",
            "192.168.1.20",
            "10.0.0.5",
            "172.16.3.4",
            "169.254.1.1",
            "::1",
            "fd12::1",
            "fe80::1",
            "::ffff:192.168.0.2",
        ] {
            assert!(is_local_peer(ok.parse().unwrap()), "{ok}");
        }
        for no in [
            "8.8.8.8",
            "100.64.0.1",
            "2001:db8::1",
            "::ffff:1.1.1.1",
            "172.32.0.1",
        ] {
            assert!(!is_local_peer(no.parse().unwrap()), "{no}");
        }
    }
}
