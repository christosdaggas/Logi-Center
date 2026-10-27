//! Standalone desktop-services panel. Register `show` in a menu/tray action;
//! no changes to preferences.rs or shared application state are necessary.
use std::cell::Cell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;
use libadwaita as adw;

use crate::battery::{Config, DeviceHistory, Monitor};
use crate::diagnostics;
use crate::model::ScanReport;

type LoadedServices = (
    diagnostics::Report,
    Result<Config, String>,
    Vec<DeviceHistory>,
    Option<String>,
);

pub fn show(parent: &adw::ApplicationWindow, report: ScanReport) -> adw::Dialog {
    show_with_saved(parent, report, |_| {})
}

/// `on_saved` runs on the GTK main thread after a successful explicit Save.
/// Long-lived monitors also reload settings on their next `observe`, so a
/// callback is optional. Closing/Cancel before Save never commits a draft;
/// an already requested Save is not rolled back by closing the panel.
pub fn show_with_saved(
    parent: &adw::ApplicationWindow,
    report: ScanReport,
    on_saved: impl Fn(Config) + 'static,
) -> adw::Dialog {
    let dialog = adw::Dialog::builder()
        .title("Battery and diagnostics")
        .content_width(640)
        .content_height(760)
        .build();
    let closed = Rc::new(Cell::new(false));
    dialog.connect_closed({
        let closed = closed.clone();
        move |_| closed.set(true)
    });
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let page = adw::PreferencesPage::new();
    let settings = adw::PreferencesGroup::builder()
        .title("Battery notifications")
        .description("Low and critical alerts are sent once per threshold crossing while Logi Center runs. Charging/full states suppress alerts. Plugging in alone does not rearm: battery level must recover by the margin. Unknown or invalid readings are ignored.")
        .build();
    let enabled = adw::SwitchRow::builder()
        .title("Enable battery notifications")
        .build();
    let (low_row, low) = spin("Low threshold (%)", 1.0, 80.0, 1.0);
    let (critical_row, critical) = spin("Critical threshold (%)", 0.0, 79.0, 1.0);
    let (hysteresis_row, hysteresis) = spin("Recovery margin (percentage points)", 1.0, 20.0, 1.0);
    settings.add(&enabled);
    settings.add(&low_row);
    settings.add(&critical_row);
    settings.add(&hysteresis_row);
    settings.set_sensitive(false);
    page.add(&settings);

    let history_settings = adw::PreferencesGroup::builder()
        .title("Private local history")
        .description("Optional and off by default. Stores stable device identity, name, percentage, status and UNIX measurement time in a private local file (0600). No telemetry. At most 32 devices; unchanged readings are sampled every 5 minutes. Save with history off to delete saved history. Devices without a stable identity are not recorded.")
        .build();
    let history_enabled = adw::SwitchRow::builder()
        .title("Record battery history (opt in)")
        .build();
    let (days_row, days) = spin("Retention (days)", 1.0, 90.0, 1.0);
    let (samples_row, samples) = spin("Maximum samples per device", 16.0, 1024.0, 16.0);
    history_settings.add(&history_enabled);
    history_settings.add(&days_row);
    history_settings.add(&samples_row);
    history_settings.set_sensitive(false);
    page.add(&history_settings);
    history_enabled.connect_active_notify({
        let days_row = days_row.clone();
        let samples_row = samples_row.clone();
        move |row| {
            days_row.set_sensitive(row.is_active());
            samples_row.set_sensitive(row.is_active());
        }
    });

    let readings = adw::PreferencesGroup::builder()
        .title("Current scan")
        .description("Read-only snapshot from when this panel opened; use Refresh in the main window for a new scan. Offline devices never receive a new measurement timestamp.")
        .build();
    let mut count = 0;
    for device in report
        .receivers
        .iter()
        .flat_map(|receiver| &receiver.devices)
        .take(64)
    {
        count += 1;
        let text = if !device.online {
            "Offline · see last saved measurement below".into()
        } else if matches!(device.battery_status, Some(5..=7)) {
            "Battery reported an error; percentage not treated as a measurement".into()
        } else if let Some(percent) = device.battery.filter(|percent| *percent <= 100) {
            let charging = if matches!(device.battery_status, Some(1..=4)) {
                " · charging/full (alerts suppressed)"
            } else if device.battery_status != Some(0) {
                " · charge status unknown"
            } else {
                ""
            };
            format!("{percent}%{charging}")
        } else {
            "Percentage unavailable".into()
        };
        readings.add(&plain_row(&device.name, &text));
    }
    if count == 0 {
        readings.add(&plain_row(
            "No devices in this scan",
            "Last-known history, if enabled, remains available below.",
        ));
    }
    page.add(&readings);
    let history_view = adw::PreferencesGroup::builder()
        .title("Saved measurements")
        .description("Loaded from local history, not a live battery poll. Times are UNIX seconds. Offline values are last-known readings, never estimates. Shows the latest 12 samples per device.")
        .build();
    let history_loading = plain_row("Loading history…", "No device is queried by this panel.");
    history_view.add(&history_loading);
    page.add(&history_view);

    let diagnostics_group = adw::PreferencesGroup::builder()
        .title("Permissions and backend diagnostics")
        .description("Read-only metadata checks. No helper commands, HID writes, pairing, input injection, permission changes or module loading. uinput access can synthesize keyboard/mouse input: grant it only to trusted local-session applications, never to everyone with MODE=0666.")
        .build();
    let diagnostic_text = gtk::Label::new(Some(
        "Collecting bounded, redacted diagnostics on a worker…",
    ));
    diagnostic_text.set_wrap(true);
    diagnostic_text.set_selectable(true);
    diagnostic_text.set_xalign(0.0);
    diagnostic_text.set_margin_top(12);
    diagnostic_text.set_margin_bottom(12);
    diagnostic_text.set_margin_start(12);
    diagnostic_text.set_margin_end(12);
    diagnostics_group.add(&diagnostic_text);
    let include = adw::SwitchRow::builder()
        .title("Include device identifiers in export (opt in)")
        .subtitle("Adds device names, serials and endpoint/helper paths. Home directories stay redacted. Raw logs/errors and process command lines are never exported.")
        .active(false)
        .build();
    diagnostics_group.add(&include);
    let export_row = adw::ActionRow::builder()
        .title("Export JSON report…")
        .build();
    let export = gtk::Button::with_label("Choose file…");
    export.set_valign(gtk::Align::Center);
    export_row.add_suffix(&export);
    export_row.set_activatable_widget(Some(&export));
    diagnostics_group.add(&export_row);
    page.add(&diagnostics_group);
    content.append(&page);

    let status = gtk::Label::new(None);
    status.set_wrap(true);
    status.set_margin_start(24);
    status.set_margin_end(24);
    status.set_visible(false);
    content.append(&status);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_end(24);
    buttons.set_margin_start(24);
    buttons.set_margin_bottom(20);
    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::with_label("Save battery settings");
    save.add_css_class("suggested-action");
    save.set_sensitive(false);
    buttons.append(&cancel);
    buttons.append(&save);
    content.append(&buttons);
    toolbar.set_content(Some(&content));
    dialog.set_child(Some(&toolbar));
    cancel.connect_clicked({
        let dialog = dialog.downgrade();
        move |_| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.close();
            }
        }
    });

    let scan = report.clone();
    worker(
        closed.clone(),
        move || {
            let diagnostic = diagnostics::collect(&scan, false);
            let config = Config::load().map_err(|error| error.to_string());
            let monitor = Monitor::load();
            let history: Vec<_> = monitor.history().cloned().collect();
            (
                diagnostic,
                config,
                history,
                monitor.storage_error().map(str::to_owned),
            )
        },
        {
            let status = status.clone();
            let save = save.clone();
            let enabled = enabled.clone();
            let low = low.clone();
            let critical = critical.clone();
            let hysteresis = hysteresis.clone();
            let history_enabled = history_enabled.clone();
            let days = days.clone();
            let samples = samples.clone();
            let report = report.clone();
            move |loaded: Result<LoadedServices, String>| match loaded {
                Ok((diagnostic, config, history, storage_error)) => {
                    diagnostic_text.set_text(&diagnostic.summary());
                    history_view.remove(&history_loading);
                    populate_history(&history_view, &history, &report);
                    match config {
                        Ok(config) => {
                            enabled.set_active(config.enabled());
                            low.set_value(f64::from(config.low()));
                            critical.set_value(f64::from(config.critical()));
                            hysteresis.set_value(f64::from(config.hysteresis()));
                            history_enabled.set_active(config.history_enabled());
                            days.set_value(f64::from(config.retention_days()));
                            samples.set_value(config.max_samples() as f64);
                            days_row.set_sensitive(config.history_enabled());
                            samples_row.set_sensitive(config.history_enabled());
                            settings.set_sensitive(true);
                            history_settings.set_sensitive(true);
                            save.set_sensitive(true);
                            if let Some(error) = storage_error {
                                message(
                                    &status,
                                    &format!("History could not be fully loaded: {error}"),
                                    true,
                                );
                            }
                        }
                        Err(error) => message(
                            &status,
                            &format!(
                                "Battery settings could not be loaded: {error}. Existing data was not overwritten. Repair the local settings file before saving."
                            ),
                            true,
                        ),
                    }
                }
                Err(error) => message(&status, &error, true),
            }
        },
    );

    let on_saved: Rc<dyn Fn(Config)> = Rc::new(on_saved);
    save.connect_clicked({
        let closed = closed.clone();
        let dialog = dialog.downgrade();
        let status = status.clone();
        move |save| {
            let config = Config::new(
                enabled.is_active(),
                low.value_as_int() as u8,
                critical.value_as_int() as u8,
                hysteresis.value_as_int() as u8,
                history_enabled.is_active(),
                days.value_as_int() as u16,
                samples.value_as_int() as usize,
            );
            let config = match config {
                Ok(config) => config,
                Err(error) => {
                    message(&status, &error.to_string(), true);
                    return;
                }
            };
            save.set_sensitive(false);
            message(&status, "Saving battery settings…", false);
            let saved = config.clone();
            worker(
                closed.clone(),
                move || config.save().map_err(|error| error.to_string()),
                {
                    let save = save.clone();
                    let status = status.clone();
                    let dialog = dialog.clone();
                    let on_saved = on_saved.clone();
                    move |result| match result {
                        Ok(Ok(())) => {
                            on_saved(saved);
                            if let Some(dialog) = dialog.upgrade() {
                                dialog.close();
                            }
                        }
                        Ok(Err(error)) | Err(error) => {
                            save.set_sensitive(true);
                            message(
                                &status,
                                &format!("Could not save battery settings: {error}"),
                                true,
                            );
                        }
                    }
                },
            );
        }
    });

    export.connect_clicked({
        let closed = closed.clone();
        let status = status.clone();
        let parent = parent.downgrade();
        move |button| {
            let include_identifiers = include.is_active();
            let report = report.clone();
            button.set_sensitive(false);
            message(&status, "Preparing diagnostic export…", false);
            worker(
                closed.clone(),
                move || {
                    diagnostics::collect(&report, include_identifiers)
                        .to_json()
                        .map_err(|error| error.to_string())
                },
                {
                    let button = button.clone();
                    let status = status.clone();
                    let parent = parent.clone();
                    move |result| {
                        button.set_sensitive(true);
                        match result {
                            Ok(Ok(bytes)) => {
                                if let Some(parent) = parent.upgrade() {
                                    choose_export(&parent, bytes, &status);
                                }
                            }
                            Ok(Err(error)) | Err(error) => message(
                                &status,
                                &format!("Could not export diagnostics: {error}"),
                                true,
                            ),
                        }
                    }
                },
            );
        }
    });
    dialog.present(Some(parent));
    dialog
}

fn spin(title: &str, min: f64, max: f64, step: f64) -> (adw::ActionRow, gtk::SpinButton) {
    let row = adw::ActionRow::builder().title(title).build();
    row.set_use_markup(false);
    let spin = gtk::SpinButton::with_range(min, max, step);
    spin.set_digits(0);
    spin.set_numeric(true);
    spin.set_valign(gtk::Align::Center);
    spin.set_tooltip_text(Some(title));
    row.add_suffix(&spin);
    row.set_activatable_widget(Some(&spin));
    (row, spin)
}

fn plain_row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .build();
    row.set_use_markup(false);
    row
}

fn populate_history(group: &adw::PreferencesGroup, histories: &[DeviceHistory], scan: &ScanReport) {
    if histories.is_empty() {
        group.add(&plain_row("No saved battery history", "Enable history and Save to record subsequent scans. Nothing is recorded merely by opening this panel."));
    }
    for history in histories.iter().take(32) {
        let Some(last) = &history.last else {
            continue;
        };
        let online = scan
            .receivers
            .iter()
            .flat_map(|receiver| &receiver.devices)
            .any(|device| {
                device.online
                    && crate::profiles::identity(device).as_deref()
                        == Some(history.identity.as_str())
            });
        let row = adw::ExpanderRow::builder()
            .title(&history.name)
            .subtitle(format!(
                "{} · last saved {}% at UNIX {} · {} samples",
                if online {
                    "Online in current scan"
                } else {
                    "Offline/not in current scan"
                },
                last.percent,
                last.timestamp,
                history.samples.len()
            ))
            .build();
        row.set_use_markup(false);
        for sample in history.samples.iter().rev().take(12) {
            let charge = match sample.status {
                Some(1..=4) => "charging/full",
                Some(0) => "discharging",
                _ => "charge state unknown",
            };
            row.add_row(&plain_row(
                &format!("{}% · {charge}", sample.percent),
                &format!("UNIX {}", sample.timestamp),
            ));
        }
        group.add(&row);
    }
}

fn message(label: &gtk::Label, text: &str, error: bool) {
    label.set_text(text);
    label.set_visible(true);
    if error {
        label.add_css_class("error");
    } else {
        label.remove_css_class("error");
    }
}

/// Work is count/size bounded; closing the dialog stops UI polling and discards
/// its result. No Gtk object is sent to a worker thread. Metadata I/O itself
/// cannot be forcibly cancelled if an underlying filesystem stalls.
fn worker<T: Send + 'static>(
    closed: Rc<Cell<bool>>,
    job: impl FnOnce() -> T + Send + 'static,
    complete: impl FnOnce(Result<T, String>) + 'static,
) {
    let (tx, rx) = mpsc::sync_channel(1);
    if let Err(error) = std::thread::Builder::new()
        .name("logicenter-services".into())
        .spawn(move || {
            let _ = tx.send(job());
        })
    {
        complete(Err(format!(
            "Could not start diagnostics/settings worker: {error}"
        )));
        return;
    }
    let mut complete = Some(complete);
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if closed.get() {
            return glib::ControlFlow::Break;
        }
        let result = match rx.try_recv() {
            Ok(value) => Ok(value),
            Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("Background task ended without a result".into())
            }
        };
        if let Some(complete) = complete.take() {
            complete(result);
        }
        glib::ControlFlow::Break
    });
}

fn choose_export(parent: &adw::ApplicationWindow, bytes: Vec<u8>, status: &gtk::Label) {
    let chooser = gtk::FileChooserNative::new(
        Some("Export diagnostic JSON"),
        Some(parent),
        gtk::FileChooserAction::Save,
        Some("Save report"),
        Some("Cancel"),
    );
    chooser.set_modal(true);
    chooser.set_current_name("logicenter-diagnostics.json");
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("JSON reports"));
    filter.add_pattern("*.json");
    chooser.add_filter(&filter);
    message(
        status,
        "Choose where to save the report. Cancel writes nothing.",
        false,
    );
    let status = status.downgrade();
    glib::MainContext::default().spawn_local(async move {
        // Keep the native chooser alive until its response, even on desktops
        // where showing a native dialog does not retain a GTK widget reference.
        let response = chooser.run_future().await;
        let file = chooser.file();
        chooser.destroy();
        if response == gtk::ResponseType::Accept {
            if let Some(file) = file {
                let result = file
                    .replace_contents_future(
                        bytes,
                        None,
                        false,
                        gio::FileCreateFlags::PRIVATE | gio::FileCreateFlags::REPLACE_DESTINATION,
                    )
                    .await;
                if let Some(status) = status.upgrade() {
                    match result {
                        Ok(_) => message(
                            &status,
                            "Diagnostic report saved. Review it before sharing.",
                            false,
                        ),
                        Err((_, error)) => {
                            message(&status, &format!("Could not save report: {error}"), true)
                        }
                    }
                }
            } else if let Some(status) = status.upgrade() {
                message(
                    &status,
                    "No export file selected; nothing was written.",
                    true,
                );
            }
        } else if let Some(status) = status.upgrade() {
            message(&status, "Export cancelled; no report was written.", false);
        }
    });
}
