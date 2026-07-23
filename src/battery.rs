//! Battery state from sysfs, woken by udev uevents.
//!
//! The netlink socket is used purely as a "something changed" signal: we never
//! parse the message, we just drain it and re-read sysfs. Battery events are
//! rare, sysfs reads are cheap, and this avoids all uevent parsing.

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

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

// ------------------------------------------------------------ udev netlink

/// A netlink socket bound to the kernel's uevent multicast group. Readable
/// whenever any device changes; we don't parse the payload.
pub struct UeventSocket(OwnedFd);

impl UeventSocket {
    pub fn new() -> anyhow::Result<Self> {
        // NETLINK_KOBJECT_UEVENT carries kernel device events.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                libc::NETLINK_KOBJECT_UEVENT,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: socket() returned a fresh fd we now own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as u16;
        addr.nl_pid = 0; // let the kernel assign
        addr.nl_groups = 1; // group 1 == kernel uevents

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
    loop {
        let n = unsafe {
            libc::recv(
                fd.as_raw_fd(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if n > 0 {
            got = true;
            continue; // keep draining; several events can be queued
        }
        // n == 0 or EAGAIN/EWOULDBLOCK: nothing left. Any other error we also
        // treat as "stop reading" -- a bar shouldn't die over a netlink hiccup.
        return got;
    }
}
