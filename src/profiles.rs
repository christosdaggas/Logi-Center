use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::backend::SettingsUpdate;
use crate::model::{DeviceSnapshot, ScanReport};

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KeyChord {
    pub keys: Vec<u16>,
    pub label: String,
}

impl KeyChord {
    pub fn validate(&self) -> Result<()> {
        if self.keys.is_empty()
            || self.keys.len() > 6
            || self.label.len() > 160
            || self.keys.iter().any(|key| !(1..=255).contains(key))
            || self.keys.iter().collect::<HashSet<_>>().len() != self.keys.len()
        {
            bail!("Invalid keyboard shortcut");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Action {
    Shortcut(KeyChord),
    CycleDpi,
    Workflow(crate::workflow::Workflow),
    SwitchHost(u8),
    Ring(Vec<(String, Action)>),
}

impl Action {
    pub fn validate(&self) -> Result<()> {
        let mut remaining = 128;
        self.validate_inner(0, &mut remaining)
    }
    fn validate_inner(&self, depth: usize, remaining: &mut usize) -> Result<()> {
        if depth > 4 || *remaining == 0 {
            bail!("Action nesting or total action limit exceeded");
        }
        *remaining -= 1;
        match self {
            Self::Shortcut(chord) => chord.validate(),
            Self::Workflow(workflow) => workflow.validate(),
            Self::SwitchHost(host) if !(1..=3).contains(host) => bail!("Host must be 1–3"),
            Self::Ring(items) => {
                if items.is_empty() || items.len() > 8 {
                    bail!("A ring must contain 1–8 actions");
                }
                for (label, action) in items {
                    valid_name(label)?;
                    action.validate_inner(depth + 1, remaining)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    pub fn needs_keyboard(&self) -> bool {
        match self {
            Self::Shortcut(_) => true,
            Self::Workflow(workflow) => workflow.needs_keyboard(),
            Self::Ring(items) => items.iter().any(|(_, action)| action.needs_keyboard()),
            _ => false,
        }
    }
    fn validate_device(&self, device: &DeviceSnapshot) -> Result<()> {
        self.validate()?;
        match self {
            Self::CycleDpi if device.dpi_feature.is_none() => {
                bail!("This device does not support adjustable DPI")
            }
            Self::SwitchHost(host)
                if device.host_feature.is_none()
                    || device.host.as_ref().is_none_or(|info| *host > info.count) =>
            {
                bail!("This device does not support that host")
            }
            Self::Ring(items) => {
                for (_, action) in items {
                    action.validate_device(device)?;
                }
            }
            _ => (),
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Binding {
    pub tap: Option<Action>,
    pub hold: Option<Action>,
    pub double_tap: Option<Action>,
    pub hold_ms: u16,
    pub double_tap_ms: u16,
    pub gesture: crate::gesture::GestureConfig,
    // Up, Down, Left, Right; a device must advertise RawXY to use these.
    pub directions: [Option<KeyChord>; 4],
}

impl Default for Binding {
    fn default() -> Self {
        Self {
            tap: None,
            hold: None,
            double_tap: None,
            hold_ms: 600,
            double_tap_ms: 250,
            gesture: Default::default(),
            directions: Default::default(),
        }
    }
}

impl Binding {
    pub fn gestures(&self) -> bool {
        self.directions.iter().any(Option::is_some)
    }
    pub fn is_empty(&self) -> bool {
        self.tap.is_none() && self.hold.is_none() && self.double_tap.is_none() && !self.gestures()
    }
    pub fn validate(&self) -> Result<()> {
        if !(100..=5000).contains(&self.hold_ms) || !(100..=1000).contains(&self.double_tap_ms) {
            bail!("Invalid hold/double-tap timing");
        }
        self.gesture.validate()?;
        for action in [&self.tap, &self.hold, &self.double_tap]
            .into_iter()
            .flatten()
        {
            action.validate()?;
        }
        for chord in self.directions.iter().flatten() {
            chord.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeviceProfile {
    pub restore: bool,
    pub saved: SettingsUpdate,
    pub dpi_presets: Vec<u16>,
    pub actions_enabled: bool,
    pub bindings: BTreeMap<u16, Binding>,
    pub restore_ignore: BTreeSet<RestoreSetting>,
}

/// Fields excluded from automatic restoration; host switches and manual diversion are
/// always excluded, regardless of this policy.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum RestoreSetting {
    Dpi,
    InvertScroll,
    HighResolutionScroll,
    ScrollDiversion,
    SmoothScroll,
    Ratcheted,
    SmartShiftThreshold,
    SmartShiftTorque,
    FnSwap,
    BacklightEnabled,
    BacklightMode,
    BacklightLevel,
    BacklightDurations,
    DisableKeys,
    ButtonRemaps,
    ThumbwheelInverted,
    ReportRate,
    OsLayout,
}

impl RestoreSetting {
    pub fn label(self) -> &'static str {
        match self {
            Self::Dpi => "DPI",
            Self::InvertScroll => "Scroll direction",
            Self::HighResolutionScroll => "High-resolution scrolling",
            Self::ScrollDiversion => "Manual wheel diversion (never restored)",
            Self::SmoothScroll => "Smooth scrolling",
            Self::Ratcheted => "Wheel ratchet mode",
            Self::SmartShiftThreshold => "SmartShift threshold",
            Self::SmartShiftTorque => "SmartShift torque",
            Self::FnSwap => "Function-key swap",
            Self::BacklightEnabled => "Backlight enabled",
            Self::BacklightMode => "Backlight mode",
            Self::BacklightLevel => "Backlight brightness",
            Self::BacklightDurations => "Backlight timeouts",
            Self::DisableKeys => "Disabled keys",
            Self::ButtonRemaps => "Button remapping",
            Self::ThumbwheelInverted => "Thumbwheel direction",
            Self::ReportRate => "Report rate",
            Self::OsLayout => "Operating-system layout",
        }
    }
    pub const ALL: [Self; 18] = [
        Self::Dpi,
        Self::InvertScroll,
        Self::HighResolutionScroll,
        Self::ScrollDiversion,
        Self::SmoothScroll,
        Self::Ratcheted,
        Self::SmartShiftThreshold,
        Self::SmartShiftTorque,
        Self::FnSwap,
        Self::BacklightEnabled,
        Self::BacklightMode,
        Self::BacklightLevel,
        Self::BacklightDurations,
        Self::DisableKeys,
        Self::ButtonRemaps,
        Self::ThumbwheelInverted,
        Self::ReportRate,
        Self::OsLayout,
    ];
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AppRule {
    pub app_id: String,
    pub profile: String,
    #[serde(default)]
    pub priority: i32,
    #[serde(default = "enabled")]
    pub enabled: bool,
}
fn enabled() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileSet {
    pub selected: String,
    pub manual_lock: bool,
    pub profiles: BTreeMap<String, DeviceProfile>,
    pub rules: Vec<AppRule>,
}
impl Default for ProfileSet {
    fn default() -> Self {
        Self {
            selected: "Default".into(),
            manual_lock: false,
            profiles: BTreeMap::from([("Default".into(), DeviceProfile::default())]),
            rules: vec![],
        }
    }
}

const SCHEMA_VERSION: u32 = 2;
pub const MAX_IMPORT_BYTES: usize = 1024 * 1024;
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Store {
    schema_version: u32,
    devices: BTreeMap<String, ProfileSet>,
}
impl Default for Store {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            devices: BTreeMap::new(),
        }
    }
}
static FILE_LOCK: Mutex<()> = Mutex::new(());
static FOREGROUND: Mutex<Option<String>> = Mutex::new(None);

pub fn set_foreground(app_id: Option<String>) {
    *FOREGROUND.lock().unwrap_or_else(|e| e.into_inner()) =
        app_id.and_then(|id| normalize_app(&id));
}
fn normalize_app(id: &str) -> Option<String> {
    let id = id.trim().to_lowercase();
    let id = id.trim_end_matches(".desktop").to_owned();
    (!id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)).then_some(id)
}
fn foreground() -> Option<String> {
    FOREGROUND.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

#[derive(Clone, Debug)]
pub struct SelectionStatus {
    pub selected: String,
    pub effective: String,
    pub manual_lock: bool,
    pub app_id: Option<String>,
    pub matched_rule: Option<usize>,
}
fn resolve(set: &ProfileSet, app: Option<String>) -> SelectionStatus {
    // Manual lock > exact app match (priority, then first rule) > selected fallback.
    let matched = (!set.manual_lock)
        .then(|| {
            set.rules
                .iter()
                .enumerate()
                .filter(|(_, rule)| {
                    rule.enabled
                        && normalize_app(&rule.app_id) == app
                        && app.is_some()
                        && set.profiles.contains_key(&rule.profile)
                })
                .max_by_key(|(index, rule)| (rule.priority, std::cmp::Reverse(*index)))
        })
        .flatten();
    SelectionStatus {
        selected: set.selected.clone(),
        effective: matched.map_or_else(|| set.selected.clone(), |(_, rule)| rule.profile.clone()),
        manual_lock: set.manual_lock,
        app_id: app,
        matched_rule: matched.map(|(i, _)| i),
    }
}

fn valid_name(name: &str) -> Result<()> {
    if name.trim().is_empty()
        || name.trim() != name
        || name.len() > 80
        || name.chars().any(char::is_control)
    {
        bail!(
            "Names must contain 1–80 bytes without leading/trailing spaces or control characters"
        );
    }
    Ok(())
}

pub fn identity(device: &DeviceSnapshot) -> Option<String> {
    let serial = device.serial.trim().to_uppercase();
    (!serial.is_empty()
        && serial.len() <= 128
        && !serial.chars().any(char::is_control)
        && (device.identity.source.is_empty() || device.identity.stable)
        && serial
            .chars()
            .any(|c| c.is_ascii_alphanumeric() && c != '0'))
    .then(|| format!("logitech:{serial}"))
}
fn previous_identity(device: &DeviceSnapshot) -> Option<String> {
    let serial = device
        .identity
        .pairing_serial
        .as_deref()?
        .trim()
        .to_uppercase();
    (!serial.is_empty() && serial.len() <= 128 && !serial.chars().any(char::is_control))
        .then(|| format!("logitech:{serial}"))
}

fn path() -> Result<std::path::PathBuf> {
    Ok(crate::preferences::config_home()
        .context("No configuration folder")?
        .join("logicenter/devices.json"))
}

fn read(path: &Path) -> Result<Store> {
    match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => {
            if !file.metadata()?.is_file() {
                bail!("Device profiles must be a regular file");
            }
            let mut data = Vec::new();
            file.take(MAX_IMPORT_BYTES as u64 + 1)
                .read_to_end(&mut data)?;
            decode_store(&data)
                .context("Device profiles are invalid; the original file was not overwritten")
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(error) => Err(error).context("Could not read device profiles"),
    }
}

fn json_value(data: &[u8]) -> Result<serde_json::Value> {
    if data.len() > MAX_IMPORT_BYTES {
        bail!("Profile data exceeds 1 MiB");
    }
    // serde_json's recursion limit remains enabled, then a stricter structural walk
    // limits typed Action/Workflow deserialization and recursive destruction.
    let value: serde_json::Value = serde_json::from_slice(data)?;
    fn check(v: &serde_json::Value, depth: usize, count: &mut usize) -> Result<()> {
        *count += 1;
        if depth > 64 || *count > 40_000 {
            bail!("Profile structure is too large or deeply nested");
        }
        match v {
            serde_json::Value::Array(items) => {
                for item in items {
                    check(item, depth + 1, count)?;
                }
            }
            serde_json::Value::Object(items) => {
                for item in items.values() {
                    check(item, depth + 1, count)?;
                }
            }
            _ => (),
        }
        Ok(())
    }
    check(&value, 0, &mut 0)?;
    Ok(value)
}

fn decode_store(data: &[u8]) -> Result<Store> {
    let value = json_value(data)?;
    let mut store = if value.get("schema_version").is_some() {
        let version = value["schema_version"]
            .as_u64()
            .context("Invalid schema version")?;
        if version != SCHEMA_VERSION as u64 {
            bail!("Unsupported profile schema {version}");
        }
        serde_json::from_value::<Store>(value)?
    } else {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Legacy {
            devices: BTreeMap<String, DeviceProfile>,
        }
        let legacy: Legacy = serde_json::from_value(value)?;
        Store {
            schema_version: SCHEMA_VERSION,
            devices: legacy
                .devices
                .into_iter()
                .map(|(id, profile)| {
                    let mut set = ProfileSet::default();
                    set.profiles.insert("Default".into(), profile);
                    (id, set)
                })
                .collect(),
        }
    };
    for set in store.devices.values_mut() {
        for profile in set.profiles.values_mut() {
            strip_transients(&mut profile.saved);
        }
    }
    validate_store(&store)?;
    Ok(store)
}

fn validate_profile(profile: &DeviceProfile) -> Result<()> {
    if profile.dpi_presets.len() > 8
        || profile.dpi_presets.contains(&0)
        || profile.bindings.len() > 64
        || profile.saved.button_remaps.len() > 64
        || profile.saved.button_diversions.len() > 64
        || profile.saved.dpi == Some(0)
        || profile.saved.smart_shift_threshold.is_some_and(|v| v > 50)
        || profile.saved.smart_shift_torque.is_some_and(|v| v > 100)
        || profile.saved.report_rate_us == Some(0)
    {
        bail!("Profile settings exceed safe limits");
    }
    for binding in profile.bindings.values() {
        binding.validate()?;
    }
    Ok(())
}
fn validate_set(set: &ProfileSet) -> Result<()> {
    if set.profiles.is_empty()
        || set.profiles.len() > 64
        || set.rules.len() > 256
        || !set.profiles.contains_key(&set.selected)
    {
        bail!("Invalid profile selection or profile/rule limit");
    }
    for (name, profile) in &set.profiles {
        valid_name(name)?;
        validate_profile(profile)?;
    }
    for rule in &set.rules {
        if normalize_app(&rule.app_id).is_none() || !set.profiles.contains_key(&rule.profile) {
            bail!("Application rule has an invalid application or missing profile");
        }
    }
    Ok(())
}
fn validate_store(store: &Store) -> Result<()> {
    if store.devices.len() > 256 {
        bail!("Too many devices");
    }
    for (id, set) in &store.devices {
        if id.is_empty() || id.len() > 256 {
            bail!("Invalid device identity");
        }
        validate_set(set)?;
    }
    Ok(())
}
fn write_store(path: &Path, store: &Store) -> Result<()> {
    validate_store(store)?;
    let bytes = serde_json::to_vec_pretty(store)?;
    if bytes.len() > MAX_IMPORT_BYTES {
        bail!("Profile store exceeds 1 MiB; original preserved");
    }
    json_value(&bytes)?;
    crate::preferences::atomic_write(path, &bytes, 0o600)
}

pub fn profile_set(device: &DeviceSnapshot) -> Result<ProfileSet> {
    let id = identity(device).context("This device has no stable serial number")?;
    let mut store = read(&path()?)?;
    Ok(store
        .devices
        .remove(&id)
        .or_else(|| previous_identity(device).and_then(|old| store.devices.remove(&old)))
        .unwrap_or_default())
}
pub fn selection_status(device: &DeviceSnapshot) -> Result<SelectionStatus> {
    Ok(resolve(&profile_set(device)?, foreground()))
}

pub fn load(device: &DeviceSnapshot) -> DeviceProfile {
    try_load(device).unwrap_or_else(|error| {
        log::warn!("{error:#}");
        DeviceProfile::default()
    })
}

fn try_load(device: &DeviceSnapshot) -> Result<DeviceProfile> {
    let Some(id) = identity(device) else {
        return Ok(DeviceProfile::default());
    };
    let mut store = read(&path()?)?;
    let set = store
        .devices
        .remove(&id)
        .or_else(|| previous_identity(device).and_then(|old| store.devices.remove(&old)))
        .unwrap_or_default();
    let status = resolve(&set, foreground());
    Ok(set.profiles[&status.effective].clone())
}

#[cfg(test)]
fn edit_at(path: &Path, id: &str, update: impl FnOnce(&mut DeviceProfile)) -> Result<()> {
    edit_set_at(path, id, |set| {
        let effective = resolve(set, foreground()).effective;
        update(
            set.profiles
                .get_mut(&effective)
                .context("Selected profile is missing")?,
        );
        Ok(())
    })
}
#[cfg(test)]
fn edit_set_at(
    path: &Path,
    id: &str,
    update: impl FnOnce(&mut ProfileSet) -> Result<()>,
) -> Result<()> {
    edit_set_at_alias(path, id, None, update)
}
fn edit_set_at_alias(
    path: &Path,
    id: &str,
    alias: Option<String>,
    update: impl FnOnce(&mut ProfileSet) -> Result<()>,
) -> Result<()> {
    let mut store = read(path)?;
    if !store.devices.contains_key(id) {
        if let Some(old) = alias.and_then(|alias| store.devices.remove(&alias)) {
            store.devices.insert(id.into(), old);
        }
    }
    update(store.devices.entry(id.to_owned()).or_default())?;
    write_store(path, &store)
}
fn edit_set(
    device: &DeviceSnapshot,
    update: impl FnOnce(&mut ProfileSet) -> Result<()>,
) -> Result<()> {
    let id = identity(device).context("This device has no stable serial number")?;
    let _lock = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    edit_set_at_alias(&path()?, &id, previous_identity(device), update)
}
pub fn create(device: &DeviceSnapshot, name: &str) -> Result<()> {
    edit_set(device, |set| create_in(set, name, None))
}
pub fn duplicate(device: &DeviceSnapshot, source: &str, name: &str) -> Result<()> {
    edit_set(device, |set| create_in(set, name, Some(source)))
}
fn create_in(set: &mut ProfileSet, name: &str, source: Option<&str>) -> Result<()> {
    valid_name(name)?;
    if set.profiles.contains_key(name) {
        bail!("A profile with this name already exists");
    }
    let profile = source
        .map(|source| {
            set.profiles
                .get(source)
                .cloned()
                .context("Source profile not found")
        })
        .transpose()?
        .unwrap_or_default();
    set.profiles.insert(name.into(), profile);
    set.selected = name.into();
    Ok(())
}
pub fn rename(device: &DeviceSnapshot, old: &str, new: &str) -> Result<()> {
    edit_set(device, |set| rename_in(set, old, new))
}
fn rename_in(set: &mut ProfileSet, old: &str, new: &str) -> Result<()> {
    valid_name(new)?;
    if !set.profiles.contains_key(old) {
        bail!("Profile not found");
    }
    if old == new {
        return Ok(());
    }
    if set.profiles.contains_key(new) {
        bail!("A profile with this name already exists");
    }
    let profile = set.profiles.remove(old).context("Profile not found")?;
    set.profiles.insert(new.into(), profile);
    if set.selected == old {
        set.selected = new.into();
    }
    for rule in &mut set.rules {
        if rule.profile == old {
            rule.profile = new.into();
        }
    }
    Ok(())
}
pub fn delete(device: &DeviceSnapshot, name: &str) -> Result<()> {
    edit_set(device, |set| delete_in(set, name))
}
fn delete_in(set: &mut ProfileSet, name: &str) -> Result<()> {
    if set.profiles.len() == 1 {
        bail!("The last profile cannot be deleted");
    }
    set.profiles.remove(name).context("Profile not found")?;
    set.rules.retain(|rule| rule.profile != name);
    if set.selected == name {
        set.selected = set.profiles.keys().next().unwrap().clone();
    }
    Ok(())
}
pub fn select(device: &DeviceSnapshot, name: &str) -> Result<()> {
    edit_set(device, |set| select_in(set, name))
}
/// Tray selections are a single atomic manual override, not two independent saves.
pub fn select_manual(device: &DeviceSnapshot, name: &str) -> Result<()> {
    edit_set(device, |set| {
        select_in(set, name)?;
        set.manual_lock = true;
        Ok(())
    })
}
fn select_in(set: &mut ProfileSet, name: &str) -> Result<()> {
    if !set.profiles.contains_key(name) {
        bail!("Profile not found");
    }
    set.selected = name.into();
    Ok(())
}
pub fn set_manual_lock(device: &DeviceSnapshot, locked: bool) -> Result<()> {
    edit_set(device, |set| {
        set.manual_lock = locked;
        Ok(())
    })
}
pub fn set_rules(device: &DeviceSnapshot, rules: Vec<AppRule>) -> Result<()> {
    edit_set(device, |set| {
        set.rules = rules;
        Ok(())
    })
}
pub fn set_restore_policy(
    device: &DeviceSnapshot,
    name: &str,
    restore: bool,
    ignore: BTreeSet<RestoreSetting>,
) -> Result<()> {
    edit_set(device, |set| {
        let profile = set.profiles.get_mut(name).context("Profile not found")?;
        profile.restore = restore;
        profile.restore_ignore = ignore;
        Ok(())
    })
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ExportBundle {
    schema_version: u32,
    device_id: String,
    device_name: String,
    profiles: ProfileSet,
}

/// Immutable preview: callers must explicitly choose entries and confirm mapping.
#[derive(Clone, Debug)]
pub struct ImportPreview {
    pub entries: Vec<ImportEntry>,
    pub rules: Vec<AppRule>,
    pub warnings: Vec<String>,
}
#[derive(Clone, Debug)]
pub struct ImportEntry {
    pub source_device: String,
    pub device_name: String,
    pub name: String,
    pub matches_device: bool,
    /// Serialized, sanitized settings and action definitions for a read-only preview.
    pub details: String,
    profile: DeviceProfile,
}

pub fn export_json(device: &DeviceSnapshot) -> Result<String> {
    let bundle = ExportBundle {
        schema_version: SCHEMA_VERSION,
        device_id: identity(device).context("No stable device identity")?,
        device_name: device.name.clone(),
        profiles: profile_set(device)?,
    };
    let json = serde_json::to_string_pretty(&bundle)?;
    json_value(json.as_bytes())?;
    Ok(json)
}
pub fn preview_json(device: &DeviceSnapshot, data: &[u8]) -> Result<ImportPreview> {
    let value = json_value(data)?;
    if value.get("schema_version").and_then(|v| v.as_u64()) != Some(SCHEMA_VERSION as u64) {
        bail!("Unsupported or missing export schema version");
    }
    let bundle: ExportBundle = serde_json::from_value(value)?;
    if bundle.device_id.is_empty() || bundle.device_id.len() > 256 || bundle.device_name.len() > 256
    {
        bail!("Invalid export device identity");
    }
    validate_set(&bundle.profiles)?;
    let matches = identity(device).as_deref() == Some(bundle.device_id.as_str());
    let mut preview = ImportPreview { entries: vec![], rules: bundle.profiles.rules.clone(), warnings: vec![
        "Imported profiles have automatic restore and actions disabled. Associated app rules are copied only for selected entries. Current selection and manual lock are preserved.".into()] };
    if !matches {
        preview
            .warnings
            .push("Device serial differs: explicit mapping confirmation is required.".into());
    }
    for (name, mut profile) in bundle.profiles.profiles {
        sanitize_import(&mut profile);
        preview.entries.push(ImportEntry {
            source_device: bundle.device_id.clone(),
            device_name: bundle.device_name.clone(),
            name,
            matches_device: matches,
            details: serde_json::to_string_pretty(&profile)?,
            profile,
        });
    }
    Ok(preview)
}
fn sanitize_import(profile: &mut DeviceProfile) {
    profile.actions_enabled = false;
    profile.restore = false;
    strip_transients(&mut profile.saved);
}
fn strip_transients(saved: &mut SettingsUpdate) {
    saved.target_host = None;
    saved.button_diversions.clear();
    saved.scroll_diversion = None;
    saved.thumbwheel_diverted = None;
}

fn validate_for_device(profile: &DeviceProfile, device: &DeviceSnapshot) -> Result<()> {
    validate_profile(profile)?;
    if profile.saved.dpi.is_some_and(|dpi| {
        device.dpi_feature.is_none()
            || !crate::dpi::supported(&device.dpi_values, device.dpi).contains(&dpi)
    }) {
        bail!("Imported DPI is not supported by the selected device");
    }
    if profile.dpi_presets.iter().any(|dpi| {
        device.dpi_feature.is_none()
            || !crate::dpi::supported(&device.dpi_values, device.dpi).contains(dpi)
    }) {
        bail!("Imported DPI presets are not supported by the selected device");
    }
    for (cid, binding) in &profile.bindings {
        let button = device
            .buttons
            .iter()
            .find(|button| button.cid == *cid && button.divertable)
            .context("Imported action button is not supported by the selected device")?;
        if binding.gestures() && !button.raw_xy {
            bail!("Imported gesture requires RawXY support");
        }
        for action in [&binding.tap, &binding.hold, &binding.double_tap]
            .into_iter()
            .flatten()
        {
            action.validate_device(device)?;
        }
    }
    if profile.bindings.values().filter(|b| b.gestures()).count() > 1 {
        bail!("Only one gesture button may own the RawXY stream");
    }
    let saved = &profile.saved;
    if saved
        .smart_shift_threshold
        .is_some_and(|v| !(1..=50).contains(&v))
        || saved.smart_shift_torque == Some(0)
    {
        bail!("Imported SmartShift threshold/torque is outside the supported range");
    }
    macro_rules! available { ($($value:expr => $support:expr),* $(,)?) => { $(
        if $value.is_some() && !$support { bail!("An imported setting is unsupported or unavailable on the selected device"); }
    )* }; }
    available!(saved.invert_scroll => device.scroll_flags.is_some(),
        saved.high_resolution_scroll => device.scroll_flags.is_some(),
        saved.smooth_scroll => device.smooth_scroll.is_some(),
        saved.ratcheted => device.smart_shift.is_some(),
        saved.smart_shift_threshold => device.smart_shift.is_some(),
        saved.smart_shift_torque => device.smart_shift.as_ref().is_some_and(|s| s.torque.is_some()),
        saved.fn_swap => device.fn_swap.is_some(),
        saved.backlight_enabled => device.backlight.is_some(),
        saved.backlight_mode => device.backlight.is_some(),
        saved.backlight_level => device.backlight.is_some(),
        saved.backlight_durations => device.backlight.is_some(),
        saved.disable_keys => device.disable_keys.is_some(),
        saved.thumbwheel_inverted => device.thumbwheel.is_some());
    if saved.backlight_level.is_some_and(|level| {
        device
            .backlight
            .as_ref()
            .is_none_or(|s| level > s.max_level)
    }) || saved
        .backlight_mode
        .is_some_and(|mode| ![1, 3].contains(&mode))
        || saved.disable_keys.is_some_and(|mask| {
            device
                .disable_keys
                .as_ref()
                .is_none_or(|s| mask & !s.supported != 0)
        })
        || saved.report_rate_us.is_some_and(|rate| {
            device
                .report_rate
                .as_ref()
                .is_none_or(|s| !s.supported_us.contains(&rate))
        })
        || saved.os_layout.is_some_and(|layout| {
            device
                .os_layout
                .as_ref()
                .is_none_or(|s| !s.settable || !s.platforms.iter().any(|p| p.id == layout))
        })
    {
        bail!("An imported setting value is not supported by this device");
    }
    for (cid, target) in &saved.button_remaps {
        if !device.buttons.iter().any(|b| b.cid == *cid && b.remappable)
            || !device.buttons.iter().any(|b| b.cid == *target)
        {
            bail!("Imported button remapping is not supported by this device");
        }
    }
    Ok(())
}

/// Adds selected entries without replacing existing profiles. The boolean is an
/// explicit user confirmation to map a different/missing source serial to this
/// device, not an automatic fuzzy device match. No hardware writes occur here.
pub fn import_selected(
    device: &DeviceSnapshot,
    preview: &ImportPreview,
    indices: &[usize],
    confirm_device_mapping: bool,
) -> Result<Vec<String>> {
    let id = identity(device).context("No stable target device identity")?;
    let _lock = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    import_at(
        &path()?,
        &id,
        device,
        preview,
        indices,
        confirm_device_mapping,
    )
}
fn import_at(
    path: &Path,
    id: &str,
    device: &DeviceSnapshot,
    preview: &ImportPreview,
    indices: &[usize],
    confirm_mapping: bool,
) -> Result<Vec<String>> {
    if indices.is_empty()
        || indices.len() > 64
        || indices.iter().collect::<HashSet<_>>().len() != indices.len()
    {
        bail!("Select 1–64 distinct profiles to import");
    }
    let mut names = vec![];
    edit_set_at_alias(path, id, previous_identity(device), |set| {
        let mut imported_names = BTreeMap::new();
        for &index in indices {
            let entry = preview
                .entries
                .get(index)
                .context("Invalid preview selection")?;
            // Re-evaluate against the actual commit target; never trust a preview's
            // matches_device flag after a caller changes the target device.
            if entry.source_device != id && !confirm_mapping {
                bail!("Confirm the explicit target device mapping first");
            }
            let mut profile = entry.profile.clone();
            sanitize_import(&mut profile);
            validate_for_device(&profile, device)?;
            let mut name = entry.name.clone();
            valid_name(&name)?;
            if set.profiles.contains_key(&name) {
                let mut base = name.clone();
                while base.len() > 48 {
                    base.pop();
                }
                for suffix in 1..=65 {
                    name = format!("{base} (import {suffix})");
                    if !set.profiles.contains_key(&name) {
                        break;
                    }
                }
            }
            set.profiles.insert(name.clone(), profile);
            imported_names.insert(entry.name.clone(), name.clone());
            names.push(name);
        }
        for source_rule in &preview.rules {
            if let Some(target) = imported_names.get(&source_rule.profile) {
                let mut rule = source_rule.clone();
                rule.profile = target.clone();
                set.rules.push(rule);
            }
        }
        Ok(())
    })?;
    Ok(names)
}

/// Budget is consumed during deserialization, including each alias expansion,
/// rather than walking an already expanded YAML tree after allocation.
fn bounded_yaml(text: &str) -> Result<serde_yaml::Value> {
    use serde::de::{DeserializeSeed, Error, MapAccess, SeqAccess, Visitor};
    struct Seed<'a> {
        remaining: &'a mut usize,
        depth: usize,
    }
    impl<'de> DeserializeSeed<'de> for Seed<'_> {
        type Value = serde_yaml::Value;
        fn deserialize<D: serde::Deserializer<'de>>(
            self,
            de: D,
        ) -> std::result::Result<Self::Value, D::Error> {
            if self.depth > 32 || *self.remaining == 0 {
                return Err(D::Error::custom("Legacy YAML structure limit exceeded"));
            }
            *self.remaining -= 1;
            de.deserialize_any(self)
        }
    }
    impl<'de> Visitor<'de> for Seed<'_> {
        type Value = serde_yaml::Value;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("bounded plain Solaar YAML")
        }
        fn visit_unit<E: Error>(self) -> std::result::Result<Self::Value, E> {
            Ok(serde_yaml::Value::Null)
        }
        fn visit_none<E: Error>(self) -> std::result::Result<Self::Value, E> {
            self.visit_unit()
        }
        fn visit_bool<E: Error>(self, v: bool) -> std::result::Result<Self::Value, E> {
            Ok(serde_yaml::Value::Bool(v))
        }
        fn visit_u64<E: Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
            Ok(serde_yaml::Value::Number(v.into()))
        }
        fn visit_i64<E: Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
            Ok(serde_yaml::Value::Number(v.into()))
        }
        fn visit_f64<E: Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
            if !v.is_finite() {
                return Err(E::custom("Non-finite legacy number"));
            }
            Ok(serde_yaml::Value::Number(v.into()))
        }
        fn visit_str<E: Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
            Ok(serde_yaml::Value::String(v.into()))
        }
        fn visit_string<E: Error>(self, v: String) -> std::result::Result<Self::Value, E> {
            Ok(serde_yaml::Value::String(v))
        }
        fn visit_seq<A: SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut values = Vec::new();
            while let Some(value) = seq.next_element_seed(Seed {
                remaining: &mut *self.remaining,
                depth: self.depth + 1,
            })? {
                values.push(value);
            }
            Ok(serde_yaml::Value::Sequence(values))
        }
        fn visit_map<A: MapAccess<'de>>(
            self,
            mut map: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut values = serde_yaml::Mapping::new();
            while let Some(key) = map.next_key_seed(Seed {
                remaining: &mut *self.remaining,
                depth: self.depth + 1,
            })? {
                let value = map.next_value_seed(Seed {
                    remaining: &mut *self.remaining,
                    depth: self.depth + 1,
                })?;
                if values.insert(key, value).is_some() {
                    return Err(A::Error::custom("Duplicate legacy mapping key"));
                }
            }
            Ok(serde_yaml::Value::Mapping(values))
        }
        // Default visit_enum rejects YAML tags. Nothing is instantiated or run.
    }
    let mut documents = serde_yaml::Deserializer::from_str(text);
    let document = documents.next().context("Empty legacy YAML")?;
    let value = Seed {
        remaining: &mut 10_000,
        depth: 0,
    }
    .deserialize(document)?;
    if documents.next().is_some() {
        bail!("Only one legacy YAML document is accepted");
    }
    Ok(value)
}

/// Reads actual Solaar config.yaml forms (version-header sequence or device map).
/// Only recognized settings are translated; diversion/rule execution is never
/// imported. Entries always require an explicit selection before persistence.
pub fn preview_solaar(device: &DeviceSnapshot, data: &[u8]) -> Result<ImportPreview> {
    if data.len() > 128 * 1024 {
        bail!("Legacy YAML exceeds 128 KiB");
    }
    let text = std::str::from_utf8(data)?;
    if text
        .lines()
        .any(|line| line.len() - line.trim_start().len() > 64)
    {
        bail!("Legacy YAML nesting exceeds the limit");
    }
    let root = bounded_yaml(text)?;
    fn entries(root: serde_yaml::Value) -> Result<Vec<serde_yaml::Value>> {
        match root {
            serde_yaml::Value::Sequence(items) if items.len() <= 257 => Ok(items),
            serde_yaml::Value::Mapping(map) if map.len() <= 257 => {
                if ["_serial", "_NAME", "_unitId"]
                    .iter()
                    .any(|key| map.contains_key(serde_yaml::Value::String((*key).into())))
                {
                    return Ok(vec![serde_yaml::Value::Mapping(map)]);
                }
                Ok(map.into_values().collect())
            }
            _ => bail!("Expected a Solaar device sequence or mapping"),
        }
    }
    let mut preview = ImportPreview { entries: vec![], rules: vec![], warnings: vec![
        "Legacy import translates supported settings only; actions, rules and auto-restore stay disabled.".into()] };
    for record in entries(root)? {
        let Some(map) = record.as_mapping() else {
            continue;
        };
        let get = |key: &str| map.get(serde_yaml::Value::String(key.into()));
        // The first sequence record is generally { _version: ... }.
        if get("_serial").is_none() && get("_NAME").is_none() && get("_unitId").is_none() {
            continue;
        }
        let string = |key: &str| {
            get(key)
                .and_then(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .or_else(|| v.as_u64().map(|n| n.to_string()))
                })
                .unwrap_or_default()
        };
        let number = |key: &str| -> Result<Option<u16>> {
            get(key)
                .map(|v| {
                    v.as_u64()
                        .and_then(|v| u16::try_from(v).ok())
                        .context(format!("Invalid Solaar integer {key}"))
                })
                .transpose()
        };
        let byte = |key: &str| -> Result<Option<u8>> {
            number(key)?
                .map(|v| u8::try_from(v).context(format!("Solaar value out of range: {key}")))
                .transpose()
        };
        let boolean = |key: &str| -> Result<Option<bool>> {
            get(key)
                .map(|v| {
                    v.as_bool()
                        .or_else(|| {
                            v.as_u64().and_then(|n| match n {
                                0 => Some(false),
                                1 => Some(true),
                                _ => None,
                            })
                        })
                        .context(format!("Invalid Solaar boolean {key}"))
                })
                .transpose()
        };
        let unit = string("_unitId");
        let serial = if unit.trim().is_empty() {
            string("_serial")
        } else {
            unit
        };
        let serial = serial.trim().to_uppercase();
        if serial.len() > 128 || serial.chars().any(char::is_control) {
            bail!("Invalid legacy device identity");
        }
        let source = if serial.is_empty() {
            String::new()
        } else {
            format!("logitech:{serial}")
        };
        let device_name = string("_NAME");
        if device_name.len() > 256 {
            bail!("Legacy device name exceeds the limit");
        }
        let recognized = [
            "_NAME",
            "_serial",
            "_wpid",
            "_modelId",
            "_unitId",
            "_sensitive",
            "_absent",
            "dpi",
            "hires-smooth-invert",
            "hires-smooth-resolution",
            "hires-scroll-mode",
            "smooth-scroll",
            "scroll-ratchet",
            "smart-shift",
            "scroll-ratchet-torque",
            "fn-swap",
            "reprogrammable-keys",
        ];
        let ignored: Vec<_> = map
            .keys()
            .filter_map(|key| key.as_str())
            .filter(|key| !recognized.contains(key))
            .take(24)
            .collect();
        if !ignored.is_empty() {
            preview.warnings.push(format!(
                "{}: ignored legacy fields: {}",
                device_name,
                ignored.join(", ")
            ));
        }
        let mut name: String = device_name
            .chars()
            .filter(|c| !c.is_control())
            .take(48)
            .collect();
        while name.len() > 48 {
            name.pop();
        }
        let name = if name.trim().is_empty() {
            "Solaar import".to_owned()
        } else {
            format!("{} (Solaar)", name.trim())
        };
        let mut profile = DeviceProfile::default();
        let ratchet = byte("scroll-ratchet")?;
        if ratchet.is_some_and(|mode| ![1, 2].contains(&mode)) {
            bail!("Invalid Solaar scroll-ratchet mode");
        }
        profile.saved = SettingsUpdate {
            dpi: number("dpi")?,
            invert_scroll: boolean("hires-smooth-invert")?,
            high_resolution_scroll: boolean("hires-smooth-resolution")?,
            scroll_diversion: boolean("hires-scroll-mode")?,
            smooth_scroll: boolean("smooth-scroll")?,
            ratcheted: ratchet.map(|v| v == 2),
            smart_shift_threshold: byte("smart-shift")?,
            smart_shift_torque: byte("scroll-ratchet-torque")?,
            fn_swap: boolean("fn-swap")?,
            ..Default::default()
        };
        if let Some(value) = get("reprogrammable-keys") {
            let remaps = value
                .as_mapping()
                .context("Invalid Solaar reprogrammable-keys map")?;
            if remaps.len() > 64 {
                bail!("Too many legacy button remaps");
            }
            for (cid, target) in remaps {
                let parse = |v: &serde_yaml::Value| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                        .and_then(|n| u16::try_from(n).ok())
                        .context("Invalid Solaar button identifier")
                };
                profile
                    .saved
                    .button_remaps
                    .push((parse(cid)?, parse(target)?));
            }
        }
        validate_profile(&profile)?;
        sanitize_import(&mut profile);
        preview.entries.push(ImportEntry {
            matches_device: identity(device).as_deref() == Some(source.as_str()),
            source_device: source,
            device_name,
            name,
            details: serde_json::to_string_pretty(&profile)?,
            profile,
        });
    }
    if preview.entries.is_empty() {
        bail!("No Solaar device records found");
    }
    Ok(preview)
}

pub fn save_applied(
    device: &DeviceSnapshot,
    update: &SettingsUpdate,
    restore: bool,
    presets: Vec<u16>,
) -> Result<()> {
    if identity(device).is_none() && !restore && presets.is_empty() {
        return Ok(());
    }
    save_applied_to(
        device,
        &selection_status(device)?.effective,
        update,
        restore,
        presets,
    )
}

/// Explicit target variant for callers that captured a profile before an async
/// hardware Apply. A foreground change must not redirect that completed save.
pub fn save_applied_to(
    device: &DeviceSnapshot,
    name: &str,
    update: &SettingsUpdate,
    restore: bool,
    presets: Vec<u16>,
) -> Result<()> {
    validate_presets(device, &presets)?;
    edit_named(device, name, |profile| {
        profile.restore = restore;
        profile.dpi_presets = presets;
        // A successful Apply can still leave unrelated features unreadable.
        // Merge actual readable values rather than deleting older saved values.
        let mut saved = profile.saved.clone();
        merge(&mut saved, &snapshot(device));
        merge(&mut saved, update);
        strip_transients(&mut saved);
        profile.saved = if restore {
            saved
        } else {
            SettingsUpdate::default()
        };
    })
}

fn validate_presets(device: &DeviceSnapshot, presets: &[u16]) -> Result<()> {
    if device.dpi_feature.is_none() && !presets.is_empty() {
        bail!("This device does not support DPI presets");
    }
    if device.dpi_feature.is_some() {
        let allowed = crate::dpi::supported(&device.dpi_values, device.dpi);
        if presets.len() > 8 || presets.iter().any(|dpi| !allowed.contains(dpi)) {
            bail!("Invalid DPI presets");
        }
    }
    Ok(())
}

/// For partial verified Apply: merge ONLY `ApplyOutcome::confirmed_update` into
/// prior saved values. No device snapshot values are copied into the profile.
/// Existing unmodified saved values remain; unknown/unconfirmed values are not
/// populated. Like the legacy API, restore=false clears saved restore values.
pub fn save_confirmed(
    device: &DeviceSnapshot,
    confirmed: &SettingsUpdate,
    restore: bool,
    presets: Vec<u16>,
) -> Result<()> {
    if identity(device).is_none() && !restore && presets.is_empty() {
        return Ok(());
    }
    save_confirmed_to(
        device,
        &selection_status(device)?.effective,
        confirmed,
        restore,
        presets,
    )
}
pub fn save_confirmed_to(
    device: &DeviceSnapshot,
    name: &str,
    confirmed: &SettingsUpdate,
    restore: bool,
    presets: Vec<u16>,
) -> Result<()> {
    validate_presets(device, &presets)?;
    edit_named(device, name, |profile| {
        save_confirmed_in(profile, confirmed, restore, presets)
    })
}
fn save_confirmed_in(
    profile: &mut DeviceProfile,
    confirmed: &SettingsUpdate,
    restore: bool,
    presets: Vec<u16>,
) {
    profile.restore = restore;
    profile.dpi_presets = presets;
    if restore {
        merge(&mut profile.saved, confirmed);
        strip_transients(&mut profile.saved);
    } else {
        profile.saved = SettingsUpdate::default();
    }
}

pub fn save_actions(
    device: &DeviceSnapshot,
    enabled: bool,
    bindings: BTreeMap<u16, Binding>,
) -> Result<()> {
    save_actions_to(
        device,
        &selection_status(device)?.effective,
        enabled,
        bindings,
    )
}
pub fn save_actions_to(
    device: &DeviceSnapshot,
    name: &str,
    enabled: bool,
    mut bindings: BTreeMap<u16, Binding>,
) -> Result<()> {
    bindings.retain(|_, binding| !binding.is_empty());
    for (cid, binding) in &bindings {
        binding.validate()?;
        let button = device
            .buttons
            .iter()
            .find(|button| button.cid == *cid && button.divertable)
            .context("This button does not support custom actions")?;
        if binding.gestures() && !button.raw_xy {
            bail!("This button does not support gesture movement reports");
        }
        for action in [&binding.tap, &binding.hold, &binding.double_tap]
            .into_iter()
            .flatten()
        {
            action.validate_device(device)?;
        }
    }
    if bindings
        .values()
        .filter(|binding| binding.gestures())
        .count()
        > 1
    {
        bail!("Only one gesture button per device can own the RawXY stream");
    }
    edit_named(device, name, |profile| {
        profile.actions_enabled = enabled;
        profile.bindings = bindings;
    })
}

pub fn preset_values(profile: &DeviceProfile, device: &DeviceSnapshot) -> Vec<u16> {
    let supported = crate::dpi::supported(&device.dpi_values, device.dpi);
    let values = if profile.dpi_presets.is_empty() {
        vec![800, 1200, 1600, 2400]
    } else {
        profile.dpi_presets.clone()
    };
    let mut valid: Vec<_> = values
        .into_iter()
        .filter(|value| supported.contains(value))
        .take(8)
        .collect();
    valid.dedup();
    if valid.is_empty() {
        if let Some(value) = supported.first() {
            valid.push(*value);
        }
    }
    valid
}

pub fn save_runtime_dpi(device: &DeviceSnapshot, dpi: u16) -> Result<()> {
    save_runtime_dpi_to(device, &selection_status(device)?.effective, dpi)
}
pub fn save_runtime_dpi_to(device: &DeviceSnapshot, name: &str, dpi: u16) -> Result<()> {
    if device.dpi_feature.is_none()
        || !crate::dpi::supported(&device.dpi_values, device.dpi).contains(&dpi)
    {
        bail!("Unsupported runtime DPI");
    }
    edit_named(device, name, |profile| {
        if profile.restore {
            profile.saved.dpi = Some(dpi);
        }
    })
}

fn edit_named(
    device: &DeviceSnapshot,
    name: &str,
    update: impl FnOnce(&mut DeviceProfile),
) -> Result<()> {
    edit_set(device, |set| {
        update(
            set.profiles
                .get_mut(name)
                .context("Target profile no longer exists")?,
        );
        Ok(())
    })
}

fn snapshot(device: &DeviceSnapshot) -> SettingsUpdate {
    let mut saved = SettingsUpdate {
        dpi: device.dpi,
        invert_scroll: device.scroll_inverted(),
        high_resolution_scroll: device.scroll_high_resolution(),
        smooth_scroll: device.smooth_scroll,
        ratcheted: device.is_ratcheted(),
        smart_shift_threshold: device.smart_shift.as_ref().map(|s| s.threshold),
        smart_shift_torque: device.smart_shift.as_ref().and_then(|s| s.torque),
        fn_swap: device.fn_swap,
        disable_keys: device.disable_keys.as_ref().map(|s| s.disabled),
        thumbwheel_inverted: device.thumbwheel.as_ref().map(|s| s.inverted),
        report_rate_us: device.report_rate.as_ref().map(|s| s.current_us),
        os_layout: device
            .os_layout
            .as_ref()
            .filter(|s| s.settable)
            .map(|s| s.current),
        button_remaps: device
            .buttons
            .iter()
            .filter(|button| button.remappable)
            .map(|button| (button.cid, button.mapped_to))
            .collect(),
        ..Default::default()
    };
    if let Some(light) = &device.backlight {
        saved.backlight_enabled = Some(light.enabled);
        saved.backlight_mode = Some(light.mode);
        saved.backlight_level = Some(light.level);
        saved.backlight_durations = Some((light.hands_out, light.hands_in, light.powered));
    }
    saved
}

fn merge(saved: &mut SettingsUpdate, update: &SettingsUpdate) {
    macro_rules! fields { ($($field:ident),*) => { $(if update.$field.is_some() { saved.$field = update.$field; })* }; }
    fields!(
        dpi,
        invert_scroll,
        high_resolution_scroll,
        smooth_scroll,
        ratcheted,
        smart_shift_threshold,
        smart_shift_torque,
        fn_swap,
        backlight_enabled,
        backlight_mode,
        backlight_level,
        backlight_durations,
        disable_keys,
        thumbwheel_inverted,
        report_rate_us,
        os_layout
    );
    for &(cid, target) in &update.button_remaps {
        saved.button_remaps.retain(|(old, _)| *old != cid);
        saved.button_remaps.push((cid, target));
    }
}

pub fn restore_update(profile: &DeviceProfile, device: &DeviceSnapshot) -> SettingsUpdate {
    let mut saved = profile.saved.clone();
    let current = snapshot(device);
    macro_rules! fields { ($($field:ident),*) => { $(if current.$field.is_none() || saved.$field == current.$field { saved.$field = None; })* }; }
    fields!(
        dpi,
        invert_scroll,
        high_resolution_scroll,
        scroll_diversion,
        smooth_scroll,
        ratcheted,
        smart_shift_threshold,
        smart_shift_torque,
        fn_swap,
        backlight_enabled,
        backlight_mode,
        backlight_level,
        backlight_durations,
        disable_keys,
        thumbwheel_inverted,
        report_rate_us,
        os_layout
    );
    macro_rules! ignore { ($($kind:ident => $field:ident),*) => { $(
        if profile.restore_ignore.contains(&RestoreSetting::$kind) { saved.$field = None; }
    )* }; }
    ignore!(Dpi => dpi, InvertScroll => invert_scroll, HighResolutionScroll => high_resolution_scroll,
        ScrollDiversion => scroll_diversion, SmoothScroll => smooth_scroll, Ratcheted => ratcheted,
        SmartShiftThreshold => smart_shift_threshold, SmartShiftTorque => smart_shift_torque,
        FnSwap => fn_swap, BacklightEnabled => backlight_enabled, BacklightMode => backlight_mode,
        BacklightLevel => backlight_level, BacklightDurations => backlight_durations, DisableKeys => disable_keys,
        ThumbwheelInverted => thumbwheel_inverted, ReportRate => report_rate_us, OsLayout => os_layout);
    if profile
        .restore_ignore
        .contains(&RestoreSetting::ButtonRemaps)
    {
        saved.button_remaps.clear();
    }
    if saved
        .dpi
        .is_some_and(|dpi| !crate::dpi::supported(&device.dpi_values, device.dpi).contains(&dpi))
    {
        saved.dpi = None;
    }
    saved.target_host = None;
    saved.button_diversions.clear();
    // Wheel-report diversion, like manual button diversion, needs a live
    // consumer and is never safe to turn on during automatic restoration.
    saved.scroll_diversion = None;
    saved.thumbwheel_diverted = None;
    if saved.report_rate_us.is_some_and(|rate| {
        device
            .report_rate
            .as_ref()
            .is_none_or(|s| !s.supported_us.contains(&rate))
    }) {
        saved.report_rate_us = None;
    }
    if saved.os_layout.is_some_and(|layout| {
        device
            .os_layout
            .as_ref()
            .is_none_or(|s| !s.settable || !s.platforms.iter().any(|p| p.id == layout))
    }) {
        saved.os_layout = None;
    }
    saved.button_remaps.retain(|(cid, target)| {
        device
            .buttons
            .iter()
            .any(|button| button.cid == *cid && button.remappable && button.mapped_to != *target)
            && device.buttons.iter().any(|button| button.cid == *target)
            && !(profile.actions_enabled && profile.bindings.contains_key(cid))
    });
    saved
}

/// A filtered-out/unreadable saved field is not proof of successful restore.
/// Keep those failures retryable rather than calling an empty update success.
fn unconfirmed_restore_fields(
    profile: &DeviceProfile,
    device: &DeviceSnapshot,
    update: &SettingsUpdate,
) -> Vec<String> {
    let current = snapshot(device);
    let mut missing = Vec::new();
    macro_rules! check { ($($kind:ident => $field:ident),*) => { $(
        if !profile.restore_ignore.contains(&RestoreSetting::$kind)
            && profile.saved.$field.is_some() && profile.saved.$field != current.$field
            && update.$field.is_none() { missing.push(stringify!($field).to_owned()); }
    )* }; }
    check!(Dpi => dpi, InvertScroll => invert_scroll, HighResolutionScroll => high_resolution_scroll,
        SmoothScroll => smooth_scroll, Ratcheted => ratcheted, SmartShiftThreshold => smart_shift_threshold,
        SmartShiftTorque => smart_shift_torque, FnSwap => fn_swap, BacklightEnabled => backlight_enabled,
        BacklightMode => backlight_mode, BacklightLevel => backlight_level, BacklightDurations => backlight_durations,
        DisableKeys => disable_keys, ThumbwheelInverted => thumbwheel_inverted, ReportRate => report_rate_us,
        OsLayout => os_layout);
    if !profile
        .restore_ignore
        .contains(&RestoreSetting::ButtonRemaps)
    {
        for (cid, target) in &profile.saved.button_remaps {
            if profile.actions_enabled && profile.bindings.contains_key(cid) {
                continue;
            }
            if !update.button_remaps.contains(&(*cid, *target))
                && !device
                    .buttons
                    .iter()
                    .any(|b| b.cid == *cid && b.mapped_to == *target)
            {
                missing.push(format!("button_remap:{cid}"));
            }
        }
    }
    missing
}

#[derive(Clone, Debug, Default)]
pub struct RestoreStatus {
    pub attempts: u8,
    pub completed: bool,
    pub last_error: Option<String>,
}
#[derive(Default)]
pub struct RestoreState {
    connections: BTreeMap<String, RestoreAttempt>,
}
struct RestoreAttempt {
    fingerprint: Vec<u8>,
    next_attempt: Instant,
    status: RestoreStatus,
}
const MAX_RESTORE_ATTEMPTS: u8 = 4;

fn confirmed_covers(requested: &SettingsUpdate, confirmed: &SettingsUpdate) -> Result<bool> {
    if requested.target_host.is_some()
        || !requested.button_diversions.is_empty()
        || requested.scroll_diversion.is_some()
        || requested.thumbwheel_diverted.is_some()
    {
        return Ok(false);
    }
    let wanted = serde_json::to_value(requested)?;
    let actual = serde_json::to_value(confirmed)?;
    Ok(wanted
        .as_object()
        .context("Invalid settings serialization")?
        .iter()
        .filter(|(_, v)| !v.is_null() && !v.as_array().is_some_and(Vec::is_empty))
        .all(|(key, value)| match (value, actual.get(key)) {
            (serde_json::Value::Array(wanted), Some(serde_json::Value::Array(actual))) => {
                wanted.iter().all(|item| actual.contains(item))
            }
            (_, actual) => actual == Some(value),
        }))
}
fn apply_restore_verified(device: &DeviceSnapshot, update: &SettingsUpdate) -> Result<Vec<String>> {
    let outcome = crate::backend::apply_verified(device, update)?;
    if !outcome.all_confirmed() || !confirmed_covers(update, &outcome.confirmed_update)? {
        bail!(
            "Restore was not fully read-back confirmed: {}{}",
            outcome
                .results
                .iter()
                .map(|r| format!(
                    "{}: {:?}{}",
                    r.setting,
                    r.state,
                    r.error
                        .as_ref()
                        .map(|s| format!(" ({s})"))
                        .unwrap_or_default()
                ))
                .collect::<Vec<_>>()
                .join("; "),
            if outcome.read_back_errors.is_empty() {
                String::new()
            } else {
                format!("; {}", outcome.read_back_errors.join("; "))
            }
        );
    }
    Ok(outcome
        .results
        .iter()
        .map(|r| format!("{}: {:?}", r.setting, r.state))
        .collect())
}
impl RestoreState {
    pub fn reset(&mut self) {
        self.connections.clear();
    }
    /// Scheduling hint only; the caller still performs a normal serialized scan.
    /// This makes bounded retries follow their backoff instead of waiting for the
    /// slow periodic battery refresh. Exhausted/completed attempts never rescan.
    pub fn next_retry_delay(&self) -> Option<Duration> {
        let now = Instant::now();
        self.connections
            .values()
            .filter(|a| !a.status.completed && a.status.attempts < MAX_RESTORE_ATTEMPTS)
            .map(|a| a.next_attempt.saturating_duration_since(now))
            .min()
    }
    pub fn status(&self, device: &DeviceSnapshot) -> Option<RestoreStatus> {
        self.connections
            .get(&format!("{}:{}", identity(device)?, device.key()))
            .map(|a| a.status.clone())
    }
    pub fn scan(&mut self) -> ScanReport {
        let mut report = crate::backend::scan();
        let changed = self.restore_report(&mut report, try_load, apply_restore_verified);
        if changed {
            let mut fresh = crate::backend::scan();
            fresh.errors.extend(report.errors);
            report = fresh;
        }
        report
    }
    fn restore_report(
        &mut self,
        report: &mut ScanReport,
        load: impl Fn(&DeviceSnapshot) -> Result<DeviceProfile>,
        mut apply: impl FnMut(&DeviceSnapshot, &SettingsUpdate) -> Result<Vec<String>>,
    ) -> bool {
        self.restore_report_at(report, load, &mut apply, Instant::now())
    }
    fn restore_report_at(
        &mut self,
        report: &mut ScanReport,
        load: impl Fn(&DeviceSnapshot) -> Result<DeviceProfile>,
        mut apply: impl FnMut(&DeviceSnapshot, &SettingsUpdate) -> Result<Vec<String>>,
        now: Instant,
    ) -> bool {
        let mut online = HashSet::new();
        let mut changed = false;
        for device in report
            .receivers
            .iter()
            .flat_map(|receiver| &receiver.devices)
            .filter(|device| device.online)
        {
            let Some(id) = identity(device) else {
                continue;
            };
            let connection = format!("{id}:{}", device.key());
            online.insert(connection.clone());
            let loaded = load(device);
            let fingerprint = loaded
                .as_ref()
                .ok()
                .and_then(|p| serde_json::to_vec(p).ok())
                .unwrap_or_default();
            let attempt = self
                .connections
                .entry(connection)
                .or_insert_with(|| RestoreAttempt {
                    fingerprint: fingerprint.clone(),
                    next_attempt: now,
                    status: RestoreStatus::default(),
                });
            if attempt.fingerprint != fingerprint {
                *attempt = RestoreAttempt {
                    fingerprint,
                    next_attempt: now,
                    status: RestoreStatus::default(),
                };
            }
            if attempt.status.completed
                || attempt.status.attempts >= MAX_RESTORE_ATTEMPTS
                || now < attempt.next_attempt
            {
                continue;
            }
            attempt.status.attempts += 1;
            let result = match loaded {
                Err(error) => Err(error.context("Could not read profile")),
                Ok(profile) => {
                    if profile.restore {
                        let update = restore_update(&profile, device);
                        let unconfirmed = unconfirmed_restore_fields(&profile, device, &update);
                        let applied = if !crate::backend::settings_empty(&update) {
                            // A partial failed write can still change hardware.
                            // Refresh the snapshot without recording success.
                            changed = true;
                            apply(device, &update).map(|_| ())
                        } else {
                            Ok(())
                        };
                        applied.and_then(|()| {
                            if unconfirmed.is_empty() { Ok(()) }
                            else { bail!("Restore could not be confirmed for unavailable/unsupported saved settings: {}", unconfirmed.join(", ")); }
                        })
                    } else {
                        Ok(())
                    }
                }
            };
            match result {
                Ok(()) => {
                    attempt.status.completed = true;
                    attempt.status.last_error = None;
                }
                Err(error) => {
                    let message = format!(
                        "Could not restore {} (attempt {}/{}): {error:#}",
                        device.name, attempt.status.attempts, MAX_RESTORE_ATTEMPTS
                    );
                    attempt.status.last_error = Some(message.clone());
                    report.errors.push(message);
                    attempt.next_attempt = now + Duration::from_secs(1 << attempt.status.attempts);
                }
            }
        }
        self.connections
            .retain(|connection, _| online.contains(connection));
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stable_identity_and_safe_restore() {
        let mut mouse = DeviceSnapshot {
            serial: "ABC123".into(),
            dpi: Some(800),
            dpi_feature: Some(1),
            ..Default::default()
        };
        let id = identity(&mouse);
        mouse.receiver_path = "other receiver".into();
        assert_eq!(identity(&mouse), id);
        mouse.serial = "other".into();
        assert_ne!(identity(&mouse), id);
        mouse.serial = String::new();
        assert!(identity(&mouse).is_none());
        let profile = DeviceProfile {
            saved: SettingsUpdate {
                dpi: Some(1600),
                target_host: Some(2),
                button_diversions: vec![(1, true)],
                fn_swap: Some(true),
                ..Default::default()
            },
            ..Default::default()
        };
        let update = restore_update(&profile, &mouse);
        assert_eq!(update.dpi, Some(1600));
        assert_eq!(update.fn_swap, None);
        assert_eq!(update.target_host, None);
        assert!(update.button_diversions.is_empty());
        mouse.dpi = Some(1600);
        assert_eq!(restore_update(&profile, &mouse).dpi, None);
    }
    #[test]
    fn shortcuts_validate_without_emitting_input() {
        assert!(
            KeyChord {
                keys: vec![29, 46],
                label: "Ctrl+C".into()
            }
            .validate()
            .is_ok()
        );
        for keys in [vec![], vec![29, 29], vec![0], vec![256]] {
            assert!(
                KeyChord {
                    keys,
                    label: "invalid".into()
                }
                .validate()
                .is_err()
            );
        }
    }
    #[test]
    fn independent_profile_serialization_and_presets() {
        let mut store = Store::default();
        let mut first = ProfileSet::default();
        first.profiles.insert(
            "Default".into(),
            DeviceProfile {
                restore: true,
                dpi_presets: vec![800, 1600],
                ..Default::default()
            },
        );
        store.devices.insert("one".into(), first);
        store.devices.insert("two".into(), ProfileSet::default());
        let decoded: Store = serde_json::from_slice(&serde_json::to_vec(&store).unwrap()).unwrap();
        assert!(decoded.devices["one"].profiles["Default"].restore);
        assert!(!decoded.devices["two"].profiles["Default"].restore);
        assert_eq!(
            preset_values(
                &decoded.devices["one"].profiles["Default"],
                &DeviceSnapshot::default()
            ),
            vec![800, 1600]
        );
    }
    #[test]
    fn files_are_private_independent_and_corruption_is_not_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let base = isolated("persistence");
        let path = base.join("devices.json");
        edit_at(&path, "one", |profile| {
            profile.restore = true;
            profile.saved.dpi = Some(1600);
        })
        .unwrap();
        edit_at(&path, "two", |profile| {
            profile.dpi_presets = vec![800, 1200]
        })
        .unwrap();
        let store = read(&path).unwrap();
        assert_eq!(
            store.devices["one"].profiles["Default"].saved.dpi,
            Some(1600)
        );
        assert!(!store.devices["two"].profiles["Default"].restore);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::write(&path, b"invalid-profile-data").unwrap();
        assert!(edit_at(&path, "one", |p| p.restore = false).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"invalid-profile-data");
        fs::remove_file(path).unwrap();
        fs::remove_dir(base).unwrap();
    }
    fn isolated(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NUMBER: AtomicU64 = AtomicU64::new(0);
        let path = Path::new("/tmp/opencode").join(format!(
            "logicenter-profile-{label}-{}-{}",
            std::process::id(),
            NUMBER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }
    #[test]
    fn old_store_migrates_only_after_successful_edit() {
        let dir = isolated("migration");
        let file = dir.join("devices.json");
        let bytes = br#"{"devices":{"logitech:ABC":{"restore":true,"saved":{"dpi":1600},"dpi_presets":[800,1600]}}}"#;
        fs::write(&file, bytes).unwrap();
        let store = read(&file).unwrap();
        assert_eq!(store.devices["logitech:ABC"].selected, "Default");
        assert_eq!(
            store.devices["logitech:ABC"].profiles["Default"].saved.dpi,
            Some(1600)
        );
        assert_eq!(fs::read(&file).unwrap(), bytes);
        edit_at(&file, "logitech:ABC", |p| p.saved.dpi = Some(2400)).unwrap();
        let written: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(written["schema_version"], 2);
        assert_eq!(
            read(&file).unwrap().devices["logitech:ABC"].profiles["Default"]
                .saved
                .dpi,
            Some(2400)
        );
        fs::remove_file(file).unwrap();
        fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn schema_corruption_and_size_fail_closed() {
        assert!(decode_store(br#"{"schema_version":999,"devices":{}}"#).is_err());
        assert!(decode_store(br#"{"schema_version":"2","devices":{}}"#).is_err());
        assert!(decode_store(b"{}").is_err());
        assert!(json_value(&vec![b' '; MAX_IMPORT_BYTES + 1]).is_err());
        let nested = format!("{}0{}", "[".repeat(80), "]".repeat(80));
        assert!(json_value(nested.as_bytes()).is_err());
        let dir = isolated("future");
        let file = dir.join("devices.json");
        let original = br#"{"schema_version":999,"devices":{}}"#;
        fs::write(&file, original).unwrap();
        assert!(edit_at(&file, "anything", |_| {}).is_err());
        assert_eq!(fs::read(&file).unwrap(), original);
        fs::remove_file(file).unwrap();
        fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn rule_precedence_lock_and_fallback_are_deterministic() {
        let mut set = ProfileSet::default();
        for name in ["Browser", "Work"] {
            set.profiles.insert(name.into(), DeviceProfile::default());
        }
        set.rules = vec![
            AppRule {
                app_id: "Firefox.desktop".into(),
                profile: "Browser".into(),
                priority: 10,
                enabled: true,
            },
            AppRule {
                app_id: "firefox".into(),
                profile: "Work".into(),
                priority: 20,
                enabled: false,
            },
            AppRule {
                app_id: "firefox".into(),
                profile: "Work".into(),
                priority: 10,
                enabled: true,
            },
        ];
        assert_eq!(
            resolve(&set, normalize_app("FIREFOX.desktop")).effective,
            "Browser"
        );
        set.rules[1].enabled = true;
        assert_eq!(resolve(&set, normalize_app("firefox")).effective, "Work");
        assert_eq!(resolve(&set, normalize_app("other")).effective, "Default");
        assert_eq!(resolve(&set, None).effective, "Default");
        set.manual_lock = true;
        assert_eq!(resolve(&set, normalize_app("firefox")).effective, "Default");
        assert!(
            resolve(&set, normalize_app("firefox"))
                .matched_rule
                .is_none()
        );
    }
    #[test]
    fn named_crud_keeps_selection_and_rule_references_consistent() {
        let mut set = ProfileSet::default();
        create_in(&mut set, "Work", None).unwrap();
        assert_eq!(set.selected, "Work");
        set.profiles.get_mut("Work").unwrap().dpi_presets = vec![800, 1600];
        create_in(&mut set, "Copy", Some("Work")).unwrap();
        assert_eq!(set.profiles["Copy"].dpi_presets, [800, 1600]);
        assert!(create_in(&mut set, "Copy", None).is_err());
        assert!(create_in(&mut set, " bad ", None).is_err());
        set.rules.push(AppRule {
            app_id: "editor".into(),
            profile: "Copy".into(),
            priority: 0,
            enabled: true,
        });
        rename_in(&mut set, "Copy", "Editor").unwrap();
        assert_eq!(set.selected, "Editor");
        assert_eq!(set.rules[0].profile, "Editor");
        assert!(rename_in(&mut set, "missing", "missing").is_err());
        select_in(&mut set, "Work").unwrap();
        assert_eq!(set.selected, "Work");
        delete_in(&mut set, "Editor").unwrap();
        assert!(set.rules.is_empty());
        delete_in(&mut set, "Work").unwrap();
        assert_eq!(set.selected, "Default");
        assert!(delete_in(&mut set, "Default").is_err());
        assert!(select_in(&mut set, "missing").is_err());
        validate_set(&set).unwrap();
    }
    #[test]
    fn pairing_serial_migrates_to_unit_identity_without_losing_legacy_profile() {
        let dir = isolated("identity");
        let file = dir.join("devices.json");
        edit_at(&file, "logitech:PAIRED", |p| p.dpi_presets = vec![1200]).unwrap();
        let device = DeviceSnapshot {
            serial: "UNIT".into(),
            identity: crate::model::DeviceIdentity {
                pairing_serial: Some("paired".into()),
                source: "unit-id".into(),
                stable: true,
                ..Default::default()
            },
            ..Default::default()
        };
        edit_set_at_alias(
            &file,
            &identity(&device).unwrap(),
            previous_identity(&device),
            |set| create_in(set, "Second", Some("Default")),
        )
        .unwrap();
        let store = read(&file).unwrap();
        assert!(!store.devices.contains_key("logitech:PAIRED"));
        assert_eq!(
            store.devices["logitech:UNIT"].profiles["Default"].dpi_presets,
            [1200]
        );
        assert_eq!(
            store.devices["logitech:UNIT"].profiles["Second"].dpi_presets,
            [1200]
        );
        fs::remove_file(file).unwrap();
        fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn imports_require_selection_mapping_and_never_enable_actions_or_restore() {
        let device = DeviceSnapshot {
            serial: "ABC".into(),
            buttons: vec![crate::model::ButtonInfo {
                cid: 80,
                divertable: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut profiles = ProfileSet::default();
        profiles
            .profiles
            .get_mut("Default")
            .unwrap()
            .actions_enabled = true;
        profiles.profiles.get_mut("Default").unwrap().restore = true;
        profiles.rules.push(AppRule {
            app_id: "editor".into(),
            profile: "Default".into(),
            priority: 5,
            enabled: true,
        });
        profiles
            .profiles
            .get_mut("Default")
            .unwrap()
            .bindings
            .insert(
                80,
                Binding {
                    tap: Some(Action::Workflow(crate::workflow::Workflow {
                        name: "Imported executable (never run by this test)".into(),
                        steps: vec![crate::workflow::Step::Launch {
                            program: "/usr/bin/true".into(),
                            args: vec![],
                            timeout_ms: 100,
                        }],
                    })),
                    ..Default::default()
                },
            );
        let json = serde_json::to_vec(&ExportBundle {
            schema_version: 2,
            device_id: "logitech:OTHER".into(),
            device_name: "Mouse".into(),
            profiles,
        })
        .unwrap();
        let preview = preview_json(&device, &json).unwrap();
        assert!(!preview.entries[0].matches_device);
        assert!(!preview.entries[0].profile.actions_enabled);
        assert!(!preview.entries[0].profile.restore);
        let dir = isolated("import");
        let file = dir.join("devices.json");
        edit_at(&file, "logitech:ABC", |p| p.dpi_presets = vec![800]).unwrap();
        let original = fs::read(&file).unwrap();
        assert!(import_at(&file, "logitech:ABC", &device, &preview, &[], true).is_err());
        assert!(import_at(&file, "logitech:ABC", &device, &preview, &[0], false).is_err());
        assert!(import_at(&file, "logitech:ABC", &device, &preview, &[0, 0], true).is_err());
        assert_eq!(fs::read(&file).unwrap(), original);
        let names = import_at(&file, "logitech:ABC", &device, &preview, &[0], true).unwrap();
        assert_ne!(names[0], "Default");
        let store = read(&file).unwrap();
        let set = &store.devices["logitech:ABC"];
        assert_eq!(set.profiles["Default"].dpi_presets, [800]);
        assert!(!set.profiles[&names[0]].actions_enabled);
        assert!(!set.profiles[&names[0]].restore);
        assert_eq!(set.rules.len(), 1);
        assert_eq!(set.rules[0].profile, names[0]);
        assert_eq!(set.selected, "Default");
        assert!(!set.manual_lock);
        fs::remove_file(file).unwrap();
        fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn solaar_real_yaml_sequence_and_mapping_translate_without_execution() {
        let device = DeviceSnapshot {
            serial: "ABC".into(),
            ..Default::default()
        };
        let sequence = br#"- _version: '1.1.13'
- _NAME: MX Master
  _serial: ABC
  dpi: 1600
  scroll-ratchet: 2
  smart-shift: 30
  hires-smooth-invert: false
  fn-swap: true
  reprogrammable-keys: {80: 81}
  divert-keys: {80: 1}
"#;
        let preview = preview_solaar(&device, sequence).unwrap();
        assert_eq!(preview.entries.len(), 1);
        let entry = &preview.entries[0];
        assert!(entry.matches_device);
        assert_eq!(entry.profile.saved.dpi, Some(1600));
        assert_eq!(entry.profile.saved.ratcheted, Some(true));
        assert_eq!(entry.profile.saved.button_remaps, [(80, 81)]);
        assert!(entry.profile.saved.button_diversions.is_empty());
        assert!(!entry.profile.restore && !entry.profile.actions_enabled);
        assert!(
            preview_solaar(&device, b"device: {_NAME: Mouse, _serial: ABC, dpi: 1200}").is_ok()
        );
        assert!(preview_solaar(&device, b"x: &x [*x]").is_err());
        assert!(preview_solaar(&device, b"[{_NAME: Mouse, dpi: -3}]").is_err());
        assert!(preview_solaar(&device, b"[{_NAME: Mouse, smart-shift: 256}]").is_err());
        assert!(preview_solaar(&device, &vec![b' '; 128 * 1024 + 1]).is_err());
        assert!(
            preview_solaar(
                &device,
                b"{_NAME: 'Bob''s & mouse', _serial: ABC, dpi: 1200}"
            )
            .is_ok()
        );
        assert!(
            preview_solaar(&device, b"{_NAME: Mouse, _unitId: ABC, dpi: 1200}")
                .unwrap()
                .entries[0]
                .matches_device
        );
    }
    #[test]
    fn yaml_budget_applies_during_alias_expansion_and_rejects_custom_tags() {
        let base = "base: &base [1,2,3,4,5,6,7,8,9,10]\n";
        let middle = format!("middle: &middle [{}]\n", vec!["*base"; 10].join(","));
        let huge = format!("huge: [{}]\n", vec!["*middle"; 200].join(","));
        assert!(bounded_yaml(&format!("{base}{middle}{huge}")).is_err());
        assert!(bounded_yaml("name: !executable anything").is_err());
        assert!(bounded_yaml("name: Mouse\nname: Other").is_err());
        assert!(bounded_yaml("---\nname: One\n---\nname: Two").is_err());
    }
    #[test]
    fn action_limits_and_capabilities_cover_every_trigger_and_ring() {
        let device = DeviceSnapshot::default();
        let binding: Binding = serde_json::from_str(r#"{"tap":"CycleDpi"}"#).unwrap();
        assert_eq!(binding.hold_ms, 600);
        assert_eq!(binding.double_tap_ms, 250);
        for action in [
            Action::CycleDpi,
            Action::SwitchHost(1),
            Action::Ring(vec![("DPI".into(), Action::CycleDpi)]),
        ] {
            assert!(action.validate_device(&device).is_err());
            for trigger in 0..3 {
                let mut b = Binding::default();
                match trigger {
                    0 => b.tap = Some(action.clone()),
                    1 => b.hold = Some(action.clone()),
                    _ => b.double_tap = Some(action.clone()),
                }
                assert!(!b.is_empty());
                assert!(b.validate().is_ok());
                assert!(
                    [&b.tap, &b.hold, &b.double_tap]
                        .into_iter()
                        .flatten()
                        .all(|a| a.validate_device(&device).is_err())
                );
            }
        }
        let mut action = Action::CycleDpi;
        for _ in 0..6 {
            action = Action::Ring(vec![("Nested".into(), action)]);
        }
        assert!(action.validate().is_err());
        assert!(Action::Ring(vec![]).validate().is_err());
        assert!(Action::SwitchHost(0).validate().is_err());
    }
    #[test]
    fn restore_ignore_and_no_manual_diversion() {
        let device = DeviceSnapshot {
            dpi: Some(800),
            dpi_feature: Some(1),
            fn_swap: Some(false),
            scroll_flags: Some(0),
            ..Default::default()
        };
        let profile = DeviceProfile {
            restore: true,
            restore_ignore: BTreeSet::from([RestoreSetting::Dpi]),
            saved: SettingsUpdate {
                dpi: Some(1600),
                fn_swap: Some(true),
                target_host: Some(2),
                scroll_diversion: Some(true),
                button_diversions: vec![(80, true)],
                ..Default::default()
            },
            ..Default::default()
        };
        let update = restore_update(&profile, &device);
        assert_eq!(update.dpi, None);
        assert_eq!(update.fn_swap, Some(true));
        assert_eq!(update.target_host, None);
        assert_eq!(update.scroll_diversion, None);
        assert!(update.button_diversions.is_empty());
    }
    #[test]
    fn partial_confirmed_save_never_populates_unknown_snapshot_values_or_commands() {
        let mut profile = DeviceProfile {
            saved: SettingsUpdate {
                fn_swap: Some(false),
                ..Default::default()
            },
            ..Default::default()
        };
        let confirmed = SettingsUpdate {
            dpi: Some(1600),
            target_host: Some(2),
            scroll_diversion: Some(true),
            thumbwheel_diverted: Some(true),
            button_diversions: vec![(80, true)],
            ..Default::default()
        };
        save_confirmed_in(&mut profile, &confirmed, true, vec![800, 1600]);
        assert_eq!(profile.saved.dpi, Some(1600));
        assert_eq!(profile.saved.fn_swap, Some(false)); // previously saved, not a new snapshot
        assert_eq!(profile.saved.invert_scroll, None);
        assert_eq!(profile.saved.backlight_level, None);
        assert_eq!(profile.saved.target_host, None);
        assert_eq!(profile.saved.scroll_diversion, None);
        assert_eq!(profile.saved.thumbwheel_diverted, None);
        assert!(profile.saved.button_diversions.is_empty());
        save_confirmed_in(&mut profile, &SettingsUpdate::default(), false, vec![]);
        assert!(crate::backend::settings_empty(&profile.saved));
    }
    #[test]
    fn restore_requires_full_confirmed_subset_not_just_a_successful_send() {
        let requested = SettingsUpdate {
            dpi: Some(1600),
            fn_swap: Some(false),
            ..Default::default()
        };
        let partial = SettingsUpdate {
            dpi: Some(1600),
            ..Default::default()
        };
        assert!(!confirmed_covers(&requested, &partial).unwrap());
        assert!(confirmed_covers(&requested, &requested).unwrap());
        assert!(
            !confirmed_covers(
                &SettingsUpdate {
                    target_host: Some(2),
                    ..Default::default()
                },
                &SettingsUpdate::default()
            )
            .unwrap()
        );
        let device = DeviceSnapshot {
            dpi_feature: Some(1),
            dpi: None,
            fn_swap: Some(false),
            ..Default::default()
        };
        let profile = DeviceProfile {
            restore: true,
            saved: requested,
            ..Default::default()
        };
        let update = restore_update(&profile, &device);
        assert_eq!(
            unconfirmed_restore_fields(&profile, &device, &update),
            ["dpi"]
        );
    }
    #[test]
    fn a_failed_setting_becoming_unreadable_is_not_marked_successful() {
        let device = DeviceSnapshot {
            serial: "unreadable".into(),
            online: true,
            dpi_feature: Some(1),
            dpi: Some(800),
            ..Default::default()
        };
        let mut report = ScanReport {
            receivers: vec![crate::model::ReceiverSnapshot {
                devices: vec![device.clone()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let load = |_: &DeviceSnapshot| {
            Ok(DeviceProfile {
                restore: true,
                saved: SettingsUpdate {
                    dpi: Some(1600),
                    ..Default::default()
                },
                ..Default::default()
            })
        };
        let mut state = RestoreState::default();
        let now = Instant::now();
        state.restore_report_at(
            &mut report,
            load,
            |_, _| bail!("simulated failed write"),
            now,
        );
        report.receivers[0].devices[0].dpi = None;
        state.restore_report_at(
            &mut report,
            load,
            |_, _| panic!("unreadable field must not be written"),
            now + Duration::from_secs(2),
        );
        let status = state.status(&device).unwrap();
        assert!(!status.completed);
        assert_eq!(status.attempts, 2);
        assert!(
            status
                .last_error
                .unwrap()
                .contains("could not be confirmed")
        );
        report.receivers[0].devices[0].dpi = Some(1600);
        state.restore_report_at(
            &mut report,
            load,
            |_, _| panic!("already matches"),
            now + Duration::from_secs(6),
        );
        assert!(state.status(&device).unwrap().completed);
    }
    #[test]
    fn retry_scheduler_stops_for_completed_or_exhausted_attempts() {
        let mut state = RestoreState::default();
        assert!(state.next_retry_delay().is_none());
        state.connections.insert(
            "fixture".into(),
            RestoreAttempt {
                fingerprint: vec![],
                next_attempt: Instant::now() + Duration::from_secs(10),
                status: RestoreStatus {
                    attempts: 1,
                    completed: false,
                    last_error: Some("retry".into()),
                },
            },
        );
        assert!(state.next_retry_delay().unwrap() <= Duration::from_secs(10));
        state.connections.get_mut("fixture").unwrap().next_attempt = Instant::now();
        assert_eq!(state.next_retry_delay(), Some(Duration::ZERO));
        state
            .connections
            .get_mut("fixture")
            .unwrap()
            .status
            .completed = true;
        assert!(state.next_retry_delay().is_none());
        let attempt = state.connections.get_mut("fixture").unwrap();
        attempt.status.completed = false;
        attempt.status.attempts = MAX_RESTORE_ATTEMPTS;
        assert!(state.next_retry_delay().is_none());
    }
    #[test]
    fn failed_restore_has_bounded_backoff_and_reconnect_or_reset_retries() {
        let device = DeviceSnapshot {
            serial: "retry".into(),
            online: true,
            dpi: Some(800),
            dpi_feature: Some(1),
            ..Default::default()
        };
        let mut report = ScanReport {
            receivers: vec![crate::model::ReceiverSnapshot {
                devices: vec![device.clone()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let load = |_: &DeviceSnapshot| {
            Ok(DeviceProfile {
                restore: true,
                saved: SettingsUpdate {
                    dpi: Some(1600),
                    ..Default::default()
                },
                ..Default::default()
            })
        };
        let mut state = RestoreState::default();
        let start = Instant::now();
        let mut calls = 0;
        let mut times = vec![];
        for second in 0..100 {
            state.restore_report_at(
                &mut report,
                load,
                |_, _| {
                    calls += 1;
                    times.push(second);
                    bail!("fake failed write")
                },
                start + Duration::from_secs(second),
            );
        }
        assert_eq!(calls, 4);
        assert_eq!(times, [0, 2, 6, 14]);
        let status = state.status(&device).unwrap();
        assert_eq!(status.attempts, 4);
        assert!(!status.completed);
        assert!(status.last_error.is_some());
        report.receivers[0].devices[0].online = false;
        state.restore_report_at(
            &mut report,
            load,
            |_, _| panic!("offline"),
            start + Duration::from_secs(101),
        );
        report.receivers[0].devices[0].online = true;
        state.restore_report_at(
            &mut report,
            load,
            |_, _| Ok(vec![]),
            start + Duration::from_secs(102),
        );
        assert!(state.status(&device).unwrap().completed);
        state.reset();
        assert!(state.status(&device).is_none());
        state.restore_report_at(
            &mut report,
            load,
            |_, _| Ok(vec![]),
            start + Duration::from_secs(103),
        );
        assert_eq!(state.status(&device).unwrap().attempts, 1);
    }
    #[test]
    fn restore_requires_opt_in_and_only_runs_on_connect_or_reset() {
        use crate::model::ReceiverSnapshot;
        let device = DeviceSnapshot {
            serial: "profile-test".into(),
            online: true,
            dpi: Some(800),
            dpi_feature: Some(1),
            ..Default::default()
        };
        let mut report = ScanReport {
            receivers: vec![ReceiverSnapshot {
                devices: vec![device],
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut state = RestoreState::default();
        let mut calls = 0;
        let enabled = |_: &DeviceSnapshot| {
            Ok(DeviceProfile {
                restore: true,
                saved: SettingsUpdate {
                    dpi: Some(1600),
                    ..Default::default()
                },
                ..Default::default()
            })
        };
        assert!(!state.restore_report(
            &mut report,
            |_| Ok(DeviceProfile::default()),
            |_, _| panic!("not opted in")
        ));
        state.reset();
        for _ in 0..2 {
            state.restore_report(&mut report, enabled, |_, update| {
                calls += 1;
                assert_eq!(update.dpi, Some(1600));
                Ok(vec![])
            });
        }
        assert_eq!(calls, 1);
        report.receivers[0].devices[0].online = false;
        state.restore_report(&mut report, enabled, |_, _| panic!("offline"));
        report.receivers[0].devices[0].online = true;
        state.restore_report(&mut report, enabled, |_, _| {
            calls += 1;
            Ok(vec![])
        });
        assert_eq!(calls, 2);
        state.reset();
        state.restore_report(&mut report, enabled, |_, _| {
            anyhow::bail!("simulated restore failure")
        });
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.contains("simulated restore failure"))
        );
    }
}
