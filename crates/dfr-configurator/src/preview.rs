//! The static Cairo preview: the actual bar, drawn by `dfr-render`
//! exactly as the daemon draws it, into a GTK drawing area. Rendering
//! happens on demand (selection changes, theme switches, debounced
//! edits) into a cached `ImageSurface`; painting the widget only blits
//! that surface, so idle previewing costs nothing.

use crate::model::Document;
use chrono::Timelike as _;
use gtk4::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// One rendered preview bar.
pub struct Preview {
    /// The widget the surface is painted into.
    pub area: gtk4::DrawingArea,
    /// The rendered frame plus the revision it was rendered from.
    frame: RefCell<Option<cairo::ImageSurface>>,
    /// Bumped on every document edit; a render records the revision it
    /// used, so a stale render never masquerades as current.
    revision: Cell<u64>,
    rendered_revision: Cell<u64>,
    /// The layer to draw.
    layer: RefCell<String>,
    /// The bar geometry to synthesize for.
    width: Cell<u16>,
    /// Rendering error, shown in place of the bar when it fails.
    error: RefCell<Option<String>>,
}

pub const HEIGHT: i32 = 60;

impl Preview {
    pub fn new(layer: &str) -> Rc<Self> {
        let area = gtk4::DrawingArea::new();
        area.set_height_request(HEIGHT + 24);
        area.set_hexpand(true);
        let preview = Rc::new(Self {
            area,
            frame: RefCell::new(None),
            revision: Cell::new(0),
            rendered_revision: Cell::new(0),
            layer: RefCell::new(layer.to_owned()),
            width: Cell::new(2008),
            error: RefCell::new(None),
        });
        let weak = Rc::downgrade(&preview);
        preview
            .area
            .set_draw_func(move |area, context, width_px, height_px| {
                if let Some(preview) = weak.upgrade() {
                    preview.paint(area, context, width_px, height_px);
                }
            });
        preview
    }

    pub fn set_layer(&self, layer: &str) {
        *self.layer.borrow_mut() = layer.to_owned();
        self.invalidate();
    }

    pub fn set_width(&self, width: u16) {
        self.width.set(width);
        self.invalidate();
    }

    /// Note that the document changed: the next render must redo work.
    pub fn invalidate(&self) {
        self.revision.set(self.revision.get() + 1);
        self.area.queue_draw();
    }

    /// Render the current document into the cache. This is the
    /// expensive path (deck build + font + icons + draw); callers
    /// schedule it, never run it per frame.
    pub fn render(&self, doc: &Document) {
        let revision = self.revision.get();
        let dir =
            std::env::temp_dir().join(format!("dfr-configurator-preview-{}", std::process::id()));
        let render = (|| -> anyhow::Result<cairo::ImageSurface> {
            std::fs::create_dir_all(&dir)?;
            let base = dir.join("base.toml");
            let user = dir.join("user.toml");
            std::fs::write(&base, doc.base_text())?;
            std::fs::write(&user, doc.candidate_text())?;
            let (cfg, mut deck) =
                dfr_render::config::load_config(self.width.get(), &base, Some(&user), None)?;
            let layer = self.layer.borrow().clone();
            let index = *deck
                .names
                .get(&layer)
                .ok_or_else(|| anyhow::anyhow!("layer {layer:?} is not in the deck"))?;
            deck.switch_to(index, &mut |_, _| {});
            // Draw into the panel's portrait orientation, then rotate
            // into the horizontal view - the same transforms the
            // offline review renderer produces.
            let portrait =
                cairo::ImageSurface::create(cairo::Format::ARgb32, HEIGHT, self.width.get() as i32)
                    .expect("the preview surface allocates");
            deck.slider.pixel_shift_width = 0;
            deck.slider.pixel_shift_x = 0.0;
            let slider = deck.slider_render();
            let background = cfg.day_cycle.background(chrono::Local::now().hour());
            deck.current_mut().draw(
                &cfg,
                (self.width.get() as i32, HEIGHT),
                &portrait,
                (0.0, 0.0),
                true,
                slider,
                background,
            );
            let horizontal =
                cairo::ImageSurface::create(cairo::Format::ARgb32, self.width.get() as i32, HEIGHT)
                    .expect("the horizontal surface allocates");
            let context = cairo::Context::new(&horizontal).expect("the preview context allocates");
            context.translate(0.0, HEIGHT as f64);
            context.rotate((-90.0_f64).to_radians());
            context.set_source_surface(&portrait, 0.0, 0.0)?;
            context.paint()?;
            Ok(horizontal)
        })();
        match render {
            Ok(surface) => {
                *self.error.borrow_mut() = None;
                *self.frame.borrow_mut() = Some(surface);
                self.rendered_revision.set(revision);
            }
            Err(err) => {
                *self.error.borrow_mut() = Some(format!("{err:#}"));
                *self.frame.borrow_mut() = None;
            }
        }
        self.area.queue_draw();
    }

    fn paint(
        &self,
        _area: &gtk4::DrawingArea,
        context: &cairo::Context,
        width_px: i32,
        height_px: i32,
    ) {
        // The panel's backdrop first: the bar never draws on transparency.
        context.set_source_rgb(0.11, 0.11, 0.12);
        context.rectangle(0.0, 0.0, width_px.max(1) as f64, height_px.max(1) as f64);
        let _ = context.paint();
        if let Some(err) = self.error.borrow().as_ref() {
            // The toy text API is plenty for a one-line diagnostic.
            context.set_source_rgb(0.94, 0.94, 0.94);
            context.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
            context.set_font_size(11.0);
            let text = format!("preview unavailable: {err}");
            if let Ok(extents) = context.text_extents(&text) {
                context.move_to(
                    8.0,
                    (height_px as f64 / 2.0 - extents.height() / 2.0
                        + extents.y_advance().max(0.0))
                    .max(12.0),
                );
            }
            let _ = context.show_text(&text);
            return;
        }
        let frame = self.frame.borrow();
        let Some(surface) = frame.as_ref() else {
            return;
        };
        let scale = (width_px as f64 / surface.width() as f64)
            .min(height_px as f64 / surface.height() as f64)
            .min(1.0);
        context.scale(scale, scale);
        if context.set_source_surface(surface, 0.0, 0.0).is_ok() {
            let _ = context.paint();
        }
    }
}
