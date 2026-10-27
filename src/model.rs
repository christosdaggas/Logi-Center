use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Compatible with Solaar's per-device YAML configuration.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DeviceConfig {
    #[serde(rename = "_NAME")]
    pub name: Option<String>,
    #[serde(rename = "_serial")]
    pub serial: Option<String>,
    #[serde(rename = "_wpid")]
    pub wpid: Option<serde_yaml::Value>,
    pub dpi: Option<u16>,
    #[serde(rename = "scroll-ratchet")]
    pub scroll_ratchet: Option<u8>,
    #[serde(rename = "smart-shift")]
    pub smart_shift: Option<u8>,
    #[serde(rename = "scroll-ratchet-torque")]
    pub scroll_ratchet_torque: Option<u8>,
    #[serde(rename = "hires-scroll-mode")]
    pub hires_scroll_mode: Option<bool>,
    #[serde(rename = "hires-smooth-invert")]
    pub hires_smooth_invert: Option<bool>,
    #[serde(rename = "hires-smooth-resolution")]
    pub hires_smooth_resolution: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SmartShift {
    /// 1 is free-spinning, 2 is ratcheted.
    pub mode: u8,
    pub threshold: u8,
    pub torque: Option<u8>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ButtonInfo {
    pub cid: u16,
    pub name: String,
    pub task: u16,
    pub diverted: bool,
    pub mapped_to: u16,
    pub remappable: bool,
    pub divertable: bool,
    pub raw_xy: bool,
    #[serde(default)]
    pub reporting_available: bool,
    #[serde(default)]
    pub group: u8,
    #[serde(default)]
    pub group_mask: u8,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct HostInfo {
    pub count: u8,
    pub current: u8,
    pub names: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct BacklightSnapshot {
    pub enabled: bool,
    /// Automatic is 1, manual is 3.
    pub mode: u8,
    pub level: u8,
    pub max_level: u8,
    pub hands_out: u16,
    pub hands_in: u16,
    pub powered: u16,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DisableKeysSnapshot {
    pub supported: u8,
    pub disabled: u8,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Usb,
    Bluetooth,
    Unifying,
    Bolt,
    Receiver,
    #[default]
    Unknown,
}

/// Unsupported means a successful complete inventory proved feature absence.
/// A timeout, permissions failure or incomplete inventory remains unavailable.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Supported,
    Unsupported,
    Unavailable,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Capability {
    pub state: CapabilityState,
    pub feature_id: Option<u16>,
    pub feature_index: Option<u8>,
    pub version: Option<u8>,
    pub writable: bool,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct FeatureInfo {
    pub id: u16,
    pub index: u8,
    pub flags: u8,
    pub version: u8,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct FirmwareInfo {
    pub layer: u8,
    pub kind: u8,
    pub name: String,
    pub version: String,
    pub extra: Vec<u8>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct DeviceIdentity {
    pub unit_id: Option<String>,
    pub model_id: Option<String>,
    pub hid_uniq: Option<String>,
    /// Receiver pairing fingerprint, preserved when unit ID supersedes serial.
    pub pairing_serial: Option<String>,
    pub source: String,
    pub stable: bool,
    pub transport_ids: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ThumbwheelSnapshot {
    pub diverted: bool,
    pub inverted: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ReportRateSnapshot {
    /// Explicit period, not frequency; extended protocol supports 125 us.
    pub current_us: u32,
    pub supported_us: Vec<u32>,
    pub extended: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PlatformDescriptor {
    pub id: u8,
    pub os_flags: u16,
    pub version_low: u16,
    pub version_high: u16,
    pub name: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct OsLayoutSnapshot {
    pub current: u8,
    pub settable: bool,
    pub dual_platform: bool,
    pub platforms: Vec<PlatformDescriptor>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ReceiverCapabilities {
    pub family: String,
    pub pairing: bool,
    pub unpairing: bool,
    pub max_slots: usize,
    pub capacity_source: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct DeviceSnapshot {
    pub receiver_path: String,
    pub receiver_name: String,
    pub number: u8,
    pub wpid: String,
    pub kind: String,
    pub serial: String,
    pub identity: DeviceIdentity,
    pub transport: Transport,
    pub features: Vec<FeatureInfo>,
    pub firmware: Vec<FirmwareInfo>,
    pub capabilities: BTreeMap<String, Capability>,
    pub errors: Vec<String>,
    pub name: String,
    pub codename: String,
    pub online: bool,
    pub battery: Option<u8>,
    pub battery_level: Option<u8>,
    pub battery_status: Option<u8>,
    pub dpi: Option<u16>,
    pub dpi_values: Vec<u16>,
    pub scroll_flags: Option<u8>,
    pub smooth_scroll: Option<bool>,
    pub smart_shift: Option<SmartShift>,
    pub buttons: Vec<ButtonInfo>,
    pub host: Option<HostInfo>,
    pub fn_swap: Option<bool>,
    pub backlight: Option<BacklightSnapshot>,
    pub disable_keys: Option<DisableKeysSnapshot>,
    pub multiplatform: Option<u8>,
    pub thumbwheel: Option<ThumbwheelSnapshot>,
    pub report_rate: Option<ReportRateSnapshot>,
    pub os_layout: Option<OsLayoutSnapshot>,
    pub feature_count: usize,
    pub dpi_feature: Option<u8>,
    pub hires_feature: Option<u8>,
    pub smooth_scroll_feature: Option<u8>,
    pub smart_shift_feature: Option<u8>,
    pub buttons_feature: Option<u8>,
    pub host_feature: Option<u8>,
    pub config: Option<DeviceConfig>,
}

impl DeviceSnapshot {
    /// Stable across hidraw renumbering. `key()` remains the live endpoint key
    /// for UI/event routing; use this optional key for per-unit persistence.
    pub fn stable_key(&self) -> Option<String> {
        if !self.identity.stable || self.serial.is_empty() {
            return None;
        }
        Some(format!("logitech:{}", self.serial))
    }

    pub fn key(&self) -> String {
        format!("{}:{}", self.receiver_path, self.number)
    }

    pub fn battery_text(&self) -> String {
        let state = self.battery_status.map(|status| match status {
            0x00 => "discharging",
            0x01 => "recharging",
            0x02 => "almost full",
            0x03 => "full",
            0x04 => "slow recharge",
            0x05 => "invalid battery",
            0x06 => "thermal error",
            0x07 => "charging error",
            _ => "unknown status",
        });
        match (self.battery, state) {
            (Some(level), Some(state)) => format!("{level}% · {state}"),
            (Some(level), None) => format!("{level}%"),
            (None, Some(state)) => format!("{state} · percentage unavailable"),
            (None, None) => "not available".to_string(),
        }
    }

    pub fn battery_is_charging(&self) -> bool {
        matches!(self.battery_status, Some(0x01 | 0x02 | 0x04))
    }

    pub fn scroll_inverted(&self) -> Option<bool> {
        self.scroll_flags.map(|flags| flags & 0x04 != 0)
    }

    pub fn scroll_high_resolution(&self) -> Option<bool> {
        self.scroll_flags.map(|flags| flags & 0x02 != 0)
    }

    pub fn scroll_diversion(&self) -> Option<bool> {
        self.scroll_flags.map(|flags| flags & 0x01 != 0)
    }

    pub fn is_ratcheted(&self) -> Option<bool> {
        self.smart_shift.as_ref().map(|s| s.mode == 2)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ReceiverSnapshot {
    pub path: String,
    pub name: String,
    pub product_id: u16,
    pub serial: String,
    pub max_devices: usize,
    pub direct: bool,
    pub transport: Transport,
    pub capabilities: ReceiverCapabilities,
    pub errors: Vec<String>,
    pub devices: Vec<DeviceSnapshot>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ScanReport {
    pub receivers: Vec<ReceiverSnapshot>,
    pub errors: Vec<String>,
}
