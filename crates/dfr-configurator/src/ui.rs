//! Small native form helpers shared by the three task-oriented pages.
use gtk4::{glib, prelude::*};
use libadwaita::{self as adw, prelude::*};

pub fn margins(widget: &impl IsA<gtk4::Widget>, size: i32) {
    widget.set_margin_start(size);
    widget.set_margin_end(size);
    widget.set_margin_top(size);
    widget.set_margin_bottom(size);
}

pub fn group(title: &str) -> adw::PreferencesGroup {
    adw::PreferencesGroup::builder().title(title).build()
}

pub fn combo(title: &str, names: &[&str], selected: u32) -> adw::ComboRow {
    let row = adw::ComboRow::builder()
        .use_markup(false)
        .title(title)
        .model(&gtk4::StringList::new(names))
        .build();
    row.set_selected(selected);
    row
}

pub fn spin(title: &str, min: f64, max: f64, value: f64) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(min, max, 1.0);
    row.set_title(title);
    row.set_value(value);
    row
}

pub fn entry(title: &str, text: &str) -> adw::EntryRow {
    let row = adw::EntryRow::builder().title(title).build();
    row.set_text(text);
    row
}

pub fn scroll(child: &impl IsA<gtk4::Widget>) -> gtk4::ScrolledWindow {
    gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vscrollbar_policy(gtk4::PolicyType::Automatic)
        .vexpand(true)
        .child(child)
        .build()
}

pub fn header(title: &str, button: &impl IsA<gtk4::Widget>) -> gtk4::Box {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    let label = gtk4::Label::new(Some(title));
    label.add_css_class("heading");
    label.set_xalign(0.0);
    label.set_hexpand(true);
    row.append(&label);
    row.append(button);
    row
}

pub fn clear(box_: &gtk4::Box) {
    while let Some(child) = box_.first_child() {
        box_.remove(&child);
    }
}

pub fn color(title: &str, hex: &str, changed: impl Fn(String) + 'static) -> adw::EntryRow {
    let row = entry(title, hex);
    let dialog = gtk4::ColorDialog::builder().with_alpha(false).build();
    let swatch = gtk4::ColorDialogButton::new(Some(dialog));
    swatch.set_valign(gtk4::Align::Center);
    if let Ok(color) = gtk4::gdk::RGBA::parse(hex) {
        swatch.set_rgba(&color);
    }
    row.add_suffix(&swatch);
    swatch.connect_rgba_notify(glib::clone!(
        #[weak]
        row,
        move |button| {
            let c = button.rgba();
            row.set_text(&format!(
                "#{:02X}{:02X}{:02X}",
                (c.red() * 255.0).round() as u8,
                (c.green() * 255.0).round() as u8,
                (c.blue() * 255.0).round() as u8
            ));
        }
    ));
    row.connect_changed(move |row| {
        let hex = row.text().to_string();
        if let Err(error) = dfr_config::daycycle::parse_hex_color(&hex) {
            row.add_css_class("error");
            row.set_tooltip_text(Some(&error.to_string()));
        } else {
            row.remove_css_class("error");
            row.set_tooltip_text(None);
        }
        if let Ok(color) = gtk4::gdk::RGBA::parse(&hex) {
            if swatch.rgba() != color {
                swatch.set_rgba(&color);
            }
        }
        changed(hex);
    });
    row
}

pub fn icon(button: &dfr_config::ButtonConfig) -> gtk4::Image {
    let image = gtk4::Image::new();
    image.set_pixel_size(24);
    if let Some(name) = &button.icon {
        let search = dfr_config::IconSearch::new(dfr_render::config::ICON_DIRS.iter().copied());
        if let Some(path) = search
            .candidates(
                name,
                button.theme.as_deref(),
                button.icon_width.unwrap_or(40),
                button.icon_height.unwrap_or(40),
            )
            .into_iter()
            .find(|path| path.is_file())
        {
            image.set_from_file(Some(path));
        } else {
            image.set_icon_name(Some("image-missing-symbolic"));
        }
    } else {
        image.set_icon_name(Some(match crate::model::button_kind(button) {
            crate::model::ButtonKind::Time => "preferences-system-time-symbolic",
            crate::model::ButtonKind::Battery => "battery-symbolic",
            crate::model::ButtonKind::Waveform => "audio-input-microphone-symbolic",
            crate::model::ButtonKind::Visualizer | crate::model::ButtonKind::NowPlaying => {
                "audio-x-generic-symbolic"
            }
            _ => "font-x-generic-symbolic",
        }));
    }
    image
}

pub fn error_dialog(parent: &impl IsA<gtk4::Widget>, title: &str, error: &str) {
    let dialog = adw::AlertDialog::builder()
        .heading(title)
        .body(error)
        .build();
    dialog.add_response("close", "Close");
    dialog.present(Some(parent));
}
