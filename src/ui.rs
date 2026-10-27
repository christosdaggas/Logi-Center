use std::cell::{Cell, RefCell};
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adw::prelude::*;
use gtk4 as gtk;
use gtk4::prelude::*;
use libadwaita as adw;
use log::{debug, warn};

use crate::backend::{self, PairingEvent, SettingsUpdate};
use crate::model::{DeviceConfig, DeviceSnapshot, ReceiverSnapshot, ScanReport};
use crate::tray::{self, TrayCommand};

const SIDEBAR_EXPANDED_WIDTH: i32 = 292;
const SIDEBAR_COLLAPSED_WIDTH: i32 = 80;
const DIAGNOSTICS_ICON: &str = "battery-good-symbolic";

const CSS: &str = include_str!("../assets/ui.css");

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemePreference {
    fn apply(self) {
        let style_manager = adw::StyleManager::default();
        style_manager.set_color_scheme(match self {
            Self::System => adw::ColorScheme::Default,
            Self::Light => adw::ColorScheme::ForceLight,
            Self::Dark => adw::ColorScheme::ForceDark,
        });
    }

    fn load() -> Self {
        let Some(path) = theme_path() else {
            return Self::default();
        };
        match fs::read_to_string(path).unwrap_or_default().trim() {
            "light" => Self::Light,
            "dark" => Self::Dark,
            _ => Self::System,
        }
    }

    fn save(self) {
        let Some(path) = theme_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                warn!("could not create theme config directory: {error}");
                return;
            }
        }
        let value = match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        };
        if let Err(error) = fs::write(path, value) {
            warn!("could not save theme preference: {error}");
        }
    }
}

fn theme_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(base) => PathBuf::from(base),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("logicenter").join("theme"))
}

struct PairingUi {
    dialog: adw::MessageDialog,
    spinner: gtk::Spinner,
    status: gtk::Label,
}

struct AppState {
    live: bool,
    app: adw::Application,
    background_hold: RefCell<Option<gtk::gio::ApplicationHoldGuard>>,
    startup_waiting: Cell<bool>,
    window: adw::ApplicationWindow,
    footer: gtk::Label,
    footer_updated: gtk::Label,
    sidebar_column: gtk::Box,
    sidebar_header: gtk::Box,
    sidebar_brand: gtk::Box,
    sidebar_brand_copy: gtk::Box,
    sidebar_toggle: gtk::Button,
    sidebar_expanded: gtk::Box,
    sidebar_rail_scroll: gtk::ScrolledWindow,
    sidebar_list: gtk::ListBox,
    sidebar_receiver_card: gtk::Box,
    sidebar_receiver_title: gtk::Label,
    sidebar_receiver_meta: gtk::Label,
    sidebar_receiver_bar: gtk::LevelBar,
    sidebar_rail: gtk::Box,
    sidebar_collapsed: Cell<bool>,
    device_rows: RefCell<Vec<(String, gtk::ListBoxRow)>>,
    rail_buttons: RefCell<Vec<(String, gtk::Button)>>,
    detail: RefCell<Option<DetailPane>>,
    detail_host: gtk::Box,
    toast: adw::ToastOverlay,
    selected_key: RefCell<Option<String>>,
    suppress_selection: Cell<bool>,
    receivers: RefCell<Vec<ReceiverSnapshot>>,
    cameras: RefCell<crate::peripherals::Inventory>,
    scan_tx: Sender<(ScanReport, crate::peripherals::Inventory)>,
    scan_rx: Receiver<(ScanReport, crate::peripherals::Inventory)>,
    scanning: Cell<bool>,
    restore_state: Arc<Mutex<crate::profiles::RestoreState>>,
    restore_reset: Arc<AtomicBool>,
    automation: RefCell<crate::automation::Supervisor>,
    automation_rx: Receiver<crate::automation::Event>,
    action_status: RefCell<std::collections::HashMap<String, String>>,
    applying: Cell<bool>,
    refresh_button: gtk::Button,
    pairing_active: Cell<bool>,
    pairing_rx: RefCell<Option<Receiver<PairingEvent>>>,
    pairing_cancel: RefCell<Option<Arc<AtomicBool>>>,
    pairing_ui: RefCell<Option<PairingUi>>,
    preferences_dialog: gtk::glib::WeakRef<adw::Dialog>,
    context_status: RefCell<String>,
    context_watcher: RefCell<Option<crate::runtime::ContextWatcher>>,
    context_rx: Receiver<crate::app_context::Detection>,
    foreground: RefCell<Option<String>>,
    applied_foreground: RefCell<Option<String>>,
    hotplug: RefCell<Option<crate::hotplug::Hotplug>>,
    hotplug_rx: Receiver<()>,
    hotplug_pending: Cell<bool>,
    battery: Arc<Mutex<Option<crate::battery::Monitor>>>,
    battery_tx: Sender<Vec<crate::battery::Alert>>,
    battery_rx: Receiver<Vec<crate::battery::Alert>>,
    tray_rx: Receiver<TrayCommand>,
    tray_handle: RefCell<Option<tray::TrayHandle>>,
    tray_active: Cell<bool>,
}

pub fn build(app: &adw::Application, start_hidden: bool) {
    if let Some(window) = app.windows().into_iter().next() {
        window.present();
        return;
    }
    build_window(app, start_hidden, true);
}

// The fixture path creates the actual window/actions without starting hardware,
// profile restore, background services, tray integration or update checks.
fn build_window(app: &adw::Application, start_hidden: bool, live: bool) -> Rc<AppState> {
    ThemePreference::load().apply();
    install_css();

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Logi Center")
        .default_width(1240)
        .default_height(800)
        .build();
    window.set_icon_name(Some(crate::APPLICATION_ID));
    let style_manager = adw::StyleManager::default();
    if !style_manager.is_dark() {
        window.add_css_class("light-labels");
    }
    let weak_window = window.downgrade();
    style_manager.connect_dark_notify(move |manager| {
        if let Some(window) = weak_window.upgrade() {
            if manager.is_dark() {
                window.remove_css_class("light-labels");
            } else {
                window.add_css_class("light-labels");
            }
        }
    });

    let title = adw::WindowTitle::new("Device workspace", "");
    let header = adw::HeaderBar::new();
    header.add_css_class("content-header");
    header.set_title_widget(Some(&title));

    let refresh_button = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh_button.set_tooltip_text(Some("Refresh devices"));
    refresh_button.add_css_class("flat");
    refresh_button.add_css_class("header-icon");

    let menu_button = gtk::MenuButton::new();
    menu_button.set_icon_name("open-menu-symbolic");
    menu_button.set_popover(Some(&build_menu_popover(app)));
    menu_button.set_tooltip_text(Some("Main menu"));
    menu_button.add_css_class("flat");
    menu_button.add_css_class("header-icon");
    header.pack_end(&menu_button);
    header.pack_end(&refresh_button);
    let control_center = gtk::Button::new();
    control_center.add_css_class("header-action");
    control_center.set_size_request(-1, 36);
    control_center.set_valign(gtk::Align::Center);
    let control_center_content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    control_center_content.append(&gtk::Image::from_icon_name("view-grid-symbolic"));
    control_center_content.append(&gtk::Label::new(Some("Control center")));
    control_center.set_child(Some(&control_center_content));
    control_center.set_action_name(Some("app.control-center"));
    control_center.set_tooltip_text(Some(
        "Profiles, workflows, receivers, battery and additional peripherals",
    ));
    control_center.set_margin_end(8);
    header.pack_end(&control_center);

    let sidebar_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar_column.add_css_class("sidebar-column");
    sidebar_column.set_hexpand(false);
    sidebar_column.set_hexpand_set(true);
    sidebar_column.set_size_request(SIDEBAR_EXPANDED_WIDTH, -1);

    let sidebar_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    sidebar_header.add_css_class("sidebar-header");
    sidebar_header.set_margin_start(12);
    sidebar_header.set_margin_end(8);

    let sidebar_brand = gtk::Box::new(gtk::Orientation::Horizontal, 11);
    // Embed the artwork so development and installed builds use the same logo.
    let logo_icon = gtk::gio::BytesIcon::new(&gtk::glib::Bytes::from_static(include_bytes!(
        "../docs/logo.svg"
    )));
    let logo = gtk::Image::from_gicon(&logo_icon);
    logo.set_pixel_size(30);
    logo.set_valign(gtk::Align::Center);
    let brand_copy = gtk::Box::new(gtk::Orientation::Vertical, 1);
    brand_copy.add_css_class("sidebar-brand-copy");
    brand_copy.set_valign(gtk::Align::Center);
    let brand_label = gtk::Label::new(Some("Logi Center"));
    brand_label.set_xalign(0.0);
    brand_label.add_css_class("sidebar-brand-name");
    let brand_caption = gtk::Label::new(Some("DEVICE MANAGER"));
    brand_caption.set_xalign(0.0);
    brand_caption.add_css_class("sidebar-brand-caption");
    brand_copy.append(&brand_label);
    brand_copy.append(&brand_caption);
    sidebar_brand.append(&logo);
    sidebar_brand.append(&brand_copy);
    sidebar_brand.set_hexpand(true);
    sidebar_brand.set_valign(gtk::Align::Center);

    let sidebar_toggle = gtk::Button::from_icon_name("sidebar-show-symbolic");
    sidebar_toggle.set_tooltip_text(Some("Toggle device list"));
    sidebar_toggle.add_css_class("flat");
    sidebar_toggle.set_halign(gtk::Align::Center);
    sidebar_toggle.set_valign(gtk::Align::Center);
    sidebar_header.append(&sidebar_brand);
    sidebar_header.append(&sidebar_toggle);
    sidebar_column.append(&sidebar_header);

    let sidebar_list = gtk::ListBox::new();
    sidebar_list.set_selection_mode(gtk::SelectionMode::Single);
    sidebar_list.add_css_class("navigation-sidebar");
    sidebar_list.add_css_class("sidebar-list");
    sidebar_list.set_margin_top(6);
    sidebar_list.set_margin_bottom(8);
    sidebar_list.set_margin_start(8);
    sidebar_list.set_margin_end(8);

    let list_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&sidebar_list)
        .build();
    list_scroll.set_vexpand(true);

    let sidebar_receiver_card = gtk::Box::new(gtk::Orientation::Vertical, 4);
    sidebar_receiver_card.add_css_class("receiver-footer-card");
    sidebar_receiver_card.set_margin_start(10);
    sidebar_receiver_card.set_margin_end(10);
    sidebar_receiver_card.set_margin_bottom(10);
    sidebar_receiver_card.set_margin_top(4);

    let receiver_top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let receiver_icon = gtk::Image::from_icon_name("network-wireless-symbolic");
    receiver_icon.set_pixel_size(14);
    receiver_icon.add_css_class("dim-label");
    let sidebar_receiver_title = gtk::Label::new(Some("Bolt Receiver"));
    sidebar_receiver_title.set_xalign(0.0);
    sidebar_receiver_title.set_hexpand(true);
    sidebar_receiver_title.add_css_class("caption");
    sidebar_receiver_title.add_css_class("sidebar-device-title");
    let receiver_state = gtk::Label::new(Some("Ready"));
    receiver_state.add_css_class("caption");
    receiver_state.add_css_class("success");
    receiver_top.append(&receiver_icon);
    receiver_top.append(&sidebar_receiver_title);
    receiver_top.append(&receiver_state);

    let sidebar_receiver_meta = gtk::Label::new(Some("/dev/hidraw2"));
    sidebar_receiver_meta.set_xalign(0.0);
    sidebar_receiver_meta.set_ellipsize(gtk::pango::EllipsizeMode::End);
    sidebar_receiver_meta.add_css_class("caption");
    sidebar_receiver_meta.add_css_class("dim-label");

    let sidebar_receiver_bar = gtk::LevelBar::new();
    sidebar_receiver_bar.set_min_value(0.0);
    sidebar_receiver_bar.set_max_value(6.0);
    sidebar_receiver_bar.set_value(2.0);
    sidebar_receiver_bar.set_margin_top(2);

    sidebar_receiver_card.append(&receiver_top);
    sidebar_receiver_card.append(&sidebar_receiver_meta);
    sidebar_receiver_card.append(&sidebar_receiver_bar);

    let expanded_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let navigation_caption = gtk::Label::new(Some("YOUR DEVICES"));
    navigation_caption.set_xalign(0.0);
    navigation_caption.add_css_class("sidebar-navigation-caption");
    expanded_box.append(&navigation_caption);
    expanded_box.append(&list_scroll);
    expanded_box.append(&sidebar_receiver_card);

    let sidebar_rail = gtk::Box::new(gtk::Orientation::Vertical, 8);
    sidebar_rail.set_halign(gtk::Align::Center);
    sidebar_rail.set_margin_top(10);
    sidebar_rail.set_margin_bottom(12);
    let rail_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&sidebar_rail)
        .build();
    rail_scroll.set_vexpand(true);

    let sidebar_body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar_body.set_vexpand(true);
    sidebar_body.append(&expanded_box);
    sidebar_body.append(&rail_scroll);
    rail_scroll.set_visible(false);
    sidebar_column.append(&sidebar_body);

    let detail = DetailPane::new();
    let detail_host = gtk::Box::new(gtk::Orientation::Vertical, 0);
    detail_host.set_vexpand(true);
    detail_host.append(&detail.root);
    let footer = gtk::Label::new(Some("Starting…"));
    footer.set_xalign(0.0);
    footer.add_css_class("caption");
    footer.add_css_class("dim-label");

    let footer_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    footer_box.add_css_class("app-footer");
    let footer_icon = gtk::Image::from_icon_name("network-wireless-symbolic");
    footer_icon.set_pixel_size(13);
    footer_icon.add_css_class("dim-label");
    footer_icon.set_margin_start(16);
    footer_box.append(&footer_icon);
    footer_box.append(&footer);

    let footer_updated = gtk::Label::new(Some("Starting…"));
    footer_updated.set_xalign(1.0);
    footer_updated.set_hexpand(true);
    footer_updated.add_css_class("caption");
    footer_updated.add_css_class("dim-label");
    footer_updated.set_margin_end(16);
    footer_box.append(&footer_updated);
    let update_banner = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    update_banner.set_visible(false);
    footer_box.append(&update_banner);
    let version = gtk::Label::new(Some(&format!("v{}", env!("CARGO_PKG_VERSION"))));
    version.add_css_class("caption");
    version.add_css_class("dim-label");
    version.set_margin_start(8);
    version.set_margin_end(16);
    footer_box.append(&version);

    let right_toolbar = adw::ToolbarView::new();
    right_toolbar.add_css_class("content-column");
    right_toolbar.add_top_bar(&header);
    right_toolbar.add_bottom_bar(&footer_box);
    right_toolbar.set_content(Some(&detail_host));
    right_toolbar.set_hexpand(true);
    right_toolbar.set_vexpand(true);

    let content_wrap = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    content_wrap.set_hexpand(true);
    content_wrap.set_hexpand_set(true);
    content_wrap.set_vexpand(true);
    content_wrap.append(&right_toolbar);

    let columns = gtk::Grid::new();
    columns.set_hexpand(true);
    columns.set_vexpand(true);
    columns.attach(&sidebar_column, 0, 0, 1, 1);
    columns.attach(&content_wrap, 1, 0, 1, 1);

    let overlay = adw::ToastOverlay::new();
    overlay.set_child(Some(&columns));
    window.set_content(Some(&overlay));

    let (scan_tx, scan_rx) = mpsc::channel();
    let (tray_tx, tray_rx) = mpsc::channel();
    let (automation, automation_rx) = crate::automation::Supervisor::new();
    let (context_watcher, context_rx) = if live {
        let (watcher, rx) = crate::runtime::ContextWatcher::start();
        (Some(watcher), rx)
    } else {
        (None, mpsc::channel().1)
    };
    let (hotplug, hotplug_rx) = match live.then(crate::hotplug::Hotplug::start) {
        Some(Ok((watcher, rx))) => (Some(watcher), rx),
        Some(Err(error)) => {
            warn!("Hotplug unavailable; periodic refresh remains enabled: {error}");
            (None, mpsc::channel().1)
        }
        None => (None, mpsc::channel().1),
    };
    let (battery_tx, battery_rx) = mpsc::channel();

    let state = Rc::new(AppState {
        live,
        app: app.clone(),
        background_hold: RefCell::new(start_hidden.then(|| app.hold())),
        startup_waiting: Cell::new(start_hidden),
        window: window.clone(),
        footer: footer.clone(),
        footer_updated: footer_updated.clone(),
        sidebar_column: sidebar_column.clone(),
        sidebar_header: sidebar_header.clone(),
        sidebar_brand: sidebar_brand.clone(),
        sidebar_brand_copy: brand_copy.clone(),
        sidebar_toggle: sidebar_toggle.clone(),
        sidebar_expanded: expanded_box.clone(),
        sidebar_rail_scroll: rail_scroll.clone(),
        sidebar_list,
        sidebar_receiver_card,
        sidebar_receiver_title,
        sidebar_receiver_meta,
        sidebar_receiver_bar,
        sidebar_rail,
        sidebar_collapsed: Cell::new(false),
        device_rows: RefCell::new(Vec::new()),
        rail_buttons: RefCell::new(Vec::new()),
        detail: RefCell::new(Some(detail)),
        detail_host,
        toast: overlay.clone(),
        selected_key: RefCell::new(None),
        suppress_selection: Cell::new(false),
        receivers: RefCell::new(Vec::new()),
        cameras: RefCell::new(crate::peripherals::Inventory::default()),
        scan_tx,
        scan_rx,
        scanning: Cell::new(false),
        restore_state: Arc::new(Mutex::new(crate::profiles::RestoreState::default())),
        restore_reset: Arc::new(AtomicBool::new(false)),
        automation: RefCell::new(automation),
        automation_rx,
        action_status: RefCell::new(std::collections::HashMap::new()),
        applying: Cell::new(false),
        refresh_button: refresh_button.clone(),
        pairing_active: Cell::new(false),
        pairing_rx: RefCell::new(None),
        pairing_cancel: RefCell::new(None),
        pairing_ui: RefCell::new(None),
        preferences_dialog: gtk::glib::WeakRef::new(),
        context_status: RefCell::new("Application detection is starting".into()),
        context_watcher: RefCell::new(context_watcher),
        context_rx,
        foreground: RefCell::new(None),
        applied_foreground: RefCell::new(None),
        hotplug: RefCell::new(hotplug),
        hotplug_rx,
        hotplug_pending: Cell::new(false),
        battery: Arc::new(Mutex::new(None)),
        battery_tx,
        battery_rx,
        tray_rx,
        tray_handle: RefCell::new(None),
        tray_active: Cell::new(false),
    });

    let weak = Rc::downgrade(&state);
    state
        .detail
        .borrow()
        .as_ref()
        .expect("detail pane")
        .set_camera_refresh(Rc::new(move || {
            if let Some(state) = weak.upgrade() {
                request_scan(&state);
            }
        }));

    let state_sidebar = state.clone();
    state
        .sidebar_toggle
        .connect_clicked(move |_| toggle_sidebar(&state_sidebar));

    let state_list = state.clone();
    state
        .sidebar_list
        .connect_row_selected(move |_list, row: Option<&gtk::ListBoxRow>| {
            if state_list.suppress_selection.get() {
                return;
            }
            let Some(row) = row else {
                return;
            };
            let key = state_list
                .device_rows
                .borrow()
                .iter()
                .find(|(_, candidate)| candidate == row)
                .map(|(key, _)| key.clone());
            let Some(key) = key else {
                return;
            };
            select_key(&state_list, &key);
        });

    let state_refresh = state.clone();
    refresh_button.connect_clicked(move |_| request_scan(&state_refresh));

    add_action(app, "apply", {
        let state_apply = state.clone();
        move || {
            if state_apply.scanning.get()
                || state_apply.applying.get()
                || state_apply.pairing_active.get()
            {
                show_toast(
                    &state_apply,
                    "Please wait for the current device operation to finish.",
                );
                return;
            }
            let detail = state_apply.detail.borrow();
            let Some(detail) = detail.as_ref() else {
                return;
            };
            let Some(device) = detail.selected.borrow().clone() else {
                return;
            };
            let update = detail.build_update(&device);
            let (remember, presets) = match detail.profile_options() {
                Ok(options) => options,
                Err(error) => {
                    show_toast(&state_apply, error.to_string());
                    return;
                }
            };
            if update_is_empty(&update) && !detail.profile_dirty() {
                show_toast(&state_apply, "No changes to apply");
                return;
            }
            apply_settings_async(&state_apply, device, update, remember, presets);
        }
    });
    state
        .detail
        .borrow()
        .as_ref()
        .unwrap()
        .apply_button
        .set_action_name(Some("app.apply"));
    state
        .detail
        .borrow()
        .as_ref()
        .unwrap()
        .custom_actions_button
        .set_action_name(Some("app.custom-actions"));
    add_action(app, "custom-actions", {
        let weak = Rc::downgrade(&state);
        move || {
            let Some(state) = weak.upgrade() else {
                return;
            };
            if state
                .detail
                .borrow()
                .as_ref()
                .is_some_and(DetailPane::has_pending_changes)
            {
                show_toast(
                    &state,
                    "Apply your pending settings before configuring custom actions.",
                );
                return;
            }
            let device = state
                .detail
                .borrow()
                .as_ref()
                .and_then(|detail| detail.selected.borrow().clone());
            if let Some(device) = device {
                let weak = Rc::downgrade(&state);
                let status = state
                    .action_status
                    .borrow()
                    .get(&device.key())
                    .cloned()
                    .unwrap_or_default();
                crate::actions_ui::show(&state.window, device, status, move || {
                    if let Some(state) = weak.upgrade() {
                        if let Some(detail) = state.detail.borrow().as_ref() {
                            detail.selected.replace(None);
                        }
                        state
                            .automation
                            .borrow_mut()
                            .reconcile(&state.receivers.borrow());
                        request_scan(&state);
                    }
                });
            }
        }
    });
    let shutdown_state = Rc::downgrade(&state);
    app.connect_shutdown(move |_| {
        if let Some(state) = shutdown_state.upgrade() {
            if let Some(detail) = state.detail.borrow().as_ref() {
                if let Some(preview) = detail.camera_preview.borrow().as_ref() {
                    preview.deactivate();
                }
            }
            if let Some(cancel) = state.pairing_cancel.borrow().as_ref() {
                cancel.store(true, Ordering::Release);
            }
            state.automation.borrow_mut().shutdown();
            if let Some(watcher) = state.context_watcher.borrow_mut().as_mut() {
                watcher.stop();
            }
            state.hotplug.borrow_mut().take();
            let battery = state.battery.clone();
            let _ = std::thread::spawn(move || {
                if let Some(monitor) = battery.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                    if let Err(error) = monitor.flush() {
                        warn!("Could not flush battery history: {error:#}");
                    }
                }
            })
            .join();
        }
    });

    let state_visible = state.clone();
    window.connect_visible_notify(move |window| {
        if window.is_visible() {
            state_visible.startup_waiting.set(false);
            state_visible.background_hold.borrow_mut().take();
            ensure_detail(&state_visible);
        }
    });

    let state_close = state.clone();
    window.connect_close_request(move |_| {
        if state_close.tray_active.get()
            && state_close
                .tray_handle
                .borrow()
                .as_ref()
                .is_some_and(|handle| !handle.is_closed())
        {
            state_close.window.set_visible(false);
            let can_release = state_close
                .detail
                .borrow()
                .as_ref()
                .is_none_or(|detail| !detail.has_pending_changes());
            // Never discard pending changes merely to save memory.
            if can_release {
                if let Some(detail) = state_close.detail.borrow_mut().take() {
                    state_close.detail_host.remove(&detail.root);
                }
            }
            // Return free allocator pages to Linux after GTK releases widgets.
            gtk::glib::idle_add_local_once(|| {
                #[cfg(target_env = "gnu")]
                unsafe {
                    libc::malloc_trim(0);
                }
            });
            gtk::glib::Propagation::Stop
        } else {
            gtk::glib::Propagation::Proceed
        }
    });

    add_action(app, "refresh", {
        let state = state.clone();
        move || request_scan(&state)
    });
    add_action(app, "about", {
        let state = state.clone();
        move || show_about(&state)
    });
    add_action(app, "preferences", {
        let state = state.clone();
        move || show_preferences(&state)
    });
    add_action(app, "pair", {
        let state = state.clone();
        move || pair_action(&state)
    });
    add_action(app, "profiles", {
        let weak = Rc::downgrade(&state);
        move || {
            let Some(state) = weak.upgrade() else { return };
            if operation_pending(&state) {
                return;
            }
            let Some(device) = selected_device(&state) else {
                show_toast(&state, "Select a device to manage its profiles");
                return;
            };
            let weak = Rc::downgrade(&state);
            crate::profile_ui::show(&state.window, device, move || {
                if let Some(state) = weak.upgrade() {
                    profiles_changed(&state);
                }
            });
        }
    });
    add_action(app, "services", {
        let weak = Rc::downgrade(&state);
        move || {
            if let Some(state) = weak.upgrade() {
                let refresh = Rc::downgrade(&state);
                crate::services_ui::show_with_saved(
                    &state.window,
                    ScanReport {
                        receivers: state.receivers.borrow().clone(),
                        errors: Vec::new(),
                    },
                    move |_| {
                        if let Some(state) = refresh.upgrade() {
                            state.hotplug_pending.set(true);
                        }
                    },
                );
            }
        }
    });
    add_action(app, "control-center", {
        let weak = Rc::downgrade(&state);
        move || {
            if let Some(state) = weak.upgrade() {
                let app = state.app.clone();
                control_center_dialog(
                    &state.window,
                    selected_device(&state),
                    state.context_status.borrow().clone(),
                    move |action| {
                        app.activate_action(action, None);
                    },
                );
            }
        }
    });
    add_action(app, "peripherals", {
        let weak = Rc::downgrade(&state);
        move || {
            if let Some(state) = weak.upgrade() {
                crate::peripherals_ui::show(&state.window);
            }
        }
    });
    add_action(app, "receivers", {
        let weak = Rc::downgrade(&state);
        move || {
            if let Some(state) = weak.upgrade() {
                if !operation_pending(&state) {
                    show_receiver_manager(&state);
                }
            }
        }
    });
    add_action(app, "switch-hosts", {
        let weak = Rc::downgrade(&state);
        move || {
            if let Some(state) = weak.upgrade() {
                if !operation_pending(&state) {
                    show_host_switcher(&state);
                }
            }
        }
    });
    add_action(app, "toggle-sidebar", {
        let state = state.clone();
        move || toggle_sidebar(&state)
    });
    add_action(app, "quit", {
        let app = app.clone();
        move || app.quit()
    });
    app.set_accels_for_action("app.refresh", &["<Control>r"]);
    app.set_accels_for_action("app.quit", &["<Control>q"]);
    app.set_accels_for_action("app.toggle-sidebar", &["<Control>b"]);
    add_action(app, "cancel-actions", {
        let weak = Rc::downgrade(&state);
        move || {
            if let Some(state) = weak.upgrade() {
                state.automation.borrow().cancel();
                show_toast(&state, "Running and queued actions cancelled");
            }
        }
    });
    app.set_accels_for_action("app.cancel-actions", &["<Control>period"]);

    if !live {
        return state;
    }

    let state_scan = state.clone();
    gtk::glib::timeout_add_local(Duration::from_millis(120), move || {
        while let Ok((report, cameras)) = state_scan.scan_rx.try_recv() {
            finish_scan(&state_scan, report, cameras);
            crate::i18n::translate_widget(state_scan.window.upcast_ref());
        }
        poll_pairing(&state_scan);
        while state_scan.hotplug_rx.try_recv().is_ok() {
            state_scan.hotplug_pending.set(true);
        }
        if state_scan.hotplug_pending.get()
            && !state_scan.scanning.get()
            && !state_scan.applying.get()
            && !state_scan.pairing_active.get()
        {
            state_scan.hotplug_pending.set(false);
            request_scan(&state_scan);
        }
        while let Ok(alerts) = state_scan.battery_rx.try_recv() {
            for alert in alerts {
                state_scan
                    .app
                    .send_notification(Some(&alert.notification_id()), &alert.notification());
            }
        }
        while let Ok(detection) = state_scan.context_rx.try_recv() {
            *state_scan.context_status.borrow_mut() =
                format!("{}: {}", detection.backend, detection.detail);
            state_scan
                .footer
                .set_tooltip_text(Some(&state_scan.context_status.borrow()));
            *state_scan.foreground.borrow_mut() = detection.app_id;
        }
        reconcile_application_profile(&state_scan);
        while let Ok(event) = state_scan.automation_rx.try_recv() {
            match event {
                crate::automation::Event::Status(key, message) => {
                    if state_scan.action_status.borrow().get(&key) != Some(&message) {
                        let previous = state_scan
                            .action_status
                            .borrow_mut()
                            .insert(key, message.clone());
                        // The first status for a device is startup/reconciliation
                        // information, even when a slow worker delivers it later.
                        if previous.is_some() && !message.starts_with("Custom actions active for ")
                        {
                            show_toast(&state_scan, message);
                        }
                    }
                }
                crate::automation::Event::DpiChanged => request_scan(&state_scan),
                crate::automation::Event::Ring(device, actions) => {
                    if state_scan.applying.get() || state_scan.pairing_active.get() {
                        continue;
                    }
                    state_scan.window.present();
                    let weak = Rc::downgrade(&state_scan);
                    crate::ring_ui::show(
                        &state_scan.window,
                        device,
                        actions,
                        move |device, action| {
                            if let Some(state) = weak.upgrade() {
                                let result = state.automation.borrow_mut().execute(device, action);
                                if let Err(error) = result {
                                    show_toast(&state, format!("Action not queued: {error:#}"));
                                }
                            }
                        },
                    );
                }
            }
        }
        gtk::glib::ControlFlow::Continue
    });

    let state_tray = state.clone();
    gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
        while let Ok(command) = state_tray.tray_rx.try_recv() {
            handle_tray_command(&state_tray, command);
        }
        gtk::glib::ControlFlow::Continue
    });

    let state_auto = state.clone();
    let retry_state = Rc::downgrade(&state);
    gtk::glib::timeout_add_seconds_local(1, move || {
        let Some(state) = retry_state.upgrade() else {
            return gtk::glib::ControlFlow::Break;
        };
        if !state.scanning.get() && !state.applying.get() && !state.pairing_active.get() {
            let due = state
                .restore_state
                .try_lock()
                .ok()
                .and_then(|restore| restore.next_retry_delay())
                .is_some_and(|delay| delay.is_zero());
            if due {
                request_scan(&state);
            }
        }
        gtk::glib::ControlFlow::Continue
    });
    gtk::glib::timeout_add_seconds_local(60, move || {
        if !state_auto.scanning.get() {
            request_scan(&state_auto);
        }
        gtk::glib::ControlFlow::Continue
    });

    let handle = tray::start(tray_tx);
    let can_start_hidden = start_hidden && handle.is_some();
    state.tray_active.set(handle.is_some());
    *state.tray_handle.borrow_mut() = handle;

    if !can_start_hidden {
        window.present();
    } else {
        if let Some(detail) = state.detail.borrow_mut().take() {
            state.detail_host.remove(&detail.root);
        }
        // An autostarted application must never remain hidden without a host.
        let weak_state = Rc::downgrade(&state);
        gtk::glib::timeout_add_seconds_local_once(5, move || {
            if let Some(state) = weak_state.upgrade() {
                if state.startup_waiting.replace(false) && !state.tray_active.get() {
                    state.window.present();
                }
            }
        });
    }
    crate::i18n::translate_widget(window.upcast_ref());
    request_scan(&state);
    watch_resume(&state);
    check_for_updates(&update_banner);
    debug!("Logi Center window created");
    state
}

#[allow(deprecated)] // The subscription must live for the application's lifetime, not a temporary guard.
fn watch_resume(state: &Rc<AppState>) {
    let weak = Rc::downgrade(state);
    gtk::gio::bus_get(
        gtk::gio::BusType::System,
        None::<&gtk::gio::Cancellable>,
        move |result| {
            let Ok(bus) = result else {
                return;
            };
            bus.signal_subscribe(
                Some("org.freedesktop.login1"),
                Some("org.freedesktop.login1.Manager"),
                Some("PrepareForSleep"),
                Some("/org/freedesktop/login1"),
                None,
                gtk::gio::DBusSignalFlags::NONE,
                move |_, _, _, _, _, parameters| {
                    if let Some(state) = weak.upgrade() {
                        state.automation.borrow().reset();
                        if parameters.get::<(bool,)>() == Some((false,)) {
                            state.restore_reset.store(true, Ordering::Release);
                            request_scan(&state);
                        }
                    }
                },
            );
        },
    );
}

fn check_for_updates(banner: &gtk::Box) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::version_check::check_for_update(env!(
            "CARGO_PKG_VERSION"
        )));
    });
    let weak_banner = banner.downgrade();
    gtk::glib::timeout_add_local(Duration::from_millis(250), move || {
        let Some(banner) = weak_banner.upgrade() else {
            return gtk::glib::ControlFlow::Break;
        };
        match rx.try_recv() {
            Ok(Some(update)) => {
                let icon = gtk::Image::from_icon_name("software-update-available-symbolic");
                icon.set_pixel_size(14);
                let label = crate::i18n::tr("v{version} available")
                    .replace("{version}", &update.latest_version);
                let link = gtk::LinkButton::with_label(&update.download_url, &label);
                banner.append(&icon);
                banner.append(&link);
                banner.set_visible(true);
                gtk::glib::ControlFlow::Break
            }
            Ok(None) | Err(mpsc::TryRecvError::Disconnected) => gtk::glib::ControlFlow::Break,
            Err(mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
        }
    });
}

fn ensure_detail(state: &Rc<AppState>) {
    if state.detail.borrow().is_some() {
        return;
    }
    let detail = DetailPane::new();
    detail.apply_button.set_action_name(Some("app.apply"));
    detail
        .custom_actions_button
        .set_action_name(Some("app.custom-actions"));
    let key = state.selected_key.borrow().clone();
    let device = state
        .receivers
        .borrow()
        .iter()
        .flat_map(|receiver| &receiver.devices)
        .find(|device| Some(device.key()) == key)
        .cloned();
    if let Some(device) = device {
        detail.show_device(&device);
    } else if let Some(camera) = state
        .cameras
        .borrow()
        .devices
        .iter()
        .find(|camera| Some(crate::peripherals::camera_key(camera)) == key)
    {
        detail.show_camera(camera, state.live);
    }
    state.detail_host.append(&detail.root);
    *state.detail.borrow_mut() = Some(detail);
    crate::i18n::translate_widget(state.window.upcast_ref());
}

fn selected_device(state: &AppState) -> Option<DeviceSnapshot> {
    let key = state.selected_key.borrow();
    state
        .receivers
        .borrow()
        .iter()
        .flat_map(|r| &r.devices)
        .find(|d| Some(d.key()) == *key)
        .cloned()
}

fn operation_pending(state: &Rc<AppState>) -> bool {
    if state.applying.get() || state.scanning.get() || state.pairing_active.get() {
        show_toast(
            state,
            "Please wait for the current device operation to finish.",
        );
        return true;
    }
    if state
        .detail
        .borrow()
        .as_ref()
        .is_some_and(DetailPane::has_pending_changes)
    {
        show_toast(
            state,
            "Apply your pending settings before changing profiles or connections.",
        );
        return true;
    }
    false
}

fn profiles_changed(state: &Rc<AppState>) {
    state.automation.borrow().reset();
    state
        .automation
        .borrow_mut()
        .reconcile(&state.receivers.borrow());
    if let Some(detail) = state.detail.borrow().as_ref() {
        detail.selected.replace(None);
    }
    state.restore_reset.store(true, Ordering::Release);
    refresh_tray_profiles(state);
    request_scan(state);
}

fn refresh_tray_profiles(state: &Rc<AppState>) {
    if state.tray_handle.borrow().is_none() {
        return;
    }
    let profiles = state
        .receivers
        .borrow()
        .iter()
        .flat_map(|r| &r.devices)
        .filter_map(|device| {
            crate::profiles::identity(device)?;
            let set = crate::profiles::profile_set(device).ok()?;
            let status = crate::profiles::selection_status(device).ok()?;
            Some(crate::tray::ProfileGroup {
                device_key: device.key(),
                device_name: device.name.clone(),
                names: set
                    .profiles
                    .keys()
                    .map(|name| (name.clone(), *name == status.effective))
                    .collect(),
            })
        })
        .collect();
    if let Some(tray) = state.tray_handle.borrow().as_ref() {
        tray.update_profiles(profiles);
    }
}

fn reconcile_application_profile(state: &Rc<AppState>) {
    if state.applying.get()
        || state.scanning.get()
        || state.pairing_active.get()
        || state
            .detail
            .borrow()
            .as_ref()
            .is_some_and(DetailPane::has_pending_changes)
        || *state.foreground.borrow() == *state.applied_foreground.borrow()
    {
        return;
    }
    let signatures = || -> Vec<_> {
        state
            .receivers
            .borrow()
            .iter()
            .flat_map(|r| &r.devices)
            .map(|d| serde_json::to_string(&crate::profiles::load(d)).unwrap_or_default())
            .collect()
    };
    let before = signatures();
    let application = state.foreground.borrow().clone();
    crate::profiles::set_foreground(application.clone());
    *state.applied_foreground.borrow_mut() = application;
    // Only a relevant profile transition causes device I/O. Ordinary focus
    // changes do not force full HID scans, and never overwrite pending edits.
    if before != signatures() {
        profiles_changed(state);
    }
}

fn apply_settings_async(
    state: &Rc<AppState>,
    device: DeviceSnapshot,
    update: SettingsUpdate,
    remember: bool,
    presets: Vec<u16>,
) {
    let profile_name = if crate::profiles::identity(&device).is_some() {
        match crate::profiles::selection_status(&device) {
            Ok(status) => Some(status.effective),
            Err(error) => {
                show_toast(state, format!("Could not load profile: {error:#}"));
                return;
            }
        }
    } else {
        None
    };
    state.applying.set(true);
    state.refresh_button.set_sensitive(false);
    if let Some(detail) = state.detail.borrow().as_ref() {
        detail.root.set_sensitive(false);
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = (|| -> anyhow::Result<_> {
            let outcome = backend::apply_verified(&device, &update)?;
            if let Some(name) = profile_name {
                if !outcome.has_failures() {
                    if let Some(snapshot) = outcome.snapshot.as_ref() {
                        crate::profiles::save_applied_to(snapshot, &name, &outcome.confirmed_update, remember, presets)?;
                    } else {
                        crate::profiles::save_confirmed_to(&device, &name, &outcome.confirmed_update, remember, presets)?;
                    }
                } else {
                    crate::profiles::save_confirmed_to(&device, &name, &outcome.confirmed_update, remember, presets)?;
                }
            }
            let details = outcome.results.iter().map(|r| format!("{}: {:?}{}", r.setting, r.state,
                r.error.as_ref().map(|e| format!(" — {e}")).unwrap_or_default())).collect::<Vec<_>>().join("\n");
            if outcome.has_failures() {
                Ok(format!("Partial Apply: only confirmed changes were saved. Hardware changes are not atomic and were not rolled back.\n{details}"))
            } else if outcome.results.iter().any(|r| r.state == backend::ApplyState::SentUnconfirmed) {
                Ok(format!("Settings verified; host switching was sent but cannot be confirmed after disconnect.\n{details}"))
            } else { Ok("Settings verified and profile saved".to_string()) }
        })()
        .map_err(|error| format!("{error:#}"));
        let _ = tx.send(result);
    });
    let weak_state = Rc::downgrade(state);
    gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
        let Some(state) = weak_state.upgrade() else {
            return gtk::glib::ControlFlow::Break;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return gtk::glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("The settings worker stopped unexpectedly.".to_string())
            }
        };
        state.applying.set(false);
        state.refresh_button.set_sensitive(true);
        if let Some(detail) = state.detail.borrow().as_ref() {
            detail.root.set_sensitive(true);
            detail.selected.replace(None);
        }
        match result {
            Ok(message) if message.len() > 200 => {
                let result = adw::AlertDialog::builder()
                    .heading("Apply results")
                    .body(message)
                    .build();
                result.add_response("close", "Close");
                result.set_close_response("close");
                result.present(Some(&state.window));
            }
            Ok(message) => show_toast(&state, message),
            Err(error) => show_toast(&state, format!("Could not apply settings: {error}")),
        }
        request_scan(&state);
        gtk::glib::ControlFlow::Break
    });
}

fn add_action<F>(app: &adw::Application, name: &str, callback: F)
where
    F: Fn() + 'static,
{
    let action = gtk::gio::SimpleAction::new(name, None);
    action.connect_activate(move |_, _| callback());
    app.add_action(&action);
}

fn install_css() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_data(CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn build_menu_popover(app: &adw::Application) -> gtk::Popover {
    fn theme_content(css_class: &str, selected: bool) -> gtk::Overlay {
        let overlay = gtk::Overlay::new();
        let swatch = gtk::Box::new(gtk::Orientation::Vertical, 0);
        swatch.set_size_request(44, 44);
        swatch.add_css_class("theme-selector");
        swatch.add_css_class(css_class);
        overlay.set_child(Some(&swatch));
        if selected {
            let check = gtk::Image::from_icon_name("object-select-symbolic");
            check.add_css_class("theme-check");
            check.set_halign(gtk::Align::Center);
            check.set_valign(gtk::Align::Center);
            overlay.add_overlay(&check);
        }
        overlay
    }

    fn refresh_theme_buttons(
        system: &gtk::ToggleButton,
        light: &gtk::ToggleButton,
        dark: &gtk::ToggleButton,
        selected: ThemePreference,
    ) {
        system.set_child(Some(&theme_content(
            "theme-default",
            selected == ThemePreference::System,
        )));
        light.set_child(Some(&theme_content(
            "theme-light",
            selected == ThemePreference::Light,
        )));
        dark.set_child(Some(&theme_content(
            "theme-dark",
            selected == ThemePreference::Dark,
        )));
    }

    fn add_row(
        container: &gtk::Box,
        app: &adw::Application,
        popover: &gtk::Popover,
        icon_name: &str,
        label: &str,
        action: &str,
    ) {
        let button = gtk::Button::new();
        button.add_css_class("flat");
        button.add_css_class("menu-row");
        button.set_halign(gtk::Align::Fill);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let icon = gtk::Image::from_icon_name(icon_name);
        icon.set_pixel_size(16);
        icon.add_css_class("dim-label");
        let text = gtk::Label::new(Some(label));
        text.set_xalign(0.0);
        text.set_hexpand(true);
        row.append(&icon);
        row.append(&text);
        button.set_child(Some(&row));

        let app = app.clone();
        let action = action.strip_prefix("app.").unwrap_or(action).to_string();
        let popover = popover.downgrade();
        button.connect_clicked(move |_| {
            // Let the popover release its input grab before opening a dialog.
            // Presenting the About window from inside the popover click can
            // leave it hidden or unfocused behind the main window.
            if let Some(popover) = popover.upgrade() {
                popover.popdown();
            }
            let app = app.clone();
            let action = action.clone();
            gtk::glib::idle_add_local_once(move || {
                app.activate_action(&action, None);
            });
        });
        container.append(&button);
    }

    let popover = gtk::Popover::new();
    popover.add_css_class("menu");

    let main_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    main_box.set_width_request(280);

    let theme_box = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    theme_box.set_halign(gtk::Align::Center);
    theme_box.set_margin_top(18);
    theme_box.set_margin_bottom(14);

    let system = gtk::ToggleButton::new();
    let light = gtk::ToggleButton::new();
    let dark = gtk::ToggleButton::new();
    light.set_group(Some(&system));
    dark.set_group(Some(&system));

    for (button, tooltip) in [
        (&system, "Follow the system theme"),
        (&light, "Light theme"),
        (&dark, "Dark theme"),
    ] {
        button.set_tooltip_text(Some(tooltip));
        button.add_css_class("flat");
        button.add_css_class("circular");
        button.add_css_class("theme-button");
    }

    let current = match adw::StyleManager::default().color_scheme() {
        adw::ColorScheme::ForceLight => ThemePreference::Light,
        adw::ColorScheme::ForceDark => ThemePreference::Dark,
        _ => ThemePreference::System,
    };
    match current {
        ThemePreference::System => system.set_active(true),
        ThemePreference::Light => light.set_active(true),
        ThemePreference::Dark => dark.set_active(true),
    }
    refresh_theme_buttons(&system, &light, &dark, current);

    for (button, preference) in [
        (system.clone(), ThemePreference::System),
        (light.clone(), ThemePreference::Light),
        (dark.clone(), ThemePreference::Dark),
    ] {
        let system = system.clone();
        let light = light.clone();
        let dark = dark.clone();
        button.connect_toggled(move |button| {
            if !button.is_active() {
                return;
            }
            preference.apply();
            preference.save();
            refresh_theme_buttons(&system, &light, &dark, preference);
        });
    }

    theme_box.append(&system);
    theme_box.append(&light);
    theme_box.append(&dark);
    main_box.append(&theme_box);

    let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator.set_margin_bottom(4);
    main_box.append(&separator);

    let items = gtk::Box::new(gtk::Orientation::Vertical, 0);
    items.set_margin_start(6);
    items.set_margin_end(6);
    items.set_margin_top(4);
    items.set_margin_bottom(6);
    add_row(
        &items,
        app,
        &popover,
        "emblem-system-symbolic",
        "Settings",
        "app.preferences",
    );
    add_row(
        &items,
        app,
        &popover,
        "document-properties-symbolic",
        "Device Profiles",
        "app.profiles",
    );
    add_row(
        &items,
        app,
        &popover,
        "camera-web-symbolic",
        "Gaming, Cameras, Lights and Audio",
        "app.peripherals",
    );
    add_row(
        &items,
        app,
        &popover,
        DIAGNOSTICS_ICON,
        "Diagnostics and Battery",
        "app.services",
    );
    add_row(
        &items,
        app,
        &popover,
        "view-refresh-symbolic",
        "Refresh Devices",
        "app.refresh",
    );
    add_row(
        &items,
        app,
        &popover,
        "list-add-symbolic",
        "Pair New Device",
        "app.pair",
    );
    add_row(
        &items,
        app,
        &popover,
        "network-wireless-symbolic",
        "Manage Receivers",
        "app.receivers",
    );
    add_row(
        &items,
        app,
        &popover,
        "computer-symbolic",
        "Switch Devices to Another Computer",
        "app.switch-hosts",
    );
    add_row(
        &items,
        app,
        &popover,
        "sidebar-show-symbolic",
        "Toggle Device List",
        "app.toggle-sidebar",
    );
    add_row(
        &items,
        app,
        &popover,
        "help-about-symbolic",
        "About Logi Center",
        "app.about",
    );
    add_row(
        &items,
        app,
        &popover,
        "process-stop-symbolic",
        "Cancel Running Actions",
        "app.cancel-actions",
    );
    add_row(
        &items,
        app,
        &popover,
        "application-exit-symbolic",
        "Quit",
        "app.quit",
    );
    main_box.append(&items);

    popover.set_child(Some(&main_box));
    popover.connect_show(|popover| crate::i18n::translate_widget(popover.upcast_ref()));
    popover
}

fn toggle_sidebar(state: &Rc<AppState>) {
    let collapsed = !state.sidebar_collapsed.get();
    state.sidebar_collapsed.set(collapsed);
    let width = if collapsed {
        SIDEBAR_COLLAPSED_WIDTH
    } else {
        SIDEBAR_EXPANDED_WIDTH
    };
    state.sidebar_column.set_size_request(width, -1);
    state.sidebar_expanded.set_visible(!collapsed);
    state.sidebar_rail_scroll.set_visible(collapsed);
    state.sidebar_brand_copy.set_visible(!collapsed);
    state.sidebar_brand.set_hexpand(!collapsed);
    state.sidebar_brand.set_halign(if collapsed {
        gtk::Align::Center
    } else {
        gtk::Align::Fill
    });
    state.sidebar_header.set_orientation(if collapsed {
        gtk::Orientation::Vertical
    } else {
        gtk::Orientation::Horizontal
    });
    state
        .sidebar_header
        .set_margin_start(if collapsed { 0 } else { 14 });
    state
        .sidebar_header
        .set_margin_end(if collapsed { 0 } else { 8 });
    state.sidebar_toggle.set_hexpand(false);
    state.sidebar_toggle.set_halign(gtk::Align::Center);
    state.sidebar_toggle.set_tooltip_text(Some(if collapsed {
        "Expand device list"
    } else {
        "Collapse device list"
    }));
}

fn request_scan(state: &Rc<AppState>) {
    if state.applying.get() {
        return;
    }
    if state.pairing_active.get() {
        if !state.scanning.get() {
            show_toast(state, "Finish pairing before refreshing devices");
        }
        return;
    }
    if state.scanning.replace(true) {
        return;
    }
    state.refresh_button.set_sensitive(false);
    state.footer.set_text("Scanning Logitech devices…");
    state.footer_updated.set_text("Refreshing…");
    let sender = state.scan_tx.clone();
    let restore = state.restore_state.clone();
    let reset = state.restore_reset.clone();
    let battery = state.battery.clone();
    let alerts = state.battery_tx.clone();
    std::thread::spawn(move || {
        let (report, cameras) = std::thread::scope(|scope| {
            let cameras = scope.spawn(crate::peripherals::cameras);
            let mut restore = restore.lock().unwrap_or_else(|error| error.into_inner());
            if reset.swap(false, Ordering::AcqRel) {
                restore.reset();
            }
            let report = restore.scan();
            drop(restore);
            {
                let mut battery = battery.lock().unwrap_or_else(|e| e.into_inner());
                let monitor = battery.get_or_insert_with(crate::battery::Monitor::load);
                let notifications = monitor.observe(&report.receivers);
                if !notifications.is_empty() {
                    let _ = alerts.send(notifications);
                }
            }
            let cameras = cameras
                .join()
                .unwrap_or_else(|_| crate::peripherals::Inventory {
                    devices: vec![],
                    backends: vec![crate::peripherals::BackendStatus {
                        backend: "v4l2-ctl".into(),
                        available: false,
                        detail: "Camera discovery worker failed; Refresh to retry.".into(),
                    }],
                });
            (report, cameras)
        });
        let _ = sender.send((report, cameras));
    });
}

fn finish_scan(state: &Rc<AppState>, report: ScanReport, cameras: crate::peripherals::Inventory) {
    state.scanning.set(false);
    state.refresh_button.set_sensitive(true);

    let errors = report.errors;
    for error in &errors {
        log::warn!("Device scan: {error}");
    }
    if let Some(tray) = state.tray_handle.borrow().as_ref() {
        tray.update_devices(&report.receivers);
    }
    *state.receivers.borrow_mut() = report.receivers;
    *state.cameras.borrow_mut() = cameras;
    refresh_tray_profiles(state);
    state
        .automation
        .borrow_mut()
        .reconcile(&state.receivers.borrow());
    if state.restore_reset.load(Ordering::Acquire) {
        request_scan(state);
    }
    rebuild_sidebar(state);

    let total_devices: usize = state
        .receivers
        .borrow()
        .iter()
        .map(|receiver| receiver.devices.len())
        .sum::<usize>()
        + state.cameras.borrow().devices.len();
    let online_devices: usize = state
        .receivers
        .borrow()
        .iter()
        .flat_map(|receiver| receiver.devices.iter())
        .filter(|device| device.online)
        .count()
        + state.cameras.borrow().devices.len();

    if total_devices == 0 {
        state.footer.set_text("No Logitech devices detected");
        state.footer_updated.set_text("");
    } else {
        state.footer.set_text(&format!(
            "{} of {} device{} online",
            online_devices,
            total_devices,
            if total_devices == 1 { "" } else { "s" }
        ));
        state.footer_updated.set_text("Updated just now");
    }

    let old_key = state.selected_key.borrow().clone();
    // Scans update batteries and the tray, not the user's unsubmitted edits.
    if state
        .detail
        .borrow()
        .as_ref()
        .is_some_and(DetailPane::has_pending_changes)
    {
        let row = state
            .device_rows
            .borrow()
            .iter()
            .find(|(key, _)| Some(key) == old_key.as_ref())
            .map(|(_, row)| row.clone());
        state.suppress_selection.set(true);
        state.sidebar_list.select_row(row.as_ref());
        state.suppress_selection.set(false);
        return;
    }
    let camera = state
        .cameras
        .borrow()
        .devices
        .iter()
        .find(|camera| Some(crate::peripherals::camera_key(camera)) == old_key)
        .cloned();
    if let Some(camera) = camera {
        select_camera(state, &camera);
        return;
    }
    let chosen = {
        let receivers = state.receivers.borrow();
        let by_key = old_key.and_then(|key| {
            receivers
                .iter()
                .flat_map(|receiver| receiver.devices.iter())
                .find(|device| device.key() == key)
                .cloned()
        });
        by_key
            .or_else(|| {
                receivers
                    .iter()
                    .flat_map(|receiver| receiver.devices.iter())
                    .find(|device| device.online)
                    .cloned()
            })
            .or_else(|| {
                receivers
                    .iter()
                    .flat_map(|receiver| receiver.devices.iter())
                    .next()
                    .cloned()
            })
    };

    if let Some(device) = chosen {
        select_device(state, &device);
    } else if let Some(camera) = state.cameras.borrow().devices.first().cloned() {
        select_camera(state, &camera);
    } else {
        let description = errors
            .first()
            .cloned()
            .or_else(|| {
                state
                    .cameras
                    .borrow()
                    .backends
                    .iter()
                    .find(|backend| !backend.available)
                    .map(|backend| backend.detail.clone())
            })
            .unwrap_or_else(|| {
                "Connect a Logitech receiver, camera or device and refresh.".to_string()
            });
        if let Some(detail) = state.detail.borrow().as_ref() {
            detail.show_empty("No Logitech device found", &description);
        }
        *state.selected_key.borrow_mut() = None;
    }
}

fn rebuild_sidebar(state: &Rc<AppState>) {
    while let Some(child) = state.sidebar_list.first_child() {
        state.sidebar_list.remove(&child);
    }
    while let Some(child) = state.sidebar_rail.first_child() {
        state.sidebar_rail.remove(&child);
    }
    state.device_rows.borrow_mut().clear();
    state.rail_buttons.borrow_mut().clear();

    let receivers = state.receivers.borrow();
    state
        .sidebar_receiver_card
        .set_visible(!receivers.is_empty());
    if receivers.is_empty() && state.cameras.borrow().devices.is_empty() {
        state.sidebar_receiver_card.set_visible(false);
        let empty = adw::StatusPage::builder()
            .icon_name("network-wireless-symbolic")
            .title("No devices")
            .description("Connect a Logitech receiver or camera.")
            .build();
        empty.set_vexpand(true);
        state.sidebar_list.append(&empty);

        let empty_icon = gtk::Image::from_icon_name("network-wireless-symbolic");
        empty_icon.set_pixel_size(22);
        empty_icon.set_tooltip_text(Some("No Logitech devices found"));
        state.sidebar_rail.append(&empty_icon);
        return;
    }

    if let Some(first) = receivers.first() {
        state.sidebar_receiver_card.set_visible(true);
        state.sidebar_receiver_title.set_text(&first.name);
        state.sidebar_receiver_meta.set_text(&format!(
            "046D:{:04X} · {} · {} paired",
            first.product_id,
            first.path,
            first.devices.len()
        ));
        let max_slots = first.max_devices.max(first.devices.len()).max(1) as f64;
        state.sidebar_receiver_bar.set_max_value(max_slots);
        state
            .sidebar_receiver_bar
            .set_value(first.devices.len() as f64);
    }

    for (receiver_index, receiver) in receivers.iter().enumerate() {
        if receiver_index > 0 {
            let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
            separator.set_margin_top(4);
            separator.set_margin_bottom(4);
            state.sidebar_rail.append(&separator);
        }

        let slot_text = if receiver.direct {
            "Direct".to_string()
        } else {
            format!("{}/{}", receiver.devices.len(), receiver.max_devices)
        };

        let section_row = gtk::ListBoxRow::new();
        section_row.set_selectable(false);
        section_row.set_activatable(false);
        section_row.add_css_class("sidebar-section-row");
        let section = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        section.set_margin_top(8);
        section.set_margin_bottom(4);
        section.set_margin_start(10);
        section.set_margin_end(10);
        let section_name = gtk::Label::new(Some(&receiver.name.to_uppercase()));
        section_name.set_xalign(0.0);
        section_name.set_hexpand(true);
        section_name.add_css_class("sidebar-section");
        let section_pill = gtk::Label::new(Some(&slot_text));
        section_pill.add_css_class("sidebar-slot-pill");
        section.append(&section_name);
        if !receiver.direct {
            let pair_button = gtk::Button::from_icon_name("list-add-symbolic");
            pair_button.add_css_class("flat");
            pair_button.add_css_class("sidebar-pair");
            pair_button.set_tooltip_text(Some("Pair a new device to this receiver"));
            let state_pair = state.clone();
            let receiver_path = receiver.path.clone();
            let receiver_name = receiver.name.clone();
            let product_id = receiver.product_id;
            pair_button.connect_clicked(move |_| {
                show_pairing_dialog(&state_pair, &receiver_path, product_id, &receiver_name);
            });
            section.append(&pair_button);
        }
        section.append(&section_pill);
        section_row.set_child(Some(&section));
        state.sidebar_list.append(&section_row);

        for device in &receiver.devices {
            let row = gtk::ListBoxRow::new();
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            content.set_margin_top(7);
            content.set_margin_bottom(7);
            content.set_margin_start(8);
            content.set_margin_end(8);

            let icon = gtk::Image::from_icon_name(device_icon_name(device));
            icon.set_pixel_size(18);
            icon.add_css_class("sidebar-device-icon");
            icon.set_valign(gtk::Align::Center);
            content.append(&icon);

            let text_col = gtk::Box::new(gtk::Orientation::Vertical, 1);
            text_col.set_valign(gtk::Align::Center);
            text_col.set_hexpand(true);

            let title = gtk::Label::new(Some(&device.name));
            title.set_xalign(0.0);
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            title.add_css_class("sidebar-device-title");
            if !device.online {
                title.add_css_class("dim-label");
            }
            let subtitle = gtk::Label::new(Some(&device.kind));
            subtitle.set_xalign(0.0);
            subtitle.add_css_class("caption");
            subtitle.add_css_class("dim-label");
            text_col.append(&title);
            text_col.append(&subtitle);
            content.append(&text_col);

            let badge = if device.online {
                if let Some(level) = device.battery {
                    let pill = gtk::Label::new(Some(&format!("{level}%")));
                    pill.add_css_class("status-pill");
                    pill.add_css_class(match battery_css_class(level) {
                        "success" => "good",
                        "warning" => "warn",
                        _ => "low",
                    });
                    pill
                } else {
                    let pill = gtk::Label::new(Some("Online"));
                    pill.add_css_class("status-pill");
                    pill.add_css_class("good");
                    pill
                }
            } else {
                let pill = gtk::Label::new(Some("Offline"));
                pill.add_css_class("status-pill");
                pill.add_css_class("offline");
                pill
            };
            badge.set_valign(gtk::Align::Center);
            content.append(&badge);

            row.set_child(Some(&content));
            row.set_tooltip_text(Some(&format!(
                "{} · {} · {}{}",
                device.name,
                device.kind,
                if device.serial.is_empty() {
                    "no serial".to_string()
                } else {
                    device.serial.clone()
                },
                device
                    .battery
                    .map(|level| format!(" · {level}%"))
                    .unwrap_or_default()
            )));
            state.sidebar_list.append(&row);
            state
                .device_rows
                .borrow_mut()
                .push((device.key(), row.clone()));

            let rail_button = gtk::Button::new();
            rail_button.add_css_class("flat");
            rail_button.add_css_class("rail-button");
            rail_button.set_halign(gtk::Align::Center);
            rail_button.set_valign(gtk::Align::Center);
            rail_button.set_tooltip_text(Some(&format!(
                "{} · {}{}",
                device.name,
                device.kind,
                device
                    .battery
                    .map(|level| format!(" · {level}%"))
                    .unwrap_or_else(|| {
                        if device.online {
                            " · Online".to_string()
                        } else {
                            " · Offline".to_string()
                        }
                    })
            )));
            let rail_icon = gtk::Image::from_icon_name(device_icon_name(device));
            rail_icon.set_pixel_size(22);
            rail_icon.set_halign(gtk::Align::Center);
            rail_icon.set_valign(gtk::Align::Center);
            if !device.online {
                rail_icon.add_css_class("dim-label");
            }
            rail_button.set_child(Some(&rail_icon));
            let state_click = state.clone();
            let rail_device = device.clone();
            rail_button.connect_clicked(move |_| {
                select_device(&state_click, &rail_device);
            });
            state.sidebar_rail.append(&rail_button);
            state
                .rail_buttons
                .borrow_mut()
                .push((device.key(), rail_button));
        }
    }
    let weak = Rc::downgrade(state);
    for (key, row, button) in append_camera_rows(
        &state.sidebar_list,
        &state.sidebar_rail,
        &state.cameras.borrow().devices,
        move |key| {
            if let Some(state) = weak.upgrade() {
                select_key(&state, key);
            }
        },
    ) {
        state.device_rows.borrow_mut().push((key.clone(), row));
        state.rail_buttons.borrow_mut().push((key, button));
    }
}

fn append_camera_rows(
    list: &gtk::ListBox,
    rail: &gtk::Box,
    cameras: &[crate::peripherals::Peripheral],
    select: impl Fn(&str) + 'static,
) -> Vec<(String, gtk::ListBoxRow, gtk::Button)> {
    if cameras.is_empty() {
        return vec![];
    }
    let section = gtk::ListBoxRow::new();
    section.set_selectable(false);
    section.set_activatable(false);
    section.add_css_class("sidebar-section-row");
    let title = gtk::Label::new(Some("CAMERAS"));
    title.add_css_class("sidebar-section");
    title.set_halign(gtk::Align::Start);
    title.set_margin_start(10);
    title.set_margin_top(12);
    section.set_child(Some(&title));
    list.append(&section);
    let select = Rc::new(select);
    cameras
        .iter()
        .map(|camera| {
            let key = crate::peripherals::camera_key(camera);
            let row = gtk::ListBoxRow::new();
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            content.set_margin_top(10);
            content.set_margin_bottom(10);
            content.set_margin_start(8);
            content.set_margin_end(8);
            let icon = gtk::Image::from_icon_name("camera-web-symbolic");
            icon.set_pixel_size(18);
            icon.add_css_class("sidebar-device-icon");
            content.append(&icon);
            let text = gtk::Box::new(gtk::Orientation::Vertical, 1);
            text.set_hexpand(true);
            let name = gtk::Label::new(Some(&camera.name));
            name.set_xalign(0.0);
            name.set_ellipsize(gtk::pango::EllipsizeMode::End);
            name.add_css_class("sidebar-device-title");
            text.append(&name);
            let subtitle = gtk::Label::new(Some(if camera.controls.is_empty() {
                "Camera · controls unavailable"
            } else {
                "Camera · USB"
            }));
            subtitle.set_xalign(0.0);
            subtitle.add_css_class("caption");
            subtitle.add_css_class("dim-label");
            text.append(&subtitle);
            content.append(&text);
            row.set_child(Some(&content));
            row.set_tooltip_text(Some(&format!("{} · {}", camera.name, camera.id)));
            list.append(&row);
            let button = gtk::Button::from_icon_name("camera-web-symbolic");
            button.add_css_class("flat");
            button.add_css_class("rail-button");
            button.set_tooltip_text(Some(&camera.name));
            let (select, target) = (select.clone(), key.clone());
            button.connect_clicked(move |_| select(&target));
            rail.append(&button);
            (key, row, button)
        })
        .collect()
}

fn select_key(state: &Rc<AppState>, key: &str) {
    let camera = state
        .cameras
        .borrow()
        .devices
        .iter()
        .find(|camera| crate::peripherals::camera_key(camera) == key)
        .cloned();
    if let Some(camera) = camera {
        select_camera(state, &camera);
        return;
    }
    let device = state
        .receivers
        .borrow()
        .iter()
        .flat_map(|r| &r.devices)
        .find(|device| device.key() == key)
        .cloned();
    if let Some(device) = device {
        select_device(state, &device);
    }
}

fn highlight_selection(state: &AppState, key: &str) {
    let row = state
        .device_rows
        .borrow()
        .iter()
        .find(|(candidate, _)| candidate == key)
        .map(|(_, row)| row.clone());
    state.suppress_selection.set(true);
    state.sidebar_list.select_row(row.as_ref());
    state.suppress_selection.set(false);
    for (candidate, button) in state.rail_buttons.borrow().iter() {
        if candidate == key {
            button.add_css_class("selected");
        } else {
            button.remove_css_class("selected");
        }
    }
}

fn select_camera(state: &Rc<AppState>, camera: &crate::peripherals::Peripheral) {
    let key = crate::peripherals::camera_key(camera);
    if state
        .detail
        .borrow()
        .as_ref()
        .is_some_and(DetailPane::has_pending_changes)
    {
        show_toast(
            state,
            "Apply your pending device settings before selecting a camera.",
        );
        if let Some(key) = state.selected_key.borrow().as_deref() {
            highlight_selection(state, key);
        }
        return;
    }
    ensure_detail(state);
    if let Some(detail) = state.detail.borrow().as_ref() {
        detail.show_camera(camera, state.live);
    }
    *state.selected_key.borrow_mut() = Some(key.clone());
    highlight_selection(state, &key);
    crate::i18n::translate_widget(state.window.upcast_ref());
}

fn show_selected_device(state: &Rc<AppState>, device: &DeviceSnapshot) {
    ensure_detail(state);
    let key = device.key();
    if let Some(detail) = state.detail.borrow().as_ref() {
        detail.show_device(device);
    }
    *state.selected_key.borrow_mut() = Some(key.clone());
    if let Some(receiver) = state
        .receivers
        .borrow()
        .iter()
        .find(|receiver| receiver.path == device.receiver_path)
    {
        state.sidebar_receiver_title.set_text(&receiver.name);
        state.sidebar_receiver_meta.set_text(&format!(
            "046D:{:04X} · {} · {} paired",
            receiver.product_id,
            receiver.path,
            receiver.devices.len()
        ));
        state
            .sidebar_receiver_bar
            .set_max_value(receiver.max_devices.max(receiver.devices.len()).max(1) as f64);
        state
            .sidebar_receiver_bar
            .set_value(receiver.devices.len() as f64);
    }
    for (rail_key, button) in state.rail_buttons.borrow().iter() {
        if *rail_key == key {
            button.add_css_class("selected");
        } else {
            button.remove_css_class("selected");
        }
    }
}

fn select_device(state: &Rc<AppState>, device: &DeviceSnapshot) {
    show_selected_device(state, device);
    let key = device.key();
    let target = state
        .device_rows
        .borrow()
        .iter()
        .find(|(row_key, _)| *row_key == key)
        .map(|(_, row)| row.clone());
    if let Some(row) = target {
        state.suppress_selection.set(true);
        state.sidebar_list.select_row(Some(&row));
        state.suppress_selection.set(false);
    }
}

fn handle_tray_command(state: &Rc<AppState>, command: TrayCommand) {
    match command {
        TrayCommand::Show => {
            state.window.set_visible(true);
            state.window.present();
        }
        TrayCommand::ShowDevice(key) => {
            let device = state
                .receivers
                .borrow()
                .iter()
                .flat_map(|receiver| &receiver.devices)
                .find(|device| device.key() == key)
                .cloned();
            if let Some(device) = device {
                select_device(state, &device);
            }
            state.window.present();
        }
        TrayCommand::Refresh => {
            state.window.set_visible(true);
            request_scan(state);
        }
        TrayCommand::About => {
            state.window.set_visible(true);
            state.window.present();
            show_about(state);
        }
        TrayCommand::Profiles => {
            state.window.present();
            state.app.activate_action("profiles", None);
        }
        TrayCommand::SelectProfile { device_key, name } => {
            if operation_pending(state) {
                state.window.present();
                return;
            }
            let device = state
                .receivers
                .borrow()
                .iter()
                .flat_map(|r| &r.devices)
                .find(|d| d.key() == device_key)
                .cloned();
            if let Some(device) = device {
                let result = crate::profiles::select_manual(&device, &name);
                match result {
                    Ok(()) => {
                        profiles_changed(state);
                        show_toast(
                            state,
                            format!(
                                "Profile {name} selected; automatic overrides paused until manual lock is released"
                            ),
                        );
                    }
                    Err(error) => {
                        state.window.present();
                        show_toast(state, error.to_string());
                    }
                }
            }
        }
        TrayCommand::Peripherals => {
            state.window.present();
            state.app.activate_action("peripherals", None);
        }
        TrayCommand::Diagnostics => {
            state.window.present();
            state.app.activate_action("services", None);
        }
        TrayCommand::Quit => {
            state.app.quit();
        }
        TrayCommand::Availability(available) => {
            state.tray_active.set(available);
            // GNOME extensions can be disabled while the window is hidden.
            // Keep the application reachable if its tray host disappears.
            if !available && !state.window.is_visible() && !state.startup_waiting.get() {
                state.window.present();
            }
        }
    }
}

fn pair_action(state: &Rc<AppState>) {
    let receiver = {
        let receivers = state.receivers.borrow();
        let selected_key = state.selected_key.borrow().clone();
        selected_key
            .and_then(|key| {
                receivers
                    .iter()
                    .find(|receiver| {
                        !receiver.direct
                            && receiver.capabilities.pairing
                            && receiver.devices.iter().any(|device| device.key() == key)
                    })
                    .cloned()
            })
            .or_else(|| {
                receivers
                    .iter()
                    .find(|receiver| !receiver.direct && receiver.capabilities.pairing)
                    .cloned()
            })
    };
    if let Some(receiver) = receiver {
        show_pairing_dialog(state, &receiver.path, receiver.product_id, &receiver.name);
    } else {
        show_toast(state, "No receiver is available for pairing");
    }
}

fn operation_dialog(title: &str) -> (adw::Dialog, adw::PreferencesPage) {
    let dialog = adw::Dialog::builder()
        .title(title)
        .content_width(620)
        .content_height(580)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let page = adw::PreferencesPage::new();
    toolbar.set_content(Some(&page));
    dialog.set_child(Some(&toolbar));
    (dialog, page)
}

fn control_center_dialog(
    parent: &adw::ApplicationWindow,
    device: Option<DeviceSnapshot>,
    context: String,
    activate: impl Fn(&str) + 'static,
) -> adw::Dialog {
    let (dialog, page) = operation_dialog("Control center");
    let overview = adw::PreferencesGroup::builder().title(format!("Logi Center {}", env!("CARGO_PKG_VERSION")))
        .description("Configure supported devices, application profiles and optional Linux integrations. Nothing is installed automatically.").build();
    let selected = adw::ActionRow::builder()
        .title("Selected device")
        .subtitle(
            device
                .as_ref()
                .map(|d| d.name.as_str())
                .unwrap_or("Choose a device in the main window for per-device controls"),
        )
        .build();
    selected.set_use_markup(false);
    overview.add(&selected);
    let backend = adw::ActionRow::builder()
        .title("Application detection")
        .subtitle(&context)
        .build();
    backend.set_use_markup(false);
    overview.add(&backend);
    page.add(&overview);
    let features = adw::PreferencesGroup::builder().title("Management tools")
        .description("Additional device-specific hardware controls are in the main window's Advanced tab. Unsupported controls stay hidden or unavailable.").build();
    let stable = device
        .as_ref()
        .is_some_and(|d| crate::profiles::identity(d).is_some());
    let actions = device
        .as_ref()
        .is_some_and(|d| d.online && stable && d.buttons.iter().any(|b| b.divertable));
    let activate: Rc<dyn Fn(&str)> = Rc::new(activate);
    for (title, subtitle, action, available) in [
        (
            "Device Profiles",
            "Named profiles, application rules, restore policy and import/export",
            "profiles",
            stable,
        ),
        (
            "Shortcuts and workflows",
            "Tap, hold, double-tap, gestures, templates and Actions Ring",
            "custom-actions",
            actions,
        ),
        (
            "Manage Receivers",
            "Receiver capacity, pairing and confirmed unpair operations",
            "receivers",
            true,
        ),
        (
            "Switch computers",
            "Coordinate hardware Easy-Switch channels; not network sharing",
            "switch-hosts",
            true,
        ),
        (
            "Diagnostics and Battery",
            "Permissions, backend status, notifications and optional history",
            "services",
            true,
        ),
        (
            "Gaming, Cameras, Lights and Audio",
            "Optional ratbag, V4L2, Litra, audio and Deskflow backends",
            "peripherals",
            true,
        ),
    ] {
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .build();
        let open = gtk::Button::with_label("Open");
        open.set_valign(gtk::Align::Center);
        open.set_sensitive(available);
        open.set_tooltip_text(Some(title));
        row.add_suffix(&open);
        row.set_activatable_widget(Some(&open));
        let weak = dialog.downgrade();
        let activate = activate.clone();
        open.connect_clicked(move |_| {
            if let Some(dialog) = weak.upgrade() {
                dialog.close();
            }
            let activate = activate.clone();
            glib::idle_add_local_once(move || activate(action));
        });
        features.add(&row);
    }
    page.add(&features);
    crate::i18n::translate_widget(dialog.upcast_ref());
    dialog.present(Some(parent));
    dialog
}

fn run_device_operation(
    state: &Rc<AppState>,
    work: impl FnOnce() -> anyhow::Result<String> + Send + 'static,
) {
    if operation_pending(state) {
        return;
    }
    state.applying.set(true);
    state.refresh_button.set_sensitive(false);
    state.automation.borrow().reset();
    if let Some(detail) = state.detail.borrow().as_ref() {
        detail.root.set_sensitive(false);
    }
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = tx.send(work().map_err(|e| format!("{e:#}")));
    });
    let weak = Rc::downgrade(state);
    glib::timeout_add_local(Duration::from_millis(100), move || {
        let Some(state) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("Device worker stopped unexpectedly".into())
            }
        };
        state.applying.set(false);
        state.refresh_button.set_sensitive(true);
        if let Some(detail) = state.detail.borrow().as_ref() {
            detail.root.set_sensitive(true);
            detail.selected.replace(None);
        }
        let message = match result {
            Ok(message) => message,
            Err(error) => error,
        };
        if message.len() > 240 || message.lines().count() > 3 {
            let result = adw::AlertDialog::builder()
                .heading("Device operation result")
                .body(&message)
                .build();
            result.add_response("close", "Close");
            result.set_close_response("close");
            result.present(Some(&state.window));
        } else {
            show_toast(&state, message);
        }
        // Writes can partially succeed, or intentionally disconnect a device.
        request_scan(&state);
        glib::ControlFlow::Break
    });
}

fn show_receiver_manager(state: &Rc<AppState>) {
    let (dialog, page) = operation_dialog("Receiver management");
    let receivers: Vec<_> = state
        .receivers
        .borrow()
        .iter()
        .filter(|r| !r.direct)
        .cloned()
        .collect();
    if receivers.is_empty() {
        page.add(&adw::PreferencesGroup::builder().title("No receiver detected")
            .description("Bluetooth and direct USB devices are not pairing receivers. Use your desktop's Bluetooth settings for initial Bluetooth pairing.").build());
    }
    for receiver in receivers {
        let group = adw::PreferencesGroup::builder()
            .title(&receiver.name)
            .description(format!(
                "{} · {} paired / {} slots · operations depend on receiver support",
                receiver.path,
                receiver.devices.len(),
                receiver.max_devices
            ))
            .build();
        let pair_row = adw::ActionRow::builder().title("Pair a device").build();
        let pair = gtk::Button::with_label("Pair");
        pair.set_valign(gtk::Align::Center);
        pair.set_sensitive(
            receiver.capabilities.pairing && receiver.devices.len() < receiver.max_devices,
        );
        pair_row.add_suffix(&pair);
        group.add(&pair_row);
        let weak = Rc::downgrade(state);
        let r = receiver.clone();
        let weak_dialog = dialog.downgrade();
        pair.connect_clicked(move |_| {
            if let Some(state) = weak.upgrade() {
                if let Some(dialog) = weak_dialog.upgrade() {
                    dialog.close();
                }
                show_pairing_dialog(&state, &r.path, r.product_id, &r.name);
            }
        });
        for device in &receiver.devices {
            let row = adw::ActionRow::builder()
                .title(&device.name)
                .subtitle(format!(
                    "Slot {} · {} · {}",
                    device.number,
                    if device.online { "online" } else { "offline" },
                    device.battery_text()
                ))
                .build();
            let remove = gtk::Button::with_label("Unpair");
            remove.set_sensitive(receiver.capabilities.unpairing);
            remove.add_css_class("destructive-action");
            remove.set_valign(gtk::Align::Center);
            row.add_suffix(&remove);
            group.add(&row);
            let weak = Rc::downgrade(state);
            let r = receiver.clone();
            let d = device.clone();
            let manager = dialog.downgrade();
            remove.connect_clicked(move |_| {
                let Some(state) = weak.upgrade() else { return };
                let confirmation = adw::AlertDialog::builder().heading(format!("Unpair {}?", d.name))
                    .body("The device will stop working through this receiver until you pair it again. This does not remove Bluetooth pairings. Unsupported receiver families will refuse this operation.").build();
                confirmation.add_response("cancel", "Cancel");
                confirmation.add_response("unpair", "Unpair");
                confirmation.set_close_response("cancel");
                confirmation.set_default_response(Some("cancel"));
                confirmation.set_response_appearance("unpair", adw::ResponseAppearance::Destructive);
                let r = r.clone();
                let d = d.clone();
                let weak = Rc::downgrade(&state);
                let manager = manager.clone();
                confirmation.connect_response(None, move |_, response| {
                    if response == "unpair" {
                        if let Some(state) = weak.upgrade() {
                            if let Some(manager) = manager.upgrade() { manager.close(); }
                            let r = r.clone(); let d = d.clone();
                            run_device_operation(&state, move || {
                                backend::unpair(&r, &d)?;
                                Ok(format!("{} unpaired", d.name))
                            });
                        }
                    }
                });
                confirmation.present(Some(&state.window));
            });
        }
        page.add(&group);
    }
    crate::i18n::translate_widget(dialog.upcast_ref());
    dialog.present(Some(&state.window));
}

fn show_host_switcher(state: &Rc<AppState>) {
    let (dialog, page) = operation_dialog("Switch devices to another computer");
    let devices: Vec<_> = state
        .receivers
        .borrow()
        .iter()
        .flat_map(|r| &r.devices)
        .filter(|d| {
            d.online && d.host_feature.is_some() && d.host.as_ref().is_some_and(|h| h.count > 1)
        })
        .cloned()
        .collect();
    let group = adw::PreferencesGroup::builder().title("Hardware Easy-Switch channels")
        .description("Choose devices and a channel already paired to another computer. Commands are sent sequentially, not atomically. This computer loses control of each switched device; network/clipboard sharing is separate.").build();
    let mut rows = Vec::new();
    for device in &devices {
        let host = device.host.as_ref().unwrap();
        let row = adw::SwitchRow::builder()
            .title(&device.name)
            .subtitle(format!(
                "Currently channel {} of {}",
                host.current + 1,
                host.count
            ))
            .active(true)
            .build();
        group.add(&row);
        rows.push(row);
    }
    let channel = adw::SpinRow::with_range(1.0, 3.0, 1.0);
    channel.set_title("Destination channel (1–3)");
    group.add(&channel);
    let apply = gtk::Button::with_label("Switch selected devices…");
    apply.set_sensitive(!devices.is_empty());
    apply.add_css_class("suggested-action");
    group.add(&apply);
    let error = gtk::Label::new(None);
    error.set_wrap(true);
    group.add(&error);
    page.add(&group);
    let weak = Rc::downgrade(state);
    let manager = dialog.downgrade();
    apply.connect_clicked(move |_| {
        let Some(state) = weak.upgrade() else { return };
        let chosen: Vec<_> = devices.iter().zip(&rows).filter(|(_, r)| r.is_active()).map(|(d, _)| d.clone()).collect();
        let target = channel.value() as u8 - 1;
        if chosen.is_empty() || chosen.iter().any(|d| d.host.as_ref().is_none_or(|h| target >= h.count)) {
            error.set_text("Select at least one device and a channel supported by every selected device.");
            return;
        }
        let confirm = adw::AlertDialog::builder().heading("Switch now?")
            .body("Make sure you can reach the physical Easy-Switch controls to return. A sent command cannot be acknowledged once its connection disappears.").build();
        confirm.add_response("cancel", "Cancel");
        confirm.add_response("switch", "Switch");
        confirm.set_close_response("cancel");
        confirm.set_default_response(Some("cancel"));
        let weak = Rc::downgrade(&state);
        let manager = manager.clone();
        confirm.connect_response(None, move |_, response| {
            if response == "switch" {
                if let Some(state) = weak.upgrade() {
                    if let Some(manager) = manager.upgrade() { manager.close(); }
                    let devices = chosen.clone();
                    run_device_operation(&state, move || {
                        let results = backend::switch_hosts(&devices, target)?;
                        Ok(serde_json::to_string_pretty(&results)?)
                    });
                }
            }
        });
        confirm.present(Some(&state.window));
    });
    crate::i18n::translate_widget(dialog.upcast_ref());
    dialog.present(Some(&state.window));
}

fn show_pairing_dialog(
    state: &Rc<AppState>,
    receiver_path: &str,
    product_id: u16,
    receiver_name: &str,
) {
    if !backend::receiver_capabilities(product_id, None).pairing {
        show_toast(
            state,
            "Pairing is not implemented for this receiver family.",
        );
        return;
    }
    if state.applying.get() || state.scanning.get() {
        show_toast(
            state,
            "Please wait for the current device operation to finish.",
        );
        return;
    }
    if state.pairing_active.replace(true) {
        show_toast(state, "A pairing operation is already in progress");
        return;
    }

    let instructions = if product_id == 0xC548 {
        "Put the Bolt device in pairing mode, usually by holding its Easy-Switch or pairing button until the LED blinks quickly.\n\nBolt receivers only work with Bolt-compatible devices. Discovery stops automatically after 30 seconds."
    } else {
        "Turn the device off and on, then hold its pairing button until the LED blinks quickly.\n\nThe device must be close to the receiver and must not be connected to another receiver. Pairing stops automatically after 30 seconds."
    };

    let dialog = adw::MessageDialog::new(
        Some(&state.window),
        Some(&format!("Pair a new device to {receiver_name}")),
        Some(instructions),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.set_close_response("cancel");

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    content.set_margin_top(6);
    let spinner = gtk::Spinner::new();
    spinner.start();
    spinner.set_valign(gtk::Align::Center);
    let status = gtk::Label::new(Some("Opening pairing mode…"));
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.set_hexpand(true);
    content.append(&spinner);
    content.append(&status);
    dialog.set_extra_child(Some(&content));

    let cancel = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::channel();
    *state.pairing_cancel.borrow_mut() = Some(cancel.clone());
    *state.pairing_rx.borrow_mut() = Some(receiver);
    *state.pairing_ui.borrow_mut() = Some(PairingUi {
        dialog: dialog.clone(),
        spinner,
        status,
    });

    backend::spawn_pairing(receiver_path.to_string(), product_id, cancel, sender);

    let state_response = state.clone();
    dialog.connect_response(None, move |dialog: &adw::MessageDialog, response| {
        if response != "done" {
            if let Some(cancel) = state_response.pairing_cancel.borrow().as_ref() {
                cancel.store(true, Ordering::Relaxed);
            }
            state_response.pairing_ui.borrow_mut().take();
        }
        dialog.close();
    });

    dialog.present();
    debug!(
        "pairing dialog presented visible={} mapped={}",
        dialog.is_visible(),
        dialog.is_mapped()
    );
}

fn poll_pairing(state: &Rc<AppState>) {
    let mut events = Vec::new();
    let mut disconnected = false;
    {
        let receiver = state.pairing_rx.borrow();
        if let Some(receiver) = receiver.as_ref() {
            loop {
                match receiver.try_recv() {
                    Ok(event) => events.push(event),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
    }

    for event in events {
        match event {
            PairingEvent::Status(text) => {
                if let Some(ui) = state.pairing_ui.borrow().as_ref() {
                    ui.status.set_text(&text);
                }
            }
            PairingEvent::Passkey(passkey) => {
                if let Some(ui) = state.pairing_ui.borrow().as_ref() {
                    ui.status
                        .set_text(&format!("Enter passcode {passkey} on the device now."));
                }
            }
            PairingEvent::Success { slot } => {
                state.pairing_active.set(false);
                state.pairing_cancel.borrow_mut().take();
                if let Some(ui) = state.pairing_ui.borrow().as_ref() {
                    ui.spinner.stop();
                    ui.spinner.set_visible(false);
                    ui.status.set_text("Pairing complete");
                    ui.dialog.set_body(&format!(
                        "A new device was paired successfully in slot {slot}."
                    ));
                    ui.dialog.add_response("done", "Done");
                    ui.dialog.set_default_response(Some("done"));
                    ui.dialog.set_close_response("done");
                    ui.dialog
                        .set_response_appearance("done", adw::ResponseAppearance::Suggested);
                }
                state.pairing_ui.borrow_mut().take();
                state.pairing_rx.borrow_mut().take();
                request_scan(state);
            }
            PairingEvent::Failed(message) => {
                state.pairing_active.set(false);
                state.pairing_cancel.borrow_mut().take();
                if let Some(ui) = state.pairing_ui.borrow().as_ref() {
                    ui.spinner.stop();
                    ui.spinner.set_visible(false);
                    ui.status.set_text("Pairing failed");
                    ui.dialog
                        .set_body(&format!("The receiver could not pair a device: {message}."));
                    ui.dialog.add_response("done", "Close");
                    ui.dialog.set_default_response(Some("done"));
                    ui.dialog.set_close_response("done");
                    ui.dialog
                        .set_response_appearance("done", adw::ResponseAppearance::Suggested);
                }
                state.pairing_ui.borrow_mut().take();
                state.pairing_rx.borrow_mut().take();
                if !message.to_ascii_lowercase().contains("cancelled") {
                    show_toast(state, format!("Pairing failed: {message}"));
                }
            }
        }
    }

    if disconnected {
        state.pairing_active.set(false);
        state.pairing_cancel.borrow_mut().take();
        state.pairing_rx.borrow_mut().take();
    }
}

fn show_preferences(state: &Rc<AppState>) {
    use crate::i18n::tr;
    use crate::preferences::{Language, Preferences, StartupMode};

    if let Some(dialog) = state.preferences_dialog.upgrade() {
        dialog.present(Some(&state.window));
        return;
    }
    let saved = Preferences::load();
    let dialog = adw::Dialog::builder()
        .title(tr("Settings"))
        .content_width(480)
        .content_height(420)
        .build();
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    toolbar.add_top_bar(&header);
    let page = adw::PreferencesPage::new();
    let language_group = adw::PreferencesGroup::builder()
        .title(tr("Language"))
        .description(tr(
            "Language changes take effect the next time you open the app.",
        ))
        .build();
    let names = [
        tr("Automatic (system language)"),
        "English".into(),
        "Ελληνικά".into(),
        "Deutsch".into(),
        "Español".into(),
        "Français".into(),
        "Italiano".into(),
        "Português".into(),
    ];
    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let language = adw::ComboRow::builder()
        .title(tr("App language"))
        .model(&gtk::StringList::new(&name_refs))
        .selected(
            Language::ALL
                .iter()
                .position(|value| *value == saved.language)
                .unwrap_or(0) as u32,
        )
        .build();
    language_group.add(&language);
    page.add(&language_group);

    let startup_group = adw::PreferencesGroup::builder()
        .title(tr("Startup"))
        .description(tr(
            "Without a tray extension, the app opens a window so you can still access it.",
        ))
        .build();
    let modes = [
        tr("Start manually"),
        tr("At login, in the tray"),
        tr("At login, with a window"),
    ];
    let mode_refs: Vec<&str> = modes.iter().map(String::as_str).collect();
    let startup = adw::ComboRow::builder()
        .title(tr("Launch behavior"))
        .model(&gtk::StringList::new(&mode_refs))
        .selected(
            StartupMode::ALL
                .iter()
                .position(|value| *value == saved.startup)
                .unwrap_or(0) as u32,
        )
        .build();
    startup_group.add(&startup);
    page.add(&startup_group);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.append(&page);
    let status = gtk::Label::new(None);
    status.set_wrap(true);
    status.add_css_class("error");
    status.set_margin_start(24);
    status.set_margin_end(24);
    status.set_visible(false);
    content.append(&status);
    let save = gtk::Button::with_label(&tr("Save settings"));
    save.add_css_class("suggested-action");
    save.add_css_class("pill");
    save.set_halign(gtk::Align::End);
    save.set_margin_end(24);
    save.set_margin_bottom(20);
    content.append(&save);
    toolbar.set_content(Some(&content));
    dialog.set_child(Some(&toolbar));

    let weak_dialog = dialog.downgrade();
    let weak_state = Rc::downgrade(state);
    save.connect_clicked(move |_| {
        let preferences = Preferences {
            language: Language::ALL
                .get(language.selected() as usize)
                .copied()
                .unwrap_or_default(),
            startup: StartupMode::ALL
                .get(startup.selected() as usize)
                .copied()
                .unwrap_or_default(),
        };
        match preferences.save() {
            Ok(()) => {
                if let Some(dialog) = weak_dialog.upgrade() {
                    dialog.close();
                }
                if let Some(state) = weak_state.upgrade() {
                    show_toast(
                        &state,
                        if preferences.language != saved.language {
                            "Settings saved. Restart the app to apply the new language."
                        } else {
                            "Settings saved"
                        },
                    );
                }
            }
            Err(error) => {
                status.set_text(&format!("{}: {error:#}", tr("Could not save settings")));
                status.set_visible(true);
            }
        }
    });
    state.preferences_dialog.set(Some(&dialog));
    dialog.present(Some(&state.window));
}

fn show_about(state: &Rc<AppState>) {
    let about = adw::AboutDialog::builder()
        .application_name("Logi Center")
        .application_icon(crate::APPLICATION_ID)
        .developer_name("Christos A. Daggas")
        .version(env!("CARGO_PKG_VERSION"))
        .comments("Manage your Logitech mice, keyboards and cameras. Made for Fedora with GNOME.")
        .website("https://chrisdaggas.com")
        .support_url("mailto:info@chrisdaggas.com")
        .developers(["Christos A. Daggas <info@chrisdaggas.com>"])
        .release_notes_version(env!("CARGO_PKG_VERSION"))
        .release_notes("<p>What's New in 1.0.0</p><ul><li>Rebuilt modern interface with responsive navigation, device cards and GNOME accent colors.</li><li>USB camera controls with live sliders and saved per-camera settings.</li><li>Optional full-width 16:9 camera preview, off by default, with no audio or recording.</li><li>Per-device and application-specific profiles with validated backups.</li><li>Device-aware DPI presets, custom shortcuts and directional gestures.</li><li>Receiver management, pairing and coordinated Easy-Switch host changes.</li><li>Battery indicators, notifications and low-battery tray alerts.</li><li>Language and login startup preferences with system light/dark themes.</li><li>GitHub update notifications and Fedora RPM installation.</li></ul>")
        .license_type(gtk::License::Gpl20)
        .build();
    about.present(Some(&state.window));
}

fn show_toast(state: &Rc<AppState>, message: impl Into<String>) {
    let toast = adw::Toast::new(&crate::i18n::tr(&message.into()));
    toast.set_use_markup(false);
    state.toast.add_toast(toast);
}

fn update_is_empty(update: &SettingsUpdate) -> bool {
    backend::settings_empty(update)
}

fn task_name(task: u16) -> String {
    let name = match task {
        56 => "Left Click",
        57 => "Right Click",
        58 => "Middle Click",
        60 => "Back",
        62 => "Forward",
        157 => "Smart Shift",
        180 => "Gesture Button",
        _ => return format!("Action {task}"),
    };
    name.to_string()
}

fn device_icon_name(device: &DeviceSnapshot) -> &'static str {
    let kind = device.kind.to_ascii_lowercase();
    if kind.contains("keyboard") {
        "input-keyboard-symbolic"
    } else if kind.contains("mouse") || kind.contains("trackball") {
        "input-mouse-symbolic"
    } else if kind.contains("gamepad") || kind.contains("joystick") {
        "input-gaming-symbolic"
    } else if kind.contains("headset") {
        "audio-headset-symbolic"
    } else {
        "input-gaming-symbolic"
    }
}

fn battery_icon_name(level: u8, status: Option<u8>) -> String {
    let rounded = ((level / 10) * 10).min(100);
    if matches!(status, Some(0x01 | 0x02 | 0x04)) {
        format!("battery-level-{rounded}-charging-symbolic")
    } else {
        format!("battery-level-{rounded}-symbolic")
    }
}

fn battery_css_class(level: u8) -> &'static str {
    if level <= 20 {
        "error"
    } else if level <= 40 {
        "warning"
    } else {
        "success"
    }
}

struct ActionRemapState {
    cid: u16,
    original: u16,
    row: adw::ActionRow,
    options: Rc<RefCell<Vec<gtk::ToggleButton>>>,
    selected: Rc<Cell<usize>>,
    target_ids: Vec<u16>,
}

impl ActionRemapState {
    fn select(&self, index: usize) {
        if let Some(option) = self.options.borrow().get(index) {
            option.set_active(true);
        }
    }

    fn selected(&self) -> usize {
        self.selected.get()
    }
}

struct DivertChipState {
    cid: u16,
    original: bool,
    button: gtk::ToggleButton,
}

struct DisableRowState {
    bit: u8,
    row: adw::SwitchRow,
}

struct SliderRow {
    row: adw::ActionRow,
    scale: gtk::Scale,
    allowed: Rc<RefCell<Vec<u16>>>,
}

impl SliderRow {
    fn new(title: &str, adjustment: &gtk::Adjustment) -> Self {
        let row = adw::ActionRow::builder().title(title).build();
        let scale = gtk::Scale::new(gtk::Orientation::Horizontal, Some(adjustment));
        scale.set_digits(0);
        scale.set_round_digits(0);
        scale.set_draw_value(true);
        scale.set_value_pos(gtk::PositionType::Top);
        scale.set_size_request(240, -1);
        scale.set_valign(gtk::Align::Center);
        let allowed = Rc::new(RefCell::new(Vec::<u16>::new()));
        let choices = allowed.clone();
        scale.connect_change_value(move |scale, scroll, value| {
            let choices = choices.borrow();
            let step_target = match scroll {
                gtk::ScrollType::StepForward
                | gtk::ScrollType::StepRight
                | gtk::ScrollType::StepUp => {
                    choices.iter().find(|v| f64::from(**v) > scale.value())
                }
                gtk::ScrollType::StepBackward
                | gtk::ScrollType::StepLeft
                | gtk::ScrollType::StepDown => choices
                    .iter()
                    .rev()
                    .find(|v| f64::from(**v) < scale.value()),
                _ => None,
            };
            if let Some(target) = step_target {
                scale.set_value(f64::from(*target));
                return gtk::glib::Propagation::Stop;
            }
            let adjustment = scale.adjustment();
            let step = adjustment.step_increment().max(1.0);
            let value = choices
                .iter()
                .min_by(|a, b| {
                    (f64::from(**a) - value)
                        .abs()
                        .total_cmp(&(f64::from(**b) - value).abs())
                })
                .map_or_else(
                    || adjustment.lower() + ((value - adjustment.lower()) / step).round() * step,
                    |value| f64::from(*value),
                );
            scale.set_value(value.clamp(adjustment.lower(), adjustment.upper()));
            gtk::glib::Propagation::Stop
        });
        scale.set_tooltip_text(Some(title));
        row.add_suffix(&scale);
        row.set_activatable_widget(Some(&scale));
        Self {
            row,
            scale,
            allowed,
        }
    }

    fn value(&self) -> f64 {
        self.scale.value()
    }

    fn set_value(&self, value: f64) {
        self.scale.set_value(value);
    }

    fn adjustment(&self) -> gtk::Adjustment {
        self.scale.adjustment()
    }
}

impl std::ops::Deref for SliderRow {
    type Target = adw::ActionRow;

    fn deref(&self) -> &Self::Target {
        &self.row
    }
}

struct DetailPane {
    camera_preview: RefCell<Option<crate::camera_preview::CameraPreview>>,
    camera_form: RefCell<Option<Rc<crate::peripherals_ui::CameraForm>>>,
    camera_refresh: RefCell<Option<Rc<dyn Fn()>>>,
    root: gtk::Stack,
    empty_page: adw::StatusPage,
    detail_scroll: gtk::ScrolledWindow,
    icon: gtk::Image,
    name: gtk::Label,
    status_badge: gtk::Label,
    battery_box: gtk::Box,
    battery_icon: gtk::Image,
    battery_bar: gtk::LevelBar,
    battery_label: gtk::Label,
    apply_button: gtk::Button,
    settings_stack: gtk::Stack,
    buttons_page: gtk::StackPage,
    advanced_page: gtk::StackPage,
    information: gtk::Expander,
    info_values: Vec<gtk::Label>,
    capability_details: gtk::Label,
    offline_notice: gtk::Label,
    host_group: adw::PreferencesGroup,
    protocol_group: adw::PreferencesGroup,
    hardware_group: adw::PreferencesGroup,
    thumbwheel_inverted_row: adw::SwitchRow,
    thumbwheel_diverted_row: adw::SwitchRow,
    report_rate_row: adw::ComboRow,
    report_rate_values: RefCell<Vec<u32>>,
    os_layout_row: adw::ComboRow,
    os_layout_values: RefCell<Vec<u8>>,
    settings_group: adw::PreferencesGroup,
    dpi_row: SliderRow,
    smooth_row: adw::SwitchRow,
    invert_row: adw::SwitchRow,
    resolution_row: adw::SwitchRow,
    scroll_diversion_row: adw::SwitchRow,
    ratchet_row: adw::SwitchRow,
    threshold_row: SliderRow,
    torque_row: SliderRow,
    host_row: adw::ComboRow,
    buttons_group: adw::PreferencesGroup,
    action_rows: RefCell<Vec<ActionRemapState>>,
    divert_group: adw::PreferencesGroup,
    divert_flow: gtk::FlowBox,
    divert_chips: RefCell<Vec<DivertChipState>>,
    keyboard_group: adw::PreferencesGroup,
    fn_swap_row: adw::SwitchRow,
    backlight_group: adw::PreferencesGroup,
    backlight_enabled_row: adw::SwitchRow,
    backlight_mode_row: adw::ComboRow,
    backlight_level_row: SliderRow,
    backlight_hands_out_row: SliderRow,
    backlight_hands_in_row: SliderRow,
    backlight_powered_row: SliderRow,
    disable_group: adw::PreferencesGroup,
    disable_rows: RefCell<Vec<DisableRowState>>,
    profile_section: gtk::Box,
    profile_grid: gtk::Grid,
    profile_cards: RefCell<Vec<gtk::Box>>,
    device_profile_group: adw::PreferencesGroup,
    remember_row: adw::SwitchRow,
    presets_row: adw::ActionRow,
    presets_entry: gtk::Entry,
    presets_flow: gtk::FlowBox,
    original_profile: RefCell<crate::profiles::DeviceProfile>,
    custom_group: adw::PreferencesGroup,
    custom_actions_button: gtk::Button,
    selected: RefCell<Option<DeviceSnapshot>>,
}

impl DetailPane {
    fn set_camera_refresh(&self, refresh: Rc<dyn Fn()>) {
        *self.camera_refresh.borrow_mut() = Some(refresh);
    }

    fn show_camera(&self, camera: &crate::peripherals::Peripheral, live: bool) {
        self.selected.replace(None);
        self.apply_button.set_visible(false);
        let unchanged = self
            .camera_preview
            .borrow()
            .as_ref()
            .is_some_and(|preview| preview.matches(camera));
        if !unchanged {
            if let Some(preview) = self.camera_preview.borrow_mut().take() {
                preview.deactivate();
            }
            if let Some(old) = self.root.child_by_name("camera") {
                self.root.remove(&old);
            }
            let preview = crate::camera_preview::CameraPreview::new(camera, live);
            let refresh = self
                .camera_refresh
                .borrow()
                .clone()
                .unwrap_or_else(|| Rc::new(|| {}));
            let (page, form) = crate::peripherals_ui::device_page(camera, &preview.root, refresh);
            self.root.add_named(&page, Some("camera"));
            *self.camera_form.borrow_mut() = Some(form);
            *self.camera_preview.borrow_mut() = Some(preview);
        }
        self.root.set_visible_child_name("camera");
        if let Some(preview) = self.camera_preview.borrow().as_ref() {
            preview.activate();
        }
    }

    fn has_pending_changes(&self) -> bool {
        if self.root.visible_child_name().as_deref() == Some("camera") {
            return self
                .camera_form
                .borrow()
                .as_ref()
                .is_some_and(|form| form.has_pending_changes());
        }
        self.selected.borrow().as_ref().is_some_and(|device| {
            !update_is_empty(&self.build_update(device)) || self.profile_dirty()
        })
    }

    fn profile_options(&self) -> anyhow::Result<(bool, Vec<u16>)> {
        let selected = self.selected.borrow();
        let Some(device) = selected.as_ref() else {
            return Ok((false, Vec::new()));
        };
        if crate::profiles::identity(device).is_none() {
            return Ok((false, Vec::new()));
        }
        let presets = if device.dpi_feature.is_some() {
            crate::dpi::presets(
                &self.presets_entry.text(),
                &crate::dpi::supported(&device.dpi_values, device.dpi),
            )?
        } else {
            Vec::new()
        };
        Ok((self.remember_row.is_active(), presets))
    }

    fn profile_dirty(&self) -> bool {
        let selected = self.selected.borrow();
        let Some(device) = selected.as_ref() else {
            return false;
        };
        if crate::profiles::identity(device).is_none() {
            return false;
        }
        let original = self.original_profile.borrow();
        match self.profile_options() {
            Ok((remember, presets)) => {
                remember != original.restore
                    || (device.dpi_feature.is_some()
                        && presets != crate::profiles::preset_values(&original, device))
            }
            Err(_) => true,
        }
    }

    fn new() -> Self {
        let empty_page = adw::StatusPage::builder()
            .icon_name("input-keyboard-symbolic")
            .title("Select a device")
            .description(
                "Choose a Logitech device from the sidebar to view its details and settings.",
            )
            .build();
        empty_page.set_vexpand(true);

        let icon = gtk::Image::from_icon_name("input-mouse-symbolic");
        icon.set_pixel_size(30);
        icon.add_css_class("device-icon");
        icon.add_css_class("device-summary-icon");
        icon.set_halign(gtk::Align::Center);
        icon.set_valign(gtk::Align::Center);

        let name = gtk::Label::new(None);
        name.set_xalign(0.0);
        name.set_wrap(true);
        name.add_css_class("device-name");

        let status_badge = gtk::Label::new(Some("Online"));
        status_badge.add_css_class("status-pill");
        status_badge.add_css_class("good");
        status_badge.set_valign(gtk::Align::Center);

        let title_row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        title_row.append(&name);
        title_row.append(&status_badge);

        let battery_icon = gtk::Image::from_icon_name("battery-missing-symbolic");
        battery_icon.set_pixel_size(16);
        battery_icon.add_css_class("dim-label");

        let battery_bar = gtk::LevelBar::new();
        battery_bar.set_min_value(0.0);
        battery_bar.set_max_value(100.0);
        battery_bar.set_value(0.0);
        battery_bar.set_size_request(100, -1);
        battery_bar.set_valign(gtk::Align::Center);

        let battery_label = gtk::Label::new(Some("Battery not available"));
        battery_label.set_xalign(0.0);
        battery_label.add_css_class("caption");
        battery_label.add_css_class("battery-label");

        let battery_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        battery_box.add_css_class("battery-box");
        battery_box.set_margin_top(3);
        battery_box.append(&battery_icon);
        battery_box.append(&battery_bar);
        battery_box.append(&battery_label);

        let heading_box = gtk::Box::new(gtk::Orientation::Vertical, 3);
        heading_box.set_valign(gtk::Align::Center);
        heading_box.set_hexpand(true);
        let eyebrow = gtk::Label::new(Some("DEVICE OVERVIEW"));
        eyebrow.set_xalign(0.0);
        eyebrow.add_css_class("device-eyebrow");
        heading_box.append(&eyebrow);
        heading_box.append(&title_row);
        heading_box.append(&battery_box);

        let apply_button = gtk::Button::new();
        let apply_content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let apply_icon = gtk::Image::from_icon_name("object-select-symbolic");
        apply_icon.set_pixel_size(16);
        apply_content.append(&apply_icon);
        apply_content.append(&gtk::Label::new(Some("Apply changes")));
        apply_button.set_child(Some(&apply_content));
        apply_button.set_tooltip_text(Some("Apply changes to this device"));
        apply_button.add_css_class("suggested-action");
        apply_button.add_css_class("device-apply");
        apply_button.set_valign(gtk::Align::Center);

        let hero = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        hero.add_css_class("device-summary");
        hero.append(&icon);
        hero.append(&heading_box);
        hero.append(&apply_button);

        let settings_group = adw::PreferencesGroup::builder()
            .title("Pointer and Scrolling")
            .build();

        let dpi_adjustment = gtk::Adjustment::new(1000.0, 100.0, 4000.0, 50.0, 500.0, 0.0);
        let dpi_row = SliderRow::new("Sensitivity (DPI)", &dpi_adjustment);

        let smooth_row = adw::SwitchRow::builder().title("Smooth scrolling").build();
        let invert_row = adw::SwitchRow::builder()
            .title("Invert scroll direction")
            .build();
        let resolution_row = adw::SwitchRow::builder()
            .title("High-resolution wheel")
            .build();
        let scroll_diversion_row = adw::SwitchRow::builder()
            .title("Divert wheel to HID++")
            .build();
        let ratchet_row = adw::SwitchRow::builder()
            .title("Ratcheted wheel mode")
            .build();

        let threshold_adjustment = gtk::Adjustment::new(1.0, 1.0, 50.0, 1.0, 5.0, 0.0);
        let threshold_row = SliderRow::new("SmartShift speed", &threshold_adjustment);

        let torque_adjustment = gtk::Adjustment::new(50.0, 1.0, 100.0, 1.0, 10.0, 0.0);
        let torque_row = SliderRow::new("SmartShift torque", &torque_adjustment);

        let host_row = adw::ComboRow::builder()
            .title("Active wireless host")
            .build();

        settings_group.add(&dpi_row.row);
        settings_group.add(&ratchet_row);
        settings_group.add(&threshold_row.row);
        settings_group.add(&torque_row.row);
        settings_group.add(&invert_row);
        settings_group.add(&resolution_row);
        settings_group.add(&smooth_row);
        let host_group = adw::PreferencesGroup::builder().title("Connection").build();
        host_group.add(&host_row);
        let protocol_group = adw::PreferencesGroup::builder()
            .title("Wheel Events")
            .build();
        protocol_group.add(&scroll_diversion_row);
        let hardware_group = adw::PreferencesGroup::builder().title("Additional hardware controls")
            .description("Only device-advertised controls are shown. Report rate is a register setting, not a measured frequency. Manual thumbwheel diversion is never restored automatically.").build();
        let thumbwheel_inverted_row = adw::SwitchRow::builder().title("Invert thumbwheel").build();
        let thumbwheel_diverted_row = adw::SwitchRow::builder()
            .title("Divert thumbwheel to HID++")
            .build();
        let report_rate_row = adw::ComboRow::builder().title("Report rate").build();
        let os_layout_row = adw::ComboRow::builder().title("Keyboard OS layout").build();
        hardware_group.add(&thumbwheel_inverted_row);
        hardware_group.add(&thumbwheel_diverted_row);
        hardware_group.add(&report_rate_row);
        hardware_group.add(&os_layout_row);

        let buttons_group = adw::PreferencesGroup::builder()
            .title("Button and Key Actions")
            .build();
        let divert_flow = gtk::FlowBox::new();
        divert_flow.set_selection_mode(gtk::SelectionMode::None);
        divert_flow.set_column_spacing(6);
        divert_flow.set_row_spacing(6);
        divert_flow.set_max_children_per_line(4);
        divert_flow.set_margin_top(8);
        divert_flow.set_margin_bottom(8);
        divert_flow.set_valign(gtk::Align::Center);

        let divert_group = adw::PreferencesGroup::builder()
            .title("HID++ Diversion")
            .description("Send HID++ events instead of normal button or key presses.")
            .build();
        divert_group.add(&divert_flow);

        let keyboard_group = adw::PreferencesGroup::builder().title("Keyboard").build();
        let fn_swap_row = adw::SwitchRow::builder()
            .title("Swap Fn keys")
            .subtitle("Use F1–F12 as media keys without holding Fn")
            .build();
        keyboard_group.add(&fn_swap_row);

        let backlight_group = adw::PreferencesGroup::builder().title("Backlight").build();
        let backlight_enabled_row = adw::SwitchRow::builder().title("Backlight enabled").build();
        let mode_model =
            gtk::StringList::new(&[&crate::i18n::tr("Automatic"), &crate::i18n::tr("Manual")]);
        let backlight_mode_row = adw::ComboRow::builder()
            .title("Backlight mode")
            .model(&mode_model)
            .build();
        let backlight_level_adjustment = gtk::Adjustment::new(0.0, 0.0, 20.0, 1.0, 2.0, 0.0);
        let backlight_level_row = SliderRow::new("Backlight level", &backlight_level_adjustment);
        let out_adjustment = gtk::Adjustment::new(30.0, 5.0, 600.0, 5.0, 30.0, 0.0);
        let backlight_hands_out_row = SliderRow::new("Delay — hands out (s)", &out_adjustment);
        let in_adjustment = gtk::Adjustment::new(30.0, 5.0, 600.0, 5.0, 30.0, 0.0);
        let backlight_hands_in_row = SliderRow::new("Delay — hands in (s)", &in_adjustment);
        let powered_adjustment = gtk::Adjustment::new(300.0, 5.0, 600.0, 5.0, 30.0, 0.0);
        let backlight_powered_row = SliderRow::new("Delay — powered (s)", &powered_adjustment);
        backlight_group.add(&backlight_enabled_row);
        backlight_group.add(&backlight_mode_row);
        backlight_group.add(&backlight_level_row.row);
        backlight_group.add(&backlight_hands_out_row.row);
        backlight_group.add(&backlight_hands_in_row.row);
        backlight_group.add(&backlight_powered_row.row);

        let disable_group = adw::PreferencesGroup::builder()
            .title("Disabled Keys")
            .build();
        let mut disable_rows = Vec::new();
        for (bit, name) in [
            (0x01u8, "Caps Lock"),
            (0x02, "Num Lock"),
            (0x04, "Scroll Lock"),
            (0x08, "Insert"),
            (0x10, "Super / Windows"),
        ] {
            let row = adw::SwitchRow::builder().title(name).build();
            disable_group.add(&row);
            disable_rows.push(DisableRowState { bit, row });
        }

        let profile_section = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let profile_heading = gtk::Label::new(Some("Saved Profile"));
        profile_heading.set_xalign(0.0);
        profile_heading.add_css_class("heading");
        let profile_grid = gtk::Grid::new();
        profile_grid.set_column_spacing(8);
        profile_grid.set_row_spacing(8);
        profile_grid.set_column_homogeneous(true);
        profile_grid.set_halign(gtk::Align::Fill);
        profile_grid.set_hexpand(true);
        profile_section.append(&profile_heading);
        profile_section.append(&profile_grid);

        let device_profile_group = adw::PreferencesGroup::builder()
            .title("Saved device settings")
            .description("Use Apply to save these options. Devices need a stable serial number.")
            .build();
        let remember_row = adw::SwitchRow::builder()
            .title("Remember this device")
            .subtitle("Restore saved settings after reconnecting or waking up")
            .build();
        let profiles_button = gtk::Button::new();
        profiles_button.add_css_class("flat");
        profiles_button.add_css_class("profile-manager-button");
        profiles_button.set_halign(gtk::Align::Fill);
        profiles_button.set_hexpand(true);
        let profiles_button_content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let profiles_button_label = gtk::Label::new(Some("Manage profiles and application rules"));
        profiles_button_label.set_xalign(0.0);
        profiles_button_label.set_hexpand(true);
        profiles_button_content.append(&profiles_button_label);
        profiles_button_content.append(&gtk::Image::from_icon_name("go-next-symbolic"));
        profiles_button.set_child(Some(&profiles_button_content));
        profiles_button.set_action_name(Some("app.profiles"));
        let presets_row = adw::ActionRow::builder().title("DPI presets").build();
        let presets_entry = gtk::Entry::new();
        presets_entry.set_width_chars(22);
        presets_entry.set_valign(gtk::Align::Center);
        presets_entry.set_tooltip_text(Some(
            "Enter 1 to 8 supported DPI values separated by commas",
        ));
        presets_row.add_suffix(&presets_entry);
        let presets_flow = gtk::FlowBox::new();
        presets_flow.set_selection_mode(gtk::SelectionMode::None);
        presets_flow.set_column_spacing(8);
        presets_flow.set_row_spacing(8);
        presets_flow.set_max_children_per_line(4);
        presets_flow.add_css_class("quick-presets");
        let profile_list = gtk::ListBox::new();
        profile_list.set_selection_mode(gtk::SelectionMode::None);
        profile_list.add_css_class("boxed-list");
        profile_list.append(&remember_row);
        profile_list.append(&presets_row);
        profile_list.append(&presets_flow);
        profile_list.append(&profiles_button);
        device_profile_group.add(&profile_list);
        let custom_group = adw::PreferencesGroup::builder()
            .title("Custom actions")
            .build();
        let custom_actions_button = gtk::Button::with_label("Configure shortcuts and gestures");
        custom_actions_button.set_halign(gtk::Align::Start);
        custom_group.add(&custom_actions_button);

        let information = gtk::Expander::new(Some("Device information"));
        information.add_css_class("device-information");
        let info_grid = gtk::Grid::new();
        info_grid.set_column_spacing(24);
        info_grid.set_row_spacing(10);
        info_grid.set_margin_top(14);
        let mut info_values = Vec::new();
        for (index, text) in [
            "Device type",
            "Receiver",
            "Slot",
            "Hardware ID",
            "Serial number",
            "Device path",
            "HID++ features",
            "Transport",
            "Firmware (read only)",
        ]
        .iter()
        .enumerate()
        {
            let label = gtk::Label::new(Some(text));
            label.set_xalign(0.0);
            label.set_valign(gtk::Align::Start);
            label.add_css_class("dim-label");
            let value = gtk::Label::new(None);
            value.set_xalign(0.0);
            value.set_hexpand(true);
            value.set_selectable(true);
            value.set_wrap(true);
            value.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            info_grid.attach(&label, 0, index as i32, 1, 1);
            info_grid.attach(&value, 1, index as i32, 1, 1);
            info_values.push(value);
        }
        let info_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
        info_box.append(&info_grid);
        let capability_expander = gtk::Expander::new(Some("Capabilities and read errors"));
        let capability_details = gtk::Label::new(None);
        capability_details.set_selectable(true);
        capability_details.set_wrap(true);
        capability_details.set_xalign(0.0);
        capability_expander.set_child(Some(&capability_details));
        info_box.append(&capability_expander);
        information.set_child(Some(&info_box));

        let offline_notice = gtk::Label::new(Some(
            "This device is offline. Connect it to change its settings.",
        ));
        offline_notice.set_xalign(0.0);
        offline_notice.set_wrap(true);
        offline_notice.add_css_class("offline-notice");

        let general = gtk::Box::new(gtk::Orientation::Vertical, 28);
        general.append(&offline_notice);
        general.append(&settings_group);
        general.append(&keyboard_group);
        general.append(&backlight_group);
        general.append(&host_group);
        general.append(&device_profile_group);
        general.append(&information);
        let buttons = gtk::Box::new(gtk::Orientation::Vertical, 28);
        buttons.append(&buttons_group);
        buttons.append(&disable_group);
        buttons.append(&custom_group);
        let advanced = gtk::Box::new(gtk::Orientation::Vertical, 28);
        advanced.append(&protocol_group);
        advanced.append(&hardware_group);
        advanced.append(&divert_group);
        advanced.append(&profile_section);

        for group in [
            &settings_group,
            &keyboard_group,
            &backlight_group,
            &host_group,
            &buttons_group,
            &disable_group,
            &protocol_group,
            &hardware_group,
            &divert_group,
            &device_profile_group,
            &custom_group,
        ] {
            group.set_title(&group.title().to_uppercase());
        }

        let settings_stack = gtk::Stack::new();
        settings_stack.add_css_class("device-settings");
        settings_stack.set_vhomogeneous(false);
        settings_stack.add_titled(&general, Some("general"), "General");
        let buttons_page = settings_stack.add_titled(&buttons, Some("buttons"), "Buttons & Keys");
        let advanced_page = settings_stack.add_titled(&advanced, Some("advanced"), "Advanced");
        let switcher = gtk::StackSwitcher::new();
        switcher.set_stack(Some(&settings_stack));
        switcher.set_halign(gtk::Align::Start);
        switcher.add_css_class("device-tabs");

        let heading = gtk::Box::new(gtk::Orientation::Vertical, 16);
        heading.set_margin_top(28);
        heading.set_margin_start(32);
        heading.set_margin_end(32);
        heading.set_margin_bottom(0);
        heading.append(&hero);
        heading.append(&switcher);
        let heading_clamp = adw::Clamp::builder()
            .maximum_size(920)
            .tightening_threshold(680)
            .child(&heading)
            .build();
        let heading_surface = gtk::Box::new(gtk::Orientation::Vertical, 0);
        heading_surface.add_css_class("device-heading");
        heading_surface.append(&heading_clamp);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_margin_top(24);
        content.set_margin_start(32);
        content.set_margin_end(32);
        content.set_margin_bottom(40);
        content.append(&settings_stack);
        let clamp = adw::Clamp::builder()
            .maximum_size(920)
            .tightening_threshold(680)
            .child(&content)
            .build();
        let detail_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .child(&clamp)
            .build();
        detail_scroll.set_vexpand(true);
        detail_scroll.set_propagate_natural_width(false);
        detail_scroll.set_can_focus(true);
        let adjustment = detail_scroll.vadjustment();
        settings_stack.connect_visible_child_notify(move |_| adjustment.set_value(0.0));

        let device_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        device_page.append(&heading_surface);
        device_page.append(&detail_scroll);

        let root = gtk::Stack::new();
        root.set_transition_type(gtk::StackTransitionType::Crossfade);
        root.set_transition_duration(140);
        root.add_named(&empty_page, Some("empty"));
        root.add_named(&device_page, Some("device"));
        root.set_visible_child_name("empty");

        let pane = Self {
            camera_preview: RefCell::new(None),
            camera_form: RefCell::new(None),
            camera_refresh: RefCell::new(None),
            root,
            empty_page,
            detail_scroll: detail_scroll.clone(),
            icon,
            name,
            status_badge,
            battery_box,
            battery_icon,
            battery_bar,
            battery_label,
            apply_button,
            settings_stack,
            buttons_page,
            advanced_page,
            information,
            info_values,
            capability_details,
            offline_notice,
            host_group,
            protocol_group,
            hardware_group,
            thumbwheel_inverted_row,
            thumbwheel_diverted_row,
            report_rate_row,
            report_rate_values: RefCell::new(Vec::new()),
            os_layout_row,
            os_layout_values: RefCell::new(Vec::new()),
            settings_group,
            dpi_row,
            smooth_row,
            invert_row,
            resolution_row,
            scroll_diversion_row,
            ratchet_row,
            threshold_row,
            torque_row,
            host_row,
            buttons_group,
            action_rows: RefCell::new(Vec::new()),
            divert_group,
            divert_flow,
            divert_chips: RefCell::new(Vec::new()),
            keyboard_group,
            fn_swap_row,
            backlight_group,
            backlight_enabled_row,
            backlight_mode_row,
            backlight_level_row,
            backlight_hands_out_row,
            backlight_hands_in_row,
            backlight_powered_row,
            disable_group,
            disable_rows: RefCell::new(disable_rows),
            profile_section,
            profile_grid,
            profile_cards: RefCell::new(Vec::new()),
            device_profile_group,
            remember_row,
            presets_row,
            presets_entry,
            presets_flow,
            original_profile: RefCell::new(crate::profiles::DeviceProfile::default()),
            custom_group,
            custom_actions_button,
            selected: RefCell::new(None),
        };
        pane.show_empty(
            "Select a device",
            "Choose a Logitech device from the sidebar to view its details and settings.",
        );
        pane
    }

    fn show_empty(&self, title: &str, description: &str) {
        if let Some(preview) = self.camera_preview.borrow().as_ref() {
            preview.deactivate();
        }
        self.selected.replace(None);
        self.apply_button.set_visible(false);
        self.empty_page.set_title(title);
        self.empty_page.set_description(Some(description));
        self.root.set_visible_child_name("empty");
    }

    fn show_device(&self, device: &DeviceSnapshot) {
        if let Some(preview) = self.camera_preview.borrow().as_ref() {
            preview.deactivate();
        }
        let is_new_selection = self
            .selected
            .borrow()
            .as_ref()
            .map(|selected| selected.key())
            .as_deref()
            != Some(device.key().as_str());
        self.selected.replace(Some(device.clone()));
        let profile = crate::profiles::load(device);
        let stable = crate::profiles::identity(device).is_some();
        self.remember_row.set_active(profile.restore);
        self.device_profile_group.set_sensitive(true);
        self.remember_row.set_sensitive(stable && device.online);
        self.presets_row.set_sensitive(stable && device.online);
        self.presets_flow.set_sensitive(stable && device.online);
        let values = crate::dpi::supported(&device.dpi_values, device.dpi);
        self.dpi_row
            .adjustment()
            .set_lower(f64::from(*values.first().unwrap_or(&100)));
        self.dpi_row
            .adjustment()
            .set_upper(f64::from(*values.last().unwrap_or(&4000)));
        self.dpi_row.adjustment().set_step_increment(1.0);
        *self.dpi_row.allowed.borrow_mut() = values;
        let presets = crate::profiles::preset_values(&profile, device);
        self.presets_entry.set_text(
            &presets
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        );
        self.presets_row.set_visible(device.dpi_feature.is_some());
        self.presets_flow.set_visible(device.dpi_feature.is_some());
        while let Some(child) = self.presets_flow.first_child() {
            self.presets_flow.remove(&child);
        }
        for dpi in presets {
            let button = gtk::Button::with_label(&format!("{dpi} DPI"));
            let scale = self.dpi_row.scale.clone();
            button.connect_clicked(move |_| scale.set_value(f64::from(dpi)));
            self.presets_flow.insert(&button, -1);
        }
        let custom =
            device.online && stable && device.buttons.iter().any(|button| button.divertable);
        self.custom_group.set_visible(custom);
        *self.original_profile.borrow_mut() = profile.clone();
        self.icon.set_icon_name(Some(device_icon_name(device)));
        self.name.set_text(&device.name);

        for class in ["good", "warn", "low", "offline"] {
            self.status_badge.remove_css_class(class);
        }
        if device.online {
            self.status_badge.set_text("Online");
            self.status_badge.add_css_class("good");
        } else {
            self.status_badge.set_text("Offline");
            self.status_badge.add_css_class("offline");
        }

        for (label, value) in self.info_values.iter().zip([
            device.kind.clone(),
            device.receiver_name.clone(),
            device.number.to_string(),
            device.wpid.clone(),
            device.serial.clone(),
            device.receiver_path.clone(),
            device.feature_count.to_string(),
            format!("{:?}", device.transport),
            if device.firmware.is_empty() {
                "Unavailable (not a firmware updater)".into()
            } else {
                device
                    .firmware
                    .iter()
                    .map(|f| format!("{} {} (layer {})", f.name, f.version, f.layer))
                    .collect::<Vec<_>>()
                    .join("\n")
            },
        ]) {
            label.set_text(if value.is_empty() { "—" } else { &value });
        }
        self.offline_notice.set_visible(!device.online);
        let mut capabilities = device
            .capabilities
            .iter()
            .map(|(name, c)| {
                format!(
                    "{name}: {:?}{}{}",
                    c.state,
                    if c.writable { " · writable" } else { "" },
                    c.detail
                        .as_ref()
                        .map(|d| format!(" · {d}"))
                        .unwrap_or_default()
                )
            })
            .collect::<Vec<_>>();
        capabilities.push(format!(
            "Feature IDs: {}",
            device
                .features
                .iter()
                .map(|f| format!("{:04X} v{}", f.id, f.version))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        capabilities.extend(device.errors.iter().cloned());
        self.capability_details.set_text(&capabilities.join("\n"));
        let thumbwheel = device
            .online
            .then_some(device.thumbwheel.as_ref())
            .flatten();
        self.thumbwheel_inverted_row
            .set_visible(thumbwheel.is_some());
        self.thumbwheel_diverted_row
            .set_visible(thumbwheel.is_some());
        if let Some(wheel) = thumbwheel {
            self.thumbwheel_inverted_row.set_active(wheel.inverted);
            self.thumbwheel_diverted_row.set_active(wheel.diverted);
        }
        let rates = device.report_rate.as_ref().filter(|_| device.online);
        let rate_values = rates
            .map(|r| {
                r.supported_us
                    .iter()
                    .copied()
                    .filter(|p| *p > 0)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let rate_names = rate_values
            .iter()
            .map(|p| format!("{:.0} Hz ({p} µs)", 1_000_000.0 / f64::from(*p)))
            .collect::<Vec<_>>();
        self.report_rate_row.set_model(Some(&gtk::StringList::new(
            &rate_names.iter().map(String::as_str).collect::<Vec<_>>(),
        )));
        self.report_rate_row.set_selected(
            rates
                .and_then(|r| rate_values.iter().position(|p| *p == r.current_us))
                .map(|i| i as u32)
                .unwrap_or(gtk::INVALID_LIST_POSITION),
        );
        self.report_rate_row.set_visible(!rate_values.is_empty());
        *self.report_rate_values.borrow_mut() = rate_values;
        let layout = device
            .os_layout
            .as_ref()
            .filter(|l| device.online && l.settable);
        let layout_values = layout
            .map(|l| l.platforms.iter().map(|p| p.id).collect::<Vec<_>>())
            .unwrap_or_default();
        let layout_names = layout
            .map(|l| {
                l.platforms
                    .iter()
                    .map(|p| format!("{} (ID {})", p.name, p.id))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.os_layout_row.set_model(Some(&gtk::StringList::new(
            &layout_names.iter().map(String::as_str).collect::<Vec<_>>(),
        )));
        self.os_layout_row.set_selected(
            layout
                .and_then(|l| layout_values.iter().position(|id| *id == l.current))
                .map(|i| i as u32)
                .unwrap_or(gtk::INVALID_LIST_POSITION),
        );
        self.os_layout_row.set_visible(!layout_values.is_empty());
        *self.os_layout_values.borrow_mut() = layout_values;
        self.hardware_group.set_visible(
            thumbwheel.is_some()
                || self.report_rate_row.is_visible()
                || self.os_layout_row.is_visible(),
        );

        self.battery_box.set_visible(true);
        self.battery_bar
            .set_value(device.battery.unwrap_or(0) as f64);
        self.battery_bar.set_visible(device.battery.is_some());
        for class in ["battery-good", "battery-warn", "battery-low"] {
            self.battery_bar.remove_css_class(class);
        }
        if let Some(level) = device.battery {
            self.battery_bar
                .add_css_class(match battery_css_class(level) {
                    "success" => "battery-good",
                    "warning" => "battery-warn",
                    _ => "battery-low",
                });
        }
        self.battery_icon.set_icon_name(Some(&match device.battery {
            Some(level) => battery_icon_name(level, device.battery_status),
            None if device.battery_is_charging() => "battery-good-charging-symbolic".to_string(),
            None if device.battery_status == Some(0x03) => {
                "battery-full-charged-symbolic".to_string()
            }
            None => "battery-missing-symbolic".to_string(),
        }));
        self.battery_label.set_text(&device.battery_text());
        for class in ["success", "warning", "error", "dim-label"] {
            self.battery_label.remove_css_class(class);
        }
        if let Some(level) = device.battery {
            self.battery_label.add_css_class(battery_css_class(level));
        } else {
            self.battery_label.add_css_class("dim-label");
        }

        let has_dpi = device.online && device.dpi_feature.is_some();
        let has_scroll = device.online && device.scroll_flags.is_some();
        let has_smooth = device.online && device.smooth_scroll.is_some();
        let has_smart_shift = device.online && device.smart_shift.is_some();
        let has_host = device
            .online
            .then_some(device.host.as_ref())
            .flatten()
            .is_some_and(|host| host.count > 1);
        let has_settings = has_dpi || has_scroll || has_smooth || has_smart_shift || has_host;
        self.settings_group
            .set_visible(has_dpi || has_scroll || has_smooth || has_smart_shift);
        self.host_group.set_visible(has_host);
        self.protocol_group.set_visible(has_scroll);
        self.dpi_row.set_visible(has_dpi);
        self.invert_row.set_visible(has_scroll);
        self.resolution_row.set_visible(has_scroll);
        self.scroll_diversion_row.set_visible(has_scroll);
        self.smooth_row.set_visible(has_smooth);
        self.ratchet_row.set_visible(has_smart_shift);
        self.threshold_row.set_visible(has_smart_shift);
        self.torque_row.set_visible(
            has_smart_shift
                && device
                    .smart_shift
                    .as_ref()
                    .and_then(|setting| setting.torque)
                    .is_some(),
        );
        self.host_row.set_visible(has_host);

        if has_dpi {
            self.dpi_row.set_value(device.dpi.unwrap_or(1000) as f64);
        }
        if has_scroll {
            self.invert_row
                .set_active(device.scroll_inverted().unwrap_or(false));
            self.resolution_row
                .set_active(device.scroll_high_resolution().unwrap_or(false));
            self.scroll_diversion_row
                .set_active(device.scroll_diversion().unwrap_or(false));
        }
        if let Some(smooth) = device.smooth_scroll {
            self.smooth_row.set_active(smooth);
        }
        if has_smart_shift {
            self.ratchet_row
                .set_active(device.is_ratcheted().unwrap_or(false));
            if let Some(setting) = &device.smart_shift {
                self.threshold_row.set_value(setting.threshold as f64);
                if let Some(torque) = setting.torque {
                    self.torque_row.set_value(torque as f64);
                }
            }
        }
        if let Some(host) = &device.host {
            if host.count > 1 {
                let names: Vec<String> = host
                    .names
                    .iter()
                    .enumerate()
                    .map(|(index, name)| format!("{}: {name}", index + 1))
                    .collect();
                let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
                let model = gtk::StringList::new(&name_refs);
                self.host_row.set_model(Some(&model));
                self.host_row
                    .set_selected(host.current.min(host.count - 1) as u32);
            }
        }

        let mut has_any = has_settings || self.hardware_group.is_visible();

        for state in self.action_rows.borrow().iter() {
            self.buttons_group.remove(&state.row);
        }
        self.action_rows.borrow_mut().clear();
        while let Some(child) = self.divert_flow.first_child() {
            self.divert_flow.remove(&child);
        }
        self.divert_chips.borrow_mut().clear();

        self.buttons_group.set_visible(false);
        self.divert_group.set_visible(false);
        if device.online && !device.buttons.is_empty() {
            for button in device.buttons.iter().filter(|button| button.remappable) {
                let mut target_ids = Vec::new();
                let mut target_names = Vec::new();
                for target in &device.buttons {
                    if (target.cid == button.cid
                        || target.cid == button.mapped_to
                        || (target.group <= 7 && button.group_mask & (1 << target.group) != 0))
                        && !target_ids.contains(&target.cid)
                    {
                        target_ids.push(target.cid);
                        target_names.push(task_name(target.task));
                    }
                }
                if !target_ids.contains(&button.mapped_to) {
                    target_ids.push(button.mapped_to);
                    target_names.push(format!("Current mapping ({})", button.mapped_to));
                }
                let selected_index = target_ids
                    .iter()
                    .position(|cid| *cid == button.mapped_to)
                    .unwrap_or(0);
                let row = adw::ActionRow::builder().title(button.name.clone()).build();
                row.set_sensitive(
                    !(profile.actions_enabled && profile.bindings.contains_key(&button.cid)),
                );
                let selector = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                selector.add_css_class("remap-selector");
                selector.set_homogeneous(true);
                selector.set_valign(gtk::Align::Center);
                let options = Rc::new(RefCell::new(Vec::new()));
                let selected = Rc::new(Cell::new(selected_index));
                for (index, target_name) in target_names.iter().enumerate() {
                    let option = gtk::ToggleButton::with_label(target_name);
                    option.add_css_class("remap-option");
                    option.set_hexpand(true);
                    option.set_tooltip_text(Some(target_name));
                    if let Some(first) = options.borrow().first() {
                        option.set_group(Some(first));
                    }
                    option.set_active(index == selected_index);
                    let selected = selected.clone();
                    option.connect_toggled(move |option| {
                        if option.is_active() {
                            selected.set(index);
                        }
                    });
                    selector.append(&option);
                    options.borrow_mut().push(option);
                }
                row.add_suffix(&selector);
                self.buttons_group.add(&row);
                self.action_rows.borrow_mut().push(ActionRemapState {
                    cid: button.cid,
                    original: button.mapped_to,
                    row,
                    options,
                    selected,
                    target_ids: target_ids.clone(),
                });
            }

            for button in device.buttons.iter().filter(|button| button.divertable) {
                let short_label = button
                    .name
                    .trim_end_matches(" Button")
                    .replace("Virtual ", "");
                let toggle = gtk::ToggleButton::new();
                let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                let icon = gtk::Image::from_icon_name(diversion_icon_name(&button.name));
                icon.set_pixel_size(16);
                let label = gtk::Label::new(Some(&short_label));
                label.set_xalign(0.0);
                label.set_hexpand(true);
                content.append(&icon);
                content.append(&label);
                toggle.set_child(Some(&content));
                toggle.add_css_class("divert-chip");
                toggle.set_active(button.diverted);
                toggle.set_sensitive(
                    !(profile.actions_enabled && profile.bindings.contains_key(&button.cid)),
                );
                toggle.set_tooltip_text(Some(&format!(
                    "Divert {} to send HID++ events",
                    button.name
                )));
                self.divert_flow.insert(&toggle, -1);
                self.divert_chips.borrow_mut().push(DivertChipState {
                    cid: button.cid,
                    original: button.diverted,
                    button: toggle,
                });
            }

            let has_divert = !self.divert_chips.borrow().is_empty();
            self.divert_group.set_visible(has_divert);
            let has_actions = !self.action_rows.borrow().is_empty();
            self.buttons_group.set_visible(has_actions);
            has_any |= has_actions || has_divert;
        }

        let show_keyboard = device.online && device.fn_swap.is_some();
        self.keyboard_group.set_visible(show_keyboard);
        if let Some(fn_swap) = device.fn_swap {
            self.fn_swap_row.set_active(fn_swap);
            has_any = true;
        }

        self.backlight_group.set_visible(false);
        if device.online {
            if let Some(backlight) = &device.backlight {
                self.backlight_enabled_row.set_active(backlight.enabled);
                self.backlight_mode_row
                    .set_selected(if backlight.mode == 0x03 { 1 } else { 0 });
                self.backlight_level_row
                    .adjustment()
                    .set_upper(backlight.max_level.max(1) as f64);
                self.backlight_level_row.set_value(backlight.level as f64);
                self.backlight_hands_out_row
                    .set_value(backlight.hands_out as f64);
                self.backlight_hands_in_row
                    .set_value(backlight.hands_in as f64);
                self.backlight_powered_row
                    .set_value(backlight.powered as f64);
                self.backlight_group.set_visible(true);
                has_any = true;
            }
        }

        self.disable_group.set_visible(false);
        if device.online {
            if let Some(disable) = &device.disable_keys {
                let mut visible = false;
                for state in self.disable_rows.borrow().iter() {
                    let supported = disable.supported & state.bit != 0;
                    state.row.set_visible(supported);
                    if supported {
                        state.row.set_active(disable.disabled & state.bit != 0);
                        visible = true;
                    }
                }
                self.disable_group.set_visible(visible);
                has_any |= visible;
            }
        }

        self.apply_button
            .set_visible(device.online && (has_any || stable));

        for card in self.profile_cards.borrow().iter() {
            self.profile_grid.remove(card);
        }
        self.profile_cards.borrow_mut().clear();
        if let Some(config) = &device.config {
            let entries = config_tiles(config);
            for (index, (label, value)) in entries.into_iter().enumerate() {
                let card = gtk::Box::new(gtk::Orientation::Vertical, 2);
                card.add_css_class("profile-card");
                card.set_hexpand(true);
                let title = gtk::Label::new(Some(&label));
                title.set_xalign(0.0);
                title.add_css_class("caption");
                title.add_css_class("dim-label");
                let val = gtk::Label::new(Some(&value));
                val.set_xalign(0.0);
                val.add_css_class("profile-value");
                card.append(&title);
                card.append(&val);
                self.profile_grid
                    .attach(&card, (index % 2) as i32, (index / 2) as i32, 1, 1);
                self.profile_cards.borrow_mut().push(card);
            }
        }
        self.profile_section
            .set_visible(!self.profile_cards.borrow().is_empty());

        self.buttons_page.set_visible(
            self.buttons_group.is_visible()
                || self.disable_group.is_visible()
                || self.custom_group.is_visible(),
        );
        self.advanced_page.set_visible(
            self.protocol_group.is_visible()
                || self.hardware_group.is_visible()
                || self.divert_group.is_visible()
                || self.profile_section.is_visible(),
        );
        let current_page_hidden = match self.settings_stack.visible_child_name().as_deref() {
            Some("buttons") => !self.buttons_page.is_visible(),
            Some("advanced") => !self.advanced_page.is_visible(),
            _ => false,
        };
        if is_new_selection || current_page_hidden {
            self.settings_stack.set_visible_child_name("general");
        }

        self.root.set_visible_child_name("device");
        crate::i18n::translate_widget(self.root.upcast_ref());
        align_control_columns(&self.settings_stack.clone().upcast());
        if is_new_selection {
            self.information.set_expanded(false);
            // Initial focus must not scroll the first editable control into view.
            if self.apply_button.is_visible() {
                self.apply_button.grab_focus();
            }
            let adjustment = self.detail_scroll.vadjustment();
            adjustment.set_value(0.0);
            let adjustment_idle = self.detail_scroll.vadjustment();
            gtk::glib::idle_add_local_once(move || adjustment_idle.set_value(0.0));
        }
    }

    fn build_update(&self, device: &DeviceSnapshot) -> SettingsUpdate {
        let mut update = SettingsUpdate::default();
        if !device.online {
            return update;
        }
        if let Some(wheel) = &device.thumbwheel {
            let inverted = self.thumbwheel_inverted_row.is_active();
            let diverted = self.thumbwheel_diverted_row.is_active();
            if inverted != wheel.inverted {
                update.thumbwheel_inverted = Some(inverted);
            }
            if diverted != wheel.diverted {
                update.thumbwheel_diverted = Some(diverted);
            }
        }
        if let Some(rate) = &device.report_rate {
            if let Some(value) = self
                .report_rate_values
                .borrow()
                .get(self.report_rate_row.selected() as usize)
            {
                if *value != rate.current_us {
                    update.report_rate_us = Some(*value);
                }
            }
        }
        if let Some(layout) = &device.os_layout {
            if let Some(value) = self
                .os_layout_values
                .borrow()
                .get(self.os_layout_row.selected() as usize)
            {
                if *value != layout.current && layout.settable {
                    update.os_layout = Some(*value);
                }
            }
        }

        if device.dpi_feature.is_some() {
            let value = self.dpi_row.value().round() as u16;
            if Some(value) != device.dpi {
                update.dpi = Some(value);
            }
        }
        if let Some(inverted) = device.scroll_inverted() {
            let value = self.invert_row.is_active();
            if value != inverted {
                update.invert_scroll = Some(value);
            }
        }
        if let Some(high_resolution) = device.scroll_high_resolution() {
            let value = self.resolution_row.is_active();
            if value != high_resolution {
                update.high_resolution_scroll = Some(value);
            }
        }
        if let Some(diversion) = device.scroll_diversion() {
            let value = self.scroll_diversion_row.is_active();
            if value != diversion {
                update.scroll_diversion = Some(value);
            }
        }
        if let Some(setting) = &device.smart_shift {
            let ratcheted = self.ratchet_row.is_active();
            if Some(ratcheted) != device.is_ratcheted() {
                update.ratcheted = Some(ratcheted);
            }
            let threshold = self.threshold_row.value().round() as u8;
            if threshold != setting.threshold {
                update.smart_shift_threshold = Some(threshold);
            }
            if let Some(torque) = setting.torque {
                let value = self.torque_row.value().round() as u8;
                if value != torque {
                    update.smart_shift_torque = Some(value);
                }
            }
        }
        if let Some(smooth) = device.smooth_scroll {
            let value = self.smooth_row.is_active();
            if value != smooth {
                update.smooth_scroll = Some(value);
            }
        }
        if let Some(host) = &device.host {
            if self.host_row.is_visible() {
                let selected = self.host_row.selected() as u8;
                if selected != host.current {
                    update.target_host = Some(selected);
                }
            }
        }
        for state in self.action_rows.borrow().iter() {
            let selected = state.selected();
            if let Some(target) = state.target_ids.get(selected) {
                if *target != state.original {
                    update.button_remaps.push((state.cid, *target));
                }
            }
        }
        for state in self.divert_chips.borrow().iter() {
            let value = state.button.is_active();
            if value != state.original {
                update.button_diversions.push((state.cid, value));
            }
        }
        if let Some(fn_swap) = device.fn_swap {
            let value = self.fn_swap_row.is_active();
            if value != fn_swap {
                update.fn_swap = Some(value);
            }
        }
        if let Some(backlight) = &device.backlight {
            if self.backlight_group.is_visible() {
                let enabled = self.backlight_enabled_row.is_active();
                if enabled != backlight.enabled {
                    update.backlight_enabled = Some(enabled);
                }
                let mode = if self.backlight_mode_row.selected() == 1 {
                    0x03
                } else {
                    0x01
                };
                if mode != backlight.mode {
                    update.backlight_mode = Some(mode);
                }
                let level = self.backlight_level_row.value().round() as u8;
                if level != backlight.level {
                    update.backlight_level = Some(level);
                }
                let durations = (
                    self.backlight_hands_out_row.value().round() as u16,
                    self.backlight_hands_in_row.value().round() as u16,
                    self.backlight_powered_row.value().round() as u16,
                );
                if durations != (backlight.hands_out, backlight.hands_in, backlight.powered) {
                    update.backlight_durations = Some(durations);
                }
            }
        }
        if let Some(disable) = &device.disable_keys {
            let mut mask = 0u8;
            for state in self.disable_rows.borrow().iter() {
                if state.row.is_visible() && state.row.is_active() {
                    mask |= state.bit;
                }
            }
            if mask != disable.disabled {
                update.disable_keys = Some(mask);
            }
        }
        update
    }
}

fn diversion_icon_name(name: &str) -> &'static str {
    match name {
        "Brightness Down" | "Brightness Up" => "display-brightness-symbolic",
        "Backlight Down" | "Backlight Up" => "keyboard-brightness-symbolic",
        "Dictation" => "audio-input-microphone-symbolic",
        "Emoji" => "face-smile-symbolic",
        "Mute Microphone" => "microphone-sensitivity-muted-symbolic",
        "Previous Track" => "media-skip-backward-symbolic",
        "Next Track" => "media-skip-forward-symbolic",
        "Play/Pause" => "media-playback-start-symbolic",
        "Mute Sound" => "audio-volume-muted-symbolic",
        "Volume Down" => "audio-volume-low-symbolic",
        "Volume Up" => "audio-volume-high-symbolic",
        "Calculator" => "accessories-calculator-symbolic",
        "Screen Capture" => "camera-photo-symbolic",
        "Context Menu" => "open-menu-symbolic",
        "Screen Lock" => "system-lock-screen-symbolic",
        _ => "input-mouse-symbolic",
    }
}

fn align_control_columns(widget: &gtk::Widget) {
    // Keep controls in the shared CSS column instead of expanding each row.
    if widget.has_css_class("suffixes") {
        widget.set_hexpand(false);
    }
    if widget.has_css_class("title") && widget.is::<gtk::Box>() {
        widget.set_hexpand(false);
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        align_control_columns(&current);
        child = current.next_sibling();
    }
}

fn config_tiles(config: &DeviceConfig) -> Vec<(String, String)> {
    let mut tiles = Vec::new();
    if let Some(dpi) = config.dpi {
        tiles.push(("Saved DPI".to_string(), format!("{dpi} DPI")));
    }
    if let Some(value) = config.scroll_ratchet {
        tiles.push((
            "Wheel Mode".to_string(),
            if value == 2 {
                "Ratcheted".to_string()
            } else {
                "Free-spin".to_string()
            },
        ));
    }
    if let Some(value) = config.smart_shift {
        tiles.push(("SmartShift".to_string(), format!("Speed {value}")));
    }
    if let Some(value) = config.scroll_ratchet_torque {
        tiles.push(("Ratchet Torque".to_string(), format!("{value}%")));
    }
    if let Some(value) = config.hires_smooth_invert {
        tiles.push((
            "Scroll Direction".to_string(),
            if value { "Inverted" } else { "Normal" }.to_string(),
        ));
    }
    if let Some(value) = config.hires_smooth_resolution {
        tiles.push((
            "Wheel Resolution".to_string(),
            if value { "High-res" } else { "Standard" }.to_string(),
        ));
    }
    if let Some(value) = config.hires_scroll_mode {
        tiles.push((
            "Wheel Diversion".to_string(),
            if value { "Diverted" } else { "Regular" }.to_string(),
        ));
    }
    if let Some(wpid) = &config.wpid {
        tiles.push(("Config WPID".to_string(), yaml_scalar(wpid)));
    }
    tiles
}

fn yaml_scalar(value: &serde_yaml::Value) -> String {
    match value {
        serde_yaml::Value::String(text) => text.clone(),
        serde_yaml::Value::Number(number) => number.to_string(),
        serde_yaml::Value::Bool(flag) => flag.to_string(),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BacklightSnapshot, ButtonInfo, DisableKeysSnapshot, HostInfo};

    // Exercises real GTK widgets without opening hardware or writing settings.
    // Run explicitly with a display: cargo test device_pages -- --ignored
    #[test]
    #[ignore = "requires a GTK display"]
    fn device_pages_preserve_pending_edits_and_reset_for_other_devices() {
        adw::init().expect("GTK display available");
        assert!(
            gtk::IconTheme::for_display(&gtk::gdk::Display::default().unwrap())
                .has_icon(DIAGNOSTICS_ICON),
            "Diagnostics/Battery menu icon must exist in the active icon theme"
        );
        install_css();
        let pane = DetailPane::new();
        fn has_action(widget: &gtk::Widget, action: &str) -> bool {
            if widget
                .downcast_ref::<gtk::Button>()
                .is_some_and(|b| b.action_name().as_deref() == Some(action))
            {
                return true;
            }
            let mut child = widget.first_child();
            while let Some(w) = child {
                if has_action(&w, action) {
                    return true;
                }
                child = w.next_sibling();
            }
            false
        }
        assert!(
            has_action(pane.root.upcast_ref(), "app.profiles"),
            "named profile manager must be reachable from device settings"
        );
        let keyboard = DeviceSnapshot {
            name: "Test keyboard".into(),
            number: 1,
            online: true,
            host: Some(HostInfo {
                count: 2,
                current: 0,
                names: vec!["Desktop".into(), "Laptop".into()],
            }),
            backlight: Some(BacklightSnapshot {
                enabled: true,
                mode: 1,
                level: 2,
                max_level: 5,
                hands_out: 30,
                hands_in: 30,
                powered: 300,
            }),
            disable_keys: Some(DisableKeysSnapshot {
                supported: 1,
                disabled: 0,
            }),
            buttons: vec![
                ButtonInfo {
                    cid: 1,
                    mapped_to: 1,
                    name: "First".into(),
                    group: 1,
                    group_mask: 2,
                    remappable: true,
                    divertable: true,
                    ..Default::default()
                },
                ButtonInfo {
                    cid: 2,
                    mapped_to: 2,
                    name: "Second".into(),
                    group: 1,
                    group_mask: 2,
                    remappable: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        pane.show_device(&keyboard);
        assert!(pane.buttons_page.is_visible());
        assert!(pane.advanced_page.is_visible());
        assert!(pane.apply_button.is_visible());
        let initial = pane.build_update(&keyboard);
        assert!(!pane.has_pending_changes());
        assert_eq!(initial.backlight_level, None);
        assert_eq!(initial.target_host, None);
        assert_eq!(initial.disable_keys, None);
        assert!(initial.button_remaps.is_empty());
        pane.backlight_level_row.set_value(4.0);
        pane.host_row.set_selected(1);
        assert!(pane.has_pending_changes());
        pane.settings_stack.set_visible_child_name("buttons");
        pane.disable_rows.borrow()[0].row.set_active(true);
        pane.action_rows.borrow()[0].select(1);
        assert_eq!(pane.action_rows.borrow()[0].selected(), 1);
        assert_eq!(
            pane.action_rows.borrow()[0]
                .options
                .borrow()
                .iter()
                .filter(|option| option.is_active())
                .count(),
            1,
            "Exactly one remapping option must be selected"
        );
        pane.settings_stack.set_visible_child_name("advanced");
        pane.divert_chips.borrow()[0].button.set_active(true);
        let update = pane.build_update(&keyboard);
        assert_eq!(update.backlight_level, Some(4));
        assert_eq!(update.target_host, Some(1));
        assert_eq!(update.disable_keys, Some(1));
        assert_eq!(update.button_remaps, vec![(1, 2)]);
        assert_eq!(update.button_diversions, vec![(1, true)]);

        pane.information.set_expanded(true);
        let mouse = DeviceSnapshot {
            name: "Test mouse".into(),
            number: 2,
            online: true,
            serial: "LOGICENTER-GTK-FIXTURE".into(),
            dpi: Some(1000),
            dpi_feature: Some(1),
            scroll_flags: Some(2),
            buttons: vec![ButtonInfo {
                cid: 195,
                name: "Gesture Button".into(),
                divertable: true,
                raw_xy: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        pane.show_device(&mouse);
        assert_eq!(
            pane.settings_stack.visible_child_name().as_deref(),
            Some("general")
        );
        assert!(!pane.information.is_expanded());
        assert!(pane.buttons_page.is_visible());
        assert!(pane.custom_group.is_visible());
        assert!(!pane.profile_dirty());
        pane.remember_row.set_active(true);
        assert!(pane.profile_dirty());
        pane.remember_row.set_active(false);
        pane.presets_entry.set_text("800, 1600");
        assert_eq!(pane.profile_options().unwrap(), (false, vec![800, 1600]));
        assert!(pane.has_pending_changes());
        pane.presets_entry.set_text("999");
        assert!(pane.profile_options().is_err());
        pane.show_device(&mouse);
        assert!(!pane.has_pending_changes());
        *pane.dpi_row.allowed.borrow_mut() = vec![400, 800, 1600, 3200];
        pane.dpi_row.set_value(800.0);
        pane.dpi_row
            .scale
            .emit_by_name::<bool>("change-value", &[&gtk::ScrollType::StepForward, &801.0f64]);
        assert_eq!(
            pane.dpi_row.value(),
            1600.0,
            "keyboard steps must advance to the next advertised DPI"
        );
        pane.dpi_row.scale.emit_by_name::<bool>(
            "change-value",
            &[&gtk::ScrollType::StepBackward, &1599.0f64],
        );
        assert_eq!(pane.dpi_row.value(), 800.0);
        pane.dpi_row.set_value(1600.0);
        pane.settings_stack.set_visible_child_name("advanced");
        pane.scroll_diversion_row.set_active(true);
        let update = pane.build_update(&mouse);
        assert_eq!(update.dpi, Some(1600));
        assert_eq!(update.scroll_diversion, Some(true));

        let window = adw::ApplicationWindow::builder()
            .title("Logi Center — GTK fixture")
            .default_width(920)
            .default_height(760)
            .build();
        window.set_content(Some(&pane.root));
        window.present();
        pane.settings_stack.set_visible_child_name("general");
        crate::actions_ui::snapshot_fixture(&window, "general");
        crate::actions_ui::exercise_dialogs(&window, mouse.clone());
        crate::peripherals_ui::exercise_fixture(&window);
        let profile_path = crate::preferences::config_home()
            .unwrap()
            .join("logicenter/devices.json");
        let before_profiles = std::fs::read(&profile_path).ok();
        let profiles_window = crate::profile_ui::show(&window, mouse.clone(), || {
            panic!("Smoke test must not save a profile")
        });
        profiles_window.close();
        let services = crate::services_ui::show(&window, ScanReport::default());
        services.force_close();
        assert_eq!(
            std::fs::read(&profile_path).ok(),
            before_profiles,
            "Opening and closing managers must not change profiles"
        );
        fn find_tool(widget: &gtk::Widget, title: &str) -> Option<gtk::Button> {
            if let Some(button) = widget.downcast_ref::<gtk::Button>() {
                if button.tooltip_text().as_deref() == Some(title) {
                    return Some(button.clone());
                }
            }
            let mut child = widget.first_child();
            while let Some(w) = child {
                if let Some(button) = find_tool(&w, title) {
                    return Some(button);
                }
                child = w.next_sibling();
            }
            None
        }
        let invoked = Rc::new(RefCell::new(None));
        let recorded = invoked.clone();
        let overview =
            control_center_dialog(&window, None, "Fixture backend".into(), move |action| {
                *recorded.borrow_mut() = Some(action.to_string());
            });
        assert!(
            !find_tool(overview.upcast_ref(), "Device Profiles")
                .unwrap()
                .is_sensitive()
        );
        find_tool(overview.upcast_ref(), "Manage Receivers")
            .unwrap()
            .emit_clicked();
        overview.force_close();
        while glib::MainContext::default().pending() {
            glib::MainContext::default().iteration(false);
        }
        assert_eq!(invoked.borrow().as_deref(), Some("receivers"));
        window.set_content(None::<&gtk::Widget>);
        window.close();
        drop(window);
        while glib::MainContext::default().pending() {
            glib::MainContext::default().iteration(false);
        }

        let offline = DeviceSnapshot {
            online: false,
            ..mouse
        };
        pane.show_device(&offline);
        assert!(pane.offline_notice.is_visible());
        assert!(!pane.apply_button.is_visible());
        assert!(!pane.advanced_page.is_visible());
        assert_eq!(
            pane.settings_stack.visible_child_name().as_deref(),
            Some("general")
        );
        assert_eq!(pane.build_update(&offline).dpi, None);
        let weak_root = pane.root.downgrade();
        let weak_slider = pane.dpi_row.scale.downgrade();
        drop(pane);
        assert!(
            weak_root.upgrade().is_none(),
            "detail tree must be released"
        );
        assert!(weak_slider.upgrade().is_none(), "controls must be released");
        exercise_camera_sidebar();
    }

    fn exercise_camera_sidebar() {
        fn contains(widget: &gtk::Widget, text: &str) -> bool {
            if widget
                .downcast_ref::<gtk::Label>()
                .is_some_and(|label| label.text().contains(text))
            {
                return true;
            }
            let mut child = widget.first_child();
            while let Some(current) = child {
                if contains(&current, text) {
                    return true;
                }
                child = current.next_sibling();
            }
            false
        }
        fn has_camera_slider(widget: &gtk::Widget) -> bool {
            if widget
                .downcast_ref::<gtk::Scale>()
                .is_some_and(|scale| scale.has_css_class("camera-slider"))
            {
                return true;
            }
            let mut child = widget.first_child();
            while let Some(current) = child {
                if has_camera_slider(&current) {
                    return true;
                }
                child = current.next_sibling();
            }
            false
        }
        let app = adw::Application::builder()
            .application_id("com.chrisdaggas.logicenter.CameraFixture")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let state = build_window(&app, false, false);
        state.window.present();
        let inventory: crate::peripherals::Inventory = serde_json::from_value(serde_json::json!({
            "backends":[], "devices":[
                {"id":"camera:/dev/video1","name":"Logitech BRIO","kind":"camera","backend":"fixture",
                 "status":"No hardware I/O", "metadata":{"usb_identity":"port1:046d:085e:fixture"},
                 "controls":[{"id":"brightness","label":"Brightness","constraint":{"type":"integer","min":0,"max":255,"step":1,"choices":[]},"current":128,"note":""}]},
                {"id":"camera:/dev/video5","name":"Brio 105","kind":"camera","backend":"fixture",
                 "status":"No hardware I/O", "metadata":{"usb_identity":"port2:046d:094b:fixture"},
                 "controls":[{"id":"contrast","label":"Contrast","constraint":{"type":"integer","min":0,"max":255,"step":1,"choices":[]},"current":128,"note":""}]}
            ]
        })).unwrap();
        let keys: Vec<_> = inventory
            .devices
            .iter()
            .map(crate::peripherals::camera_key)
            .collect();
        finish_scan(&state, ScanReport::default(), inventory.clone());
        assert_eq!(state.device_rows.borrow().len(), 2);
        assert!(!state.sidebar_receiver_card.is_visible());
        assert!(contains(state.sidebar_list.upcast_ref(), "Logitech BRIO"));
        assert!(contains(state.sidebar_list.upcast_ref(), "Brio 105"));
        assert_eq!(state.footer.text(), "2 of 2 devices online");
        assert!(
            state
                .detail
                .borrow()
                .as_ref()
                .unwrap()
                .camera_preview
                .borrow()
                .is_some()
        );
        let initial_camera_page = state
            .detail
            .borrow()
            .as_ref()
            .unwrap()
            .root
            .child_by_name("camera")
            .unwrap();
        assert!(contains(&initial_camera_page, "Start preview"));
        assert!(contains(&initial_camera_page, "Save settings"));
        assert!(has_camera_slider(&initial_camera_page));
        println!("CAMERA INLINE PREVIEW INTEGRATION PASSED");
        let row = state.device_rows.borrow()[1].1.clone();
        state.sidebar_list.select_row(Some(&row));
        assert_eq!(*state.selected_key.borrow(), Some(keys[1].clone()));
        {
            let detail = state.detail.borrow();
            let detail = detail.as_ref().unwrap();
            assert_eq!(detail.root.visible_child_name().as_deref(), Some("camera"));
            let page = detail.root.child_by_name("camera").unwrap();
            assert!(contains(&page, "Brio 105"));
            assert!(contains(&page, "Contrast"));
            assert!(
                detail.selected.borrow().is_none(),
                "Camera must not retain an HID Apply target"
            );
        }
        toggle_sidebar(&state);
        assert!(state.sidebar_brand.is_visible());
        assert!(!state.sidebar_brand_copy.is_visible());
        assert_eq!(
            state.sidebar_header.orientation(),
            gtk::Orientation::Vertical
        );
        let button = state.rail_buttons.borrow()[0].1.clone();
        button.emit_clicked();
        assert_eq!(*state.selected_key.borrow(), Some(keys[0].clone()));
        assert!(button.has_css_class("selected"));
        assert!(contains(
            &state
                .detail
                .borrow()
                .as_ref()
                .unwrap()
                .root
                .child_by_name("camera")
                .unwrap(),
            "Brightness"
        ));

        let mut refreshed = inventory.clone();
        refreshed.devices[0].id = "camera:/dev/video11".into();
        finish_scan(&state, ScanReport::default(), refreshed);
        assert_eq!(
            *state.selected_key.borrow(),
            Some(keys[0].clone()),
            "Preserve selection across node renumbering"
        );
        assert!(contains(
            &state
                .detail
                .borrow()
                .as_ref()
                .unwrap()
                .root
                .child_by_name("camera")
                .unwrap(),
            "/dev/video11"
        ));

        let mouse = DeviceSnapshot {
            name: "Fixture mouse".into(),
            serial: "CAMERA-SIDEBAR-MOUSE-FIXTURE".into(),
            receiver_path: "/dev/fixture".into(),
            number: 1,
            online: true,
            dpi: Some(800),
            dpi_feature: Some(1),
            dpi_values: vec![800, 1600],
            ..Default::default()
        };
        let report = ScanReport {
            receivers: vec![ReceiverSnapshot {
                name: "Fixture receiver".into(),
                path: "/dev/fixture".into(),
                devices: vec![mouse.clone()],
                ..Default::default()
            }],
            errors: vec![],
        };
        finish_scan(&state, report.clone(), inventory.clone());
        assert_eq!(state.device_rows.borrow().len(), 3);
        assert_eq!(state.footer.text(), "3 of 3 devices online");
        select_device(&state, &mouse);
        state
            .detail
            .borrow()
            .as_ref()
            .unwrap()
            .dpi_row
            .set_value(1600.0);
        assert!(
            state
                .detail
                .borrow()
                .as_ref()
                .unwrap()
                .has_pending_changes()
        );
        select_key(&state, &keys[1]);
        assert_eq!(
            *state.selected_key.borrow(),
            Some(mouse.key()),
            "Camera selection must not discard HID edits"
        );
        finish_scan(&state, report, inventory.clone());
        assert_eq!(
            state.detail.borrow().as_ref().unwrap().dpi_row.value(),
            1600.0
        );
        state.detail.borrow().as_ref().unwrap().show_device(&mouse);
        select_key(&state, &keys[1]);
        assert_eq!(*state.selected_key.borrow(), Some(keys[1].clone()));
        let mut unavailable = inventory.clone();
        unavailable.devices[1].controls.clear();
        unavailable.devices[1].status = "Camera controls unavailable: Permission denied".into();
        finish_scan(&state, ScanReport::default(), unavailable);
        assert_eq!(
            state.device_rows.borrow().len(),
            2,
            "Permission errors must not hide a camera"
        );
        assert!(contains(
            &state
                .detail
                .borrow()
                .as_ref()
                .unwrap()
                .root
                .child_by_name("camera")
                .unwrap(),
            "Permission denied"
        ));
        let mut removed = inventory;
        removed.devices.remove(1);
        finish_scan(&state, ScanReport::default(), removed);
        assert_eq!(
            *state.selected_key.borrow(),
            Some(keys[0].clone()),
            "Fall back to remaining camera after disconnect"
        );
        finish_scan(
            &state,
            ScanReport::default(),
            crate::peripherals::Inventory::default(),
        );
        assert!(state.device_rows.borrow().is_empty());
        assert!(state.selected_key.borrow().is_none());
        assert_eq!(
            state
                .detail
                .borrow()
                .as_ref()
                .unwrap()
                .root
                .visible_child_name()
                .as_deref(),
            Some("empty")
        );
        state.window.destroy();
        println!("CAMERA SIDEBAR INTEGRATION PASSED");
    }
}
