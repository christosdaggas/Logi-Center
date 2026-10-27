use std::sync::mpsc::Sender;

use ksni::Tray;
use ksni::menu::{MenuItem, StandardItem, SubMenu};
use log::warn;

use crate::model::ReceiverSnapshot;

#[derive(Clone, Debug)]
pub enum TrayCommand {
    Show,
    ShowDevice(String),
    Refresh,
    Profiles,
    SelectProfile { device_key: String, name: String },
    Peripherals,
    Diagnostics,
    About,
    Quit,
    Availability(bool),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum BatteryAlert {
    #[default]
    Normal,
    Low,
    Critical,
}

impl BatteryAlert {
    fn for_level(level: Option<u8>) -> Self {
        match level {
            Some(0..=10) => Self::Critical,
            Some(11..=20) => Self::Low,
            _ => Self::Normal,
        }
    }

    fn color(self) -> Option<[u8; 3]> {
        match self {
            Self::Normal => None,
            Self::Low => Some([246, 211, 45]),
            Self::Critical => Some([224, 27, 36]),
        }
    }
}

// Keep only presentation data here, rather than cloning device settings and
// configuration into the tray service on every scan.
#[derive(Debug, PartialEq, Eq)]
struct TrayDevice {
    key: String,
    label: String,
    alert: BatteryAlert,
}

fn connected_devices(receivers: &[ReceiverSnapshot]) -> Vec<TrayDevice> {
    receivers
        .iter()
        .flat_map(|receiver| &receiver.devices)
        .filter(|device| device.online)
        .map(|device| {
            let level = device.battery.filter(|level| *level <= 100);
            let alert = BatteryAlert::for_level(level);
            let battery = level.map(|level| format!("{level}%")).unwrap_or_else(|| {
                if device.battery_status.is_some() {
                    "Percentage unavailable".to_string()
                } else {
                    "Battery unavailable".to_string()
                }
            });
            // Device-provided names must not create extra rows in a tooltip.
            let name = device.name.split_whitespace().collect::<Vec<_>>().join(" ");
            let name = if name.is_empty() {
                "Logitech device"
            } else {
                &name
            };
            let mut label = format!("{name} — {battery}");
            match alert {
                BatteryAlert::Critical => label.push_str(" · Very low battery"),
                BatteryAlert::Low => label.push_str(" · Low battery"),
                BatteryAlert::Normal => {}
            }
            if device.battery_is_charging() {
                label.push_str(" · Charging");
            } else if device.battery_status == Some(0x03) {
                label.push_str(" · Fully charged");
            }
            TrayDevice {
                key: device.key(),
                label,
                alert,
            }
        })
        .collect()
}

pub struct LogiCenterTray {
    sender: Sender<TrayCommand>,
    icons: [Vec<ksni::Icon>; 3],
    devices: Vec<TrayDevice>,
    alert: BatteryAlert,
    scanned: bool,
    profiles: Vec<ProfileGroup>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileGroup {
    pub device_key: String,
    pub device_name: String,
    pub names: Vec<(String, bool)>,
}

impl LogiCenterTray {
    fn new(sender: Sender<TrayCommand>) -> Self {
        // Decode once and cache the two colored variants (48 x 48 pixels each).
        let icon = load_icon();
        Self {
            sender,
            icons: [
                icon.clone(),
                tint_icon(&icon, BatteryAlert::Low),
                tint_icon(&icon, BatteryAlert::Critical),
            ],
            devices: Vec::new(),
            alert: BatteryAlert::Normal,
            scanned: false,
            profiles: Vec::new(),
        }
    }

    fn set_devices(&mut self, devices: Vec<TrayDevice>) {
        self.alert = devices
            .iter()
            .map(|device| device.alert)
            .max()
            .unwrap_or_default();
        self.devices = devices;
        self.scanned = true;
    }

    fn empty_message(&self) -> &'static str {
        if self.scanned {
            "No connected devices"
        } else {
            "Scanning devices…"
        }
    }

    fn command(&self, command: TrayCommand) {
        let _ = self.sender.send(command);
    }
}

impl Tray for LogiCenterTray {
    // Let the desktop host place its own popup: no global coordinates or X11
    // window positioning are needed on GNOME/KDE Wayland.
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        crate::APPLICATION_ID.to_string()
    }

    fn title(&self) -> String {
        "Logi Center".to_string()
    }

    fn icon_name(&self) -> String {
        // Always use the embedded root symbolic.svg, not a potentially stale
        // installed icon. Named symbolic icons also override warning colors.
        String::new()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icons[self.alert as usize].clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let description = if self.devices.is_empty() {
            self.empty_message().to_string()
        } else {
            self.devices
                .iter()
                .map(|device| {
                    glib::markup_escape_text(&crate::i18n::tray_label(&device.label)).to_string()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        ksni::ToolTip {
            title: self.title(),
            description,
            ..Default::default()
        }
    }

    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        self.command(TrayCommand::Show);
    }

    fn watcher_online(&self) {
        self.command(TrayCommand::Availability(true));
    }

    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        self.command(TrayCommand::Availability(false));
        true
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items = Vec::with_capacity(self.devices.len() + 7);
        if self.devices.is_empty() {
            items.push(
                StandardItem {
                    label: self.empty_message().to_string(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        } else {
            for device in &self.devices {
                let key = device.key.clone();
                items.push(
                    StandardItem {
                        // D-Bus menus interpret a single underscore as a mnemonic.
                        label: crate::i18n::tray_label(&device.label).replace('_', "__"),
                        activate: Box::new(move |tray: &mut LogiCenterTray| {
                            tray.command(TrayCommand::ShowDevice(key.clone()));
                        }),
                        ..Default::default()
                    }
                    .into(),
                );
            }
        }
        items.push(MenuItem::Separator);
        if !self.profiles.is_empty() {
            let devices = self
                .profiles
                .iter()
                .map(|group| {
                    let profiles = group
                        .names
                        .iter()
                        .map(|(name, active)| {
                            let device_key = group.device_key.clone();
                            let selected_name = name.clone();
                            StandardItem {
                                label: format!(
                                    "{}{}",
                                    if *active { "✓ " } else { "" },
                                    name.replace('_', "__")
                                ),
                                activate: Box::new(move |tray: &mut LogiCenterTray| {
                                    tray.command(TrayCommand::SelectProfile {
                                        device_key: device_key.clone(),
                                        name: selected_name.clone(),
                                    });
                                }),
                                ..Default::default()
                            }
                            .into()
                        })
                        .collect();
                    SubMenu {
                        label: group.device_name.replace('_', "__"),
                        submenu: profiles,
                        ..Default::default()
                    }
                    .into()
                })
                .collect();
            items.push(
                SubMenu {
                    label: crate::i18n::tr("Device profiles"),
                    submenu: devices,
                    ..Default::default()
                }
                .into(),
            );
        }
        items.extend([
            StandardItem {
                label: crate::i18n::tr("Show Logi Center"),
                activate: Box::new(|tray: &mut LogiCenterTray| tray.command(TrayCommand::Show)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: crate::i18n::tr("Refresh Devices"),
                activate: Box::new(|tray: &mut LogiCenterTray| tray.command(TrayCommand::Refresh)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: crate::i18n::tr("Device profiles"),
                activate: Box::new(|tray: &mut LogiCenterTray| tray.command(TrayCommand::Profiles)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: crate::i18n::tr("Gaming, cameras, lights and audio"),
                activate: Box::new(|tray: &mut LogiCenterTray| {
                    tray.command(TrayCommand::Peripherals)
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: crate::i18n::tr("Diagnostics and battery"),
                activate: Box::new(|tray: &mut LogiCenterTray| {
                    tray.command(TrayCommand::Diagnostics)
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: crate::i18n::tr("About Logi Center"),
                activate: Box::new(|tray: &mut LogiCenterTray| tray.command(TrayCommand::About)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: crate::i18n::tr("Quit"),
                activate: Box::new(|tray: &mut LogiCenterTray| tray.command(TrayCommand::Quit)),
                ..Default::default()
            }
            .into(),
        ]);
        items
    }
}

pub struct TrayHandle(ksni::blocking::Handle<LogiCenterTray>);

impl TrayHandle {
    pub fn update_devices(&self, receivers: &[ReceiverSnapshot]) {
        let devices = connected_devices(receivers);
        self.0.update(move |tray| tray.set_devices(devices));
    }

    pub fn update_profiles(&self, profiles: Vec<ProfileGroup>) {
        self.0.update(move |tray| tray.profiles = profiles);
    }

    pub fn is_closed(&self) -> bool {
        self.0.is_closed()
    }
}

pub fn start(sender: Sender<TrayCommand>) -> Option<TrayHandle> {
    use ksni::blocking::TrayMethods;

    let tray = LogiCenterTray::new(sender);
    if tray.icons[0].is_empty() {
        // Do not enable close-to-tray if the SVG loader is unavailable and the
        // host would have no visible icon through which to restore the window.
        return None;
    }
    match tray.spawn() {
        Ok(handle) => Some(TrayHandle(handle)),
        Err(error) => {
            warn!("could not start the StatusNotifierItem tray: {error}");
            None
        }
    }
}

fn load_icon() -> Vec<ksni::Icon> {
    use gtk4::{gdk_pixbuf, gio};

    let bytes = glib::Bytes::from_static(include_bytes!("../docs/symbolic.svg"));
    let stream = gio::MemoryInputStream::from_bytes(&bytes);
    let image = match gdk_pixbuf::Pixbuf::from_stream_at_scale(
        &stream,
        48,
        48,
        true,
        None::<&gio::Cancellable>,
    ) {
        Ok(image) => image,
        Err(error) => {
            warn!("could not render embedded symbolic.svg: {error}");
            return Vec::new();
        }
    };
    let (width, height) = (image.width(), image.height());
    let pixels = image.read_pixel_bytes();
    let channels = image.n_channels() as usize;
    let stride = image.rowstride() as usize;
    let mut data = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height as usize {
        let row = &pixels[y * stride..y * stride + width as usize * channels];
        for pixel in row.chunks_exact(channels) {
            let alpha = if image.has_alpha() { pixel[3] } else { 255 };
            // StatusNotifierItem uses ARGB32 in network byte order.
            data.extend_from_slice(&[alpha, pixel[0], pixel[1], pixel[2]]);
        }
    }
    vec![ksni::Icon {
        width,
        height,
        data,
    }]
}

fn tint_icon(icons: &[ksni::Icon], alert: BatteryAlert) -> Vec<ksni::Icon> {
    let mut icons = icons.to_vec();
    if let Some(color) = alert.color() {
        for icon in &mut icons {
            for pixel in icon.data.as_chunks_mut::<4>().0 {
                // ARGB: retain alpha, including antialiased transparent edges.
                pixel[1..].copy_from_slice(&color);
            }
        }
    }
    icons
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DeviceSnapshot;
    use std::sync::mpsc;

    fn device(name: &str, number: u8, battery: Option<u8>, online: bool) -> DeviceSnapshot {
        DeviceSnapshot {
            receiver_path: "/dev/hidraw-test".to_string(),
            number,
            name: name.to_string(),
            battery,
            online,
            ..Default::default()
        }
    }

    fn receivers(devices: Vec<DeviceSnapshot>) -> Vec<ReceiverSnapshot> {
        vec![ReceiverSnapshot {
            devices,
            ..Default::default()
        }]
    }

    #[test]
    fn battery_threshold_boundaries() {
        for (level, expected) in [
            (None, BatteryAlert::Normal),
            (Some(0), BatteryAlert::Critical),
            (Some(10), BatteryAlert::Critical),
            (Some(11), BatteryAlert::Low),
            (Some(20), BatteryAlert::Low),
            (Some(21), BatteryAlert::Normal),
            (Some(100), BatteryAlert::Normal),
            (Some(255), BatteryAlert::Normal),
        ] {
            assert_eq!(BatteryAlert::for_level(level), expected, "level={level:?}");
        }
    }

    #[test]
    fn menu_lists_online_devices_and_opens_the_clicked_device() {
        let (tx, rx) = mpsc::channel();
        let mut tray = LogiCenterTray::new(tx);
        let mut inventory = receivers(vec![
            device("MX_Mouse", 1, Some(75), true),
            device("Offline", 2, Some(1), false),
        ]);
        inventory.push(ReceiverSnapshot {
            devices: vec![device("Keyboard", 3, None, true)],
            ..Default::default()
        });
        tray.set_devices(connected_devices(&inventory));

        assert_eq!(tray.devices.len(), 2);
        assert_eq!(tray.alert, BatteryAlert::Normal);
        let menu = tray.menu();
        let MenuItem::Standard(mouse) = &menu[0] else {
            panic!("missing mouse row")
        };
        assert_eq!(mouse.label, "MX__Mouse — 75%");
        let MenuItem::Standard(keyboard) = &menu[1] else {
            panic!("missing keyboard row")
        };
        assert_eq!(keyboard.label, "Keyboard — Battery unavailable");
        assert!(matches!(menu[2], MenuItem::Separator));
        (keyboard.activate)(&mut tray);
        assert!(
            matches!(rx.try_recv().unwrap(), TrayCommand::ShowDevice(key) if key == "/dev/hidraw-test:3")
        );
    }

    #[test]
    fn most_urgent_device_wins_and_alert_recovers() {
        let (tx, _) = mpsc::channel();
        let mut tray = LogiCenterTray::new(tx);
        let mut inventory = receivers(vec![
            device("Mouse", 1, Some(15), true),
            device("Keyboard", 2, Some(5), true),
        ]);
        inventory[0].devices[1].battery_status = Some(1);
        tray.set_devices(connected_devices(&inventory));
        assert_eq!(tray.alert, BatteryAlert::Critical);
        assert!(
            tray.devices[1]
                .label
                .contains("5% · Very low battery · Charging")
        );

        inventory[0].devices[1].online = false;
        tray.set_devices(connected_devices(&inventory));
        assert_eq!(tray.alert, BatteryAlert::Low);
        assert!(tray.devices[0].label.contains("15% · Low battery"));

        inventory[0].devices[0].battery = Some(80);
        tray.set_devices(connected_devices(&inventory));
        assert_eq!(tray.alert, BatteryAlert::Normal);
        assert_eq!(tray.icon_pixmap()[0].data, tray.icons[0][0].data);

        tray.set_devices(Vec::new());
        assert_eq!(tray.alert, BatteryAlert::Normal);
        let MenuItem::Standard(empty) = &tray.menu()[0] else {
            panic!("missing empty row")
        };
        assert_eq!(empty.label, "No connected devices");
        assert!(!empty.enabled);
    }

    #[test]
    fn warning_icons_preserve_alpha_and_cannot_be_overridden_by_symbolic_name() {
        let (tx, _) = mpsc::channel();
        let mut tray = LogiCenterTray::new(tx);
        let normal = tray.icon_pixmap();
        assert!(tray.icon_name().is_empty());
        assert_eq!(normal.len(), 1);
        assert_eq!(normal[0].data.len(), 48 * 48 * 4);
        assert!(
            normal[0]
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[0] > 0)
        );
        assert!(
            normal[0]
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[0] == 0)
        );

        for (level, color) in [(20, [246, 211, 45]), (10, [224, 27, 36])] {
            tray.set_devices(connected_devices(&receivers(vec![device(
                "Mouse",
                1,
                Some(level),
                true,
            )])));
            assert!(tray.icon_name().is_empty());
            let tinted = tray.icon_pixmap();
            assert_eq!(tinted[0].width, normal[0].width);
            assert_eq!(tinted[0].height, normal[0].height);
            for (original, colored) in normal[0]
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .zip(tinted[0].data.as_chunks::<4>().0)
            {
                assert_eq!(original[0], colored[0]);
                assert_eq!(&colored[1..], &color);
            }
        }
    }

    #[test]
    fn tooltip_escapes_names_and_invalid_battery_is_unavailable() {
        let (tx, _) = mpsc::channel();
        let mut tray = LogiCenterTray::new(tx);
        assert_eq!(tray.tool_tip().description, "Scanning devices…");
        tray.set_devices(connected_devices(&receivers(vec![
            device("Mouse\n<&>", 1, Some(255), true),
            device("Keyboard", 2, Some(42), true),
        ])));
        assert_eq!(tray.alert, BatteryAlert::Normal);
        let tooltip = tray.tool_tip().description;
        assert_eq!(tooltip.lines().count(), 2);
        assert!(tooltip.contains("Mouse &lt;&amp;&gt; — Battery unavailable"));
        assert!(tooltip.contains("Keyboard — 42%"));
    }

    #[test]
    fn host_availability_is_forwarded_to_the_ui() {
        let (tx, rx) = mpsc::channel();
        let tray = LogiCenterTray::new(tx);
        assert!(tray.watcher_offline(ksni::OfflineReason::No));
        assert!(matches!(
            rx.try_recv().unwrap(),
            TrayCommand::Availability(false)
        ));
        tray.watcher_online();
        assert!(matches!(
            rx.try_recv().unwrap(),
            TrayCommand::Availability(true)
        ));
    }

    #[test]
    fn charging_unknown_or_high_percentage_does_not_raise_a_false_alarm() {
        let (tx, _) = mpsc::channel();
        let mut tray = LogiCenterTray::new(tx);
        let mut mouse = device("MX Master 2S", 1, None, true);
        mouse.battery_status = Some(1);
        for level in [None, Some(90)] {
            mouse.battery = level;
            tray.set_devices(connected_devices(&receivers(vec![mouse.clone()])));
            assert_eq!(tray.alert, BatteryAlert::Normal);
            assert_eq!(tray.icon_pixmap()[0].data, tray.icons[0][0].data);
            assert!(tray.devices[0].label.ends_with(" · Charging"));
            assert!(!tray.devices[0].label.contains("low battery"));
            if level.is_none() {
                assert!(tray.devices[0].label.contains("Percentage unavailable"));
                assert!(!tray.devices[0].label.contains("0%"));
                assert_eq!(mouse.battery_text(), "recharging · percentage unavailable");
            }
        }

        // Charging alone must not suppress a genuinely measured critical level.
        mouse.battery = Some(5);
        tray.set_devices(connected_devices(&receivers(vec![mouse.clone()])));
        assert_eq!(tray.alert, BatteryAlert::Critical);

        mouse.battery = None;
        mouse.battery_status = Some(3);
        tray.set_devices(connected_devices(&receivers(vec![mouse])));
        assert_eq!(tray.alert, BatteryAlert::Normal);
        assert!(tray.devices[0].label.ends_with(" · Fully charged"));
    }

    #[test]
    fn profile_submenus_select_a_named_profile_for_the_correct_device() {
        let (tx, rx) = mpsc::channel();
        let mut tray = LogiCenterTray::new(tx);
        tray.profiles = vec![ProfileGroup {
            device_key: "receiver:2".into(),
            device_name: "Work_Mouse".into(),
            names: vec![("Default".into(), true), ("Editor_Profile".into(), false)],
        }];
        let menu = tray.menu();
        let profiles = menu
            .iter()
            .find_map(|m| {
                if let MenuItem::SubMenu(s) = m {
                    Some(s)
                } else {
                    None
                }
            })
            .unwrap();
        let MenuItem::SubMenu(device) = &profiles.submenu[0] else {
            panic!("device submenu missing")
        };
        assert_eq!(device.label, "Work__Mouse");
        let MenuItem::Standard(profile) = &device.submenu[1] else {
            panic!("profile row missing")
        };
        assert_eq!(profile.label, "Editor__Profile");
        (profile.activate)(&mut tray);
        assert!(
            matches!(rx.try_recv().unwrap(), TrayCommand::SelectProfile { device_key, name }
            if device_key == "receiver:2" && name == "Editor_Profile")
        );
    }
}
