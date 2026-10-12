use crate::proxy::ButtonAction;
use input_linux::Key;
use serde::{Deserialize, Serialize};

/// The bar's one primary label size, in pixels: every single-line button
/// label - Text and clock buttons, `Battery N/A`, the waveform widgets'
/// `Viz` and `No feed` labels - renders at this size, shrunk only where
/// the established fit logic already shrinks. One size for same-class
/// labels keeps the bar coherent; any smaller rendering is deliberate
/// hierarchy and documented at its own site (the battery's caption and
/// percent pair, the recording timer's readout).
pub const LABEL_FONT: f64 = 28.0;

/// Semantic accents keep the deck readable without depending on color alone.
#[derive(
    Clone, Copy, Debug, Default, Hash, Ord, PartialOrd, PartialEq, Eq, Deserialize, Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ButtonAccent {
    #[default]
    Neutral,
    Cyan,
    Amber,
    Blue,
    Violet,
    Orange,
    Red,
    Green,
}

/// The accent palette the bar renders with: the RGB behind each accent
/// name. Themes preset these values; everything that draws an accent
/// resolves it through the palette, so one palette swap recolors every
/// fill, outline, slider, and accent-driven widget on the next frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette([(f64, f64, f64); Self::LEN]);

impl Palette {
    const LEN: usize = 8;

    /// The shipped live palette: the accent colors the deck has always
    /// rendered with, and the base every theme's overrides land on.
    pub const fn shipped() -> Self {
        use ButtonAccent::*;
        let mut palette = [(0.0, 0.0, 0.0); Self::LEN];
        palette[Neutral as usize] = Neutral.rgb();
        palette[Cyan as usize] = Cyan.rgb();
        palette[Amber as usize] = Amber.rgb();
        palette[Blue as usize] = Blue.rgb();
        palette[Violet as usize] = Violet.rgb();
        palette[Orange as usize] = Orange.rgb();
        palette[Red as usize] = Red.rgb();
        palette[Green as usize] = Green.rgb();
        Palette(palette)
    }

    pub fn accent(&self, accent: ButtonAccent) -> (f64, f64, f64) {
        self.0[accent as usize]
    }

    pub fn set(&mut self, accent: ButtonAccent, color: (u8, u8, u8)) {
        self.0[accent as usize] = (
            f64::from(color.0) / 255.0,
            f64::from(color.1) / 255.0,
            f64::from(color.2) / 255.0,
        );
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self::shipped()
    }
}

impl ButtonAccent {
    /// The wire name of this accent, as the configuration spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Neutral => "neutral",
            Self::Cyan => "cyan",
            Self::Amber => "amber",
            Self::Blue => "blue",
            Self::Violet => "violet",
            Self::Orange => "orange",
            Self::Red => "red",
            Self::Green => "green",
        }
    }

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
            // Theme buttons are appearance controls in the navigation
            // family: they restyle the bar rather than run anything.
            ButtonAction::SwitchLayer(_)
            | ButtonAction::ApplyTheme(_)
            | ButtonAction::ApplyThemeLayer { .. }
            | ButtonAction::CycleTheme => Self::Violet,
            ButtonAction::Command(_) | ButtonAction::CommandLayer { .. } => Self::Cyan,
            _ => Self::Neutral,
        }
    }

    /// The shipped color for this accent: the live palette before any theme. Theme-aware drawing resolves through a [`Palette`]; the
    /// few colors that must never follow a theme - the waveform's
    /// dead-feed alarm, defaults outside a config - stay on this.
    pub const fn rgb(self) -> (f64, f64, f64) {
        match self {
            Self::Neutral => (0.58, 0.63, 0.72),
            Self::Cyan => (0.19, 0.83, 0.95),
            Self::Amber => (1.0, 0.72, 0.27),
            Self::Blue => (0.36, 0.60, 1.0),
            Self::Violet => (0.67, 0.53, 1.0),
            // A clear warning orange: visibly redder than the amber knob
            // accent and visibly yellower than the critical red, so the two
            // escalation tiers never read as the same hue on the strip.
            Self::Orange => (1.0, 0.52, 0.13),
            Self::Red => (1.0, 0.32, 0.42),
            Self::Green => (0.27, 0.88, 0.57),
        }
    }
}
