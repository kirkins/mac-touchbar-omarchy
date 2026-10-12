//! Contextual static previews use the daemon's renderer and isolated audition state.
use crate::{
    layers::{action_summary, button_title, Editor},
    model::Document,
    ui,
};
use chrono::Timelike as _;
use gtk4::{glib, prelude::*};
use libadwaita::{self as adw, prelude::*};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

pub const HEIGHT: i32 = 60;

pub struct Preview {
    pub area: gtk4::DrawingArea,
    pub diagnostic: gtk4::Label,
    frame: RefCell<Option<cairo::ImageSurface>>,
    layer: RefCell<String>,
    width: Cell<u16>,
    theme: RefCell<Option<String>>,
    hour: Cell<Option<u32>>,
    one_to_one: Cell<bool>,
    selected: Cell<Option<usize>>,
    bounds: Cell<Option<(f64, f64)>>,
    title: RefCell<Option<glib::WeakRef<gtk4::Label>>>,
    pub slots: RefCell<Vec<(String, Option<usize>)>>,
}

impl Preview {
    pub fn new(layer: &str) -> Rc<Self> {
        let area = gtk4::DrawingArea::builder()
            .height_request(84)
            .hexpand(true)
            .build();
        let diagnostic = gtk4::Label::new(Some("Updating preview…"));
        diagnostic.set_wrap(true);
        diagnostic.set_xalign(0.0);
        diagnostic.add_css_class("dim-label");
        let preview = Rc::new(Self {
            area,
            diagnostic,
            frame: RefCell::new(None),
            layer: RefCell::new(layer.into()),
            width: Cell::new(2008),
            theme: RefCell::new(None),
            hour: Cell::new(None),
            one_to_one: Cell::new(false),
            selected: Cell::new(None),
            bounds: Cell::new(None),
            title: RefCell::new(None),
            slots: RefCell::new(Vec::new()),
        });
        preview.area.set_draw_func(glib::clone!(
            #[weak]
            preview,
            move |area, context, w, h| preview.paint(area, context, w, h)
        ));
        preview
    }

    pub fn context(&self) -> (String, Option<String>) {
        (self.layer.borrow().clone(), self.theme.borrow().clone())
    }

    pub fn audition_theme(&self, theme: Option<String>) {
        *self.theme.borrow_mut() = theme;
        self.invalidate();
    }

    pub fn copy_context(&self, other: &Self) {
        self.set_layer(&other.layer.borrow());
        self.width.set(other.width.get());
        *self.theme.borrow_mut() = other.theme.borrow().clone();
        self.hour.set(other.hour.get());
        self.selected.set(other.selected.get());
    }

    pub fn set_selected(&self, index: Option<usize>) {
        self.selected.set(index);
        self.invalidate();
    }

    pub fn set_layer(&self, layer: &str) {
        *self.layer.borrow_mut() = layer.into();
        if let Some(label) = self
            .title
            .borrow()
            .as_ref()
            .and_then(|label| label.upgrade())
        {
            label.set_text(&format!("Preview — {layer}"));
        }
        self.invalidate();
    }

    pub fn invalidate(&self) {
        // An invalid or unfinished current draft never presents an old valid frame.
        self.frame.borrow_mut().take();
        self.bounds.set(None);
        self.diagnostic.set_text("Updating preview…");
        self.diagnostic.set_visible(true);
        self.area.queue_draw();
    }

    pub fn pause(&self, diagnostic: &str) {
        self.slots.borrow_mut().clear();
        self.frame.borrow_mut().take();
        self.bounds.set(None);
        self.diagnostic.set_text(&format!(
            "Preview paused — fix the highlighted fields. {diagnostic}"
        ));
        self.diagnostic.set_visible(true);
        self.area.queue_draw();
    }

    pub fn render(&self, doc: &Document) {
        let dir =
            std::env::temp_dir().join(format!("dfr-configurator-preview-{}", std::process::id()));
        let render = (|| -> anyhow::Result<cairo::ImageSurface> {
            let mut audition = doc.clone();
            if let Some(theme) = self.theme.borrow().clone() {
                audition.user_mut().active_theme = Some(theme);
            }
            let candidate = audition.candidate_text();
            let model =
                dfr_config::parse_config(doc.base_text(), Some(&candidate), self.width.get())?;
            let layer = self.layer.borrow().clone();
            let effective = model
                .layout
                .layers
                .iter()
                .find(|l| l.name == layer)
                .ok_or_else(|| anyhow::anyhow!("Choose a layer to preview"))?;
            let authored = doc
                .layer_snapshot()
                .into_iter()
                .find(|l| l.name == layer)
                .map(|l| l.keys)
                .unwrap_or_default();
            let mut next_authored = 0;
            let mut slots = Vec::new();
            let total: usize = effective.keys.iter().map(|b| b.stretch.unwrap_or(1)).sum();
            let shift = if model.proxy.enable_pixel_shift.unwrap_or(false) {
                dfr_render::PIXEL_SHIFT_WIDTH_PX as f64
            } else {
                0.0
            };
            let spacing = dfr_render::BUTTON_SPACING_PX as f64;
            let unit =
                (self.width.get() as f64 - shift - spacing * (total.saturating_sub(1)) as f64)
                    / total.max(1) as f64;
            let mut position = 0usize;
            self.bounds.set(None);
            for (rendered_index, button) in effective.keys.iter().enumerate() {
                let synthesized_escape = self.width.get() >= 2170 && rendered_index == 0;
                let authored_index =
                    if !synthesized_escape && authored.get(next_authored) == Some(button) {
                        let index = next_authored;
                        next_authored += 1;
                        Some(index)
                    } else {
                        None
                    };
                let size = button.stretch.unwrap_or(1);
                if authored_index.is_some() && authored_index == self.selected.get() {
                    let left = (position as f64 * (unit + spacing)).floor() + shift / 2.0;
                    let width = unit + ((size - 1) as f64 * (unit + spacing)).floor();
                    self.bounds.set(Some((left, width)));
                }
                slots.push((
                    format!(
                        "{} — {}{}",
                        button_title(button),
                        action_summary(&button.action),
                        if authored_index.is_none() {
                            " · Generated control"
                        } else {
                            ""
                        }
                    ),
                    authored_index,
                ));
                position += size;
            }
            *self.slots.borrow_mut() = slots;
            std::fs::create_dir_all(&dir)?;
            let base = dir.join("base.toml");
            let user = dir.join("user.toml");
            std::fs::write(&base, doc.base_text())?;
            std::fs::write(&user, candidate)?;
            // No real theme-state path: this loader can persist its resolved choice.
            let (cfg, mut deck) =
                dfr_render::config::load_config(self.width.get(), &base, Some(&user), None)?;
            let index = *deck
                .names
                .get(&layer)
                .ok_or_else(|| anyhow::anyhow!("Unknown layer {layer}"))?;
            deck.switch_to(index, &mut |_, _| {});
            let portrait = cairo::ImageSurface::create(
                cairo::Format::ARgb32,
                HEIGHT,
                self.width.get() as i32,
            )?;
            deck.slider.pixel_shift_width = 0;
            deck.slider.pixel_shift_x = 0.0;
            let slider = deck.slider_render();
            let hour = self
                .hour
                .get()
                .unwrap_or_else(|| chrono::Local::now().hour());
            deck.current_mut().draw(
                &cfg,
                (self.width.get() as i32, HEIGHT),
                &portrait,
                (0.0, 0.0),
                true,
                slider,
                cfg.day_cycle.background(hour),
            );
            let horizontal = cairo::ImageSurface::create(
                cairo::Format::ARgb32,
                self.width.get() as i32,
                HEIGHT,
            )?;
            let context = cairo::Context::new(&horizontal)?;
            context.translate(0.0, HEIGHT as f64);
            context.rotate((-90.0_f64).to_radians());
            context.set_source_surface(&portrait, 0.0, 0.0)?;
            context.paint()?;
            Ok(horizontal)
        })();
        let _ = std::fs::remove_dir_all(&dir);
        match render {
            Ok(surface) => {
                *self.frame.borrow_mut() = Some(surface);
                let theme = self.theme.borrow().clone().unwrap_or_else(|| {
                    doc.effective()
                        .ok()
                        .and_then(|p| p.active_theme)
                        .unwrap_or_else(|| "Default theme (remembered choice not read)".into())
                });
                let scale = if self.one_to_one.get() { "1:1" } else { "Fit" };
                self.diagnostic
                    .set_text(&format!("{scale} · {theme} · static preview"));
                self.diagnostic.set_visible(true);
            }
            Err(error) => self.pause(&format!("{error:#}")),
        }
        self.area.queue_draw();
    }

    fn paint(&self, area: &gtk4::DrawingArea, context: &cairo::Context, w: i32, h: i32) {
        let frame = self.frame.borrow();
        let Some(surface) = frame.as_ref() else {
            return;
        };
        let scale = if self.one_to_one.get() {
            1.0
        } else {
            (w as f64 / surface.width() as f64)
                .min(h as f64 / HEIGHT as f64)
                .min(1.0)
        };
        let _ = context.save();
        context.translate(
            ((w as f64 - surface.width() as f64 * scale) / 2.0).max(0.0),
            (h as f64 - HEIGHT as f64 * scale) / 2.0,
        );
        context.scale(scale, scale);
        if context.set_source_surface(surface, 0.0, 0.0).is_ok() {
            let _ = context.paint();
        }
        // Selection is a separate overlay; cached renderer pixels stay untouched.
        if let Some((left, width)) = self.bounds.get() {
            let color = area.color();
            context.set_source_rgba(
                color.red() as f64,
                color.green() as f64,
                color.blue() as f64,
                1.0,
            );
            context.set_line_width(2.0 / scale);
            context.rectangle(left, -3.0 / scale, width, HEIGHT as f64 + 6.0 / scale);
            let _ = context.stroke();
        }
        let _ = context.restore();
    }
}

pub fn compact(editor: &Rc<Editor>, preview: &Rc<Preview>) -> gtk4::Box {
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    let enlarge = gtk4::Button::with_label("Enlarge");
    let heading = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    let label = gtk4::Label::new(Some(&format!("Preview — {}", preview.layer.borrow())));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.add_css_class("heading");
    *preview.title.borrow_mut() = Some(label.downgrade());
    heading.append(&label);
    heading.append(&enlarge);
    content.append(&heading);
    content.append(&preview.area);
    content.append(&preview.diagnostic);
    enlarge.connect_clicked(glib::clone!(
        #[strong]
        editor,
        #[weak]
        preview,
        move |button| expanded(&editor, &preview, button)
    ));
    content
}

fn expanded(editor: &Rc<Editor>, source: &Rc<Preview>, parent: &gtk4::Button) {
    let dialog = adw::Dialog::builder()
        .title("Touch Bar Preview")
        .content_width(1000)
        .content_height(560)
        .build();
    let preview = Preview::new(&source.layer.borrow());
    preview.copy_context(source);
    editor.previews.borrow_mut().push(Rc::downgrade(&preview));
    let page = adw::PreferencesPage::new();
    let group = ui::group("Preview Only");
    group.set_description(Some(
        "These choices audition the draft. They do not change startup settings or run actions.",
    ));
    let names: Vec<String> = editor
        .doc
        .borrow()
        .layer_snapshot()
        .into_iter()
        .map(|l| l.name)
        .collect();
    let strings: Vec<&str> = names.iter().map(String::as_str).collect();
    let layer = ui::combo(
        "Layer",
        &strings,
        names
            .iter()
            .position(|n| *n == *preview.layer.borrow())
            .unwrap_or(0) as u32,
    );
    let geometry = ui::combo(
        "Device Geometry",
        &["2008 px", "2170 px (with Esc)"],
        u32::from(preview.width.get() == 2170),
    );
    let scale = ui::combo("Scale", &["Fit", "1:1"], 0);
    let mut themes = vec!["Document Settings".to_owned()];
    themes.extend(editor.doc.borrow().known_themes());
    let strings: Vec<&str> = themes.iter().map(String::as_str).collect();
    let theme = ui::combo(
        "Audition Theme",
        &strings,
        preview
            .theme
            .borrow()
            .as_ref()
            .and_then(|n| themes.iter().position(|t| t == n))
            .unwrap_or(0) as u32,
    );
    let hour = ui::spin(
        "Background Hour",
        0.0,
        23.0,
        preview
            .hour
            .get()
            .unwrap_or_else(|| chrono::Local::now().hour()) as f64,
    );
    let slots = gtk4::ListBox::new();
    slots.add_css_class("boxed-list");
    slots.set_selection_mode(gtk4::SelectionMode::None);
    let refresh: Rc<dyn Fn()> = Rc::new(glib::clone!(
        #[weak]
        preview,
        #[strong]
        editor,
        #[weak]
        slots,
        #[weak]
        dialog,
        move || {
            let errors = editor.doc.borrow().validate();
            if errors.is_empty() {
                preview.render(&editor.doc.borrow());
            } else {
                preview.pause(&errors.join("\n"));
            }
            while let Some(row) = slots.first_child() {
                slots.remove(&row);
            }
            for (index, (identity, authored)) in preview.slots.borrow().iter().enumerate() {
                let row = adw::ActionRow::builder()
                    .title(format!("{}  {identity}", index + 1))
                    .activatable(true)
                    .build();
                let layer_name = preview.layer.borrow().clone();
                let authored = *authored;
                row.connect_activated(glib::clone!(#[strong] editor, #[weak] dialog, move |row| {
                if let Some(button) = authored {
                    let index = editor.doc.borrow().layer_snapshot().iter().position(|l| l.name == layer_name);
                    if let Some(index) = index { editor.select(index, Some(button)); let _ = row.activate_action("win.show-layers", None); dialog.close(); }
                } else {
                    ui::error_dialog(row, "Generated Control", "Generated Pet and theme controls come from Behavior → Pet and Appearance → Color Theme. Home and Esc navigation follow the shared layer layout and device geometry.");
                }
            }));
                slots.append(&row);
            }
        }
    ));
    layer.connect_selected_notify(glib::clone!(
        #[weak]
        preview,
        #[strong]
        refresh,
        move |row| {
            preview.set_layer(&names[row.selected() as usize]);
            preview.set_selected(None);
            refresh();
        }
    ));
    geometry.connect_selected_notify(glib::clone!(
        #[weak]
        preview,
        #[strong]
        refresh,
        move |row| {
            preview
                .width
                .set(if row.selected() == 0 { 2008 } else { 2170 });
            if preview.one_to_one.get() {
                preview.area.set_width_request(preview.width.get() as i32);
            }
            refresh();
        }
    ));
    let strip = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Automatic)
        .vscrollbar_policy(gtk4::PolicyType::Never)
        .child(&preview.area)
        .build();
    scale.connect_selected_notify(glib::clone!(
        #[weak]
        preview,
        #[strong]
        refresh,
        move |row| {
            let actual = row.selected() == 1;
            preview.one_to_one.set(actual);
            preview.area.set_width_request(if actual {
                preview.width.get() as i32
            } else {
                -1
            });
            preview.area.queue_draw();
            refresh();
        }
    ));
    theme.connect_selected_notify(glib::clone!(
        #[weak]
        preview,
        #[strong]
        refresh,
        move |row| {
            *preview.theme.borrow_mut() =
                (row.selected() > 0).then(|| themes[row.selected() as usize].clone());
            refresh();
        }
    ));
    hour.connect_value_notify(glib::clone!(
        #[weak]
        preview,
        #[strong]
        refresh,
        move |row| {
            preview.hour.set(Some(row.value() as u32));
            refresh();
        }
    ));
    for row in [
        layer.clone().upcast::<gtk4::Widget>(),
        geometry.clone().upcast(),
        scale.clone().upcast(),
        theme.clone().upcast(),
        hour.clone().upcast(),
    ] {
        group.add(&row);
    }
    let reset = gtk4::Button::with_label("Reset to Document Settings");
    reset.connect_clicked(glib::clone!(
        #[weak]
        preview,
        #[weak]
        theme,
        #[weak]
        hour,
        #[strong]
        refresh,
        move |_| {
            *preview.theme.borrow_mut() = None;
            preview.hour.set(None);
            theme.set_selected(0);
            hour.set_value(chrono::Local::now().hour() as f64);
            preview.hour.set(None);
            refresh();
        }
    ));
    group.add(&reset);
    page.add(&group);
    let identities = ui::group("Buttons in Rendered Order");
    identities.add(&slots);
    page.add(&identities);
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    ui::margins(&content, 18);
    content.append(&strip);
    content.append(&preview.diagnostic);
    content.append(&page);
    content.connect_destroy(move |_| {
        let _ = &preview;
    });
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&content));
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(parent));
    refresh();
}
