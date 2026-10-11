use crate::config::ButtonAction;
use cairo::Context;
use input_linux::Key;
use serde::Deserialize;

/// The bar's one primary label size, in pixels: every single-line button
/// label - Text and clock buttons, `Battery N/A`, the waveform widgets'
/// `Viz` and `No feed` labels - renders at this size, shrunk only where
/// the established fit logic already shrinks. One size for same-class
/// labels keeps the bar coherent; any smaller rendering is deliberate
/// hierarchy and documented at its own site (the battery's caption and
/// percent pair, the recording timer's readout).
pub const LABEL_FONT: f64 = 28.0;

/// Semantic accents keep the deck readable without depending on color alone.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ButtonAccent {
    #[default]
    Neutral,
    Cyan,
    Amber,
    Blue,
    Violet,
    Red,
    Green,
}

impl ButtonAccent {
    pub fn for_action(action: &ButtonAction) -> Self {
        match action {
            ButtonAction::VolumeSlider => Self::Cyan,
            ButtonAction::BrightnessSlider => Self::Amber,
            ButtonAction::Keys(keys) if keys.len() == 1 => match keys[0] {
                Key::BrightnessDown | Key::BrightnessUp | Key::IllumDown | Key::IllumUp => {
                    Self::Amber
                }
                Key::VolumeDown
                | Key::VolumeUp
                | Key::Mute
                | Key::MicMute
                | Key::PlayPause
                | Key::PreviousSong
                | Key::NextSong => Self::Cyan,
                _ => Self::Neutral,
            },
            ButtonAction::SwitchLayer(_) => Self::Violet,
            ButtonAction::Command(_) | ButtonAction::CommandLayer { .. } => Self::Cyan,
            _ => Self::Neutral,
        }
    }

    pub fn rgb(self) -> (f64, f64, f64) {
        match self {
            Self::Neutral => (0.58, 0.63, 0.72),
            Self::Cyan => (0.19, 0.83, 0.95),
            Self::Amber => (1.0, 0.72, 0.27),
            Self::Blue => (0.36, 0.60, 1.0),
            Self::Violet => (0.67, 0.53, 1.0),
            Self::Red => (1.0, 0.32, 0.42),
            Self::Green => (0.27, 0.88, 0.57),
        }
    }

    pub fn surface(self, c: &Context, active: bool, primary: bool) {
        let strength = if active {
            0.40
        } else if primary && self != Self::Neutral {
            0.55
        } else {
            0.22
        };
        let (r, g, b) = self.rgb();
        let mix = |channel| 0.07 * (1.0 - strength) + channel * strength;
        c.set_source_rgb(mix(r), mix(g), mix(b));
    }

    pub fn outline(self, c: &Context, active: bool) {
        let (r, g, b) = self.rgb();
        c.set_source_rgba(r, g, b, if active { 1.0 } else { 0.65 });
    }

    pub fn foreground(self, c: &Context) {
        let (r, g, b) = self.rgb();
        c.set_source_rgb(r, g, b);
    }
}
