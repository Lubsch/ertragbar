//! Local time polling and a minute-boundary timerfd.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Date(pub u16, pub u8, pub u8);
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Time(pub u8, pub u8);

/// Current local wall-clock date and time, via libc `localtime_r`.
pub fn poll_time() -> anyhow::Result<(Date, Time)> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&secs, &mut tm);
    }
    let d = Date(
        (tm.tm_year + 1900).try_into()?,
        (tm.tm_mon + 1).try_into()?,
        tm.tm_mday.try_into()?,
    );
    let t = Time(tm.tm_hour.try_into()?, tm.tm_min.try_into()?);
    Ok((d, t))
}

/// Create a CLOCK_REALTIME timerfd armed to the next minute boundary.
/// CANCEL_ON_SET so clock steps (NTP, suspend, tz change) cancel the wait and
/// let us re-poll immediately rather than showing a stale minute.
pub fn minute_timerfd() -> anyhow::Result<OwnedFd> {
    let fd = unsafe {
        libc::timerfd_create(libc::CLOCK_REALTIME, libc::TFD_CLOEXEC | libc::TFD_NONBLOCK)
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    arm_next_minute(&fd)?;
    Ok(fd)
}

/// Arm (or re-arm) the timerfd to fire at the next whole minute.
pub fn arm_next_minute(fd: &OwnedFd) -> anyhow::Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let next = ((now / 60) + 1) * 60;
    let spec = libc::itimerspec {
        it_value: libc::timespec { tv_sec: next as libc::time_t, tv_nsec: 0 },
        it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 }, // one-shot; re-arm after fire
    };
    let flags = libc::TFD_TIMER_ABSTIME | libc::TFD_TIMER_CANCEL_ON_SET;
    let r = unsafe {
        libc::timerfd_settime(fd.as_raw_fd(), flags, &spec, std::ptr::null_mut())
    };
    if r < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
