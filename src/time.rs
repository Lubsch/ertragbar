//! Local time polling and a minute-boundary timerfd.

use std::os::fd::OwnedFd;
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::time;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Time {
    pub year: i32,
    pub month: i32,
    pub day: i32,
    pub hour: i32,
    pub minute: i32
}

/// Current local wall-clock date and time
pub fn poll_time() -> anyhow::Result<Time> {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as libc::time_t;

    // let ts = libc::time_t::default();
    let mut tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };

    Ok(Time {
        year: (1900 + tm.tm_year).try_into()?,
        month: (1 + tm.tm_mon).try_into()?,
        day: tm.tm_mday.try_into()?,
        hour: tm.tm_hour.try_into()?,
        minute: tm.tm_min.try_into()?,
    })
}

pub fn minute_timerfd() -> anyhow::Result<OwnedFd> {
    let timer = time::timerfd_create(time::TimerfdClockId::Realtime, time::TimerfdFlags::NONBLOCK)?;
    arm_next_minute(&timer)?;
    Ok(timer)
}

/// Arm (or re-arm) the timerfd to fire at the next whole minute.
pub fn arm_next_minute(timer: &OwnedFd) -> anyhow::Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let next = ((now / 60) + 1) * 60;
    let spec = time::Itimerspec {
        it_value: time::Timespec { tv_sec: next as i64, tv_nsec: 0 },
        it_interval: time::Timespec { tv_sec: 0, tv_nsec: 0 },
    };
    let flags = time::TimerfdTimerFlags::ABSTIME |  time::TimerfdTimerFlags::CANCEL_ON_SET;
    time::timerfd_settime(timer, flags, &spec)?;
    Ok(())
}
