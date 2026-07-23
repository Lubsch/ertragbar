//! Network state from sysfs + /proc, woken by rtnetlink events.
//!
//! As with battery, the netlink socket is only a wakeup: we subscribe to link
//! and address multicast groups, drain without parsing, and recompute state by
//! reading sysfs. Avoids all nlmsghdr/rtattr handling.
//!
//! "Connected" for wifi means: carrier up AND the interface has a routable
//! (non-link-local) address. Carrier alone goes up during association, before
//! DHCP, which would show a connected icon while nothing actually works.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Network {
    Ethernet,
    WifiConnected,
    WifiDisconnected,
}

const NET: &str = "/sys/class/net";

fn read_trimmed(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned())
}

/// An interface is wireless if it has a `wireless/` directory (or a phy80211
/// symlink, which iwd-managed devices have).
fn is_wireless(dir: &Path) -> bool {
    dir.join("wireless").exists() || dir.join("phy80211").exists()
}

/// carrier is 1 when the link has a physical/associated connection. Reading it
/// on a down interface returns EINVAL, hence the Option.
fn has_carrier(dir: &Path) -> bool {
    read_trimmed(&dir.join("carrier")).as_deref() == Some("1")
}

/// Whether `iface` has a routable address: any non-loopback, non-link-local
/// address. We read /proc rather than opening a netlink query, keeping this
/// dependency-free and simple.
fn has_routable_addr(iface: &str) -> bool {
    // IPv4: /proc/net/route lists per-interface routes. A default route or any
    // non-link-local route on this iface means DHCP/config completed.
    if let Ok(routes) = std::fs::read_to_string("/proc/net/route") {
        for line in routes.lines().skip(1) {
            let mut f = line.split_whitespace();
            let (Some(dev), Some(dest)) = (f.next(), f.next()) else {
                continue;
            };
            if dev != iface {
                continue;
            }
            // dest is hex, little-endian. "00000000" is the default route.
            // 169.254.0.0/16 (link-local) is 0000FEA9 in this representation.
            if dest.eq_ignore_ascii_case("00000000") {
                return true;
            }
            if let Ok(d) = u32::from_str_radix(dest, 16) {
                let octets = d.to_le_bytes(); // dest is stored LE
                if octets[0] == 169 && octets[1] == 254 {
                    continue; // link-local, not routable
                }
                if d != 0 {
                    return true;
                }
            }
        }
    }

    // IPv6: /proc/net/if_inet6 columns are addr, ifindex, prefixlen, scope,
    // flags, name. scope 0x00 is global.
    if let Ok(v6) = std::fs::read_to_string("/proc/net/if_inet6") {
        for line in v6.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 6 || f[5] != iface {
                continue;
            }
            if f[3].eq_ignore_ascii_case("00") {
                return true; // global scope
            }
        }
    }
    false
}

/// Determine current network state.
///
/// Ethernet wins when it has carrier: if you're on a cable, that's what the bar
/// should show regardless of wifi. Otherwise report wifi, connected only if it
/// both has carrier and a routable address.
pub fn read() -> Network {
    let Ok(entries) = std::fs::read_dir(NET) else {
        return Network::WifiDisconnected;
    };

    let mut wifi_up = false;
    let mut wifi_seen = false;

    for e in entries.flatten() {
        let dir = e.path();
        let Some(name) = dir.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name == "lo" {
            continue;
        }

        if is_wireless(&dir) {
            wifi_seen = true;
            if has_carrier(&dir) && has_routable_addr(name) {
                wifi_up = true;
            }
        } else if has_carrier(&dir) && has_routable_addr(name) {
            // A wired interface that's actually usable.
            return Network::Ethernet;
        }
    }

    if wifi_up {
        Network::WifiConnected
    } else if wifi_seen {
        Network::WifiDisconnected
    } else {
        Network::WifiDisconnected
    }
}

// --------------------------------------------------------- rtnetlink socket

/// Netlink socket subscribed to link and address changes. Used as a wakeup
/// only; payloads are drained and discarded.
pub struct NetlinkSocket(OwnedFd);

// Multicast group bits (RTMGRP_*), which libc doesn't always expose.
const RTMGRP_LINK: u32 = 1;
const RTMGRP_IPV4_IFADDR: u32 = 0x10;
const RTMGRP_IPV6_IFADDR: u32 = 0x100;

impl NetlinkSocket {
    pub fn new() -> anyhow::Result<Self> {
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                libc::NETLINK_ROUTE,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: socket() returned a fresh fd we now own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as u16;
        addr.nl_pid = 0;
        // Link up/down, and v4/v6 address add/remove -- everything that can
        // change what `read()` would return.
        addr.nl_groups = RTMGRP_LINK | RTMGRP_IPV4_IFADDR | RTMGRP_IPV6_IFADDR;

        let r = unsafe {
            libc::bind(
                fd.as_fd().as_raw_fd(),
                &addr as *const libc::sockaddr_nl as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if r < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(NetlinkSocket(fd))
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }

    /// Drain queued messages without parsing. True if anything arrived.
    pub fn drain(&self) -> bool {
        crate::battery::drain_socket(self.0.as_fd())
    }
}
