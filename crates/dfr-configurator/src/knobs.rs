//! The Knobs page: every global knob from the `dfr-config` registry,
//! rendered as a preferences page with validated inputs. The registry
//! drives ordering, grouping, documentation, and constraints; a
//! type-aware accessor per knob name drives reading and writing the
//! overlay. Knobs the Deck page owns (the layer lists and their
//! pointers) appear as pointers to that page instead of duplicating
//! editors, and the legacy layer knobs offer the one-way migration
//! the first Deck edit performs.

use crate::layers::Editor;
use gtk4::{glib, prelude::*};
use libadwaita as adw;
use libadwaita::prelude::*;
use std::rc::Rc;

pub struct KnobsPage {
    pub root: adw::PreferencesPage,
}

impl KnobsPage {
    pub fn build(editor: &Rc<Editor>) -> Self {
        let page = adw::PreferencesPage::new();

        // Group the registry's global knobs by their group field, in
        // registry order, and render each with the right widget.
        let mut current_group = "";
        let mut current_group_row = adw::PreferencesGroup::new();
        for knob in dfr_config::KNOBS
            .iter()
            .filter(|k| k.scope == dfr_config::registry::Scope::Global)
        {
            if knob.group != current_group {
                current_group = knob.group;
                let group = adw::PreferencesGroup::new();
                group.set_title(knob.group);
                page.add(&group);
                current_group_row = group;
            }
            let group = &current_group_row;
            add_knob_row(group, editor, knob);
        }

        Self { root: page }
    }
}

fn add_knob_row(
    group: &adw::PreferencesGroup,
    editor: &Rc<Editor>,
    knob: &dfr_config::registry::Knob,
) {
    match knob.name {
        // The layer-list knobs live on the Deck page; pointers only.
        "Layers" | "DefaultLayer" | "FnLayer" => {
            let row = adw::ActionRow::builder()
                .title(knob.name)
                .subtitle("Edited on the Deck page")
                .build();
            let button = gtk4::Button::with_label("Open Deck page");
            button.add_css_class("suggested-action");
            button.set_valign(gtk4::Align::Center);
            row.add_suffix(&button);
            button.connect_clicked(|button| {
                // The window registers `show-deck`; activating it here
                // switches the stack without the page knowing the
                // window's shape.
                let _ = button.activate_action("win.show-deck", None);
            });
            group.add(&row);
        }
        // The legacy pair: migrates on the Deck page's first edit.
        "PrimaryLayerKeys" | "MediaLayerKeys" | "MediaLayerDefault" => {
            let legacy = {
                let doc = editor.doc.borrow();
                doc.user().primary_layer_keys.is_some()
                    || doc.user().media_layer_keys.is_some()
                    || doc.user().media_layer_default.is_some()
            };
            if !legacy {
                // The overlay uses the named syntax; nothing to show.
                return;
            }
            let row = adw::ActionRow::builder()
                .title(knob.name)
                .subtitle("Legacy syntax - migrate to named layers on the Deck page")
                .build();
            group.add(&row);
        }
        // The bands and the theme map get bespoke editors.
        "DayCycleBands" => {
            let row = adw::ExpanderRow::builder()
                .title(knob.name)
                .subtitle(knob.doc)
                .build();
            group.add(&row);
            build_bands_editor(row, editor);
        }
        "Themes" => {
            let row = adw::ExpanderRow::builder()
                .title(knob.name)
                .subtitle(knob.doc)
                .build();
            group.add(&row);
            build_themes_editor(row, editor);
        }
        "ActiveTheme" => {
            let themes = editor.doc.borrow().known_themes();
            let mut names: Vec<String> = vec!["remembered choice".into()];
            names.extend(themes);
            let strings: Vec<&str> = names.iter().map(String::as_str).collect();
            let dropdown = gtk4::DropDown::from_strings(&strings);
            dropdown.set_valign(gtk4::Align::Center);
            let current = editor.doc.borrow().user().active_theme.clone();
            let selected = match &current {
                Some(name) => names
                    .iter()
                    .position(|candidate| candidate == name)
                    .unwrap_or(0),
                None => 0,
            };
            dropdown.set_selected(selected as u32);
            let row = titled_row(editor, knob, &dropdown);
            group.add(&row);
            dropdown.connect_selected_notify(glib::clone!(
                #[strong]
                editor,
                #[strong]
                names,
                move |dropdown| {
                    let index = dropdown.selected() as usize;
                    let pinned = (index > 0).then(|| names[index].clone());
                    editor.doc.borrow_mut().user_mut().active_theme = pinned;
                    editor.run_pipeline();
                }
            ));
        }
        name => {
            let user = editor.doc.borrow();
            let proxy = user.effective().expect("the base document parses");
            drop(user);
            match knob.kind {
                "bool" => {
                    let value = read_bool(&proxy, name).or_else(|| knob.default.parse().ok());
                    let switch = gtk4::Switch::new();
                    switch.set_valign(gtk4::Align::Center);
                    if let Some(value) = value {
                        switch.set_active(value);
                    }
                    let row = titled_row(editor, knob, &switch);
                    group.add(&row);
                    switch.connect_state_set(glib::clone!(
                        #[strong]
                        editor,
                        move |_, active| {
                            editor.doc.borrow_mut().user_mut().write_bool(name, active);
                            editor.run_pipeline();
                            gtk4::glib::Propagation::Proceed
                        }
                    ));
                }
                "u32" => {
                    let (min, max) = parse_range(knob.constraint);
                    let value = read_u32(&proxy, name)
                        .unwrap_or_else(|| knob.default.parse().unwrap_or(min));
                    let spin = gtk4::SpinButton::with_range(min as f64, max as f64, 1.0);
                    spin.set_value(value as f64);
                    spin.set_valign(gtk4::Align::Center);
                    let row = titled_row(editor, knob, &spin);
                    group.add(&row);
                    let name = knob.name;
                    spin.connect_value_changed(glib::clone!(
                        #[strong]
                        editor,
                        move |spin| {
                            let value = spin.value() as u32;
                            editor
                                .doc
                                .borrow_mut()
                                .user_mut()
                                .write_u32(name, Some(value));
                            editor.run_pipeline();
                        }
                    ));
                }
                "enum" => {
                    let options: Vec<&str> = knob
                        .constraint
                        .unwrap_or("")
                        .split(',')
                        .map(str::trim)
                        .filter(|option| !option.is_empty() && !option.contains(' '))
                        .collect();
                    // Accent constraints read "neutral, cyan, ...";
                    // pet constraints "cat, fox".
                    let dropdown = gtk4::DropDown::from_strings(&options);
                    dropdown.set_valign(gtk4::Align::Center);
                    let current =
                        read_enum(&proxy, name).unwrap_or_else(|| knob.default.to_owned());
                    if let Some(index) = options.iter().position(|option| *option == current) {
                        dropdown.set_selected(index as u32);
                    }
                    let row = titled_row(editor, knob, &dropdown);
                    group.add(&row);
                    dropdown.connect_selected_notify(glib::clone!(
                        #[strong]
                        editor,
                        move |dropdown| {
                            let index = dropdown.selected() as usize;
                            let value = options[index];
                            editor.doc.borrow_mut().user_mut().write_enum(name, value);
                            editor.run_pipeline();
                        }
                    ));
                }
                _ => {
                    // The string knobs (FontTemplate and friends).
                    let entry = gtk4::Entry::new();
                    entry.set_text(
                        &read_string(&proxy, name)
                            .unwrap_or_else(|| knob.default.trim_matches('"').to_owned()),
                    );
                    entry.set_width_chars(28);
                    entry.set_valign(gtk4::Align::Center);
                    let row = titled_row(editor, knob, &entry);
                    group.add(&row);
                    entry.connect_changed(glib::clone!(
                        #[strong]
                        editor,
                        move |entry| {
                            let text = entry.text().to_string();
                            editor
                                .doc
                                .borrow_mut()
                                .user_mut()
                                .write_string(name, (!text.is_empty()).then(|| text.clone()));
                            editor.run_pipeline();
                        }
                    ));
                }
            }
        }
    }
}

fn titled_row(
    editor: &Rc<Editor>,
    knob: &dfr_config::registry::Knob,
    suffix: &impl IsA<gtk4::Widget>,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(knob.name)
        .subtitle(knob.doc)
        .build();
    row.add_suffix(suffix);
    let source = if editor.doc.borrow().has_override(knob.name) {
        "Customized"
    } else {
        "Using Base Setting"
    };
    row.set_subtitle(&format!("{source} · {}", knob.doc));
    let reset = gtk4::Button::from_icon_name("edit-undo-symbolic");
    reset.set_tooltip_text(Some("Use Base Setting"));
    reset.set_valign(gtk4::Align::Center);
    reset.set_sensitive(editor.doc.borrow().has_override(knob.name));
    let name = knob.name;
    reset.connect_clicked(glib::clone!(
        #[strong]
        editor,
        move |_| {
            editor.doc.borrow_mut().reset_global(name);
            editor.restructured();
        }
    ));
    row.add_suffix(&reset);
    row.set_activatable(false);
    row
}

fn parse_range(constraint: Option<&str>) -> (u32, u32) {
    // "0-255", "1-3600 seconds", ...; fall back to a generous range.
    if let Some(constraint) = constraint {
        let digits: Vec<&str> = constraint.split('-').collect();
        if digits.len() == 2 {
            if let (Ok(min), Ok(max)) = (
                digits[0].trim().parse::<u32>(),
                digits[1]
                    .trim()
                    .trim_end_matches(|c: char| !c.is_ascii_digit())
                    .parse::<u32>(),
            ) {
                return (min, max);
            }
        }
    }
    (0, u32::MAX)
}

/// The band editor: one row per band with name, hour, and color.
fn build_bands_editor(row: adw::ExpanderRow, editor: &Rc<Editor>) {
    let bands = editor.doc.borrow().bands().cloned();
    let Some(bands) = bands else {
        row.set_subtitle("Using the default bands - add one to override them");
        let add = gtk4::Button::with_label("Override bands");
        add.set_valign(gtk4::Align::Center);
        row.add_suffix(&add);
        add.connect_clicked(glib::clone!(
            #[strong]
            editor,
            move |_| {
                {
                    let mut doc = editor.doc.borrow_mut();
                    let defaults = dfr_config::BandConfig {
                        name: "day".into(),
                        start_hour: 12,
                        color: "#1A1B1E".into(),
                    };
                    doc.set_bands(vec![defaults]);
                }
                editor.restructured();
            }
        ));
        return;
    };
    drop_bands_into_row(&row, editor, &bands);
}

fn drop_bands_into_row(
    row: &adw::ExpanderRow,
    editor: &Rc<Editor>,
    bands: &[dfr_config::BandConfig],
) {
    for (index, band) in bands.iter().enumerate() {
        let band_row = adw::ActionRow::builder().title(&band.name).build();
        let hour = gtk4::SpinButton::with_range(0.0, 23.0, 1.0);
        hour.set_value(band.start_hour as f64);
        hour.set_valign(gtk4::Align::Center);
        let color = gtk4::Entry::new();
        color.set_text(&band.color);
        color.set_width_chars(9);
        color.set_valign(gtk4::Align::Center);
        let remove = gtk4::Button::from_icon_name("user-trash-symbolic");
        remove.add_css_class("flat");
        remove.set_valign(gtk4::Align::Center);
        band_row.add_suffix(&hour);
        band_row.add_suffix(&color);
        band_row.add_suffix(&remove);

        hour.connect_value_changed(glib::clone!(
            #[strong]
            editor,
            move |spin| {
                set_band(&editor, index, |band| band.start_hour = spin.value() as u8);
            }
        ));
        color.connect_changed(glib::clone!(
            #[strong]
            editor,
            move |entry| {
                let text = entry.text().to_string();
                set_band(&editor, index, |band| band.color = text.clone());
            }
        ));
        remove.connect_clicked(glib::clone!(
            #[strong]
            editor,
            move |_| {
                let mut doc = editor.doc.borrow_mut();
                if let Some(bands) = doc.bands().cloned() {
                    let mut bands = bands;
                    if index < bands.len() {
                        bands.remove(index);
                    }
                    if bands.is_empty() {
                        // An empty list is the default bands again.
                        doc.user_mut().day_cycle_bands = None;
                    } else {
                        doc.set_bands(bands);
                    }
                }
                drop(doc);
                editor.run_pipeline();
            }
        ));
        row.add_row(&band_row);
    }
    let add = gtk4::Button::from_icon_name("list-add-symbolic");
    add.set_tooltip_text(Some("Add a band"));
    add.set_valign(gtk4::Align::Center);
    row.add_suffix(&add);
    add.connect_clicked(glib::clone!(
        #[strong]
        editor,
        move |_| {
            {
                let mut doc = editor.doc.borrow_mut();
                let mut bands = doc.bands().cloned().unwrap_or_default();
                bands.push(dfr_config::BandConfig {
                    name: format!("band-{}", bands.len() + 1),
                    start_hour: 0,
                    color: "#1A1B1E".into(),
                });
                doc.set_bands(bands);
            }
            editor.run_pipeline();
        }
    ));
}

fn set_band(editor: &Rc<Editor>, index: usize, change: impl FnOnce(&mut dfr_config::BandConfig)) {
    let mut doc = editor.doc.borrow_mut();
    if let Some(bands) = doc.user_mut().day_cycle_bands.as_mut() {
        if let Some(band) = bands.get_mut(index) {
            change(band);
        }
    }
    drop(doc);
    editor.run_pipeline();
}

/// The user-themes map editor: add/remove a theme and edit its accent
/// hex values; the built-ins are visible as the picker's options.
fn build_themes_editor(row: adw::ExpanderRow, editor: &Rc<Editor>) {
    let themes = editor
        .doc
        .borrow()
        .user()
        .themes
        .clone()
        .unwrap_or_default();
    for (name, theme) in themes.iter() {
        let theme_row = adw::ExpanderRow::builder().title(name).build();
        row.add_row(&theme_row);
        for (accent, hex) in theme.accents.clone().unwrap_or_default() {
            let accent_row = adw::ActionRow::builder().title(accent.name()).build();
            let entry = gtk4::Entry::new();
            entry.set_text(&hex);
            entry.set_width_chars(9);
            entry.set_valign(gtk4::Align::Center);
            accent_row.add_suffix(&entry);
            theme_row.add_row(&accent_row);
            let name = name.clone();
            let accent = accent;
            entry.connect_changed(glib::clone!(
                #[strong]
                editor,
                move |entry| {
                    let hex = entry.text().to_string();
                    let mut doc = editor.doc.borrow_mut();
                    if let Some(theme) = doc
                        .user_mut()
                        .themes
                        .as_mut()
                        .and_then(|themes| themes.get_mut(&name))
                    {
                        theme
                            .accents
                            .get_or_insert_with(Default::default)
                            .insert(accent, hex.clone());
                    }
                    drop(doc);
                    editor.run_pipeline();
                }
            ));
        }
        let remove = gtk4::Button::from_icon_name("user-trash-symbolic");
        remove.add_css_class("flat");
        remove.set_valign(gtk4::Align::Center);
        theme_row.add_suffix(&remove);
        let name = name.clone();
        remove.connect_clicked(glib::clone!(
            #[strong]
            editor,
            move |_| {
                let mut doc = editor.doc.borrow_mut();
                doc.remove_theme(&name);
                drop(doc);
                editor.run_pipeline();
            }
        ));
    }
    let add = gtk4::Button::from_icon_name("list-add-symbolic");
    add.set_tooltip_text(Some("Add a theme"));
    add.set_valign(gtk4::Align::Center);
    row.add_suffix(&add);
    add.connect_clicked(glib::clone!(
        #[strong]
        editor,
        move |_| {
            let mut n = 1;
            loop {
                let name = format!("theme-{n}");
                if editor
                    .doc
                    .borrow()
                    .known_themes()
                    .iter()
                    .all(|known| *known != name)
                {
                    let _ = editor
                        .doc
                        .borrow_mut()
                        .set_theme(&name, dfr_config::ThemeConfig::default());
                    break;
                }
                n += 1;
            }
            editor.run_pipeline();
        }
    ));
}

// ---- typed accessors over the overlay ------------------------------

fn read_bool(proxy: &dfr_config::ConfigProxy, name: &str) -> Option<bool> {
    match name {
        "ShowButtonOutlines" => proxy.show_button_outlines,
        "EnablePixelShift" => proxy.enable_pixel_shift,
        "AdaptiveBrightness" => proxy.adaptive_brightness,
        "BatteryCycleEnabled" => proxy.battery_cycle_enabled,
        "PetEnabled" => proxy.pet_enabled,
        "DayCycleEnabled" => proxy.day_cycle_enabled,
        "ThemesEnabled" => proxy.themes_enabled,
        _ => None,
    }
}

fn read_u32(proxy: &dfr_config::ConfigProxy, name: &str) -> Option<u32> {
    match name {
        "ActiveBrightness" => proxy.active_brightness,
        "DoublePressSwitchLayers" => proxy.double_press_switch_layers,
        "BatteryCycleTimeoutSecs" => proxy.battery_cycle_timeout_secs,
        "BatteryWarningThreshold" => proxy.battery_warning_threshold,
        "BatteryCriticalThreshold" => proxy.battery_critical_threshold,
        _ => None,
    }
}

fn read_enum(proxy: &dfr_config::ConfigProxy, name: &str) -> Option<String> {
    match name {
        "BatteryWarningColor" => proxy.battery_warning_color.map(|a| a.name().to_owned()),
        "BatteryCriticalColor" => proxy.battery_critical_color.map(|a| a.name().to_owned()),
        "PetVariant" => proxy.pet_variant.clone(),
        "PetEnergy" => proxy.pet_energy.clone(),
        _ => None,
    }
}

fn read_string(proxy: &dfr_config::ConfigProxy, name: &str) -> Option<String> {
    match name {
        "FontTemplate" => proxy.font_template.clone(),
        _ => None,
    }
}

/// Write helpers on the overlay, in one trait so the knob rows stay
/// declarative. Writing `None` keeps the base value flowing through,
/// which is the app's "reset to base" everywhere.
trait OverlayWrite {
    fn write_bool(&mut self, name: &str, value: bool);
    fn write_u32(&mut self, name: &str, value: Option<u32>);
    fn write_enum(&mut self, name: &str, value: &str);
    fn write_string(&mut self, name: &str, value: Option<String>);
}

impl OverlayWrite for dfr_config::ConfigProxy {
    fn write_bool(&mut self, name: &str, value: bool) {
        match name {
            "ShowButtonOutlines" => self.show_button_outlines = Some(value),
            "EnablePixelShift" => self.enable_pixel_shift = Some(value),
            "AdaptiveBrightness" => self.adaptive_brightness = Some(value),
            "BatteryCycleEnabled" => self.battery_cycle_enabled = Some(value),
            "PetEnabled" => self.pet_enabled = Some(value),
            "DayCycleEnabled" => self.day_cycle_enabled = Some(value),
            "ThemesEnabled" => self.themes_enabled = Some(value),
            _ => {}
        }
    }

    fn write_u32(&mut self, name: &str, value: Option<u32>) {
        match name {
            "ActiveBrightness" => self.active_brightness = value,
            "DoublePressSwitchLayers" => self.double_press_switch_layers = value,
            "BatteryCycleTimeoutSecs" => self.battery_cycle_timeout_secs = value,
            "BatteryWarningThreshold" => self.battery_warning_threshold = value,
            "BatteryCriticalThreshold" => self.battery_critical_threshold = value,
            _ => {}
        }
    }

    fn write_enum(&mut self, name: &str, value: &str) {
        let accent = crate::layers::accent_from_name(value);
        match name {
            "BatteryWarningColor" => self.battery_warning_color = accent,
            "BatteryCriticalColor" => self.battery_critical_color = accent,
            "PetVariant" => self.pet_variant = Some(value.to_owned()),
            "PetEnergy" => self.pet_energy = Some(value.to_owned()),
            _ => {}
        }
    }

    fn write_string(&mut self, name: &str, value: Option<String>) {
        match name {
            "FontTemplate" => self.font_template = value,
            _ => {}
        }
    }
}
