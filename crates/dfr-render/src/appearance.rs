//! Render-side appearance helpers. The accent model and palette live
//! in the `dfr-config` crate, shared with validation and tooling; this
//! module keeps the bar's label sizing and the cairo painting
//! operations that only the renderer needs.

use cairo::Context;

pub use dfr_config::{ButtonAccent, Palette};

/// The bar's one primary label size, in pixels: every single-line button
/// label - Text and clock buttons, `Battery N/A`, the waveform widgets'
/// `Viz` and `No feed` labels - renders at this size, shrunk only where
/// the established fit logic already shrinks. One size for same-class
/// labels keeps the bar coherent; any smaller rendering is deliberate
/// hierarchy and documented at its own site (the battery's caption and
/// percent pair, the recording timer's readout).
pub const LABEL_FONT: f64 = 28.0;

/// The cairo painting operations behind each accent: the key fill, the
/// outline, and the accent used as a foreground color. An extension
/// trait because the accent type itself lives in `dfr-config`, which
/// does not depend on cairo.
pub trait AccentPaint {
    fn surface(self, c: &Context, active: bool, primary: bool, palette: &Palette);
    fn outline(self, c: &Context, active: bool, palette: &Palette);
    fn foreground(self, c: &Context, palette: &Palette);
}

impl AccentPaint for ButtonAccent {
    fn surface(self, c: &Context, active: bool, primary: bool, palette: &Palette) {
        let strength = if active {
            0.40
        } else if primary && self != ButtonAccent::Neutral {
            0.55
        } else {
            0.22
        };
        let (r, g, b) = palette.accent(self);
        let mix = |channel| 0.07 * (1.0 - strength) + channel * strength;
        c.set_source_rgb(mix(r), mix(g), mix(b));
    }

    fn outline(self, c: &Context, active: bool, palette: &Palette) {
        let (r, g, b) = palette.accent(self);
        c.set_source_rgba(r, g, b, if active { 1.0 } else { 0.65 });
    }

    fn foreground(self, c: &Context, palette: &Palette) {
        let (r, g, b) = palette.accent(self);
        c.set_source_rgb(r, g, b);
    }
}
