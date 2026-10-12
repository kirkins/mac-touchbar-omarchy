//! The configuration wire types: the exact serde shapes a base or user
//! TOML file deserializes into, the field-by-field overlay that merges
//! a user file over the base, and the canonical `PascalCase` key names
//! every tool that reads or writes this configuration must share.
//!
//! Every struct in this module denies unknown fields: a typo in a knob
//! name rejects the candidate configuration with the offending key
//! named (and, where the parser knows it, its line and column) instead
//! of being silently ignored. The one historical exception is kept as
//! an alias: `Svg` remains an accepted spelling of `Icon`.

use crate::appearance::ButtonAccent;
use crate::daycycle::{self, DayCycle, TimeBand};
use anyhow::{bail, Context, Result};
use input_linux::Key;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct ConfigProxy {
    pub media_layer_default: Option<bool>,
    pub show_button_outlines: Option<bool>,
    pub enable_pixel_shift: Option<bool>,
    pub font_template: Option<String>,
    pub adaptive_brightness: Option<bool>,
    pub active_brightness: Option<u32>,
    pub double_press_switch_layers: Option<u32>,
    pub battery_cycle_enabled: Option<bool>,
    pub battery_cycle_timeout_secs: Option<u32>,
    pub battery_warning_threshold: Option<u32>,
    pub battery_critical_threshold: Option<u32>,
    pub battery_warning_color: Option<ButtonAccent>,
    pub battery_critical_color: Option<ButtonAccent>,
    pub pet_enabled: Option<bool>,
    pub pet_variant: Option<String>,
    pub pet_energy: Option<String>,
    pub day_cycle_enabled: Option<bool>,
    pub day_cycle_bands: Option<Vec<BandConfig>>,
    pub themes_enabled: Option<bool>,
    pub active_theme: Option<String>,
    pub themes: Option<BTreeMap<String, crate::themes::ThemeConfig>>,
    pub primary_layer_keys: Option<Vec<ButtonConfig>>,
    pub media_layer_keys: Option<Vec<ButtonConfig>>,
    pub layers: Option<Vec<LayerConfig>>,
    pub default_layer: Option<String>,
    pub fn_layer: Option<String>,
}

impl ConfigProxy {
    /// Overlay a user configuration on top of this (base) one: every
    /// field the user file sets replaces the base value; fields it
    /// leaves out keep the base value. Arrays replace wholesale, never
    /// item by item. This is the contract every reader of the two files
    /// must replicate exactly.
    pub fn merge(&mut self, user: Self) {
        macro_rules! merge {
            ($($field:ident),* $(,)?) => { $(self.$field = user.$field.or(self.$field.take());)* };
        }
        merge!(
            media_layer_default,
            show_button_outlines,
            enable_pixel_shift,
            font_template,
            adaptive_brightness,
            active_brightness,
            double_press_switch_layers,
            battery_cycle_enabled,
            battery_cycle_timeout_secs,
            battery_warning_threshold,
            battery_critical_threshold,
            battery_warning_color,
            battery_critical_color,
            pet_enabled,
            pet_variant,
            pet_energy,
            day_cycle_enabled,
            day_cycle_bands,
            themes_enabled,
            active_theme,
            themes,
            primary_layer_keys,
            media_layer_keys,
            layers,
            default_layer,
            fn_layer,
        );
    }

    /// The day cycle resolves the enabled flag over the configured or
    /// default bands, validating band names, hours, and colors together.
    pub fn day_cycle(&self) -> Result<DayCycle> {
        let enabled = self
            .day_cycle_enabled
            .unwrap_or(crate::defaults::DAY_CYCLE_ENABLED);
        let bands = match &self.day_cycle_bands {
            None => DayCycle::default_bands(),
            Some(bands) => parse_bands(bands)?,
        };
        Ok(DayCycle::new(enabled, bands))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ButtonAction {
    /// Opens the absolute output-volume slider without stepping the mixer.
    VolumeSlider,
    /// Opens the built-in display slider without stepping its backlight.
    BrightnessSlider,
    Keys(Vec<Key>),
    SwitchLayer(String),
    /// Runs a shell command on touch-down.
    Command(String),
    /// Runs a shell command on touch-down and switches to the named layer,
    /// so a single press does both.
    CommandLayer {
        command: String,
        layer: String,
    },
    /// Applies the named color theme on touch-down and stays on the bar.
    ApplyTheme(String),
    /// Applies the named color theme and switches to the named layer in
    /// one press - the picker's per-theme buttons return home this way.
    ApplyThemeLayer {
        theme: String,
        layer: String,
    },
    /// Applies the next theme in the cycle, wrapping.
    CycleTheme,
}

impl ButtonAction {
    /// The layer this action switches to on touch-down, if any.
    pub fn layer_target(&self) -> Option<&str> {
        match self {
            Self::SwitchLayer(layer) | Self::CommandLayer { layer, .. } => Some(layer),
            Self::ApplyThemeLayer { layer, .. } => Some(layer),
            _ => None,
        }
    }
    /// The shell command this action runs on touch-down, if any.
    pub fn command(&self) -> Option<&str> {
        match self {
            Self::Command(command) | Self::CommandLayer { command, .. } => Some(command),
            _ => None,
        }
    }
}

impl Default for ButtonAction {
    fn default() -> Self {
        Self::Keys(Vec::new())
    }
}

impl<'de> Deserialize<'de> for ButtonAction {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Action {
            Unit(UnitAction),
            Key(Key),
            Keys(Vec<Key>),
            Table(TableAction),
        }
        #[derive(Deserialize)]
        enum UnitAction {
            VolumeSlider,
            BrightnessSlider,
            CycleTheme,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase", deny_unknown_fields)]
        struct TableAction {
            layer: Option<String>,
            command: Option<String>,
            theme: Option<String>,
        }
        Ok(match Action::deserialize(deserializer)? {
            Action::Unit(UnitAction::VolumeSlider) => Self::VolumeSlider,
            Action::Unit(UnitAction::BrightnessSlider) => Self::BrightnessSlider,
            Action::Unit(UnitAction::CycleTheme) => Self::CycleTheme,
            Action::Key(key) => Self::Keys(vec![key]),
            Action::Keys(keys) => Self::Keys(keys),
            Action::Table(action) => {
                let theme = action.theme.filter(|theme| !theme.trim().is_empty());
                match (action.layer, action.command, theme) {
                    (Some(layer), None, None) => Self::SwitchLayer(layer),
                    (None, Some(command), None) => Self::Command(command),
                    (Some(layer), Some(command), None) => Self::CommandLayer { command, layer },
                    (None, None, Some(theme)) => Self::ApplyTheme(theme),
                    (Some(layer), None, Some(theme)) => Self::ApplyThemeLayer { theme, layer },
                    (_, Some(_), Some(_)) => {
                        return Err(serde::de::Error::custom(
                            "an Action table cannot combine Command with Theme",
                        ))
                    }
                    (None, None, None) => {
                        return Err(serde::de::Error::custom(
                            "an Action table needs Layer, Command, or Theme",
                        ))
                    }
                }
            }
        })
    }
}

impl Serialize for ButtonAction {
    /// The inverse of the untagged grammar: the special actions as their
    /// string forms, key lists as a single key or an array, and the
    /// combined forms as their tables. A `Keys` action holding one key
    /// serializes as that key's string, matching how the daemon reads
    /// both spellings identically.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(untagged)]
        enum ActionRepr {
            Unit(&'static str),
            Key(String),
            Keys(Vec<String>),
            #[serde(rename_all = "PascalCase")]
            Layer {
                layer: String,
            },
            #[serde(rename_all = "PascalCase")]
            Command {
                command: String,
            },
            #[serde(rename_all = "PascalCase")]
            CommandLayer {
                command: String,
                layer: String,
            },
            #[serde(rename_all = "PascalCase")]
            Theme {
                theme: String,
            },
            #[serde(rename_all = "PascalCase")]
            ThemeLayer {
                theme: String,
                layer: String,
            },
        }
        let key_name = |key: &Key| serde_plain_name(key);
        let repr = match self {
            Self::VolumeSlider => ActionRepr::Unit("VolumeSlider"),
            Self::BrightnessSlider => ActionRepr::Unit("BrightnessSlider"),
            Self::CycleTheme => ActionRepr::Unit("CycleTheme"),
            Self::Keys(keys) => match keys.as_slice() {
                [key] => ActionRepr::Key(key_name(key)),
                keys => ActionRepr::Keys(keys.iter().map(key_name).collect()),
            },
            Self::SwitchLayer(layer) => ActionRepr::Layer {
                layer: layer.clone(),
            },
            Self::Command(command) => ActionRepr::Command {
                command: command.clone(),
            },
            Self::CommandLayer { command, layer } => ActionRepr::CommandLayer {
                command: command.clone(),
                layer: layer.clone(),
            },
            Self::ApplyTheme(theme) => ActionRepr::Theme {
                theme: theme.clone(),
            },
            Self::ApplyThemeLayer { theme, layer } => ActionRepr::ThemeLayer {
                theme: theme.clone(),
                layer: layer.clone(),
            },
        };
        repr.serialize(serializer)
    }
}

/// The wire name of an `input_linux::Key`, obtained the same way its
/// serde implementation spells it so a serialized action re-parses to
/// the identical key.
fn serde_plain_name(key: &Key) -> String {
    key.serialize(VariantName)
        .expect("keys serialize as their variant names")
}

/// A serializer that accepts exactly what a unit-only enum emits: its
/// variant name, returned as the owned `String`. Anything else is an
/// error, since only key names are ever routed through it.
struct VariantName;

#[derive(Debug)]
enum KeyError {
    Unsupported,
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("only unit variant names can be captured")
    }
}

impl std::error::Error for KeyError {}

impl serde::ser::Error for KeyError {
    fn custom<T: std::fmt::Display>(_: T) -> Self {
        KeyError::Unsupported
    }
}

impl serde::Serializer for VariantName {
    type Ok = String;
    type Error = KeyError;
    type SerializeSeq = serde::ser::Impossible<String, KeyError>;
    type SerializeTuple = serde::ser::Impossible<String, KeyError>;
    type SerializeTupleStruct = serde::ser::Impossible<String, KeyError>;
    type SerializeTupleVariant = serde::ser::Impossible<String, KeyError>;
    type SerializeMap = serde::ser::Impossible<String, KeyError>;
    type SerializeStruct = serde::ser::Impossible<String, KeyError>;
    type SerializeStructVariant = serde::ser::Impossible<String, KeyError>;

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<String, KeyError> {
        Ok(variant.to_owned())
    }

    fn serialize_str(self, value: &str) -> Result<String, KeyError> {
        Ok(value.to_owned())
    }

    fn serialize_bool(self, _: bool) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_i8(self, _: i8) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_i16(self, _: i16) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_i32(self, _: i32) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_i64(self, _: i64) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_u8(self, _: u8) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_u16(self, _: u16) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_u32(self, _: u32) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_u64(self, _: u64) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_f32(self, _: f32) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_f64(self, _: f64) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_char(self, _: char) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_bytes(self, _: &[u8]) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_none(self) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_some<T: ?Sized + serde::Serialize>(self, _: &T) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_unit(self) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_unit_struct(self, _: &'static str) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_newtype_struct<T: ?Sized + serde::Serialize>(
        self,
        _: &'static str,
        _: &T,
    ) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_newtype_variant<T: ?Sized + serde::Serialize>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<String, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_tuple_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleStruct, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStruct, KeyError> {
        Err(KeyError::Unsupported)
    }

    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant, KeyError> {
        Err(KeyError::Unsupported)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct ButtonConfig {
    #[serde(alias = "Svg")]
    pub icon: Option<String>,
    pub text: Option<String>,
    pub theme: Option<String>,
    pub time: Option<String>,
    pub battery: Option<String>,
    pub waveform: Option<bool>,
    pub waveform_style: Option<String>,
    pub waveform_smoothing: Option<u32>,
    pub waveform_lag: Option<u32>,
    pub visualizer: Option<bool>,
    pub visualizer_style: Option<String>,
    pub visualizer_smoothing: Option<u32>,
    pub visualizer_lag: Option<u32>,
    pub now_playing: Option<bool>,
    pub locale: Option<String>,
    #[serde(default)]
    pub action: ButtonAction,
    pub stretch: Option<usize>,
    pub icon_width: Option<i32>,
    pub icon_height: Option<i32>,
    pub accent: Option<ButtonAccent>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct LayerConfig {
    pub name: String,
    pub keys: Vec<ButtonConfig>,
}

/// One `[[DayCycleBands]]` entry: the background color for the local
/// hours from `StartHour` until the next band begins.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct BandConfig {
    pub name: String,
    pub start_hour: u8,
    pub color: String,
}

/// Validate and resolve a band list, shared by the global day cycle and
/// each theme's bands. Band names must be nonempty and unique, hours
/// 0-23 and unique, and colors six-hex-digit literals.
pub fn parse_bands(bands: &[BandConfig]) -> Result<Vec<TimeBand>> {
    if bands.is_empty() {
        bail!("DayCycleBands must define at least one band");
    }
    let mut names = HashSet::new();
    let mut hours = HashSet::new();
    let mut parsed = Vec::with_capacity(bands.len());
    for band in bands {
        if band.name.trim().is_empty() || !names.insert(band.name.clone()) {
            bail!("band names must be nonempty and unique: {:?}", band.name);
        }
        if band.start_hour > 23 {
            bail!("band {:?} StartHour must be 0-23", band.name);
        }
        if !hours.insert(band.start_hour) {
            bail!(
                "band {:?} repeats the StartHour of an earlier band",
                band.name
            );
        }
        parsed.push(TimeBand {
            name: band.name.clone(),
            start_hour: band.start_hour,
            color: daycycle::parse_hex_color(&band.color)
                .with_context(|| format!("band {:?} Color", band.name))?,
        });
    }
    Ok(parsed)
}

/// Every field name a configuration author can write, across all the
/// table shapes, used to suggest the intended knob when an unknown
/// field is rejected.
pub(crate) fn known_knob_names() -> Vec<&'static str> {
    let mut names = vec![
        "MediaLayerDefault",
        "ShowButtonOutlines",
        "EnablePixelShift",
        "FontTemplate",
        "AdaptiveBrightness",
        "ActiveBrightness",
        "DoublePressSwitchLayers",
        "BatteryCycleEnabled",
        "BatteryCycleTimeoutSecs",
        "BatteryWarningThreshold",
        "BatteryCriticalThreshold",
        "BatteryWarningColor",
        "BatteryCriticalColor",
        "PetEnabled",
        "PetVariant",
        "PetEnergy",
        "DayCycleEnabled",
        "DayCycleBands",
        "ThemesEnabled",
        "ActiveTheme",
        "Themes",
        "PrimaryLayerKeys",
        "MediaLayerKeys",
        "Layers",
        "DefaultLayer",
        "FnLayer",
        // Button fields (Svg alias included).
        "Icon",
        "Svg",
        "Text",
        "Theme",
        "Time",
        "Battery",
        "Waveform",
        "WaveformStyle",
        "WaveformSmoothing",
        "WaveformLag",
        "Visualizer",
        "VisualizerStyle",
        "VisualizerSmoothing",
        "VisualizerLag",
        "NowPlaying",
        "Locale",
        "Action",
        "Stretch",
        "IconWidth",
        "IconHeight",
        "Accent",
        // Action table fields.
        "Layer",
        "Command",
        // Band and theme table fields.
        "Name",
        "StartHour",
        "Color",
        "Accents",
    ];
    names.sort_unstable();
    names.dedup();
    names
}

/// Find the closest known knob name to `typo`, if one is plausibly
/// close: at most two edits for short names, a third allowed for longer
/// ones. Returns `None` when nothing is close enough to suggest.
pub(crate) fn nearest_knob_name(typo: &str) -> Option<&'static str> {
    let budget = if typo.len() < 6 { 2 } else { 3 };
    known_knob_names()
        .into_iter()
        .map(|name| (name, edit_distance(typo, name)))
        .filter(|(_, distance)| *distance <= budget)
        .min_by_key(|(_, distance)| *distance)
        .map(|(name, _)| name)
}

/// Plain Levenshtein distance over bytes; knob names are ASCII.
pub(crate) fn edit_distance(a: &str, b: &str) -> usize {
    let b_bytes = b.as_bytes();
    let mut previous: Vec<usize> = (0..=b_bytes.len()).collect();
    let mut current = vec![0usize; b_bytes.len() + 1];
    for (i, a_byte) in a.as_bytes().iter().enumerate() {
        current[0] = i + 1;
        for (j, b_byte) in b_bytes.iter().enumerate() {
            let substitution = previous[j] + usize::from(a_byte != b_byte);
            current[j + 1] = (current[j] + 1).min(previous[j + 1] + 1).min(substitution);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b_bytes.len()]
}
