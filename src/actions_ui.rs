use crate::model::DeviceSnapshot;
use crate::profiles::{self, Action, Binding, KeyChord};
use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

pub fn show(
    parent: &adw::ApplicationWindow,
    device: DeviceSnapshot,
    status: String,
    on_saved: impl Fn() + 'static,
) -> Option<adw::Dialog> {
    let buttons: Vec<_> = device
        .buttons
        .iter()
        .filter(|button| button.divertable)
        .cloned()
        .collect();
    if buttons.is_empty() {
        return None;
    }
    let profile = profiles::load(&device);
    let target_profile = profiles::selection_status(&device)
        .map(|s| s.effective)
        .map_err(|e| e.to_string());
    let draft = Rc::new(RefCell::new(profile.bindings));
    let dialog = adw::Dialog::builder()
        .title("Shortcuts and gestures")
        .content_width(650)
        .content_height(800)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let page = adw::PreferencesPage::new();
    page.add_css_class("device-settings");
    page.add_css_class("actions-page");
    let general = adw::PreferencesGroup::builder().title(&device.name)
        .description("Optional. Input actions need /dev/uinput. Saving enables only when you explicitly turn the switch on. Launch steps run trusted local programs directly, never through a shell. Review imported workflows before enabling.").build();
    let enabled = adw::SwitchRow::builder()
        .title("Enable custom actions")
        .subtitle("Allow configured shortcuts, gestures and workflows")
        .active(profile.actions_enabled)
        .build();
    general.add(&enabled);
    let names: Vec<_> = buttons
        .iter()
        .enumerate()
        .map(|(i, b)| format!("{} · {} (CID {})", i + 1, b.name, b.cid))
        .collect();
    let refs: Vec<_> = names.iter().map(String::as_str).collect();
    let selected = adw::ComboRow::builder()
        .title("Button")
        .model(&gtk::StringList::new(&refs))
        .build();
    general.add(&selected);
    page.add(&general);
    let diagram_group = adw::PreferencesGroup::builder()
        .title("Button map")
        .description("Choose a numbered control to edit its actions. Positions are illustrative; the Button list contains every discovered control.")
        .build();
    diagram_group.add(&mouse_diagram(&buttons, &selected));
    page.add(&diagram_group);
    let form = Rc::new(BindingEditor::new(&dialog, &page));
    let current = Rc::new(Cell::new(0usize));
    form.load(
        &draft
            .borrow()
            .get(&buttons[0].cid)
            .cloned()
            .unwrap_or_default(),
        buttons[0].raw_xy,
    );
    let error = gtk::Label::new(Some(&status));
    error.set_wrap(true);
    error.set_xalign(0.0);
    error.add_css_class("caption");
    let footer = adw::PreferencesGroup::new();
    footer.add_css_class("actions-footer");
    footer.add(&error);
    let save = gtk::Button::with_label("Save custom actions");
    save.add_css_class("suggested-action");
    save.add_css_class("device-apply");
    save.set_halign(gtk::Align::End);
    footer.add(&save);
    page.add(&footer);
    toolbar.set_content(Some(&page));
    dialog.set_child(Some(&toolbar));
    let switching = Rc::new(Cell::new(false));
    {
        let form = form.clone();
        let draft = draft.clone();
        let current = current.clone();
        let buttons = buttons.clone();
        let error = error.downgrade();
        selected.connect_selected_notify(move |row| {
            if switching.get() {
                return;
            }
            let next = row.selected() as usize;
            if next >= buttons.len() {
                return;
            }
            match form.read() {
                Ok(binding) => {
                    draft
                        .borrow_mut()
                        .insert(buttons[current.get()].cid, binding);
                    current.set(next);
                    form.load(
                        &draft
                            .borrow()
                            .get(&buttons[next].cid)
                            .cloned()
                            .unwrap_or_default(),
                        buttons[next].raw_xy,
                    );
                    if let Some(error) = error.upgrade() {
                        error.set_text("");
                    }
                }
                Err(message) => {
                    if let Some(error) = error.upgrade() {
                        error.set_text(&format!("Fix this button before switching: {message:#}"));
                    }
                    switching.set(true);
                    row.set_selected(current.get() as u32);
                    switching.set(false);
                }
            }
        });
    }
    let weak = dialog.downgrade();
    save.connect_clicked(move |_| {
        let result = (|| -> anyhow::Result<()> {
            draft
                .borrow_mut()
                .insert(buttons[current.get()].cid, form.read()?);
            for binding in draft.borrow().values() {
                for action in [&binding.tap, &binding.hold, &binding.double_tap]
                    .into_iter()
                    .flatten()
                {
                    crate::automation::validate_action(action)?;
                }
            }
            let name = target_profile
                .as_ref()
                .map_err(|e| anyhow::anyhow!("Could not load target profile: {e}"))?;
            profiles::save_actions_to(&device, name, enabled.is_active(), draft.borrow().clone())
        })();
        match result {
            Ok(()) => {
                on_saved();
                if let Some(dialog) = weak.upgrade() {
                    dialog.close();
                }
            }
            Err(message) => {
                error.set_text(&format!("{message:#}"));
                error.add_css_class("error");
            }
        }
    });
    crate::i18n::translate_widget(dialog.upcast_ref());
    dialog.present(Some(parent));
    Some(dialog)
}

fn mouse_diagram(buttons: &[crate::model::ButtonInfo], selected: &adw::ComboRow) -> gtk::Box {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 4);
    container.add_css_class("action-diagram");
    let overlay = gtk::Overlay::new();
    overlay.set_halign(gtk::Align::Center);
    let drawing = gtk::DrawingArea::builder()
        .content_width(360)
        .content_height(220)
        .build();
    drawing.set_draw_func(|area, cr, _, _| {
        let color = area.style_context().color();
        let (red, green, blue) = (
            f64::from(color.red()),
            f64::from(color.green()),
            f64::from(color.blue()),
        );
        cr.set_source_rgba(red, green, blue, 0.055);
        cr.move_to(110.0, 90.0);
        cr.curve_to(110.0, 5.0, 250.0, 5.0, 250.0, 90.0);
        cr.line_to(250.0, 135.0);
        cr.curve_to(250.0, 220.0, 110.0, 220.0, 110.0, 135.0);
        cr.close_path();
        let _ = cr.fill_preserve();
        cr.set_source_rgba(red, green, blue, 0.28);
        cr.set_line_width(1.5);
        let _ = cr.stroke();
        cr.move_to(180.0, 18.0);
        cr.line_to(180.0, 104.0);
        let _ = cr.stroke();
        cr.set_source_rgba(red, green, blue, 0.12);
        cr.rectangle(172.0, 62.0, 16.0, 36.0);
        let _ = cr.fill_preserve();
        cr.set_source_rgba(red, green, blue, 0.32);
        let _ = cr.stroke();
    });
    overlay.set_child(Some(&drawing));
    let hotspots = gtk::Fixed::new();
    let positions = [
        (125., 35.),
        (200., 35.),
        (165., 105.),
        (65., 105.),
        (255., 105.),
        (125., 160.),
        (200., 160.),
        (165., 4.),
    ];
    let mut hotspot_widgets = Vec::new();
    for (i, (button, (x, y))) in buttons.iter().zip(positions).enumerate() {
        let hotspot = gtk::Button::with_label(&(i + 1).to_string());
        hotspot.add_css_class("mouse-hotspot");
        if i == 0 {
            hotspot.add_css_class("selected");
        }
        hotspot.set_tooltip_text(Some(&format!("{} (CID {})", button.name, button.cid)));
        let selected = selected.downgrade();
        hotspot.connect_clicked(move |_| {
            if let Some(row) = selected.upgrade() {
                row.set_selected(i as u32);
            }
        });
        hotspots.put(&hotspot, x, y);
        hotspot_widgets.push(hotspot);
    }
    selected.connect_selected_notify(move |row| {
        for (index, hotspot) in hotspot_widgets.iter().enumerate() {
            if index == row.selected() as usize {
                hotspot.add_css_class("selected");
            } else {
                hotspot.remove_css_class("selected");
            }
        }
    });
    overlay.add_overlay(&hotspots);
    container.append(&overlay);
    container
}

#[derive(Clone)]
struct ActionEditor {
    choice: adw::ComboRow,
    shortcut_row: adw::ActionRow,
    record: gtk::Button,
    shortcut: Rc<RefCell<KeyChord>>,
    json: gtk::TextView,
    json_box: gtk::Box,
    presets_row: adw::ActionRow,
    host_row: adw::SpinRow,
    loading: Rc<Cell<bool>>,
}
impl ActionEditor {
    fn new(parent: &adw::Dialog, page: &adw::PreferencesPage, title: &str) -> Self {
        let group = adw::PreferencesGroup::builder().title(title).build();
        let choice = adw::ComboRow::builder()
            .title("Action")
            .model(&gtk::StringList::new(&[
                "No action",
                "Keyboard shortcut",
                "Cycle DPI presets",
                "Workflow",
                "Switch host",
                "Actions ring",
            ]))
            .build();
        group.add(&choice);
        let record = gtk::Button::with_label("Record shortcut");
        record.set_valign(gtk::Align::Center);
        let shortcut_row = adw::ActionRow::builder().title("Shortcut").build();
        shortcut_row.add_suffix(&record);
        group.add(&shortcut_row);
        let host_row = adw::SpinRow::with_range(1., 3., 1.);
        host_row.set_title("Host slot (1-based)");
        group.add(&host_row);
        let presets = crate::workflow::templates();
        let names: Vec<_> = presets.iter().map(|w| w.name.as_str()).collect();
        let preset = gtk::DropDown::from_strings(&names);
        let use_preset = gtk::Button::with_label("Use template");
        use_preset.set_valign(gtk::Align::Center);
        let presets_row = adw::ActionRow::builder()
            .title("Workflow templates")
            .build();
        presets_row.add_suffix(&preset);
        presets_row.add_suffix(&use_preset);
        group.add(&presets_row);
        let json_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let hint = gtk::Label::new(Some(
            "Validated JSON: workflow {name,steps}, or ring [[label,Action],…].\nSteps: Shortcut, Delay(ms), Media, OpenUri, Launch {program,args,timeout_ms}.\nOpenUri opens a document/web address in its default desktop application.\nLimits: 64 steps, 30 seconds, 32 KiB; Launch runs a bounded command, not a persistent application.\nLaunch is supervised, not a sandbox for untrusted programs.",
        ));
        hint.set_wrap(true);
        json_box.append(&hint);
        let json = gtk::TextView::builder()
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(8)
            .right_margin(8)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .min_content_height(180)
            .max_content_height(260)
            .propagate_natural_height(true)
            .child(&json)
            .build();
        json_box.append(&scroll);
        json_box.append(&workflow_step_rows(&json, &choice));
        let validate = gtk::Button::with_label("Validate JSON");
        json_box.append(&validate);
        let validation = gtk::Label::new(None);
        validation.set_wrap(true);
        json_box.append(&validation);
        group.add(&json_box);
        page.add(&group);
        let editor = Self {
            choice,
            shortcut_row,
            record,
            shortcut: Rc::new(RefCell::new(KeyChord::default())),
            json,
            json_box,
            presets_row,
            host_row,
            loading: Rc::new(Cell::new(false)),
        };
        // Signal handlers hold only weak widget references, preventing dialog
        // cycles. The state cells contain data, never owning GTK widgets.
        let weak = editor.weak();
        editor.choice.connect_selected_notify(move |_| {
            if let Some(editor) = weak.upgrade() {
                if !editor.loading.get() {
                    editor.fill_default();
                }
                editor.visibility();
            }
        });
        let weak = editor.weak();
        let parent = parent.downgrade();
        editor.record.connect_clicked(move |_| {
            if let (Some(editor), Some(parent)) = (weak.upgrade(), parent.upgrade()) {
                let weak = editor.weak();
                record_shortcut(&parent, move |chord| {
                    if let Some(editor) = weak.upgrade() {
                        editor.record.set_label(&chord.label);
                        *editor.shortcut.borrow_mut() = chord;
                    }
                });
            }
        });
        let json = editor.json.downgrade();
        use_preset.connect_clicked(move |_| {
            if let Some(json) = json.upgrade() {
                if let Some(flow) = presets.get(preset.selected() as usize) {
                    json.buffer()
                        .set_text(&serde_json::to_string_pretty(flow).unwrap());
                }
            }
        });
        let weak = editor.weak();
        let validation = validation.downgrade();
        validate.connect_clicked(move |_| {
            if let (Some(editor), Some(label)) = (weak.upgrade(), validation.upgrade()) {
                label.set_text(&match editor.read() {
                    Ok(_) => "Valid; nothing executed".into(),
                    Err(error) => format!("{error:#}"),
                });
            }
        });
        editor.visibility();
        editor
    }
    fn visibility(&self) {
        let choice = self.choice.selected();
        self.shortcut_row.set_visible(choice == 1);
        self.host_row.set_visible(choice == 4);
        self.json_box.set_visible(choice == 3 || choice == 5);
        self.presets_row.set_visible(choice == 3);
    }
    fn fill_default(&self) {
        match self.choice.selected() {
            3 => self
                .json
                .buffer()
                .set_text(&serde_json::to_string_pretty(&crate::workflow::templates()[0]).unwrap()),
            5 => self.json.buffer().set_text(
                &serde_json::to_string_pretty(&vec![
                    (
                        "Copy",
                        Action::Shortcut(KeyChord {
                            keys: vec![29, 46],
                            label: "Ctrl+C".into(),
                        }),
                    ),
                    (
                        "Paste",
                        Action::Shortcut(KeyChord {
                            keys: vec![29, 47],
                            label: "Ctrl+V".into(),
                        }),
                    ),
                ])
                .unwrap(),
            ),
            _ => {}
        }
    }
    fn load(&self, value: &Option<Action>) {
        self.loading.set(true);
        let choice = match value {
            None => 0,
            Some(Action::Shortcut(chord)) => {
                *self.shortcut.borrow_mut() = chord.clone();
                self.record.set_label(&chord.label);
                1
            }
            Some(Action::CycleDpi) => 2,
            Some(Action::Workflow(flow)) => {
                self.json
                    .buffer()
                    .set_text(&serde_json::to_string_pretty(flow).unwrap());
                3
            }
            Some(Action::SwitchHost(host)) => {
                self.host_row.set_value(f64::from(*host));
                4
            }
            Some(Action::Ring(items)) => {
                self.json
                    .buffer()
                    .set_text(&serde_json::to_string_pretty(items).unwrap());
                5
            }
        };
        if choice != 1 {
            *self.shortcut.borrow_mut() = KeyChord::default();
            self.record.set_label("Record shortcut");
        }
        self.choice.set_selected(choice);
        self.loading.set(false);
        self.visibility();
    }
    fn read(&self) -> anyhow::Result<Option<Action>> {
        let buffer = self.json.buffer();
        let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
        let limit = if self.choice.selected() == 5 {
            262_144
        } else {
            32_768
        };
        if matches!(self.choice.selected(), 3 | 5) && text.len() > limit {
            anyhow::bail!("JSON editor exceeds its action payload limit");
        }
        let action = match self.choice.selected() {
            1 => Some(Action::Shortcut(self.shortcut.borrow().clone())),
            2 => Some(Action::CycleDpi),
            3 => Some(Action::Workflow(serde_json::from_str(&text)?)),
            4 => Some(Action::SwitchHost(self.host_row.value() as u8)),
            5 => Some(Action::Ring(serde_json::from_str(&text)?)),
            _ => None,
        };
        if let Some(action) = &action {
            crate::automation::validate_action(action)?;
        }
        Ok(action)
    }
    fn weak(&self) -> WeakActionEditor {
        WeakActionEditor {
            choice: self.choice.downgrade(),
            shortcut_row: self.shortcut_row.downgrade(),
            record: self.record.downgrade(),
            shortcut: self.shortcut.clone(),
            json: self.json.downgrade(),
            json_box: self.json_box.downgrade(),
            presets_row: self.presets_row.downgrade(),
            host_row: self.host_row.downgrade(),
            loading: self.loading.clone(),
        }
    }
}

/// Visual step management supplements the JSON fields (including literal argv).
/// Every button updates the same buffer used by validation and save.
fn workflow_step_rows(json: &gtk::TextView, choice: &adw::ComboRow) -> gtk::Box {
    use crate::workflow::{MediaAction, Step, Workflow};
    fn edit(json: &glib::WeakRef<gtk::TextView>, update: impl FnOnce(&mut Vec<Step>)) {
        let Some(json) = json.upgrade() else {
            return;
        };
        let buffer = json.buffer();
        let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
        if text.len() > 32_768 {
            return;
        }
        if let Ok(mut workflow) = serde_json::from_str::<Workflow>(&text) {
            update(&mut workflow.steps);
            buffer.set_text(&serde_json::to_string_pretty(&workflow).unwrap());
        }
    }
    let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let heading = gtk::Label::new(Some(
        "Workflow steps — edit fields in JSON; reorder/remove here",
    ));
    heading.set_wrap(true);
    root.append(&heading);
    let rows = gtk::Box::new(gtk::Orientation::Vertical, 4);
    root.append(&rows);
    let adders = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    for (title, step) in [
        ("Add delay", Step::Delay(150)),
        ("Add media", Step::Media(MediaAction::PlayPause)),
        (
            "Add location",
            Step::OpenUri("https://www.logitech.com/".into()),
        ),
    ] {
        let button = gtk::Button::with_label(title);
        let weak = json.downgrade();
        button.connect_clicked(move |_| {
            edit(&weak, |steps| {
                if steps.len() < crate::workflow::MAX_STEPS {
                    steps.push(step.clone());
                }
            })
        });
        adders.append(&button);
    }
    root.append(&adders);
    let weak_rows = rows.downgrade();
    let weak_json = json.downgrade();
    let weak_choice = choice.downgrade();
    let weak_root = root.downgrade();
    let weak_adders = adders.downgrade();
    let refresh: Rc<dyn Fn()> = Rc::new(move || {
        let (Some(rows), Some(json), Some(choice), Some(root), Some(adders)) = (
            weak_rows.upgrade(),
            weak_json.upgrade(),
            weak_choice.upgrade(),
            weak_root.upgrade(),
            weak_adders.upgrade(),
        ) else {
            return;
        };
        root.set_visible(choice.selected() == 3);
        while let Some(child) = rows.first_child() {
            rows.remove(&child);
        }
        if choice.selected() != 3 {
            return;
        }
        let buffer = json.buffer();
        let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
        if text.len() > 32_768 {
            adders.set_sensitive(false);
            return;
        }
        let Ok(workflow) = serde_json::from_str::<Workflow>(&text) else {
            adders.set_sensitive(false);
            return;
        };
        adders.set_sensitive(workflow.steps.len() < crate::workflow::MAX_STEPS);
        for (index, step) in workflow
            .steps
            .iter()
            .take(crate::workflow::MAX_STEPS)
            .enumerate()
        {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let summary = match step {
                Step::Shortcut(chord) => format!("{}. Shortcut: {}", index + 1, chord.label),
                Step::Delay(ms) => format!("{}. Delay: {ms} ms", index + 1),
                Step::Media(media) => format!("{}. Media: {media:?}", index + 1),
                Step::OpenUri(uri) => format!("{}. Open location: {uri}", index + 1),
                Step::Launch {
                    program,
                    args,
                    timeout_ms,
                } => format!(
                    "{}. Launch: {program} ({} args, {timeout_ms} ms)",
                    index + 1,
                    args.len()
                ),
            };
            let label = gtk::Label::new(Some(&summary));
            label.set_wrap(true);
            label.set_hexpand(true);
            label.set_xalign(0.);
            row.append(&label);
            for (title, direction) in [("↑", -1isize), ("↓", 1), ("Remove", 0)] {
                let button = gtk::Button::with_label(title);
                button.set_tooltip_text(Some(match direction {
                    -1 => "Move step up",
                    1 => "Move step down",
                    _ => "Remove step",
                }));
                button.set_sensitive(match direction {
                    -1 => index > 0,
                    1 => index + 1 < workflow.steps.len(),
                    _ => true,
                });
                let weak = json.downgrade();
                button.connect_clicked(move |_| {
                    edit(&weak, |steps| {
                        if index >= steps.len() {
                            return;
                        }
                        if direction == 0 {
                            steps.remove(index);
                        } else {
                            let target = index as isize + direction;
                            if target >= 0 && (target as usize) < steps.len() {
                                steps.swap(index, target as usize);
                            }
                        }
                    })
                });
                row.append(&button);
            }
            rows.append(&row);
        }
    });
    let changed = refresh.clone();
    json.buffer().connect_changed(move |_| changed());
    let changed = refresh.clone();
    choice.connect_selected_notify(move |_| changed());
    refresh();
    root
}
struct WeakActionEditor {
    choice: glib::WeakRef<adw::ComboRow>,
    shortcut_row: glib::WeakRef<adw::ActionRow>,
    record: glib::WeakRef<gtk::Button>,
    shortcut: Rc<RefCell<KeyChord>>,
    json: glib::WeakRef<gtk::TextView>,
    json_box: glib::WeakRef<gtk::Box>,
    presets_row: glib::WeakRef<adw::ActionRow>,
    host_row: glib::WeakRef<adw::SpinRow>,
    loading: Rc<Cell<bool>>,
}
impl WeakActionEditor {
    fn upgrade(&self) -> Option<ActionEditor> {
        Some(ActionEditor {
            choice: self.choice.upgrade()?,
            shortcut_row: self.shortcut_row.upgrade()?,
            record: self.record.upgrade()?,
            shortcut: self.shortcut.clone(),
            json: self.json.upgrade()?,
            json_box: self.json_box.upgrade()?,
            presets_row: self.presets_row.upgrade()?,
            host_row: self.host_row.upgrade()?,
            loading: self.loading.clone(),
        })
    }
}

struct BindingEditor {
    actions: [ActionEditor; 3],
    hold_ms: adw::SpinRow,
    double_ms: adw::SpinRow,
    gestures: adw::PreferencesGroup,
    settings: [adw::SpinRow; 5],
    records: Vec<gtk::Button>,
    directions: Rc<RefCell<[Option<KeyChord>; 4]>>,
}
impl BindingEditor {
    fn new(parent: &adw::Dialog, page: &adw::PreferencesPage) -> Self {
        let actions = [
            ActionEditor::new(parent, page, "Tap action"),
            ActionEditor::new(parent, page, "Hold action"),
            ActionEditor::new(parent, page, "Double-tap action"),
        ];
        let timings=adw::PreferencesGroup::builder().title("Timing").description("A double-tap delays the single tap until its window expires. Hold/gesture consumes the press; release before repeating. Timer resolution: approximately 10 ms, not realtime.").build();
        let hold_ms = adw::SpinRow::with_range(100., 5000., 10.);
        hold_ms.set_title("Hold threshold (ms)");
        timings.add(&hold_ms);
        let double_ms = adw::SpinRow::with_range(100., 1000., 10.);
        double_ms.set_title("Double-tap window (ms)");
        timings.add(&double_ms);
        page.add(&timings);
        let gestures=adw::PreferencesGroup::builder().title("Hold and move").description("One RawXY owner per device; one gesture per press. Jitter/threshold are raw device units, not pixels. Movement suppresses tap/hold. Unassigned directions do nothing.").build();
        let directions = Rc::new(RefCell::new([None, None, None, None]));
        let mut records = Vec::new();
        for (index, name) in ["Up", "Down", "Left", "Right"].into_iter().enumerate() {
            let row = adw::ActionRow::builder().title(name).build();
            let record = gtk::Button::with_label("Record shortcut");
            record.set_valign(gtk::Align::Center);
            let clear = gtk::Button::with_label("Clear");
            clear.set_valign(gtk::Align::Center);
            row.add_suffix(&record);
            row.add_suffix(&clear);
            gestures.add(&row);
            let data = directions.clone();
            let weak = record.downgrade();
            clear.connect_clicked(move |_| {
                data.borrow_mut()[index] = None;
                if let Some(record) = weak.upgrade() {
                    record.set_label("Record shortcut");
                }
            });
            let data = directions.clone();
            let weak = record.downgrade();
            let parent = parent.downgrade();
            record.connect_clicked(move |_| {
                if let Some(parent) = parent.upgrade() {
                    let data = data.clone();
                    let weak = weak.clone();
                    record_shortcut(&parent, move |chord| {
                        if let Some(record) = weak.upgrade() {
                            record.set_label(&chord.label);
                        }
                        data.borrow_mut()[index] = Some(chord);
                    });
                }
            });
            records.push(record);
        }
        let settings = [
            ("Movement threshold", 2., 4096.),
            ("Jitter allowance", 0., 4095.),
            ("Minimum samples", 1., 32.),
            ("Gesture timeout (ms)", 50., 5000.),
            ("Axis dominance (%)", 110., 400.),
        ]
        .map(|(title, min, max)| {
            let row = adw::SpinRow::with_range(min, max, 1.);
            row.set_title(title);
            gestures.add(&row);
            row
        });
        page.add(&gestures);
        Self {
            actions,
            hold_ms,
            double_ms,
            gestures,
            settings,
            records,
            directions,
        }
    }
    fn load(&self, binding: &Binding, raw_xy: bool) {
        for (editor, value) in
            self.actions
                .iter()
                .zip([&binding.tap, &binding.hold, &binding.double_tap])
        {
            editor.load(value);
        }
        self.hold_ms.set_value(f64::from(binding.hold_ms));
        self.double_ms.set_value(f64::from(binding.double_tap_ms));
        *self.directions.borrow_mut() = binding.directions.clone();
        for (record, chord) in self.records.iter().zip(&binding.directions) {
            record.set_label(
                chord
                    .as_ref()
                    .map(|c| c.label.as_str())
                    .unwrap_or("Record shortcut"),
            );
        }
        let c = &binding.gesture;
        for (row, value) in self.settings.iter().zip([
            c.threshold,
            c.jitter,
            u16::from(c.min_samples),
            c.timeout_ms,
            c.axis_ratio_percent,
        ]) {
            row.set_value(f64::from(value));
        }
        self.gestures.set_visible(raw_xy);
    }
    fn read(&self) -> anyhow::Result<Binding> {
        let binding = Binding {
            tap: self.actions[0].read()?,
            hold: self.actions[1].read()?,
            double_tap: self.actions[2].read()?,
            hold_ms: self.hold_ms.value() as u16,
            double_tap_ms: self.double_ms.value() as u16,
            directions: self.directions.borrow().clone(),
            gesture: crate::gesture::GestureConfig {
                threshold: self.settings[0].value() as u16,
                jitter: self.settings[1].value() as u16,
                min_samples: self.settings[2].value() as u8,
                timeout_ms: self.settings[3].value() as u16,
                axis_ratio_percent: self.settings[4].value() as u16,
            },
        };
        binding.gesture.validate()?;
        binding.validate()?;
        Ok(binding)
    }
}

fn record_shortcut(parent: &adw::Dialog, on_recorded: impl Fn(KeyChord) + 'static) -> adw::Dialog {
    let dialog = adw::Dialog::builder()
        .title("Record shortcut")
        .content_width(400)
        .content_height(180)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let label = gtk::Label::new(Some(
        "Press your shortcut now. Escape cancels.\nOnly keys in this popup are recorded.",
    ));
    label.set_wrap(true);
    label.set_margin_start(24);
    label.set_margin_end(24);
    label.set_margin_top(20);
    label.set_margin_bottom(24);
    label.set_focusable(true);
    toolbar.set_content(Some(&label));
    dialog.set_child(Some(&toolbar));
    let controller = gtk::EventControllerKey::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = dialog.downgrade();
    controller.connect_key_pressed(move |_, key, code, state| {
        if key == gtk::gdk::Key::Escape {
            if let Some(dialog) = weak.upgrade() {
                dialog.close();
            }
            return gtk::glib::Propagation::Stop;
        }
        if let Some(keys) = physical_keys(code, state) {
            let chord = KeyChord {
                keys,
                label: gtk::accelerator_get_label(key, state).to_string(),
            };
            if chord.validate().is_ok() {
                on_recorded(chord);
                if let Some(dialog) = weak.upgrade() {
                    dialog.close();
                }
            }
        }
        gtk::glib::Propagation::Stop
    });
    dialog.add_controller(controller);
    crate::i18n::translate_widget(dialog.upcast_ref());
    dialog.present(Some(parent));
    label.grab_focus();
    dialog
}

#[cfg(test)]
pub(crate) fn exercise_dialogs(parent: &adw::ApplicationWindow, device: DeviceSnapshot) {
    fn find<T: IsA<gtk::Widget> + Clone + glib::object::IsClass>(
        widget: &gtk::Widget,
        predicate: &impl Fn(&T) -> bool,
    ) -> Option<T> {
        if let Ok(item) = widget.clone().downcast::<T>() {
            if predicate(&item) {
                return Some(item);
            }
        }
        let mut child = widget.first_child();
        while let Some(item) = child {
            if let Some(found) = find(&item, predicate) {
                return Some(found);
            }
            child = item.next_sibling();
        }
        None
    }
    let dialog = show(parent, device, String::new(), || {
        panic!("test must not save settings")
    })
    .unwrap();
    snapshot_fixture(parent, "actions");
    let action =
        find::<adw::ComboRow>(dialog.upcast_ref(), &|row| row.title() == "Action").unwrap();
    action.set_selected(1);
    let row =
        find::<adw::ActionRow>(dialog.upcast_ref(), &|row| row.title() == "Shortcut").unwrap();
    assert!(row.is_visible());
    action.set_selected(2);
    assert!(!row.is_visible());
    action.set_selected(0);
    // Round-trip every action slot and all timing/gesture controls without
    // saving, input injection, hardware access or executing any workflow.
    let page = adw::PreferencesPage::new();
    let editor = BindingEditor::new(&dialog, &page);
    let binding = Binding {
        tap: Some(Action::Workflow(crate::workflow::templates().remove(0))),
        hold: Some(Action::Ring(vec![(
            "Copy".into(),
            Action::Shortcut(KeyChord {
                keys: vec![29, 46],
                label: "Ctrl+C".into(),
            }),
        )])),
        double_tap: Some(Action::SwitchHost(2)),
        hold_ms: 700,
        double_tap_ms: 300,
        directions: [
            Some(KeyChord {
                keys: vec![29, 46],
                label: "Ctrl+C".into(),
            }),
            None,
            None,
            None,
        ],
        gesture: crate::gesture::GestureConfig {
            threshold: 80,
            jitter: 6,
            min_samples: 4,
            timeout_ms: 1200,
            axis_ratio_percent: 170,
        },
    };
    editor.load(&binding, true);
    assert_eq!(editor.read().unwrap(), binding);
    assert!(editor.actions[0].json_box.is_visible());
    assert!(editor.actions[0].presets_row.is_visible());
    assert!(editor.actions[1].json_box.is_visible());
    assert!(editor.actions[2].host_row.is_visible());
    editor.actions[0].json.buffer().set_text("not JSON");
    assert!(editor.read().is_err());
    editor.load(&binding, false);
    assert!(!editor.gestures.is_visible());
    editor.settings[1].set_value(100.);
    assert!(editor.read().is_err(), "jitter above threshold cannot save");
    drop(editor);
    drop(page);
    let chosen = Rc::new(RefCell::new(Vec::new()));
    let capture = chosen.clone();
    let ring = crate::ring_ui::show(
        parent,
        DeviceSnapshot::default(),
        vec![(
            "Copy".into(),
            Action::Shortcut(KeyChord {
                keys: vec![29, 46],
                label: "Ctrl+C".into(),
            }),
        )],
        move |_, action| capture.borrow_mut().push(action),
    );
    let ring_controllers = ring.observe_controllers();
    let ring_key = (0..ring_controllers.n_items())
        .filter_map(|i| {
            ring_controllers
                .item(i)
                .and_downcast::<gtk::EventControllerKey>()
        })
        .next()
        .unwrap();
    ring_key.emit_by_name::<bool>(
        "key-pressed",
        &[&gtk::gdk::Key::_1, &10u32, &gtk::gdk::ModifierType::empty()],
    );
    assert_eq!(chosen.borrow().len(), 1);
    ring_key.emit_by_name::<bool>(
        "key-pressed",
        &[
            &gtk::gdk::Key::Return,
            &36u32,
            &gtk::gdk::ModifierType::empty(),
        ],
    );
    assert_eq!(
        chosen.borrow().len(),
        1,
        "ring selection fires at most once"
    );
    ring.force_close();
    drop(ring_key);
    drop(ring_controllers);
    drop(ring);
    let captured = Rc::new(RefCell::new(None));
    let capture = captured.clone();
    let recording = record_shortcut(&dialog, move |chord| *capture.borrow_mut() = Some(chord));
    snapshot_fixture(parent, "recording");
    let controllers = recording.observe_controllers();
    let key = (0..controllers.n_items())
        .filter_map(|i| {
            controllers
                .item(i)
                .and_downcast::<gtk::EventControllerKey>()
        })
        .next()
        .unwrap();
    key.emit_by_name::<bool>(
        "key-pressed",
        &[
            &gtk::gdk::Key::c,
            &54u32,
            &gtk::gdk::ModifierType::CONTROL_MASK,
        ],
    );
    assert_eq!(captured.borrow().as_ref().unwrap().keys, vec![29, 46]);
    recording.force_close();
    drop(key);
    drop(controllers);
    drop(recording);
    dialog.force_close();
    drop(action);
    drop(row);
    let weak = dialog.downgrade();
    drop(dialog);
    while glib::MainContext::default().pending() {
        glib::MainContext::default().iteration(false);
    }
    assert!(
        weak.upgrade().is_none(),
        "action dialog must release its widgets and callbacks"
    );
}

#[cfg(test)]
pub(crate) fn snapshot_fixture(window: &adw::ApplicationWindow, name: &str) {
    if std::env::var_os("LOGICENTER_TEST_SNAPSHOTS").is_none() {
        return;
    }
    let context = glib::MainContext::default();
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_millis(350) {
        while context.pending() {
            context.iteration(false);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(
        &snapshot,
        f64::from(window.width()),
        f64::from(window.height()),
    );
    let node = snapshot.to_node().unwrap();
    let renderer = gtk::gsk::Renderer::for_surface(&window.surface().unwrap()).unwrap();
    let texture = renderer.render_texture(&node, None);
    texture
        .save_to_png(format!(
            "/tmp/opencode/logicenter-device-features/{name}.png"
        ))
        .unwrap();
    renderer.unrealize();
}

fn physical_keys(code: u32, state: gtk::gdk::ModifierType) -> Option<Vec<u16>> {
    let key = u16::try_from(code.checked_sub(8)?).ok()?;
    if !(1..=255).contains(&key) || [29, 42, 54, 56, 97, 100, 125, 126].contains(&key) {
        return None;
    }
    let mut keys = Vec::new();
    for (modifier, code) in [
        (gtk::gdk::ModifierType::CONTROL_MASK, 29),
        (gtk::gdk::ModifierType::SHIFT_MASK, 42),
        (gtk::gdk::ModifierType::ALT_MASK, 56),
        (gtk::gdk::ModifierType::SUPER_MASK, 125),
    ] {
        if state.contains(modifier) {
            keys.push(code);
        }
    }
    keys.push(key);
    Some(keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_uses_physical_linux_keycodes_without_global_listeners() {
        assert_eq!(
            physical_keys(
                33,
                gtk::gdk::ModifierType::CONTROL_MASK | gtk::gdk::ModifierType::SHIFT_MASK
            ),
            Some(vec![29, 42, 25])
        );
        assert_eq!(physical_keys(1, gtk::gdk::ModifierType::empty()), None);
        assert_eq!(
            physical_keys(37, gtk::gdk::ModifierType::CONTROL_MASK),
            None
        );
    }
}
