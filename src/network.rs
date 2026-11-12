//! Network state from sysfs + /proc, woken by rtnetlink events.
//!
//! As with battery, the netlink socket is only a wakeup: we subscribe to link
//! and address multicast groups, drain without parsing, and recompute state by
//! reading sysfs. Avoids all nlmsghdr/rtattr handling.
//!
//! "Connected" for wifi means: carrier up AND the interface has a routable
//! (non-link-local) address. Carrier alone goes up during association, before
//! DHCP, which would show a connected icon while nothing actually works.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::net;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Network {
    Ethernet,
    WifiConnected,
    WifiDisconnected,
}

const SYS_NET: &str = "/sys/class/net";
const PROC_NET: &str = "/proc/net";

fn read_trimmed(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned())
}

/// An interface is wireless if it has a `wireless/` directory (or a phy80211
/// symlink, which iwd-managed devices have).
fn is_wireless(dir: &Path) -> bool {
    dir.join("wireless").exists() || dir.join("phy80211").exists()
}

/// Physical NICs (PCI, USB, SDIO, ...) have a `device` symlink pointing at
/// their bus device. Virtual interfaces -- wireguard, tun/tap, bridges, veth,
/// docker -- don't, so this keeps an always-on VPN from posing as ethernet.
fn is_physical(dir: &Path) -> bool {
    dir.join("device").exists()
}

/// carrier is 1 when the link has a physical/associated connection. Reading it
/// on a down interface returns EINVAL, hence the Option.
fn has_carrier(dir: &Path) -> bool {
    read_trimmed(&dir.join("carrier")).as_deref() == Some("1")
}

/// Whether `iface` has a routable address: any non-loopback, non-link-local
/// address. We read /proc rather than opening a netlink query, keeping this
/// dependency-free and simple.
fn has_routable_addr(proc_net: &Path, iface: &str) -> bool {
    // IPv4: /proc/net/route lists per-interface routes. A default route or any
    // non-link-local route on this iface means DHCP/config completed.
    if let Ok(routes) = std::fs::read_to_string(proc_net.join("route")) {
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
    if let Ok(v6) = std::fs::read_to_string(proc_net.join("if_inet6")) {
        for line in v6.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 6 || f[5] != iface {
                continue;
            }
            if f[3] == "00" {
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
    read_from(Path::new(SYS_NET), Path::new(PROC_NET))
}

/// [`read`] against arbitrary sysfs/procfs roots, for tests.
fn read_from(sys_net: &Path, proc_net: &Path) -> Network {
    let Ok(entries) = std::fs::read_dir(sys_net) else {
        return Network::WifiDisconnected;
    };

    let mut wifi_up = false;

    for e in entries.flatten() {
        let dir = e.path();
        let Some(name) = dir.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name == "lo" || !is_physical(&dir) {
            continue;
        }

        if is_wireless(&dir) {
            if has_carrier(&dir) && has_routable_addr(proc_net, name) {
                wifi_up = true;
            }
        } else if has_carrier(&dir) && has_routable_addr(proc_net, name) {
            // A wired interface that's actually usable.
            return Network::Ethernet;
        }
    }

    if wifi_up {
        Network::WifiConnected
    } else {
        Network::WifiDisconnected
    }
}

/// Netlink socket subscribed to link and address changes. Used as a wakeup
/// only; payloads are drained and discarded.
pub struct NetlinkSocket(OwnedFd);

// Multicast group bits (RTMGRP_*)
const RTMGRP_LINK: u32 = 1;
const RTMGRP_IPV4_IFADDR: u32 = 0x10;
const RTMGRP_IPV6_IFADDR: u32 = 0x100;

impl NetlinkSocket {
    pub fn new() -> anyhow::Result<Self> {
        let fd = net::socket_with(
            net::AddressFamily::NETLINK,
            net::SocketType::RAW,
            net::SocketFlags::CLOEXEC | net::SocketFlags::NONBLOCK,
            None,
        )?;

        // Link up/down, and v4/v6 address add/remove -- everything that can
        // change what `read()` would return.
        let addr = net::netlink::SocketAddrNetlink::new(
            0,
            RTMGRP_LINK | RTMGRP_IPV4_IFADDR | RTMGRP_IPV6_IFADDR,
        );
        net::bind(&fd, &addr)?;

        Ok(NetlinkSocket(fd))
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }

    /// Drain queued messages without parsing. True if anything arrived.
    pub fn drain(&self) -> bool {
        crate::socket::drain(self.0.as_fd())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Fake sysfs + procfs under a fresh temp dir.
    struct Fixture(PathBuf);

    impl Fixture {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("ertragbar-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("sys")).unwrap();
            fs::create_dir_all(root.join("proc")).unwrap();
            // Header line, then routes (Iface, Destination, ...).
            fs::write(root.join("proc/route"), "Iface\tDestination\n").unwrap();
            Fixture(root)
        }

        /// An interface with carrier and a default route.
        fn iface(&self, name: &str, physical: bool, wireless: bool) -> &Self {
            let dir = self.0.join("sys").join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("carrier"), "1\n").unwrap();
            if physical {
                fs::create_dir(dir.join("device")).unwrap();
            }
            if wireless {
                fs::create_dir(dir.join("wireless")).unwrap();
            }
            let mut routes = fs::read_to_string(self.0.join("proc/route")).unwrap();
            routes += &format!("{name}\t00000000\n");
            fs::write(self.0.join("proc/route"), routes).unwrap();
            self
        }

        fn read(&self) -> Network {
            read_from(&self.0.join("sys"), &self.0.join("proc"))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn wifi_behind_wireguard() {
        let f = Fixture::new("wg");
        f.iface("wlan0", true, true).iface("wg0", false, false);
        assert_eq!(f.read(), Network::WifiConnected);
    }

    #[test]
    fn ethernet_wins_over_wifi() {
        let f = Fixture::new("eth");
        f.iface("wlan0", true, true).iface("enp0s31f6", true, false);
        assert_eq!(f.read(), Network::Ethernet);
    }

    #[test]
    fn link_local_only_is_disconnected() {
        let f = Fixture::new("ll");
        f.iface("wlan0", true, true);
        // Replace the default route with a 169.254.0.0/16 one.
        fs::write(
            f.0.join("proc/route"),
            "Iface\tDestination\nwlan0\t0000FEA9\n",
        )
        .unwrap();
        assert_eq!(f.read(), Network::WifiDisconnected);
    }
}
