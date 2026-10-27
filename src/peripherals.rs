//! Optional peripheral adapters. Discovery never writes hardware or installs helpers.
//! All public setters re-discover their target: a JSON descriptor is not authority.
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

const TIMEOUT: Duration = Duration::from_secs(3);
const OUTPUT_LIMIT: usize = 256 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackendStatus {
    pub backend: String,
    pub available: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Inventory {
    pub devices: Vec<Peripheral>,
    pub backends: Vec<BackendStatus>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    GamingMouse,
    Camera,
    Light,
    Audio,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Peripheral {
    pub id: String,
    pub name: String,
    pub kind: Kind,
    pub backend: String,
    pub status: String,
    pub metadata: BTreeMap<String, String>,
    pub controls: Vec<Control>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ControlValue {
    Bool(bool),
    Integer(i64),
    Text(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Constraint {
    Toggle,
    Integer {
        min: i64,
        max: i64,
        step: i64,
        choices: Vec<i64>,
    },
    Choice {
        options: Vec<String>,
    },
    Rgb,
}

impl Constraint {
    pub fn validate(&self, value: &ControlValue) -> Result<()> {
        let valid = match (self, value) {
            (Self::Toggle, ControlValue::Bool(_)) => true,
            (
                Self::Integer {
                    min,
                    max,
                    step,
                    choices,
                },
                ControlValue::Integer(v),
            ) => {
                *step > 0
                    && min <= max
                    && v >= min
                    && v <= max
                    && (i128::from(*v) - i128::from(*min)) % i128::from(*step) == 0
                    && (choices.is_empty() || choices.contains(v))
            }
            (Self::Choice { options }, ControlValue::Text(v)) => options.contains(v),
            (Self::Rgb, ControlValue::Text(v)) => {
                v.len() == 6 && v.bytes().all(|b| b.is_ascii_hexdigit())
            }
            _ => false,
        };
        ensure!(
            valid,
            "Value does not satisfy the discovered control bounds/type/step"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Control {
    pub id: String,
    pub label: String,
    pub constraint: Constraint,
    pub current: Option<ControlValue>,
    pub note: String,
    #[serde(skip)]
    route: Option<Route>,
}

#[derive(Clone, Debug)]
enum Route {
    Ratbag(Vec<String>),
    Camera {
        path: PathBuf,
        name: String,
    },
    Litra {
        path: PathBuf,
        model: LitraModel,
        command: u8,
    },
    Audio {
        name: String,
        source: bool,
        mute: bool,
    },
}

fn control(
    id: String,
    label: String,
    constraint: Constraint,
    current: Option<ControlValue>,
    route: Route,
) -> Control {
    Control {
        id,
        label,
        constraint,
        current,
        note: String::new(),
        route: Some(route),
    }
}

fn integer(min: i64, max: i64, step: i64) -> Constraint {
    Constraint::Integer {
        min,
        max,
        step,
        choices: Vec::new(),
    }
}

fn choices(values: Vec<i64>) -> Option<Constraint> {
    Some(Constraint::Integer {
        min: *values.iter().min()?,
        max: *values.iter().max()?,
        step: 1,
        choices: values,
    })
}

fn helper(name: &str) -> Result<PathBuf> {
    find_helper(name, std::env::var_os("PATH").as_deref())
        .with_context(|| format!("{name} is unavailable. Install it with your distribution's package manager if you want this optional backend; Logi Center never installs helpers."))
}

fn find_helper(name: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .filter(|p| p.is_absolute())
        .map(|p| p.join(name))
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0))
}

/// Process group ownership ensures timed-out helpers (and their children) are killed/reaped.
struct OwnedChild {
    child: Child,
    reaped: bool,
}
impl OwnedChild {
    fn new(child: Child) -> Self {
        Self {
            child,
            reaped: false,
        }
    }
    fn exited(&mut self) -> Result<bool> {
        // WNOWAIT retains the leader's PID until cleanup: group signaling cannot
        // accidentally target a recycled PID after a normal try_wait() reap.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                self.reaped = true;
            }
            return Err(error.into());
        }
        Ok(unsafe { info.si_pid() } != 0)
    }
    fn finish(&mut self) -> Result<std::process::ExitStatus> {
        if !self.reaped {
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
        }
        let status = self.child.wait()?;
        self.reaped = true;
        Ok(status)
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        // Each child is spawned into its own process group, never the application's group.
        if !self.reaped {
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn nonblocking(file: &impl AsRawFd) -> Result<()> {
    let fd = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(
        flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
        "Could not make helper output nonblocking"
    );
    Ok(())
}

fn drain(reader: &mut impl Read, output: &mut Vec<u8>) -> Result<()> {
    let mut bytes = [0; 4096];
    loop {
        match reader.read(&mut bytes) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                ensure!(
                    output.len() + n <= OUTPUT_LIMIT,
                    "Helper output exceeded 256 KiB"
                );
                output.extend_from_slice(&bytes[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
}

fn run_at(program: &Path, args: &[String]) -> Result<String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    parent_death_signal(&mut command);
    let mut child = OwnedChild::new(
        command
            .spawn()
            .with_context(|| format!("Could not start {}", program.display()))?,
    );
    let mut stdout = child.child.stdout.take().context("Missing helper stdout")?;
    let mut stderr = child.child.stderr.take().context("Missing helper stderr")?;
    nonblocking(&stdout)?;
    nonblocking(&stderr)?;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let start = Instant::now();
    loop {
        drain(&mut stdout, &mut out)?;
        drain(&mut stderr, &mut err)?;
        if child.exited()? {
            drain(&mut stdout, &mut out)?;
            drain(&mut stderr, &mut err)?;
            let status = child.finish()?;
            ensure!(
                status.success(),
                "{} failed: {}{}",
                program.display(),
                String::from_utf8_lossy(&err),
                String::from_utf8_lossy(&out)
            );
            return String::from_utf8(out).context("Helper output is not UTF-8");
        }
        ensure!(
            start.elapsed() < TIMEOUT,
            "{} timed out after 3 seconds",
            program.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn run(name: &str, args: &[String]) -> Result<String> {
    run_at(&helper(name)?, args)
}
fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).into()).collect()
}

fn backend(name: &str, work: impl FnOnce() -> Result<Vec<Peripheral>>) -> Inventory {
    match work() {
        Ok(devices) => Inventory {
            devices,
            backends: vec![BackendStatus {
                backend: name.into(),
                available: true,
                detail: "Discovery succeeded; only explicitly listed controls are supported."
                    .into(),
            }],
        },
        Err(e) => Inventory {
            devices: vec![],
            backends: vec![BackendStatus {
                backend: name.into(),
                available: false,
                detail: format!("{e:#}"),
            }],
        },
    }
}

/// Bounded, read-only discovery. Call on a worker thread, not the GTK main thread.
pub fn scan() -> Inventory {
    std::thread::scope(|scope| {
        let workers = [
            scope.spawn(|| backend("ratbagctl / ratbagd", scan_mice)),
            scope.spawn(|| backend("v4l2-ctl", scan_cameras)),
            scope.spawn(|| backend("Litra hidraw", scan_lights)),
            scope.spawn(|| backend("pactl (generic audio)", scan_audio)),
        ];
        let mut result = Inventory::default();
        for worker in workers {
            match worker.join() {
                Ok(mut inventory) => {
                    result.devices.append(&mut inventory.devices);
                    result.backends.append(&mut inventory.backends);
                }
                Err(_) => result.backends.push(BackendStatus {
                    backend: "discovery worker".into(),
                    available: false,
                    detail: "Worker failed".into(),
                }),
            }
        }
        let executable = helper("deskflow-server").or_else(|_| helper("deskflow-core"));
        result.backends.push(BackendStatus { backend: "Deskflow".into(), available: executable.is_ok(), detail: executable.map_or_else(|_| "Optional: choose an existing trusted Deskflow server/core executable and configuration below. No install or network connection happens during discovery.".into(), |p| format!("Found {}. A trusted configuration and explicit security acknowledgement are required. Not Logitech Flow.", p.display())) });
        result
    })
}

/// Camera-only inventory for the main window; no optional audio/gaming helpers
/// or video streams are started. Run on a worker thread.
pub fn cameras() -> Inventory {
    backend("v4l2-ctl", scan_cameras)
}

/// Keep selection on the physical USB camera if its video-node number changes.
pub fn camera_key(device: &Peripheral) -> String {
    device
        .metadata
        .get("usb_identity")
        .map(|identity| format!("camera-usb:{identity}"))
        .unwrap_or_else(|| device.id.clone())
}

/// Validate the current USB identity before opening a preview, not just a cached
/// /dev/video number. This performs no control writes and never opens audio.
pub fn camera_preview_source(device: &Peripheral) -> Result<(PathBuf, PathBuf)> {
    ensure!(device.kind == Kind::Camera, "Not a camera");
    let path = PathBuf::from(
        device
            .id
            .strip_prefix("camera:")
            .context("Invalid camera ID")?,
    );
    let current = camera_identity(&path)?;
    ensure!(
        camera_key(&current) == camera_key(device),
        "Camera identity changed; Refresh devices before previewing"
    );
    Ok((helper("ffmpeg")?, path))
}

#[derive(Default, Debug)]
struct MouseProfile {
    index: u32,
    active: bool,
    disabled: bool,
    rate: Option<i64>,
    resolutions: Vec<(u32, Option<i64>, bool)>,
    buttons: Vec<(u32, i64)>,
    leds: Vec<(u32, String, String, Option<String>)>,
}

fn parse_mouse_info(text: &str) -> Vec<MouseProfile> {
    let mut profiles: Vec<MouseProfile> = vec![];
    let mut in_resolutions = false;
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("Profile ") {
            if let Some((index, flags)) = rest.split_once(':') {
                if let Ok(index) = index.parse() {
                    profiles.push(MouseProfile {
                        index,
                        active: flags.contains("(active)"),
                        disabled: flags.contains("(disabled)"),
                        ..Default::default()
                    });
                }
            }
            in_resolutions = false;
            continue;
        }
        let Some(p) = profiles.last_mut() else {
            continue;
        };
        if let Some(rate) = line.strip_prefix("Report Rate: ") {
            p.rate = rate.trim_end_matches("Hz").parse().ok();
        }
        if line == "Resolutions:" {
            in_resolutions = true;
            continue;
        }
        if in_resolutions {
            if let Some((index, value)) = line.split_once(": ") {
                if let Ok(index) = index.parse::<u32>() {
                    let dpi = value.split("dpi").next().unwrap_or("");
                    let dpi = dpi.split('x').next().and_then(|v| v.parse::<i64>().ok());
                    if !value.contains("disabled") {
                        p.resolutions.push((index, dpi, value.contains("(active)")));
                    }
                    continue;
                }
            }
            in_resolutions = false;
        }
        if let Some(button) = line.strip_prefix("Button: ") {
            if let Some((index, mapping)) = button.split_once(" is mapped to 'button ") {
                if let (Ok(index), Ok(mapping)) =
                    (index.parse(), mapping.trim_end_matches('\'').parse())
                {
                    p.buttons.push((index, mapping));
                }
            }
        }
        if let Some(led) = line.strip_prefix("LED: ") {
            let fields: Vec<_> = led.split(", ").collect();
            if let (Some(index), Some(depth), Some(mode)) = (
                fields.first().and_then(|s| s.parse().ok()),
                fields.iter().find_map(|s| s.strip_prefix("depth: ")),
                fields.iter().find_map(|s| s.strip_prefix("mode: ")),
            ) {
                p.leds.push((
                    index,
                    depth.into(),
                    mode.into(),
                    fields
                        .iter()
                        .find_map(|s| s.strip_prefix("color: "))
                        .map(str::to_owned),
                ));
            }
        }
    }
    profiles
}

fn parse_numbers(text: &str) -> Vec<i64> {
    let mut values: Vec<i64> = text
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .filter(|v| *v > 0 && *v <= 100_000)
        .take(20_000)
        .collect();
    values.sort_unstable();
    values.dedup();
    values
}

fn ratbag_list(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let (id, name) = line.split_once(':')?;
            (safe_identifier(id.trim()) && !name.trim().is_empty())
                .then(|| (id.trim().into(), name.trim().into()))
        })
        .take(8)
        .collect()
}

fn safe_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 240
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}

fn scan_mice() -> Result<Vec<Peripheral>> {
    scan_mice_for(None)
}

fn scan_mice_for(target: Option<&str>) -> Result<Vec<Peripheral>> {
    let list = run("ratbagctl", &args(&["list"]))?;
    let mut devices = vec![];
    let started = Instant::now();
    let mut budget = 96usize;
    let mut query = |args: &[String]| -> Result<String> {
        ensure!(
            budget > 0 && started.elapsed() < Duration::from_secs(12),
            "Gaming discovery budget reached; some controls are omitted. Retry with fewer devices or resolve slow ratbagd replies."
        );
        budget -= 1;
        run("ratbagctl", args)
    };
    for (id, name) in ratbag_list(&list)
        .into_iter()
        .filter(|(id, _)| target.is_none_or(|target| target == id))
    {
        let info = query(&args(&[&id, "info"]));
        let mut device = Peripheral { id: format!("mouse:{id}"), name, kind: Kind::GamingMouse, backend: "ratbagctl / ratbagd".into(), status: "libratbag-selected device; helper performs final hardware capability checks. Button remapping is limited to existing button-type mappings.".into(), metadata: BTreeMap::new(), controls: vec![] };
        let info = match info {
            Ok(info) => info,
            Err(e) => {
                device.status = e.to_string();
                devices.push(device);
                continue;
            }
        };
        device.metadata.insert("info".into(), info.clone());
        let profiles = parse_mouse_info(&info);
        let enabled: Vec<_> = profiles.iter().filter(|p| !p.disabled).collect();
        if enabled.len() > 1 {
            if let Some(constraint) = choices(enabled.iter().map(|p| i64::from(p.index)).collect())
            {
                device.controls.push(control(
                    "profile".into(),
                    "Active onboard profile".into(),
                    constraint,
                    enabled
                        .iter()
                        .find(|p| p.active)
                        .map(|p| ControlValue::Integer(p.index.into())),
                    Route::Ratbag(args(&[&id, "profile", "active", "set"])),
                ));
            }
        }
        for p in enabled.into_iter().take(8) {
            let prefix = args(&[&id, "profile", &p.index.to_string()]);
            if p.resolutions.len() > 1 {
                if let Some(constraint) =
                    choices(p.resolutions.iter().map(|r| r.0.into()).collect())
                {
                    let mut command = prefix.clone();
                    command.extend(args(&["resolution", "active", "set"]));
                    device.controls.push(control(
                        format!("p{}.resolution", p.index),
                        format!("Profile {} · active DPI slot", p.index),
                        constraint,
                        p.resolutions
                            .iter()
                            .find(|r| r.2)
                            .map(|r| ControlValue::Integer(r.0.into())),
                        Route::Ratbag(command),
                    ));
                }
            }
            for (slot, current, _) in p.resolutions.iter().take(16) {
                let mut command = prefix.clone();
                command.extend(args(&["resolution", &slot.to_string(), "dpi", "get-all"]));
                if let Ok(dpis) = query(&command) {
                    if let Some(constraint) = choices(parse_numbers(&dpis)) {
                        command.pop();
                        command.push("set".into());
                        device.controls.push(control(
                            format!("p{}.dpi{slot}", p.index),
                            format!("Profile {} · DPI slot {slot} (both axes)", p.index),
                            constraint,
                            current.map(ControlValue::Integer),
                            Route::Ratbag(command),
                        ));
                    }
                }
            }
            let mut rate = prefix.clone();
            rate.extend(args(&["rate", "get-all"]));
            if let Ok(rates) = query(&rate) {
                if let Some(constraint) = choices(parse_numbers(&rates)) {
                    rate.pop();
                    rate.push("set".into());
                    device.controls.push(control(
                        format!("p{}.rate", p.index),
                        format!("Profile {} · report rate (Hz)", p.index),
                        constraint,
                        p.rate.map(ControlValue::Integer),
                        Route::Ratbag(rate),
                    ));
                }
            }
            // Only an existing BUTTON action establishes this action type without a D-Bus API dependency.
            let mut mappings: Vec<_> = p
                .buttons
                .iter()
                .map(|(_, mapping)| *mapping)
                .filter(|mapping| (1..=32).contains(mapping))
                .collect();
            mappings.sort_unstable();
            mappings.dedup();
            for (button, current) in p.buttons.iter().take(32) {
                if let Some(constraint) = choices(mappings.clone()) {
                    let mut command = prefix.clone();
                    command.extend(args(&[
                        "button",
                        &button.to_string(),
                        "action",
                        "set",
                        "button",
                    ]));
                    device.controls.push(control(
                        format!("p{}.button{button}", p.index),
                        format!("Profile {} · button {button} → button number", p.index),
                        constraint,
                        Some(ControlValue::Integer(*current)),
                        Route::Ratbag(command),
                    ));
                }
            }
            for (led, depth, mode, color) in p.leds.iter().take(8) {
                let mut command = prefix.clone();
                command.extend(args(&["led", &led.to_string(), "capabilities"]));
                if let Ok(caps) = query(&command) {
                    let options: Vec<_> = caps
                        .lines()
                        .find_map(|s| s.trim().strip_prefix("Modes: "))
                        .unwrap_or("")
                        .split(", ")
                        .filter(|s| ["off", "on", "cycle", "breathing"].contains(s))
                        .map(str::to_owned)
                        .collect();
                    if !options.is_empty() {
                        command.pop();
                        command.extend(args(&["set", "mode"]));
                        device.controls.push(control(
                            format!("p{}.led{led}.mode", p.index),
                            format!("Profile {} · LED {led} mode", p.index),
                            Constraint::Choice { options },
                            Some(ControlValue::Text(mode.clone())),
                            Route::Ratbag(command),
                        ));
                    }
                }
                if depth == "rgb" && ["on", "breathing"].contains(&mode.as_str()) {
                    let mut command = prefix.clone();
                    command.extend(args(&["led", &led.to_string(), "set", "color"]));
                    device.controls.push(control(
                        format!("p{}.led{led}.color", p.index),
                        format!("Profile {} · LED {led} RGB (RRGGBB)", p.index),
                        Constraint::Rgb,
                        color.clone().map(ControlValue::Text),
                        Route::Ratbag(command),
                    ));
                }
            }
        }
        devices.push(device);
    }
    if budget == 0 || started.elapsed() >= Duration::from_secs(12) {
        for device in &mut devices {
            device.status.push_str(" Discovery budget reached: only successfully queried controls are shown; try refreshing after resolving slow ratbagd replies.");
        }
    }
    Ok(devices)
}

fn numbered_node(path: &Path, prefix: &str) -> bool {
    path.parent() == Some(Path::new("/dev"))
        && path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix(prefix))
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}

fn validate_node(path: &Path, prefix: &str) -> Result<()> {
    ensure!(
        numbered_node(path, prefix),
        "Only /dev/{prefix}N device nodes are accepted"
    );
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_char_device(),
        "Device is not a non-symlink character device"
    );
    Ok(())
}

fn logitech_usb_ancestor(device: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(device)?;
    for ancestor in canonical.ancestors().take(12) {
        if let Ok(vendor) = fs::read_to_string(ancestor.join("idVendor")) {
            ensure!(
                vendor.trim().eq_ignore_ascii_case("046d"),
                "Not a Logitech USB device"
            );
            return Ok(ancestor.to_owned());
        }
    }
    bail!("No Logitech USB vendor identity in sysfs")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CameraControl {
    pub name: String,
    pub min: i64,
    pub max: i64,
    pub step: i64,
    pub current: i64,
    pub menu: BTreeMap<i64, String>,
    pub writable: bool,
}

/// Parses v4l2-ctl --list-ctrls-menus, ignoring unsupported compound/string controls.
pub fn parse_camera_controls(text: &str) -> Vec<CameraControl> {
    let mut result: Vec<CameraControl> = vec![];
    for line in text.lines() {
        let line = line.trim();
        if line.contains(" 0x") && line.contains(" : ") {
            let words: Vec<_> = line.split_whitespace().collect();
            let name = words.first().copied().unwrap_or("");
            let kind = words
                .iter()
                .find(|s| s.starts_with('('))
                .copied()
                .unwrap_or("");
            if !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                || !["(int)", "(bool)", "(menu)", "(intmenu)"].contains(&kind)
            {
                continue;
            }
            let number = |key: &str| {
                words
                    .iter()
                    .find_map(|s| s.strip_prefix(key).and_then(|v| v.parse::<i64>().ok()))
            };
            let Some(current) = number("value=") else {
                continue;
            };
            let (min, max) = if kind == "(bool)" {
                (0, 1)
            } else {
                let (Some(min), Some(max)) = (number("min="), number("max=")) else {
                    continue;
                };
                (min, max)
            };
            let step = number("step=").unwrap_or(1);
            if min > max || step <= 0 {
                continue;
            }
            result.push(CameraControl {
                name: name.into(),
                min,
                max,
                step,
                current,
                menu: BTreeMap::new(),
                writable: !line.contains("read-only")
                    && !line.contains("inactive")
                    && !line.contains("disabled")
                    && !line.contains("grabbed"),
            });
        } else if let (Some(c), Some((key, name))) = (result.last_mut(), line.split_once(": ")) {
            if let Ok(key) = key.trim().parse::<i64>() {
                c.menu.insert(key, name.into());
            }
        }
    }
    result
}

fn camera_identity(path: &Path) -> Result<Peripheral> {
    validate_node(path, "video")?;
    let node = path.file_name().context("Missing node")?;
    let sys = Path::new("/sys/class/video4linux").join(node);
    let usb = logitech_usb_ancestor(&sys.join("device"))?;
    let name = fs::read_to_string(sys.join("name"))
        .unwrap_or_else(|_| node.to_string_lossy().into())
        .trim()
        .to_owned();
    let usb_identity = ["idVendor", "idProduct", "serial"]
        .iter()
        .map(|key| {
            fs::read_to_string(usb.join(key))
                .unwrap_or_default()
                .trim()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join(":");
    Ok(Peripheral { id: format!("camera:{}", path.display()), name, kind: Kind::Camera, backend: "v4l2-ctl".into(), status: "Actual V4L2 controls; inactive/manual controls may require disabling auto focus/exposure/white balance, then refreshing. No stream is started.".into(), metadata: BTreeMap::from([("usb_sysfs".into(), usb.display().to_string()), ("usb_identity".into(), format!("{}:{usb_identity}", usb.display()))]), controls: vec![] })
}

fn camera_at(path: &Path) -> Result<Peripheral> {
    let mut device = camera_identity(path)?;
    let text = run(
        "v4l2-ctl",
        &args(&[
            "--device",
            path.to_str().context("Device path is not UTF-8")?,
            "--list-ctrls-menus",
        ]),
    )?;
    for c in parse_camera_controls(&text) {
        if !c.writable {
            continue;
        }
        let constraint = Constraint::Integer {
            min: c.min,
            max: c.max,
            step: c.step,
            choices: c.menu.keys().copied().collect(),
        };
        let mut ctl = control(
            c.name.clone(),
            c.name.replace('_', " "),
            constraint,
            Some(ControlValue::Integer(c.current)),
            Route::Camera {
                path: path.into(),
                name: c.name,
            },
        );
        if !c.menu.is_empty() {
            ctl.note = c
                .menu
                .iter()
                .map(|(v, label)| format!("{v}: {label}"))
                .collect::<Vec<_>>()
                .join("; ");
        }
        device.controls.push(ctl);
    }
    Ok(device)
}

fn video_number(id: &str) -> u32 {
    id.rsplit("video")
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(u32::MAX)
}

/// UVC cameras expose capture, IR and metadata nodes for the same USB device.
/// Prefer a node with usable controls, then the lowest node number (normally
/// the primary capture interface). Never group by model name or serial alone:
/// two identical cameras on different USB ports must remain separate.
fn group_cameras(mut devices: Vec<Peripheral>) -> Vec<Peripheral> {
    devices.sort_by_key(|device| video_number(&device.id));
    let mut grouped: BTreeMap<String, Peripheral> = BTreeMap::new();
    for device in devices {
        let key = camera_key(&device);
        match grouped.entry(key) {
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if device.controls.len() > entry.get().controls.len() {
                    entry.insert(device);
                }
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(device);
            }
        }
    }
    grouped.into_values().collect()
}

fn scan_cameras() -> Result<Vec<Peripheral>> {
    helper("v4l2-ctl")?;
    let entries = match fs::read_dir("/sys/class/video4linux") {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let mut devices = vec![];
    let started = Instant::now();
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| video_number(&entry.file_name().to_string_lossy()));
    for entry in entries.into_iter().take(32) {
        if started.elapsed() >= Duration::from_secs(12) {
            break;
        }
        if logitech_usb_ancestor(&entry.path().join("device")).is_err() {
            continue;
        }
        let path = Path::new("/dev").join(entry.file_name());
        match camera_at(&path) {
            Ok(device) => devices.push(device),
            Err(e) => {
                // A missing permission must not hide the camera or turn every
                // one of its metadata nodes into an unnamed duplicate.
                if let Ok(mut device) = camera_identity(&path) {
                    device.status = format!("Camera controls unavailable: {e:#}");
                    devices.push(device);
                }
            }
        }
    }
    Ok(group_cameras(devices))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LitraModel {
    Glow,
    Beam,
}

/* Litra identity, ranges and report encoding below are adapted from local OpenLogi
 * crates/openlogi-device/src/write/litra.rs and openlogi-device-registry/src/litra.rs,
 * https://github.com/AprilNEA/OpenLogi (workspace version 0.8.17), under its MIT option.
 * MIT License
 * Copyright (c) 2026 AprilNEA
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */
fn litra_model(vendor: u16, product: u16) -> Option<LitraModel> {
    match (vendor, product) {
        (0x046d, 0xc900) => Some(LitraModel::Glow),
        (0x046d, 0xc901) => Some(LitraModel::Beam),
        _ => None,
    }
}

/// Recognize the application collection, usage page/usage and writable report, not VID alone.
fn litra_collection(bytes: &[u8]) -> bool {
    let (mut page, mut usage, mut report, mut size, mut count) = (0u32, 0u32, 0u32, 0u32, 0u32);
    let mut globals = vec![];
    let mut collections = vec![];
    let mut bits = 0u32;
    let mut i = 0;
    while i < bytes.len() {
        let prefix = bytes[i];
        i += 1;
        if prefix == 0xfe {
            return false;
        } // fail closed on unknown long items
        let len = match prefix & 3 {
            3 => 4,
            n => n as usize,
        };
        if i + len > bytes.len() {
            return false;
        }
        let value = bytes[i..i + len]
            .iter()
            .enumerate()
            .fold(0u32, |v, (n, b)| v | (u32::from(*b) << (n * 8)));
        i += len;
        match prefix & 0xfc {
            0x04 => page = value,
            0x08 => usage = value,
            0x74 => size = value,
            0x84 => report = value,
            0x94 => count = value,
            0xa4 => globals.push((page, report, size, count)),
            0xb4 => {
                let Some(saved) = globals.pop() else {
                    return false;
                };
                (page, report, size, count) = saved;
            }
            0xa0 => {
                collections.push(
                    collections.last().copied().unwrap_or(false)
                        || (page == 0xff43 && usage == 0x0202 && value == 1),
                );
                usage = 0;
            }
            0xc0 => {
                if collections.pop().is_none() {
                    return false;
                }
                usage = 0;
            }
            0x90 if collections.last() == Some(&true) && report == 0x11 => {
                bits = bits.saturating_add(size.saturating_mul(count));
                usage = 0;
            }
            0x80 | 0x90 | 0xb0 => usage = 0,
            _ => {}
        }
        if collections.len() > 32 || globals.len() > 32 {
            return false;
        }
    }
    collections.is_empty() && bits == 19 * 8
}

fn light_identity(path: &Path) -> Result<LitraModel> {
    validate_node(path, "hidraw")?;
    let sys = Path::new("/sys/class/hidraw")
        .join(path.file_name().context("Missing HID node")?)
        .join("device");
    let uevent = fs::read_to_string(sys.join("uevent"))?;
    let identity = uevent
        .lines()
        .find_map(|s| s.strip_prefix("HID_ID="))
        .context("Missing HID identity")?;
    let parts: Vec<_> = identity.split(':').collect();
    ensure!(
        parts.len() == 3 && u32::from_str_radix(parts[0], 16)? == 3,
        "Litra requires a USB HID interface"
    );
    let model = litra_model(
        u16::from_str_radix(parts[1].trim_start_matches('0'), 16).unwrap_or(0),
        u16::from_str_radix(parts[2].trim_start_matches('0'), 16).unwrap_or(0),
    )
    .context("Not an allowlisted Litra Glow/Beam")?;
    let descriptor = fs::read(sys.join("report_descriptor"))?;
    ensure!(
        descriptor.len() <= 16384 && litra_collection(&descriptor),
        "Not the verified Litra vendor collection/output report"
    );
    Ok(model)
}

fn light_at(path: &Path) -> Result<Peripheral> {
    let model = light_identity(path)?;
    let (name, min, max) = match model {
        LitraModel::Glow => ("Litra Glow", 20, 250),
        LitraModel::Beam => ("Litra Beam", 30, 400),
    };
    let route = |command| Route::Litra {
        path: path.into(),
        model,
        command,
    };
    Ok(Peripheral { id: format!("light:{}", path.display()), name: name.into(), kind: Kind::Light, backend: "Litra hidraw".into(), status: "Verified OpenLogi fixed reports; write-only (no state/readback acknowledgement). Permission is checked on Apply. Beam LX/RGB and other lights are not supported.".into(), metadata: BTreeMap::new(), controls: vec![
        control("power".into(), "Power".into(), Constraint::Toggle, None, route(0x1c)),
        control("brightness".into(), "Brightness (native lumens)".into(), integer(min, max, 1), None, route(0x4c)),
        control("temperature".into(), "Temperature (kelvin)".into(), integer(2700, 6500, 100), None, route(0x9c)),
    ] })
}

fn scan_lights() -> Result<Vec<Peripheral>> {
    let entries = match fs::read_dir("/sys/class/hidraw") {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let mut devices = vec![];
    for entry in entries.flatten().take(256) {
        let path = Path::new("/dev").join(entry.file_name());
        let uevent = fs::read_to_string(entry.path().join("device/uevent")).unwrap_or_default();
        let candidate = uevent
            .lines()
            .find_map(|s| s.strip_prefix("HID_ID="))
            .is_some_and(|id| {
                let parts: Vec<_> = id.split(':').collect();
                parts.len() == 3
                    && u32::from_str_radix(parts[0], 16).ok() == Some(3)
                    && litra_model(
                        u16::from_str_radix(parts[1].trim_start_matches('0'), 16).unwrap_or(0),
                        u16::from_str_radix(parts[2].trim_start_matches('0'), 16).unwrap_or(0),
                    )
                    .is_some()
            });
        if !candidate {
            continue;
        }
        match light_at(&path) {
            Ok(light) => devices.push(light),
            Err(error) => devices.push(Peripheral { id: format!("light:{}", path.display()), name: "Litra candidate (interface not verified)".into(), kind: Kind::Light, backend: "Litra hidraw".into(), status: format!("No controls enabled: {error:#}. Only the vendor application collection with report 0x11 is writable through this adapter."), metadata: BTreeMap::new(), controls: vec![] }),
        }
    }
    Ok(devices)
}

fn litra_report(model: LitraModel, command: u8, value: &ControlValue) -> Result<[u8; 20]> {
    let mut report = [0; 20];
    report[..4].copy_from_slice(&[0x11, 0xff, 0x04, command]);
    match (command, value) {
        (0x1c, ControlValue::Bool(v)) => report[4] = u8::from(*v),
        (0x4c, ControlValue::Integer(v)) => {
            let (min, max) = if model == LitraModel::Glow {
                (20, 250)
            } else {
                (30, 400)
            };
            integer(min, max, 1).validate(value)?;
            report[4..6].copy_from_slice(&(*v as u16).to_be_bytes());
        }
        (0x9c, ControlValue::Integer(v)) => {
            integer(2700, 6500, 100).validate(value)?;
            report[4..6].copy_from_slice(&(*v as u16).to_be_bytes());
        }
        _ => bail!("Unsupported Litra command/value"),
    }
    Ok(report)
}

fn write_light(path: &Path, model: LitraModel, command: u8, value: &ControlValue) -> Result<()> {
    ensure!(
        light_identity(path)? == model,
        "Light identity changed; refresh inventory"
    );
    let before = fs::symlink_metadata(path)?;
    let mut file = OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path).context("Cannot open light; check the device's hidraw permission/udev policy (do not run the whole UI as root)")?;
    ensure!(
        file.metadata()?.rdev() == before.rdev()
            && file.metadata()?.file_type().is_char_device()
            && light_identity(path)? == model,
        "Light node changed during open"
    );
    let report = litra_report(model, command, value)?;
    let start = Instant::now();
    loop {
        match file.write(&report) {
            Ok(20) => return Ok(()),
            Ok(_) => bail!("Short Litra output report; state is unknown"),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::Interrupted =>
            {
                ensure!(
                    start.elapsed() < TIMEOUT,
                    "Litra write timed out; state is unknown"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e).context("Litra write failed; state is unknown"),
        }
    }
}

fn parse_audio(text: &str, source: bool) -> Result<Vec<Peripheral>> {
    let entries: serde_json::Value =
        serde_json::from_str(text).context("pactl must support --format=json")?;
    let entries = entries.as_array().context("Expected pactl JSON array")?;
    let mut result = vec![];
    for e in entries.iter().take(128) {
        let props = &e["properties"];
        let vendor = props["device.vendor.id"]
            .as_str()
            .or_else(|| props["device.vendor_id"].as_str())
            .unwrap_or("");
        if !["046d", "0x046d", "1133"].contains(&vendor.to_ascii_lowercase().as_str()) {
            continue;
        }
        if source
            && (e["monitor_of_sink"].as_str().is_some_and(|s| s != "n/a")
                || e["name"].as_str().is_some_and(|s| s.ends_with(".monitor")))
        {
            continue;
        }
        let Some(name) = e["name"].as_str().filter(|s| safe_identifier(s)) else {
            continue;
        };
        let mut volume = vec![];
        if let Some(channels) = e["volume"].as_object() {
            for channel in channels.values() {
                if let Some(percent) = channel["value_percent"]
                    .as_str()
                    .and_then(|s| s.trim_end_matches('%').parse::<i64>().ok())
                    .filter(|v| (0..=1000).contains(v))
                {
                    volume.push(percent);
                }
            }
        }
        let role = if source { "source" } else { "sink" };
        let route = |mute| Route::Audio {
            name: name.into(),
            source,
            mute,
        };
        result.push(Peripheral { id: format!("audio:{role}:{name}"), name: format!("{} ({role})", e["description"].as_str().unwrap_or(name)), kind: Kind::Audio, backend: "pactl (generic audio)".into(), status: "Generic system audio-server controls only, not hardware EQ/sidetone/surround. Volume is capped at 100%; Apply sets all channels equally.".into(), metadata: BTreeMap::from([("audio_name".into(), name.into())]), controls: vec![
            control("mute".into(), "Mute".into(), Constraint::Toggle, e["mute"].as_bool().map(ControlValue::Bool), route(true)),
            control("volume".into(), "Volume (%)".into(), integer(0, 100, 1), (!volume.is_empty()).then(|| ControlValue::Integer(volume.iter().sum::<i64>() / volume.len() as i64)), route(false)),
        ] });
    }
    Ok(result)
}

fn scan_audio() -> Result<Vec<Peripheral>> {
    let mut devices = vec![];
    for source in [false, true] {
        let output = run(
            "pactl",
            &args(&[
                "--format=json",
                "list",
                if source { "sources" } else { "sinks" },
            ]),
        )?;
        devices.extend(parse_audio(&output, source)?);
    }
    Ok(devices)
}

/// Apply a single discovered control. Re-discovery prevents forged/stale descriptors
/// from bypassing path, capability or range validation. Does not install helpers.
pub fn apply(device_id: &str, control_id: &str, value: ControlValue) -> Result<String> {
    ensure!(
        device_id.len() <= 2048 && control_id.len() <= 256,
        "Peripheral/control ID exceeds bounds"
    );
    static LOCKS: std::sync::LazyLock<
        std::sync::Mutex<BTreeMap<String, std::sync::Weak<std::sync::Mutex<()>>>>,
    > = std::sync::LazyLock::new(|| std::sync::Mutex::new(BTreeMap::new()));
    let lock = {
        let mut locks = LOCKS
            .lock()
            .map_err(|_| anyhow::anyhow!("Peripheral lock unavailable"))?;
        locks.retain(|_, lock| lock.strong_count() > 0);
        let lock = locks
            .get(device_id)
            .and_then(std::sync::Weak::upgrade)
            .unwrap_or_else(|| std::sync::Arc::new(std::sync::Mutex::new(())));
        locks.insert(device_id.into(), std::sync::Arc::downgrade(&lock));
        lock
    };
    let _guard = lock
        .lock()
        .map_err(|_| anyhow::anyhow!("Device operation lock unavailable"))?;
    let devices = if let Some(id) = device_id.strip_prefix("mouse:") {
        scan_mice_for(Some(id))?
    } else if let Some(path) = device_id.strip_prefix("camera:") {
        vec![camera_at(Path::new(path))?]
    } else if let Some(path) = device_id.strip_prefix("light:") {
        vec![light_at(Path::new(path))?]
    } else if device_id.starts_with("audio:") {
        scan_audio()?
    } else {
        bail!("Unknown peripheral device ID");
    };
    let device = devices
        .iter()
        .find(|d| d.id == device_id)
        .context("Device is no longer available; refresh inventory")?;
    let control = device
        .controls
        .iter()
        .find(|c| c.id == control_id)
        .context("Control is no longer writable/supported; refresh inventory")?;
    control.constraint.validate(&value)?;
    match control
        .route
        .as_ref()
        .context("Missing validated control route")?
    {
        Route::Ratbag(command) => {
            let mut command = command.clone();
            command.push(value_argument(&value));
            run("ratbagctl", &command)?;
        }
        Route::Camera { path, name } => {
            validate_node(path, "video")?;
            logitech_usb_ancestor(
                &Path::new("/sys/class/video4linux")
                    .join(path.file_name().context("Missing node")?)
                    .join("device"),
            )?;
            run(
                "v4l2-ctl",
                &args(&[
                    "--device",
                    path.to_str().context("Non-UTF8 path")?,
                    "--set-ctrl",
                    &format!("{name}={}", value_argument(&value)),
                ]),
            )?;
        }
        Route::Litra {
            path,
            model,
            command,
        } => {
            write_light(path, *model, *command, &value)?;
            return Ok(
                "Output report written; raw Litra provides no readback or device acknowledgement."
                    .into(),
            );
        }
        Route::Audio { name, source, mute } => {
            let action = format!(
                "set-{}-{}",
                if *source { "source" } else { "sink" },
                if *mute { "mute" } else { "volume" }
            );
            let value = if *mute {
                value_argument(&value)
            } else {
                format!("{}%", value_argument(&value))
            };
            run("pactl", &args(&[&action, name, &value]))?;
        }
    }
    Ok("Helper accepted the setting. Refresh to read the device/server state.".into())
}

fn value_argument(value: &ControlValue) -> String {
    match value {
        ControlValue::Bool(v) => u8::from(*v).to_string(),
        ControlValue::Integer(v) => v.to_string(),
        ControlValue::Text(v) => v.clone(),
    }
}

pub type CameraChangeResults = Vec<(String, std::result::Result<ControlValue, String>)>;

/// One user submission, with honest per-setting results rather than pretending
/// V4L2 writes are atomic. Validate every value before any write, apply automatic
/// modes first, then read back each requested value before confirming it.
pub fn apply_camera_changes(
    device: &Peripheral,
    mut changes: Vec<(String, ControlValue)>,
) -> Result<CameraChangeResults> {
    ensure!(
        device.kind == Kind::Camera && changes.len() <= 64,
        "Invalid camera change set"
    );
    let path = Path::new(
        device
            .id
            .strip_prefix("camera:")
            .context("Not a camera ID")?,
    );
    let current = camera_at(path)?;
    ensure!(
        camera_key(&current) == camera_key(device),
        "Camera identity changed; refresh before applying settings"
    );
    for (id, value) in &changes {
        let control = current
            .controls
            .iter()
            .find(|control| control.id == *id)
            .with_context(|| format!("{id} is no longer writable; refresh camera settings"))?;
        control.constraint.validate(value)?;
    }
    changes.sort_by_key(|(name, _)| !name.contains("auto"));
    let started = Instant::now();
    let mut results = vec![];
    for (id, value) in changes {
        let outcome = (|| -> Result<ControlValue> {
            ensure!(
                started.elapsed() < Duration::from_secs(20),
                "Apply time limit reached; change remains pending"
            );
            let identity = camera_identity(path)?;
            ensure!(
                camera_key(&identity) == camera_key(device),
                "Camera identity changed; remaining changes were not sent"
            );
            let sent = apply(&device.id, &id, value.clone());
            let observed = camera_at(path)?;
            ensure!(
                camera_key(&observed) == camera_key(device),
                "Camera identity changed during Apply"
            );
            let actual = observed
                .controls
                .iter()
                .find(|control| control.id == id)
                .and_then(|control| control.current.as_ref());
            if actual == Some(&value) {
                return Ok(value);
            }
            sent?;
            bail!(
                "Device did not confirm the requested value; check its automatic mode and refresh"
            );
        })()
        .map_err(|error| format!("{error:#}"));
        results.push((id, outcome));
    }
    Ok(results)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraPreset {
    pub version: u32,
    pub name: String,
    pub usb_identity: String,
    pub values: BTreeMap<String, i64>,
}

fn preset_control(name: &str) -> bool {
    if ["relative", "speed", "reset"]
        .iter()
        .any(|part| name.contains(part))
    {
        return false;
    }
    [
        "focus",
        "exposure",
        "white_balance",
        "temperature",
        "brightness",
        "contrast",
        "saturation",
        "hue",
        "gamma",
        "gain",
        "sharpness",
        "backlight",
        "power_line",
        "rotate",
        "zoom",
        "pan",
        "tilt",
        "auto_exposure",
        "auto_white_balance",
        "auto_focus",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}

fn preset_path(name: &str) -> Result<PathBuf> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
        "Preset names must be 1–64 ASCII letters/digits/_/-"
    );
    Ok(crate::preferences::config_home()
        .context("No configuration directory")?
        .join("logicenter/camera-presets")
        .join(format!("{name}.json")))
}

/// Save current camera values explicitly. Presets never apply automatically.
pub fn save_camera_preset(device_id: &str, name: &str) -> Result<CameraPreset> {
    let destination = preset_path(name)?;
    let path = device_id
        .strip_prefix("camera:")
        .context("Not a camera ID")?;
    let camera = camera_at(Path::new(path))?;
    let preset = CameraPreset {
        version: 1,
        name: name.into(),
        usb_identity: camera
            .metadata
            .get("usb_identity")
            .cloned()
            .context("Missing USB identity")?,
        values: camera
            .controls
            .iter()
            .filter(|c| preset_control(&c.id))
            .filter_map(|c| match c.current {
                Some(ControlValue::Integer(v)) => Some((c.id.clone(), v)),
                Some(ControlValue::Bool(v)) => Some((c.id.clone(), i64::from(v))),
                _ => None,
            })
            .collect(),
    };
    ensure!(
        !preset.values.is_empty(),
        "No supported camera controls to save"
    );
    crate::preferences::atomic_write(&destination, &serde_json::to_vec_pretty(&preset)?, 0o600)?;
    Ok(preset)
}

fn saved_settings_name(device: &Peripheral) -> String {
    // Stable FNV-1a avoids putting USB paths, serials or port names in filenames.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in camera_key(device).bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("saved-{hash:016x}")
}

/// Persist the values read back from this physical camera, not unconfirmed edits.
pub fn save_camera_settings(device: &Peripheral) -> Result<CameraPreset> {
    let path = device
        .id
        .strip_prefix("camera:")
        .context("Not a camera ID")?;
    let current = camera_at(Path::new(path))?;
    ensure!(
        camera_key(&current) == camera_key(device),
        "Camera identity changed; saved settings were not overwritten"
    );
    save_camera_preset(&device.id, &saved_settings_name(device))
}

fn saved_camera_preset(device: &Peripheral) -> Result<Option<CameraPreset>> {
    let name = saved_settings_name(device);
    let path = preset_path(&name)?;
    if !path.try_exists()? {
        return Ok(None);
    }
    let preset = read_preset(&path, &name)?;
    ensure!(
        device.metadata.get("usb_identity") == Some(&preset.usb_identity),
        "Saved settings belong to a different USB camera/port"
    );
    Ok(Some(preset))
}

pub fn has_saved_camera_settings(device: &Peripheral) -> Result<bool> {
    Ok(saved_camera_preset(device)?.is_some())
}

/// Restore automatic modes before rediscovering and applying their manual controls.
pub fn apply_saved_camera_settings(
    device: &Peripheral,
) -> Result<BTreeMap<String, std::result::Result<String, String>>> {
    apply_camera_preset(&device.id, &saved_settings_name(device))
}

pub fn load_camera_preset(name: &str) -> Result<CameraPreset> {
    let path = preset_path(name)?;
    read_preset(&path, name)
}

fn read_preset(path: &Path, name: &str) -> Result<CameraPreset> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= 65536,
        "Invalid/oversized camera preset"
    );
    let preset: CameraPreset = serde_json::from_reader(file.take(65537))?;
    ensure!(
        preset.version == 1
            && preset.name == name
            && preset.values.len() <= 64
            && preset.values.keys().all(|s| preset_control(s)),
        "Unsupported/invalid camera preset"
    );
    Ok(preset)
}

/// Per-control partial results are intentional: automatic/manual mode changes can
/// alter availability. Apply auto controls first; every later control is rediscovered.
pub fn apply_camera_preset(
    device_id: &str,
    name: &str,
) -> Result<BTreeMap<String, std::result::Result<String, String>>> {
    let preset = load_camera_preset(name)?;
    let path = device_id
        .strip_prefix("camera:")
        .context("Not a camera ID")?;
    let camera = camera_at(Path::new(path))?;
    ensure!(
        camera.metadata.get("usb_identity") == Some(&preset.usb_identity),
        "Preset belongs to a different USB camera/port; it will not be applied"
    );
    let mut controls: Vec<_> = preset.values.into_iter().collect();
    controls.sort_by_key(|(name, _)| !(name.contains("auto") || name == "exposure_auto"));
    Ok(controls
        .into_iter()
        .map(|(name, value)| {
            let result = (|| -> Result<String> {
                // Automatic-mode changes can make later scalar controls writable.
                // Resolve each type after the earlier mode writes, not from the
                // stale inventory captured before applying the preset.
                let current = camera_at(Path::new(path))?;
                let control =
                    current.controls.iter().find(|c| c.id == name).context(
                        "Preset control is currently unavailable; check its automatic mode",
                    )?;
                let typed = camera_preset_value(&control.constraint, value)?;
                apply(device_id, &name, typed)
            })()
            .map_err(|e| format!("{e:#}"));
            (name, result)
        })
        .collect())
}

fn camera_preset_value(constraint: &Constraint, value: i64) -> Result<ControlValue> {
    let value = if matches!(constraint, Constraint::Toggle) {
        ensure!(
            value == 0 || value == 1,
            "Invalid camera automatic-mode value"
        );
        ControlValue::Bool(value == 1)
    } else {
        ControlValue::Integer(value)
    };
    constraint.validate(&value)?;
    Ok(value)
}

pub const DESKFLOW_SECURITY: &str = "Deskflow shares keyboard/mouse input over the network, not Logitech Flow. Use only a trusted executable/configuration and trusted LAN/VPN; enable TLS and verify peer certificate fingerprints in Deskflow, restrict the listening port with a firewall, and never expose it to the public Internet. No file/clipboard encryption guarantee is made by Logi Center. Stop/close this panel to terminate only the process launched here.";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeskflowConfig {
    pub executable: PathBuf,
    pub configuration: PathBuf,
    pub security_acknowledged: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeskflowStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub detail: String,
}

/// Owned server, not a PID file: never signals a foreign/reused PID from disk.
pub struct DeskflowSession {
    stop: Option<std::sync::mpsc::Sender<()>>,
    state: std::sync::Arc<std::sync::Mutex<DeskflowStatus>>,
    supervisor: Option<std::thread::JoinHandle<()>>,
}
impl DeskflowSession {
    pub fn status(&mut self) -> Result<DeskflowStatus> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Deskflow status lock unavailable"))?
            .clone())
    }
    pub fn stop(&mut self) {
        if let Some(sender) = self.stop.take() {
            let _ = sender.send(());
        }
        if let Some(worker) = self.supervisor.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for DeskflowSession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn parent_death_signal(command: &mut Command) {
    let parent = unsafe { libc::getpid() };
    // Linux binds PDEATHSIG to the creating *thread*. Short helpers are waited
    // in that same thread; a Deskflow server is created by its persistent
    // supervisor, never by a transient GTK task worker.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(std::io::Error::other("Parent exited before helper launch"));
            }
            Ok(())
        });
    }
}

fn trusted_file(path: &Path, executable: bool) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "Choose an absolute trusted file path");
    let path = fs::canonicalize(path)?;
    let metadata = fs::metadata(&path)?;
    let uid = unsafe { libc::geteuid() };
    ensure!(
        metadata.is_file()
            && (metadata.uid() == uid || metadata.uid() == 0)
            && metadata.mode() & 0o022 == 0,
        "Trusted files must be regular, owned by you/root and not group/world writable"
    );
    if executable {
        ensure!(
            metadata.mode() & 0o111 != 0 && metadata.mode() & 0o6000 == 0,
            "Choose a non-setuid executable"
        );
    } else {
        ensure!(metadata.len() <= 1024 * 1024, "Configuration exceeds 1 MiB");
    }
    for parent in path.ancestors().skip(1) {
        let m = fs::metadata(parent)?;
        ensure!(
            (m.uid() == uid || m.uid() == 0) && m.mode() & 0o022 == 0,
            "Trusted file has a writable/untrusted parent directory"
        );
    }
    Ok(path)
}

// CLI dialects checked against deskflow/deskflow v1.23.0 ArgParser.cpp and current
// src/apps/deskflow-core/{CoreArgs.h,CoreArgParser.cpp,deskflow-core.cpp}, and the
// security key names against src/lib/common/Settings.h. No Deskflow code copied.
fn deskflow_arguments(help: &str, configuration: &Path, settings: &str) -> Result<Vec<String>> {
    let path = configuration
        .to_str()
        .context("Deskflow configuration path is not UTF-8")?;
    if help.contains("--settings") && help.contains("coremode") && help.contains("server") {
        validate_deskflow_security(settings)?;
        return Ok(args(&["server", "--settings", path]));
    }
    for flag in ["--config", "--no-daemon", "--enable-crypto"] {
        ensure!(
            help.contains(flag),
            "Unsupported Deskflow CLI: missing {flag}. Select a compatible server CLI or modern deskflow-core advertising coremode/--settings, not the GUI."
        );
    }
    let mut command = vec![];
    if help.contains("server [") || help.contains("server|client") {
        command.push("server".into());
    }
    command.extend(args(&["--no-daemon", "--enable-crypto", "--config", path]));
    Ok(command)
}

fn validate_deskflow_security(settings: &str) -> Result<()> {
    // Accept only unambiguous QSettings INI booleans, not guessed defaults or
    // Qt escaped/value encodings. A user can set these in Deskflow's GUI first.
    let mut group = "";
    let mut tls = None;
    let mut peers = None;
    for line in settings.lines().map(str::trim) {
        if line.starts_with('[') && line.ends_with(']') {
            group = &line[1..line.len() - 1];
            continue;
        }
        if group != "security" {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let target = match key.trim() {
                "tlsEnabled" => &mut tls,
                "checkPeerFingerprints" => &mut peers,
                _ => continue,
            };
            ensure!(
                target.is_none(),
                "Duplicate Deskflow security setting is ambiguous"
            );
            *target = Some(value.trim() == "true");
        }
    }
    ensure!(
        tls == Some(true) && peers == Some(true),
        "Modern Deskflow settings must explicitly contain [security] tlsEnabled=true and checkPeerFingerprints=true. Enable TLS and peer verification in Deskflow, save settings, then retry."
    );
    Ok(())
}

/// Supports advertised classic server and modern core CLI dialects. Unknown
/// variants fail closed. Modern settings require explicit TLS/peer verification.
pub fn start_deskflow(config: &DeskflowConfig) -> Result<DeskflowSession> {
    ensure!(
        config.security_acknowledged,
        "Read and acknowledge Deskflow network/security requirements first"
    );
    let executable = trusted_file(&config.executable, true)?;
    let configuration = trusted_file(&config.configuration, false)?;
    let help = run_at(&executable, &args(&["--help"]))?;
    let settings =
        fs::read_to_string(&configuration).context("Deskflow configuration must be UTF-8 text")?;
    let arguments = deskflow_arguments(&help, &configuration, &settings)?;
    let mut command = Command::new(executable);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    parent_death_signal(&mut command);
    let (stop, commands) = std::sync::mpsc::channel();
    let (ready, started) = std::sync::mpsc::sync_channel(1);
    let state = std::sync::Arc::new(std::sync::Mutex::new(DeskflowStatus {
        running: false,
        pid: None,
        detail: "Starting".into(),
    }));
    let shared = state.clone();
    let supervisor = std::thread::spawn(move || {
        let mut child = match command.spawn() {
            Ok(child) => OwnedChild::new(child),
            Err(error) => {
                let _ = ready.send(Err(error.to_string()));
                return;
            }
        };
        if let Ok(mut state) = shared.lock() {
            *state = DeskflowStatus { running: true, pid: Some(child.child.id()), detail: "Owned Deskflow server process is running; this does not prove a peer is connected or TLS trust is established.".into() };
        }
        if ready.send(Ok(())).is_err() {
            return;
        }
        let detail = loop {
            match commands.recv_timeout(Duration::from_millis(100)) {
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    break "Stopped owned server.".to_owned();
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
            match child.exited() {
                Ok(false) => {}
                Ok(true) => {
                    break match child.finish() {
                        Ok(status) => format!(
                            "Deskflow exited: {status}. Check configuration, certificates and compositor/input permissions."
                        ),
                        Err(error) => format!("Deskflow cleanup error: {error:#}"),
                    };
                }
                Err(error) => break format!("Deskflow status error: {error:#}"),
            }
        };
        drop(child); // kill group and reap before advertising stopped
        if let Ok(mut state) = shared.lock() {
            *state = DeskflowStatus {
                running: false,
                pid: None,
                detail,
            };
        }
    });
    match started.recv_timeout(TIMEOUT) {
        Ok(Ok(())) => Ok(DeskflowSession {
            stop: Some(stop),
            state,
            supervisor: Some(supervisor),
        }),
        Ok(Err(error)) => {
            drop(stop);
            let _ = supervisor.join();
            bail!("Could not launch Deskflow: {error}");
        }
        Err(_) => {
            drop(stop);
            bail!("Deskflow launch timed out; supervisor will terminate any late child");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn peripheral_bounds_types_steps_and_choices() {
        let c = integer(-10, 10, 5);
        for v in [-10, 0, 10] {
            assert!(c.validate(&ControlValue::Integer(v)).is_ok());
        }
        for v in [-11, 1, 11] {
            assert!(c.validate(&ControlValue::Integer(v)).is_err());
        }
        assert!(c.validate(&ControlValue::Text("0".into())).is_err());
        assert!(
            choices(vec![125, 1000])
                .unwrap()
                .validate(&ControlValue::Integer(500))
                .is_err()
        );
        assert!(
            Constraint::Rgb
                .validate(&ControlValue::Text("ff00A1".into()))
                .is_ok()
        );
        assert!(
            Constraint::Rgb
                .validate(&ControlValue::Text("--help".into()))
                .is_err()
        );
        assert!(
            integer(i64::MIN, i64::MAX, 1)
                .validate(&ControlValue::Integer(i64::MAX))
                .is_ok()
        );
    }
    #[test]
    fn peripheral_missing_helper_and_identifier_restrictions() {
        assert!(
            find_helper(
                "not-real-logicenter-helper",
                Some(std::ffi::OsStr::new("relative:/nonexistent"))
            )
            .is_none()
        );
        assert!(!safe_identifier("--help"));
        assert!(!safe_identifier("mouse;touch /tmp/a"));
        assert!(safe_identifier("alsa_output.usb-046d_0.analog-stereo"));
        assert!(numbered_node(Path::new("/dev/video10"), "video"));
        for path in [
            "/dev/video",
            "/tmp/video0",
            "/dev/../dev/video0",
            "/dev/video0/evil",
        ] {
            assert!(!numbered_node(Path::new(path), "video"));
        }
        assert!(ratbag_list("warbling-mara: Logitech G502\nNo devices available.").len() == 1);
    }
    #[test]
    fn peripheral_ratbag_info_fixture() {
        let p = parse_mouse_info(
            "mouse - G502\n Number of Buttons: 11\nProfile 0: (active)\n Name: test\n Report Rate: 1000Hz\n Resolutions:\n  0: 800x800dpi (active)\n  1: 1600x1600dpi\n Button: 0 is mapped to 'button 1'\n Button: 1 is mapped to key 'KEY_A'\n LED: 0, depth: rgb, mode: on, color: ffffff\nProfile 1: (disabled)\n",
        );
        assert_eq!(p.len(), 2);
        assert!(p[0].active);
        assert!(p[1].disabled);
        assert_eq!(
            p[0].resolutions,
            [(0, Some(800), true), (1, Some(1600), false)]
        );
        assert_eq!(p[0].buttons, [(0, 1)]);
        assert_eq!(p[0].leds[0].1, "rgb");
        assert_eq!(parse_numbers("400 800 1600\n"), [400, 800, 1600]);
    }
    #[test]
    fn peripheral_cameras_group_nodes_but_keep_identical_usb_cameras_separate() {
        let camera = |node: u32, usb: &str, controls: bool| -> Peripheral {
            serde_json::from_value(serde_json::json!({
                "id":format!("camera:/dev/video{node}"), "name":"Logitech BRIO",
                "kind":"camera", "backend":"fixture", "status":"read-only fixture",
                "metadata":{"usb_identity":format!("{usb}:046d:085e:same-serial")},
                "controls":if controls { serde_json::json!([{
                    "id":"zoom_absolute","label":"Zoom","constraint":{"type":"integer","min":100,"max":500,"step":1,"choices":[]},
                    "current":100,"note":""
                }]) } else { serde_json::json!([]) }
            })).unwrap()
        };
        let devices = group_cameras(vec![
            camera(10, "/usb/port1", true),
            camera(3, "/usb/port1", true),
            camera(2, "/usb/port1", false),
            camera(5, "/usb/port2", true),
            camera(6, "/usb/port2", false),
        ]);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].id, "camera:/dev/video3");
        assert_eq!(devices[1].id, "camera:/dev/video5");
        assert_ne!(camera_key(&devices[0]), camera_key(&devices[1]));
        assert_eq!(
            camera_key(&devices[0]),
            camera_key(&camera(42, "/usb/port1", true))
        );
        assert!(devices.iter().all(|d| d.controls.len() == 1));
        let denied = group_cameras(vec![
            camera(2, "/usb/port1", false),
            camera(1, "/usb/port1", false),
        ]);
        assert_eq!(denied.len(), 1);
        assert_eq!(denied[0].id, "camera:/dev/video1");
        assert_eq!(denied[0].name, "Logitech BRIO");
        let mut no_identity = camera(8, "/usb/port3", false);
        no_identity.metadata.clear();
        let mut another = no_identity.clone();
        another.id = "camera:/dev/video9".into();
        assert_eq!(group_cameras(vec![no_identity, another]).len(), 2);
    }

    #[test]
    fn peripheral_camera_control_fixture() {
        let controls = parse_camera_controls(
            "focus_absolute 0x009a090a (int) : min=0 max=250 step=5 default=0 value=20 flags=inactive\nfocus_auto 0x009a090c (bool) : default=1 value=1\nexposure_auto 0x009a0901 (menu) : min=0 max=3 default=3 value=3\n  1: Manual Mode\n  3: Aperture Priority Mode\nserial 0x009a0999 (string) : min=0 max=40 value=x\n",
        );
        assert_eq!(controls.len(), 3);
        assert!(!controls[0].writable);
        assert_eq!(controls[0].step, 5);
        assert_eq!(controls[1].max, 1);
        assert_eq!(controls[2].menu.len(), 2);
        assert_eq!(
            camera_preset_value(&Constraint::Toggle, 0).unwrap(),
            ControlValue::Bool(false)
        );
        assert_eq!(
            camera_preset_value(&Constraint::Toggle, 1).unwrap(),
            ControlValue::Bool(true)
        );
        assert!(camera_preset_value(&Constraint::Toggle, 2).is_err());
    }
    #[test]
    fn peripheral_litra_allowlist_reports_and_collection() {
        assert_eq!(litra_model(0x046d, 0xc900), Some(LitraModel::Glow));
        assert!(litra_model(0x046d, 0xc902).is_none());
        assert!(litra_model(1, 0xc900).is_none());
        let descriptor = [
            0x06, 0x43, 0xff, 0x0a, 0x02, 0x02, 0xa1, 0x01, 0x85, 0x11, 0x75, 0x08, 0x95, 0x13,
            0x91, 0x02, 0xc0,
        ];
        assert!(litra_collection(&descriptor));
        let mut wrong = descriptor;
        wrong[4] = 3;
        assert!(!litra_collection(&wrong));
        assert!(!litra_collection(&descriptor[..16]));
        let report = litra_report(LitraModel::Glow, 0x9c, &ControlValue::Integer(4600)).unwrap();
        assert_eq!(&report[..6], &[0x11, 0xff, 0x04, 0x9c, 0x11, 0xf8]);
        assert!(report[6..].iter().all(|b| *b == 0));
        assert!(litra_report(LitraModel::Glow, 0x4c, &ControlValue::Integer(251)).is_err());
        assert!(litra_report(LitraModel::Beam, 0x4c, &ControlValue::Integer(400)).is_ok());
        assert!(litra_report(LitraModel::Glow, 0x9c, &ControlValue::Integer(2750)).is_err());
    }
    #[test]
    fn peripheral_audio_json_is_generic_vendor_scoped() {
        let json = r#"[{"name":"alsa_output.usb-logitech","description":"Headset","properties":{"device.vendor.id":"046d"},"mute":false,"volume":{"front-left":{"value_percent":"42%"}}},{"name":"other","properties":{"device.vendor.id":"1234"}}]"#;
        let devices = parse_audio(json, false).unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(
            devices[0].controls[1].current,
            Some(ControlValue::Integer(42))
        );
        assert!(devices[0].status.contains("not hardware EQ"));
        assert!(parse_audio("not json", false).is_err());
    }
    #[test]
    fn peripheral_preset_config_serialization_and_path_restrictions() {
        assert!(preset_control("brightness"));
        assert!(preset_control("focus_absolute"));
        assert!(!preset_control("focus_relative"));
        let preset = CameraPreset {
            version: 1,
            name: "desk".into(),
            usb_identity: "/sys/devices/usb1/1-1".into(),
            values: BTreeMap::from([("focus_absolute".into(), 20)]),
        };
        assert_eq!(
            serde_json::from_slice::<CameraPreset>(&serde_json::to_vec(&preset).unwrap()).unwrap(),
            preset
        );
        for name in ["../evil", "", "/absolute", "a/b", "bad\nname"] {
            assert!(preset_path(name).is_err());
        }
        let config = DeskflowConfig {
            executable: "/usr/bin/deskflow-server".into(),
            configuration: "/home/me/desk.conf".into(),
            security_acknowledged: true,
        };
        assert!(
            serde_json::from_str::<DeskflowConfig>(&serde_json::to_string(&config).unwrap())
                .is_ok()
        );
        assert!(trusted_file(Path::new("relative"), true).is_err());
        assert!(
            start_deskflow(&DeskflowConfig {
                security_acknowledged: false,
                ..config
            })
            .is_err()
        );
    }

    #[test]
    fn peripheral_deskflow_cli_dialects_security_and_argument_separation() {
        let path = Path::new("/home/user/My configs/server;not-a-shell.conf");
        let classic = deskflow_arguments("--config --no-daemon --enable-crypto", path, "").unwrap();
        assert_eq!(
            classic,
            [
                "--no-daemon",
                "--enable-crypto",
                "--config",
                path.to_str().unwrap()
            ]
        );
        assert!(deskflow_arguments("GUI --help", path, "").is_err());
        let secure = "[security]\ntlsEnabled=true\ncheckPeerFingerprints=true\n";
        let modern =
            deskflow_arguments("coremode server or client --settings", path, secure).unwrap();
        assert_eq!(modern, ["server", "--settings", path.to_str().unwrap()]);
        for settings in [
            "",
            "[security]\ntlsEnabled=false\ncheckPeerFingerprints=true",
            "[security]\ntlsEnabled=true\ncheckPeerFingerprints=false",
            "[security]\ntlsEnabled=true\ntlsEnabled=false\ncheckPeerFingerprints=true",
        ] {
            assert!(validate_deskflow_security(settings).is_err());
        }
    }

    #[test]
    fn peripheral_private_atomic_preset_and_symlink_rejection() {
        use std::os::unix::fs::symlink;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let base = PathBuf::from("/tmp/opencode").join(format!(
            "logicenter-peripherals-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&base).unwrap();
        let path = base.join("preset.json");
        let mut preset = CameraPreset {
            version: 1,
            name: "desk".into(),
            usb_identity: "usb:046d:1234:serial".into(),
            values: BTreeMap::from([("focus_absolute".into(), 20)]),
        };
        crate::preferences::atomic_write(&path, &serde_json::to_vec(&preset).unwrap(), 0o600)
            .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(read_preset(&path, "desk").unwrap(), preset);
        preset.values.insert("zoom_absolute".into(), 100);
        crate::preferences::atomic_write(&path, &serde_json::to_vec(&preset).unwrap(), 0o600)
            .unwrap();
        assert_eq!(read_preset(&path, "desk").unwrap(), preset);
        assert_eq!(fs::read_dir(&base).unwrap().count(), 1);
        let link = base.join("link.json");
        symlink(&path, &link).unwrap();
        assert!(read_preset(&link, "desk").is_err());
        assert!(read_preset(&path, "other").is_err());
        fs::remove_file(link).unwrap();
        fs::remove_file(path).unwrap();
        fs::remove_dir(base).unwrap();
    }

    #[test]
    fn peripheral_inventory_json_and_unsafe_apply_ids() {
        let inventory = Inventory {
            devices: vec![Peripheral {
                id: "camera:/dev/video0".into(),
                name: "fixture".into(),
                kind: Kind::Camera,
                backend: "fixture".into(),
                status: "test".into(),
                metadata: BTreeMap::new(),
                controls: vec![control(
                    "focus".into(),
                    "Focus".into(),
                    integer(0, 250, 5),
                    Some(ControlValue::Integer(20)),
                    Route::Camera {
                        path: "/dev/video0".into(),
                        name: "focus_absolute".into(),
                    },
                )],
            }],
            backends: vec![],
        };
        let text = serde_json::to_string(&inventory).unwrap();
        assert!(!text.contains("route"));
        let restored: Inventory = serde_json::from_str(&text).unwrap();
        assert!(restored.devices[0].controls[0].route.is_none());
        assert!(apply("unknown:id", "anything", ControlValue::Integer(1)).is_err());
        assert!(
            apply(
                "camera:/tmp/not-a-device",
                "focus",
                ControlValue::Integer(1)
            )
            .is_err()
        );
        assert!(apply("light:/dev/../tmp/fake", "power", ControlValue::Bool(true)).is_err());
    }
}
