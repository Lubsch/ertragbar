//! Battery state from sysfs, woken by udev uevents.
//!
//! The netlink socket is used purely as a "something changed" signal: we never
//! parse the message, we just drain it and re-read sysfs. Battery events are
//! rare, sysfs reads are cheap, and this avoids all uevent parsing.

use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};

use rustix::net;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Percentage(pub u8);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Battery {
    None,
    Charging(Percentage),
    Discharging(Percentage),
}

const POWER_SUPPLY: &str = "/sys/class/power_supply";

/// Find the first real battery. Power supplies also include AC adapters and
/// peripheral batteries (mice, headsets); we want `type == Battery` and skip
/// anything scoped as a Device rather than the system battery.
fn battery_path() -> Option<PathBuf> {
    let entries = std::fs::read_dir(POWER_SUPPLY).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if read_trimmed(&p.join("type")).as_deref() != Some("Battery") {
            continue;
        }
        // "Device" scope means a peripheral (mouse, keyboard), not the laptop.
        if read_trimmed(&p.join("scope")).as_deref() == Some("Device") {
            continue;
        }
        return Some(p);
    }
    None
}

fn read_trimmed(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned())
}

/// Read current battery state. Returns NoBattery on a desktop or if sysfs is
/// missing/unreadable -- a status bar should degrade quietly, not error out.
pub fn read(path: Option<&Path>) -> Battery {
    let Some(p) = path else {
        return Battery::None;
    };

    // capacity is the kernel's already-computed percentage; prefer it over
    // charge_now/charge_full arithmetic, which disagrees on some firmware.
    let pct = read_trimmed(&p.join("capacity"))
        .and_then(|s| s.parse::<u32>().ok())
        .map(|v| v.min(100) as u8);

    let Some(pct) = pct else {
        return Battery::None;
    };

    // status: Charging | Discharging | Full | Not charging | Unknown
    match read_trimmed(&p.join("status")).as_deref() {
        Some("Charging") => Battery::Charging(Percentage(pct)),
        Some("Full") => Battery::Charging(Percentage(pct)),
        // "Not charging" means plugged in but held (charge thresholds), which
        // is closer to charging than draining.
        Some("Not charging") => Battery::Charging(Percentage(pct)),
        _ => Battery::Discharging(Percentage(pct)),
    }
}

/// Cached battery location so we don't re-scan the directory on every update.
pub struct BatterySource {
    path: Option<PathBuf>,
}

impl BatterySource {
    pub fn new() -> Self {
        BatterySource { path: battery_path() }
    }

    pub fn read(&self) -> Battery {
        read(self.path.as_deref())
    }
}

impl Default for BatterySource {
    fn default() -> Self {
        Self::new()
    }
}

/// A netlink socket bound to the kernel's uevent multicast group. Readable
/// whenever any device changes; we don't parse the payload.
pub struct UeventSocket(OwnedFd);

impl UeventSocket {
    pub fn new() -> anyhow::Result<Self> {
        let fd = net::socket_with(net::AddressFamily::NETLINK, net::SocketType::DGRAM, net::SocketFlags::CLOEXEC | net::SocketFlags::NONBLOCK, Some(net::netlink::KOBJECT_UEVENT))?;

        let addr = net::netlink::SocketAddrNetlink::new(0, 1);
        net::bind(&fd, &addr)?;

        Ok(UeventSocket(fd))
    }

    pub fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }

    /// Drain all pending messages without parsing them. Returns true if at
    /// least one arrived, i.e. something changed and state should be re-read.
    pub fn drain(&self) -> bool {
        drain_socket(self.0.as_fd())
    }
}

/// Read and discard everything queued on a nonblocking socket.
pub(crate) fn drain_socket(fd: std::os::fd::BorrowedFd<'_>) -> bool {
    let mut buf = [0u8; 4096];
    let mut got = false;
    while let Ok((n, _)) = net::recv(fd, &mut buf, net::RecvFlags::DONTWAIT) {
        if n > 0 {
            got = true;
        }
    }
    got
}
