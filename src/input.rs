use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

use crate::profiles::KeyChord;
use anyhow::{Context, Result};

#[repr(C)]
#[derive(Default)]
struct InputId {
    bus: u16,
    vendor: u16,
    product: u16,
    version: u16,
}
#[repr(C)]
struct Setup {
    id: InputId,
    name: [u8; 80],
    effects: u32,
}
#[repr(C)]
struct Event {
    time: libc::timeval,
    kind: u16,
    code: u16,
    value: i32,
}

pub struct Keyboard {
    file: File,
    active: bool,
}

impl Keyboard {
    pub fn open() -> Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open("/dev/uinput")
            .context(
                "Shortcuts and gestures need write access to /dev/uinput. No buttons were diverted",
            )?;
        let fd = file.as_raw_fd();
        let check = |result: i32| -> Result<()> {
            if result < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        };
        unsafe {
            check(libc::ioctl(fd, 0x40045564, 1))?;
            for key in 1..=255 {
                check(libc::ioctl(fd, 0x40045565, key))?;
            }
            let mut setup = Setup {
                id: InputId {
                    bus: 0x06,
                    vendor: 1,
                    product: 1,
                    version: 1,
                },
                name: [0; 80],
                effects: 0,
            };
            let name = b"Logi Center shortcuts";
            setup.name[..name.len()].copy_from_slice(name);
            let request = (1u64 << 30)
                | ((std::mem::size_of::<Setup>() as u64) << 16)
                | ((b'U' as u64) << 8)
                | 3;
            check(libc::ioctl(fd, request as libc::c_ulong, &setup))?;
            check(libc::ioctl(fd, 0x5501))?;
        }
        // Allow the compositor to discover the newly created virtual keyboard.
        std::thread::sleep(std::time::Duration::from_millis(80));
        Ok(Self { file, active: true })
    }

    pub fn send(&mut self, chord: &KeyChord) -> Result<()> {
        anyhow::ensure!(
            self.active,
            "Virtual keyboard was disabled after an input error"
        );
        chord.validate()?;
        let result = emit_chord(
            chord,
            |kind, code, value| self.event(kind, code, value),
            || {
                std::thread::sleep(std::time::Duration::from_millis(8));
            },
        );
        if result.is_err() {
            // Release writes can fail too (unplug/EAGAIN). Destroying the
            // endpoint makes the input subsystem release its remaining keys.
            unsafe {
                libc::ioctl(self.file.as_raw_fd(), 0x5502);
            }
            self.active = false;
        }
        result
    }

    fn event(&mut self, kind: u16, code: u16, value: i32) -> Result<()> {
        let event = Event {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            kind,
            code,
            value,
        };
        let data = unsafe {
            std::slice::from_raw_parts(
                (&event as *const Event).cast::<u8>(),
                std::mem::size_of::<Event>(),
            )
        };
        self.file
            .write_all(data)
            .context("Could not send shortcut input")
    }
}

impl Drop for Keyboard {
    fn drop(&mut self) {
        if self.active {
            unsafe {
                libc::ioctl(self.file.as_raw_fd(), 0x5502);
            }
        }
    }
}

fn emit_chord(
    chord: &KeyChord,
    mut emit: impl FnMut(u16, u16, i32) -> Result<()>,
    pause: impl FnOnce(),
) -> Result<()> {
    chord.validate()?;
    let result = (|| -> Result<()> {
        for key in &chord.keys {
            emit(1, *key, 1)?;
        }
        emit(0, 0, 0)?;
        pause();
        for key in chord.keys.iter().rev() {
            emit(1, *key, 0)?;
        }
        emit(0, 0, 0)
    })();
    if result.is_err() {
        for key in chord.keys.iter().rev() {
            let _ = emit(1, *key, 0);
        }
        let _ = emit(0, 0, 0);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_chords_never_emit_any_input() {
        assert!(
            emit_chord(
                &KeyChord::default(),
                |_, _, _| panic!("invalid chord emitted input"),
                || {}
            )
            .is_err()
        );
    }
    #[test]
    fn every_press_has_a_reverse_order_release() {
        let chord = KeyChord {
            keys: vec![29, 42, 25],
            label: "Ctrl+Shift+P".into(),
        };
        let mut events = Vec::new();
        emit_chord(
            &chord,
            |kind, code, value| {
                events.push((kind, code, value));
                Ok(())
            },
            || {},
        )
        .unwrap();
        assert_eq!(
            events,
            vec![
                (1, 29, 1),
                (1, 42, 1),
                (1, 25, 1),
                (0, 0, 0),
                (1, 25, 0),
                (1, 42, 0),
                (1, 29, 0),
                (0, 0, 0)
            ]
        );
        assert_eq!(std::mem::size_of::<Setup>(), 92);
    }
    #[test]
    fn every_failure_attempts_all_releases_and_sync() {
        let chord = KeyChord {
            keys: vec![29, 46],
            label: "Ctrl+C".into(),
        };
        for fail_at in 0..6 {
            let mut events = Vec::new();
            let result = emit_chord(
                &chord,
                |kind, code, value| {
                    events.push((kind, code, value));
                    if events.len() == fail_at + 1 {
                        anyhow::bail!("simulated failure");
                    }
                    Ok(())
                },
                || {},
            );
            assert!(result.is_err());
            assert!(events.ends_with(&[(1, 46, 0), (1, 29, 0), (0, 0, 0)]));
        }
    }
}
