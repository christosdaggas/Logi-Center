//! Battery policy and optional private, local history. Call `observe` on the scan
//! worker: it may read settings and atomically persist history. Notification
//! delivery is deliberately left to the GTK main thread/coordinator.
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::model::{DeviceSnapshot, ReceiverSnapshot};

const MAX_DEVICES: usize = 32;
const MAX_ALERT_DEVICES: usize = 256;
const HISTORY_FILE_LIMIT: u64 = 8 * 1024 * 1024;
const SAMPLE_INTERVAL: u64 = 300;
const DAY: u64 = 86_400;
// Coordinate explicit settings saves with worker history writes in this process.
static STORAGE_LOCK: Mutex<()> = Mutex::new(());

/// Private fields and checked deserialization prevent invalid thresholds from
/// reaching the monitor, including via hand-edited JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ConfigData", into = "ConfigData")]
pub struct Config {
    enabled: bool,
    low: u8,
    critical: u8,
    hysteresis: u8,
    history_enabled: bool,
    retention_days: u16,
    max_samples: usize,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct ConfigData {
    enabled: bool,
    low: u8,
    critical: u8,
    hysteresis: u8,
    history_enabled: bool,
    retention_days: u16,
    max_samples: usize,
}

impl Default for ConfigData {
    fn default() -> Self {
        Self {
            enabled: true,
            low: 20,
            critical: 5,
            hysteresis: 5,
            history_enabled: false,
            retention_days: 30,
            max_samples: 512,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        ConfigData::default().try_into().expect("valid defaults")
    }
}

impl TryFrom<ConfigData> for Config {
    type Error = anyhow::Error;
    fn try_from(value: ConfigData) -> Result<Self> {
        Self::new(
            value.enabled,
            value.low,
            value.critical,
            value.hysteresis,
            value.history_enabled,
            value.retention_days,
            value.max_samples,
        )
    }
}

impl From<Config> for ConfigData {
    fn from(value: Config) -> Self {
        Self {
            enabled: value.enabled,
            low: value.low,
            critical: value.critical,
            hysteresis: value.hysteresis,
            history_enabled: value.history_enabled,
            retention_days: value.retention_days,
            max_samples: value.max_samples,
        }
    }
}

impl Config {
    pub fn new(
        enabled: bool,
        low: u8,
        critical: u8,
        hysteresis: u8,
        history_enabled: bool,
        retention_days: u16,
        max_samples: usize,
    ) -> Result<Self> {
        if !(1..=80).contains(&low) || critical >= low {
            bail!("Critical must be lower than low; low must be between 1 and 80 percent");
        }
        if !(1..=20).contains(&hysteresis) || u16::from(low) + u16::from(hysteresis) > 100 {
            bail!("Recovery margin must be between 1 and 20 percentage points");
        }
        if !(1..=90).contains(&retention_days) || !(16..=1024).contains(&max_samples) {
            bail!("History must retain 1–90 days and 16–1024 samples per device");
        }
        Ok(Self {
            enabled,
            low,
            critical,
            hysteresis,
            history_enabled,
            retention_days,
            max_samples,
        })
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn low(&self) -> u8 {
        self.low
    }
    pub fn critical(&self) -> u8 {
        self.critical
    }
    pub fn hysteresis(&self) -> u8 {
        self.hysteresis
    }
    pub fn history_enabled(&self) -> bool {
        self.history_enabled
    }
    pub fn retention_days(&self) -> u16 {
        self.retention_days
    }
    pub fn max_samples(&self) -> usize {
        self.max_samples
    }

    pub fn load() -> Result<Self> {
        let (config, _) = paths()?;
        Self::load_from(&config)
    }

    fn load_from(path: &Path) -> Result<Self> {
        match read_json(path, 16 * 1024)? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .context("Battery settings are invalid; not overwritten"),
            None => Ok(Self::default()),
        }
    }

    /// Explicit user Save only. History opt-out also deletes previously saved
    /// history; the observing worker clears its in-memory copy on the next scan.
    pub fn save(&self) -> Result<()> {
        let (config, history) = paths()?;
        self.save_to(&config, &history)
    }

    fn save_to(&self, config: &Path, history: &Path) -> Result<()> {
        let _lock = STORAGE_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Commit opt-out before deletion, preventing future workers from
        // recreating history even if cleanup fails. A cleanup error is returned
        // explicitly; never report that deletion succeeded when it did not.
        crate::preferences::atomic_write(config, &serde_json::to_vec_pretty(self)?, 0o600)?;
        if !self.history_enabled {
            remove_history(history)?;
        }
        Ok(())
    }
}

fn paths() -> Result<(PathBuf, PathBuf)> {
    let base = crate::preferences::config_home()
        .context("No configuration folder available")?
        .join("logicenter");
    Ok((base.join("battery.json"), base.join("battery-history.json")))
}

fn remove_history(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("Could not remove private battery history"),
    }
}

fn read_json(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Could not read battery data"),
    };
    if !file.metadata()?.is_file() {
        bail!("Battery data must be a regular file");
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("Battery data exceeds its size limit");
    }
    Ok(Some(bytes))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Low,
    Critical,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alert {
    /// Stable profile identity, or a session-only endpoint identity when absent.
    pub device_id: String,
    pub device_name: String,
    pub percent: u8,
    pub severity: Severity,
}

impl Alert {
    /// No identifiers are exposed in the desktop notification ID.
    pub fn notification_id(&self) -> String {
        let hash = self
            .device_id
            .bytes()
            .fold(0xcbf29ce484222325u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
            });
        format!("battery-{hash:016x}")
    }

    /// Constructs but does NOT send. Coordinator calls on GTK main thread, then
    /// `app.send_notification(Some(&alert.notification_id()), &alert.notification())`.
    pub fn notification(&self) -> gtk4::gio::Notification {
        let title = match self.severity {
            Severity::Low => "Device battery is low",
            Severity::Critical => "Device battery is critically low",
        };
        let notification = gtk4::gio::Notification::new(title);
        notification.set_body(Some(&format!(
            "{}: {}% remaining",
            self.device_name, self.percent
        )));
        notification.set_priority(match self.severity {
            Severity::Low => gtk4::gio::NotificationPriority::Normal,
            Severity::Critical => gtk4::gio::NotificationPriority::Urgent,
        });
        notification
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Measurement {
    /// UNIX seconds, actual valid measurement time, never the disconnect time.
    pub timestamp: u64,
    pub percent: u8,
    /// HID++ status byte; None means unavailable, not known to be discharging.
    pub status: Option<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceHistory {
    pub identity: String,
    pub name: String,
    /// Current scan presence. Always false after loading from disk until observed.
    #[serde(skip)]
    pub online: bool,
    pub last: Option<Measurement>,
    pub samples: VecDeque<Measurement>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryFile {
    devices: Vec<DeviceHistory>,
}

#[derive(Default)]
struct Latch {
    low: bool,
    critical: bool,
    touched: u64,
}

pub struct Monitor {
    config: Config,
    config_path: Option<PathBuf>,
    history_path: Option<PathBuf>,
    history: BTreeMap<String, DeviceHistory>,
    latches: BTreeMap<String, Latch>,
    dirty: bool,
    last_flush: u64,
    disconnected: bool,
    error: Option<String>,
}

impl Monitor {
    /// Safe defaults on load failure; errors remain visible via `storage_error`.
    /// Run on a worker, not the GTK main loop.
    pub fn load() -> Self {
        match paths() {
            Ok((config, history)) => Self::load_from(config, history),
            Err(error) => {
                let mut monitor = Self::memory(Config::default());
                monitor.error = Some(error.to_string());
                monitor
            }
        }
    }

    pub fn memory(config: Config) -> Self {
        Self {
            config,
            config_path: None,
            history_path: None,
            history: BTreeMap::new(),
            latches: BTreeMap::new(),
            dirty: false,
            last_flush: 0,
            disconnected: false,
            error: None,
        }
    }

    fn load_from(config_path: PathBuf, history_path: PathBuf) -> Self {
        let loaded = Config::load_from(&config_path);
        let mut monitor = Self::memory(loaded.as_ref().cloned().unwrap_or_default());
        if let Err(error) = loaded {
            monitor.error = Some(error.to_string());
        }
        if monitor.config.history_enabled {
            let loaded = (|| -> Result<HistoryFile> {
                match read_json(&history_path, HISTORY_FILE_LIMIT)? {
                    Some(bytes) => Ok(serde_json::from_slice(&bytes)?),
                    None => Ok(HistoryFile::default()),
                }
            })();
            match loaded {
                Ok(file) => {
                    for mut device in file.devices.into_iter().take(MAX_DEVICES) {
                        if device.identity.is_empty() || device.identity.len() > 512 {
                            continue;
                        }
                        device.name = clean_name(&device.name);
                        device.online = false;
                        monitor.history.insert(device.identity.clone(), device);
                    }
                    monitor.prune(now());
                }
                Err(error) => {
                    monitor.error = Some(format!("Could not load private battery history: {error}"))
                }
            }
        }
        monitor.config_path = Some(config_path);
        monitor.history_path = Some(history_path);
        monitor
    }

    pub fn config(&self) -> &Config {
        &self.config
    }
    pub fn storage_error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn history(&self) -> impl Iterator<Item = &DeviceHistory> {
        self.history.values()
    }
    pub fn history_for(&self, identity: &str) -> Option<&DeviceHistory> {
        self.history.get(identity)
    }

    /// Call once for each completed scan, including empty scans. Settings saved
    /// by the panel are reloaded automatically. Unknown, >100%, offline and
    /// invalid-battery/error measurements never alert or refresh history.
    /// Charging/almost-full/full/slow-charge suppress alerts WITHOUT rearming;
    /// only recovery by `hysteresis` percentage points rearms each threshold.
    /// Missing/unrecognized status is accepted if the percentage itself is
    /// known; the coarse `battery_level` field is never guessed as a percentage.
    /// Alerts retain at most 256 identities per session; additional identities
    /// are not alerted rather than evicting latches and creating repeat spam.
    pub fn observe(&mut self, receivers: &[ReceiverSnapshot]) -> Vec<Alert> {
        self.reload_config();
        let timestamp = now();
        let alerts = self.observe_at(receivers, timestamp);
        // Bound routine disk writes to once per five minutes. Disconnects and
        // an explicit shutdown flush preserve the freshest last-known time.
        if self.disconnected || timestamp.saturating_sub(self.last_flush) >= SAMPLE_INTERVAL {
            if let Err(error) = self.flush() {
                self.error = Some(error.to_string());
            }
        }
        alerts
    }

    fn reload_config(&mut self) {
        let Some(path) = &self.config_path else {
            return;
        };
        match Config::load_from(path) {
            Ok(config) => {
                if !config.history_enabled && !self.history.is_empty() {
                    self.history.clear();
                    self.dirty = false;
                }
                self.config = config;
            }
            Err(error) => self.error = Some(error.to_string()), // retain last valid policy
        }
    }

    fn observe_at(&mut self, receivers: &[ReceiverSnapshot], timestamp: u64) -> Vec<Alert> {
        let mut alerts = Vec::new();
        let previously_online: HashSet<_> = self
            .history
            .iter()
            .filter(|(_, history)| history.online)
            .map(|(id, _)| id.clone())
            .collect();
        for device in self.history.values_mut() {
            device.online = false;
        }
        let mut seen = HashSet::new();
        for device in receivers.iter().flat_map(|receiver| &receiver.devices) {
            let stable = crate::profiles::identity(device).filter(|id| id.len() <= 512);
            let id = stable.clone().unwrap_or_else(|| {
                let hash = device
                    .receiver_path
                    .bytes()
                    .fold(0xcbf29ce484222325u64, |hash, byte| {
                        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
                    });
                format!("session:{hash:016x}:{}", device.number)
            });
            if let Some(history) = self.history.get_mut(&id) {
                history.online |= device.online;
            }
            let Some(percent) = valid_percent(device) else {
                continue;
            };
            if !self.latches.contains_key(&id) && self.latches.len() >= MAX_ALERT_DEVICES {
                if self.config.history_enabled && stable.is_some() {
                    self.record(&id, &device.name, percent, device.battery_status, timestamp);
                }
                continue;
            }
            if !seen.insert(id.clone()) {
                continue;
            }
            let latch = self.latches.entry(id.clone()).or_default();
            latch.touched = timestamp;
            if percent >= self.config.low + self.config.hysteresis {
                latch.low = false;
            }
            if percent >= self.config.critical + self.config.hysteresis {
                latch.critical = false;
            }
            let charging = matches!(device.battery_status, Some(1..=4));
            let severity = if !self.config.enabled || charging {
                None
            } else if percent <= self.config.critical && !latch.critical {
                latch.critical = true;
                latch.low = true;
                Some(Severity::Critical)
            } else if percent <= self.config.low && !latch.low {
                latch.low = true;
                Some(Severity::Low)
            } else {
                None
            };
            if let Some(severity) = severity {
                alerts.push(Alert {
                    device_id: id.clone(),
                    device_name: clean_name(&device.name),
                    percent,
                    severity,
                });
            }
            if self.config.history_enabled && stable.is_some() {
                self.record(&id, &device.name, percent, device.battery_status, timestamp);
            }
        }
        self.disconnected = self
            .history
            .iter()
            .any(|(id, history)| previously_online.contains(id) && !history.online);
        self.prune(timestamp);
        alerts
    }

    fn record(&mut self, id: &str, name: &str, percent: u8, status: Option<u8>, timestamp: u64) {
        if !self.history.contains_key(id) && self.history.len() >= MAX_DEVICES {
            if let Some(oldest) = self
                .history
                .iter()
                .min_by_key(|(_, history)| {
                    history
                        .last
                        .as_ref()
                        .map(|last| last.timestamp)
                        .unwrap_or(0)
                })
                .map(|(id, _)| id.clone())
            {
                self.history.remove(&oldest);
            }
        }
        let history = self
            .history
            .entry(id.to_owned())
            .or_insert_with(|| DeviceHistory {
                identity: id.to_owned(),
                name: clean_name(name),
                ..Default::default()
            });
        history.online = true;
        history.name = clean_name(name);
        let measurement = Measurement {
            timestamp,
            percent,
            status,
        };
        // Last-known time is the actual most recent valid reading; stored
        // history samples are coalesced when unchanged to bound sample growth.
        history.last = Some(measurement.clone());
        self.dirty = true;
        let sample = history.samples.back().is_none_or(|last| {
            last.percent != percent
                || last.status != status
                || timestamp.saturating_sub(last.timestamp) >= SAMPLE_INTERVAL
        });
        if sample {
            history.samples.push_back(measurement);
        }
    }

    fn prune(&mut self, timestamp: u64) {
        let cutoff = timestamp.saturating_sub(u64::from(self.config.retention_days) * DAY);
        self.history.retain(|_, history| {
            let previous = history.samples.len();
            history.samples.retain(|sample| {
                sample.percent <= 100
                    && !matches!(sample.status, Some(5..=7))
                    && sample.timestamp >= cutoff
                    && sample.timestamp <= timestamp
            });
            while history.samples.len() > self.config.max_samples {
                history.samples.pop_front();
            }
            if previous != history.samples.len() {
                self.dirty = true;
            }
            if history.last.as_ref().is_some_and(|last| {
                last.percent > 100
                    || matches!(last.status, Some(5..=7))
                    || last.timestamp < cutoff
                    || last.timestamp > timestamp
            }) {
                history.last = history.samples.back().cloned();
                self.dirty = true;
            }
            let keep = history.last.is_some();
            if !keep {
                self.dirty = true;
            }
            keep
        });
        self.latches
            .retain(|_, latch| timestamp.saturating_sub(latch.touched) <= 90 * DAY);
    }

    /// Explicit flush is useful before orderly shutdown. Does not write when
    /// history is disabled or when this is an in-memory test monitor.
    pub fn flush(&mut self) -> Result<()> {
        if !self.config.history_enabled || !self.dirty {
            return Ok(());
        }
        let Some(path) = self.history_path.clone() else {
            return Ok(());
        };
        let _lock = STORAGE_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // A user may have opted out after this scan began. Recheck under the
        // same lock used by Save so a stale worker cannot recreate deleted data.
        if let Some(config_path) = &self.config_path {
            let current = Config::load_from(config_path)?;
            self.config = current;
            if !self.config.history_enabled {
                self.history.clear();
                self.dirty = false;
                return remove_history(&path);
            }
            self.prune(now());
        }
        let file = HistoryFile {
            devices: self.history.values().cloned().collect(),
        };
        let bytes = serde_json::to_vec(&file)?;
        if bytes.len() as u64 > HISTORY_FILE_LIMIT {
            bail!("Battery history exceeds its storage limit");
        }
        crate::preferences::atomic_write(&path, &bytes, 0o600)?;
        // Also protect against an opt-out saved by another process during our
        // write; process-local locking alone would not close that race.
        if let Some(config_path) = &self.config_path {
            let current = Config::load_from(config_path)?;
            if !current.history_enabled {
                self.config = current;
                self.history.clear();
                self.dirty = false;
                return remove_history(&path);
            }
        }
        self.dirty = false;
        self.last_flush = now();
        self.disconnected = false;
        Ok(())
    }
}

fn valid_percent(device: &DeviceSnapshot) -> Option<u8> {
    if !device.online || matches!(device.battery_status, Some(5..=7)) {
        return None;
    }
    device.battery.filter(|percent| *percent <= 100)
}

fn clean_name(name: &str) -> String {
    let name: String = name.chars().filter(|c| !c.is_control()).take(100).collect();
    if name.trim().is_empty() {
        "Logitech device".to_owned()
    } else {
        name
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn scan(percent: Option<u8>, status: Option<u8>, online: bool) -> Vec<ReceiverSnapshot> {
        vec![ReceiverSnapshot {
            devices: vec![DeviceSnapshot {
                serial: "BATTERY-TEST-1234".into(),
                name: "Test mouse".into(),
                battery: percent,
                battery_status: status,
                online,
                ..Default::default()
            }],
            ..Default::default()
        }]
    }
    fn config(history: bool) -> Config {
        Config::new(true, 20, 5, 5, history, 1, 16).unwrap()
    }

    #[test]
    fn battery_config_rejects_boundaries_and_unchecked_json() {
        assert!(Config::new(true, 20, 20, 5, false, 30, 512).is_err());
        assert!(Config::new(true, 81, 5, 5, false, 30, 512).is_err());
        assert!(Config::new(true, 20, 5, 0, false, 30, 512).is_err());
        assert!(Config::new(true, 20, 5, 5, false, 0, 512).is_err());
        assert!(Config::new(true, 20, 5, 5, false, 91, 512).is_err());
        assert!(Config::new(true, 20, 5, 5, false, 30, 1025).is_err());
        assert!(serde_json::from_str::<Config>(r#"{"low":5,"critical":10}"#).is_err());
        assert_eq!(
            serde_json::from_str::<Config>("{}").unwrap(),
            Config::default()
        );
    }

    #[test]
    fn battery_alerts_are_edge_triggered_with_independent_hysteresis() {
        let mut monitor = Monitor::memory(config(false));
        let observe = |monitor: &mut Monitor, value| {
            monitor.observe_at(&scan(Some(value), Some(0), true), 100)
        };
        assert!(observe(&mut monitor, 21).is_empty());
        assert_eq!(observe(&mut monitor, 20)[0].severity, Severity::Low);
        assert!(observe(&mut monitor, 19).is_empty());
        assert_eq!(observe(&mut monitor, 5)[0].severity, Severity::Critical);
        assert!(observe(&mut monitor, 6).is_empty());
        assert!(observe(&mut monitor, 9).is_empty());
        assert!(observe(&mut monitor, 10).is_empty());
        assert_eq!(observe(&mut monitor, 5)[0].severity, Severity::Critical);
        assert!(observe(&mut monitor, 24).is_empty());
        assert!(observe(&mut monitor, 20).is_empty());
        assert!(observe(&mut monitor, 25).is_empty());
        assert_eq!(observe(&mut monitor, 20)[0].severity, Severity::Low);
    }

    #[test]
    fn battery_charging_unknown_invalid_and_offline_do_not_spam() {
        let mut monitor = Monitor::memory(config(true));
        assert_eq!(
            monitor.observe_at(&scan(Some(3), Some(0), true), 100).len(),
            1
        );
        for status in [1, 2, 3, 4] {
            assert!(
                monitor
                    .observe_at(&scan(Some(3), Some(status), true), 101)
                    .is_empty()
            );
        }
        assert!(
            monitor
                .observe_at(&scan(Some(3), Some(0), true), 102)
                .is_empty()
        );
        for (percent, status, online) in [
            (None, None, true),
            (Some(255), None, true),
            (Some(1), Some(5), true),
            (Some(1), Some(0), false),
        ] {
            assert!(
                monitor
                    .observe_at(&scan(percent, status, online), 200)
                    .is_empty()
            );
        }
        let last = monitor.history().next().unwrap().last.as_ref().unwrap();
        assert_eq!((last.timestamp, last.percent), (102, 3));
        assert!(!monitor.history().next().unwrap().online);
        assert!(monitor.observe_at(&[], 201).is_empty());
        assert_eq!(
            monitor
                .history()
                .next()
                .unwrap()
                .last
                .as_ref()
                .unwrap()
                .timestamp,
            102
        );
    }

    #[test]
    fn battery_history_is_opt_in_bounded_and_expires() {
        let mut monitor = Monitor::memory(config(false));
        monitor.observe_at(&scan(Some(40), None, true), 100);
        assert_eq!(monitor.history().count(), 0);
        monitor.config = config(true);
        for time in 100..150 {
            monitor.observe_at(&scan(Some((time % 100) as u8), None, true), time);
        }
        assert_eq!(monitor.history().next().unwrap().samples.len(), 16);
        let mut receivers = scan(Some(50), None, true);
        for index in 0..50 {
            receivers[0].devices[0].serial = format!("TEST-{index}");
            monitor.observe_at(&receivers, 200 + index);
        }
        assert_eq!(monitor.history().count(), MAX_DEVICES);
        assert!(monitor.latches.len() <= MAX_ALERT_DEVICES);
        monitor.observe_at(&[], DAY + 500);
        assert_eq!(monitor.history().count(), 0);
    }

    #[test]
    fn battery_notification_and_history_optins_are_independent_and_require_stable_history_ids() {
        let mut monitor = Monitor::memory(Config::new(false, 20, 5, 5, true, 1, 16).unwrap());
        assert!(
            monitor
                .observe_at(&scan(Some(2), None, true), 100)
                .is_empty()
        );
        assert_eq!(monitor.history().count(), 1); // independent history opt-in
        let mut monitor = Monitor::memory(config(true));
        let mut unknown = scan(Some(2), None, true);
        unknown[0].devices[0].serial.clear();
        assert_eq!(monitor.observe_at(&unknown, 100).len(), 1); // session-only alert
        assert_eq!(monitor.history().count(), 0);
        assert!(monitor.observe_at(&unknown, 101).is_empty());
    }

    #[test]
    fn battery_coalesces_samples_but_preserves_latest_valid_offline_time() {
        let mut monitor = Monitor::memory(config(true));
        let mut duplicate = scan(Some(80), None, true);
        let mut offline = duplicate[0].devices[0].clone();
        offline.online = false;
        duplicate[0].devices.insert(0, offline);
        monitor.observe_at(&duplicate, 100);
        monitor.observe_at(&scan(Some(80), None, true), 200);
        monitor.observe_at(&[], 201);
        let history = monitor.history().next().unwrap();
        assert_eq!(history.samples.len(), 1);
        assert_eq!(history.last.as_ref().unwrap().timestamp, 200);
        assert!(!history.online);
        assert!(monitor.disconnected);
        monitor.observe_at(&scan(Some(80), None, true), 400);
        assert_eq!(monitor.history().next().unwrap().samples.len(), 2);
    }

    #[test]
    fn battery_alert_state_capacity_does_not_evict_and_repeat() {
        let mut monitor = Monitor::memory(config(false));
        let mut receivers = scan(Some(2), None, true);
        for index in 0..MAX_ALERT_DEVICES + 10 {
            receivers[0].devices[0].serial = format!("CAPACITY-{index}");
            let alerts = monitor.observe_at(&receivers, 100);
            assert_eq!(alerts.len(), usize::from(index < MAX_ALERT_DEVICES));
        }
        assert_eq!(monitor.latches.len(), MAX_ALERT_DEVICES);
        receivers[0].devices[0].serial = "CAPACITY-0".into();
        assert!(monitor.observe_at(&receivers, 101).is_empty());
    }

    #[test]
    fn battery_files_are_isolated_optout_deletes_history_and_reload_is_safe() {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "logicenter-battery-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        let settings = root.join("settings.json");
        let history = root.join("history.json");
        config(true).save_to(&settings, &history).unwrap();
        let mut monitor = Monitor::load_from(settings.clone(), history.clone());
        monitor.observe_at(&scan(Some(33), None, true), now());
        monitor.flush().unwrap();
        let loaded = Monitor::load_from(settings.clone(), history.clone());
        assert_eq!(loaded.history().count(), 1);
        assert!(!loaded.history().next().unwrap().online);
        std::fs::write(&settings, r#"{"low":1,"critical":20}"#).unwrap();
        monitor.reload_config();
        assert_eq!(monitor.config(), &config(true));
        assert!(monitor.storage_error().is_some());
        monitor.observe_at(&scan(Some(34), None, true), now());
        config(false).save_to(&settings, &history).unwrap();
        monitor.flush().unwrap(); // stale worker must not recreate opted-out data
        assert!(!history.exists());
        assert_eq!(monitor.history().count(), 0);
        monitor.reload_config();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&settings).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
