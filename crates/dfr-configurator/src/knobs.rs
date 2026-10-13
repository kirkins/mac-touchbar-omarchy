//! Task-oriented preferences. The registry supplies constraints and help, not navigation order.
use crate::{
    layers::Editor,
    preview::{self, Preview},
    ui,
};
use gtk4::{glib, prelude::*};
use libadwaita::{self as adw, prelude::*};
use std::{cell::RefCell, rc::Rc};

#[derive(Clone, Copy)]
pub enum PageKind {
    Appearance,
    Behavior,
}

pub struct SettingsPage {
    pub root: gtk4::Box,
    body: gtk4::Box,
    kind: PageKind,
    sources: RefCell<Vec<(&'static str, adw::ActionRow, gtk4::Button)>>,
    pub preview: Option<Rc<Preview>>,
}

impl SettingsPage {
    pub fn build(editor: &Rc<Editor>, kind: PageKind) -> Rc<Self> {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        let body = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        body.set_vexpand(true);
        let preview = if matches!(kind, PageKind::Appearance) {
            let layer = editor.doc.borrow().layer_roles().0;
            let preview = Preview::new(&layer);
            let container = preview::compact(editor, &preview);
            ui::margins(&container, 24);
            root.append(&container);
            editor.previews.borrow_mut().push(Rc::downgrade(&preview));
            Some(preview)
        } else {
            None
        };
        root.append(&body);
        let page = Rc::new(Self {
            root,
            body,
            kind,
            sources: RefCell::new(Vec::new()),
            preview,
        });
        page.refresh(editor);
        page
    }

    pub fn refresh(&self, editor: &Rc<Editor>) {
        ui::clear(&self.body);
        self.sources.borrow_mut().clear();
        let page = adw::PreferencesPage::new();
        match self.kind {
            PageKind::Appearance => {
                let switch = ui::group("Choose a Theme");
                switch.set_description(Some(
                    "Select a theme to preview it above. Switch saves the valid draft and reloads the Touch Bar.",
                ));
                let names = editor.doc.borrow().known_themes();
                let strings: Vec<&str> = names.iter().map(String::as_str).collect();
                let active = editor
                    .doc
                    .borrow()
                    .effective()
                    .ok()
                    .and_then(|config| config.active_theme);
                let selected = active
                    .as_ref()
                    .and_then(|theme| names.iter().position(|name| name == theme))
                    .unwrap_or(0);
                let chooser = ui::combo("Preview Theme", &strings, selected as u32);
                let preview_names = names.clone();
                let apply = gtk4::Button::with_label("Switch to Theme");
                apply.add_css_class("suggested-action");
                apply.set_valign(gtk4::Align::Center);
                chooser.add_suffix(&apply);
                if let Some(preview) = &self.preview {
                    preview.audition_theme(names.get(selected).cloned());
                    chooser.connect_selected_notify(glib::clone!(
                        #[strong]
                        editor,
                        #[weak]
                        preview,
                        #[weak]
                        apply,
                        move |row| {
                            let Some(name) = preview_names.get(row.selected() as usize).cloned()
                            else {
                                return;
                            };
                            preview.audition_theme(Some(name.clone()));
                            apply.set_sensitive(
                                editor
                                    .doc
                                    .borrow()
                                    .effective()
                                    .ok()
                                    .and_then(|config| config.active_theme)
                                    .as_deref()
                                    != Some(name.as_str()),
                            );
                            editor.run_pipeline();
                        }
                    ));
                    editor.run_pipeline();
                }
                apply.set_sensitive(
                    names
                        .get(selected)
                        .is_some_and(|name| active.as_deref() != Some(name.as_str())),
                );
                apply.connect_clicked(glib::clone!(
                    #[strong]
                    editor,
                    #[weak]
                    chooser,
                    move |_| {
                        let Some(theme) = names.get(chooser.selected() as usize) else {
                            return;
                        };
                        let result = {
                            let mut doc = editor.doc.borrow_mut();
                            doc.set_active_theme(theme);
                            doc.save().map_err(|error| format!("{error:#}"))
                        };
                        editor.save_feedback.borrow_mut().record(result);
                        editor.run_pipeline();
                    }
                ));
                switch.add(&chooser);
                page.add(&switch);

                let color = ui::group("Color Theme");
                self.scalar(&color, editor, "ThemesEnabled", "Color Themes");
                self.theme_startup(&color, editor);
                let custom = adw::ActionRow::builder()
                    .use_markup(false)
                    .title("Custom Themes")
                    .subtitle("View built-in palettes, override colors, or create a theme")
                    .activatable(true)
                    .build();
                custom.add_suffix(&gtk4::Image::from_icon_name("go-next-symbolic"));
                custom.connect_activated(glib::clone!(
                    #[strong]
                    editor,
                    move |row| themes_dialog(&editor, row)
                ));
                color.add(&custom);
                page.add(&color);
                let background = ui::group("Background");
                self.scalar(&background, editor, "DayCycleEnabled", "Day Cycle");
                let bands = adw::ActionRow::builder()
                    .use_markup(false)
                    .title("Time-of-Day Colors")
                    .subtitle("Edit names, hours, and colors for the background bands")
                    .activatable(true)
                    .build();
                bands.add_suffix(&gtk4::Image::from_icon_name("go-next-symbolic"));
                bands.connect_activated(glib::clone!(
                    #[strong]
                    editor,
                    move |row| bands_dialog(&editor, row, None)
                ));
                background.add(&bands);
                page.add(&background);
                let style = ui::group("Button Style");
                self.scalar(&style, editor, "ShowButtonOutlines", "Button Outlines");
                let font = ui::entry(
                    "Font Pattern",
                    editor
                        .doc
                        .borrow()
                        .setting("FontTemplate")
                        .as_str()
                        .unwrap_or(":bold"),
                );
                let typography = adw::ActionRow::builder()
                    .use_markup(false)
                    .title("Label Font")
                    .subtitle("Choose a font; inspect the current pattern in Advanced Typography")
                    .build();
                // A fontconfig pattern can express more than Pango's font
                // description. Offer a choice without pretending a fixed
                // Pango default represents the current inherited pattern.
                let chooser = gtk4::Button::with_label("Choose…");
                chooser.set_valign(gtk4::Align::Center);
                typography.set_activatable_widget(Some(&chooser));
                chooser.connect_clicked(glib::clone!(
                    #[weak]
                    font,
                    move |button| {
                        let parent = button.root().and_downcast::<gtk4::Window>();
                        gtk4::FontDialog::new().choose_font(
                            parent.as_ref(),
                            None,
                            None::<&gtk4::gio::Cancellable>,
                            glib::clone!(
                                #[weak]
                                font,
                                move |result| {
                                    let Ok(desc) = result else {
                                        return;
                                    };
                                    let family = desc.family().unwrap_or_else(|| "Sans".into());
                                    let weight = if desc.weight() >= gtk4::pango::Weight::Bold {
                                        "Bold"
                                    } else {
                                        "Regular"
                                    };
                                    let italic = if desc.style() == gtk4::pango::Style::Normal {
                                        ""
                                    } else {
                                        " Italic"
                                    };
                                    font.set_text(&format!("{family}:style={weight}{italic}"));
                                }
                            ),
                        );
                    }
                ));
                typography.add_suffix(&chooser);
                style.add(&typography);
                let advanced = adw::ExpanderRow::builder()
                    .use_markup(false)
                    .title("Advanced Typography")
                    .subtitle("Use a fontconfig pattern")
                    .build();
                font.connect_changed(glib::clone!(
                    #[strong]
                    editor,
                    move |row| {
                        editor.doc.borrow_mut().set_setting(
                            "FontTemplate",
                            Some(toml::Value::String(row.text().into())),
                        );
                        editor.run_pipeline();
                    }
                ));
                advanced.add_row(&font);
                let source = adw::ActionRow::builder()
                    .use_markup(false)
                    .title("Font Pattern Source")
                    .build();
                self.source(editor, "FontTemplate", &source);
                advanced.add_row(&source);
                style.add(&advanced);
                page.add(&style);
                if let Some(preview) = &self.preview {
                    let context = ui::group("");
                    let options = adw::ExpanderRow::builder()
                        .use_markup(false)
                        .title("Preview Options")
                        .subtitle("Audition a layer or theme without changing the draft")
                        .build();
                    let names: Vec<String> = editor
                        .doc
                        .borrow()
                        .layer_snapshot()
                        .into_iter()
                        .map(|l| l.name)
                        .collect();
                    let strings: Vec<&str> = names.iter().map(String::as_str).collect();
                    let (current_layer, current_theme) = preview.context();
                    let selected = names
                        .iter()
                        .position(|name| *name == current_layer)
                        .unwrap_or(0);
                    if names
                        .get(selected)
                        .is_some_and(|name| *name != current_layer)
                    {
                        preview.set_layer(&names[selected]);
                    }
                    let layer = ui::combo("Preview Layer", &strings, selected as u32);
                    layer.connect_selected_notify(glib::clone!(
                        #[strong]
                        editor,
                        #[weak]
                        preview,
                        move |row| {
                            preview.set_layer(&names[row.selected() as usize]);
                            editor.run_pipeline();
                        }
                    ));
                    let mut themes = vec!["Document Settings".to_owned()];
                    themes.extend(editor.doc.borrow().known_themes());
                    let strings: Vec<&str> = themes.iter().map(String::as_str).collect();
                    let selected = current_theme
                        .and_then(|name| themes.iter().position(|theme| *theme == name))
                        .unwrap_or(0);
                    if selected == 0 {
                        preview.audition_theme(None);
                    }
                    let theme = ui::combo("Audition Theme", &strings, selected as u32);
                    theme.connect_selected_notify(glib::clone!(
                        #[strong]
                        editor,
                        #[weak]
                        preview,
                        move |row| {
                            preview.audition_theme(
                                (row.selected() > 0)
                                    .then(|| themes[row.selected() as usize].clone()),
                            );
                            editor.run_pipeline();
                        }
                    ));
                    options.add_row(&layer);
                    options.add_row(&theme);
                    context.add(&options);
                    page.add(&context);
                }
            }
            PageKind::Behavior => {
                let switching = ui::group("Layer Switching");
                let layers = editor.doc.borrow().layer_snapshot();
                let (startup, function) = editor.doc.borrow().layer_roles();
                for (title, is_startup) in
                    [("Startup Layer", true), ("Layer While Holding Fn", false)]
                {
                    let base: dfr_config::ConfigProxy =
                        toml::from_str(editor.doc.borrow().base_text()).unwrap();
                    let permits_none = base.fn_layer.is_none();
                    let mut names: Vec<String> = if is_startup || !permits_none {
                        Vec::new()
                    } else {
                        vec!["None".into()]
                    };
                    names.extend(layers.iter().map(|l| l.name.clone()));
                    let current = if is_startup {
                        Some(startup.as_str())
                    } else {
                        function.as_deref()
                    };
                    let strings: Vec<&str> = names.iter().map(String::as_str).collect();
                    let row = ui::combo(
                        title,
                        &strings,
                        current
                            .and_then(|n| names.iter().position(|name| name == n))
                            .unwrap_or(0) as u32,
                    );
                    self.source(
                        editor,
                        if is_startup {
                            "DefaultLayer"
                        } else {
                            "FnLayer"
                        },
                        row.upcast_ref(),
                    );
                    row.connect_selected_notify(glib::clone!(
                        #[strong]
                        editor,
                        move |row| {
                            let name = if !is_startup && permits_none && row.selected() == 0 {
                                None
                            } else {
                                Some(names[row.selected() as usize].clone())
                            };
                            editor.doc.borrow_mut().set_layer_role(name, is_startup);
                            editor.restructured();
                        }
                    ));
                    switching.add(&row);
                }
                let timing = editor
                    .doc
                    .borrow()
                    .setting("DoublePressSwitchLayers")
                    .as_integer()
                    .unwrap_or(0);
                let enabled = adw::SwitchRow::builder()
                    .use_markup(false)
                    .title("Double-Press Fn")
                    .subtitle("Swap the startup layer with two Fn presses")
                    .active(timing > 0)
                    .build();
                let window = ui::spin(
                    "Double-Press Window (ms)",
                    1.0,
                    u32::MAX as f64,
                    if timing > 0 { timing } else { 300 } as f64,
                );
                window.set_sensitive(timing > 0);
                self.source(editor, "DoublePressSwitchLayers", window.upcast_ref());
                enabled.connect_active_notify(glib::clone!(
                    #[strong]
                    editor,
                    #[weak]
                    window,
                    move |row| {
                        window.set_sensitive(row.is_active());
                        editor.doc.borrow_mut().set_setting(
                            "DoublePressSwitchLayers",
                            Some(toml::Value::Integer(if row.is_active() {
                                window.value() as i64
                            } else {
                                0
                            })),
                        );
                        editor.run_pipeline();
                    }
                ));
                window.connect_value_notify(glib::clone!(
                    #[strong]
                    editor,
                    #[weak]
                    enabled,
                    move |row| {
                        if enabled.is_active() {
                            editor.doc.borrow_mut().set_setting(
                                "DoublePressSwitchLayers",
                                Some(toml::Value::Integer(row.value() as i64)),
                            );
                            editor.run_pipeline();
                        }
                    }
                ));
                switching.add(&enabled);
                switching.add(&window);
                page.add(&switching);
                let display = ui::group("Display");
                display.set_description(Some(
                    "The preview does not simulate hardware brightness or wear protection.",
                ));
                self.scalar(
                    &display,
                    editor,
                    "AdaptiveBrightness",
                    "Follow Display Brightness",
                );
                self.scalar(&display, editor, "ActiveBrightness", "Brightness Level");
                let advanced = adw::ExpanderRow::builder()
                    .use_markup(false)
                    .title("Advanced Display")
                    .build();
                let shift = self.scalar_row(editor, "EnablePixelShift", "Pixel Shift");
                advanced.add_row(&shift);
                display.add(&advanced);
                page.add(&display);
                let battery = ui::group("Battery");
                for (name, title) in [
                    ("BatteryCycleEnabled", "Tap to Cycle Device Batteries"),
                    (
                        "BatteryCycleTimeoutSecs",
                        "Return to System Battery (seconds)",
                    ),
                    ("BatteryWarningThreshold", "Warning Below (%)"),
                    ("BatteryCriticalThreshold", "Critical Below (%)"),
                    ("BatteryWarningColor", "Warning Accent"),
                    ("BatteryCriticalColor", "Critical Accent"),
                ] {
                    self.scalar(&battery, editor, name, title);
                }
                page.add(&battery);
                let pet = ui::group("Pet");
                pet.set_description(Some("Adds a Pet button and playground layer. Disabling it keeps your animal and activity settings."));
                self.scalar(&pet, editor, "PetEnabled", "Pet");
                self.scalar(&pet, editor, "PetVariant", "Animal");
                self.scalar(&pet, editor, "PetEnergy", "Activity Level");
                page.add(&pet);
            }
        }
        self.body.append(&page);
        self.update_feedback(editor);
    }

    fn source(&self, editor: &Rc<Editor>, name: &'static str, row: &adw::ActionRow) {
        let reset = gtk4::Button::from_icon_name("edit-undo-symbolic");
        reset.set_tooltip_text(Some("Use Base Setting"));
        reset.set_valign(gtk4::Align::Center);
        reset.connect_clicked(glib::clone!(
            #[strong]
            editor,
            move |_| {
                editor.doc.borrow_mut().reset_global(name);
                editor.restructured();
            }
        ));
        row.add_suffix(&reset);
        self.sources.borrow_mut().push((name, row.clone(), reset));
    }

    fn scalar(
        &self,
        group: &adw::PreferencesGroup,
        editor: &Rc<Editor>,
        name: &'static str,
        title: &str,
    ) {
        group.add(&self.scalar_row(editor, name, title));
    }

    fn scalar_row(&self, editor: &Rc<Editor>, name: &'static str, title: &str) -> gtk4::Widget {
        let knob = dfr_config::KNOBS.iter().find(|k| k.name == name).unwrap();
        let value = editor.doc.borrow().setting(name);
        match knob.kind {
            "bool" => {
                let row = adw::SwitchRow::builder()
                    .use_markup(false)
                    .title(title)
                    .active(value.as_bool().unwrap_or(false))
                    .build();
                self.source(editor, name, row.upcast_ref());
                row.connect_active_notify(glib::clone!(
                    #[strong]
                    editor,
                    move |row| {
                        editor
                            .doc
                            .borrow_mut()
                            .set_setting(name, Some(toml::Value::Boolean(row.is_active())));
                        editor.run_pipeline();
                    }
                ));
                row.upcast()
            }
            "u32" => {
                let (min, max) = match name {
                    "ActiveBrightness" => (0., 255.),
                    "BatteryCycleTimeoutSecs" => (1., 3600.),
                    "BatteryWarningThreshold" | "BatteryCriticalThreshold" => (0., 100.),
                    _ => (0., u32::MAX as f64),
                };
                let row = ui::spin(title, min, max, value.as_integer().unwrap_or(0) as f64);
                self.source(editor, name, row.upcast_ref());
                row.connect_value_notify(glib::clone!(
                    #[strong]
                    editor,
                    move |row| {
                        editor
                            .doc
                            .borrow_mut()
                            .set_setting(name, Some(toml::Value::Integer(row.value() as i64)));
                        editor.run_pipeline();
                    }
                ));
                row.upcast()
            }
            _ => {
                let names: Vec<&str> = knob
                    .constraint
                    .unwrap_or("")
                    .split(',')
                    .map(str::trim)
                    .collect();
                let row = ui::combo(
                    title,
                    &names,
                    names
                        .iter()
                        .position(|n| Some(*n) == value.as_str())
                        .unwrap_or(0) as u32,
                );
                self.source(editor, name, row.upcast_ref());
                row.connect_selected_notify(glib::clone!(
                    #[strong]
                    editor,
                    move |row| {
                        editor.doc.borrow_mut().set_setting(
                            name,
                            Some(toml::Value::String(names[row.selected() as usize].into())),
                        );
                        editor.run_pipeline();
                    }
                ));
                row.upcast()
            }
        }
    }

    fn theme_startup(&self, group: &adw::PreferencesGroup, editor: &Rc<Editor>) {
        let current = editor.doc.borrow().effective().unwrap().active_theme;
        let mode = ui::combo(
            "Startup Theme",
            &["Remember Last Touch Bar Choice", "Use a Fixed Theme"],
            u32::from(current.is_some()),
        );
        self.source(editor, "ActiveTheme", mode.upcast_ref());
        let mut names = vec!["Choose…".to_owned()];
        names.extend(editor.doc.borrow().known_themes());
        if let Some(current) = &current {
            if !names.contains(current) {
                names.push(current.clone());
            }
        }
        let strings: Vec<&str> = names.iter().map(String::as_str).collect();
        let theme = ui::combo(
            "Fixed Theme",
            &strings,
            current
                .and_then(|n| names.iter().position(|name| *name == n))
                .unwrap_or(0) as u32,
        );
        theme.set_visible(mode.selected() == 1);
        mode.connect_selected_notify(glib::clone!(#[strong] editor, #[weak] theme, move |row| {
            if row.selected() == 0 {
                let base: dfr_config::ConfigProxy = toml::from_str(editor.doc.borrow().base_text()).unwrap();
                if base.active_theme.is_some() {
                    ui::error_dialog(row, "Base Uses a Fixed Theme", "The user overlay cannot remove a theme pinned by the base file. Choose a fixed theme here, or edit the base file separately.");
                    row.set_selected(1); return;
                }
                editor.doc.borrow_mut().reset_global("ActiveTheme");
            } else if editor.doc.borrow().effective().unwrap().active_theme.is_none() {
                editor.doc.borrow_mut().user_mut().active_theme = Some(String::new());
            }
            theme.set_visible(row.selected() == 1); editor.run_pipeline();
        }));
        theme.connect_selected_notify(glib::clone!(
            #[strong]
            editor,
            move |row| {
                editor.doc.borrow_mut().user_mut().active_theme = Some(if row.selected() == 0 {
                    String::new()
                } else {
                    names[row.selected() as usize].clone()
                });
                editor.run_pipeline();
            }
        ));
        group.add(&mode);
        group.add(&theme);
    }

    pub fn update_feedback(&self, editor: &Rc<Editor>) {
        let doc = editor.doc.borrow();
        let errors = doc.validate();
        let battery_error = errors
            .iter()
            .find(|e| e.contains("BatteryCriticalThreshold"));
        for (name, row, reset) in self.sources.borrow().iter() {
            let source = if doc.has_override(name) {
                "Customized"
            } else {
                "Using Base Setting"
            };
            reset.set_sensitive(doc.has_override(name));
            let help = if *name == "ActiveBrightness" {
                if doc.setting("AdaptiveBrightness").as_bool() == Some(true) {
                    "Maximum while following display brightness (0–255)"
                } else {
                    "Fixed brightness (0–255)"
                }
            } else {
                dfr_config::KNOBS
                    .iter()
                    .find(|k| k.name == *name)
                    .map(|k| k.doc)
                    .unwrap_or("")
            };
            let error = if matches!(
                *name,
                "BatteryWarningThreshold" | "BatteryCriticalThreshold"
            ) {
                battery_error
            } else {
                None
            };
            row.set_subtitle(&format!(
                "{source} · {help}{}",
                error.map(|e| format!("\n{e}")).unwrap_or_default()
            ));
            if error.is_some() {
                row.add_css_class("error");
            } else {
                row.remove_css_class("error");
            }
            if matches!(*name, "PetVariant" | "PetEnergy") {
                row.set_sensitive(doc.setting("PetEnabled").as_bool() == Some(true));
            }
        }
    }
}

fn bands_for(editor: &Rc<Editor>, theme: Option<&str>) -> Vec<dfr_config::BandConfig> {
    let effective = editor.doc.borrow().effective().unwrap();
    if let Some(name) = theme {
        if let Some(bands) = effective
            .themes
            .as_ref()
            .and_then(|themes| themes.get(name))
            .and_then(|t| t.day_cycle_bands.clone())
        {
            return bands;
        }
        // Built-in theme bands are authored in the same public wire shape as custom themes.
        #[derive(serde::Deserialize)]
        struct Seed {
            #[serde(rename = "Theme")]
            themes: Vec<SeedTheme>,
        }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct SeedTheme {
            name: String,
            day_cycle_bands: Option<Vec<dfr_config::BandConfig>>,
        }
        if let Ok(seed) =
            toml::from_str::<Seed>(include_str!("../../../share/tiny-dfr/themes.toml"))
        {
            if let Some(bands) = seed
                .themes
                .into_iter()
                .find(|t| t.name == name)
                .and_then(|t| t.day_cycle_bands)
            {
                return bands;
            }
        }
    }
    effective.day_cycle_bands.unwrap_or_else(|| {
        dfr_config::daycycle::DayCycle::default_bands()
            .into_iter()
            .map(|b| dfr_config::BandConfig {
                name: b.name,
                start_hour: b.start_hour,
                color: format!("#{:02X}{:02X}{:02X}", b.color.0, b.color.1, b.color.2),
            })
            .collect()
    })
}

fn write_bands(
    editor: &Rc<Editor>,
    theme: Option<&str>,
    bands: Vec<dfr_config::BandConfig>,
) -> anyhow::Result<()> {
    if let Some(name) = theme {
        let mut value = editor
            .doc
            .borrow()
            .effective()?
            .themes
            .and_then(|themes| themes.get(name).cloned())
            .unwrap_or_default();
        value.day_cycle_bands = Some(bands);
        editor.doc.borrow_mut().set_theme(name, value)?;
    } else {
        editor.doc.borrow_mut().set_bands(bands);
    }
    editor.run_pipeline();
    Ok(())
}

fn bands_dialog(editor: &Rc<Editor>, parent: &impl IsA<gtk4::Widget>, theme: Option<String>) {
    let dialog = adw::Dialog::builder()
        .title("Time-of-Day Colors")
        .content_width(480)
        .content_height(600)
        .build();
    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    fill_bands(&body, editor, theme.as_deref());
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&body));
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(parent));
}

fn fill_bands(body: &gtk4::Box, editor: &Rc<Editor>, theme: Option<&str>) {
    ui::clear(body);
    let page = adw::PreferencesPage::new();
    let group = ui::group("Background Bands");
    let bands = bands_for(editor, theme);
    let theme = theme.map(str::to_owned);
    group.set_description(Some("Bands are shown in hour order. Names and start hours must be unique. Invalid edits stay in the draft until repaired."));
    let feedback = gtk4::Label::new(None);
    feedback.set_wrap(true);
    feedback.set_xalign(0.0);
    feedback.add_css_class("error");
    group.add(&feedback);
    let refresh_feedback: Rc<dyn Fn()> = Rc::new(glib::clone!(
        #[strong]
        editor,
        #[weak]
        feedback,
        #[strong]
        theme,
        move || {
            let bands = bands_for(&editor, theme.as_deref());
            let error = dfr_config::parse_bands(&bands)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            feedback.set_text(&error);
            feedback.set_visible(!error.is_empty());
        }
    ));
    let mut display: Vec<usize> = (0..bands.len()).collect();
    display.sort_by_key(|i| bands[*i].start_hour);
    for index in display {
        let band = &bands[index];
        let row = adw::ExpanderRow::builder()
            .use_markup(false)
            .title(&band.name)
            .subtitle(format!(
                "Starts at {:02}:00 · {}",
                band.start_hour, band.color
            ))
            .build();
        let name = ui::entry("Name", &band.name);
        name.connect_changed(glib::clone!(
            #[strong]
            editor,
            #[strong]
            theme,
            #[weak]
            row,
            #[strong]
            refresh_feedback,
            move |entry| {
                let mut bands = bands_for(&editor, theme.as_deref());
                bands[index].name = entry.text().into();
                row.set_title(&entry.text());
                if let Err(error) = write_bands(&editor, theme.as_deref(), bands) {
                    ui::error_dialog(entry, "Cannot Edit Bands", &error.to_string());
                }
                refresh_feedback();
            }
        ));
        row.add_row(&name);
        let hour = ui::spin("Start Hour", 0., 23., band.start_hour as f64);
        hour.connect_value_notify(glib::clone!(
            #[strong]
            editor,
            #[strong]
            theme,
            #[weak]
            row,
            #[strong]
            refresh_feedback,
            move |spin| {
                let mut bands = bands_for(&editor, theme.as_deref());
                bands[index].start_hour = spin.value() as u8;
                row.set_subtitle(&format!(
                    "Starts at {:02}:00 · {}",
                    bands[index].start_hour, bands[index].color
                ));
                if let Err(error) = write_bands(&editor, theme.as_deref(), bands) {
                    ui::error_dialog(spin, "Cannot Edit Bands", &error.to_string());
                }
                refresh_feedback();
            }
        ));
        row.add_row(&hour);
        let color = ui::color(
            "Color (#RRGGBB)",
            &band.color,
            glib::clone!(
                #[strong]
                editor,
                #[strong]
                theme,
                #[weak]
                row,
                #[strong]
                refresh_feedback,
                move |hex| {
                    let mut bands = bands_for(&editor, theme.as_deref());
                    bands[index].color = hex;
                    row.set_subtitle(&format!(
                        "Starts at {:02}:00 · {}",
                        bands[index].start_hour, bands[index].color
                    ));
                    let _ = write_bands(&editor, theme.as_deref(), bands);
                    refresh_feedback();
                }
            ),
        );
        row.add_row(&color);
        let remove = gtk4::Button::from_icon_name("user-trash-symbolic");
        remove.set_tooltip_text(Some("Delete Band"));
        remove.set_valign(gtk4::Align::Center);
        remove.set_sensitive(bands.len() > 1);
        remove.connect_clicked(glib::clone!(
            #[strong]
            editor,
            #[weak]
            body,
            #[strong]
            theme,
            move |button| {
                let mut bands = bands_for(&editor, theme.as_deref());
                if bands.len() <= 1 {
                    return;
                }
                bands.remove(index);
                match write_bands(&editor, theme.as_deref(), bands) {
                    Ok(()) => fill_bands(&body, &editor, theme.as_deref()),
                    Err(error) => {
                        ui::error_dialog(button, "Cannot Delete Band", &error.to_string())
                    }
                }
            }
        ));
        row.add_suffix(&remove);
        group.add(&row);
    }
    let add = gtk4::Button::with_label("Add Band");
    add.set_sensitive(bands.len() < 24);
    add.connect_clicked(glib::clone!(
        #[strong]
        editor,
        #[weak]
        body,
        #[strong]
        theme,
        move |button| {
            let mut bands = bands_for(&editor, theme.as_deref());
            let Some(hour) = (0..24).find(|h| !bands.iter().any(|b| b.start_hour == *h)) else {
                return;
            };
            let mut n = bands.len() + 1;
            while bands.iter().any(|b| b.name == format!("Band {n}")) {
                n += 1;
            }
            bands.push(dfr_config::BandConfig {
                name: format!("Band {n}"),
                start_hour: hour,
                color: "#1A1B1E".into(),
            });
            match write_bands(&editor, theme.as_deref(), bands) {
                Ok(()) => fill_bands(&body, &editor, theme.as_deref()),
                Err(error) => ui::error_dialog(button, "Cannot Add Band", &error.to_string()),
            }
        }
    ));
    group.set_header_suffix(Some(&add));
    let reset = gtk4::Button::with_label("Use Base Setting");
    reset.connect_clicked(glib::clone!(
        #[strong]
        editor,
        #[weak]
        body,
        #[strong]
        theme,
        move |_| {
            if let Some(name) = theme.as_deref() {
                let mut value = editor
                    .doc
                    .borrow()
                    .effective()
                    .unwrap()
                    .themes
                    .and_then(|themes| themes.get(name).cloned())
                    .unwrap_or_default();
                value.day_cycle_bands = None;
                let _ = editor.doc.borrow_mut().set_theme(name, value);
            } else {
                editor.doc.borrow_mut().reset_global("DayCycleBands");
            }
            editor.run_pipeline();
            fill_bands(&body, &editor, theme.as_deref());
        }
    ));
    group.add(&reset);
    page.add(&group);
    body.append(&page);
    refresh_feedback();
}

fn themes_dialog(editor: &Rc<Editor>, parent: &adw::ActionRow) {
    let dialog = adw::Dialog::builder()
        .title("Custom Themes")
        .content_width(480)
        .content_height(560)
        .build();
    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    fill_themes(&body, editor);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&body));
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(parent));
}

fn fill_themes(body: &gtk4::Box, editor: &Rc<Editor>) {
    ui::clear(body);
    let page = adw::PreferencesPage::new();
    let group = ui::group("Themes");
    let names = editor.doc.borrow().known_themes();
    group.set_description(Some(&format!(
        "{} of {} theme slots used. Built-in palettes can be customized with an override.",
        names.len(),
        dfr_config::MAX_THEMES
    )));
    let builtins = dfr_config::themes::builtin_themes().unwrap().names;
    for name in &names {
        let source = if editor
            .doc
            .borrow()
            .user()
            .themes
            .as_ref()
            .is_some_and(|themes| themes.contains_key(name))
        {
            "Customized"
        } else if builtins.contains(name) {
            "Built-in"
        } else {
            "Using Base Setting"
        };
        let row = adw::ActionRow::builder()
            .use_markup(false)
            .title(name)
            .subtitle(source)
            .activatable(true)
            .build();
        row.add_suffix(&gtk4::Image::from_icon_name("go-next-symbolic"));
        let name = name.clone();
        row.connect_activated(glib::clone!(
            #[strong]
            editor,
            #[weak]
            body,
            move |row| theme_detail(&editor, row, &name, &body)
        ));
        group.add(&row);
    }
    page.add(&group);
    let create = ui::group("New Theme");
    let entry = ui::entry("Theme Name", "");
    let add = gtk4::Button::with_label("Create Theme");
    add.set_valign(gtk4::Align::Center);
    add.set_sensitive(names.len() < dfr_config::MAX_THEMES);
    entry.add_suffix(&add);
    add.connect_clicked(glib::clone!(
        #[strong]
        editor,
        #[weak]
        body,
        #[weak]
        entry,
        move |button| {
            let name = entry.text().trim().to_owned();
            if name.is_empty()
                || name.chars().any(char::is_control)
                || editor.doc.borrow().known_themes().contains(&name)
            {
                ui::error_dialog(
                    button,
                    "Choose a Theme Name",
                    "Enter a unique, nonempty name. Open an existing theme to override it.",
                );
                return;
            }
            let result = editor.doc.borrow_mut().set_theme(&name, Default::default());
            match result {
                Ok(()) => {
                    editor.restructured();
                    fill_themes(&body, &editor);
                }
                Err(error) => ui::error_dialog(button, "Cannot Create Theme", &error.to_string()),
            }
        }
    ));
    create.add(&entry);
    page.add(&create);
    body.append(&page);
}

fn theme_detail(editor: &Rc<Editor>, parent: &adw::ActionRow, name: &str, list: &gtk4::Box) {
    let dialog = adw::Dialog::builder()
        .title(name)
        .content_width(480)
        .content_height(640)
        .build();
    let page = adw::PreferencesPage::new();
    let group = ui::group("Accent Palette");
    let effective = editor.doc.borrow().effective().unwrap();
    let mut set = dfr_config::ThemeSet::resolve(true, effective.themes)
        .unwrap_or_else(|_| dfr_config::themes::builtin_themes().unwrap());
    set.apply(name);
    let palette = set.palette();
    group.set_description(Some(
        "Colors shown are the effective palette. Editing a color stages a theme override.",
    ));
    for accent_name in crate::layers::ACCENTS {
        let accent = crate::layers::accent_from_name(accent_name).unwrap();
        let color = palette.accent(accent);
        let hex = format!(
            "#{:02X}{:02X}{:02X}",
            (color.0 * 255.).round() as u8,
            (color.1 * 255.).round() as u8,
            (color.2 * 255.).round() as u8
        );
        let row = ui::color(
            accent_name,
            &hex,
            glib::clone!(
                #[strong]
                editor,
                #[to_owned]
                name,
                move |hex| {
                    let mut value = editor
                        .doc
                        .borrow()
                        .effective()
                        .unwrap()
                        .themes
                        .and_then(|themes| themes.get(&name).cloned())
                        .unwrap_or_default();
                    value
                        .accents
                        .get_or_insert_with(Default::default)
                        .insert(accent, hex);
                    let _ = editor.doc.borrow_mut().set_theme(&name, value);
                    editor.run_pipeline();
                }
            ),
        );
        group.add(&row);
    }
    page.add(&group);
    let background = ui::group("Background");
    let bands = adw::ActionRow::builder()
        .use_markup(false)
        .title("Time-of-Day Colors")
        .subtitle("Override this theme’s background bands")
        .activatable(true)
        .build();
    bands.connect_activated(glib::clone!(
        #[strong]
        editor,
        #[to_owned]
        name,
        move |row| bands_dialog(&editor, row, Some(name.clone()))
    ));
    background.add(&bands);
    page.add(&background);
    let manage = ui::group("Theme Options");
    let builtin = dfr_config::themes::builtin_themes().unwrap().contains(name);
    let remove = gtk4::Button::with_label(if builtin {
        "Reset Override"
    } else {
        "Delete Custom Theme"
    });
    remove.connect_clicked(glib::clone!(#[strong] editor, #[to_owned] name, #[weak] dialog, #[weak] list, move |button| {
        let doc = editor.doc.borrow(); let effective = doc.effective().unwrap();
        let in_use = !builtin && (effective.active_theme.as_deref() == Some(&name) || doc.layer_snapshot().iter().any(|l| l.keys.iter().any(|b| matches!(&b.action,
            dfr_config::ButtonAction::ApplyTheme(theme) | dfr_config::ButtonAction::ApplyThemeLayer {theme,..} if theme == &name))));
        drop(doc);
        if in_use { ui::error_dialog(button,"Theme Is in Use","Choose another startup theme and repair buttons that use this theme before deleting it."); return; }
        editor.doc.borrow_mut().remove_theme(&name); editor.restructured(); fill_themes(&list,&editor); dialog.close();
    }));
    manage.add(&remove);
    page.add(&manage);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(parent));
}
