use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::model::Transport;
use anyhow::{Context, Result, bail};

/// A raw Linux hidraw endpoint.
pub struct Hidraw {
    file: File,
    path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct HidrawInterface {
    pub path: PathBuf,
    pub vendor_id: u16,
    pub product_id: u16,
    pub name: String,
    pub transport: Transport,
    pub hid_uniq: String,
}

impl Hidraw {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_SYNC)
            .open(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    /// Write one report. HID reports on hidraw include the report ID as the
    /// first byte.
    pub fn write_report(&mut self, report: &[u8]) -> Result<()> {
        self.file
            .write_all(report)
            .with_context(|| format!("write failed on {}", self.path.display()))?;
        self.file
            .flush()
            .with_context(|| format!("flush failed on {}", self.path.display()))?;
        Ok(())
    }

    /// Read one report, waiting at most `timeout`. A timeout is represented by
    /// `Ok(None)`, while an actual I/O failure is an error.
    pub fn read_report(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let fd = self.file.as_raw_fd();
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = timeout.as_millis().min(i32::MAX as u128) as i32;

        loop {
            let result = unsafe { libc::poll(&mut pollfd, 1, millis) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error)
                    .with_context(|| format!("poll failed on {}", self.path.display()));
            }
            if result == 0 {
                return Ok(None);
            }
            if pollfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                bail!("hidraw device {} disconnected", self.path.display());
            }
            if pollfd.revents & libc::POLLIN == 0 {
                return Ok(None);
            }

            let mut buffer = [0u8; 64];
            loop {
                match self.file.read(&mut buffer) {
                    Ok(0) => bail!("hidraw device {} closed", self.path.display()),
                    Ok(count) => return Ok(Some(buffer[..count].to_vec())),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("read failed on {}", self.path.display()));
                    }
                }
            }
        }
    }

    /// Consume unsolicited notifications and stale replies.
    pub fn drain(&mut self) {
        for _ in 0..64 {
            match self.read_report(Duration::ZERO) {
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
    }
}

/// Enumerate Logitech hidraw nodes that expose the HID++ interface. HID++
/// uses interface 2 on USB receivers, which is also where its reports live.
pub fn logitech_interfaces() -> Vec<HidrawInterface> {
    logitech_interfaces_with_errors().0
}

pub fn logitech_interfaces_with_errors() -> (Vec<HidrawInterface>, Vec<String>) {
    let mut result = Vec::new();
    let mut errors = Vec::new();
    let entries = match fs::read_dir("/sys/class/hidraw") {
        Ok(entries) => entries,
        Err(error) => {
            errors.push(format!("cannot enumerate /sys/class/hidraw: {error}"));
            return (result, errors);
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(format!("hidraw enumeration: {error}"));
                continue;
            }
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("hidraw") {
            continue;
        }
        let uevent_path = entry.path().join("device/uevent");
        let uevent = match fs::read_to_string(&uevent_path) {
            Ok(value) => value,
            Err(error) => {
                errors.push(format!("{}: {error}", uevent_path.display()));
                continue;
            }
        };
        let mut hid_id = None;
        let mut hid_name = String::new();
        let mut hid_phys = String::new();
        let mut hid_uniq = String::new();
        for line in uevent.lines() {
            if let Some(value) = line.strip_prefix("HID_ID=") {
                hid_id = Some(value.to_string());
            } else if let Some(value) = line.strip_prefix("HID_NAME=") {
                hid_name = value.to_string();
            } else if let Some(value) = line.strip_prefix("HID_PHYS=") {
                hid_phys = value.to_string();
            } else if let Some(value) = line.strip_prefix("HID_UNIQ=") {
                hid_uniq = value.to_string();
            }
        }
        let Some(hid_id) = hid_id else { continue };
        let Some((vendor_id, product_id)) = parse_hid_id(&hid_id) else {
            continue;
        };
        if vendor_id != 0x046d {
            continue;
        }
        let interface = parse_interface(&hid_phys);
        // Direct USB devices can expose HID++ on interfaces other than 2.
        // Check report IDs rather than probing ordinary keyboard/mouse nodes.
        let descriptor_hidpp = match fs::read(entry.path().join("device/report_descriptor")) {
            Ok(bytes) => has_hidpp_reports(&bytes),
            Err(error) => {
                if !supports_transport(&hid_id, interface) {
                    errors.push(format!(
                        "{} report descriptor could not be checked: {error}",
                        entry.path().display()
                    ));
                }
                false
            }
        };
        if !supports_transport(&hid_id, interface) && !descriptor_hidpp {
            continue;
        }
        result.push(HidrawInterface {
            path: PathBuf::from("/dev").join(name.as_ref()),
            vendor_id,
            product_id,
            name: hid_name,
            transport: if hid_id.starts_with("0005:") {
                Transport::Bluetooth
            } else {
                Transport::Usb
            },
            hid_uniq,
        });
    }

    result.sort_by(|a, b| a.path.cmp(&b.path));
    (result, errors)
}

fn has_hidpp_reports(descriptor: &[u8]) -> bool {
    // Parse HID short/long items, not arbitrary byte substrings in usages.
    let mut offset = 0;
    let mut short = false;
    let mut long = false;
    while offset < descriptor.len() {
        let prefix = descriptor[offset];
        if prefix == 0xFE {
            let Some(size) = descriptor.get(offset + 1) else {
                return false;
            };
            offset += 3 + *size as usize;
            continue;
        }
        let size = match prefix & 3 {
            3 => 4,
            size => size as usize,
        };
        if offset + 1 + size > descriptor.len() {
            return false;
        }
        if prefix == 0x85 && size == 1 {
            short |= descriptor[offset + 1] == 0x10;
            long |= descriptor[offset + 1] == 0x11;
        }
        offset += 1 + size;
    }
    short || long
}

fn supports_transport(hid_id: &str, interface: Option<u32>) -> bool {
    // Bluetooth HID++ nodes do not have a USB interface-2 physical path.
    hid_id.starts_with("0005:") || interface == Some(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bluetooth_does_not_require_a_usb_interface() {
        assert!(supports_transport("0005:0000046D:0000B034", None));
        assert!(supports_transport("0003:0000046D:0000C548", Some(2)));
        assert!(!supports_transport("0003:0000046D:0000C548", Some(0)));
    }

    #[test]
    fn hid_descriptor_checks_report_items_not_embedded_usage_bytes() {
        assert!(has_hidpp_reports(&[0x85, 0x10, 0x85, 0x11]));
        assert!(!has_hidpp_reports(&[0x06, 0x85, 0x11]));
        assert!(!has_hidpp_reports(&[0xFE, 0x02, 0, 0x85, 0x11]));
        assert!(!has_hidpp_reports(&[0x85]));
        assert_eq!(
            parse_hid_id("0003:0000046D:0000C548"),
            Some((0x046D, 0xC548))
        );
        assert_eq!(parse_hid_id("0003:00000000:00000000"), Some((0, 0)));
    }
}

fn parse_hid_id(value: &str) -> Option<(u16, u16)> {
    // HID_ID=0003:0000046D:0000C548
    let mut parts = value.split(':');
    parts.next()?;
    let vendor = parts.next()?;
    let product = parts.next()?;
    let vendor = u16::from_str_radix(vendor, 16).ok()?;
    let product = u16::from_str_radix(product, 16).ok()?;
    Some((vendor, product))
}

fn parse_interface(phys: &str) -> Option<u32> {
    let marker = phys.rfind("input")?;
    phys[marker + 5..].parse().ok()
}
