//! Optional adapters panel; GTK objects stay on the main thread, all I/O on workers.
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::peripherals::{
    self, Constraint, Control, ControlValue, DeskflowSession, Kind, Peripheral,
};

fn label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_wrap(true);
    label.set_xalign(0.0);
    label.set_selectable(true);
    label
}

/// A task is polled from the GTK loop without blocking it. Closing the associated
/// widget drops the receiver, so a late owned DeskflowSession is dropped/stopped.
fn work<T: Send + 'static>(
    owner: &impl IsA<gtk::Widget>,
    task: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(T) + 'static,
) {
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = tx.send(task());
    });
    let widget: &gtk::Widget = owner.as_ref();
    let weak = widget.downgrade();
    let mut done = Some(done);
    glib::timeout_add_local(Duration::from_millis(40), move || {
        let Some(owner) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if owner.root().is_none() {
            return glib::ControlFlow::Break;
        }
        match rx.try_recv() {
            Ok(value) => {
                if let Some(done) = done.take() {
                    done(value);
                }
                glib::ControlFlow::Break
            }
            Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
        }
    });
}

fn apply_button(
    device: &Peripheral,
    control: &Control,
    editor: &gtk::Widget,
    read: impl Fn() -> ControlValue + 'static,
    result: &gtk::Label,
) -> gtk::Button {
    let button = gtk::Button::with_label("Apply");
    button.add_css_class("pill");
    button.set_size_request(100, 42);
    button.set_valign(gtk::Align::Center);
    let device_id = device.id.clone();
    let control_id = control.id.clone();
    let constraint = control.constraint.clone();
    let result = result.clone();
    let editor = editor.clone();
    button.connect_clicked(move |button| {
        let value = read();
        if let Err(error) = constraint.validate(&value) {
            result.set_label(&error.to_string());
            return;
        }
        button.set_sensitive(false);
        editor.set_sensitive(false);
        result.set_label("Applying…");
        let (device_id, control_id) = (device_id.clone(), control_id.clone());
        let (button, editor, result) = (button.clone(), editor.clone(), result.clone());
        let owner = button.clone();
        work(
            &owner,
            move || peripherals::apply(&device_id, &control_id, value),
            move |reply| {
                button.set_sensitive(true);
                editor.set_sensitive(true);
                result.set_label(&reply.unwrap_or_else(|e| format!("Not applied: {e:#}")));
            },
        );
    });
    button
}

struct PendingControl {
    control: Control,
    editor: gtk::Widget,
}

thread_local! {
    static CAMERA_SESSIONS: RefCell<std::collections::HashSet<String>> = RefCell::default();
}

#[derive(Default)]
pub struct CameraForm {
    controls: RefCell<Vec<PendingControl>>,
    busy: Cell<bool>,
    queued: RefCell<std::collections::BTreeMap<String, ControlValue>>,
    generation: Cell<u64>,
    initializing: Cell<bool>,
}

impl CameraForm {
    pub fn has_pending_changes(&self) -> bool {
        self.busy.get() || !self.queued.borrow().is_empty()
    }
}

fn bind_control(
    row: &adw::ActionRow,
    device: &Peripheral,
    control: &Control,
    editor: &gtk::Widget,
    read: impl Fn() -> ControlValue + 'static,
    result: &gtk::Label,
    form: Option<&Rc<CameraForm>>,
) {
    if let Some(form) = form {
        form.controls.borrow_mut().push(PendingControl {
            control: control.clone(),
            editor: editor.clone(),
        });
    } else {
        row.add_suffix(&apply_button(device, control, editor, read, result));
    }
}

fn queue_camera_change(
    form: &Rc<CameraForm>,
    device: &Peripheral,
    control_id: &str,
    value: ControlValue,
    result: &gtk::Label,
) {
    if form.initializing.get() {
        return;
    }
    if let Err(error) = form
        .controls
        .borrow()
        .iter()
        .find(|control| control.control.id == control_id)
        .ok_or_else(|| anyhow::anyhow!("Camera control is unavailable"))
        .and_then(|control| control.control.constraint.validate(&value))
    {
        result.set_text(&format!("Not applied: {error:#}"));
        return;
    }
    form.queued.borrow_mut().insert(control_id.into(), value);
    let generation = form.generation.get().wrapping_add(1);
    form.generation.set(generation);
    let (form, device, result) = (form.clone(), device.clone(), result.clone());
    glib::timeout_add_local_once(Duration::from_millis(220), move || {
        if form.generation.get() == generation {
            flush_camera_changes(&form, &device, &result);
        }
    });
}

fn flush_camera_changes(form: &Rc<CameraForm>, device: &Peripheral, result: &gtk::Label) {
    if form.busy.get() || form.initializing.get() {
        return;
    }
    let changes = std::mem::take(&mut *form.queued.borrow_mut())
        .into_iter()
        .collect::<Vec<(String, ControlValue)>>();
    if changes.is_empty() {
        return;
    }
    form.busy.set(true);
    result.set_text("");
    let (form, device, result) = (form.clone(), device.clone(), result.clone());
    let worker_device = device.clone();
    let owner = result.clone();
    work(
        &owner,
        move || peripherals::apply_camera_changes(&worker_device, changes),
        move |reply| {
            form.busy.set(false);
            match reply {
                Err(error) => result.set_text(&format!("Live update failed: {error:#}")),
                Ok(results) => {
                    let mut errors = vec![];
                    for (id, outcome) in results {
                        match outcome {
                            Ok(_) => {}
                            Err(error) => errors.push(format!("{id}: {error}")),
                        }
                    }
                    if errors.is_empty() {
                        result.set_text("");
                    } else {
                        result.set_text(&format!(
                            "Some settings were not confirmed:\n{}",
                            errors.join("\n")
                        ));
                    }
                }
            }
            if !form.queued.borrow().is_empty() {
                flush_camera_changes(&form, &device, &result);
            }
        },
    );
}

fn restore_camera_settings(
    device: &Peripheral,
    form: &Rc<CameraForm>,
    owner: &gtk::Box,
    on_updated: Rc<dyn Fn()>,
) -> anyhow::Result<bool> {
    let first_open = CAMERA_SESSIONS.with(|sessions| {
        sessions
            .borrow_mut()
            .insert(peripherals::camera_key(device))
    });
    if !first_open || !peripherals::has_saved_camera_settings(device)? {
        return Ok(false);
    }
    form.busy.set(true);
    for control in form.controls.borrow().iter() {
        control.editor.set_sensitive(false);
    }
    let (form, device, owner) = (form.clone(), device.clone(), owner.clone());
    work(
        &owner,
        move || peripherals::apply_saved_camera_settings(&device),
        move |reply| {
            form.busy.set(false);
            for control in form.controls.borrow().iter() {
                control.editor.set_sensitive(true);
            }
            match reply {
                Err(error) => log::warn!("Saved camera settings were not restored: {error:#}"),
                Ok(results) => {
                    for (id, result) in results {
                        if let Err(error) = result {
                            log::warn!("Saved camera setting {id} was not restored: {error}");
                        }
                    }
                }
            }
            // Automatic modes can expose different manual controls. Rebuild from
            // a fresh scan rather than silently dropping those saved values.
            on_updated();
        },
    );
    Ok(true)
}

fn camera_save_footer(device: &Peripheral, form: &Rc<CameraForm>) -> gtk::Box {
    let footer = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let result = label("");
    result.set_visible(false);
    result.connect_label_notify(|label| label.set_visible(!label.text().is_empty()));
    footer.append(&result);
    let save = gtk::Button::with_label("Save settings");
    save.add_css_class("suggested-action");
    save.add_css_class("pill");
    save.set_size_request(160, 42);
    save.set_halign(gtk::Align::End);
    footer.append(&save);
    let (device, form) = (device.clone(), form.clone());
    save.connect_clicked(move |button| {
        if form.has_pending_changes() {
            result.set_text("Wait for the live camera update to finish before saving.");
            return;
        }
        form.busy.set(true);
        button.set_sensitive(false);
        result.set_text("Reading confirmed camera settings…");
        let (device, form, button, result) =
            (device.clone(), form.clone(), button.clone(), result.clone());
        let worker_device = device.clone();
        let owner = button.clone();
        work(
            &owner,
            move || peripherals::save_camera_settings(&worker_device),
            move |reply| {
                form.busy.set(false);
                button.set_sensitive(true);
                match reply {
                    Ok(preset) => result.set_text(&format!(
                        "Saved {} camera settings permanently for this camera.",
                        preset.values.len()
                    )),
                    Err(error) => result.set_text(&format!("Settings not saved: {error:#}")),
                }
                if !form.queued.borrow().is_empty() {
                    flush_camera_changes(&form, &device, &result);
                }
            },
        );
    });
    footer
}

fn control_row(device: &Peripheral, control: &Control, form: Option<&Rc<CameraForm>>) -> gtk::Box {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let row = adw::ActionRow::builder()
        .title(&control.label)
        .subtitle(&control.note)
        .build();
    row.set_use_markup(false);
    let result = label(if control.current.is_none() {
        "Current state unknown; live adjustment is explicit."
    } else {
        ""
    });
    result.add_css_class("dim-label");
    result.set_margin_start(12);
    result.set_margin_end(12);
    result.set_visible(!result.text().is_empty());
    result.connect_label_notify(|label| label.set_visible(!label.text().is_empty()));
    match &control.constraint {
        Constraint::Toggle => {
            let editor = gtk::Switch::new();
            editor.set_valign(gtk::Align::Center);
            editor.set_active(matches!(control.current, Some(ControlValue::Bool(true))));
            row.add_suffix(&editor);
            let input = editor.clone();
            bind_control(
                &row,
                device,
                control,
                editor.upcast_ref(),
                move || ControlValue::Bool(input.is_active()),
                &result,
                form,
            );
            if let Some(form) = form {
                let (form, device, control_id, result) = (
                    form.clone(),
                    device.clone(),
                    control.id.clone(),
                    result.clone(),
                );
                editor.connect_active_notify(move |input| {
                    queue_camera_change(
                        &form,
                        &device,
                        &control_id,
                        ControlValue::Bool(input.is_active()),
                        &result,
                    );
                });
            }
        }
        Constraint::Integer { choices, .. } if !choices.is_empty() => {
            let strings: Vec<_> = choices.iter().map(ToString::to_string).collect();
            let refs: Vec<_> = strings.iter().map(String::as_str).collect();
            let editor = gtk::DropDown::from_strings(&refs);
            editor.set_size_request(144, 42);
            editor.set_valign(gtk::Align::Center);
            if let Some(ControlValue::Integer(v)) = control.current.as_ref() {
                if let Some(index) = choices.iter().position(|n| n == v) {
                    editor.set_selected(index as u32);
                }
            }
            row.add_suffix(&editor);
            let (input, values) = (editor.clone(), choices.clone());
            bind_control(
                &row,
                device,
                control,
                editor.upcast_ref(),
                move || {
                    values
                        .get(input.selected() as usize)
                        .copied()
                        .map(ControlValue::Integer)
                        .unwrap_or(ControlValue::Integer(i64::MIN))
                },
                &result,
                form,
            );
            if let Some(form) = form {
                let (form, device, control_id, result, values) = (
                    form.clone(),
                    device.clone(),
                    control.id.clone(),
                    result.clone(),
                    choices.clone(),
                );
                editor.connect_selected_notify(move |input| {
                    if let Some(value) = values.get(input.selected() as usize) {
                        queue_camera_change(
                            &form,
                            &device,
                            &control_id,
                            ControlValue::Integer(*value),
                            &result,
                        );
                    }
                });
            }
        }
        Constraint::Integer { min, max, step, .. } => {
            let editor = gtk::Scale::with_range(
                gtk::Orientation::Horizontal,
                *min as f64,
                *max as f64,
                *step as f64,
            );
            editor.add_css_class("camera-slider");
            editor.set_size_request(280, 48);
            editor.set_valign(gtk::Align::Center);
            editor.set_digits(0);
            editor.set_round_digits(0);
            editor.set_draw_value(true);
            editor.set_value_pos(gtk::PositionType::Right);
            editor.set_value(match control.current.as_ref() {
                Some(ControlValue::Integer(v)) => *v as f64,
                _ => *min as f64,
            });
            let (minimum, increment) = (*min as f64, *step as f64);
            editor.connect_change_value(move |input, _, value| {
                let snapped = minimum + ((value - minimum) / increment).round() * increment;
                input.set_value(snapped);
                gtk::glib::Propagation::Stop
            });
            row.add_suffix(&editor);
            let input = editor.clone();
            bind_control(
                &row,
                device,
                control,
                editor.upcast_ref(),
                move || ControlValue::Integer(input.value().round() as i64),
                &result,
                form,
            );
            if let Some(form) = form {
                let (form, device, control_id, result) = (
                    form.clone(),
                    device.clone(),
                    control.id.clone(),
                    result.clone(),
                );
                editor.connect_value_changed(move |input| {
                    queue_camera_change(
                        &form,
                        &device,
                        &control_id,
                        ControlValue::Integer(input.value().round() as i64),
                        &result,
                    );
                });
            }
        }
        Constraint::Choice { options } => {
            let refs: Vec<_> = options.iter().map(String::as_str).collect();
            let editor = gtk::DropDown::from_strings(&refs);
            editor.set_size_request(144, 42);
            editor.set_valign(gtk::Align::Center);
            if let Some(ControlValue::Text(v)) = control.current.as_ref() {
                if let Some(index) = options.iter().position(|s| s == v) {
                    editor.set_selected(index as u32);
                }
            }
            row.add_suffix(&editor);
            let (input, values) = (editor.clone(), options.clone());
            bind_control(
                &row,
                device,
                control,
                editor.upcast_ref(),
                move || {
                    ControlValue::Text(
                        values
                            .get(input.selected() as usize)
                            .cloned()
                            .unwrap_or_default(),
                    )
                },
                &result,
                form,
            );
            if let Some(form) = form {
                let (form, device, control_id, result, values) = (
                    form.clone(),
                    device.clone(),
                    control.id.clone(),
                    result.clone(),
                    options.clone(),
                );
                editor.connect_selected_notify(move |input| {
                    if let Some(value) = values.get(input.selected() as usize) {
                        queue_camera_change(
                            &form,
                            &device,
                            &control_id,
                            ControlValue::Text(value.clone()),
                            &result,
                        );
                    }
                });
            }
        }
        Constraint::Rgb => {
            let editor = gtk::Entry::new();
            editor.set_size_request(144, 42);
            editor.set_valign(gtk::Align::Center);
            editor.set_width_chars(8);
            editor.set_max_length(6);
            editor.set_placeholder_text(Some("RRGGBB"));
            if let Some(ControlValue::Text(v)) = control.current.as_ref() {
                editor.set_text(v);
            }
            row.add_suffix(&editor);
            let input = editor.clone();
            bind_control(
                &row,
                device,
                control,
                editor.upcast_ref(),
                move || ControlValue::Text(input.text().to_string()),
                &result,
                form,
            );
        }
    }
    column.append(&row);
    column.append(&result);
    column
}

fn camera_presets(
    device: &Peripheral,
    form: &Rc<CameraForm>,
    on_updated: Rc<dyn Fn()>,
) -> gtk::Box {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
    column.append(&label("Camera presets · focus / exposure / white balance / zoom / pan / tilt. Explicit local save/load; no automatic camera changes. Enable manual modes, then refresh to expose their controls."));
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let name = gtk::Entry::new();
    name.set_placeholder_text(Some("Preset name (letters/digits/_/-)"));
    name.set_hexpand(true);
    name.set_max_length(64);
    let save = gtk::Button::with_label("Save current");
    let load = gtk::Button::with_label("Load & apply");
    for button in [&save, &load] {
        button.add_css_class("pill");
        button.set_size_request(130, 42);
    }
    name.set_size_request(-1, 42);
    row.append(&name);
    row.append(&save);
    row.append(&load);
    column.append(&row);
    let result = label("");
    column.append(&result);
    let save_form = form.clone();
    let load_form = form.clone();
    let (device_id, input, output, other, form) = (
        device.id.clone(),
        name.clone(),
        result.clone(),
        load.downgrade(),
        save_form,
    );
    save.connect_clicked(move |button| {
        let Some(other) = other.upgrade() else { return };
        if form.has_pending_changes() {
            output.set_label("Wait for the current camera operation to finish.");
            return;
        }
        form.busy.set(true);
        for control in form.controls.borrow().iter() {
            control.editor.set_sensitive(false);
        }
        let name = input.text().to_string();
        let device_id = device_id.clone();
        button.set_sensitive(false);
        other.set_sensitive(false);
        let (button, other, output, form) =
            (button.clone(), other.clone(), output.clone(), form.clone());
        let owner = button.clone();
        work(
            &owner,
            move || peripherals::save_camera_preset(&device_id, &name),
            move |reply| {
                form.busy.set(false);
                for control in form.controls.borrow().iter() {
                    control.editor.set_sensitive(true);
                }
                button.set_sensitive(true);
                other.set_sensitive(true);
                output.set_label(&match reply {
                    Ok(p) => format!("Saved {} values privately as {}.", p.values.len(), p.name),
                    Err(e) => format!("Not saved: {e:#}"),
                });
            },
        );
    });
    let (device_id, input, output, other, form) =
        (device.id.clone(), name, result, save.downgrade(), load_form);
    load.connect_clicked(move |button| {
        let Some(other) = other.upgrade() else { return };
        if form.has_pending_changes() {
            output.set_label("Wait for the current camera operation to finish.");
            return;
        }
        form.busy.set(true);
        for control in form.controls.borrow().iter() {
            control.editor.set_sensitive(false);
        }
        let (name, device_id) = (input.text().to_string(), device_id.clone());
        button.set_sensitive(false);
        other.set_sensitive(false);
        output.set_label("Applying preset…");
        let (button, other, output, form, on_updated) = (
            button.clone(),
            other.clone(),
            output.clone(),
            form.clone(),
            on_updated.clone(),
        );
        let owner = button.clone();
        work(
            &owner,
            move || peripherals::apply_camera_preset(&device_id, &name),
            move |reply| {
                form.busy.set(false);
                for control in form.controls.borrow().iter() {
                    control.editor.set_sensitive(true);
                }
                button.set_sensitive(true);
                other.set_sensitive(true);
                output.set_label(&match reply {
                    Ok(results) => results
                        .into_iter()
                        .map(|(name, result)| {
                            format!(
                                "{name}: {}",
                                result.unwrap_or_else(|e| format!("FAILED: {e}"))
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                    Err(e) => format!("Not loaded: {e:#}"),
                });
                on_updated();
            },
        );
    });
    column
}

fn render_inventory(container: &gtk::Box, inventory: peripherals::Inventory) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
    let dependencies = label("Optional backends");
    dependencies.add_css_class("title-3");
    container.append(&dependencies);
    for backend in inventory.backends {
        container.append(&label(&format!(
            "{} · {}\n{}",
            backend.backend,
            if backend.available {
                "available"
            } else {
                "unavailable"
            },
            backend.detail
        )));
    }
    if inventory.devices.is_empty() {
        container.append(&label("No supported peripherals discovered. Missing helpers are optional; install them yourself only if needed, then Refresh. No helper is installed automatically."));
    }
    for device in inventory.devices {
        container.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        append_device(container, &device);
    }
}

fn append_device(container: &gtk::Box, device: &Peripheral) {
    let title = label(&device.name);
    title.add_css_class("title-3");
    container.append(&title);
    container.append(&label(&format!(
        "{} · {}\n{}",
        device.backend, device.id, device.status
    )));
    if let Some(info) = device.metadata.get("info") {
        let details = gtk::Expander::new(Some("Device / capability information"));
        let text = label(info);
        text.add_css_class("monospace");
        details.set_child(Some(&text));
        container.append(&details);
    }
    let form = (device.kind == Kind::Camera).then(|| Rc::new(CameraForm::default()));
    for control in &device.controls {
        container.append(&control_row(device, control, form.as_ref()));
    }
    if let Some(form) = form.as_ref() {
        container.append(&camera_save_footer(device, form));
        if !device.controls.is_empty() {
            container.append(&camera_presets(device, form, Rc::new(|| {})));
        }
    }
}

/// The same real controls as the optional panel, embedded in the main window.
pub fn device_page(
    device: &Peripheral,
    preview: &gtk::Box,
    on_updated: Rc<dyn Fn()>,
) -> (gtk::ScrolledWindow, Rc<CameraForm>) {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 24);
    content.add_css_class("camera-settings-page");
    content.add_css_class("device-settings");
    content.set_margin_top(28);
    content.set_margin_bottom(40);
    content.set_margin_start(32);
    content.set_margin_end(32);
    let heading = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    heading.add_css_class("camera-hero");
    let icon = gtk::Image::from_icon_name("camera-web-symbolic");
    icon.set_pixel_size(30);
    icon.add_css_class("device-summary-icon");
    icon.set_halign(gtk::Align::Center);
    icon.set_valign(gtk::Align::Center);
    heading.append(&icon);
    let heading_copy = gtk::Box::new(gtk::Orientation::Vertical, 3);
    heading_copy.set_hexpand(true);
    heading_copy.set_valign(gtk::Align::Center);
    let eyebrow = label("CAMERA WORKSPACE");
    eyebrow.add_css_class("camera-eyebrow");
    let title = label(&device.name);
    title.add_css_class("device-name");
    let subtitle = label(&format!("{} · {}", device.backend, device.id));
    subtitle.add_css_class("camera-device-path");
    heading_copy.append(&eyebrow);
    heading_copy.append(&title);
    heading_copy.append(&subtitle);
    let status_needs_attention = [
        "unavailable",
        "permission",
        "denied",
        "failed",
        "error",
        "busy",
    ]
    .iter()
    .any(|needle| device.status.to_lowercase().contains(needle));
    if !device.status.is_empty() && status_needs_attention {
        let status = label(&device.status);
        status.add_css_class("camera-device-status");
        status.set_lines(2);
        status.set_ellipsize(gtk::pango::EllipsizeMode::End);
        status.set_tooltip_text(Some(&device.status));
        heading_copy.append(&status);
    }
    heading.append(&heading_copy);
    let badge = label("CAMERA");
    badge.add_css_class("status-pill");
    badge.add_css_class("good");
    badge.set_valign(gtk::Align::Center);
    heading.append(&badge);
    content.append(&heading);
    preview.add_css_class("camera-preview-card");
    preview.set_hexpand(true);
    content.append(preview);
    let settings = adw::PreferencesGroup::builder()
        .title("Camera settings")
        .description("Numeric controls update the camera live as you move their sliders. Use Save settings to keep the confirmed configuration.")
        .build();
    let form = Rc::new(CameraForm::default());
    form.initializing.set(true);
    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    card.add_css_class("camera-settings-card");
    let inset = gtk::Box::new(gtk::Orientation::Vertical, 16);
    inset.set_margin_top(16);
    inset.set_margin_bottom(16);
    inset.set_margin_start(16);
    inset.set_margin_end(16);
    let rows = gtk::ListBox::new();
    rows.set_selection_mode(gtk::SelectionMode::None);
    rows.add_css_class("boxed-list");
    for control in &device.controls {
        rows.append(&control_row(device, control, Some(&form)));
    }
    inset.append(&rows);
    inset.append(&camera_save_footer(device, &form));
    card.append(&inset);
    settings.add(&card);
    content.append(&settings);
    if !device.controls.is_empty() {
        let presets = camera_presets(device, &form, on_updated.clone());
        presets.add_css_class("camera-presets");
        content.append(&presets);
    }
    if device.controls.is_empty() {
        let notice = label(
            "No writable controls available. Check the camera status and permissions, then Refresh devices.",
        );
        notice.add_css_class("offline-notice");
        content.append(&notice);
    }
    let information = gtk::Expander::new(Some("Camera information"));
    information.add_css_class("device-information");
    information.set_child(Some(&label(&format!(
        "{} · {}\n{}",
        device.backend, device.id, device.status
    ))));
    content.append(&information);
    form.initializing.set(false);
    if let Err(error) = restore_camera_settings(device, &form, preview, on_updated) {
        let notice = label(&format!("Saved settings could not be restored: {error:#}"));
        notice.add_css_class("offline-notice");
        content.append(&notice);
    }
    let clamp = adw::Clamp::builder()
        .maximum_size(920)
        .tightening_threshold(680)
        .child(&content)
        .build();
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_child(Some(&clamp));
    scroll.set_vexpand(true);
    (scroll, form)
}

fn choose_file(parent: &adw::Window, entry: &gtk::Entry, executable: bool) -> gtk::Button {
    let button = gtk::Button::with_label("Choose…");
    let parent = parent.downgrade();
    let entry = entry.clone();
    button.connect_clicked(move |_| {
        let Some(parent) = parent.upgrade() else {
            return;
        };
        let chooser = gtk::FileChooserNative::new(
            Some(if executable {
                "Choose trusted Deskflow server executable"
            } else {
                "Choose trusted Deskflow server configuration"
            }),
            Some(&parent),
            gtk::FileChooserAction::Open,
            Some("Choose"),
            Some("Cancel"),
        );
        let entry = entry.clone();
        chooser.connect_response(move |chooser, response| {
            if response == gtk::ResponseType::Accept {
                if let Some(path) = chooser.file().and_then(|f| f.path()) {
                    entry.set_text(&path.to_string_lossy());
                }
            }
            chooser.destroy();
        });
        chooser.show();
    });
    button
}

fn deskflow_panel(window: &adw::Window) -> gtk::Box {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let title = label("Deskflow · trusted network keyboard/mouse sharing");
    title.add_css_class("title-3");
    column.append(&title);
    column.append(&label(peripherals::DESKFLOW_SECURITY));
    column.append(&label("Supports advertised classic server CLI (--config/--no-daemon/--enable-crypto) and modern coremode/--settings CLI. Modern settings must explicitly enable TLS and peer fingerprint checks. Configure layout, certificates, peer trust and input permissions in Deskflow first. Writable/untrusted files or parent directories are rejected. Deskflow itself may migrate its selected settings file."));
    let executable = gtk::Entry::new();
    executable.set_placeholder_text(Some(
        "Absolute path to trusted deskflow-server / compatible deskflow-core",
    ));
    executable.set_hexpand(true);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.append(&executable);
    row.append(&choose_file(window, &executable, true));
    column.append(&row);
    let configuration = gtk::Entry::new();
    configuration.set_placeholder_text(Some("Absolute path to trusted Deskflow server config"));
    configuration.set_hexpand(true);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.append(&configuration);
    row.append(&choose_file(window, &configuration, false));
    column.append(&row);
    let acknowledgement = gtk::CheckButton::with_label(
        "I trust these files and understand the network/TLS/peer-verification requirements.",
    );
    column.append(&acknowledgement);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let start = gtk::Button::with_label("Start server");
    let stop = gtk::Button::with_label("Stop owned server");
    stop.set_sensitive(false);
    row.append(&start);
    row.append(&stop);
    column.append(&row);
    let status = label(
        "Stopped. Existing externally launched Deskflow processes are not managed or terminated here.",
    );
    column.append(&status);
    let session: Rc<RefCell<Option<DeskflowSession>>> = Rc::new(RefCell::new(None));
    let (state, output, stop_button) = (session.clone(), status.clone(), stop.downgrade());
    start.connect_clicked(move |button| {
        let Some(stop_button) = stop_button.upgrade() else {
            return;
        };
        if state.borrow().is_some() {
            return;
        }
        let config = peripherals::DeskflowConfig {
            executable: PathBuf::from(executable.text().as_str()),
            configuration: PathBuf::from(configuration.text().as_str()),
            security_acknowledged: acknowledgement.is_active(),
        };
        button.set_sensitive(false);
        output.set_label("Validating executable, CLI, configuration and security acknowledgement…");
        let (state, output, button, stop_button) = (
            state.clone(),
            output.clone(),
            button.clone(),
            stop_button.clone(),
        );
        let owner = button.clone();
        work(
            &owner,
            move || peripherals::start_deskflow(&config),
            move |reply| match reply {
                Ok(mut new_session) => {
                    output.set_label(
                        &new_session
                            .status()
                            .map(|s| s.detail)
                            .unwrap_or_else(|e| e.to_string()),
                    );
                    state.replace(Some(new_session));
                    stop_button.set_sensitive(true);
                }
                Err(error) => {
                    output.set_label(&format!("Not started: {error:#}"));
                    button.set_sensitive(true);
                }
            },
        );
    });
    let (state, output, start_button) = (session.clone(), status.clone(), start.downgrade());
    stop.connect_clicked(move |button| {
        let Some(start_button) = start_button.upgrade() else {
            return;
        };
        // Reaping a terminated owned process runs on a worker, never the GTK loop.
        let owned = state.borrow_mut().take();
        button.set_sensitive(false);
        output.set_label("Stopping owned server…");
        let (output, start_button) = (output.clone(), start_button.clone());
        let owner = button.clone();
        work(
            &owner,
            move || drop(owned),
            move |_| {
                output.set_label("Stopped owned server.");
                start_button.set_sensitive(true);
            },
        );
    });
    let weak = column.downgrade();
    let (state, output, start_button, stop_button) = (session.clone(), status, start, stop);
    glib::timeout_add_local(Duration::from_secs(1), move || {
        let Some(column) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if column.root().is_none() {
            return glib::ControlFlow::Break;
        }
        let result = state.borrow_mut().as_mut().map(DeskflowSession::status);
        if let Some(result) = result {
            match result {
                Ok(status) => {
                    output.set_label(&status.detail);
                    if !status.running {
                        state.borrow_mut().take();
                        start_button.set_sensitive(true);
                        stop_button.set_sensitive(false);
                    }
                }
                Err(e) => output.set_label(&format!("Status error: {e}")),
            }
        }
        glib::ControlFlow::Continue
    });
    window.connect_close_request(move |_| {
        let owned = session.borrow_mut().take();
        std::thread::spawn(move || drop(owned));
        glib::Propagation::Proceed
    });
    column
}

/// Standalone reachable panel; the coordinator only needs to call this function.
pub fn show(parent: &adw::ApplicationWindow) {
    let window = adw::Window::builder()
        .title("Optional peripherals & Deskflow")
        .transient_for(parent)
        .modal(false)
        .default_width(860)
        .default_height(760)
        .build();
    let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let header = adw::HeaderBar::new();
    let refresh = gtk::Button::with_label("Refresh");
    header.pack_end(&refresh);
    page.append(&header);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 16);
    for side in [
        gtk::PositionType::Top,
        gtk::PositionType::Bottom,
        gtk::PositionType::Left,
        gtk::PositionType::Right,
    ] {
        match side {
            gtk::PositionType::Top => content.set_margin_top(18),
            gtk::PositionType::Bottom => content.set_margin_bottom(18),
            gtk::PositionType::Left => content.set_margin_start(18),
            _ => content.set_margin_end(18),
        }
    }
    let inventory = gtk::Box::new(gtk::Orientation::Vertical, 12);
    inventory.append(&label("Discovering optional devices…"));
    content.append(&inventory);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&deskflow_panel(&window));
    scroll.set_child(Some(&content));
    page.append(&scroll);
    window.set_content(Some(&page));
    let inventory_copy = inventory.clone();
    refresh.connect_clicked(move |button| {
        button.set_sensitive(false);
        let (button, inventory) = (button.clone(), inventory_copy.clone());
        let owner = button.clone();
        work(&owner, peripherals::scan, move |result| {
            render_inventory(&inventory, result);
            button.set_sensitive(true);
        });
    });
    window.present();
    refresh.emit_clicked();
}

#[cfg(test)]
pub(crate) fn exercise_fixture(parent: &adw::ApplicationWindow) {
    // Deserialized descriptors deliberately have no write routes; this tests
    // rendering only and cannot invoke discovery or a real hardware operation.
    let inventory: peripherals::Inventory = serde_json::from_value(serde_json::json!({
        "backends": [{"backend":"fixture","available":true,"detail":"No hardware"}],
        "devices": [{"id":"camera:/dev/fixture","name":"Fixture camera","kind":"camera",
            "backend":"fixture","status":"No I/O","metadata":{},"controls":[
                {"id":"auto_focus","label":"Auto focus","constraint":{"type":"toggle"},"current":false,"note":""},
                {"id":"focus","label":"Focus","constraint":{"type":"integer","min":0,"max":100,"step":5,"choices":[]},"current":20,"note":""},
                {"id":"mode","label":"Mode","constraint":{"type":"choice","options":["manual","auto"]},"current":"manual","note":""},
                {"id":"dpi","label":"DPI","constraint":{"type":"integer","min":400,"max":1600,"step":1,"choices":[400,800,1600]},"current":800,"note":""},
                {"id":"rgb","label":"RGB","constraint":{"type":"rgb"},"current":"abcdef","note":""}
            ]}]
    })).unwrap();
    let window = adw::Window::builder()
        .transient_for(parent)
        .title("Peripheral fixture")
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    render_inventory(&content, inventory);
    content.append(&deskflow_panel(&window));
    window.set_content(Some(&content));
    window.present();
    window.close();
    window.set_content(None::<&gtk::Widget>);
}
