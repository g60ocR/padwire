//! Local address discovery, specifically "which of these is the tailnet".
//!
//! The exporter binds to the Tailscale address by default. Port 3240 carries
//! plaintext USB traffic — keystrokes and controller input in the general case
//! — so the default must not be reachable from a café network. Tailscale
//! already provides the encryption and identity that would otherwise have to
//! be built here, but only for traffic that actually goes over it.

use std::ffi::CStr;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub addr: IpAddr,
}

/// CGNAT space, which is where Tailscale hands out IPv4 addresses.
pub const TAILSCALE_V4_NET: (Ipv4Addr, u32) = (Ipv4Addr::new(100, 64, 0, 0), 10);
/// Tailscale's ULA prefix, `fd7a:115c:a1e0::/48`.
pub const TAILSCALE_V6_PREFIX: [u8; 6] = [0xfd, 0x7a, 0x11, 0x5c, 0xa1, 0xe0];

pub fn is_tailscale_v4(a: Ipv4Addr) -> bool {
    let (net, bits) = TAILSCALE_V4_NET;
    let mask = u32::MAX << (32 - bits);
    (u32::from(a) & mask) == (u32::from(net) & mask)
}

pub fn is_tailscale_v6(a: Ipv6Addr) -> bool {
    a.octets()[..6] == TAILSCALE_V6_PREFIX
}

pub fn is_tailscale_addr(a: &IpAddr) -> bool {
    match a {
        IpAddr::V4(v4) => is_tailscale_v4(*v4),
        IpAddr::V6(v6) => is_tailscale_v6(*v6),
    }
}

/// Every configured IPv4/IPv6 address on the host.
pub fn list() -> io::Result<Vec<Interface>> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut out = Vec::new();
    let mut cur = head;
    // Safety: the list is valid until freeifaddrs, and every pointer we follow
    // is null-checked.
    unsafe {
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || ifa.ifa_name.is_null() {
                continue;
            }
            let name = CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
            let family = (*ifa.ifa_addr).sa_family as i32;
            let addr = match family {
                libc::AF_INET => {
                    let sa = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    IpAddr::V4(Ipv4Addr::from(u32::from_be(sa.sin_addr.s_addr)))
                }
                libc::AF_INET6 => {
                    let sa = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    IpAddr::V6(Ipv6Addr::from(sa.sin6_addr.s6_addr))
                }
                _ => continue,
            };
            out.push(Interface { name, addr });
        }
        libc::freeifaddrs(head);
    }
    Ok(out)
}

/// Addresses that belong to the tailnet, preferring IPv4 (MagicDNS hands out
/// both, and the v4 address is the one people paste into config files).
pub fn tailscale_addrs() -> io::Result<Vec<IpAddr>> {
    let mut v: Vec<IpAddr> = list()?
        .into_iter()
        .filter(|i| is_tailscale_addr(&i.addr) || i.name.starts_with("tailscale"))
        .map(|i| i.addr)
        .filter(|a| match a {
            IpAddr::V4(v4) => !v4.is_loopback(),
            // A link-local address needs a scope id to bind, and Tailscale
            // does not assign one anyway.
            IpAddr::V6(v6) => !v6.is_loopback() && (v6.segments()[0] & 0xffc0) != 0xfe80,
        })
        .collect();
    v.sort_by_key(|a| match a {
        IpAddr::V4(_) => 0,
        IpAddr::V6(_) => 1,
    });
    v.dedup();
    Ok(v)
}

/// Routable non-loopback IPv4 addresses, used to answer mDNS `A` queries.
pub fn lan_v4_addrs() -> io::Result<Vec<Ipv4Addr>> {
    Ok(list()?
        .into_iter()
        .filter_map(|i| match i.addr {
            IpAddr::V4(a) if !a.is_loopback() && !a.is_link_local() => Some(a),
            _ => None,
        })
        .collect())
}

pub fn hostname() -> String {
    let mut buf = [0 as libc::c_char; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len() - 1) };
    if rc != 0 {
        return "usbfwd".into();
    }
    // Safety: gethostname NUL-terminates within the buffer we sized above.
    let s = unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    // mDNS wants the short name, not a search-domain-qualified one.
    s.split('.').next().unwrap_or("usbfwd").to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_tailscale_v4_range() {
        assert!(is_tailscale_v4("100.64.0.1".parse().unwrap()));
        assert!(is_tailscale_v4("100.101.102.103".parse().unwrap()));
        assert!(is_tailscale_v4("100.127.255.254".parse().unwrap()));
        assert!(!is_tailscale_v4("100.63.255.255".parse().unwrap()));
        assert!(!is_tailscale_v4("100.128.0.1".parse().unwrap()));
        assert!(!is_tailscale_v4("192.168.1.10".parse().unwrap()));
        assert!(!is_tailscale_v4("10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn recognises_the_tailscale_v6_prefix() {
        assert!(is_tailscale_v6("fd7a:115c:a1e0::1".parse().unwrap()));
        assert!(is_tailscale_v6(
            "fd7a:115c:a1e0:ab12:4843:cd96:6265:1234".parse().unwrap()
        ));
        assert!(!is_tailscale_v6("fd00::1".parse().unwrap()));
        assert!(!is_tailscale_v6("2001:db8::1".parse().unwrap()));
    }

    #[test]
    fn enumerating_interfaces_finds_loopback() {
        let ifs = list().expect("getifaddrs");
        assert!(
            ifs.iter().any(|i| i.addr.is_loopback()),
            "no loopback address among {ifs:?}"
        );
    }

    #[test]
    fn hostname_is_short_and_nonempty() {
        let h = hostname();
        assert!(!h.is_empty());
        assert!(!h.contains('.'));
    }
}
