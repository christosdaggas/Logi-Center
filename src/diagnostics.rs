//! Bounded, read-only diagnostics. No shell, subprocess, HID transactions,
//! uinput opens/ioctls, raw logs, command lines or process environments.
//! `collect` walks sysfs/procfs and belongs on a worker, never the GTK loop.
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::model::ScanReport;

const MAX_HIDRAW: usize = 128;
const MAX_PROCESSES: usize = 8192;
const MAX_RECEIVERS: usize = 32;
const MAX_DEVICES: usize = 32;
pub const MAX_EXPORT_BYTES: usize = 512 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Accessible,
    PermissionDenied,
    Absent,
    Unavailable,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            Self::Accessible => "Available",
            Self::PermissionDenied => "Permission denied",
            Self::Absent => "Not present",
            Self::Unavailable => "Could not determine",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccessProbe {
    pub target: String,
    pub state: State,
    /// Device endpoint paths are omitted unless identifiers are opted in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub explanation: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Helper {
    pub name: String,
    pub state: State,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Backend {
    pub name: String,
    pub status: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Desktop {
    pub session_type: String,
    /// Only recognized desktop names, never untrusted arbitrary env values.
    pub desktop: String,
    pub display_present: bool,
    pub wayland_display_present: bool,
    pub session_bus_present: bool,
    pub backends: Vec<Backend>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Conflict {
    pub name: String,
    pub instances: usize,
    pub explanation: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Processes {
    pub state: State,
    pub inspected: usize,
    pub incomplete: bool,
    pub conflicts: Vec<Conflict>,
    pub note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Device {
    pub reference: String,
    pub online: bool,
    pub battery_percent: Option<u8>,
    pub feature_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receiver {
    pub reference: String,
    pub product_id: u16,
    pub direct: bool,
    pub devices: Vec<Device>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u8,
    pub generated_at_unix: u64,
    pub identifiers_included: bool,
    pub access: Vec<AccessProbe>,
    pub helpers: Vec<Helper>,
    pub desktop: Desktop,
    pub processes: Processes,
    pub receivers: Vec<Receiver>,
    pub scan_error_count: usize,
    /// Categories only: raw scanner errors can contain serials, paths and logs.
    pub scan_error_categories: BTreeMap<String, usize>,
    pub truncated: bool,
    pub notes: Vec<String>,
}

impl Report {
    pub fn to_json(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec_pretty(self)?;
        if bytes.len() > MAX_EXPORT_BYTES {
            bail!("Diagnostic report exceeds the export limit");
        }
        Ok(bytes)
    }

    /// Concise plain text for the UI. Uses already-redacted fields only.
    pub fn summary(&self) -> String {
        let mut lines = vec![format!(
            "Session: {} · {}",
            self.desktop.desktop, self.desktop.session_type
        )];
        for access in &self.access {
            lines.push(format!(
                "{}: {} — {}",
                access.target,
                access.state.label(),
                access.explanation
            ));
        }
        for backend in &self.desktop.backends {
            lines.push(format!("{}: {}", backend.name, backend.status));
        }
        for helper in &self.helpers {
            lines.push(format!("{}: {}", helper.name, helper.state.label()));
        }
        lines.push(format!(
            "Process metadata: {}{}",
            self.processes.state.label(),
            if self.processes.incomplete {
                " (incomplete; hidden or unreadable processes may exist)"
            } else {
                ""
            }
        ));
        if self.processes.conflicts.is_empty() {
            lines.push(
                "No recognized conflicting process names found (not proof of exclusive access)."
                    .into(),
            );
        }
        for conflict in &self.processes.conflicts {
            lines.push(format!(
                "Possible conflict: {} ({} instance(s)) — {}",
                conflict.name, conflict.instances, conflict.explanation
            ));
        }
        lines.push(self.processes.note.clone());
        lines.push(format!(
            "{} receiver(s), {} scan error(s); raw errors are never exported.",
            self.receivers.len(),
            self.scan_error_count
        ));
        if self.truncated {
            lines.push("Report was bounded; some entries were omitted.".into());
        }
        lines.extend(self.notes.iter().cloned());
        lines.join("\n")
    }
}

/// Permission checks use faccessat(AT_EACCESS), NOT opening a device. A positive
/// result is a current effective-credential permission hint, not proof that a
/// driver accepts a protocol or that input injection works. Never sends data.
pub fn collect(scan: &ScanReport, include_identifiers: bool) -> Report {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    let mut truncated = false;
    let mut hidraw_metadata_incomplete = false;
    let mut access = vec![access_probe(
        Path::new("/dev/uinput"),
        "Virtual input (uinput)",
        true,
        include_identifiers,
    )];
    if access[0].state == State::Absent {
        access.push(access_probe(
            Path::new("/dev/input/uinput"),
            "Legacy virtual input endpoint",
            true,
            include_identifiers,
        ));
    }
    let mut endpoints = Vec::new();
    match fs::read_dir("/sys/class/hidraw") {
        Ok(entries) => {
            for (index, entry) in entries.enumerate() {
                if index >= MAX_HIDRAW {
                    truncated = true;
                    hidraw_metadata_incomplete = true;
                    break;
                }
                let Ok(entry) = entry else {
                    hidraw_metadata_incomplete = true;
                    continue;
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                if !hidraw_name(&name) {
                    continue;
                }
                match read_bounded(&entry.path().join("device/uevent"), 4096) {
                    Ok(uevent) => {
                        if is_logitech(&uevent) {
                            endpoints.push(Path::new("/dev").join(name));
                        }
                    }
                    Err(_) => hidraw_metadata_incomplete = true,
                }
            }
        }
        Err(error) => {
            hidraw_metadata_incomplete = true;
            access.push(AccessProbe {
                target: "Hidraw enumeration".into(),
                state: classify_error(&error),
                path: None,
                explanation: "Sysfs could not be inspected; no HID transaction was attempted."
                    .into(),
            });
        }
    }
    // Include already discovered endpoints, but refuse arbitrary paths supplied
    // in snapshots. This is a permission probe, not a filesystem read primitive.
    for receiver in scan.receivers.iter().take(MAX_RECEIVERS) {
        let path = PathBuf::from(&receiver.path);
        if path.parent() == Some(Path::new("/dev"))
            && path
                .file_name()
                .is_some_and(|name| hidraw_name(&name.to_string_lossy()))
        {
            endpoints.push(path);
        }
    }
    endpoints.sort();
    endpoints.dedup();
    if endpoints.is_empty() {
        access.push(AccessProbe {
            target: "Logitech hidraw".into(), state: if hidraw_metadata_incomplete { State::Unavailable } else { State::Absent }, path: None,
            explanation: if hidraw_metadata_incomplete {
                "Enumeration was incomplete; absence cannot be concluded. No HID transaction was attempted."
            } else {
                "No Logitech hidraw endpoint found in readable metadata. This is not a protocol compatibility test."
            }.into(),
        });
    }
    if endpoints.len() > MAX_HIDRAW {
        truncated = true;
    }
    for (index, endpoint) in endpoints.iter().take(MAX_HIDRAW).enumerate() {
        access.push(access_probe(
            endpoint,
            &format!("Logitech hidraw endpoint {}", index + 1),
            false,
            include_identifiers,
        ));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let helpers: Vec<_> = [
        "solaar",
        "logid",
        "ratbagctl",
        "g810-led",
        "liquidctl",
        "v4l2-ctl",
        "pactl",
        "wpctl",
        "deskflow",
        "gdbus",
        "xprop",
        "xdotool",
        "ydotool",
    ]
    .into_iter()
    .map(|name| helper(name, &path, include_identifiers, home.as_deref()))
    .collect();
    let desktop = desktop(&helpers);
    let processes = processes(Path::new("/proc"));
    let (receivers, inventory_truncated) = inventory(scan, include_identifiers, home.as_deref());
    truncated |= inventory_truncated || processes.incomplete;
    let scan_error_categories = error_categories(&scan.errors);
    truncated |= scan.errors.len() > 256 || hidraw_metadata_incomplete;
    Report {
        schema_version: 1,
        generated_at_unix: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
        identifiers_included: include_identifiers,
        access, helpers, desktop, processes, receivers,
        scan_error_count: scan.errors.len(), scan_error_categories, truncated,
        notes: vec![
            "Read-only metadata and effective-credential access checks only; no helper was run and no device was opened or written.".into(),
            "uinput access permits synthetic keyboard/mouse input into the session. Grant it only to trusted local users/apps; never use MODE=0666.".into(),
            "uaccess is for the active local seat, not SSH/headless sessions. Missing uinput may mean the kernel module or device is unavailable; diagnostics do not load modules or change permissions.".into(),
            "Process names indicate possible interference, not proof that a device is held. Python wrappers and sandboxed/hidden processes may not be identifiable.".into(),
            "Default exports omit names, serials, endpoint/helper paths and raw errors. Home paths remain redacted even when identifiers are explicitly included.".into(),
        ],
    }
}

fn error_categories(errors: &[String]) -> BTreeMap<String, usize> {
    // Fixed prefixes only, never verbatim errors, even in identifier mode.
    let mut categories = BTreeMap::new();
    for error in errors.iter().take(256) {
        let prefix: String = error.chars().take(1024).collect();
        let prefix = prefix.to_ascii_lowercase();
        let category = if prefix.contains("permission") || prefix.contains("denied") {
            "permission"
        } else if prefix.contains("timeout") || prefix.contains("timed out") {
            "timeout"
        } else if prefix.contains("disconnect") || prefix.contains("not found") {
            "disconnected_or_absent"
        } else {
            "other"
        };
        *categories.entry(category.to_owned()).or_insert(0) += 1;
    }
    categories
}

fn hidraw_name(name: &str) -> bool {
    name.strip_prefix("hidraw").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn is_logitech(uevent: &str) -> bool {
    uevent.lines().any(|line| {
        line.strip_prefix("HID_ID=")
            .and_then(|id| id.split(':').nth(1))
            .and_then(|vendor| u32::from_str_radix(vendor, 16).ok())
            == Some(0x046d)
    })
}

fn read_bounded(path: &Path, limit: u64) -> io::Result<String> {
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "metadata limit exceeded",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "metadata is not UTF-8"))
}

fn classify_error(error: &io::Error) -> State {
    match error.kind() {
        io::ErrorKind::PermissionDenied => State::PermissionDenied,
        io::ErrorKind::NotFound => State::Absent,
        _ => State::Unavailable,
    }
}

fn effective_access(path: &Path, mode: libc::c_int) -> State {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return State::Unavailable;
    };
    // SAFETY: valid NUL-terminated path; no file descriptors are opened and no
    // data is read/written. Linux faccessat understands effective credentials.
    if unsafe { libc::faccessat(libc::AT_FDCWD, path.as_ptr(), mode, libc::AT_EACCESS) } == 0 {
        State::Accessible
    } else {
        classify_error(&io::Error::last_os_error())
    }
}

fn access_probe(path: &Path, target: &str, uinput: bool, include: bool) -> AccessProbe {
    let state = match fs::metadata(path) {
        Ok(metadata) if metadata.file_type().is_char_device() => effective_access(
            path,
            if uinput {
                libc::W_OK
            } else {
                libc::R_OK | libc::W_OK
            },
        ),
        Ok(_) => State::Unavailable,
        Err(error) => classify_error(&error),
    };
    let explanation = match state {
        State::Accessible => {
            "Permission check passed; protocol support, exclusive access and successful injection are not tested."
        }
        State::PermissionDenied => {
            "Endpoint exists but access is denied. Check active-seat uaccess rules/ACLs; no permission was changed."
        }
        State::Absent => "Endpoint is absent; this differs from insufficient permissions.",
        State::Unavailable => {
            "Endpoint type or access could not be verified; it is not reported as permission denied."
        }
    };
    AccessProbe {
        target: target.into(),
        state,
        path: include.then(|| path.to_string_lossy().into_owned()),
        explanation: explanation.into(),
    }
}

fn helper(name: &str, path: &std::ffi::OsStr, include: bool, home: Option<&Path>) -> Helper {
    let mut state = State::Absent;
    let mut found = None;
    // Ignore relative/empty PATH components, which could resolve repository or
    // current-directory executables. Existence is never runtime compatibility.
    for directory in std::env::split_paths(path)
        .filter(|path| path.is_absolute())
        .take(64)
    {
        let candidate = directory.join(name);
        match fs::metadata(&candidate) {
            Ok(metadata) if metadata.is_file() => {
                let access = effective_access(&candidate, libc::X_OK);
                if access == State::Accessible {
                    state = access;
                    found = Some(candidate);
                    break;
                }
                if state != State::PermissionDenied {
                    state = access;
                }
            }
            Ok(_) => {
                if state == State::Absent {
                    state = State::Unavailable;
                }
            }
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                if state != State::PermissionDenied {
                    state = classify_error(&error);
                }
            }
            Err(_) => (),
        }
    }
    Helper {
        name: name.into(),
        state,
        path: if include {
            found.map(|path| redact_home(&path.to_string_lossy(), home))
        } else {
            None
        },
    }
}

fn desktop(helpers: &[Helper]) -> Desktop {
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    let session_type = match session.as_str() {
        "wayland" | "x11" | "tty" => session,
        _ => "unknown".into(),
    };
    let desktops = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let name = if desktops
        .split(':')
        .any(|desktop| desktop == "gnome" || desktop == "ubuntu")
    {
        "GNOME"
    } else if desktops.split(':').any(|desktop| desktop == "kde") {
        "KDE Plasma"
    } else if desktops.contains("sway") {
        "Sway"
    } else if desktops.contains("hyprland") {
        "Hyprland"
    } else if desktops.contains("xfce") {
        "Xfce"
    } else {
        "Other/unknown"
    };
    let present = |key| std::env::var_os(key).is_some_and(|value| !value.is_empty());
    let display_present = present("DISPLAY");
    let wayland_display_present = present("WAYLAND_DISPLAY");
    let session_bus_present = present("DBUS_SESSION_BUS_ADDRESS");
    let has_helper = |name| {
        helpers
            .iter()
            .any(|helper| helper.name == name && helper.state == State::Accessible)
    };
    Desktop {
        session_type, desktop: name.into(), display_present, wayland_display_present, session_bus_present,
        backends: vec![
            Backend { name: "X11 foreground detection".into(), status: if display_present && has_helper("xprop") { "xprop and DISPLAY present; query success not tested (XWayland cannot see native Wayland windows)" } else { "DISPLAY or xprop missing" }.into() },
            Backend { name: "GNOME/KDE foreground detection".into(), status: if session_bus_present { "Session bus present; compositor integration/extension authorization still required and not tested" } else { "Session bus unavailable" }.into() },
            Backend { name: "wlroots foreground detection".into(), status: if wayland_display_present { "Wayland present; compositor-specific IPC/protocol support is not assumed" } else { "Wayland display unavailable" }.into() },
            Backend { name: "HID++ controls".into(), status: "Only advertised controls from the supplied scan are known; no compatibility inferred from permissions".into() },
        ],
    }
}

fn conflict_name(name: &str) -> Option<(&'static str, &'static str)> {
    match name.trim() {
        "solaar" => Some((
            "Solaar",
            "Another HID++ manager may compete for reports or overwrite settings.",
        )),
        "logid" | "logiops" => Some((
            "logid/logiops",
            "Another HID++ manager may compete for reports or overwrite settings.",
        )),
        "ratbagd" => Some((
            "ratbagd",
            "Another device configuration backend may control the same device.",
        )),
        "input-remapper"
        | "input-remapper-"
        | "input-remapper-service"
        | "keyd"
        | "kmonad"
        | "evremap" => Some((
            "Input remapper",
            "Multiple remappers can duplicate or intercept input.",
        )),
        "deskflow" | "deskflow-core" | "barrier" | "barrierc" | "barriers" | "synergy"
        | "synergyc" | "synergys" => Some((
            "Deskflow/Barrier/Synergy",
            "Input sharing may reroute synthetic or physical input.",
        )),
        _ => None,
    }
}

fn processes(root: &Path) -> Processes {
    let mut result = Processes { state: State::Accessible, inspected: 0, incomplete: false, conflicts: Vec::new(), note: "Only bounded /proc/PID/comm names were inspected; no command lines, PIDs or process environments are exported.".into() };
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            result.state = classify_error(&error);
            result.incomplete = true;
            return result;
        }
    };
    let mut counts = BTreeMap::new();
    for (index, entry) in entries.enumerate() {
        if index >= MAX_PROCESSES {
            result.incomplete = true;
            break;
        }
        let Ok(entry) = entry else {
            result.incomplete = true;
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        match read_bounded(&entry.path().join("comm"), 128) {
            Ok(name) => {
                result.inspected += 1;
                if let Some((canonical, explanation)) = conflict_name(&name) {
                    let entry = counts.entry(canonical).or_insert((0, explanation));
                    entry.0 += 1;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (), // normal process exit race
            Err(error) => {
                result.incomplete = true;
                if result.inspected == 0 {
                    result.state = classify_error(&error);
                }
            }
        }
    }
    if result.inspected > 0 {
        result.state = State::Accessible;
    }
    for (name, (instances, explanation)) in counts {
        result.conflicts.push(Conflict {
            name: name.into(),
            instances,
            explanation: explanation.into(),
        });
    }
    result
}

fn inventory(scan: &ScanReport, include: bool, home: Option<&Path>) -> (Vec<Receiver>, bool) {
    let mut truncated = scan.receivers.len() > MAX_RECEIVERS;
    let receivers = scan
        .receivers
        .iter()
        .take(MAX_RECEIVERS)
        .enumerate()
        .map(|(receiver_index, receiver)| {
            truncated |= receiver.devices.len() > MAX_DEVICES;
            let optional = |text: &str| {
                include.then(|| {
                    redact_home(
                        &text
                            .chars()
                            .filter(|c| !c.is_control())
                            .take(160)
                            .collect::<String>(),
                        home,
                    )
                })
            };
            Receiver {
                reference: format!("receiver-{}", receiver_index + 1),
                product_id: receiver.product_id,
                direct: receiver.direct,
                name: optional(&receiver.name),
                serial: optional(&receiver.serial),
                path: optional(&receiver.path),
                devices: receiver
                    .devices
                    .iter()
                    .take(MAX_DEVICES)
                    .enumerate()
                    .map(|(index, device)| Device {
                        reference: format!("device-{}.{}", receiver_index + 1, index + 1),
                        online: device.online,
                        battery_percent: device.battery.filter(|value| *value <= 100),
                        feature_count: device.feature_count,
                        name: optional(&device.name),
                        serial: optional(&device.serial),
                    })
                    .collect(),
            }
        })
        .collect();
    (receivers, truncated)
}

/// Home paths never belong in a support export, even in identifier mode. Also
/// strips /home/USERNAME and /root when HOME is absent or a different user.
fn redact_home(text: &str, home: Option<&Path>) -> String {
    let mut text = text.to_owned();
    if let Some(home) = home.filter(|home| *home != Path::new("/")) {
        let home = home.to_string_lossy();
        if !home.is_empty() {
            text = text.replace(home.as_ref(), "$HOME");
        }
    }
    let mut output = String::new();
    let mut remainder = text.as_str();
    while let Some(index) = remainder.find("/home/") {
        output.push_str(&remainder[..index]);
        let tail = &remainder[index + 6..];
        let end = tail
            .find(['/', ' ', '\n', '\r', '\t', '"', '\''])
            .unwrap_or(tail.len());
        output.push_str("$HOME");
        remainder = &tail[end..];
    }
    output.push_str(remainder);
    let output = output.replace("/root/", "$HOME/");
    if output == "/root" {
        "$HOME".into()
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DeviceSnapshot, ReceiverSnapshot};
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn diagnostics_default_inventory_never_contains_names_serials_or_paths() {
        let scan = ScanReport {
            receivers: vec![ReceiverSnapshot {
                name: "Alice private receiver".into(),
                serial: "SECRET-RECEIVER".into(),
                path: "/home/alice/private/hidraw".into(),
                devices: vec![DeviceSnapshot {
                    name: "Alice secret mouse".into(),
                    serial: "SECRET-DEVICE".into(),
                    battery: Some(255),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            errors: vec!["permission denied /home/alice/SECRET-DEVICE".into()],
        };
        let (devices, _) = inventory(&scan, false, Some(Path::new("/home/alice")));
        let json = serde_json::to_string(&devices).unwrap();
        for secret in ["Alice", "SECRET", "/home/", "alice", "hidraw"] {
            assert!(!json.contains(secret), "leaked {secret}");
        }
        assert!(devices[0].devices[0].battery_percent.is_none());
        let (devices, _) = inventory(&scan, true, Some(Path::new("/home/alice")));
        let json = serde_json::to_string(&devices).unwrap();
        assert!(json.contains("SECRET-DEVICE"));
        assert!(!json.contains("/home/alice"));
        assert!(json.contains("$HOME/private/hidraw"));
    }

    #[test]
    fn diagnostics_home_redaction_and_probe_states_are_distinct() {
        assert_eq!(
            redact_home(
                "/home/alice/.bin/helper /home/bob/file /root/file",
                Some(Path::new("/home/alice"))
            ),
            "$HOME/.bin/helper $HOME/file $HOME/file"
        );
        assert_eq!(
            classify_error(&io::Error::from_raw_os_error(libc::EACCES)),
            State::PermissionDenied
        );
        assert_eq!(
            classify_error(&io::Error::from_raw_os_error(libc::ENOENT)),
            State::Absent
        );
        assert_eq!(
            classify_error(&io::Error::from_raw_os_error(libc::EIO)),
            State::Unavailable
        );
        assert!(hidraw_name("hidraw12"));
        assert!(!hidraw_name("hidraw../../etc/passwd"));
        assert!(!hidraw_name("hidraw"));
        assert!(is_logitech("HID_ID=0003:0000046D:0000C548\n"));
        assert!(!is_logitech("HID_ID=0003:00001234:0000C548\n"));
    }

    #[test]
    fn diagnostics_process_scan_uses_only_bounded_comm_metadata() {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "logicenter-diagnostics-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        for (pid, name) in [
            ("1", "solaar\n"),
            ("2", "logid\n"),
            ("3", "python3\n"),
            ("4", "solaar\n"),
        ] {
            fs::create_dir(root.join(pid)).unwrap();
            fs::write(root.join(pid).join("comm"), name).unwrap();
            fs::write(root.join(pid).join("cmdline"), "SECRET-COMMAND-LINE").unwrap();
        }
        let result = processes(&root);
        assert_eq!(result.inspected, 4);
        assert_eq!(result.conflicts.len(), 2);
        assert_eq!(
            result
                .conflicts
                .iter()
                .find(|conflict| conflict.name == "Solaar")
                .unwrap()
                .instances,
            2
        );
        assert!(!serde_json::to_string(&result).unwrap().contains("SECRET"));
        assert_eq!(processes(&root.join("missing")).state, State::Absent);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn diagnostics_inventory_is_bounded_and_home_env_is_never_mutated() {
        let scan = ScanReport {
            receivers: (0..50)
                .map(|_| ReceiverSnapshot {
                    devices: vec![DeviceSnapshot::default(); 50],
                    ..Default::default()
                })
                .collect(),
            errors: Vec::new(),
        };
        let (receivers, truncated) = inventory(&scan, false, None);
        assert!(truncated);
        assert_eq!(receivers.len(), MAX_RECEIVERS);
        assert!(
            receivers
                .iter()
                .all(|receiver| receiver.devices.len() == MAX_DEVICES)
        );
    }

    #[test]
    fn diagnostics_json_export_redacts_raw_errors_and_has_a_hard_limit() {
        let scan = ScanReport {
            receivers: vec![ReceiverSnapshot {
                serial: "SECRET-SERIAL".into(),
                name: "SECRET-NAME".into(),
                path: "/home/alice/SECRET-PATH".into(),
                ..Default::default()
            }],
            errors: vec!["permission denied SECRET-SERIAL /home/alice/RAW-LOG".into()],
        };
        let (receivers, _) = inventory(&scan, false, Some(Path::new("/home/alice")));
        let mut report = Report {
            schema_version: 1,
            generated_at_unix: 100,
            identifiers_included: false,
            access: Vec::new(),
            helpers: Vec::new(),
            desktop: Desktop {
                session_type: "unknown".into(),
                desktop: "Other/unknown".into(),
                display_present: false,
                wayland_display_present: false,
                session_bus_present: false,
                backends: Vec::new(),
            },
            processes: Processes {
                state: State::Unavailable,
                inspected: 0,
                incomplete: true,
                conflicts: Vec::new(),
                note: "fixture".into(),
            },
            receivers,
            scan_error_count: 1,
            scan_error_categories: error_categories(&scan.errors),
            truncated: false,
            notes: Vec::new(),
        };
        let json = String::from_utf8(report.to_json().unwrap()).unwrap();
        for secret in ["SECRET", "alice", "/home/", "RAW-LOG"] {
            assert!(!json.contains(secret), "leaked {secret}");
        }
        assert!(json.contains("permission")); // positive control: errors are summarized, not silently omitted
        report.notes.push("X".repeat(MAX_EXPORT_BYTES));
        assert!(report.to_json().is_err());
        let errors = vec!["raw log SECRET".into(); 300];
        assert_eq!(error_categories(&errors).values().sum::<usize>(), 256);
    }
}
