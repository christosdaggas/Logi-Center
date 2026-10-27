//! Configuration-only profile manager. The caller decides when to apply hardware.
use std::collections::BTreeSet;
use std::rc::Rc;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::model::DeviceSnapshot;
use crate::profiles::{self, AppRule, RestoreSetting};

type Changed = Rc<dyn Fn()>;

pub fn show(
    parent: &adw::ApplicationWindow,
    device: DeviceSnapshot,
    on_changed: impl Fn() + 'static,
) -> adw::Window {
    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(format!("Profiles — {}", device.name))
        .default_width(680)
        .default_height(760)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let body = gtk::Box::new(gtk::Orientation::Vertical, 16);
    body.set_margin_start(20);
    body.set_margin_end(20);
    body.set_margin_top(16);
    body.set_margin_bottom(20);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&body)
        .build();
    toolbar.set_content(Some(&scroll));
    window.set_content(Some(&toolbar));
    refresh(
        &body,
        Rc::new(device),
        Rc::new(on_changed),
        "Changes are saved immediately. Enabled restore policies can apply saved settings when the active profile changes.",
    );
    window.present();
    window
}

fn message(body: &gtk::Box, text: &str) {
    let label = gtk::Label::new(Some(text));
    label.set_wrap(true);
    label.set_xalign(0.0);
    label.set_selectable(true);
    body.append(&label);
}
fn operation(
    button: &gtk::Button,
    body: &gtk::Box,
    device: Rc<DeviceSnapshot>,
    changed: Changed,
    action: impl Fn(&DeviceSnapshot) -> anyhow::Result<String> + 'static,
) {
    let weak = body.downgrade();
    button.connect_clicked(move |_| {
        if let Some(body) = weak.upgrade() {
            match action(&device) {
                Ok(text) => {
                    changed();
                    refresh(&body, device.clone(), changed.clone(), &text);
                }
                Err(error) => message(&body, &format!("Error: {error:#}")),
            }
        }
    });
}
fn refresh(body: &gtk::Box, device: Rc<DeviceSnapshot>, changed: Changed, notice: &str) {
    while let Some(child) = body.first_child() {
        body.remove(&child);
    }
    message(body, notice);
    let set = match profiles::profile_set(&device) {
        Ok(set) => set,
        Err(error) => {
            message(
                body,
                &format!("Cannot read profiles: {error:#}. Original data has not been modified."),
            );
            return;
        }
    };
    if let Ok(status) = profiles::selection_status(&device) {
        message(
            body,
            &format!(
                "Selected: {} · Effective: {} · App: {} · {}",
                status.selected,
                status.effective,
                status.app_id.as_deref().unwrap_or("unavailable"),
                if status.manual_lock {
                    "manual lock"
                } else if status.matched_rule.is_some() {
                    "app rule"
                } else {
                    "selected fallback"
                }
            ),
        );
    }
    let group = adw::PreferencesGroup::builder()
        .title("Named profiles")
        .build();
    let names: Vec<String> = set.profiles.keys().cloned().collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let chooser = gtk::DropDown::from_strings(&refs);
    chooser.set_valign(gtk::Align::Center);
    chooser.set_selected(names.iter().position(|n| *n == set.selected).unwrap_or(0) as u32);
    let row = adw::ActionRow::builder()
        .title("Selected profile")
        .subtitle("App rules override this unless manual lock is on")
        .build();
    row.add_suffix(&chooser);
    group.add(&row);
    let weak = body.downgrade();
    let (dev, callback, choice_names) = (device.clone(), changed.clone(), names.clone());
    chooser.connect_selected_notify(move |chooser| {
        if let (Some(body), Some(name)) = (
            weak.upgrade(),
            choice_names.get(chooser.selected() as usize),
        ) {
            match profiles::select(&dev, name) {
                Ok(()) => {
                    callback();
                    refresh(
                        &body,
                        dev.clone(),
                        callback.clone(),
                        "Selected profile saved.",
                    );
                }
                Err(error) => message(&body, &error.to_string()),
            }
        }
    });
    let name = adw::EntryRow::builder()
        .title("New / renamed profile name")
        .build();
    name.set_max_length(80);
    group.add(&name);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    for label in ["Create", "Duplicate", "Rename", "Delete"] {
        let button = gtk::Button::with_label(label);
        let entry = name.clone();
        let selected = set.selected.clone();
        operation(
            &button,
            body,
            device.clone(),
            changed.clone(),
            move |device| {
                let new = entry.text().to_string();
                match label {
                    "Create" => profiles::create(device, &new)?,
                    "Duplicate" => profiles::duplicate(device, &selected, &new)?,
                    "Rename" => profiles::rename(device, &selected, &new)?,
                    _ => profiles::delete(device, &selected)?,
                }
                Ok(format!(
                    "{label} completed. App rules still take precedence unless locked."
                ))
            },
        );
        if label == "Delete" {
            button.add_css_class("destructive-action");
        }
        buttons.append(&button);
    }
    group.add(&buttons);
    body.append(&group);

    let rules = adw::PreferencesGroup::builder().title("Application rules").description("Exact case-insensitive app IDs; highest priority wins, then first rule. Manual lock wins over every rule.").build();
    let lock = adw::SwitchRow::builder()
        .title("Manual lock")
        .active(set.manual_lock)
        .build();
    let (dev, callback, weak) = (device.clone(), changed.clone(), body.downgrade());
    lock.connect_active_notify(
        move |row| match profiles::set_manual_lock(&dev, row.is_active()) {
            Ok(()) => {
                callback();
                if let Some(body) = weak.upgrade() {
                    refresh(&body, dev.clone(), callback.clone(), "Manual lock updated.");
                }
            }
            Err(error) => {
                if let Some(body) = weak.upgrade() {
                    message(&body, &error.to_string());
                }
            }
        },
    );
    rules.add(&lock);
    for (index, rule) in set.rules.iter().enumerate() {
        let expected = rule.clone();
        let row = adw::ActionRow::builder()
            .title(&rule.app_id)
            .subtitle(format!(
                "{} · priority {} · {}",
                rule.profile,
                rule.priority,
                if rule.enabled { "enabled" } else { "disabled" }
            ))
            .build();
        let remove = gtk::Button::with_label("Remove");
        remove.set_valign(gtk::Align::Center);
        operation(
            &remove,
            body,
            device.clone(),
            changed.clone(),
            move |device| {
                let mut current = profiles::profile_set(device)?.rules;
                if current.get(index) != Some(&expected) {
                    anyhow::bail!("Rules changed; reopen the panel");
                }
                current.remove(index);
                profiles::set_rules(device, current)?;
                Ok("Rule removed.".into())
            },
        );
        row.add_suffix(&remove);
        rules.add(&row);
    }
    let app = adw::EntryRow::builder()
        .title("App ID for a new rule")
        .build();
    app.set_max_length(256);
    rules.add(&app);
    let priority = gtk::SpinButton::with_range(-1000.0, 1000.0, 1.0);
    let row = adw::ActionRow::builder()
        .title("Rule priority")
        .subtitle(format!(
            "New rule targets selected profile: {}",
            set.selected
        ))
        .build();
    row.add_suffix(&priority);
    rules.add(&row);
    let add = gtk::Button::with_label("Add rule for selected profile");
    let target = set.selected.clone();
    operation(&add, body, device.clone(), changed.clone(), move |device| {
        let mut current = profiles::profile_set(device)?.rules;
        current.push(AppRule {
            app_id: app.text().into(),
            profile: target.clone(),
            priority: priority.value_as_int(),
            enabled: true,
        });
        profiles::set_rules(device, current)?;
        Ok("Rule added.".into())
    });
    rules.add(&add);
    body.append(&rules);

    let restore = adw::PreferencesGroup::builder().title("Automatic restore policy")
        .description(format!("Editing '{}', not an app override. Saved values come from Apply. Host switches and manual button diversion are never restored.", set.selected)).build();
    let profile = &set.profiles[&set.selected];
    let enabled = adw::SwitchRow::builder()
        .title("Restore saved settings on connection / profile change")
        .active(profile.restore)
        .build();
    restore.add(&enabled);
    let mut switches = Vec::new();
    for setting in RestoreSetting::ALL {
        let row = adw::SwitchRow::builder()
            .title(setting.label())
            .subtitle("On: restore this setting; off: ignore it")
            .active(!profile.restore_ignore.contains(&setting))
            .build();
        restore.add(&row);
        if setting == RestoreSetting::ScrollDiversion {
            row.set_active(false);
            row.set_sensitive(false);
            row.set_subtitle("Manual wheel diversion is never restored automatically");
        }
        switches.push((setting, row));
    }
    let save_policy = gtk::Button::with_label("Save restore policy");
    let target = set.selected.clone();
    operation(
        &save_policy,
        body,
        device.clone(),
        changed.clone(),
        move |device| {
            let ignore: BTreeSet<_> = switches
                .iter()
                .filter(|(_, row)| !row.is_active())
                .map(|(s, _)| *s)
                .collect();
            profiles::set_restore_policy(device, &target, enabled.is_active(), ignore)?;
            Ok("Restore policy saved; Logi Center will apply enabled policies.".into())
        },
    );
    restore.add(&save_policy);
    body.append(&restore);
    transfer_panel(body, device, changed);
}

fn transfer_panel(body: &gtk::Box, device: Rc<DeviceSnapshot>, changed: Changed) {
    let group = adw::PreferencesGroup::builder().title("Import / export")
        .description("Paste bounded JSON or Solaar YAML below. Preview, select profiles, then confirm the target. Import adds profiles without replacing originals; actions and restore remain disabled.").build();
    let text = gtk::TextView::builder()
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .build();
    let buffer = text.buffer();
    buffer.connect_changed(|buffer| {
        // Character limit is an additional UI bound; the parser enforces bytes.
        if buffer.char_count() > profiles::MAX_IMPORT_BYTES as i32 {
            let mut start = buffer.iter_at_offset(profiles::MAX_IMPORT_BYTES as i32);
            let mut end = buffer.end_iter();
            buffer.delete(&mut start, &mut end);
        }
    });
    let scroll = gtk::ScrolledWindow::builder()
        .min_content_height(160)
        .child(&text)
        .build();
    group.add(&scroll);
    let format = gtk::DropDown::from_strings(&["Versioned JSON", "Legacy Solaar YAML"]);
    group.add(&format);
    let export = gtk::Button::with_label("Export JSON into text field");
    let (dev, buf, weak) = (device.clone(), buffer.clone(), body.downgrade());
    export.connect_clicked(move |_| match profiles::export_json(&dev) {
        Ok(json) => buf.set_text(&json),
        Err(error) => {
            if let Some(body) = weak.upgrade() {
                message(&body, &error.to_string());
            }
        }
    });
    group.add(&export);
    let preview_button = gtk::Button::with_label("Preview import");
    group.add(&preview_button);
    let preview_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let weak_preview = preview_box.downgrade();
    buffer.connect_changed(move |_| {
        if let Some(preview) = weak_preview.upgrade() {
            while let Some(child) = preview.first_child() {
                preview.remove(&child);
            }
        }
    });
    let weak_preview = preview_box.downgrade();
    format.connect_selected_notify(move |_| {
        if let Some(preview) = weak_preview.upgrade() {
            while let Some(child) = preview.first_child() {
                preview.remove(&child);
            }
        }
    });
    group.add(&preview_box);
    let weak = body.downgrade();
    preview_button.connect_clicked(move |_| {
        while let Some(child) = preview_box.first_child() {
            preview_box.remove(&child);
        }
        let data = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
        let result = if format.selected() == 0 {
            profiles::preview_json(&device, data.as_bytes())
        } else {
            profiles::preview_solaar(&device, data.as_bytes())
        };
        match result {
            Err(error) => message(&preview_box, &format!("Preview rejected: {error:#}")),
            Ok(preview) => {
                for warning in &preview.warnings {
                    message(&preview_box, warning);
                }
                if !preview.rules.is_empty() {
                    let rules = adw::ExpanderRow::builder()
                        .title("Review associated application rules")
                        .build();
                    for rule in &preview.rules {
                        let label = gtk::Label::new(Some(&format!(
                            "{} → {} · priority {} · {}",
                            rule.app_id,
                            rule.profile,
                            rule.priority,
                            if rule.enabled { "enabled" } else { "disabled" }
                        )));
                        label.set_wrap(true);
                        label.set_xalign(0.0);
                        rules.add_row(&label);
                    }
                    preview_box.append(&rules);
                }
                let checks: Vec<_> = preview
                    .entries
                    .iter()
                    .map(|entry| {
                        let row = gtk::CheckButton::with_label(&format!(
                            "{} · {} · serial {} · {}",
                            entry.name,
                            entry.device_name,
                            entry.source_device,
                            if entry.matches_device {
                                "exact match"
                            } else {
                                "different / unknown device"
                            }
                        ));
                        preview_box.append(&row);
                        let details = adw::ExpanderRow::builder()
                            .title("Review sanitized settings and actions")
                            .build();
                        let label = gtk::Label::new(Some(&entry.details));
                        label.set_selectable(true);
                        label.set_wrap(true);
                        label.set_xalign(0.0);
                        label.add_css_class("monospace");
                        details.add_row(&label);
                        preview_box.append(&details);
                        row
                    })
                    .collect();
                let confirm = gtk::CheckButton::with_label(&format!(
                    "I explicitly map selected source profiles to {} ({})",
                    device.name, device.serial
                ));
                preview_box.append(&confirm);
                let import = gtk::Button::with_label("Import selected profiles");
                let (dev, callback, weak) = (device.clone(), changed.clone(), weak.clone());
                import.connect_clicked(move |_| {
                    let indices: Vec<_> = checks
                        .iter()
                        .enumerate()
                        .filter(|(_, check)| check.is_active())
                        .map(|(i, _)| i)
                        .collect();
                    if let Some(body) = weak.upgrade() {
                        match profiles::import_selected(
                            &dev,
                            &preview,
                            &indices,
                            confirm.is_active(),
                        ) {
                            Ok(names) => {
                                callback();
                                refresh(
                                    &body,
                                    dev.clone(),
                                    callback.clone(),
                                    &format!(
                                        "Imported {}. Review before enabling actions or restore.",
                                        names.join(", ")
                                    ),
                                );
                            }
                            Err(error) => message(&body, &format!("Import rejected: {error:#}")),
                        }
                    }
                });
                preview_box.append(&import);
            }
        }
    });
    body.append(&group);
}
