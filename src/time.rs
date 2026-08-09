//! Local time polling and a minute-boundary timerfd.

use std::os::fd::OwnedFd;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Date(pub i16, pub i8, pub i8);
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Time(pub i8, pub i8);

/// Current local wall-clock date and time, via libc `localtime_r`.
pub fn poll_time() -> anyhow::Result<(Date, Time)> {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

    let ts = jiff::Timestamp::from_second(secs as i64)?;
    let tz = jiff::tz::TimeZone::system(); // uses internal cache
    let zoned = ts.to_zoned(tz);

    let d = Date(zoned.year(), zoned.month(), zoned.day());
    let t = Time(zoned.hour(), zoned.minute());

    Ok((d, t))
}

/// Create a CLOCK_REALTIME timerfd armed to the next minute boundary.
/// CANCEL_ON_SET so clock steps (NTP, suspend, tz change) cancel the wait and
/// let us re-poll immediately rather than showing a stale minute.
pub fn minute_timerfd() -> anyhow::Result<OwnedFd> {
    let fd = rustix::time::timerfd_create(rustix::time::TimerfdClockId::Realtime, rustix::time::TimerfdFlags::NONBLOCK)?;
    arm_next_minute(&fd)?;
    Ok(fd)
}

/// Arm (or re-arm) the timerfd to fire at the next whole minute.
pub fn arm_next_minute(fd: &OwnedFd) -> anyhow::Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let next = ((now / 60) + 1) * 60;
    let spec = rustix::time::Itimerspec {
        it_value: rustix::time::Timespec { tv_sec: next as i64, tv_nsec: 0 },
        it_interval: rustix::time::Timespec { tv_sec: 0, tv_nsec: 0 }, // one-shot; re-arm after fire
    };
    let flags = rustix::time::TimerfdTimerFlags::ABSTIME |  rustix::time::TimerfdTimerFlags::CANCEL_ON_SET;
    rustix::time::timerfd_settime(fd, flags, &spec)?;
    Ok(())
}
