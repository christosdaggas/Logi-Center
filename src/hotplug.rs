//! Linux inotify change signals only; never enumerates/opens a HID endpoint.
//! Keep the returned `Hotplug` alive and drain the bounded receiver to request a
//! normal coordinator scan. Dropping/stopping joins the worker via a wake fd.
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const DEBOUNCE: Duration = Duration::from_millis(250);
const MAX_BURST: Duration = Duration::from_secs(2);
const WATCH_MASK: u32 = libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_ATTRIB
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF;

pub struct Hotplug {
    stop: Option<OwnedFd>,
    thread: Option<JoinHandle<()>>,
}

impl Hotplug {
    /// Watches /dev plus /sys/class/hidraw when present. Receiver capacity is
    /// one: bursts coalesce, rather than queuing repeated full scans. No initial
    /// event is emitted; coordinator must perform its usual startup scan.
    pub fn start() -> io::Result<(Self, Receiver<()>)> {
        Self::watch_paths(&[Path::new("/dev"), Path::new("/sys/class/hidraw")])
    }

    fn watch_paths(paths: &[&Path]) -> io::Result<(Self, Receiver<()>)> {
        // SAFETY: returned fd is checked and immediately placed in OwnedFd.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let inotify = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut watched = 0;
        let mut last_error = None;
        for path in paths {
            let path = CString::new(path.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in watch path"))?;
            // SAFETY: inotify and the NUL-terminated path are valid.
            let wd =
                unsafe { libc::inotify_add_watch(inotify.as_raw_fd(), path.as_ptr(), WATCH_MASK) };
            if wd >= 0 {
                watched += 1;
            } else {
                last_error = Some(io::Error::last_os_error());
            }
        }
        if watched == 0 {
            return Err(last_error.unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "No hotplug directory available")
            }));
        }
        // eventfd avoids a pipe's SIGPIPE risk if a failed watcher has already
        // exited. Duplicates share one counter; neither endpoint is a device.
        let wake = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if wake < 0 {
            return Err(io::Error::last_os_error());
        }
        let write = unsafe { OwnedFd::from_raw_fd(wake) };
        let duplicate = unsafe { libc::fcntl(write.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error());
        }
        let read = unsafe { OwnedFd::from_raw_fd(duplicate) };
        let (sender, receiver) = sync_channel(1);
        let thread = thread::Builder::new()
            .name("logicenter-hotplug".into())
            .spawn(move || run(inotify, read, sender))?;
        Ok((
            Self {
                stop: Some(write),
                thread: Some(thread),
            },
            receiver,
        ))
    }

    pub fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let wake = 1u64;
            // SAFETY: eventfd expects one native-endian u64. Retry interrupted
            // writes; EAGAIN means a pending counter already wakes the worker.
            loop {
                let written = unsafe {
                    libc::write(
                        stop.as_raw_fd(),
                        (&wake as *const u64).cast(),
                        std::mem::size_of::<u64>(),
                    )
                };
                if written >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    break;
                }
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Hotplug {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Default)]
struct Debouncer {
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Debouncer {
    fn event(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }
    fn remaining(&self, now: Instant) -> Option<Duration> {
        let first = self.first?;
        let last = self.last?;
        Some(
            (last + DEBOUNCE)
                .min(first + MAX_BURST)
                .saturating_duration_since(now),
        )
    }
    fn ready(&mut self, now: Instant) -> bool {
        if self.remaining(now) == Some(Duration::ZERO) {
            self.first = None;
            self.last = None;
            true
        } else {
            false
        }
    }
}

fn relevant(name: &[u8], mask: u32) -> bool {
    if mask & (libc::IN_Q_OVERFLOW | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_IGNORED)
        != 0
    {
        return true;
    }
    name == b"uinput"
        || [b"hidraw".as_slice(), b"video".as_slice()]
            .iter()
            .any(|prefix| {
                name.strip_prefix(*prefix).is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.iter().all(u8::is_ascii_digit)
                })
            })
}

fn events_relevant(buffer: &[u8]) -> bool {
    let mut offset = 0usize;
    let header = std::mem::size_of::<libc::inotify_event>();
    while offset + header <= buffer.len() {
        // inotify_event is native-endian and may be unaligned in our byte vec.
        let event = unsafe {
            std::ptr::read_unaligned(buffer[offset..].as_ptr().cast::<libc::inotify_event>())
        };
        let Some(end) = offset
            .checked_add(header)
            .and_then(|start| start.checked_add(event.len as usize))
        else {
            return true;
        };
        if end > buffer.len() {
            return true;
        } // malformed/partial => one rescan, not a panic
        let name = &buffer[offset + header..end];
        let name = name.split(|byte| *byte == 0).next().unwrap_or_default();
        if relevant(name, event.mask) {
            return true;
        }
        offset = end;
    }
    false
}

fn run(inotify: OwnedFd, stop: OwnedFd, sender: SyncSender<()>) {
    let mut debounce = Debouncer::default();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let now = Instant::now();
        let timeout = debounce
            .remaining(now)
            .map(|duration| duration.as_millis().max(1).min(i32::MAX as u128) as i32)
            .unwrap_or(-1);
        let mut fds = [
            libc::pollfd {
                fd: stop.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: inotify.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if result < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if fds[0].revents != 0 {
            break;
        }
        if fds[1].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            break;
        }
        if fds[1].revents & libc::POLLIN != 0 {
            // Bound each drain so sustained unrelated /dev activity cannot
            // starve cancellation or the maximum burst deadline.
            for _ in 0..16 {
                let count = unsafe {
                    libc::read(
                        inotify.as_raw_fd(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                    )
                };
                if count < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    if error.kind() == io::ErrorKind::WouldBlock {
                        break;
                    }
                    return;
                }
                if count == 0 {
                    return;
                }
                if events_relevant(&buffer[..count as usize]) {
                    debounce.event(Instant::now());
                }
            }
        }
        if debounce.ready(Instant::now()) {
            match sender.try_send(()) {
                Ok(()) | Err(TrySendError::Full(())) => (),
                Err(TrySendError::Disconnected(())) => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn hotplug_debounce_coalesces_bursts_and_caps_sustained_activity() {
        let mut debounce = Debouncer::default();
        let time = Instant::now();
        assert!(debounce.remaining(time).is_none());
        debounce.event(time);
        debounce.event(time + Duration::from_millis(200));
        assert!(!debounce.ready(time + Duration::from_millis(449)));
        assert!(debounce.ready(time + Duration::from_millis(450)));
        assert!(!debounce.ready(time + Duration::from_secs(3)));
        for millis in (0..=2000).step_by(100) {
            debounce.event(time + Duration::from_millis(millis));
        }
        assert!(debounce.ready(time + MAX_BURST));
    }

    #[test]
    fn hotplug_filters_unrelated_dev_events_and_handles_overflow() {
        assert!(relevant(b"hidraw12", libc::IN_CREATE));
        assert!(relevant(b"video5", libc::IN_CREATE));
        assert!(relevant(b"video1", libc::IN_DELETE));
        assert!(relevant(b"video5", libc::IN_ATTRIB));
        assert!(!relevant(b"video", libc::IN_CREATE));
        assert!(!relevant(b"videox", libc::IN_CREATE));
        assert!(relevant(b"uinput", libc::IN_ATTRIB));
        assert!(!relevant(b"tty2", libc::IN_CREATE));
        assert!(!relevant(b"hidraw", libc::IN_CREATE));
        assert!(relevant(b"", libc::IN_Q_OVERFLOW));
        assert!(relevant(b"", libc::IN_IGNORED));
        assert!(!events_relevant(&[]));
        let event = |name: &[u8], mask: u32, length: u32| {
            let mut buffer = Vec::new();
            buffer.extend_from_slice(&1i32.to_ne_bytes());
            buffer.extend_from_slice(&mask.to_ne_bytes());
            buffer.extend_from_slice(&0u32.to_ne_bytes());
            buffer.extend_from_slice(&length.to_ne_bytes());
            buffer.extend_from_slice(name);
            buffer
        };
        assert!(events_relevant(&event(b"hidraw99\0", libc::IN_CREATE, 9)));
        assert!(!events_relevant(&event(
            b"unrelated\0",
            libc::IN_CREATE,
            10
        )));
        assert!(events_relevant(&event(b"", libc::IN_Q_OVERFLOW, 0)));
        assert!(events_relevant(&event(b"", libc::IN_CREATE, u32::MAX)));
    }

    #[test]
    fn hotplug_real_inotify_fixture_is_stoppable_without_hardware() {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "logicenter-hotplug-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        let (mut watcher, events) = Hotplug::watch_paths(&[&root]).unwrap();
        std::fs::write(root.join("hidraw99"), "fixture, not a device").unwrap();
        events.recv_timeout(Duration::from_secs(3)).unwrap();
        std::fs::write(root.join("unrelated"), "fixture").unwrap();
        assert!(events.recv_timeout(Duration::from_millis(350)).is_err());
        let start = Instant::now();
        watcher.stop();
        assert!(start.elapsed() < Duration::from_secs(1));
        watcher.stop(); // idempotent
        assert!(matches!(
            events.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }
}
