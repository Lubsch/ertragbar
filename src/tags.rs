//! Tag state from mango's `mmsg`.
//!
//! `mmsg get all-tags` gives the initial state, `mmsg watch all-tags` streams
//! one JSON object per line on every change. The watch pipe is read
//! nonblocking into our own line buffer, so a partially written line can never
//! stall the event loop.

use std::os::fd::{AsFd, BorrowedFd};
use std::process::{Child, ChildStdout, Command, Stdio};

use nanoserde::DeJson;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum TagState {
    Empty,
    Occupied,
    Urgent,
    Selected,
}

/// Tag state per monitor name, as reported by mmsg.
pub type TagSnapshot = Vec<(String, [TagState; 9])>;

#[derive(Debug, DeJson)]
pub struct MmsgOutput {
    all_tags: Vec<MonitorTags>,
}

#[derive(Debug, DeJson)]
struct MonitorTags {
    monitor: String,
    tags: [Tag; 9],
}

#[derive(Debug, DeJson)]
struct Tag {
    is_active: bool,
    is_urgent: bool,
    client_count: u32,
}

pub fn mmsg_to_tags(msg_output: MmsgOutput) -> TagSnapshot {
    msg_output
        .all_tags
        .into_iter()
        .map(|MonitorTags { monitor, tags }| {
            let tags = tags.map(|t| {
                if t.is_active {
                    TagState::Selected
                } else if t.is_urgent {
                    TagState::Urgent
                } else if t.client_count > 0 {
                    TagState::Occupied
                } else {
                    TagState::Empty
                }
            });
            (monitor, tags)
        })
        .collect()
}

pub fn poll_tags() -> anyhow::Result<TagSnapshot> {
    let output = Command::new("mmsg").args(["get", "all-tags"]).output()?;
    let output = String::from_utf8(output.stdout)?;
    Ok(mmsg_to_tags(MmsgOutput::deserialize_json(&output)?))
}

/// A running `mmsg watch all-tags`. Killed on drop.
pub struct TagWatcher {
    child: Child,
    stdout: ChildStdout,
    buf: Vec<u8>,
}

impl TagWatcher {
    pub fn spawn() -> anyhow::Result<Self> {
        let mut child = Command::new("mmsg")
            .args(["watch", "all-tags"])
            .stdout(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let flags = rustix::fs::fcntl_getfl(&stdout)?;
        rustix::fs::fcntl_setfl(&stdout, flags | rustix::fs::OFlags::NONBLOCK)?;
        Ok(TagWatcher {
            child,
            stdout,
            buf: Vec::new(),
        })
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.stdout.as_fd()
    }

    /// Read everything currently in the pipe and return the newest complete
    /// snapshot, if any. A tag switch can emit several lines at once; only the
    /// last matters, so a burst costs one paint.
    pub fn read_latest(&mut self) -> anyhow::Result<Option<TagSnapshot>> {
        let mut chunk = [0u8; 4096];
        loop {
            match rustix::io::read(&self.stdout, &mut chunk) {
                Ok(0) => anyhow::bail!("mmsg watch exited"),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(rustix::io::Errno::AGAIN) => break,
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(last_snapshot(&mut self.buf))
    }
}

impl Drop for TagWatcher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Consume all complete lines from `buf`, leaving a trailing partial line in
/// place, and return the last one that parsed.
fn last_snapshot(buf: &mut Vec<u8>) -> Option<TagSnapshot> {
    let end = buf.iter().rposition(|&b| b == b'\n')? + 1;
    let latest = buf[..end]
        .split(|&b| b == b'\n')
        .rev()
        .find_map(|l| MmsgOutput::deserialize_json(std::str::from_utf8(l).ok()?).ok())
        .map(mmsg_to_tags);
    buf.drain(..end);
    latest
}

#[cfg(test)]
mod tests {
    use super::*;
    use TagState::*;

    /// One monitor; tag 1 has `active`, the rest are empty unless overridden.
    fn line(monitor: &str, tag2: &str) -> String {
        let empty = r#"{"is_active":false,"is_urgent":false,"client_count":0}"#;
        let mut tags = [empty; 9];
        tags[0] = r#"{"is_active":true,"is_urgent":false,"client_count":1}"#;
        tags[1] = tag2;
        format!(
            r#"{{"all_tags":[{{"monitor":"{monitor}","tags":[{}]}}]}}"#,
            tags.join(",")
        )
    }

    #[test]
    fn tag_states() {
        let urgent = r#"{"is_active":false,"is_urgent":true,"client_count":1}"#;
        let snap = mmsg_to_tags(MmsgOutput::deserialize_json(&line("eDP-1", urgent)).unwrap());
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].0, "eDP-1");
        assert_eq!(&snap[0].1[..3], &[Selected, Urgent, Empty]);
    }

    #[test]
    fn partial_line_is_kept() {
        let occupied = r#"{"is_active":false,"is_urgent":false,"client_count":3}"#;
        let full = line("A", occupied);
        let next = line("B", occupied);
        let (head, tail) = next.split_at(10);

        let mut buf = format!("{full}\n{head}").into_bytes();
        let snap = last_snapshot(&mut buf).unwrap();
        assert_eq!(snap[0].0, "A");
        assert_eq!(snap[0].1[1], Occupied);
        assert_eq!(buf, head.as_bytes());

        assert!(last_snapshot(&mut buf).is_none());
        buf.extend_from_slice(format!("{tail}\n").as_bytes());
        assert_eq!(last_snapshot(&mut buf).unwrap()[0].0, "B");
        assert!(buf.is_empty());
    }
}
