use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use log::debug;
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, MutexGuard};

use crate::config::ConfigStore;
use crate::hidpp::{Hidpp, RECEIVER_DEVICE_NUMBER};
use crate::hidraw::{self, HidrawInterface};
use crate::model::{
    BacklightSnapshot, ButtonInfo, Capability, CapabilityState, DeviceSnapshot,
    DisableKeysSnapshot, FeatureInfo, FirmwareInfo, HostInfo, OsLayoutSnapshot, PlatformDescriptor,
    ReceiverCapabilities, ReceiverSnapshot, ReportRateSnapshot, ScanReport, SmartShift,
    ThumbwheelSnapshot, Transport,
};

pub const FEATURE_FEATURE_SET: u16 = 0x0001;
pub const FEATURE_DEVICE_INFO: u16 = 0x0003;
pub const FEATURE_DEVICE_NAME: u16 = 0x0005;
pub const FEATURE_BATTERY_STATUS: u16 = 0x1000;
pub const FEATURE_UNIFIED_BATTERY: u16 = 0x1004;
pub const FEATURE_CHANGE_HOST: u16 = 0x1814;
pub const FEATURE_HOSTS_INFO: u16 = 0x1815;
pub const FEATURE_REPROG_CONTROLS_V4: u16 = 0x1B04;
pub const FEATURE_BACKLIGHT2: u16 = 0x1982;
pub const FEATURE_SMART_SHIFT: u16 = 0x2110;
pub const FEATURE_SMART_SHIFT_ENHANCED: u16 = 0x2111;
pub const FEATURE_HI_RES_SCROLLING: u16 = 0x2120;
pub const FEATURE_HIRES_WHEEL: u16 = 0x2121;
pub const FEATURE_THUMB_WHEEL: u16 = 0x2150;
pub const FEATURE_ADJUSTABLE_DPI: u16 = 0x2201;
pub const FEATURE_FN_INVERSION: u16 = 0x40A0;
pub const FEATURE_NEW_FN_INVERSION: u16 = 0x40A2;
pub const FEATURE_KEYBOARD_DISABLE_KEYS: u16 = 0x4521;
pub const FEATURE_MULTIPLATFORM: u16 = 0x4531;
pub const FEATURE_DUALPLATFORM: u16 = 0x4530;
pub const FEATURE_REPORT_RATE: u16 = 0x8060;
pub const FEATURE_EXTENDED_REPORT_RATE: u16 = 0x8061;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SettingsUpdate {
    pub dpi: Option<u16>,
    pub invert_scroll: Option<bool>,
    pub high_resolution_scroll: Option<bool>,
    pub scroll_diversion: Option<bool>,
    pub smooth_scroll: Option<bool>,
    pub ratcheted: Option<bool>,
    pub smart_shift_threshold: Option<u8>,
    pub smart_shift_torque: Option<u8>,
    /// Transient command only. Never emitted into saved profile JSON/YAML.
    #[serde(skip_serializing)]
    pub target_host: Option<u8>,
    pub button_diversions: Vec<(u16, bool)>,
    pub button_remaps: Vec<(u16, u16)>,
    pub fn_swap: Option<bool>,
    pub backlight_enabled: Option<bool>,
    pub backlight_mode: Option<u8>,
    pub backlight_level: Option<u8>,
    pub backlight_durations: Option<(u16, u16, u16)>,
    pub disable_keys: Option<u8>,
    pub thumbwheel_inverted: Option<bool>,
    pub thumbwheel_diverted: Option<bool>,
    /// Report period in microseconds (not Hz).
    pub report_rate_us: Option<u32>,
    /// Device-provided platform ID, not an OS enum shared between models.
    pub os_layout: Option<u8>,
}

static HARDWARE: Mutex<()> = Mutex::new(());
thread_local! {
    static HARDWARE_HELD: std::cell::RefCell<(usize, Option<MutexGuard<'static, ()>>)> = const { std::cell::RefCell::new((0, None)) };
}

/// Thread-bound, reentrant process-wide hardware guard. The underlying mutex
/// stays held until the last nested guard drops, even with out-of-order drops.
pub(crate) struct HardwareGuard {
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Drop for HardwareGuard {
    fn drop(&mut self) {
        HARDWARE_HELD.with(|held| {
            let mut held = held.borrow_mut();
            held.0 -= 1;
            if held.0 == 0 {
                held.1.take();
            }
        });
    }
}
pub fn settings_empty(update: &SettingsUpdate) -> bool {
    update.dpi.is_none()
        && update.invert_scroll.is_none()
        && update.high_resolution_scroll.is_none()
        && update.scroll_diversion.is_none()
        && update.smooth_scroll.is_none()
        && update.ratcheted.is_none()
        && update.smart_shift_threshold.is_none()
        && update.smart_shift_torque.is_none()
        && update.target_host.is_none()
        && update.button_diversions.is_empty()
        && update.button_remaps.is_empty()
        && update.fn_swap.is_none()
        && update.backlight_enabled.is_none()
        && update.backlight_mode.is_none()
        && update.backlight_level.is_none()
        && update.backlight_durations.is_none()
        && update.disable_keys.is_none()
        && update.thumbwheel_inverted.is_none()
        && update.thumbwheel_diverted.is_none()
        && update.report_rate_us.is_none()
        && update.os_layout.is_none()
}
pub(crate) fn hardware_lock() -> HardwareGuard {
    HARDWARE_HELD.with(|held| {
        let mut held = held.borrow_mut();
        if held.0 == 0 {
            held.1 = Some(HARDWARE.lock().unwrap_or_else(|error| error.into_inner()));
        }
        held.0 += 1;
    });
    HardwareGuard {
        _not_send: std::marker::PhantomData,
    }
}

fn read_dpi_values(hp: &mut Hidpp, number: u8, index: u8) -> Vec<u16> {
    let mut bytes = Vec::new();
    for page in 0..8 {
        let Some(reply) = hp.read_feature(number, index, 0x10, &[0, 0, page]) else {
            break;
        };
        if reply.len() < 3 {
            break;
        }
        if let Some(values) = crate::dpi::append_page(&mut bytes, &reply) {
            return values;
        }
    }
    Vec::new()
}

pub fn scan() -> ScanReport {
    let _lock = hardware_lock();
    let config = ConfigStore::load();
    let (interfaces, enumeration_errors) = hidraw::logitech_interfaces_with_errors();
    let mut results: Vec<(usize, Result<ReceiverSnapshot>)> = Vec::new();

    // Each receiver/device lives on its own hidraw node, so probe all nodes in
    // parallel instead of paying every timeout one after another.
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (index, interface) in interfaces.into_iter().enumerate() {
            let config = config.clone();
            handles.push(scope.spawn(move || {
                debug!(
                    "probing {} ({:04X}:{:04X}, {})",
                    interface.path.display(),
                    interface.vendor_id,
                    interface.product_id,
                    interface.name
                );
                let result = if looks_like_receiver(&interface) {
                    scan_receiver(&interface, &config)
                } else {
                    scan_direct(&interface, &config)
                };
                if let Err(error) = &result {
                    debug!(
                        "{} is not a usable HID++ endpoint: {error:#}",
                        interface.path.display()
                    );
                }
                (
                    index,
                    result.with_context(|| interface.path.display().to_string()),
                )
            }));
        }
        for handle in handles {
            results.push(
                handle
                    .join()
                    .unwrap_or_else(|_| (usize::MAX, Err(anyhow!("HID++ scan worker panicked")))),
            );
        }
    });

    results.sort_by_key(|(index, _)| *index);
    let mut report = ScanReport::default();
    report.errors = enumeration_errors;
    for (_, result) in results {
        match result {
            Ok(receiver) => {
                report.errors.extend(
                    receiver
                        .errors
                        .iter()
                        .map(|e| format!("{}: {e}", receiver.path)),
                );
                for device in &receiver.devices {
                    report.errors.extend(
                        device
                            .errors
                            .iter()
                            .map(|e| format!("{} slot {}: {e}", receiver.path, device.number)),
                    );
                }
                report.receivers.push(receiver);
            }
            Err(error) => report.errors.push(format!("{error:#}")),
        }
    }

    if report.receivers.is_empty() {
        report
            .errors
            .push("No Logitech HID++ receiver or device found".to_string());
    }
    report
}

/// Progress messages emitted while a receiver is put into pairing mode.
#[derive(Clone, Debug)]
pub enum PairingEvent {
    Status(String),
    Passkey(String),
    Success { slot: u8 },
    Failed(String),
}

/// Open the pairing lock (Unifying/Nano) or discovery mode (Bolt) on a
/// receiver and wait for a device to join. Runs on a worker thread because the
/// receiver needs several seconds to discover a device.
pub fn spawn_pairing(
    receiver_path: String,
    product_id: u16,
    cancel: Arc<AtomicBool>,
    events: Sender<PairingEvent>,
) {
    std::thread::spawn(move || {
        let result = pair_receiver(&receiver_path, product_id, &cancel, &events);
        let event = match result {
            Ok(slot) => PairingEvent::Success { slot },
            Err(error) => PairingEvent::Failed(format!("{error:#}")),
        };
        let _ = events.send(event);
    });
}

fn pair_receiver(
    receiver_path: &str,
    product_id: u16,
    cancel: &AtomicBool,
    events: &Sender<PairingEvent>,
) -> Result<u8> {
    // Pairing holds the same process-wide lock as scan/apply/automation. Do not
    // wait behind another pairing session and accidentally open a second one.
    let _lock = HARDWARE
        .try_lock()
        .map_err(|_| anyhow!("hardware is busy; another operation or pairing session is active"))?;
    if !matches!(product_id, 0xC548 | 0xC52B | 0xC532) {
        bail!("pairing protocol is not safely supported for receiver {product_id:04X}");
    }
    if cancel.load(Ordering::Relaxed) {
        bail!("pairing cancelled");
    }
    let current = hidraw::logitech_interfaces()
        .into_iter()
        .find(|i| i.path == Path::new(receiver_path))
        .context("receiver is no longer present")?;
    if current.product_id != product_id {
        bail!("receiver changed; rescan before pairing");
    }
    let mut hp = Hidpp::open(Path::new(receiver_path))
        .with_context(|| format!("cannot open receiver {receiver_path}"))?;
    let result = if product_id == 0xC548 {
        pair_bolt(&mut hp, cancel, events)
    } else {
        pair_unifying(&mut hp, cancel, events)
    };
    // Close on transport errors as well as explicit cancellation/timeout.
    if product_id == 0xC548 {
        close_bolt_pairing(&mut hp);
    } else {
        close_unifying_pairing(&mut hp);
    }
    result
}

fn pair_unifying(hp: &mut Hidpp, cancel: &AtomicBool, events: &Sender<PairingEvent>) -> Result<u8> {
    if !hp.write_register(0xB2, &[0x01, 0x00, 0x1E])? {
        bail!("the receiver refused to open its pairing lock");
    }
    let _ = events.send(PairingEvent::Status(
        "Pairing mode is open. Turn the device off and on, or hold its pairing button until the LED blinks."
            .to_string(),
    ));

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if cancel.load(Ordering::Relaxed) {
            close_unifying_pairing(hp);
            bail!("pairing cancelled");
        }
        if Instant::now() >= deadline {
            close_unifying_pairing(hp);
            bail!(
                "pairing timed out; make sure the device is close to the receiver and in pairing mode"
            );
        }
        let Some(notification) = hp.read_notification(Duration::from_millis(250))? else {
            continue;
        };
        if notification.sub_id != 0x54 {
            continue;
        }
        let error = notification.data.first().copied().unwrap_or(0);
        if error != 0 {
            close_unifying_pairing(hp);
            bail!("{}", pairing_error_text(error));
        }
        if notification.address == 0x00 {
            let _ = events.send(PairingEvent::Status(
                "Receiver is ready. Put the device in pairing mode now.".to_string(),
            ));
        } else if notification.address == 0x02 {
            let slot = notification.data.get(7).copied().unwrap_or(0);
            close_unifying_pairing(hp);
            return Ok(slot);
        }
    }
}

fn pair_bolt(hp: &mut Hidpp, cancel: &AtomicBool, events: &Sender<PairingEvent>) -> Result<u8> {
    if !hp.write_register(0xC0, &[0x1E, 0x01])? {
        bail!("the receiver refused to start discovery");
    }
    let _ = events.send(PairingEvent::Status(
        "Discovery mode is open. Put the Bolt device in pairing mode.".to_string(),
    ));

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut counter: Option<u16> = None;
    let mut address: Option<Vec<u8>> = None;
    let mut authentication = 0u8;
    let mut kind = 0u8;
    let mut name: Option<String> = None;
    let mut pair_requested = false;

    loop {
        if cancel.load(Ordering::Relaxed) {
            close_bolt_pairing(hp);
            bail!("pairing cancelled");
        }
        if Instant::now() >= deadline {
            close_bolt_pairing(hp);
            bail!(
                "pairing timed out; make sure the device is close to the receiver and in pairing mode"
            );
        }

        let Some(notification) = hp.read_notification(Duration::from_millis(250))? else {
            continue;
        };
        match notification.sub_id {
            0x53 => {
                let error = notification.data.first().copied().unwrap_or(0);
                if error != 0 {
                    close_bolt_pairing(hp);
                    bail!("{}", pairing_error_text(error));
                }
                if notification.address == 0x00 {
                    let _ = events.send(PairingEvent::Status(
                        "Receiver is discovering. Press the device pairing button now.".to_string(),
                    ));
                }
            }
            0x4F => {
                if notification.data.len() < 15 {
                    continue;
                }
                let packet_counter =
                    notification.address as u16 | ((notification.data[0] as u16) << 8);
                match counter {
                    None => counter = Some(packet_counter),
                    Some(current) if current != packet_counter => continue,
                    _ => {}
                }

                match notification.data[1] {
                    0 => {
                        kind = notification.data[3];
                        address = Some(notification.data[6..12].to_vec());
                        authentication = notification.data[14];
                    }
                    1 => {
                        let length =
                            (notification.data[2] as usize).min(notification.data.len() - 3);
                        let bytes = &notification.data[3..3 + length];
                        if !bytes.is_empty() {
                            name = Some(String::from_utf8_lossy(bytes).into_owned());
                        }
                    }
                    _ => {}
                }

                if !pair_requested {
                    if let (Some(address), Some(name)) = (&address, &name) {
                        let entropy = if kind == 0x01 { 20 } else { 10 };
                        let mut params = vec![0x01, 0x00];
                        params.extend_from_slice(address);
                        params.push(authentication);
                        params.push(entropy);
                        if !hp.write_register(0x2C1, &params)? {
                            close_bolt_pairing(hp);
                            bail!("the receiver refused to pair the device");
                        }
                        pair_requested = true;
                        let _ = events.send(PairingEvent::Status(format!(
                            "Pairing {name}… follow the instructions on the device."
                        )));
                    }
                }
            }
            0x54 => {
                let error = notification.data.first().copied().unwrap_or(0);
                if error != 0 {
                    close_bolt_pairing(hp);
                    bail!("{}", pairing_error_text(error));
                }
                if notification.address == 0x02 {
                    let slot = notification.data.get(7).copied().unwrap_or(0);
                    close_bolt_pairing(hp);
                    return Ok(slot);
                }
            }
            0x55 => {
                let length = notification.data.len().min(6);
                let passkey = String::from_utf8_lossy(&notification.data[..length])
                    .trim()
                    .to_string();
                if !passkey.is_empty() {
                    let _ = events.send(PairingEvent::Passkey(passkey));
                }
            }
            _ => {}
        }
    }
}

fn close_unifying_pairing(hp: &mut Hidpp) {
    let _ = hp.write_register(0xB2, &[0x02, 0x00, 0x00]);
}

fn close_bolt_pairing(hp: &mut Hidpp) {
    let _ = hp.write_register(0xC0, &[0x1E, 0x02]);
}

fn pairing_error_text(code: u8) -> String {
    match code {
        0x01 => "device timeout; make sure the device is powered on and in range".to_string(),
        0x02 => "the device is not supported by this receiver".to_string(),
        0x03 => "the receiver has no free pairing slots".to_string(),
        0x06 => "the pairing sequence timed out".to_string(),
        _ => format!("receiver reported pairing error 0x{code:02X}"),
    }
}

fn looks_like_receiver(interface: &HidrawInterface) -> bool {
    (0xC500..=0xC5FF).contains(&interface.product_id)
        || interface.name.to_ascii_lowercase().contains("receiver")
}

fn scan_receiver(interface: &HidrawInterface, config: &ConfigStore) -> Result<ReceiverSnapshot> {
    let mut hp = Hidpp::open(&interface.path)?;

    let info = if interface.product_id == 0xC548 {
        Ok(None)
    } else {
        hp.read_register(0x2B5, &[0x03])
    };
    let mut errors = Vec::new();
    let info = match info {
        Ok(Some(info)) => Some(info),
        Ok(None) => {
            if interface.product_id != 0xC548 {
                errors.push(
                    "receiver information request timed out; using known-model capacity".into(),
                );
            }
            None
        }
        Err(error) => {
            errors.push(format!("receiver information: {error:#}"));
            None
        }
    };
    let capabilities = receiver_capabilities(interface.product_id, info.as_deref());
    let serial = if interface.product_id == 0xC548 {
        match hp.read_register(0x2FB, &[]) {
            Ok(Some(bytes)) => serial_to_string(&bytes),
            Ok(None) => {
                errors.push("Bolt unique ID request timed out".into());
                String::new()
            }
            Err(error) => {
                errors.push(format!("Bolt unique ID: {error:#}"));
                String::new()
            }
        }
    } else {
        info.as_ref()
            .filter(|v| v.len() >= 7 && v[0] == 3)
            .and_then(|v| valid_serial(&hex_string(&v[1..5])))
            .unwrap_or_default()
    };
    let max_devices = capabilities.max_slots;

    let mut receiver = ReceiverSnapshot {
        path: interface.path.to_string_lossy().into_owned(),
        name: receiver_name(interface),
        product_id: interface.product_id,
        serial,
        max_devices,
        direct: false,
        transport: receiver_transport(interface.product_id),
        capabilities,
        errors,
        devices: Vec::new(),
    };

    // Bolt receivers use subregister 0x5N for pairing data and reverse the
    // WPID bytes; Unifying/Nano receivers use 0x2N (zero-based) and store the
    // extended pairing data (serial number) under 0x3N.
    let bolt = interface.product_id == 0xC548;

    for number in 1..=max_devices as u8 {
        let Some(pairing) = read_pairing_info(&mut hp, number, bolt) else {
            continue;
        };
        let device = scan_paired_device(&mut hp, &receiver, number, pairing, bolt, config);
        receiver.devices.push(device);
    }

    receiver.errors.extend(hp.take_diagnostics());

    Ok(receiver)
}

struct PairingInfo {
    wpid: String,
    kind: String,
    serial: String,
    online: bool,
}

fn read_pairing_info(hp: &mut Hidpp, number: u8, bolt: bool) -> Option<PairingInfo> {
    if bolt {
        let subregister = 0x50u16.checked_add(number as u16)? as u8;
        let reply = read_pairing_register(hp, subregister)?;
        if reply.len() < 8 || reply[0] != subregister {
            hp.record_diagnostic(format!("malformed Bolt pairing record for slot {number}"));
            return None;
        }
        Some(PairingInfo {
            kind: kind_name(reply[1] & 0x0F),
            wpid: format!("{:02X}{:02X}", reply[3], reply[2]),
            serial: hex_string(&reply[4..8]),
            online: reply[1] & 0x40 == 0,
        })
    } else {
        let subregister = 0x20u16.checked_add(number as u16 - 1)? as u8;
        let reply = read_pairing_register(hp, subregister)?;
        if reply.len() < 8 || reply[0] != subregister {
            hp.record_diagnostic(format!(
                "malformed receiver pairing record for slot {number}"
            ));
            return None;
        }
        let serial_register = 0x30u16.checked_add(number as u16 - 1)? as u8;
        let serial = read_pairing_register(hp, serial_register)
            .filter(|reply| reply.len() >= 5 && reply[0] == serial_register)
            .map(|reply| hex_string(&reply[1..5]))
            .unwrap_or_default();
        Some(PairingInfo {
            kind: kind_name(reply[7] & 0x0F),
            wpid: hex_string(&reply[3..5]),
            serial,
            online: true,
        })
    }
}

fn scan_paired_device(
    hp: &mut Hidpp,
    receiver: &ReceiverSnapshot,
    number: u8,
    pairing: PairingInfo,
    bolt: bool,
    config: &ConfigStore,
) -> DeviceSnapshot {
    let PairingInfo {
        wpid,
        kind,
        serial,
        online,
    } = pairing;
    let codename = receiver_codename(hp, number, bolt).unwrap_or_default();
    let serial = valid_serial(&serial).unwrap_or_default();
    let device_config = config.for_device(&serial, &codename);
    debug!("paired device {number} online={online} wpid={wpid}");

    let mut device = DeviceSnapshot {
        receiver_path: receiver.path.clone(),
        receiver_name: receiver.name.clone(),
        number,
        wpid,
        kind,
        serial,
        transport: receiver.transport.clone(),
        name: codename.clone(),
        codename: codename.clone(),
        online,
        config: device_config.clone(),
        ..DeviceSnapshot::default()
    };
    device.identity.source = "receiver_pairing".into();
    device.identity.stable = !device.serial.is_empty();
    device.identity.pairing_serial = valid_serial(&device.serial);
    for (name, _) in CONTROL_FEATURES {
        device.capabilities.insert(
            (*name).into(),
            Capability {
                state: CapabilityState::Unavailable,
                detail: Some("device is offline or has not answered feature queries".into()),
                ..Default::default()
            },
        );
    }

    if !online {
        if let Some(config) = &device_config {
            if let Some(name) = &config.name {
                device.name = name.clone();
            }
        }
        return device;
    }

    // A response from the feature-set feature proves that HID++ 2.0 is alive.
    // Wake sleeping devices first; the receiver's "online" bit only says the
    // device is paired and connected at the radio level.
    let awake = match hp.ping(number) {
        Ok(awake) => awake,
        Err(error) => {
            device.errors.push(format!("ping: {error:#}"));
            false
        }
    };
    debug!("paired device {number} awake={awake}");
    if !awake {
        device.online = false;
        device
            .errors
            .push("device did not answer HID++ ping (asleep, disconnected or unavailable)".into());
        if let Some(config) = &device_config {
            if let Some(name) = &config.name {
                device.name = name.clone();
            }
        }
        return device;
    }

    let feature_set = feature_index(hp, number, FEATURE_FEATURE_SET);
    debug!("paired device {number} feature-set={feature_set:?}");
    if feature_set.is_none() {
        device.online = false;
        device.errors.extend(hp.take_diagnostics());
        if let Some(config) = &device_config {
            if let Some(name) = &config.name {
                device.name = name.clone();
            }
        }
        return device;
    }

    read_device_features(hp, number, &mut device);
    // Unit ID is transport-independent when the device exposes it. Keep a
    // receiver serial fallback only when device-info has no valid unit ID.
    device.config = config.for_device(&device.serial, &codename);
    device
}

fn read_device_features(hp: &mut Hidpp, number: u8, device: &mut DeviceSnapshot) {
    read_inventory(hp, number, device);
    read_identity_and_firmware(hp, number, device);
    let name_index = feature_index(hp, number, FEATURE_DEVICE_NAME);
    if let Some(index) = name_index {
        if let Some(name) = read_device_name(hp, number, index) {
            if !name.is_empty() {
                device.name = name;
            }
        }
    }
    if device.name.is_empty() {
        if let Some(config) = &device.config {
            if let Some(name) = &config.name {
                device.name = name.clone();
            }
        }
    }
    if device.name.is_empty() {
        device.name = format!("Logitech device {}", device.wpid);
    }

    let battery_index = feature_index(hp, number, FEATURE_UNIFIED_BATTERY);
    let status_index = feature_index(hp, number, FEATURE_BATTERY_STATUS);
    if let Some(index) = battery_index {
        if let Some(reply) = hp.read_feature(number, index, 0x10, &[]) {
            if reply.len() >= 3 {
                device.battery = (reply[0] <= 100).then_some(reply[0]);
                device.battery_level = Some(reply[1]);
                device.battery_status = Some(reply[2]);
            }
        }
    } else if let Some(index) = status_index {
        if let Some(reply) = hp.read_feature(number, index, 0x00, &[]) {
            decode_legacy_battery(device, &reply);
        }
    }

    if let Some(index) = feature_index(hp, number, FEATURE_ADJUSTABLE_DPI) {
        device.dpi_feature = Some(index);
        device.dpi_values = read_dpi_values(hp, number, index);
        if let Some(reply) = hp.read_feature(number, index, 0x20, &[]) {
            device.dpi = crate::dpi::current(&reply);
        }
    }

    if let Some(index) = feature_index(hp, number, FEATURE_HIRES_WHEEL) {
        device.hires_feature = Some(index);
        if let Some(reply) = hp.read_feature(number, index, 0x10, &[]) {
            device.scroll_flags = reply.first().copied();
        }
    }

    if let Some(index) = feature_index(hp, number, FEATURE_SMART_SHIFT_ENHANCED) {
        device.smart_shift_feature = Some(index);
        let torque_supported = hp
            .read_feature(number, index, 0, &[])
            .and_then(|v| v.first().copied())
            .is_some_and(|v| v & 1 != 0);
        if let Some(reply) = hp.read_feature(number, index, 0x10, &[]) {
            if reply.len() >= 2 {
                device.smart_shift = Some(SmartShift {
                    mode: reply[0],
                    threshold: reply[1].min(50),
                    torque: if torque_supported {
                        reply.get(2).copied()
                    } else {
                        None
                    },
                });
            }
        }
    } else if let Some(index) = feature_index(hp, number, FEATURE_SMART_SHIFT) {
        device.smart_shift_feature = Some(index);
        if let Some(reply) = hp.read_feature(number, index, 0x00, &[]) {
            if reply.len() >= 2 {
                device.smart_shift = Some(SmartShift {
                    mode: reply[0],
                    threshold: reply[1].min(50),
                    torque: None,
                });
            }
        }
    }

    if let Some(index) = feature_index(hp, number, FEATURE_FEATURE_SET) {
        if let Some(reply) = hp.read_feature(number, index, 0x00, &[]) {
            device.feature_count = reply
                .first()
                .copied()
                .map(|count| count as usize + 1)
                .unwrap_or(0);
        }
    }

    read_extra_features(hp, number, device);
    read_additional_controls(hp, number, device);
    device.errors.extend(hp.take_diagnostics());
    update_capability_states(device);
}

fn read_extra_features(hp: &mut Hidpp, number: u8, device: &mut DeviceSnapshot) {
    if let Some(index) = feature_index(hp, number, FEATURE_HI_RES_SCROLLING) {
        device.smooth_scroll_feature = Some(index);
        if let Some(reply) = hp.read_feature(number, index, 0x00, &[]) {
            device.smooth_scroll = reply.first().map(|value| *value != 0);
        }
    }

    if let Some(index) = feature_index(hp, number, FEATURE_CHANGE_HOST) {
        if let Some(reply) = hp.read_feature(number, index, 0x00, &[]) {
            if reply.len() >= 2 {
                let count = reply[0];
                let current = reply[1];
                if count == 0 || count > 16 || current >= count {
                    hp.record_diagnostic("invalid ChangeHost count/current reply".into());
                    return;
                }
                let names = read_host_names(hp, number, count, current);
                device.host_feature = Some(index);
                device.host = Some(HostInfo {
                    count,
                    current,
                    names,
                });
            }
        }
    }

    if let Some(index) = feature_index(hp, number, FEATURE_REPROG_CONTROLS_V4) {
        device.buttons_feature = Some(index);
        device.buttons = read_buttons(hp, number, index);
    }

    if let Some(index) = feature_index(hp, number, FEATURE_BACKLIGHT2) {
        device.backlight = read_backlight(hp, number, index);
    }

    if let Some(index) = feature_index(hp, number, FEATURE_NEW_FN_INVERSION)
        .or_else(|| feature_index(hp, number, FEATURE_FN_INVERSION))
    {
        if let Some(reply) = hp.read_feature(number, index, 0x00, &[]) {
            device.fn_swap = reply.first().map(|value| *value != 0);
        }
    }

    if let Some(index) = feature_index(hp, number, FEATURE_KEYBOARD_DISABLE_KEYS) {
        let supported = hp
            .read_feature(number, index, 0x00, &[])
            .and_then(|reply| reply.first().copied())
            .unwrap_or(0);
        let disabled = hp
            .read_feature(number, index, 0x10, &[])
            .and_then(|reply| reply.first().copied());
        if supported != 0 && disabled.is_some() {
            device.disable_keys = Some(DisableKeysSnapshot {
                supported,
                disabled: disabled.unwrap_or(0),
            });
        }
    }
}

fn read_host_names(hp: &mut Hidpp, number: u8, count: u8, current: u8) -> Vec<String> {
    let mut names = vec![String::new(); count as usize];
    let Some(index) = feature_index(hp, number, FEATURE_HOSTS_INFO) else {
        return fallback_host_names(count, current, names);
    };
    let Some(state) = hp.read_feature(number, index, 0x00, &[]) else {
        return fallback_host_names(count, current, names);
    };
    if state.len() < 4 || state[0] & 0x01 == 0 {
        return fallback_host_names(count, current, names);
    }
    let num_hosts = state[2].min(count);
    for host in 0..num_hosts {
        let Some(info) = hp.read_feature(number, index, 0x10, &[host]) else {
            continue;
        };
        if info.len() < 6 {
            continue;
        }
        let length = info[4] as usize;
        if length == 0 {
            continue;
        }
        let mut name = String::new();
        let mut offset = 0u8;
        while name.len() < length {
            let Some(piece) = hp.read_feature(number, index, 0x30, &[host, offset]) else {
                break;
            };
            if piece.len() < 2 {
                break;
            }
            let take = (length - name.len()).min(piece.len() - 2);
            name.push_str(&String::from_utf8_lossy(&piece[2..2 + take]));
            offset = offset.saturating_add(take as u8);
            if take == 0 {
                break;
            }
        }
        names[host as usize] = name;
    }
    fallback_host_names(count, current, names)
}

fn fallback_host_names(count: u8, current: u8, mut names: Vec<String>) -> Vec<String> {
    names.resize(count as usize, String::new());
    for host in 0..count as usize {
        if names[host].is_empty() {
            let suffix = if host as u8 == current {
                " (current)"
            } else {
                ""
            };
            names[host] = format!("Host {}{suffix}", host + 1);
        } else if host as u8 == current {
            names[host].push_str(" (current)");
        }
    }
    names
}

fn read_buttons(hp: &mut Hidpp, number: u8, index: u8) -> Vec<ButtonInfo> {
    let Some(count_reply) = hp.read_feature(number, index, 0x00, &[]) else {
        return Vec::new();
    };
    let count = count_reply.first().copied().unwrap_or(0);
    let mut buttons = Vec::new();
    for button_index in 0..count {
        let Some(info) = hp.read_feature(number, index, 0x10, &[button_index]) else {
            continue;
        };
        if info.len() < 9 {
            continue;
        }
        let cid = u16::from_be_bytes([info[0], info[1]]);
        if cid == 0 {
            continue;
        }
        let task = u16::from_be_bytes([info[2], info[3]]);
        let flags = info[4] as u16 | ((info[8] as u16) << 8);
        let mut diverted = false;
        let mut mapped_to = cid;
        let mut reporting_available = false;
        if let Some(reporting) = hp.read_feature(number, index, 0x20, &cid.to_be_bytes()) {
            if reporting.len() >= 5 && reporting[0..2] == cid.to_be_bytes() {
                reporting_available = true;
                diverted = reporting[2] & 0x01 != 0;
                let mapped = u16::from_be_bytes([reporting[3], reporting[4]]);
                if mapped != 0 {
                    mapped_to = mapped;
                }
            }
        }
        buttons.push(ButtonInfo {
            cid,
            name: control_name(cid),
            task,
            diverted,
            mapped_to,
            remappable: flags & 0x10 != 0,
            divertable: flags & 0x20 != 0,
            raw_xy: flags & 0x100 != 0,
            reporting_available,
            group: info[6],
            group_mask: info[7],
        });
    }
    buttons
}

fn control_name(cid: u16) -> String {
    let name = match cid {
        10 => "Calculator",
        80 => "Left Button",
        81 => "Right Button",
        82 => "Middle Button",
        83 => "Back Button",
        86 => "Forward Button",
        91 => "Left Tilt",
        93 => "Right Tilt",
        111 => "Screen Lock",
        195 => "Gesture Button",
        196 => "Smart Shift",
        199 => "Brightness Down",
        200 => "Brightness Up",
        215 => "Virtual Gesture Button",
        226 => "Backlight Down",
        227 => "Backlight Up",
        228 => "Previous Track",
        229 => "Play/Pause",
        230 => "Next Track",
        231 => "Mute Sound",
        232 => "Volume Down",
        233 => "Volume Up",
        234 => "Context Menu",
        253 => "DPI Switch",
        259 => "Dictation",
        264 => "Emoji",
        266 => "Screen Capture",
        284 => "Mute Microphone",
        _ => return format!("Control {cid}"),
    };
    name.to_string()
}

fn read_backlight(hp: &mut Hidpp, number: u8, index: u8) -> Option<BacklightSnapshot> {
    let reply = hp.read_feature(number, index, 0x00, &[])?;
    if reply.len() < 12 {
        return None;
    }
    let enabled = reply[0] != 0;
    let options = reply[1];
    let level = reply[5];
    let hands_out = u16::from_le_bytes([reply[6], reply[7]]).saturating_mul(5);
    let hands_in = u16::from_le_bytes([reply[8], reply[9]]).saturating_mul(5);
    let powered = u16::from_le_bytes([reply[10], reply[11]]).saturating_mul(5);
    let max_level = hp
        .read_feature(number, index, 0x20, &[])
        .and_then(|value| value.first().copied())
        .map(|max| max.saturating_sub(1).max(1))
        .unwrap_or(0);
    Some(BacklightSnapshot {
        enabled,
        mode: (options >> 3) & 0x03,
        level,
        max_level,
        hands_out,
        hands_in,
        powered,
    })
}

fn scan_direct(interface: &HidrawInterface, config: &ConfigStore) -> Result<ReceiverSnapshot> {
    let mut hp = Hidpp::open(&interface.path)?;
    if !hp.ping(RECEIVER_DEVICE_NUMBER)? {
        bail!("direct device did not answer HID++ ping (unavailable or not HID++ 2.0)");
    }
    if hp
        .feature_index(RECEIVER_DEVICE_NUMBER, FEATURE_FEATURE_SET)?
        .is_none()
    {
        return Err(anyhow!("not a HID++ 2.0 device"));
    }

    let mut device = DeviceSnapshot {
        receiver_path: interface.path.to_string_lossy().into_owned(),
        receiver_name: interface.name.clone(),
        number: RECEIVER_DEVICE_NUMBER,
        wpid: String::new(),
        kind: kind_from_name(&interface.name).to_string(),
        serial: String::new(),
        name: interface.name.clone(),
        codename: interface.name.clone(),
        online: true,
        transport: interface.transport.clone(),
        ..DeviceSnapshot::default()
    };
    device.identity.hid_uniq = valid_hid_uniq(&interface.hid_uniq);
    read_device_features(&mut hp, RECEIVER_DEVICE_NUMBER, &mut device);
    if device.serial.is_empty() {
        if let Some(uniq) = &device.identity.hid_uniq {
            device.serial = format!("HID-{uniq}");
            device.identity.source = "hid_uniq".into();
            device.identity.stable = true;
        }
    }
    if !device.identity.stable {
        device.errors.push(
            "no unambiguous unit ID or HID_UNIQ; automatic per-unit persistence is unavailable"
                .into(),
        );
    }
    device.config = config.for_device(&device.serial, &device.codename);
    Ok(ReceiverSnapshot {
        path: interface.path.to_string_lossy().into_owned(),
        name: interface.name.clone(),
        product_id: interface.product_id,
        serial: device.serial.clone(),
        max_devices: 1,
        direct: true,
        transport: interface.transport.clone(),
        capabilities: ReceiverCapabilities {
            family: "direct".into(),
            max_slots: 1,
            capacity_source: "direct endpoint".into(),
            ..Default::default()
        },
        errors: Vec::new(),
        devices: vec![device],
    })
}

fn decode_legacy_battery(device: &mut DeviceSnapshot, reply: &[u8]) {
    let [level, next_level, status, ..] = reply else {
        return;
    };
    // HID++ 0x1000 reports zero when the discharge percentage is unknown,
    // notably on MX Master 2S while charging. It is not an empty battery.
    // Byte 1 is the next discharge threshold, not an alternative charge level.
    device.battery = (1..=100).contains(level).then_some(*level);
    device.battery_level = Some(*next_level);
    device.battery_status = Some(*status);
}

fn feature_index(hp: &mut Hidpp, device: u8, feature: u16) -> Option<u8> {
    match hp.feature_index(device, feature) {
        Ok(index) => index,
        Err(error) => {
            hp.record_diagnostic(format!("feature {feature:04X} lookup: {error:#}"));
            None
        }
    }
}

// Protocol reference: Solaar (GPL-2.0-or-later), Daniel Pavel and contributors,
// lib/logitech_receiver/{hidpp20.py,settings_templates.py,receiver.py}, local
// e7304c4c snapshot. These independently written decoders follow its documented
// field layouts (device-info, thumb wheel, report rate, multiplatform/unpair).
const CONTROL_FEATURES: &[(&str, &[u16])] = &[
    ("identity", &[FEATURE_DEVICE_INFO]),
    (
        "battery",
        &[FEATURE_UNIFIED_BATTERY, FEATURE_BATTERY_STATUS],
    ),
    ("dpi", &[FEATURE_ADJUSTABLE_DPI]),
    ("scroll", &[FEATURE_HIRES_WHEEL]),
    ("smooth_scroll", &[FEATURE_HI_RES_SCROLLING]),
    (
        "smart_shift",
        &[FEATURE_SMART_SHIFT_ENHANCED, FEATURE_SMART_SHIFT],
    ),
    ("thumbwheel", &[FEATURE_THUMB_WHEEL]),
    (
        "report_rate",
        &[FEATURE_EXTENDED_REPORT_RATE, FEATURE_REPORT_RATE],
    ),
    ("os_layout", &[FEATURE_MULTIPLATFORM, FEATURE_DUALPLATFORM]),
    ("host", &[FEATURE_CHANGE_HOST]),
    ("buttons", &[FEATURE_REPROG_CONTROLS_V4]),
    ("backlight", &[FEATURE_BACKLIGHT2]),
    ("fn_swap", &[FEATURE_NEW_FN_INVERSION, FEATURE_FN_INVERSION]),
    ("disable_keys", &[FEATURE_KEYBOARD_DISABLE_KEYS]),
];

fn read_inventory(hp: &mut Hidpp, number: u8, device: &mut DeviceSnapshot) {
    let mut complete = false;
    if let Some(index) = feature_index(hp, number, FEATURE_FEATURE_SET) {
        if let Some(count) = hp
            .read_feature(number, index, 0, &[])
            .and_then(|v| v.first().copied())
        {
            device.feature_count = count as usize + 1;
            device.features = vec![FeatureInfo {
                id: 0,
                index: 0,
                flags: 0,
                version: 0,
            }];
            complete = true;
            for feature_index in 1..=count {
                match hp.read_feature(number, index, 0x10, &[feature_index]) {
                    Some(reply) if reply.len() >= 4 => device.features.push(FeatureInfo {
                        id: u16::from_be_bytes([reply[0], reply[1]]),
                        index: feature_index,
                        flags: reply[2],
                        version: reply[3],
                    }),
                    _ => {
                        complete = false;
                    }
                }
            }
        }
    }
    for (name, ids) in CONTROL_FEATURES {
        let found = ids
            .iter()
            .find_map(|id| device.features.iter().find(|f| f.id == *id));
        let capability = if let Some(feature) = found {
            Capability {
                state: CapabilityState::Supported,
                feature_id: Some(feature.id),
                feature_index: Some(feature.index),
                version: Some(feature.version),
                writable: false,
                detail: Some("feature discovered; control read pending".into()),
            }
        } else {
            Capability {
                state: if complete {
                    CapabilityState::Unsupported
                } else {
                    CapabilityState::Unavailable
                },
                detail: (!complete)
                    .then(|| "feature inventory incomplete; no absence inferred".into()),
                ..Default::default()
            }
        };
        device.capabilities.insert((*name).into(), capability);
    }
}

fn update_capability_states(device: &mut DeviceSnapshot) {
    let states = [
        ("identity", device.identity.unit_id.is_some(), false),
        ("battery", device.battery_status.is_some(), false),
        ("dpi", device.dpi.is_some(), !device.dpi_values.is_empty()),
        ("scroll", device.scroll_flags.is_some(), true),
        ("smooth_scroll", device.smooth_scroll.is_some(), true),
        ("smart_shift", device.smart_shift.is_some(), true),
        ("thumbwheel", device.thumbwheel.is_some(), true),
        ("report_rate", device.report_rate.is_some(), true),
        (
            "os_layout",
            device.os_layout.is_some(),
            device.os_layout.as_ref().is_some_and(|s| s.settable),
        ),
        (
            "host",
            device.host.is_some(),
            device.host.as_ref().is_some_and(|h| h.count > 1),
        ),
        ("buttons", !device.buttons.is_empty(), true),
        ("backlight", device.backlight.is_some(), true),
        ("fn_swap", device.fn_swap.is_some(), true),
        ("disable_keys", device.disable_keys.is_some(), true),
    ];
    for (name, readable, writable) in states {
        let cap = device.capabilities.entry(name.into()).or_default();
        if readable {
            cap.state = CapabilityState::Supported;
            cap.writable = writable;
            cap.detail = None;
        } else if cap.state == CapabilityState::Supported {
            cap.state = CapabilityState::Unavailable;
            cap.writable = false;
            cap.detail =
                Some("feature present, but required state could not be read or validated".into());
        }
    }
}

fn read_identity_and_firmware(hp: &mut Hidpp, number: u8, device: &mut DeviceSnapshot) {
    let Some(index) = feature_index(hp, number, FEATURE_DEVICE_INFO) else {
        return;
    };
    let Some(reply) = hp.read_feature(number, index, 0, &[]) else {
        return;
    };
    if reply.len() < 13 {
        hp.record_diagnostic("malformed device-info identity".into());
        return;
    }
    if let Some(unit_id) = valid_serial(&hex_string(&reply[1..5])) {
        device.identity.unit_id = Some(unit_id.clone());
        device.identity.stable = true;
        device.identity.source = "hidpp_unit_id".into();
        device.serial = unit_id;
    }
    device.identity.model_id = valid_serial(&hex_string(&reply[7..13]));
    let mut offset = 7;
    for (name, bit) in [
        ("bluetooth", 1),
        ("bluetooth_le", 2),
        ("wireless", 4),
        ("usb", 8),
    ] {
        if reply[6] & bit != 0 {
            if offset + 2 > 13 {
                hp.record_diagnostic("device-info transport IDs exceed model ID capacity".into());
                break;
            }
            device
                .identity
                .transport_ids
                .insert(name.into(), hex_string(&reply[offset..offset + 2]));
            offset += 2;
        }
    }
    // Firmware entities are indexed by one byte. A malformed response must not
    // cause an unbounded scan or any writes.
    if reply[0] > 32 {
        hp.record_diagnostic("firmware entity count exceeds safe inventory bound (32)".into());
        return;
    }
    for layer in 0..reply[0] {
        let Some(info) = hp.read_feature(number, index, 0x10, &[layer]) else {
            continue;
        };
        if let Some(firmware) = decode_firmware(layer, &info) {
            device.firmware.push(firmware);
        } else {
            hp.record_diagnostic(format!("malformed firmware entity {layer}"));
        }
    }
}

fn decode_firmware(layer: u8, reply: &[u8]) -> Option<FirmwareInfo> {
    let kind = *reply.first()? & 0x0F;
    let mut firmware = FirmwareInfo {
        layer,
        kind,
        ..Default::default()
    };
    match kind {
        0 | 1 => {
            if reply.len() < 9 {
                return None;
            }
            firmware.name = String::from_utf8_lossy(&reply[1..4])
                .trim_end_matches('\0')
                .to_string();
            let build = u16::from_be_bytes([reply[6], reply[7]]);
            firmware.version = format!("{:02X}.{:02X}", reply[4], reply[5]);
            if build != 0 {
                firmware.version.push_str(&format!(".B{build:04X}"));
            }
            firmware.extra = reply[9..].to_vec();
            while firmware.extra.last() == Some(&0) {
                firmware.extra.pop();
            }
        }
        2 => firmware.version = reply.get(1)?.to_string(),
        _ => firmware.extra = reply[1..].to_vec(),
    }
    Some(firmware)
}

fn valid_serial(value: &str) -> Option<String> {
    let value = value.trim().to_ascii_uppercase();
    if value.len() < 8
        || value.len() > 128
        || !value.chars().all(|c| c.is_ascii_alphanumeric())
        || value.chars().all(|c| c == '0')
        || value.chars().all(|c| c == 'F')
        || matches!(
            value.as_str(),
            "UNKNOWN" | "LOGITECH" | "UNDEFINED" | "12345678"
        )
    {
        None
    } else {
        Some(value)
    }
}

fn valid_hid_uniq(value: &str) -> Option<String> {
    let value = value.trim();
    let hex: String = value
        .chars()
        .filter(|c| !matches!(c, ':' | '-'))
        .collect::<String>()
        .to_ascii_uppercase();
    // Linux Bluetooth addresses and hexadecimal USB unique serials are safe
    // fallbacks. Arbitrary strings can be marketing names shared by every
    // unit, so conservatively leave non-hex HID_UNIQ unavailable.
    if !(8..=64).contains(&hex.len())
        || hex.len() % 2 != 0
        || !hex.bytes().all(|c| c.is_ascii_hexdigit())
        || hex.bytes().all(|b| Some(b) == hex.bytes().next())
        || hex.eq_ignore_ascii_case("DEADBEEF")
    {
        return None;
    }
    valid_serial(&hex)
}

fn read_additional_controls(hp: &mut Hidpp, number: u8, device: &mut DeviceSnapshot) {
    if let Some(index) = feature_index(hp, number, FEATURE_THUMB_WHEEL) {
        if let Some(reply) = hp.read_feature(number, index, 0x10, &[]) {
            if reply.len() >= 2 {
                device.thumbwheel = Some(ThumbwheelSnapshot {
                    diverted: reply[0] & 1 != 0,
                    inverted: reply[1] & 1 != 0,
                });
            }
        }
    }
    let extended = feature_index(hp, number, FEATURE_EXTENDED_REPORT_RATE);
    if let Some(index) = extended.or_else(|| feature_index(hp, number, FEATURE_REPORT_RATE)) {
        let extended = extended.is_some();
        let mask = hp.read_feature(number, index, if extended { 0x10 } else { 0 }, &[]);
        let current = hp.read_feature(number, index, if extended { 0x20 } else { 0x10 }, &[]);
        if let (Some(mask), Some(current)) = (mask, current) {
            if let Some(rate) = decode_report_rate(&mask, &current, extended) {
                device.report_rate = Some(rate);
            } else {
                hp.record_diagnostic("invalid report-rate mask/current reply".into());
            }
        }
    }
    if let Some(index) = feature_index(hp, number, FEATURE_MULTIPLATFORM) {
        if let Some(reply) = hp.read_feature(number, index, 0, &[]) {
            if reply.len() >= 7 && reply[2] <= 32 {
                let mut platforms = Vec::new();
                let mut complete = true;
                for descriptor in 0..reply[2] {
                    match hp.read_feature(number, index, 0x10, &[descriptor]) {
                        Some(bytes) if bytes.len() >= 8 => platforms.push(decode_platform(&bytes)),
                        _ => complete = false,
                    }
                }
                let settable = reply[0] & 2 != 0 && complete && !platforms.is_empty();
                device.multiplatform = Some(reply[6]);
                device.os_layout = Some(OsLayoutSnapshot {
                    current: reply[6],
                    settable,
                    dual_platform: false,
                    platforms,
                });
            } else {
                hp.record_diagnostic("invalid multiplatform info reply".into());
            }
        }
    } else if let Some(index) = feature_index(hp, number, FEATURE_DUALPLATFORM) {
        if let Some(current) = hp
            .read_feature(number, index, 0, &[])
            .and_then(|v| v.first().copied())
            .filter(|v| *v < 2)
        {
            device.multiplatform = Some(current);
            device.os_layout = Some(OsLayoutSnapshot {
                current,
                settable: true,
                dual_platform: true,
                platforms: vec![
                    PlatformDescriptor {
                        id: 0,
                        name: "iOS / macOS".into(),
                        ..Default::default()
                    },
                    PlatformDescriptor {
                        id: 1,
                        name: "Android / Windows".into(),
                        ..Default::default()
                    },
                ],
            });
        }
    }
}

fn decode_report_rate(mask: &[u8], current: &[u8], extended: bool) -> Option<ReportRateSnapshot> {
    let flags = if extended {
        u16::from_be_bytes([*mask.first()?, *mask.get(1)?])
    } else {
        *mask.first()? as u16
    };
    let code = *current.first()?;
    let supported_us: Vec<u32> = (0..if extended { 7 } else { 8 })
        .filter(|bit| flags & (1 << bit) != 0)
        .map(|bit| {
            if extended {
                8000 >> bit
            } else {
                (bit + 1) * 1000
            }
        })
        .collect();
    let current_us = if extended {
        if code > 6 {
            return None;
        }
        8000 >> code
    } else {
        if !(1..=8).contains(&code) {
            return None;
        }
        code as u32 * 1000
    };
    if supported_us.is_empty() || !supported_us.contains(&current_us) {
        return None;
    }
    Some(ReportRateSnapshot {
        current_us,
        supported_us,
        extended,
    })
}

fn decode_platform(bytes: &[u8]) -> PlatformDescriptor {
    let os_flags = u16::from_be_bytes([bytes[2], bytes[3]]);
    let name = [
        ("Linux", 0x0400),
        ("macOS", 0x2000),
        ("Windows", 0x0100),
        ("iOS", 0x4000),
        ("Android", 0x1000),
        ("ChromeOS", 0x0800),
        ("WebOS", 0x8000),
        ("Windows Embedded", 0x0200),
        ("Tizen", 1),
    ]
    .into_iter()
    .filter(|(_, mask)| os_flags & mask != 0)
    .map(|(name, _)| name)
    .collect::<Vec<_>>()
    .join(" / ");
    PlatformDescriptor {
        id: bytes[0],
        os_flags,
        version_low: u16::from_be_bytes([bytes[4], bytes[5]]),
        version_high: u16::from_be_bytes([bytes[6], bytes[7]]),
        name,
    }
}

fn receiver_transport(product_id: u16) -> Transport {
    match product_id {
        0xC548 => Transport::Bolt,
        0xC52B | 0xC532 => Transport::Unifying,
        _ => Transport::Receiver,
    }
}

/// Pure capability decoder. Unknown receiver models never gain mutation rights
/// merely because a register happens to resemble a Unifying register.
pub fn receiver_capabilities(product_id: u16, info: Option<&[u8]>) -> ReceiverCapabilities {
    let (family, known_slots, mutations) = match product_id {
        0xC548 => ("bolt", 6, true),
        0xC52B | 0xC532 => ("unifying", 6, true),
        0xC534 => ("nano", 2, false),
        0xC539 | 0xC53F | 0xC547 => ("lightspeed", 1, false),
        _ => ("unknown", 1, false),
    };
    let register_slots = if product_id != 0xC548 {
        info.filter(|v| v.first() == Some(&3))
            .and_then(|v| v.get(6))
            .copied()
            .filter(|n| (1..=6).contains(n))
    } else {
        None
    };
    ReceiverCapabilities {
        family: family.into(),
        pairing: mutations,
        unpairing: mutations,
        max_slots: register_slots.map(usize::from).unwrap_or(known_slots),
        capacity_source: if register_slots.is_some() {
            "receiver_info"
        } else {
            "known_model_conservative"
        }
        .into(),
    }
}

fn read_pairing_register(hp: &mut Hidpp, subregister: u8) -> Option<Vec<u8>> {
    read_receiver_register(hp, 0x2B5, &[subregister])
}

fn read_receiver_register(hp: &mut Hidpp, register: u16, params: &[u8]) -> Option<Vec<u8>> {
    match hp.read_register(register, params) {
        Ok(Some(reply)) => Some(reply),
        Ok(None) => {
            hp.record_diagnostic(format!(
                "receiver register {register:03X} {params:02X?} timed out"
            ));
            None
        }
        Err(error)
            if matches!(
                error.downcast_ref::<crate::hidpp::HidppError>(),
                Some(crate::hidpp::HidppError::Device(2))
            ) =>
        {
            None
        }
        Err(error) => {
            hp.record_diagnostic(format!(
                "receiver register {register:03X} {params:02X?}: {error:#}"
            ));
            None
        }
    }
}

/// Destructive: the caller must obtain explicit confirmation before invoking.
/// Acknowledgement means the receiver accepted the request, not UI confirmation.
pub fn unpair_device(receiver: &ReceiverSnapshot, slot: u8) -> Result<()> {
    let device = receiver
        .devices
        .iter()
        .find(|d| d.number == slot)
        .context("slot is not in the scanned paired inventory")?;
    unpair(receiver, device)
}

pub fn unpair(receiver: &ReceiverSnapshot, device: &DeviceSnapshot) -> Result<()> {
    let _lock = hardware_lock();
    let slot = device.number;
    if device.receiver_path != receiver.path || device.number == RECEIVER_DEVICE_NUMBER {
        bail!("selected device does not belong to this receiver");
    }
    if receiver.direct {
        bail!("direct devices cannot be unpaired through a receiver");
    }
    let interface = hidraw::logitech_interfaces()
        .into_iter()
        .find(|i| i.path == Path::new(&receiver.path))
        .context("receiver is no longer present")?;
    if interface.product_id != receiver.product_id {
        bail!("receiver changed; rescan before unpairing");
    }
    let capabilities = receiver_capabilities(interface.product_id, None);
    if !capabilities.unpairing
        || slot == 0
        || slot as usize > receiver.max_devices.min(capabilities.max_slots)
    {
        bail!("unpairing is unsupported for this receiver or slot is invalid");
    }
    let mut hp = Hidpp::open(&interface.path)?;
    unpair_selected_with(&mut hp, receiver, device)
}

fn unpair_selected_with(
    hp: &mut Hidpp,
    receiver: &ReceiverSnapshot,
    device: &DeviceSnapshot,
) -> Result<()> {
    let slot = device.number;
    let product_id = receiver.product_id;
    let capabilities = receiver_capabilities(product_id, None);
    if !capabilities.unpairing
        || slot == 0
        || slot as usize > receiver.max_devices.min(capabilities.max_slots)
    {
        bail!("unsupported receiver or invalid slot");
    }
    let bolt = product_id == 0xC548;
    let current_serial = if bolt {
        hp.read_register(0x2FB, &[])?.map(|v| serial_to_string(&v))
    } else {
        hp.read_register(0x2B5, &[3])?
            .filter(|v| v.len() >= 7 && v[0] == 3)
            .and_then(|v| valid_serial(&hex_string(&v[1..5])))
    };
    if receiver.serial.is_empty() || current_serial.as_deref() != Some(receiver.serial.as_str()) {
        bail!("receiver identity could not be verified; rescan before unpairing");
    }
    let expected = device;
    let pairing = read_pairing_info(hp, slot, bolt)
        .context("could not verify current pairing; no unpair command was sent")?;
    // Slot reuse can happen between confirmation and execution.
    let expected_serial = expected.identity.pairing_serial.clone().or_else(|| {
        if expected.identity.unit_id.is_none() {
            valid_serial(&expected.serial)
        } else {
            None
        }
    });
    if pairing.wpid != expected.wpid
        || expected_serial.is_none()
        || valid_serial(&pairing.serial) != expected_serial
    {
        bail!("pairing changed; rescan and confirm again");
    }
    unpair_with(hp, product_id, slot)
}

fn unpair_with(hp: &mut Hidpp, product_id: u16, slot: u8) -> Result<()> {
    if !receiver_capabilities(product_id, None).unpairing || !(1..=6).contains(&slot) {
        bail!("unsupported receiver or invalid slot");
    }
    if !hp.write_register(if product_id == 0xC548 { 0x2C1 } else { 0xB2 }, &[3, slot])? {
        bail!("unpair request timed out; outcome is unknown, rescan before retrying");
    }
    Ok(())
}

fn read_device_name(hp: &mut Hidpp, device: u8, index: u8) -> Option<String> {
    let count = hp.read_feature(device, index, 0x00, &[])?;
    let length = count.first().copied()? as usize;
    if length == 0 || length > 256 {
        return None;
    }
    let mut bytes = Vec::with_capacity(length);
    while bytes.len() < length {
        let offset = bytes.len().min(u8::MAX as usize) as u8;
        let fragment = hp.read_feature(device, index, 0x10, &[offset])?;
        if fragment.is_empty() {
            break;
        }
        let remaining = length - bytes.len();
        bytes.extend_from_slice(&fragment[..fragment.len().min(remaining)]);
    }
    let name = String::from_utf8(bytes).ok()?;
    let name = name.split('\0').next().unwrap_or_default().trim();
    if name.is_empty() || name.chars().any(char::is_control) {
        return None;
    }
    Some(name.to_string())
}

fn receiver_codename(hp: &mut Hidpp, number: u8, bolt: bool) -> Option<String> {
    if bolt {
        let subregister = 0x60u16.checked_add(number as u16)? as u8;
        let reply = read_receiver_register(hp, 0x2B5, &[subregister, 0x01])?;
        if reply.len() < 3 || reply[0] != subregister || reply[1] != 0x01 {
            return None;
        }
        let length = reply[2] as usize;
        if length == 0 || reply.len() < 3 + length {
            return None;
        }
        String::from_utf8(reply[3..3 + length].to_vec()).ok()
    } else {
        let subregister = 0x40u16.checked_add(number as u16 - 1)? as u8;
        let reply = read_pairing_register(hp, subregister)?;
        if reply.len() < 2 || reply[0] != subregister {
            return None;
        }
        let length = reply[1] as usize;
        if length == 0 || reply.len() < 2 + length {
            return None;
        }
        String::from_utf8(reply[2..2 + length].to_vec()).ok()
    }
}

fn hex_string(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect()
}

fn receiver_name(interface: &HidrawInterface) -> String {
    match interface.product_id {
        0xC548 => "Bolt Receiver".to_string(),
        0xC52B | 0xC532 => "Unifying Receiver".to_string(),
        0xC534 => "Nano Receiver".to_string(),
        0xC539 | 0xC53F | 0xC547 => "Lightspeed Receiver".to_string(),
        _ => interface.name.clone(),
    }
}

fn serial_to_string(bytes: &[u8]) -> String {
    if bytes.is_empty() || bytes.iter().all(|b| *b == 0) || bytes.iter().all(|b| *b == 0xFF) {
        return String::new();
    }
    let end = bytes
        .iter()
        .rposition(|b| *b != 0)
        .map(|v| v + 1)
        .unwrap_or(0);
    let trimmed = &bytes[..end];
    if trimmed.len() >= 4 && trimmed.iter().all(|byte| byte.is_ascii_graphic()) {
        String::from_utf8_lossy(trimmed).into_owned()
    } else {
        hex_string(bytes)
    }
}

fn kind_name(kind: u8) -> String {
    match kind {
        0x01 => "Keyboard",
        0x02 => "Mouse",
        0x03 => "Numpad",
        0x04 => "Presenter",
        0x07 => "Remote",
        0x08 => "Trackball",
        0x09 => "Touchpad",
        0x0A => "Tablet",
        0x0B => "Gamepad",
        0x0C => "Joystick",
        0x0D => "Headset",
        0x0F => "Receiver",
        _ => "Unknown device",
    }
    .to_string()
}

fn kind_from_name(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    if lower.contains("keyboard") {
        "Keyboard"
    } else if lower.contains("mouse") {
        "Mouse"
    } else if lower.contains("trackball") {
        "Trackball"
    } else {
        "Device"
    }
}

/// Apply user-selected live settings. Only values present in `update` are
/// written; everything else is left untouched.
pub fn apply_settings(device: &DeviceSnapshot, update: &SettingsUpdate) -> Result<Vec<String>> {
    let outcome = apply_verified(device, update)?;
    if outcome.results.iter().any(|r| {
        matches!(
            r.state,
            ApplyState::Failed | ApplyState::Mismatch | ApplyState::Unconfirmed
        )
    }) {
        bail!(
            "settings were only partially applied (no atomic rollback): {}",
            outcome
                .results
                .iter()
                .map(|r| format!(
                    "{}: {:?}{}",
                    r.setting,
                    r.state,
                    r.error
                        .as_ref()
                        .map(|e| format!(" ({e})"))
                        .unwrap_or_default()
                ))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    Ok(outcome
        .results
        .iter()
        .map(|r| format!("{}: {:?}", r.setting, r.state))
        .collect())
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyState {
    Confirmed,
    Mismatch,
    Unconfirmed,
    Failed,
    SentUnconfirmed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SettingResult {
    pub setting: String,
    pub state: ApplyState,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ApplyOutcome {
    pub results: Vec<SettingResult>,
    pub snapshot: Option<DeviceSnapshot>,
    pub read_back_errors: Vec<String>,
    /// Safe input to a profile-save API. Only read-back-confirmed requested
    /// values are included. Never includes target_host, even after send.
    pub confirmed_update: SettingsUpdate,
}

impl ApplyOutcome {
    pub fn all_confirmed(&self) -> bool {
        self.results
            .iter()
            .all(|r| r.state == ApplyState::Confirmed)
    }
    pub fn has_failures(&self) -> bool {
        self.results.iter().any(|r| {
            matches!(
                r.state,
                ApplyState::Failed | ApplyState::Mismatch | ApplyState::Unconfirmed
            )
        })
    }
}

fn validate_profile_conflicts(device: &DeviceSnapshot, update: &SettingsUpdate) -> Result<()> {
    if !update.button_remaps.is_empty() || !update.button_diversions.is_empty() {
        let profile = crate::profiles::load(device);
        if profile.actions_enabled
            && update
                .button_remaps
                .iter()
                .map(|(cid, _)| cid)
                .chain(update.button_diversions.iter().map(|(cid, _)| cid))
                .any(|cid| {
                    profile
                        .bindings
                        .get(cid)
                        .is_some_and(|binding| !binding.is_empty())
                })
        {
            bail!("Disable custom actions before changing this button's mapping or diversion");
        }
    }
    if !device.online {
        return Err(anyhow!("device is offline"));
    }
    if device.receiver_path.is_empty() {
        return Err(anyhow!("device has no HID++ path"));
    }
    Ok(())
}

/// Best effort, deliberately non-atomic. Every requested setting gets a result;
/// failed writes may have taken effect, so state is re-read after failures too.
/// Uses the reentrant process-wide hardware lock, not an atomic transaction.
pub fn apply_verified(device: &DeviceSnapshot, update: &SettingsUpdate) -> Result<ApplyOutcome> {
    validate_profile_conflicts(device, update)?;
    let _lock = hardware_lock();
    let mut hp = open_device_connection(device)
        .with_context(|| format!("cannot open {}", device.receiver_path))?;
    apply_verified_with(&mut hp, device, update)
}

/// Injectable equivalent for protocol replay and embedders that already own the
/// hardware lock. The transport must refer to `device`'s endpoint.
pub fn apply_verified_with(
    hp: &mut Hidpp,
    device: &DeviceSnapshot,
    update: &SettingsUpdate,
) -> Result<ApplyOutcome> {
    if !device.online {
        bail!("device is offline");
    }
    verify_endpoint_identity(hp, device)?;
    let mut pending = Vec::new();
    for (name, single) in setting_updates(update) {
        if name == "target_host" {
            continue;
        }
        let error = apply_writes(hp, device, &single)
            .err()
            .map(|e| format!("{e:#}"));
        pending.push((name, single, error));
    }
    // Never retain the previous snapshot's settings after a read failure.
    let mut snapshot = DeviceSnapshot {
        receiver_path: device.receiver_path.clone(),
        receiver_name: device.receiver_name.clone(),
        number: device.number,
        wpid: device.wpid.clone(),
        kind: device.kind.clone(),
        serial: device.serial.clone(),
        name: device.name.clone(),
        codename: device.codename.clone(),
        identity: device.identity.clone(),
        transport: device.transport.clone(),
        config: device.config.clone(),
        online: true,
        ..Default::default()
    };
    read_device_features(hp, device.number, &mut snapshot);
    let mut outcome = ApplyOutcome {
        read_back_errors: snapshot.errors.clone(),
        ..Default::default()
    };
    for (name, single, error) in pending {
        let matched = setting_matches(&snapshot, &single);
        let state = match matched {
            Some(true) => ApplyState::Confirmed,
            Some(false) if error.is_none() => ApplyState::Mismatch,
            Some(false) => ApplyState::Failed,
            None if error.is_some() => ApplyState::Failed,
            None => ApplyState::Unconfirmed,
        };
        if state == ApplyState::Confirmed {
            merge_setting(&mut outcome.confirmed_update, &single);
        }
        outcome.results.push(SettingResult {
            setting: name,
            state,
            error,
        });
    }
    if let Some(host) = update.target_host {
        let sent = validate_host(hp, device.number, host)
            .and_then(|index| hp.send_feature(device.number, index, 0x10, &[host]));
        let (state, error) = match sent {
            Ok(()) => (ApplyState::SentUnconfirmed, None),
            Err(e) => (ApplyState::Failed, Some(format!("{e:#}"))),
        };
        outcome.results.push(SettingResult {
            setting: "target_host".into(),
            state,
            error,
        });
    }
    outcome.snapshot = Some(snapshot);
    Ok(outcome)
}

fn apply_writes(
    hp: &mut Hidpp,
    device: &DeviceSnapshot,
    update: &SettingsUpdate,
) -> Result<Vec<String>> {
    let number = device.number;
    let mut changed = Vec::new();

    if let Some(dpi) = update.dpi {
        let index = hp
            .feature_index(number, FEATURE_ADJUSTABLE_DPI)?
            .ok_or_else(|| anyhow!("device does not support adjustable DPI"))?;
        let supported = read_dpi_values(hp, number, index);
        if supported.is_empty() {
            bail!("DPI capability values are unavailable; no DPI write was sent");
        }
        if !supported.contains(&dpi) {
            bail!("DPI {dpi} is outside the device-advertised values");
        }
        hp.write_feature(
            number,
            index,
            0x30,
            &[0x00, (dpi >> 8) as u8, (dpi & 0xFF) as u8],
        )?;
        changed.push(format!("DPI set to {dpi}"));
    }

    if update.invert_scroll.is_some()
        || update.high_resolution_scroll.is_some()
        || update.scroll_diversion.is_some()
    {
        let index = hp
            .feature_index(number, FEATURE_HIRES_WHEEL)?
            .ok_or_else(|| anyhow!("device does not support scroll wheel settings"))?;
        let current = hp
            .read_feature(number, index, 0x10, &[])
            .ok_or_else(|| anyhow!("could not read scroll wheel flags"))?;
        let mut flags = current
            .first()
            .copied()
            .context("invalid scroll wheel flags reply")?;
        if let Some(invert) = update.invert_scroll {
            if invert {
                flags |= 0x04;
            } else {
                flags &= !0x04;
            }
            changed.push(format!(
                "Scroll direction {}",
                if invert { "inverted" } else { "normal" }
            ));
        }
        if let Some(high_resolution) = update.high_resolution_scroll {
            if high_resolution {
                flags |= 0x02;
            } else {
                flags &= !0x02;
            }
            changed.push(format!(
                "Scroll resolution {}",
                if high_resolution { "high" } else { "normal" }
            ));
        }
        if let Some(diversion) = update.scroll_diversion {
            if diversion {
                flags |= 0x01;
            } else {
                flags &= !0x01;
            }
            changed.push(format!(
                "Scroll diversion {}",
                if diversion { "enabled" } else { "disabled" }
            ));
        }
        hp.write_feature(number, index, 0x20, &[flags])?;
    }

    if let Some(smooth) = update.smooth_scroll {
        let index = hp
            .feature_index(number, FEATURE_HI_RES_SCROLLING)?
            .ok_or_else(|| anyhow!("device does not support smooth scrolling"))?;
        hp.write_feature(number, index, 0x10, &[smooth as u8])?;
        changed.push(format!(
            "Smooth scrolling {}",
            if smooth { "enabled" } else { "disabled" }
        ));
    }

    if update.ratcheted.is_some()
        || update.smart_shift_threshold.is_some()
        || update.smart_shift_torque.is_some()
    {
        let smart_shift_index =
            if let Some(index) = hp.feature_index(number, FEATURE_SMART_SHIFT_ENHANCED)? {
                Some((index, true))
            } else {
                hp.feature_index(number, FEATURE_SMART_SHIFT)?
                    .map(|index| (index, false))
            };

        let (index, enhanced) =
            smart_shift_index.ok_or_else(|| anyhow!("device does not support SmartShift"))?;
        if update
            .smart_shift_threshold
            .is_some_and(|value| !(1..=50).contains(&value))
        {
            bail!("SmartShift threshold must be in 1..=50 (50 means always ratcheted)");
        }
        if let Some(torque) = update.smart_shift_torque {
            if !enhanced || !(1..=100).contains(&torque) {
                bail!("SmartShift torque requires enhanced SmartShift and a value in 1..=100");
            }
            let caps = hp.read_feature_checked(number, index, 0, &[])?;
            if caps.first().is_none_or(|flags| flags & 1 == 0) {
                bail!("device does not advertise tunable SmartShift torque");
            }
        }
        let current = if enhanced {
            hp.read_feature(number, index, 0x10, &[])
        } else {
            hp.read_feature(number, index, 0x00, &[])
        }
        .ok_or_else(|| anyhow!("could not read SmartShift settings"))?;
        if current.len() < if enhanced { 3 } else { 2 } {
            bail!("invalid SmartShift state reply");
        }
        let current_mode = current[0];
        let current_threshold = current[1];
        let current_torque = current.get(2).copied().unwrap_or(0);

        let mode = match update.ratcheted {
            Some(true) => 2,
            Some(false) => 1,
            None => current_mode,
        };
        let threshold = update
            .smart_shift_threshold
            .map(|v| if v == 50 { 255 } else { v })
            .unwrap_or(current_threshold);
        let torque = update.smart_shift_torque.unwrap_or(current_torque);

        if enhanced {
            hp.write_feature(number, index, 0x20, &[mode, threshold, torque])?;
        } else {
            hp.write_feature(number, index, 0x10, &[mode, threshold])?;
        }

        if update.ratcheted.is_some() {
            changed.push(format!(
                "Wheel mode {}",
                if mode == 2 {
                    "ratcheted"
                } else {
                    "free-spinning"
                }
            ));
        }
        if update.smart_shift_threshold.is_some() {
            changed.push(format!("SmartShift threshold {threshold}"));
        }
        if update.smart_shift_torque.is_some() {
            changed.push(format!("SmartShift torque {torque}"));
        }
    }

    if !update.button_diversions.is_empty() {
        let index = hp
            .feature_index(number, FEATURE_REPROG_CONTROLS_V4)?
            .ok_or_else(|| anyhow!("device does not support button diversion"))?;
        for (cid, diverted) in &update.button_diversions {
            if !device.buttons.iter().any(|b| b.cid == *cid && b.divertable) {
                bail!("control {cid} is not divertable");
            }
            let bfield = if *diverted { 0x03 } else { 0x02 };
            hp.write_feature(
                number,
                index,
                0x30,
                &[(cid >> 8) as u8, *cid as u8, bfield, 0x00, 0x00],
            )?;
        }
        changed.push("Button diversion updated".to_string());
    }

    if !update.button_remaps.is_empty() {
        let index = hp
            .feature_index(number, FEATURE_REPROG_CONTROLS_V4)?
            .ok_or_else(|| anyhow!("device does not support button remapping"))?;
        for (cid, target) in &update.button_remaps {
            let source = device
                .buttons
                .iter()
                .find(|b| b.cid == *cid && b.remappable)
                .context("source control is not remappable")?;
            let destination = device
                .buttons
                .iter()
                .find(|b| b.cid == *target)
                .context("target control is absent")?;
            if cid != target
                && (destination.group > 7 || source.group_mask & (1 << destination.group) == 0)
            {
                bail!("invalid or non-remappable button/target {cid}/{target}");
            }
            hp.write_feature(
                number,
                index,
                0x30,
                &[
                    (cid >> 8) as u8,
                    *cid as u8,
                    0x00,
                    (target >> 8) as u8,
                    *target as u8,
                ],
            )?;
        }
        changed.push("Button actions updated".to_string());
    }

    if let Some(fn_swap) = update.fn_swap {
        let newer = hp.feature_index(number, FEATURE_NEW_FN_INVERSION)?;
        let index = match newer {
            Some(index) => Some(index),
            None => hp.feature_index(number, FEATURE_FN_INVERSION)?,
        }
        .ok_or_else(|| anyhow!("device does not support Fn swap"))?;
        hp.write_feature(number, index, 0x10, &[fn_swap as u8])?;
        changed.push(format!(
            "Fn swap {}",
            if fn_swap { "enabled" } else { "disabled" }
        ));
    }

    if update.backlight_enabled.is_some()
        || update.backlight_mode.is_some()
        || update.backlight_level.is_some()
        || update.backlight_durations.is_some()
    {
        let index = hp
            .feature_index(number, FEATURE_BACKLIGHT2)?
            .ok_or_else(|| anyhow!("device does not support backlight settings"))?;
        let current = hp
            .read_feature(number, index, 0x00, &[])
            .ok_or_else(|| anyhow!("could not read backlight settings"))?;
        if current.len() < 12 {
            return Err(anyhow!("invalid backlight reply"));
        }
        let current_enabled = current[0] != 0;
        let current_options = current[1];
        let current_mode = (current_options >> 3) & 0x03;
        let current_level = current[5];
        let current_hands_out = u16::from_le_bytes([current[6], current[7]]);
        let current_hands_in = u16::from_le_bytes([current[8], current[9]]);
        let current_powered = u16::from_le_bytes([current[10], current[11]]);

        let enabled = update.backlight_enabled.unwrap_or(current_enabled);
        let mode = update.backlight_mode.unwrap_or(current_mode);
        let level = update.backlight_level.unwrap_or(current_level);
        if update.backlight_mode.is_some()
            && (mode > 3 || mode != 0 && current[2] & (1 << (mode + 2)) == 0)
        {
            bail!("backlight mode {mode} is not advertised by this device");
        }
        if let Some(level) = update.backlight_level {
            let max = hp
                .read_feature_checked(number, index, 0x20, &[])?
                .first()
                .copied()
                .context("invalid backlight range")?;
            if level >= max {
                bail!("backlight level exceeds device range");
            }
        }
        let (hands_out, hands_in, powered) = update
            .backlight_durations
            .map(|(out, inside, power)| (out.div_ceil(5), inside.div_ceil(5), power.div_ceil(5)))
            .unwrap_or((current_hands_out, current_hands_in, current_powered));
        let options = (current_options & !0x18) | ((mode & 0x03) << 3);
        let mut payload = Vec::with_capacity(10);
        payload.push(enabled as u8);
        payload.push(options);
        payload.push(0xFF);
        payload.push(if mode == 0x03 { level } else { 0 });
        payload.extend_from_slice(&hands_out.to_le_bytes());
        payload.extend_from_slice(&hands_in.to_le_bytes());
        payload.extend_from_slice(&powered.to_le_bytes());
        hp.write_feature(number, index, 0x10, &payload)?;
        changed.push("Backlight settings updated".to_string());
    }

    if let Some(mask) = update.disable_keys {
        let index = hp
            .feature_index(number, FEATURE_KEYBOARD_DISABLE_KEYS)?
            .ok_or_else(|| anyhow!("device does not support disabling keys"))?;
        let supported = hp
            .read_feature_checked(number, index, 0, &[])?
            .first()
            .copied()
            .context("invalid supported key mask")?;
        if mask & !supported != 0 {
            bail!("disable-keys mask contains unsupported keys");
        }
        hp.write_feature(number, index, 0x20, &[mask])?;
        changed.push("Disabled keyboard keys updated".to_string());
    }

    if update.thumbwheel_inverted.is_some() || update.thumbwheel_diverted.is_some() {
        let index = hp
            .feature_index(number, FEATURE_THUMB_WHEEL)?
            .context("device does not support thumbwheel settings")?;
        let current = hp.read_feature_checked(number, index, 0x10, &[])?;
        if current.len() < 2 {
            bail!("invalid thumbwheel state reply");
        }
        let mut flags = [current[0], current[1]];
        if let Some(value) = update.thumbwheel_diverted {
            flags[0] = (flags[0] & !1) | u8::from(value);
        }
        if let Some(value) = update.thumbwheel_inverted {
            flags[1] = (flags[1] & !1) | u8::from(value);
        }
        hp.write_feature(number, index, 0x20, &flags)?;
        changed.push("Thumbwheel settings updated".into());
    }
    if let Some(period) = update.report_rate_us {
        let extended = hp.feature_index(number, FEATURE_EXTENDED_REPORT_RATE)?;
        let index = match extended {
            Some(index) => index,
            None => hp
                .feature_index(number, FEATURE_REPORT_RATE)?
                .context("device does not support report-rate controls")?,
        };
        let extended = extended.is_some();
        let mask = hp.read_feature_checked(number, index, if extended { 0x10 } else { 0 }, &[])?;
        let current =
            hp.read_feature_checked(number, index, if extended { 0x20 } else { 0x10 }, &[])?;
        let rate = decode_report_rate(&mask, &current, extended)
            .context("invalid report-rate capabilities")?;
        if !rate.supported_us.contains(&period) {
            bail!("report period {period} us is not advertised by this device");
        }
        let code = if extended {
            (0..7)
                .find(|code| (8000 >> code) == period)
                .context("invalid extended report period")?
        } else {
            (period / 1000) as u8
        };
        hp.write_feature(number, index, if extended { 0x30 } else { 0x20 }, &[code])?;
        changed.push(format!(
            "Report period set to {period} us; onboard profiles may override it"
        ));
    }
    if let Some(platform) = update.os_layout {
        if let Some(index) = hp.feature_index(number, FEATURE_MULTIPLATFORM)? {
            let state = hp.read_feature_checked(number, index, 0, &[])?;
            if state.len() < 7 || state[0] & 2 == 0 || state[2] > 32 {
                bail!("OS layout is not settable or capabilities are invalid");
            }
            let mut found = false;
            for descriptor in 0..state[2] {
                let info = hp.read_feature_checked(number, index, 0x10, &[descriptor])?;
                if info.len() < 8 {
                    bail!("invalid OS platform descriptor");
                }
                found |= info[0] == platform;
            }
            if !found {
                bail!("OS platform ID {platform} is not advertised by this device");
            }
            hp.write_feature(number, index, 0x30, &[0xFF, platform])?;
        } else {
            let index = hp
                .feature_index(number, FEATURE_DUALPLATFORM)?
                .context("device does not support OS-layout controls")?;
            if platform > 1 {
                bail!("dual-platform ID must be 0 or 1");
            }
            hp.write_feature(number, index, 0x20, &[platform])?;
        }
        changed.push("OS layout updated".into());
    }

    Ok(changed)
}

// Keep scalar field lists identical in splitting/merging. Button vectors are
// split by CID so one rejected control does not conceal other write outcomes.
macro_rules! scalar_settings {
    ($visit:ident) => {
        $visit!(dpi);
        $visit!(invert_scroll);
        $visit!(high_resolution_scroll);
        $visit!(scroll_diversion);
        $visit!(smooth_scroll);
        $visit!(ratcheted);
        $visit!(smart_shift_threshold);
        $visit!(smart_shift_torque);
        $visit!(target_host);
        $visit!(fn_swap);
        $visit!(backlight_enabled);
        $visit!(backlight_mode);
        $visit!(backlight_level);
        $visit!(backlight_durations);
        $visit!(disable_keys);
        $visit!(thumbwheel_inverted);
        $visit!(thumbwheel_diverted);
        $visit!(report_rate_us);
        $visit!(os_layout);
    };
}

fn setting_updates(update: &SettingsUpdate) -> Vec<(String, SettingsUpdate)> {
    let mut updates = Vec::new();
    macro_rules! split {
        ($field:ident) => {
            if update.$field.is_some() {
                let mut single = SettingsUpdate::default();
                single.$field = update.$field;
                updates.push((stringify!($field).into(), single));
            }
        };
    }
    scalar_settings!(split);
    for &(cid, value) in &update.button_diversions {
        updates.push((
            format!("button_diversions:{cid}"),
            SettingsUpdate {
                button_diversions: vec![(cid, value)],
                ..Default::default()
            },
        ));
    }
    for &(cid, value) in &update.button_remaps {
        updates.push((
            format!("button_remaps:{cid}"),
            SettingsUpdate {
                button_remaps: vec![(cid, value)],
                ..Default::default()
            },
        ));
    }
    updates
}

fn merge_setting(target: &mut SettingsUpdate, source: &SettingsUpdate) {
    macro_rules! merge {
        ($field:ident) => {
            if source.$field.is_some() {
                target.$field = source.$field;
            }
        };
    }
    scalar_settings!(merge);
    target.target_host = None;
    target
        .button_diversions
        .extend_from_slice(&source.button_diversions);
    target
        .button_remaps
        .extend_from_slice(&source.button_remaps);
}

fn setting_matches(device: &DeviceSnapshot, update: &SettingsUpdate) -> Option<bool> {
    macro_rules! check {
        ($field:ident, $actual:expr) => {
            if let Some(expected) = update.$field {
                return $actual.map(|actual| actual == expected);
            }
        };
    }
    check!(dpi, device.dpi);
    check!(invert_scroll, device.scroll_inverted());
    check!(high_resolution_scroll, device.scroll_high_resolution());
    check!(scroll_diversion, device.scroll_diversion());
    check!(smooth_scroll, device.smooth_scroll);
    check!(ratcheted, device.is_ratcheted());
    check!(
        smart_shift_threshold,
        device.smart_shift.as_ref().map(|s| s.threshold)
    );
    check!(
        smart_shift_torque,
        device.smart_shift.as_ref().and_then(|s| s.torque)
    );
    check!(fn_swap, device.fn_swap);
    check!(
        backlight_enabled,
        device.backlight.as_ref().map(|s| s.enabled)
    );
    check!(backlight_mode, device.backlight.as_ref().map(|s| s.mode));
    check!(backlight_level, device.backlight.as_ref().map(|s| s.level));
    if let Some((out, inside, power)) = update.backlight_durations {
        let rounded = |v: u16| v.div_ceil(5).saturating_mul(5);
        return device.backlight.as_ref().map(|s| {
            (s.hands_out, s.hands_in, s.powered) == (rounded(out), rounded(inside), rounded(power))
        });
    }
    check!(
        disable_keys,
        device.disable_keys.as_ref().map(|s| s.disabled)
    );
    check!(
        thumbwheel_inverted,
        device.thumbwheel.as_ref().map(|s| s.inverted)
    );
    check!(
        thumbwheel_diverted,
        device.thumbwheel.as_ref().map(|s| s.diverted)
    );
    check!(
        report_rate_us,
        device.report_rate.as_ref().map(|s| s.current_us)
    );
    check!(os_layout, device.os_layout.as_ref().map(|s| s.current));
    if let Some((cid, expected)) = update.button_diversions.first() {
        return device
            .buttons
            .iter()
            .find(|b| b.cid == *cid && b.reporting_available)
            .map(|b| b.diverted == *expected);
    }
    if let Some((cid, expected)) = update.button_remaps.first() {
        return device
            .buttons
            .iter()
            .find(|b| b.cid == *cid && b.reporting_available)
            .map(|b| b.mapped_to == *expected);
    }
    None
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostSwitchState {
    SentUnconfirmed,
    Error,
    NotSent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HostSwitchResult {
    pub device_key: String,
    pub target_host: u8,
    pub state: HostSwitchState,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct HostSwitchOutcome {
    pub devices: Vec<HostSwitchResult>,
}

fn validate_host(hp: &mut Hidpp, number: u8, target: u8) -> Result<u8> {
    let index = hp
        .feature_index(number, FEATURE_CHANGE_HOST)?
        .context("device does not expose ChangeHost")?;
    let info = hp.read_feature_checked(number, index, 0, &[])?;
    if info.len() < 2 || info[0] == 0 || info[0] > 16 || info[1] >= info[0] {
        bail!("invalid ChangeHost state");
    }
    if target >= info[0] {
        bail!(
            "target host {} is outside device's {} slots",
            target as u16 + 1,
            info[0]
        );
    }
    Ok(index)
}

fn open_device_connection(device: &DeviceSnapshot) -> Result<Hidpp> {
    if device.identity.unit_id.is_none() {
        if let Some(expected) = &device.identity.hid_uniq {
            let current = hidraw::logitech_interfaces()
                .into_iter()
                .find(|i| i.path == Path::new(&device.receiver_path))
                .context("device is no longer present")?;
            if valid_hid_uniq(&current.hid_uniq).as_ref() != Some(expected) {
                bail!("device endpoint identity changed; rescan before writing");
            }
        }
    }
    Hidpp::open(Path::new(&device.receiver_path))
}

fn verify_endpoint_identity(hp: &mut Hidpp, device: &DeviceSnapshot) -> Result<()> {
    if let Some(expected) = &device.identity.unit_id {
        let index = hp
            .feature_index(device.number, FEATURE_DEVICE_INFO)?
            .context("cannot verify the previously discovered unit ID")?;
        let info = hp.read_feature_checked(device.number, index, 0, &[])?;
        if info.len() < 5 || valid_serial(&hex_string(&info[1..5])).as_ref() != Some(expected) {
            bail!("device unit ID changed; no settings were written");
        }
    } else if let Some(expected) = &device.identity.pairing_serial {
        let pairing = read_pairing_info(hp, device.number, device.transport == Transport::Bolt)
            .context("cannot verify the previously discovered pairing identity")?;
        if pairing.wpid != device.wpid || valid_serial(&pairing.serial).as_ref() != Some(expected) {
            bail!("device pairing changed; no settings were written");
        }
    }
    Ok(())
}

/// Validate all selected devices before sending any switch. Send is not proof
/// of success: changing host intentionally disconnects, so no confirmation is
/// claimed and no target is saved. Devices are processed in caller order.
pub fn switch_hosts(devices: &[DeviceSnapshot], target_host: u8) -> Result<HostSwitchOutcome> {
    let _lock = hardware_lock();
    let connections = devices
        .iter()
        .map(|device| {
            if !device.online {
                bail!("device is offline");
            }
            open_device_connection(device)
        })
        .collect::<Vec<_>>();
    Ok(switch_hosts_with(devices, target_host, connections))
}

fn switch_hosts_with(
    devices: &[DeviceSnapshot],
    target_host: u8,
    connections: Vec<Result<Hidpp>>,
) -> HostSwitchOutcome {
    let mut prepared = Vec::new();
    let mut outcome = HostSwitchOutcome::default();
    let mut seen = std::collections::HashSet::new();
    for (device, connection) in devices.iter().zip(connections) {
        let checked = connection.and_then(|mut hp| {
            if !seen.insert(device.key()) {
                bail!("duplicate device in coordinated host switch");
            }
            verify_endpoint_identity(&mut hp, device)?;
            let index = validate_host(&mut hp, device.number, target_host)?;
            Ok((hp, index))
        });
        let error = checked.as_ref().err().map(|e| format!("{e:#}"));
        outcome.devices.push(HostSwitchResult {
            device_key: device.key(),
            target_host,
            state: if error.is_some() {
                HostSwitchState::Error
            } else {
                HostSwitchState::NotSent
            },
            error,
        });
        prepared.push(checked.ok());
    }
    if outcome
        .devices
        .iter()
        .any(|r| r.state == HostSwitchState::Error)
    {
        for result in &mut outcome.devices {
            if result.state == HostSwitchState::NotSent {
                result.error =
                    Some("another device failed validation; no switches were sent".into());
            }
        }
        return outcome;
    }
    for ((device, connection), result) in devices.iter().zip(prepared).zip(&mut outcome.devices) {
        if let Some((mut hp, index)) = connection {
            match hp.send_feature(device.number, index, 0x10, &[target_host]) {
                Ok(()) => result.state = HostSwitchState::SentUnconfirmed,
                Err(error) => {
                    result.state = HostSwitchState::Error;
                    result.error = Some(format!("{error:#}; command outcome may be unknown"));
                }
            }
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashSet, VecDeque};

    struct Fake {
        features: Vec<u16>,
        replies: VecDeque<Vec<u8>>,
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        values: BTreeMap<u16, Vec<u8>>,
        deny_writes: HashSet<u16>,
        drop_ack: HashSet<u16>,
        timeout_reads: HashSet<(u16, u8)>,
        registers: BTreeMap<(u16, u8), Vec<u8>>,
        unit: [u8; 4],
    }

    impl Fake {
        fn new(features: &[u16]) -> Self {
            let mut all = vec![FEATURE_FEATURE_SET];
            all.extend_from_slice(features);
            let mut values = BTreeMap::new();
            values.insert(FEATURE_HIRES_WHEEL, vec![0xA0]);
            values.insert(FEATURE_SMART_SHIFT, vec![2, 20]);
            values.insert(FEATURE_SMART_SHIFT_ENHANCED, vec![2, 20, 50]);
            values.insert(FEATURE_THUMB_WHEEL, vec![0xA0, 0xB0]);
            values.insert(FEATURE_REPORT_RATE, vec![1]);
            values.insert(FEATURE_EXTENDED_REPORT_RATE, vec![3]);
            values.insert(FEATURE_MULTIPLATFORM, vec![0]);
            values.insert(FEATURE_DUALPLATFORM, vec![0]);
            values.insert(FEATURE_CHANGE_HOST, vec![3, 0]);
            values.insert(FEATURE_NEW_FN_INVERSION, vec![0]);
            values.insert(FEATURE_ADJUSTABLE_DPI, vec![0, 3, 0x20, 0, 0]);
            Self {
                features: all,
                replies: VecDeque::new(),
                writes: Arc::new(Mutex::new(Vec::new())),
                values,
                deny_writes: HashSet::new(),
                drop_ack: HashSet::new(),
                timeout_reads: HashSet::new(),
                registers: BTreeMap::new(),
                unit: [0x12, 0, 0, 0x34],
            }
        }
        fn payload(&mut self, index: u8, function: u8, params: &[u8]) -> Result<Option<Vec<u8>>> {
            if index == 0 {
                if function == 0x10 {
                    return Ok(Some(params.to_vec()));
                }
                let id = u16::from_be_bytes([params[0], params[1]]);
                let found = self
                    .features
                    .iter()
                    .position(|f| *f == id)
                    .map(|p| p as u8 + 1)
                    .unwrap_or(0);
                return Ok(Some(vec![found, 0, 3]));
            }
            let id = self.features[index as usize - 1];
            if self.timeout_reads.contains(&(id, function)) {
                return Ok(None);
            }
            let write = match id {
                FEATURE_HIRES_WHEEL
                | FEATURE_THUMB_WHEEL
                | FEATURE_REPORT_RATE
                | FEATURE_SMART_SHIFT_ENHANCED => function == 0x20,
                FEATURE_EXTENDED_REPORT_RATE | FEATURE_MULTIPLATFORM => function == 0x30,
                FEATURE_ADJUSTABLE_DPI => function == 0x30,
                FEATURE_SMART_SHIFT | FEATURE_NEW_FN_INVERSION | FEATURE_CHANGE_HOST => {
                    function == 0x10
                }
                FEATURE_DUALPLATFORM => function == 0x20,
                _ => false,
            };
            if write {
                if self.deny_writes.contains(&id) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "fixture write denied",
                    )
                    .into());
                }
                let len = match id {
                    FEATURE_HIRES_WHEEL
                    | FEATURE_REPORT_RATE
                    | FEATURE_EXTENDED_REPORT_RATE
                    | FEATURE_NEW_FN_INVERSION
                    | FEATURE_DUALPLATFORM => 1,
                    FEATURE_THUMB_WHEEL | FEATURE_SMART_SHIFT => 2,
                    FEATURE_SMART_SHIFT_ENHANCED | FEATURE_ADJUSTABLE_DPI => 3,
                    _ => 1,
                };
                self.values.insert(
                    id,
                    if id == FEATURE_MULTIPLATFORM {
                        vec![params[1]]
                    } else {
                        params[..len].to_vec()
                    },
                );
                if self.drop_ack.contains(&id) {
                    return Ok(None);
                }
                return Ok(Some(vec![0; 16]));
            }
            let value = match id {
                FEATURE_FEATURE_SET if function == 0 => vec![self.features.len() as u8],
                FEATURE_FEATURE_SET => {
                    let id = self.features[params[0] as usize - 1];
                    vec![(id >> 8) as u8, id as u8, 0, 3]
                }
                FEATURE_DEVICE_INFO if function == 0 => {
                    let mut ids = vec![1];
                    ids.extend(self.unit);
                    ids.extend([0, 4, 0x40, 0x69, 0, 0, 0, 0]);
                    ids
                }
                FEATURE_DEVICE_INFO => {
                    vec![0, b'M', b'X', b'R', 0x12, 0x34, 0x01, 0x05, 0, 0xAB, 0]
                }
                FEATURE_SMART_SHIFT_ENHANCED if function == 0 => vec![1],
                FEATURE_REPORT_RATE if function == 0 => vec![0x83],
                FEATURE_EXTENDED_REPORT_RATE if function == 0x10 => vec![0, 0x7F],
                FEATURE_MULTIPLATFORM if function == 0 => {
                    vec![2, 0, 2, 0, 0, 0, self.values[&id][0]]
                }
                FEATURE_MULTIPLATFORM => {
                    if params[0] == 0 {
                        vec![0, 0, 0x05, 0, 0, 0, 0, 0]
                    } else {
                        vec![7, 0, 0x20, 0, 0, 0, 0, 0]
                    }
                }
                FEATURE_ADJUSTABLE_DPI if function == 0x10 => vec![0, 3, 0x20, 6, 0x40, 0, 0],
                _ => self.values.get(&id).cloned().unwrap_or_default(),
            };
            Ok(Some(value))
        }
    }

    impl crate::hidpp::Transport for Fake {
        fn drain(&mut self) -> Result<()> {
            self.replies.clear();
            Ok(())
        }
        fn write_report(&mut self, report: &[u8]) -> Result<()> {
            self.writes.lock().unwrap().push(report.to_vec());
            // Receiver register fixture acknowledges exactly the requested ID.
            let payload = if report[2] >= 0x80 {
                if report[2] & 1 != 0 {
                    self.registers
                        .get(&(
                            (((report[2] & 2) as u16) << 8) | report[3] as u16,
                            report[4],
                        ))
                        .cloned()
                } else {
                    Some(vec![0; 16])
                }
            } else {
                self.payload(report[2], report[3] & 0xF0, &report[4..])?
            };
            if let Some(payload) = payload {
                let mut reply = report[..4].to_vec();
                reply.extend(payload);
                self.replies.push_back(reply);
            }
            Ok(())
        }
        fn read_report(&mut self, _: Duration) -> Result<Option<Vec<u8>>> {
            Ok(self.replies.pop_front())
        }
    }

    fn live_device(number: u8) -> DeviceSnapshot {
        DeviceSnapshot {
            online: true,
            number,
            receiver_path: "fixture-only".into(),
            ..Default::default()
        }
    }

    #[test]
    fn hardware_lock_is_reentrant_and_nested_guards_keep_other_threads_out() {
        let outer = hardware_lock();
        let inner = hardware_lock();
        drop(outer);
        assert!(
            std::thread::spawn(|| HARDWARE.try_lock().is_err())
                .join()
                .unwrap()
        );
        drop(inner);
        assert!(
            std::thread::spawn(|| HARDWARE.try_lock().is_ok())
                .join()
                .unwrap()
        );
    }

    #[test]
    fn identity_rejects_ambiguous_fallback_and_matches_across_transports() {
        for value in [
            "",
            "00000000",
            "00:00:00:00:00:00",
            "ff:ff:ff:ff:ff:ff",
            "Logitech",
            "usb-0000:00/input2",
            "12345678",
            "MXMASTER3S",
            "ABC123XYZ",
            "AAAAAAAAAAAA",
            "DEADBEEF",
        ] {
            assert_eq!(valid_hid_uniq(value), None, "{value}");
        }
        assert_eq!(
            valid_hid_uniq("AA:12:34:56:78:90"),
            Some("AA1234567890".into())
        );
        let mut devices = Vec::new();
        for (number, transport) in [
            (1, Transport::Bolt),
            (0xFF, Transport::Bluetooth),
            (0xFF, Transport::Usb),
        ] {
            let mut hp = Hidpp::with_transport(Fake::new(&[FEATURE_DEVICE_INFO]));
            let mut device = live_device(number);
            device.transport = transport;
            read_device_features(&mut hp, number, &mut device);
            assert_eq!(device.serial, "12000034");
            assert_eq!(device.firmware[0].version, "12.34.B0105");
            assert_eq!(device.firmware[0].extra, [0xAB]);
            assert!(
                device
                    .features
                    .iter()
                    .any(|f| f.id == FEATURE_DEVICE_INFO && f.version == 3)
            );
            devices.push(device);
        }
        assert_eq!(devices[0].stable_key(), devices[1].stable_key());
        assert_eq!(devices[1].stable_key(), devices[2].stable_key());
        let mut fake = Fake::new(&[FEATURE_DEVICE_INFO]);
        fake.unit = [0; 4];
        let mut device = live_device(0xFF);
        read_device_features(&mut Hidpp::with_transport(fake), 0xFF, &mut device);
        assert!(device.serial.is_empty());
        assert_eq!(device.stable_key(), None);
    }

    #[test]
    fn direct_and_receiver_legacy_smartshift_use_identical_read_and_write_protocol() {
        for number in [1, 0xFF] {
            let fake = Fake::new(&[FEATURE_SMART_SHIFT]);
            let writes = fake.writes.clone();
            let mut hp = Hidpp::with_transport(fake);
            let outcome = apply_verified_with(
                &mut hp,
                &live_device(number),
                &SettingsUpdate {
                    smart_shift_threshold: Some(50),
                    ratcheted: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(outcome.all_confirmed(), "{:?}", outcome.results);
            assert_eq!(outcome.snapshot.unwrap().smart_shift.unwrap().threshold, 50);
            let writes = writes.lock().unwrap();
            assert!(writes.iter().any(|w| w[1] == number
                && w[2] == 2
                && w[3] & 0xF0 == 0x10
                && w[4..6] == [2, 255]));
            assert!(!writes.iter().any(|w| w[2] == 2 && w[3] & 0xF0 == 0x20));
        }
    }

    #[test]
    fn partial_apply_reads_back_after_error_and_only_saves_confirmed_subset() {
        let mut fake = Fake::new(&[FEATURE_HIRES_WHEEL, FEATURE_NEW_FN_INVERSION]);
        fake.deny_writes.insert(FEATURE_NEW_FN_INVERSION);
        let outcome = apply_verified_with(
            &mut Hidpp::with_transport(fake),
            &live_device(1),
            &SettingsUpdate {
                invert_scroll: Some(true),
                fn_swap: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(outcome.results[0].state, ApplyState::Confirmed);
        assert_eq!(outcome.results[1].state, ApplyState::Failed);
        assert!(
            outcome.results[1]
                .error
                .as_ref()
                .unwrap()
                .contains("denied")
        );
        assert_eq!(outcome.confirmed_update.invert_scroll, Some(true));
        assert_eq!(outcome.confirmed_update.fn_swap, None);
        assert_eq!(outcome.snapshot.unwrap().fn_swap, Some(false));
    }

    #[test]
    fn unreadable_state_is_not_replaced_by_cached_state_or_unsupported() {
        let mut fake = Fake::new(&[FEATURE_HIRES_WHEEL]);
        fake.timeout_reads.insert((FEATURE_HIRES_WHEEL, 0x10));
        let mut device = live_device(1);
        device.scroll_flags = Some(4);
        let outcome = apply_verified_with(
            &mut Hidpp::with_transport(fake),
            &device,
            &SettingsUpdate {
                invert_scroll: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(outcome.confirmed_update.invert_scroll, None);
        let readback = outcome.snapshot.unwrap();
        assert_eq!(readback.scroll_flags, None);
        assert_eq!(
            readback.capabilities["scroll"].state,
            CapabilityState::Unavailable
        );
        assert!(
            outcome
                .read_back_errors
                .iter()
                .any(|e| e.contains("timed out"))
        );
    }

    #[test]
    fn lost_write_ack_can_still_confirm_actual_state_without_claiming_rollback() {
        let mut fake = Fake::new(&[FEATURE_HIRES_WHEEL]);
        fake.drop_ack.insert(FEATURE_HIRES_WHEEL);
        let outcome = apply_verified_with(
            &mut Hidpp::with_transport(fake),
            &live_device(1),
            &SettingsUpdate {
                invert_scroll: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(outcome.results[0].state, ApplyState::Confirmed);
        assert!(
            outcome.results[0]
                .error
                .as_ref()
                .unwrap()
                .contains("timed out")
        );
        assert_eq!(outcome.confirmed_update.invert_scroll, Some(true));
    }

    #[test]
    fn changed_unit_identity_blocks_apply_before_any_setting_write() {
        let fake = Fake::new(&[FEATURE_DEVICE_INFO, FEATURE_HIRES_WHEEL]);
        let writes = fake.writes.clone();
        let mut device = live_device(1);
        device.identity.unit_id = Some("DEADBEEF".into());
        assert!(
            apply_verified_with(
                &mut Hidpp::with_transport(fake),
                &device,
                &SettingsUpdate {
                    invert_scroll: Some(true),
                    ..Default::default()
                }
            )
            .unwrap_err()
            .to_string()
            .contains("unit ID changed")
        );
        assert!(
            writes
                .lock()
                .unwrap()
                .iter()
                .all(|w| w[2] != 3 || w[3] & 0xF0 != 0x20)
        );
    }

    #[test]
    fn unpair_rechecks_receiver_and_slot_identity_before_destructive_write() {
        let receiver = ReceiverSnapshot {
            path: "fixture-only".into(),
            product_id: 0xC548,
            serial: "1234567890ABCDEF".into(),
            max_devices: 6,
            ..Default::default()
        };
        let mut device = live_device(1);
        device.wpid = "4069".into();
        device.identity.pairing_serial = Some("AABBCCDD".into());
        for (wpid, serial, allowed) in [
            ("4069", [0xAA, 0xBB, 0xCC, 0xDD], true),
            ("4070", [0xAA, 0xBB, 0xCC, 0xDD], false),
            ("4069", [0x11, 0x22, 0x33, 0x44], false),
            ("4069", [0; 4], false),
        ] {
            let mut fake = Fake::new(&[]);
            fake.registers.insert(
                (0x2FB, 0),
                vec![0x12, 0x34, 0x56, 0x78, 0x90, 0xAB, 0xCD, 0xEF],
            );
            let mut pairing = vec![0x51, 2, if wpid == "4069" { 0x69 } else { 0x70 }, 0x40];
            pairing.extend(serial);
            fake.registers.insert((0x2B5, 0x51), pairing);
            let writes = fake.writes.clone();
            let result = unpair_selected_with(&mut Hidpp::with_transport(fake), &receiver, &device);
            assert_eq!(result.is_ok(), allowed, "{wpid} {serial:?}: {result:?}");
            assert_eq!(
                writes
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|w| w[2..4] == [0x82, 0xC1]),
                allowed
            );
        }
        let fake = Fake::new(&[]);
        let writes = fake.writes.clone();
        assert!(
            unpair_selected_with(&mut Hidpp::with_transport(fake), &receiver, &device).is_err()
        );
        assert!(
            !writes
                .lock()
                .unwrap()
                .iter()
                .any(|w| w[2..4] == [0x82, 0xC1])
        );
    }

    #[test]
    fn thumbwheel_report_rate_and_platform_writes_are_capability_gated() {
        let fake = Fake::new(&[
            FEATURE_THUMB_WHEEL,
            FEATURE_EXTENDED_REPORT_RATE,
            FEATURE_MULTIPLATFORM,
        ]);
        let writes = fake.writes.clone();
        let outcome = apply_verified_with(
            &mut Hidpp::with_transport(fake),
            &live_device(1),
            &SettingsUpdate {
                thumbwheel_inverted: Some(true),
                report_rate_us: Some(125),
                os_layout: Some(7),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(outcome.all_confirmed(), "{:?}", outcome.results);
        let writes = writes.lock().unwrap();
        assert!(
            writes
                .iter()
                .any(|w| w[2] == 2 && w[3] & 0xF0 == 0x20 && w[4..6] == [0xA0, 0xB1])
        );
        assert!(
            writes
                .iter()
                .any(|w| w[2] == 3 && w[3] & 0xF0 == 0x30 && w[4] == 6)
        );
        assert!(
            writes
                .iter()
                .any(|w| w[2] == 4 && w[3] & 0xF0 == 0x30 && w[4..6] == [0xFF, 7])
        );
        drop(writes);
        let fake = Fake::new(&[FEATURE_REPORT_RATE, FEATURE_MULTIPLATFORM]);
        let writes = fake.writes.clone();
        let outcome = apply_verified_with(
            &mut Hidpp::with_transport(fake),
            &live_device(1),
            &SettingsUpdate {
                report_rate_us: Some(3000),
                os_layout: Some(6),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(outcome.has_failures());
        assert!(
            writes
                .lock()
                .unwrap()
                .iter()
                .all(|w| w[2] == 0 || !matches!(w[3] & 0xF0, 0x20 | 0x30))
        );
    }

    #[test]
    fn capacity_and_unpair_never_assume_every_receiver_is_unifying() {
        assert_eq!(receiver_capabilities(0xC534, None).max_slots, 2);
        assert_eq!(
            receiver_capabilities(0xC52B, Some(&[3, 1, 2, 3, 4, 0, 4])).max_slots,
            4
        );
        assert_eq!(
            receiver_capabilities(0xC52B, Some(&[3, 1, 2, 3, 4, 0, 255])).max_slots,
            6
        );
        for product in [0xC534, 0xC539, 0xC53F, 0xC547, 0xC599] {
            let fake = Fake::new(&[]);
            let writes = fake.writes.clone();
            assert!(unpair_with(&mut Hidpp::with_transport(fake), product, 1).is_err());
            assert!(writes.lock().unwrap().is_empty());
        }
        for (product, request) in [(0xC52B, [0x80, 0xB2]), (0xC548, [0x82, 0xC1])] {
            let fake = Fake::new(&[]);
            let writes = fake.writes.clone();
            unpair_with(&mut Hidpp::with_transport(fake), product, 4).unwrap();
            assert_eq!(
                writes.lock().unwrap()[0][2..6],
                [request[0], request[1], 3, 4]
            );
        }
    }

    #[test]
    fn standard_rate_dual_platform_and_enhanced_torque_paths_verify() {
        let fake = Fake::new(&[
            FEATURE_REPORT_RATE,
            FEATURE_DUALPLATFORM,
            FEATURE_SMART_SHIFT_ENHANCED,
        ]);
        let outcome = apply_verified_with(
            &mut Hidpp::with_transport(fake),
            &live_device(0xFF),
            &SettingsUpdate {
                report_rate_us: Some(2000),
                os_layout: Some(1),
                smart_shift_torque: Some(65),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(outcome.all_confirmed(), "{:?}", outcome.results);
        let state = outcome.snapshot.unwrap();
        assert_eq!(state.report_rate.unwrap().current_us, 2000);
        assert!(state.os_layout.unwrap().dual_platform);
        assert_eq!(state.smart_shift.unwrap().torque, Some(65));
    }

    #[test]
    fn invalid_torque_and_unadvertised_dpi_never_send_setting_writes() {
        let fake = Fake::new(&[FEATURE_ADJUSTABLE_DPI, FEATURE_SMART_SHIFT]);
        let writes = fake.writes.clone();
        let outcome = apply_verified_with(
            &mut Hidpp::with_transport(fake),
            &live_device(1),
            &SettingsUpdate {
                dpi: Some(1200),
                smart_shift_torque: Some(60),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(outcome.has_failures());
        assert!(settings_empty(&outcome.confirmed_update));
        assert!(
            !writes
                .lock()
                .unwrap()
                .iter()
                .any(|w| (w[2] == 2 && w[3] & 0xF0 == 0x30) || (w[2] == 3 && w[3] & 0xF0 == 0x10))
        );
        let mut fake = Fake::new(&[FEATURE_ADJUSTABLE_DPI]);
        fake.timeout_reads.insert((FEATURE_ADJUSTABLE_DPI, 0x10));
        let writes = fake.writes.clone();
        let outcome = apply_verified_with(
            &mut Hidpp::with_transport(fake),
            &live_device(1),
            &SettingsUpdate {
                dpi: Some(800),
                ..Default::default()
            },
        )
        .unwrap();
        // Even if 800 was already current, the failed write attempt is explicit.
        assert!(
            outcome.results[0]
                .error
                .as_ref()
                .unwrap()
                .contains("unavailable")
        );
        assert!(
            !writes
                .lock()
                .unwrap()
                .iter()
                .any(|w| w[2] == 2 && w[3] & 0xF0 == 0x30)
        );
    }

    #[test]
    fn incomplete_inventory_never_claims_feature_absence() {
        let mut fake = Fake::new(&[]);
        fake.timeout_reads.insert((FEATURE_FEATURE_SET, 0));
        let mut device = live_device(1);
        read_device_features(&mut Hidpp::with_transport(fake), 1, &mut device);
        assert!(
            device
                .capabilities
                .values()
                .all(|cap| cap.state == CapabilityState::Unavailable)
        );
        let mut absent = live_device(1);
        read_device_features(&mut Hidpp::with_transport(Fake::new(&[])), 1, &mut absent);
        assert_eq!(
            absent.capabilities["thumbwheel"].state,
            CapabilityState::Unsupported
        );
    }

    #[test]
    fn coordinated_host_switch_validates_every_device_before_any_send() {
        let devices = [live_device(1), live_device(2)];
        let first = Fake::new(&[FEATURE_CHANGE_HOST]);
        let first_writes = first.writes.clone();
        let second = Fake::new(&[]);
        let outcome = switch_hosts_with(
            &devices,
            1,
            vec![
                Ok(Hidpp::with_transport(first)),
                Ok(Hidpp::with_transport(second)),
            ],
        );
        assert_eq!(outcome.devices[0].state, HostSwitchState::NotSent);
        assert_eq!(outcome.devices[1].state, HostSwitchState::Error);
        assert!(
            !first_writes
                .lock()
                .unwrap()
                .iter()
                .any(|w| w[2] != 0 && w[3] & 0xF0 == 0x10)
        );
        let connections = devices
            .iter()
            .map(|_| Ok(Hidpp::with_transport(Fake::new(&[FEATURE_CHANGE_HOST]))))
            .collect();
        let outcome = switch_hosts_with(&devices, 2, connections);
        assert!(
            outcome
                .devices
                .iter()
                .all(|r| r.state == HostSwitchState::SentUnconfirmed)
        );
    }

    #[test]
    fn host_switch_is_zero_based_and_never_serializes_as_saved_setting() {
        let outcome = apply_verified_with(
            &mut Hidpp::with_transport(Fake::new(&[FEATURE_CHANGE_HOST])),
            &live_device(1),
            &SettingsUpdate {
                target_host: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(outcome.results[0].state, ApplyState::SentUnconfirmed);
        assert_eq!(outcome.confirmed_update.target_host, None);
        let json = serde_json::to_value(SettingsUpdate {
            target_host: Some(2),
            ..Default::default()
        })
        .unwrap();
        assert!(json.get("target_host").is_none());
        let mut hp = Hidpp::with_transport(Fake::new(&[FEATURE_CHANGE_HOST]));
        assert!(validate_host(&mut hp, 1, 3).is_err());
    }

    #[test]
    fn malformed_decoders_reject_short_and_unadvertised_values() {
        assert!(decode_firmware(0, &[]).is_none());
        assert!(decode_firmware(0, &[0, 1, 2]).is_none());
        assert!(decode_report_rate(&[], &[1], false).is_none());
        assert!(decode_report_rate(&[0, 1], &[7], true).is_none());
        assert!(decode_report_rate(&[1], &[8], false).is_none());
    }

    #[test]
    fn legacy_unknown_charge_is_not_zero_percent() {
        for status in 0..=7 {
            let mut device = DeviceSnapshot::default();
            decode_legacy_battery(&mut device, &[0, 20, status]);
            assert_eq!(device.battery, None, "status={status}");
            assert_eq!(device.battery_status, Some(status));
            assert_eq!(device.battery_level, Some(20));
        }
    }

    #[test]
    fn legacy_decoder_preserves_real_levels_and_rejects_invalid_readings() {
        for (level, expected) in [(5, Some(5)), (90, Some(90)), (100, Some(100)), (255, None)] {
            let mut device = DeviceSnapshot::default();
            decode_legacy_battery(&mut device, &[level, 0, 1]);
            assert_eq!(device.battery, expected);
            assert!(device.battery_is_charging());
        }
        let mut device = DeviceSnapshot::default();
        decode_legacy_battery(&mut device, &[0, 20]);
        assert_eq!(device.battery, None);
        assert_eq!(device.battery_status, None);
    }
}
