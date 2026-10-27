//! GTK ring presenter. Deliberately parent-centered: Wayland normally does not
//! permit global pointer placement. This never claims universal cursor centering.
use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::model::DeviceSnapshot;
use crate::profiles::Action;

/// Present validated ring entries. Selection only invokes `on_choose(device,
/// action)`; the coordinator should call `Supervisor::execute` from that callback.
/// Arrow keys cycle, 1–8 select, Enter activates and Escape cancels. Native Tab
/// focus and clickable buttons are available too. Always centered on the parent.
pub fn show(
    parent: &adw::ApplicationWindow,
    device: DeviceSnapshot,
    actions: Vec<(String, Action)>,
    on_choose: impl Fn(DeviceSnapshot, Action) + 'static,
) -> adw::Dialog {
    let dialog = adw::Dialog::builder()
        .title("Actions ring")
        .presentation_mode(adw::DialogPresentationMode::Floating)
        .content_width(450)
        .content_height(440)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_start(20);
    content.set_margin_end(20);
    content.set_margin_bottom(20);
    let explanation = gtk::Label::new(Some(
        "Centered on this window (Wayland-safe fallback).\nArrow keys: select · Enter: run · 1–8: run · Escape: cancel",
    ));
    explanation.set_wrap(true);
    content.append(&explanation);
    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(8)
        .row_homogeneous(true)
        .column_homogeneous(true)
        .halign(gtk::Align::Center)
        .build();
    let callback: Rc<dyn Fn(DeviceSnapshot, Action)> = Rc::new(on_choose);
    let used = Rc::new(Cell::new(false));
    let closed = used.clone();
    dialog.connect_closed(move |_| closed.set(true));
    let mut buttons = Vec::new();
    if let Err(error) = crate::automation::validate_action(&Action::Ring(actions.clone())) {
        content.append(&gtk::Label::new(Some(&error.to_string())));
    } else {
        let positions = [
            (1, 0),
            (2, 0),
            (2, 1),
            (2, 2),
            (1, 2),
            (0, 2),
            (0, 1),
            (0, 0),
        ];
        let count = actions.len();
        for (index, (name, action)) in actions.into_iter().enumerate() {
            let (x, y) = positions[index * 8 / count];
            let button = gtk::Button::with_label(&format!("{} · {}", index + 1, name));
            button.set_size_request(110, 70);
            button.set_tooltip_text(Some(&name));
            if let Some(label) = button.child().and_downcast::<gtk::Label>() {
                label.set_wrap(true);
                label.set_max_width_chars(16);
                label.set_lines(3);
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            }
            let weak = dialog.downgrade();
            let used = used.clone();
            let device = device.clone();
            let callback = callback.clone();
            button.connect_clicked(move |_| {
                if used.replace(true) {
                    return;
                }
                if let Some(dialog) = weak.upgrade() {
                    dialog.close();
                }
                callback(device.clone(), action.clone());
            });
            grid.attach(&button, x, y, 1, 1);
            buttons.push(button);
        }
    }
    content.append(&grid);
    toolbar.set_content(Some(&content));
    dialog.set_child(Some(&toolbar));
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = dialog.downgrade();
    let weak_buttons: Vec<_> = buttons.iter().map(|b| b.downgrade()).collect();
    keys.connect_key_pressed(move |_, key, _, _| {
        use gtk::gdk::Key;
        if key == Key::Escape {
            used.set(true);
            if let Some(dialog) = weak.upgrade() {
                dialog.close();
            }
            return glib::Propagation::Stop;
        }
        if weak_buttons.is_empty() {
            return glib::Propagation::Proceed;
        }
        let focused = weak_buttons
            .iter()
            .position(|b| b.upgrade().is_some_and(|b| b.has_focus()))
            .unwrap_or(0);
        let index = if matches!(key, Key::Right | Key::Down) {
            Some((focused + 1) % weak_buttons.len())
        } else if matches!(key, Key::Left | Key::Up) {
            Some((focused + weak_buttons.len() - 1) % weak_buttons.len())
        } else {
            None
        };
        if let Some(index) = index {
            if let Some(button) = weak_buttons[index].upgrade() {
                button.grab_focus();
            }
            return glib::Propagation::Stop;
        }
        let digit = key
            .to_unicode()
            .and_then(|c| c.to_digit(10))
            .and_then(|d| d.checked_sub(1))
            .map(|d| d as usize);
        if let Some(index) = digit.filter(|i| *i < weak_buttons.len()) {
            if let Some(button) = weak_buttons[index].upgrade() {
                button.emit_clicked();
            }
            return glib::Propagation::Stop;
        }
        if matches!(key, Key::Return | Key::KP_Enter) {
            if let Some(button) = weak_buttons[focused].upgrade() {
                button.emit_clicked();
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    dialog.add_controller(keys);
    dialog.present(Some(parent));
    if let Some(button) = buttons.first() {
        button.grab_focus();
    }
    dialog
}
