use crate::appearance::ButtonAccent;
use crate::daycycle::{self, DayCycle, TimeBand};
use crate::fonts::{FontConfig, Pattern};
use crate::pet::{PetEnergy, PetVariant};
use crate::themes::{ThemeConfig, ThemeSet, THEMES_LAYER};
use crate::{FunctionLayer, LayerDeck};
use anyhow::{anyhow, bail, Context, Result};
use cairo::FontFace;
use freetype::Library as FtLibrary;
use input_linux::Key;
use nix::{
    errno::Errno,
    sys::inotify::{AddWatchFlags, InitFlags, Inotify, InotifyEvent, WatchDescriptor},
};
use serde::{Deserialize, Deserializer};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, read_to_string},
    os::fd::AsFd,
    path::{Path, PathBuf},
};

const USER_CFG_PATH: &str = "/etc/tiny-dfr/config.toml";
const BASE_CFG_PATH: &str = "/usr/share/tiny-dfr/config.toml";
/// Where the daemon remembers the theme chosen on the glass. The
/// directory is prepared by the service install (see the README); the
/// daemon itself only reads and atomically rewrites this one file.
const THEME_STATE_PATH: &str = "/var/lib/tiny-dfr/active-theme";

/// Name of the pet's own layer. When the pet is enabled, a layer with this
/// name is synthesized if the configuration does not define one, and the
/// home bar gains a Pet button opening it.
const PET_LAYER: &str = "pet";
/// Free button spaces after the Home button inside the synthesized pet
/// layer: open ground for the pet to wander.
const PET_LAYER_STRETCH: usize = 7;

pub struct Config {
    pub show_button_outlines: bool,
    pub enable_pixel_shift: bool,
    pub font_face: FontFace,
    pub adaptive_brightness: bool,
    pub active_brightness: u32,
    pub double_press_switch_layers: u32,
    pub battery_cycle_enabled: bool,
    pub battery_cycle_timeout_secs: u32,
    pub battery_warning_threshold: u32,
    pub battery_critical_threshold: u32,
    pub battery_warning_color: ButtonAccent,
    pub battery_critical_color: ButtonAccent,
    pub pet_enabled: bool,
    pub pet_variant: PetVariant,
    pub pet_energy: PetEnergy,
    /// The global (theme-free) day cycle as configured; the active
    /// theme's bands, if any, are applied on top for `day_cycle`, and
    /// re-applying a theme at runtime rebuilds from this copy.
    pub global_day_cycle: DayCycle,
    pub day_cycle: DayCycle,
    /// The accent palette the bar renders with, resolved from the
    /// active theme (or the shipped live palette with none).
    pub palette: crate::appearance::Palette,
    pub themes: ThemeSet,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ConfigProxy {
    media_layer_default: Option<bool>,
    show_button_outlines: Option<bool>,
    enable_pixel_shift: Option<bool>,
    font_template: Option<String>,
    adaptive_brightness: Option<bool>,
    active_brightness: Option<u32>,
    double_press_switch_layers: Option<u32>,
    battery_cycle_enabled: Option<bool>,
    battery_cycle_timeout_secs: Option<u32>,
    battery_warning_threshold: Option<u32>,
    battery_critical_threshold: Option<u32>,
    battery_warning_color: Option<ButtonAccent>,
    battery_critical_color: Option<ButtonAccent>,
    pet_enabled: Option<bool>,
    pet_variant: Option<String>,
    pet_energy: Option<String>,
    day_cycle_enabled: Option<bool>,
    day_cycle_bands: Option<Vec<BandConfig>>,
    themes_enabled: Option<bool>,
    active_theme: Option<String>,
    themes: Option<BTreeMap<String, ThemeConfig>>,
    primary_layer_keys: Option<Vec<ButtonConfig>>,
    media_layer_keys: Option<Vec<ButtonConfig>>,
    layers: Option<Vec<LayerConfig>>,
    default_layer: Option<String>,
    fn_layer: Option<String>,
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

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
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

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct LayerConfig {
    pub name: String,
    pub keys: Vec<ButtonConfig>,
}

/// One `[[DayCycleBands]]` entry: the background color for the local
/// hours from `StartHour` until the next band begins.
#[derive(Clone, Debug, Deserialize)]
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

pub struct LayerLayout {
    pub layers: Vec<LayerConfig>,
    pub home: usize,
    pub fn_layer: Option<usize>,
    pub legacy: bool,
}

impl ConfigProxy {
    fn merge(&mut self, user: Self) {
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
    fn day_cycle(&self) -> Result<DayCycle> {
        let enabled = self.day_cycle_enabled.unwrap_or(true);
        let bands = match &self.day_cycle_bands {
            None => DayCycle::default_bands(),
            Some(bands) => parse_bands(bands)?,
        };
        Ok(DayCycle::new(enabled, bands))
    }

    fn layer_layout(&mut self, width: u16, themes: &ThemeSet) -> Result<LayerLayout> {
        let legacy = self.layers.is_none();
        let mut layers = if let Some(layers) = self.layers.take() {
            layers
        } else {
            if self.default_layer.is_some() || self.fn_layer.is_some() {
                bail!("DefaultLayer and FnLayer require named Layers");
            }
            vec![
                LayerConfig {
                    name: "primary".into(),
                    keys: self
                        .primary_layer_keys
                        .take()
                        .context("missing PrimaryLayerKeys")?,
                },
                LayerConfig {
                    name: "media".into(),
                    keys: self
                        .media_layer_keys
                        .take()
                        .context("missing MediaLayerKeys")?,
                },
            ]
        };
        let mut names = HashSet::new();
        for layer in &layers {
            if layer.name.trim().is_empty() || !names.insert(layer.name.clone()) {
                bail!("layer names must be nonempty and unique: {:?}", layer.name);
            }
            if layer.keys.is_empty() {
                bail!("layer {:?} has no buttons", layer.name);
            }
        }
        let home_name = if legacy {
            if self.media_layer_default.unwrap_or(false) {
                "media"
            } else {
                "primary"
            }
        } else {
            self.default_layer
                .as_deref()
                .context("named Layers require DefaultLayer")?
        };
        let home = layers
            .iter()
            .position(|layer| layer.name == home_name)
            .with_context(|| format!("DefaultLayer target {home_name:?} does not exist"))?;
        let fn_layer = if legacy {
            Some(1 - home)
        } else {
            self.fn_layer
                .as_deref()
                .map(|name| {
                    layers
                        .iter()
                        .position(|layer| layer.name == name)
                        .with_context(|| format!("FnLayer target {name:?} does not exist"))
                })
                .transpose()?
        };
        // The pet lives on its own layer, opened by a Pet button on the home
        // bar whenever PetEnabled is on. A user-defined layer named "pet"
        // replaces the synthesized playground, and an existing visible
        // button targeting it replaces the synthesized Pet button.
        if self.pet_enabled.unwrap_or(true) {
            if names.insert(PET_LAYER.to_owned()) {
                layers.push(LayerConfig {
                    name: PET_LAYER.into(),
                    keys: vec![
                        ButtonConfig {
                            text: Some("Home".into()),
                            action: ButtonAction::SwitchLayer(home_name.to_owned()),
                            ..ButtonConfig::default()
                        },
                        ButtonConfig {
                            stretch: Some(PET_LAYER_STRETCH),
                            ..ButtonConfig::default()
                        },
                    ],
                });
            }
            let home_layer = &mut layers[home];
            let has_pet_button = home_layer.keys.iter().any(|button| {
                button.action.layer_target() == Some(PET_LAYER)
                    && button.text.as_deref().map_or(
                        button.icon.is_some() || button.time.is_some() || button.battery.is_some(),
                        |text| !text.trim().is_empty(),
                    )
            });
            if !has_pet_button {
                let at = home_layer
                    .keys
                    .iter()
                    .rposition(|button| matches!(button.action, ButtonAction::SwitchLayer(_)))
                    .map_or(home_layer.keys.len(), |last| last + 1);
                home_layer.keys.insert(
                    at,
                    ButtonConfig {
                        text: Some("Pet".into()),
                        action: ButtonAction::SwitchLayer(PET_LAYER.into()),
                        ..ButtonConfig::default()
                    },
                );
            }
        }
        // The theme picker takes over its own layer - the full-bar surface
        // the captain described - listing every theme name as a button that
        // applies it and returns home. A user-defined layer named "themes"
        // replaces the synthesized picker, and the quick-keys "more" bar
        // carries the two compact activation surfaces: a Theme button that
        // cycles to the next theme in place, and a Themes button opening
        // the picker. Configs without a "more" bar stay untouched; their
        // authors reach the picker by naming the layer in their own button.
        if themes.enabled {
            if names.insert(THEMES_LAYER.to_owned()) {
                let mut keys = vec![ButtonConfig {
                    text: Some("Home".into()),
                    action: ButtonAction::SwitchLayer(home_name.to_owned()),
                    ..ButtonConfig::default()
                }];
                keys.extend(themes.names.iter().map(|name| ButtonConfig {
                    text: Some(name.clone()),
                    action: ButtonAction::ApplyThemeLayer {
                        theme: name.clone(),
                        layer: home_name.to_owned(),
                    },
                    ..ButtonConfig::default()
                }));
                layers.push(LayerConfig {
                    name: THEMES_LAYER.into(),
                    keys,
                });
            }
            let visible = |button: &ButtonConfig| {
                button.text.as_deref().map_or(
                    button.icon.is_some() || button.time.is_some() || button.battery.is_some(),
                    |text| !text.trim().is_empty(),
                )
            };
            let more = layers.iter().position(|layer| layer.name == "more");
            if let Some(more) = more {
                let more_layer = &mut layers[more];
                let has_cycle = more_layer.keys.iter().any(|button| {
                    matches!(button.action, ButtonAction::CycleTheme) && visible(button)
                });
                let has_picker = more_layer.keys.iter().any(|button| {
                    button.action.layer_target() == Some(THEMES_LAYER) && visible(button)
                });
                if !has_cycle {
                    more_layer.keys.push(ButtonConfig {
                        text: Some("Theme".into()),
                        action: ButtonAction::CycleTheme,
                        ..ButtonConfig::default()
                    });
                }
                if !has_picker {
                    more_layer.keys.push(ButtonConfig {
                        text: Some("Themes".into()),
                        action: ButtonAction::SwitchLayer(THEMES_LAYER.into()),
                        ..ButtonConfig::default()
                    });
                }
            }
        }
        for layer in &layers {
            for button in &layer.keys {
                if let Some(target) = button.action.layer_target() {
                    if !names.contains(target) {
                        bail!(
                            "layer {:?} has an unknown button target {target:?}",
                            layer.name
                        );
                    }
                }
                if button
                    .action
                    .command()
                    .is_some_and(|command| command.trim().is_empty())
                {
                    bail!("layer {:?} has a button with an empty command", layer.name);
                }
            }
        }
        // Every named sub-bar remains navigable even when its author omits Home.
        let home_name = home_name.to_owned();
        for (index, layer) in layers.iter_mut().enumerate() {
            if !legacy
                && index != home
                && !layer.keys.iter().any(|button| {
                    button.action.layer_target() == Some(home_name.as_str())
                        && button.text.as_deref().map_or(
                            button.icon.is_some()
                                || button.time.is_some()
                                || button.battery.is_some(),
                            |text| !text.trim().is_empty(),
                        )
                })
            {
                layer.keys.insert(
                    0,
                    ButtonConfig {
                        text: Some("Home".into()),
                        action: ButtonAction::SwitchLayer(home_name.clone()),
                        ..ButtonConfig::default()
                    },
                );
            }
            if width >= 2170 {
                layer.keys.insert(
                    0,
                    ButtonConfig {
                        text: Some("esc".into()),
                        action: ButtonAction::Keys(vec![Key::Esc]),
                        ..ButtonConfig::default()
                    },
                );
            }
        }
        Ok(LayerLayout {
            layers,
            home,
            fn_layer,
            legacy,
        })
    }
}

fn parse_config(base: &str, user: Option<&str>, width: u16) -> Result<(ConfigProxy, LayerLayout)> {
    let mut base: ConfigProxy = toml::from_str(base).context("invalid base configuration")?;
    if let Some(user) = user {
        base.merge(toml::from_str(user).context("invalid user configuration")?);
    }
    if base.battery_cycle_timeout_secs == Some(0) {
        bail!("BatteryCycleTimeoutSecs must be greater than zero");
    }
    if base
        .battery_warning_threshold
        .is_some_and(|value| value > 100)
    {
        bail!("BatteryWarningThreshold must be at most 100");
    }
    if base
        .battery_critical_threshold
        .is_some_and(|value| value > 100)
    {
        bail!("BatteryCriticalThreshold must be at most 100");
    }
    if base.battery_critical_threshold.unwrap_or(10) > base.battery_warning_threshold.unwrap_or(25)
    {
        bail!("BatteryCriticalThreshold must not exceed BatteryWarningThreshold");
    }
    base.pet_variant
        .as_deref()
        .map(PetVariant::parse)
        .transpose()
        .map_err(|err| anyhow!(err))?;
    base.pet_energy
        .as_deref()
        .map(PetEnergy::parse)
        .transpose()
        .map_err(|err| anyhow!(err))?;
    base.day_cycle()?;
    let themes = ThemeSet::resolve(base.themes_enabled.unwrap_or(true), base.themes.clone())?;
    if let Some(active) = base.active_theme.as_deref() {
        if active.trim().is_empty() {
            bail!("ActiveTheme must not be empty");
        }
        if !themes.contains(active) {
            bail!("ActiveTheme {active:?} is not a defined theme");
        }
    }
    let layers = base.layer_layout(width, &themes)?;
    Ok((base, layers))
}

fn load_font(name: &str) -> FontFace {
    let fontconfig = FontConfig::new();
    let mut pattern = Pattern::new(name);
    fontconfig.perform_substitutions(&mut pattern);
    let pat_match = match fontconfig.match_pattern(&pattern) {
        Ok(pat) => pat,
        Err(_) => panic!("Unable to find specified font. If you are using the default config, make sure you have at least one font installed")
    };
    let file_name = pat_match.get_file_name();
    let file_idx = pat_match.get_font_index();
    let ft_library = FtLibrary::init().unwrap();
    let face = ft_library.new_face(file_name, file_idx).unwrap();
    FontFace::create_from_ft(&face).unwrap()
}

fn load_config(
    width: u16,
    base_path: &Path,
    user_path: Option<&Path>,
    state_path: Option<&Path>,
) -> Result<(Config, LayerDeck)> {
    let user = user_path.map(read_to_string).transpose();
    let user = match user {
        Ok(user) => user,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err.into()),
    };
    let (base, layout) = parse_config(&read_to_string(base_path)?, user.as_deref(), width)?;
    let global_day_cycle = base.day_cycle()?;
    // Resolve the active theme: a config `ActiveTheme` pins the choice,
    // and with no pin the state file's last on-glass choice decides. A
    // state entry that no longer names a theme (the theme was removed
    // or renamed) falls back to the first theme and the file heals to
    // match, so a stale file can never wedge the bar.
    let mut themes = ThemeSet::resolve(base.themes_enabled.unwrap_or(true), base.themes.clone())?;
    themes.active = base.active_theme.clone().filter(|_| themes.enabled);
    if themes.enabled && themes.active.is_none() {
        match state_path.and_then(read_persisted_theme) {
            Some(name) if themes.contains(&name) => themes.active = Some(name),
            Some(stale) => {
                eprintln!(
                    "persisted theme {stale:?} is no longer defined; using {:?}",
                    themes.names.first()
                );
                themes.active = themes.names.first().cloned();
            }
            None => {}
        }
    }
    if themes.enabled && themes.active.is_none() {
        themes.active = themes.names.first().cloned();
    }
    if themes.enabled {
        if let Some(active) = &themes.active {
            persist_active_theme(state_path, active);
        }
    }
    let palette = themes.palette();
    let pet_enabled = base.pet_enabled.unwrap_or(true);
    let pet_layer = pet_enabled
        .then(|| {
            layout
                .layers
                .iter()
                .position(|layer| layer.name == PET_LAYER)
        })
        .flatten();
    let layers = LayerDeck::new(
        layout
            .layers
            .into_iter()
            .map(|layer| (layer.name, FunctionLayer::with_config(layer.keys)))
            .collect(),
        layout.home,
        layout.fn_layer,
        pet_layer,
        layout.legacy,
    );
    // Buttons resolve accent colors at draw time, except the waveform
    // widgets' live-dot tint, which is resolved once per build; retint
    // every widget so the active theme paints them too.
    let mut layers = layers;
    layers.apply_palette(&palette);
    let day_cycle = themes.day_cycle(&global_day_cycle);
    let cfg = Config {
        show_button_outlines: base.show_button_outlines.unwrap(),
        enable_pixel_shift: base.enable_pixel_shift.unwrap(),
        adaptive_brightness: base.adaptive_brightness.unwrap(),
        font_face: load_font(&base.font_template.unwrap()),
        active_brightness: base.active_brightness.unwrap(),
        double_press_switch_layers: base.double_press_switch_layers.unwrap_or(0),
        battery_cycle_enabled: base.battery_cycle_enabled.unwrap_or(true),
        battery_cycle_timeout_secs: base.battery_cycle_timeout_secs.unwrap_or(15),
        battery_warning_threshold: base.battery_warning_threshold.unwrap_or(25),
        battery_critical_threshold: base.battery_critical_threshold.unwrap_or(10),
        battery_warning_color: base.battery_warning_color.unwrap_or(ButtonAccent::Orange),
        battery_critical_color: base.battery_critical_color.unwrap_or(ButtonAccent::Red),
        pet_enabled,
        pet_variant: base
            .pet_variant
            .as_deref()
            .and_then(|value| PetVariant::parse(value).ok())
            .unwrap_or_default(),
        pet_energy: base
            .pet_energy
            .as_deref()
            .and_then(|value| PetEnergy::parse(value).ok())
            .unwrap_or_default(),
        global_day_cycle,
        day_cycle,
        palette,
        themes,
    };
    Ok((cfg, layers))
}

/// Read the persisted active-theme choice, trimmed; `None` when the
/// file is missing or blank.
fn read_persisted_theme(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let name = text.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// Best-effort persistence of the active theme: written atomically
/// (staged then renamed) only when the file does not already say it.
/// Failures are logged, never fatal - the bar still applies the theme
/// live without the file.
fn persist_active_theme(state_path: Option<&Path>, name: &str) {
    let Some(path) = state_path else {
        return;
    };
    if read_persisted_theme(path).as_deref() == Some(name) {
        return;
    }
    if let Err(err) = write_persisted_theme(path, name) {
        eprintln!("could not persist the active theme {name:?}: {err}");
    }
}

fn write_persisted_theme(path: &Path, name: &str) -> std::io::Result<()> {
    let stage = path.with_extension("tmp");
    fs::write(&stage, name)?;
    fs::rename(&stage, path)
}

pub struct ConfigManager {
    inotify_fd: Inotify,
    dir_watch: Option<WatchDescriptor>,
    file_watch: Option<WatchDescriptor>,
    user_path: PathBuf,
    base_path: PathBuf,
    state_path: PathBuf,
}

fn arm_dir_watch(inotify_fd: &Inotify, user_path: &Path) -> Option<WatchDescriptor> {
    // Watch the parent to handle both direct writes and editors' atomic renames.
    let flags = AddWatchFlags::IN_MOVED_TO
        | AddWatchFlags::IN_MOVED_FROM
        | AddWatchFlags::IN_CLOSE_WRITE
        | AddWatchFlags::IN_DELETE
        | AddWatchFlags::IN_DELETE_SELF
        | AddWatchFlags::IN_MOVE_SELF;
    match inotify_fd.add_watch(user_path.parent().unwrap(), flags) {
        Ok(wd) => Some(wd),
        Err(Errno::ENOENT) => None,
        e => Some(e.unwrap()),
    }
}

fn arm_file_watch(inotify_fd: &Inotify, user_path: &Path) -> Option<WatchDescriptor> {
    let flags =
        AddWatchFlags::IN_CLOSE_WRITE | AddWatchFlags::IN_MOVE_SELF | AddWatchFlags::IN_DELETE_SELF;
    match inotify_fd.add_watch(user_path, flags) {
        Ok(wd) => Some(wd),
        Err(Errno::ENOENT) => None,
        e => Some(e.unwrap()),
    }
}

impl ConfigManager {
    pub fn new() -> ConfigManager {
        Self::with_paths(
            PathBuf::from(BASE_CFG_PATH),
            PathBuf::from(USER_CFG_PATH),
            PathBuf::from(THEME_STATE_PATH),
        )
    }
    fn with_paths(base_path: PathBuf, user_path: PathBuf, state_path: PathBuf) -> ConfigManager {
        let inotify_fd = Inotify::init(InitFlags::IN_NONBLOCK).unwrap();
        let dir_watch = arm_dir_watch(&inotify_fd, &user_path);
        let file_watch = arm_file_watch(&inotify_fd, &user_path);
        ConfigManager {
            inotify_fd,
            dir_watch,
            file_watch,
            user_path,
            base_path,
            state_path,
        }
    }
    /// Persist a theme chosen on the glass for the next start; atomic
    /// and best-effort, exactly like the load-time persistence.
    pub fn save_theme(&self, name: &str) -> std::io::Result<()> {
        write_persisted_theme(&self.state_path, name)
    }
    pub fn load_config(&self, width: u16) -> (Config, LayerDeck) {
        load_config(
            width,
            &self.base_path,
            Some(&self.user_path),
            Some(&self.state_path),
        )
        .unwrap_or_else(|err| {
            eprintln!("Ignoring user configuration: {err:#}");
            load_config(width, &self.base_path, None, Some(&self.state_path))
                .expect("failed to load base configuration")
        })
    }
    pub fn update_config(&mut self, width: u16) -> Option<(Config, LayerDeck)> {
        if !self.pending_reload() {
            return None;
        }
        match load_config(
            width,
            &self.base_path,
            Some(&self.user_path),
            Some(&self.state_path),
        ) {
            Ok(parts) => Some(parts),
            Err(err) => {
                eprintln!("Keeping current configuration after reload error: {err:#}");
                None
            }
        }
    }
    fn pending_reload(&mut self) -> bool {
        let mut pending = false;
        if self.dir_watch.is_none() {
            self.dir_watch = arm_dir_watch(&self.inotify_fd, &self.user_path);
            pending = self.dir_watch.is_some();
        }
        if self.file_watch.is_none() {
            self.file_watch = arm_file_watch(&self.inotify_fd, &self.user_path);
            pending |= self.file_watch.is_some();
        }
        match self.inotify_fd.read_events() {
            Err(Errno::EAGAIN) => pending,
            r => self.handle_events(r) || pending,
        }
    }
    #[cold]
    fn handle_events(&mut self, evts: Result<Vec<InotifyEvent>, Errno>) -> bool {
        let mut ret = false;
        for evt in evts.unwrap() {
            if Some(evt.wd) == self.dir_watch {
                if evt.mask.intersects(
                    AddWatchFlags::IN_IGNORED
                        | AddWatchFlags::IN_DELETE_SELF
                        | AddWatchFlags::IN_MOVE_SELF,
                ) {
                    self.dir_watch = None;
                }
                if evt.name.as_deref() == self.user_path.file_name() {
                    self.rearm_file_watch();
                    ret = true;
                }
            } else if Some(evt.wd) == self.file_watch {
                if evt
                    .mask
                    .intersects(AddWatchFlags::IN_MOVE_SELF | AddWatchFlags::IN_DELETE_SELF)
                {
                    self.rearm_file_watch();
                    ret = true;
                } else if evt.mask.intersects(AddWatchFlags::IN_IGNORED) {
                    self.file_watch = arm_file_watch(&self.inotify_fd, &self.user_path);
                } else if evt.mask.intersects(AddWatchFlags::IN_CLOSE_WRITE) {
                    ret = true;
                }
            }
        }
        ret
    }
    fn rearm_file_watch(&mut self) {
        if let Some(wd) = self.file_watch.take() {
            let _ = self.inotify_fd.rm_watch(wd);
        }
        self.file_watch = arm_file_watch(&self.inotify_fd, &self.user_path);
    }
    pub fn fd(&self) -> &impl AsFd {
        &self.inotify_fd
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::time::{Duration, Instant};

    const BASE: &str = include_str!("../share/tiny-dfr/config.toml");
    const NAMED: &str = r#"
DefaultLayer = "home"
FnLayer = "function"
[[Layers]]
Name = "home"
Keys = [{ Text = "Functions", Action = { Layer = "function" } }]
[[Layers]]
Name = "function"
Keys = [{ Text = "F1", Action = "F1" }]
"#;
    /// The minimal named layout plus a quick-keys `more` bar, so the theme
    /// system's synthesized buttons have their home.
    const NAMED_WITH_MORE: &str = r#"
DefaultLayer = "home"
FnLayer = "function"
[[Layers]]
Name = "home"
Keys = [{ Text = "Functions", Action = { Layer = "function" } }]
[[Layers]]
Name = "function"
Keys = [{ Text = "F1", Action = "F1" }]
[[Layers]]
Name = "more"
Keys = [
    { Text = "Sync", Action = { Layer = "home" } },
]
"#;

    #[test]
    fn button_accents_are_optional_and_invalid_values_reject_the_candidate() {
        let (_, old) = parse_config(BASE, Some(NAMED), 2008).unwrap();
        assert_eq!(old.layers[0].keys[0].accent, None);
        for (name, accent) in [
            ("neutral", ButtonAccent::Neutral),
            ("cyan", ButtonAccent::Cyan),
            ("amber", ButtonAccent::Amber),
            ("blue", ButtonAccent::Blue),
            ("violet", ButtonAccent::Violet),
            ("orange", ButtonAccent::Orange),
            ("red", ButtonAccent::Red),
            ("green", ButtonAccent::Green),
        ] {
            let candidate = NAMED.replace(
                "Text = \"F1\"",
                &format!("Accent = \"{name}\", Text = \"F1\""),
            );
            let (_, styled) = parse_config(BASE, Some(&candidate), 2008).unwrap();
            let key = styled.layers[1].keys.last().unwrap();
            assert_eq!(key.accent, Some(accent));
            assert_eq!(key.action, ButtonAction::Keys(vec![Key::F1]));
        }
        let invalid = NAMED.replace("Text = \"F1\"", "Accent = \"magenta\", Text = \"F1\"");
        assert!(parse_config(BASE, Some(&invalid), 2008).is_err());
    }

    #[test]
    fn waveform_buttons_carry_an_optional_graph_style() {
        for (value, style) in [
            ("line", crate::waveform::Style::Line),
            ("bars", crate::waveform::Style::Bars),
        ] {
            let candidate = NAMED.replace(
                "Text = \"F1\"",
                &format!("Waveform = true, WaveformStyle = \"{value}\""),
            );
            let (_, styled) = parse_config(BASE, Some(&candidate), 2008).unwrap();
            let key = styled.layers[1].keys.last().unwrap();
            assert_eq!(key.waveform, Some(true));
            assert_eq!(key.waveform_style.as_deref(), Some(value));
            assert_eq!(crate::waveform::Style::parse(value), Some(style));
        }
        // A style without a Waveform button stays inert; an unknown value
        // maps to no style, which the button builder reports as a
        // configuration error.
        let inert = NAMED.replace("Text = \"F1\"", "Text = \"F1\", WaveformStyle = \"line\"");
        let (_, styled) = parse_config(BASE, Some(&inert), 2008).unwrap();
        assert_eq!(styled.layers[1].keys.last().unwrap().waveform, None);
        assert!(crate::waveform::Style::parse("dots").is_none());
    }

    #[test]
    fn visualizer_buttons_carry_their_own_knobs_and_stay_inert_without_one() {
        for (value, style) in [
            ("line", crate::waveform::Style::Line),
            ("bars", crate::waveform::Style::Bars),
        ] {
            let candidate = NAMED.replace(
                "Text = \"F1\"",
                &format!(
                    "Visualizer = true, VisualizerStyle = \"{value}\", \
                     VisualizerSmoothing = 240, VisualizerLag = 80"
                ),
            );
            let (_, styled) = parse_config(BASE, Some(&candidate), 2008).unwrap();
            let key = styled.layers[1].keys.last().unwrap();
            assert_eq!(key.visualizer, Some(true));
            assert_eq!(key.visualizer_style.as_deref(), Some(value));
            assert_eq!(key.visualizer_smoothing, Some(240));
            assert_eq!(key.visualizer_lag, Some(80));
            assert_eq!(crate::waveform::Style::parse(value), Some(style));
        }
        // The knobs without a Visualizer button stay inert, and an
        // unknown style maps to no style, which the button builder
        // reports as a configuration error.
        let inert = NAMED.replace("Text = \"F1\"", "Text = \"F1\", VisualizerStyle = \"line\"");
        let (_, styled) = parse_config(BASE, Some(&inert), 2008).unwrap();
        assert_eq!(styled.layers[1].keys.last().unwrap().visualizer, None);
        assert!(crate::waveform::Style::parse("dots").is_none());
        // A waveform button and a visualizer button coexist on one bar as
        // distinct widgets: the first meters the mic, the second music. The
        // synthesized Home button leads the sub-bar as usual.
        let both = NAMED.replace("Text = \"F1\"", "Waveform = true, Text = \"F1\"")
            + "\n[[Layers]]\nName = \"media\"\nKeys = [{ Visualizer = true, Stretch = 2 }]\n";
        let (_, styled) = parse_config(BASE, Some(&both), 2008).unwrap();
        let media = styled
            .layers
            .iter()
            .find(|layer| layer.name == "media")
            .unwrap();
        assert_eq!(media.keys.len(), 2);
        assert_eq!(media.keys[0].text.as_deref(), Some("Home"));
        assert_eq!(media.keys[1].visualizer, Some(true));
        assert_eq!(media.keys[1].waveform, None);
    }

    #[test]
    fn now_playing_buttons_parse_and_coexist_with_their_neighbors() {
        // The knob lands on the button that declares it.
        let candidate = NAMED.replace("Text = \"F1\"", "NowPlaying = true, Text = \"F1\"");
        let (_, styled) = parse_config(BASE, Some(&candidate), 2008).unwrap();
        let key = styled.layers[1].keys.last().unwrap();
        assert_eq!(key.now_playing, Some(true));
        assert_eq!(key.visualizer, None);
        // Buttons without the knob stay inert.
        let (_, styled) = parse_config(BASE, Some(NAMED), 2008).unwrap();
        assert_eq!(styled.layers[1].keys.last().unwrap().now_playing, None);
        // A now-playing row and a visualizer button coexist on one bar
        // as distinct widgets, each display-only, alongside transport:
        // the shipped media bar's shape. The synthesized Home button
        // leads the sub-bar as usual.
        let media = "\n[[Layers]]\nName = \"media\"\nKeys = [{ Icon = \"play_pause\", Action = \"PlayPause\" }, \
            { NowPlaying = true, Stretch = 2 }, { Visualizer = true }]\n";
        let (_, styled) = parse_config(BASE, Some(&(NAMED.to_owned() + media)), 2008).unwrap();
        let media = styled
            .layers
            .iter()
            .find(|layer| layer.name == "media")
            .unwrap();
        assert_eq!(media.keys.len(), 4);
        assert_eq!(media.keys[0].text.as_deref(), Some("Home"));
        assert_eq!(
            media.keys[1].action,
            ButtonAction::Keys(vec![Key::PlayPause])
        );
        assert_eq!(media.keys[2].now_playing, Some(true));
        assert_eq!(media.keys[2].stretch, Some(2));
        assert_eq!(media.keys[3].visualizer, Some(true));
    }

    #[test]
    fn approved_multibar_preserves_voice_dispatch_and_renders_offline() {
        use chrono::Timelike;
        let tmp = TempDir::new();
        let base_path = tmp.0.join("base.toml");
        let user_path = tmp.0.join("config.toml");
        fs::write(&base_path, BASE).unwrap();
        let named = std::env::var_os("DFR_DECK_CONFIG_REVIEW_INPUT")
            .map(fs::read_to_string)
            .transpose()
            .unwrap()
            .unwrap_or_else(|| include_str!("../share/tiny-dfr/multibar.toml").into());
        fs::write(&user_path, named).unwrap();
        let (mut cfg, mut deck) = load_config(2008, &base_path, Some(&user_path), None).unwrap();

        // Renders one frame with an explicit background color, used for the
        // time-of-day band review frames below.
        let render_with_background =
            |deck: &mut LayerDeck, name: &str, cfg: &Config, background: (f64, f64, f64)| {
                let portrait =
                    cairo::ImageSurface::create(cairo::Format::ARgb32, 64, 2048).unwrap();
                deck.slider.pixel_shift_width = if cfg.enable_pixel_shift {
                    crate::PIXEL_SHIFT_WIDTH_PX
                } else {
                    0
                };
                deck.slider.pixel_shift_x = 0.0;
                let volume = deck.slider_render();
                deck.current_mut().draw(
                    cfg,
                    (2008, 60),
                    &portrait,
                    (0.0, 0.0),
                    true,
                    volume,
                    background,
                );
                if let Some(dir) = std::env::var_os("DFR_DECK_REVIEW_OUTPUT") {
                    fs::create_dir_all(&dir).unwrap();
                    let horizontal =
                        cairo::ImageSurface::create(cairo::Format::ARgb32, 2008, 60).unwrap();
                    let c = cairo::Context::new(&horizontal).unwrap();
                    c.translate(0.0, 60.0);
                    c.rotate((-90.0_f64).to_radians());
                    c.set_source_surface(&portrait, 0.0, 0.0).unwrap();
                    c.paint().unwrap();
                    horizontal
                        .write_to_png(&mut fs::File::create(Path::new(&dir).join(name)).unwrap())
                        .unwrap();
                }
            };
        let render = |deck: &mut LayerDeck, name: &str| {
            let background = cfg.day_cycle.background(chrono::Local::now().hour());
            render_with_background(deck, name, &cfg, background);
        };
        let tap = |deck: &mut LayerDeck, label: &str, commands: &mut Vec<String>| {
            let index = deck.current().buttons.iter().position(|(_, button)| {
                matches!(&button.image, crate::ButtonImage::Text(text) if text == label)
            }).unwrap();
            let spans = deck.current().layout_spans(2008, 0);
            let x = (spans[index].left + spans[index].right) / 2.0;
            let mut emit = |keys: &[Key], _| assert!(keys.is_empty(), "voice button emitted keys");
            deck.touch_down_at(
                0,
                x,
                30.0,
                2008,
                60,
                &mut emit,
                &mut |command| commands.push(command.into()),
                &mut |_| None,
            );
            deck.touch_up(0, &mut emit);
        };
        let mut commands = Vec::new();
        assert!(matches!(&deck.current().buttons.last().unwrap().1.image,
            crate::ButtonImage::Text(text) if text == "Mic"));
        for layer in [deck.home, deck.names["media"]] {
            assert_eq!(
                deck.layers[layer]
                    .buttons
                    .iter()
                    .filter(|(_, button)| button.action == ButtonAction::VolumeSlider)
                    .count(),
                1
            );
            assert!(!deck.layers[layer].buttons.iter().any(|(_, button)|
                matches!(&button.action, ButtonAction::Keys(keys)
                    if keys.iter().any(|key| matches!(key, Key::VolumeUp | Key::VolumeDown | Key::Mute)))));
        }
        let brightness_index = deck
            .current()
            .buttons
            .iter()
            .position(|(_, button)| button.action == ButtonAction::BrightnessSlider)
            .unwrap();
        assert_eq!(brightness_index, 1);
        assert_eq!(
            deck.current()
                .buttons
                .iter()
                .filter(|(_, b)| b.action == ButtonAction::BrightnessSlider)
                .count(),
            1
        );
        assert!(!deck.current().buttons.iter().any(|(_, b)| matches!(&b.action, ButtonAction::Keys(keys) if keys.iter().any(|k| matches!(k, Key::BrightnessDown | Key::BrightnessUp)))));
        render(&mut deck, "implemented-home.png");
        // The persistent closed-slider mute mark on the same home bar: the
        // watch's probe read a muted mixer, so the volume button carries
        // the slash the drawn slider would show; the bar is otherwise the
        // identical plain frame above, at the same geometry.
        let (revision, request) = deck
            .mute_probe(Instant::now())
            .expect("a volume-slider deck probes the mixer for the closed-bar mute mark");
        assert_eq!(request, crate::level_control::Request::Get);
        deck.mute_reply(
            revision,
            Ok(crate::level_control::State {
                percent: 65,
                muted: true,
            }),
        );
        assert!(deck
            .current()
            .buttons
            .iter()
            .any(|(_, button)| button.muted && button.action == ButtonAction::VolumeSlider));
        render(&mut deck, "implemented-home-muted.png");
        deck.show_volume_muted(false);
        // One review frame per time-of-day band, at an hour inside each
        // default band, showing the strip background behind the same home bar.
        for (hour, name) in [(7, "morning"), (14, "day"), (19, "evening"), (23, "night")] {
            let background = cfg.day_cycle.background(hour);
            render_with_background(
                &mut deck,
                &format!("implemented-daycycle-{name}.png"),
                &cfg,
                background,
            );
        }
        deck.switch_to(deck.names["media"], &mut |_, _| {});
        render(&mut deck, "implemented-media.png");
        // The now-playing row doing its job on the same bar: a playing
        // track's title - a long one, truncated to the row's budget and
        // fitted - above its artist caption, then the graceful empty
        // state again: nothing playing, nothing stale. Without the
        // desktop receiver every poll is empty, so the shipped frame is
        // the dim `Nothing playing` state exactly as a missing helper
        // leaves it.
        assert!(
            deck.now_playing_tick(Some(crate::now_playing::Feed::Playing(
                crate::now_playing::Track {
                    title: "Lucky Star Episode 1 [English Dubbed]".into(),
                    artist: Some("Weaban".into()),
                },
            )))
        );
        render(&mut deck, "implemented-media-playing.png");
        assert!(deck.now_playing_tick(Some(crate::now_playing::Feed::Idle)));
        // The media bar's visualizer button: off by default with only its
        // glyph-and-Viz label group, then expanded into the full-bar view
        // with live monitor levels, then Off returning the plain media
        // bar - each exported for review.
        let viz_index = deck.current().visualizer.unwrap();
        let spans = deck.current().layout_spans(2008, 0);
        let viz_x = (spans[viz_index].left + spans[viz_index].right) / 2.0;
        assert!(!deck.viz_active());
        let mut viz_emit = |_: &[Key], _: bool| panic!("visualizer emitted keys");
        deck.touch_down_at(
            0,
            viz_x,
            30.0,
            2008,
            60,
            &mut viz_emit,
            &mut |_| {},
            &mut |_| None,
        );
        assert!(deck.viz_active());
        assert!(deck.current().viz_view);
        assert_eq!(deck.current().layout_spans(2008, 0).len(), 2);
        let mut viz_now = Instant::now();
        for level in [
            12, 38, 8, 61, 84, 30, 55, 72, 21, 47, 66, 15, 52, 33, 70, 44,
        ] {
            viz_now += Duration::from_millis(50);
            deck.visualizer_tick(
                Some(crate::MicSample {
                    level,
                    live: true,
                    dead: false,
                }),
                viz_now,
            );
        }
        render(&mut deck, "implemented-media-viz.png");
        // The view's single control is its Off button; tapping it restores
        // the exact media bar that opened the view.
        let off_spans = deck.current().layout_spans(2008, 0);
        let off_x = (off_spans[1].left + off_spans[1].right) / 2.0;
        deck.touch_down_at(
            1,
            off_x,
            30.0,
            2008,
            60,
            &mut viz_emit,
            &mut |_| {},
            &mut |_| None,
        );
        assert!(!deck.viz_active());
        assert!(!deck.current().viz_view);
        assert_eq!(deck.current().visualizer, Some(viz_index));
        deck.visualizer_tick(
            Some(crate::MicSample {
                level: 90,
                live: true,
                dead: false,
            }),
            viz_now,
        );
        render(&mut deck, "implemented-media-off.png");
        deck.switch_to(deck.home, &mut |_, _| {});
        let spans = deck.current().layout_spans(2008, 0);
        let x = (spans[0].left + spans[0].right) / 2.0;
        let mut emit = |_: &[Key], _: bool| panic!("slider emitted keys");
        deck.touch_down_at(0, x, 30.0, 2008, 60, &mut emit, &mut |_| {}, &mut |_| None);
        assert_eq!(deck.slider.region, Some((0, 1)));
        assert_eq!(
            deck.slider.requests.last().unwrap().2,
            crate::level_control::Request::Get
        );
        deck.slider_reply(
            deck.slider.revision,
            crate::slider::Kind::Volume,
            Ok(crate::LevelState {
                percent: 65,
                muted: false,
            }),
        );
        render(&mut deck, "implemented-volume-mid.png");
        let (left, right) = deck.slider_track(2008).unwrap();
        deck.touch_motion(0, left, 30.0, 2008, 60, &mut emit);
        deck.slider_reply(
            deck.slider.revision,
            crate::slider::Kind::Volume,
            Ok(crate::LevelState {
                percent: 0,
                muted: false,
            }),
        );
        render(&mut deck, "implemented-volume-zero.png");
        deck.slider_drag_motion(0, right, 30.0, 2008, &mut emit);
        deck.slider_reply(
            deck.slider.revision,
            crate::slider::Kind::Volume,
            Ok(crate::LevelState {
                percent: 130,
                muted: false,
            }),
        );
        assert_eq!(deck.slider.level, 130);
        render(&mut deck, "implemented-volume-max.png");
        deck.touch_up(0, &mut emit);
        deck.slider_collapse();
        let spans = deck.current().layout_spans(2008, 0);
        let brightness_x = (spans[brightness_index].left + spans[brightness_index].right) / 2.0;
        deck.touch_down_at(
            0,
            brightness_x,
            30.0,
            2008,
            60,
            &mut emit,
            &mut |_| {},
            &mut |_| None,
        );
        assert_eq!(deck.slider.region, Some((0, 1)));
        assert_eq!(deck.slider.kind, crate::slider::Kind::Brightness);
        assert!(!deck.slider.confirmed);
        assert_eq!(
            deck.slider.requests.last().unwrap().2,
            crate::level_control::Request::Get
        );
        deck.slider_reply(
            deck.slider.revision,
            crate::slider::Kind::Brightness,
            Ok(crate::LevelState {
                percent: 93,
                muted: false,
            }),
        );
        render(&mut deck, "implemented-brightness.png");
        deck.touch_motion(0, left, 30.0, 2008, 60, &mut emit);
        deck.slider_reply(
            deck.slider.revision,
            crate::slider::Kind::Brightness,
            Ok(crate::LevelState {
                percent: 1,
                muted: false,
            }),
        );
        render(&mut deck, "implemented-brightness-min.png");
        deck.slider_drag_motion(0, right, 30.0, 2008, &mut emit);
        deck.slider_reply(
            deck.slider.revision,
            crate::slider::Kind::Brightness,
            Ok(crate::LevelState {
                percent: 100,
                muted: false,
            }),
        );
        render(&mut deck, "implemented-brightness-max.png");
        deck.touch_up(0, &mut emit);
        // Mic stays reachable while the brightness overlay is still open.
        tap(&mut deck, "Mic", &mut commands);
        assert_eq!(deck.active, deck.names["recording"]);
        // The waveform widget shows live levels and its duration timer while
        // this layer is displayed; entering starts the session.
        let mut feed_now = Instant::now();
        let (activated, _) = deck.waveform_tick(None, feed_now);
        assert!(activated);
        for level in [
            8, 24, 12, 40, 66, 88, 35, 19, 57, 74, 28, 90, 46, 61, 33, 15, 52, 70,
        ] {
            feed_now += Duration::from_millis(50);
            deck.waveform_tick(
                Some(crate::MicSample {
                    level,
                    live: true,
                    dead: false,
                }),
                feed_now,
            );
        }
        // Mid-period frame: the graph is caught halfway between two samples,
        // with the newest level eased to half its measured value and the
        // whole curve glided half a slot left of its sample positions.
        let mid = Instant::now() - Duration::from_millis(25);
        deck.waveform_tick(
            Some(crate::MicSample {
                level: 88,
                live: true,
                dead: false,
            }),
            mid,
        );
        render(&mut deck, "implemented-recording-mid.png");
        // Settled frame: a sample one full period old rests the interpolation,
        // so the exported graph shows its sample positions and full heights.
        let settled = Instant::now() - Duration::from_millis(60);
        deck.waveform_tick(
            Some(crate::MicSample {
                level: 70,
                live: true,
                dead: false,
            }),
            settled,
        );
        feed_now += Duration::from_secs(73);
        deck.waveform_tick(None, feed_now);
        render(&mut deck, "implemented-recording-live.png");
        // Without the desktop level feed every poll fails: the widget shows
        // the dead-feed state - the red slashed dot and the `No feed` label
        // - never the calm idle look of a merely quiet feed.
        for _ in 0..4 {
            feed_now += Duration::from_millis(50);
            deck.waveform_tick(Some(crate::MicSample::dead()), feed_now);
        }
        render(&mut deck, "implemented-recording-dead.png");
        tap(&mut deck, "Stop", &mut commands);
        assert_eq!(deck.active, deck.home);
        tap(&mut deck, "Mic", &mut commands);
        tap(&mut deck, "Cancel", &mut commands);
        assert_eq!(deck.active, deck.home);
        // The home bar's More button opens the quick-keys bar; its
        // Screenshot entry fires the screenshot flow and returns home in
        // one press.
        tap(&mut deck, "More", &mut commands);
        assert_eq!(deck.active, deck.names["more"]);
        render(&mut deck, "implemented-more.png");
        // The theme system: the Themes opener switches into the picker, a
        // full-bar takeover listing every theme name; tapping one applies
        // it live - the palette and the day-cycle bands swap in place, no
        // restart, no reload - and returns home. The exported frames: the
        // picker itself, the home bar under the applied theme, and the
        // more bar's cycle button briefly wearing the applied name.
        let themes_button = deck
            .current()
            .buttons
            .iter()
            .position(|(_, button)| {
                matches!(&button.action, ButtonAction::SwitchLayer(name) if name == "themes")
            })
            .unwrap();
        let spans = deck.current().layout_spans(2008, 0);
        let themes_x = (spans[themes_button].left + spans[themes_button].right) / 2.0;
        deck.touch_down_at(
            0,
            themes_x,
            30.0,
            2008,
            60,
            &mut viz_emit,
            &mut |_| {},
            &mut |_| None,
        );
        assert_eq!(deck.active, deck.names["themes"]);
        assert!(deck.current().buttons.iter().any(|(_, button)| matches!(
            (&button.image, &button.action),
            (crate::ButtonImage::Text(text), ButtonAction::ApplyThemeLayer { theme, .. })
                if text == theme
        )));
        render(&mut deck, "implemented-themes.png");
        // Apply Nord by tapping its button through the same callback the
        // daemon wires, then watch the home bar render in its colors.
        let nord_button = deck
            .current()
            .buttons
            .iter()
            .position(
                |(_, button)| matches!(&button.action, ButtonAction::ApplyThemeLayer { theme, .. } if theme == "Nord"),
            )
            .unwrap();
        let spans = deck.current().layout_spans(2008, 0);
        let nord_x = (spans[nord_button].left + spans[nord_button].right) / 2.0;
        // The same apply callback the daemon wires: the palette, the day
        // cycle, and the active choice live on the config, and each tap's
        // callback borrows it only for that tap.
        let apply_theme = |cfg: &mut Config,
                           request: crate::themes::ThemeRequest|
         -> Option<(String, crate::appearance::Palette)> {
            let name = match request {
                crate::themes::ThemeRequest::Next => cfg.themes.cycle_next()?,
                crate::themes::ThemeRequest::Named(name) => name.to_owned(),
            };
            if !cfg.themes.apply(&name) {
                return None;
            }
            cfg.palette = cfg.themes.palette();
            cfg.day_cycle = cfg.themes.day_cycle(&cfg.global_day_cycle);
            Some((name, cfg.palette))
        };
        deck.touch_down_at(
            1,
            nord_x,
            30.0,
            2008,
            60,
            &mut viz_emit,
            &mut |_| {},
            &mut |request| apply_theme(&mut cfg, request),
        );
        assert_eq!(deck.active, deck.home);
        assert_eq!(cfg.themes.active.as_deref(), Some("Nord"));
        // The frame helper paints with whatever theme is live.
        let render = |deck: &mut LayerDeck, name: &str| {
            let background = cfg.day_cycle.background(chrono::Local::now().hour());
            render_with_background(deck, name, &cfg, background);
        };
        render(&mut deck, "implemented-home-nord.png");
        // The cycle button on the more bar: one tap applies the next theme
        // in place and briefly shows its name on the button.
        tap(&mut deck, "More", &mut commands);
        let theme_button = deck
            .current()
            .buttons
            .iter()
            .position(|(_, button)| matches!(button.action, ButtonAction::CycleTheme))
            .unwrap();
        let spans = deck.current().layout_spans(2008, 0);
        let theme_x = (spans[theme_button].left + spans[theme_button].right) / 2.0;
        deck.touch_down_at(
            2,
            theme_x,
            30.0,
            2008,
            60,
            &mut viz_emit,
            &mut |_| {},
            &mut |request| apply_theme(&mut cfg, request),
        );
        assert!(matches!(
            &deck.current().buttons[theme_button].1.image,
            crate::ButtonImage::Text(text) if text == "Ember"
        ));
        let render = |deck: &mut LayerDeck, name: &str| {
            let background = cfg.day_cycle.background(chrono::Local::now().hour());
            render_with_background(deck, name, &cfg, background);
        };
        render(&mut deck, "implemented-more-theme-flash.png");
        let (restored, _) = deck.theme_flash_tick(Instant::now() + Duration::from_millis(1600));
        assert!(restored);
        tap(&mut deck, "Screenshot", &mut commands);
        assert_eq!(deck.active, deck.home);
        assert_eq!(commands.len(), 5);
        let actions: Vec<_> = commands
            .iter()
            .map(|command| command.split_whitespace().last().unwrap())
            .collect();
        assert_eq!(actions, ["start", "stop", "start", "cancel", "screenshot"]);
    }

    #[test]
    fn upstream_layers_and_overrides_keep_key_actions() {
        for media_default in [false, true] {
            let user = format!(
                r#"
MediaLayerDefault = {media_default}
PrimaryLayerKeys = [{{ Text = "Combo", Action = ["LeftCtrl", "LeftAlt", "F1"] }}]
MediaLayerKeys = [{{ Text = "Volume", Action = "VolumeUp" }}]
DoublePressSwitchLayers = 200
"#
            );
            let (cfg, layout) = parse_config(BASE, Some(&user), 2008).unwrap();
            assert!(layout.legacy);
            // primary, media, the synthesized pet layer, and the
            // synthesized themes picker.
            assert_eq!(layout.layers.len(), 4);
            assert_eq!(layout.home, usize::from(media_default));
            assert_eq!(layout.fn_layer, Some(1 - layout.home));
            assert_eq!(
                layout.layers[0].keys[0].action,
                ButtonAction::Keys(vec![Key::LeftCtrl, Key::LeftAlt, Key::F1])
            );
            assert_eq!(
                layout.layers[1].keys[0].action,
                ButtonAction::Keys(vec![Key::VolumeUp])
            );
            assert_eq!(cfg.double_press_switch_layers, Some(200));
            assert_eq!(cfg.active_brightness, Some(128));
        }
        let (_, stock) = parse_config(BASE, None, 2008).unwrap();
        assert_eq!(stock.layers[0].keys.len(), 13);
        assert_eq!(stock.layers[1].keys.len(), 12);
    }

    #[test]
    fn named_layout_adds_a_visible_home_button_and_escape_when_needed() {
        for width in [2008, 2170] {
            let (_, layout) = parse_config(BASE, Some(NAMED), width).unwrap();
            assert!(!layout.legacy);
            assert_eq!(layout.home, 0);
            assert_eq!(layout.fn_layer, Some(1));
            let offset = usize::from(width >= 2170);
            let home = &layout.layers[1].keys[offset];
            assert_eq!(home.text.as_deref(), Some("Home"));
            assert_eq!(home.action, ButtonAction::SwitchLayer("home".into()));
            if offset == 1 {
                for layer in &layout.layers {
                    assert_eq!(layer.keys[0].action, ButtonAction::Keys(vec![Key::Esc]));
                }
            }
        }
        let (_, legacy) = parse_config(BASE, None, 2170).unwrap();
        assert!(legacy
            .layers
            .iter()
            .all(|layer| layer.keys[0].action == ButtonAction::Keys(vec![Key::Esc])));
        // The legacy primary bar ends with the synthesized Pet button.
        let primary = &legacy.layers[0];
        assert_eq!(primary.keys.len(), 14);
        let pet_button = primary.keys.last().unwrap();
        assert_eq!(pet_button.text.as_deref(), Some("Pet"));
        assert_eq!(pet_button.action, ButtonAction::SwitchLayer("pet".into()));
    }

    #[test]
    fn named_default_can_be_any_position_and_fn_is_optional() {
        let config = NAMED
            .replace("DefaultLayer = \"home\"", "DefaultLayer = \"function\"")
            .replace("FnLayer = \"function\"", "");
        let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
        assert_eq!(layout.home, 1);
        assert_eq!(layout.fn_layer, None);
        // Existing visible navigation to the default already provides a return path.
        assert_eq!(layout.layers[0].keys[0].text.as_deref(), Some("Functions"));
        assert_eq!(layout.layers[0].keys.len(), 1);
        // The Pet button joins whatever layer is the home bar, here "function".
        let home = &layout.layers[layout.home];
        assert_eq!(home.keys.len(), 2);
        assert_eq!(home.keys.last().unwrap().text.as_deref(), Some("Pet"));
    }

    #[test]
    fn an_invisible_home_action_does_not_suppress_home_insertion() {
        for button in [
            "{ Action = { Layer = \"home\" } }",
            "{ Text = \" \" , Action = { Layer = \"home\" } }",
            "{ Text = \" \" , Icon = \"ignored\", Action = { Layer = \"home\" } }",
        ] {
            let config = NAMED.replace("{ Text = \"F1\", Action = \"F1\" }", button);
            let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
            assert_eq!(layout.layers[1].keys[0].text.as_deref(), Some("Home"));
            assert_eq!(layout.layers[1].keys.len(), 2);
        }
    }

    #[test]
    fn example_has_all_function_keys_navigation_and_live_widgets() {
        let (_, layout) = parse_config(
            BASE,
            Some(include_str!("../share/tiny-dfr/multibar.toml")),
            2008,
        )
        .unwrap();
        assert_eq!(layout.layers.len(), 7);
        let home = &layout.layers[layout.home];
        // The clock is gone from the shipped home bar; its width goes to
        // the remaining buttons.
        assert!(!home.keys.iter().any(|key| key.time.is_some()));
        assert!(home.keys.iter().any(|key| key.battery.is_some()));
        for name in ["function", "media", "more", "pet"] {
            assert!(home
                .keys
                .iter()
                .any(|key| key.action == ButtonAction::SwitchLayer(name.into())));
            let layer = layout
                .layers
                .iter()
                .find(|layer| layer.name == name)
                .unwrap();
            assert_eq!(
                layer
                    .keys
                    .iter()
                    .filter(|key| key.action == ButtonAction::SwitchLayer("home".into()))
                    .count(),
                1
            );
        }
        // The voice-dictation flow: the mic button starts recording and
        // opens the recording layer in one press; its Cancel and Stop
        // buttons visibly return home, so no extra Home button appears.
        let recording = layout
            .layers
            .iter()
            .find(|layer| layer.name == "recording")
            .unwrap();
        assert_eq!(recording.keys.len(), 3);
        assert_eq!(
            recording.keys[0].action,
            ButtonAction::CommandLayer {
                command: "voice-dictate-user cancel".into(),
                layer: "home".into()
            }
        );
        // The middle button is the live input-level waveform widget; the
        // line style is the default, so no style in the shipped config.
        assert_eq!(recording.keys[1].waveform, Some(true));
        assert_eq!(recording.keys[1].waveform_style, None);
        assert!(recording.keys[1].text.is_none());
        assert_eq!(recording.keys[1].stretch, Some(3));
        assert_eq!(
            recording.keys[2].action,
            ButtonAction::CommandLayer {
                command: "voice-dictate-user stop".into(),
                layer: "home".into()
            }
        );
        let mic = home
            .keys
            .iter()
            .position(|key| key.text.as_deref() == Some("Mic"))
            .unwrap();
        assert_eq!(mic, home.keys.len() - 1);
        assert_eq!(home.keys[mic].accent, Some(ButtonAccent::Cyan));
        assert_eq!(
            home.keys[mic].action,
            ButtonAction::CommandLayer {
                command: "voice-dictate-user start".into(),
                layer: "recording".into()
            }
        );
        // The quick-keys bar: its one shipped entry fires the screenshot
        // flow and returns home, behind an explicit Home button that
        // leaves without running anything.
        let more = layout
            .layers
            .iter()
            .find(|layer| layer.name == "more")
            .unwrap();
        // Home, the shipped quick keys (Screenshot, Lock), and the two
        // synthesized theme buttons: the compact cycle button and the
        // picker opener.
        assert_eq!(more.keys.len(), 5);
        assert_eq!(more.keys[0].text.as_deref(), Some("Home"));
        assert_eq!(
            more.keys[0].action,
            ButtonAction::SwitchLayer("home".into())
        );
        assert_eq!(more.keys[1].text.as_deref(), Some("Screenshot"));
        assert_eq!(
            more.keys[1].action,
            ButtonAction::CommandLayer {
                command: "touchbar-capture-user screenshot".into(),
                layer: "home".into()
            }
        );
        // The Pet button joins the navigation cluster after Functions,
        // Media, and More, matching their styling.
        let positions: Vec<usize> = ["function", "media", "more", "pet"]
            .into_iter()
            .map(|name| {
                home.keys
                    .iter()
                    .position(|key| key.action == ButtonAction::SwitchLayer(name.into()))
                    .unwrap()
            })
            .collect();
        assert_eq!(positions, vec![3, 4, 5, 6]);
        // The media bar keeps its existing controls and gains two
        // buttons: the now-playing row after the transport cluster, and
        // the opt-in music visualizer at the end, off by default every
        // time.
        let media = layout
            .layers
            .iter()
            .find(|layer| layer.name == "media")
            .unwrap();
        assert_eq!(media.keys.len(), 7);
        assert_eq!(media.keys[1].icon.as_deref(), Some("fast_rewind"));
        assert_eq!(media.keys[2].icon.as_deref(), Some("play_pause"));
        assert_eq!(media.keys[3].icon.as_deref(), Some("fast_forward"));
        let now_playing = &media.keys[4];
        assert_eq!(now_playing.now_playing, Some(true));
        assert_eq!(now_playing.stretch, Some(2));
        assert_eq!(now_playing.action, ButtonAction::Keys(vec![]));
        assert_eq!(now_playing.text, None);
        assert_eq!(now_playing.icon, None);
        assert_eq!(media.keys[5].action, ButtonAction::VolumeSlider);
        assert_eq!(media.keys[5].stretch, Some(3));
        let viz = media.keys.last().unwrap();
        assert_eq!(viz.visualizer, Some(true));
        assert_eq!(viz.waveform, None);
        assert_eq!(viz.stretch, Some(2));
        assert_eq!(viz.action, ButtonAction::Keys(vec![]));
        let pet_button = &home.keys[positions[3]];
        assert_eq!(pet_button.text.as_deref(), Some("Pet"));
        let function = layout
            .layers
            .iter()
            .find(|layer| layer.name == "function")
            .unwrap();
        let expected = [
            Key::F1,
            Key::F2,
            Key::F3,
            Key::F4,
            Key::F5,
            Key::F6,
            Key::F7,
            Key::F8,
            Key::F9,
            Key::F10,
            Key::F11,
            Key::F12,
        ];
        assert_eq!(
            function
                .keys
                .iter()
                .skip(1)
                .map(|key| key.action.clone())
                .collect::<Vec<_>>(),
            expected
                .into_iter()
                .map(|key| ButtonAction::Keys(vec![key]))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn shipped_home_slider_pair_sizes_identically() {
        // The volume and brightness sliders are one pair of equal twins:
        // the sizing knob is the per-button Stretch, and the shipped home
        // bar must give both the same explicit value so they render the
        // same width.
        let (_, layout) = parse_config(
            BASE,
            Some(include_str!("../share/tiny-dfr/multibar.toml")),
            2008,
        )
        .unwrap();
        let home = &layout.layers[layout.home];
        for action in [ButtonAction::VolumeSlider, ButtonAction::BrightnessSlider] {
            assert_eq!(
                home.keys.iter().filter(|key| key.action == action).count(),
                1
            );
        }
        let volume = home
            .keys
            .iter()
            .position(|key| key.action == ButtonAction::VolumeSlider)
            .unwrap();
        let brightness = home
            .keys
            .iter()
            .position(|key| key.action == ButtonAction::BrightnessSlider)
            .unwrap();
        // Adjacent icon buttons sharing one explicit width.
        assert_eq!(brightness, volume + 1);
        assert!(home.keys[volume].icon.is_some());
        assert!(home.keys[brightness].icon.is_some());
        assert_eq!(home.keys[volume].stretch, home.keys[brightness].stretch);
        assert!(home.keys[volume].stretch.is_some());
    }

    #[test]
    fn command_actions_parse_alone_and_combined_with_layer_switches() {
        let config = format!(
            "{NAMED}\n\
[[Layers]]\n\
Name = \"recording\"\n\
Keys = [\n\
    {{ Text = \"Sync\", Action = {{ Command = \"sync-now\" }} }},\n\
    {{ Text = \"Mic\", Action = {{ Command = \"voice-dictate start\", Layer = \"recording\" }} }},\n\
]\n"
        );
        let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
        let recording = layout
            .layers
            .iter()
            .find(|layer| layer.name == "recording")
            .unwrap();
        // No button visibly returns home, so the generic Home button is
        // still synthesized ahead of the configured keys.
        assert_eq!(recording.keys.len(), 3);
        assert_eq!(
            recording.keys[0].action,
            ButtonAction::SwitchLayer("home".into())
        );
        assert_eq!(
            recording.keys[1].action,
            ButtonAction::Command("sync-now".into())
        );
        assert_eq!(
            recording.keys[2].action,
            ButtonAction::CommandLayer {
                command: "voice-dictate start".into(),
                layer: "recording".into()
            }
        );
        // The helpers see through both action shapes.
        assert_eq!(recording.keys[1].action.layer_target(), None);
        assert_eq!(recording.keys[1].action.command(), Some("sync-now"));
        assert_eq!(recording.keys[2].action.layer_target(), Some("recording"));
        assert_eq!(
            recording.keys[2].action.command(),
            Some("voice-dictate start")
        );
    }

    #[test]
    fn a_combined_command_button_counts_as_visible_home_navigation() {
        // A sub-bar whose buttons visibly return home through a command
        // needs no synthesized Home button, exactly like pure navigation.
        let config = NAMED.replace(
            "{ Text = \"F1\", Action = \"F1\" }",
            "{ Text = \"Stop\", Action = { Command = \"voice-dictate stop\", Layer = \"home\" } }",
        );
        let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
        let function = &layout.layers[1];
        assert_eq!(function.keys.len(), 1);
        assert_eq!(function.keys[0].text.as_deref(), Some("Stop"));
        // An invisible combined button still leaves room for the Home button.
        let config = config.replace("Text = \"Stop\"", "Text = \" \"");
        let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
        let function = &layout.layers[1];
        assert_eq!(function.keys.len(), 2);
        assert_eq!(function.keys[0].text.as_deref(), Some("Home"));
    }

    #[test]
    fn quick_key_entries_carry_a_label_an_optional_glyph_and_a_command() {
        // The shipped quick-keys bar: one example entry firing the
        // screenshot flow and returning home, behind an explicit Home
        // button that leaves without running anything.
        let example = include_str!("../share/tiny-dfr/multibar.toml");
        let (_, layout) = parse_config(BASE, Some(example), 2008).unwrap();
        let more = layout
            .layers
            .iter()
            .find(|layer| layer.name == "more")
            .unwrap();
        assert_eq!(more.keys.len(), 5);
        assert_eq!(more.keys[0].text.as_deref(), Some("Home"));
        assert_eq!(
            more.keys[0].action,
            ButtonAction::SwitchLayer("home".into())
        );
        let screenshot = &more.keys[1];
        assert_eq!(screenshot.text.as_deref(), Some("Screenshot"));
        assert_eq!(screenshot.icon, None);
        assert_eq!(
            screenshot.action,
            ButtonAction::CommandLayer {
                command: "touchbar-capture-user screenshot".into(),
                layer: "home".into()
            }
        );

        // A captain's own quick keys are further entries in the same Keys
        // array - a Text label or an Icon glyph, an optional Accent, and
        // the Command to run, with or without a Layer switch. Entries
        // without a Layer switch stay on the bar, and the visible
        // home-targeting buttons keep the synthesized Home button from
        // duplicating the explicit one.
        let screenshot_line = "    { Text = \"Screenshot\", Action = { Command = \"touchbar-capture-user screenshot\", Layer = \"home\" } },\n";
        let with_own_keys = concat!(
            "    { Text = \"Screenshot\", Action = { Command = \"touchbar-capture-user screenshot\", Layer = \"home\" } },\n",
            "    { Icon = \"search\", Action = { Command = \"my-quick-tool\" } },\n",
            "    { Text = \"Lock\", Accent = \"amber\", Action = { Command = \"loginctl lock-session\" } },\n",
        );
        let config = example.replace(screenshot_line, with_own_keys);
        let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
        let more = layout
            .layers
            .iter()
            .find(|layer| layer.name == "more")
            .unwrap();
        assert_eq!(more.keys.len(), 7);
        assert_eq!(more.keys[2].icon.as_deref(), Some("search"));
        assert!(more.keys[2].text.is_none());
        assert_eq!(
            more.keys[2].action,
            ButtonAction::Command("my-quick-tool".into())
        );
        assert_eq!(more.keys[3].text.as_deref(), Some("Lock"));
        assert_eq!(more.keys[3].accent, Some(ButtonAccent::Amber));
        assert_eq!(
            more.keys[3].action,
            ButtonAction::Command("loginctl lock-session".into())
        );
        // A quick key naming an unknown layer is rejected like any other
        // button target.
        let invalid = config.replace(
            "Command = \"my-quick-tool\"",
            "Command = \"my-quick-tool\", Layer = \"missing\"",
        );
        assert!(parse_config(BASE, Some(&invalid), 2008).is_err());
    }

    #[test]
    fn theme_actions_parse_in_all_three_shapes_and_reject_bad_combinations() {
        let config = format!(
            "{NAMED}\n\
[[Layers]]\n\
Name = \"restyler\"\n\
Keys = [\n\
    {{ Text = \"Theme\", Action = \"CycleTheme\" }},\n\
    {{ Text = \"Nord\", Action = {{ Theme = \"Nord\" }} }},\n\
    {{ Text = \"Pick\", Action = {{ Theme = \"Nord\", Layer = \"home\" }} }},\n\
]"
        );
        let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
        let restyler = layout
            .layers
            .iter()
            .find(|layer| layer.name == "restyler")
            .unwrap();
        // The restyler bar visibly returns home through its Pick button,
        // so no extra Home button is synthesized.
        assert_eq!(restyler.keys.len(), 3);
        assert_eq!(restyler.keys[0].action, ButtonAction::CycleTheme);
        assert_eq!(
            restyler.keys[1].action,
            ButtonAction::ApplyTheme("Nord".into())
        );
        assert_eq!(
            restyler.keys[2].action,
            ButtonAction::ApplyThemeLayer {
                theme: "Nord".into(),
                layer: "home".into()
            }
        );
        // The helpers see through the theme shapes like the others.
        assert_eq!(restyler.keys[0].action.layer_target(), None);
        assert_eq!(restyler.keys[1].action.command(), None);
        assert_eq!(restyler.keys[2].action.layer_target(), Some("home"));
        // A theme combined with a command, an empty theme name, or an
        // unknown layer target is rejected like any other bad action.
        for invalid in [
            config.replace(
                "Action = { Theme = \"Nord\" }",
                "Action = { Theme = \"Nord\", Command = \"x\" }",
            ),
            config.replace(
                "Action = { Theme = \"Nord\" }",
                "Action = { Theme = \" \" }",
            ),
            config.replace(
                "Theme = \"Nord\", Layer = \"home\"",
                "Theme = \"Nord\", Layer = \"missing\"",
            ),
            config.replace("Action = \"CycleTheme\"", "Action = \"CycleThemes\""),
        ] {
            assert!(
                parse_config(BASE, Some(&invalid), 2008).is_err(),
                "accepted invalid theme action: {invalid}"
            );
        }
    }

    #[test]
    fn theme_tables_parse_into_the_resolved_set_and_reject_invalid_knobs() {
        let user = "\nThemesEnabled = true\nActiveTheme = \"Mine\"\n\
[Themes.Mine.Accents]\ncyan = \"#40E0D0\"\namber = \"40e0a0\"\n";
        let (base, _) = parse_config(BASE, Some(user), 2008).unwrap();
        let themes =
            ThemeSet::resolve(base.themes_enabled.unwrap_or(true), base.themes.clone()).unwrap();
        assert!(themes.contains("Mine"));
        assert!(themes.contains("Nord"), "the seeded set always rides along");
        // An unknown or blank ActiveTheme rejects the candidate config.
        for invalid in [
            "ActiveTheme = \"Nope\"\n",
            "ActiveTheme = \" \"\n",
            "ThemesEnabled = 'true'\n",
            "[Themes.Bad.Accents]\nmauve = \"#101010\"\n",
            "[Themes.Bad.Accents]\ncyan = \"10g010\"\n",
            "[Themes.Bad]\n[[Themes.Bad.DayCycleBands]]\nName = \"x\"\nStartHour = 24\nColor = \"#101010\"\n",
            "[Themes.Bad]\nSurprise = true\n",
            "[Themes.\" \"].Accents\n",
        ] {
            assert!(
                parse_config(BASE, Some(invalid), 2008).is_err(),
                "accepted invalid theme configuration: {invalid}"
            );
        }
    }

    #[test]
    fn the_theme_picker_takes_over_its_own_layer_and_the_more_bar_gains_both_buttons() {
        // With the theme system on (the default), a `themes` layer is
        // synthesized listing every theme, and the quick-keys `more` bar
        // gains the compact Theme cycle button and the Themes opener.
        let (_, layout) = parse_config(BASE, Some(NAMED_WITH_MORE), 2008).unwrap();
        let picker = layout
            .layers
            .iter()
            .find(|layer| layer.name == "themes")
            .unwrap();
        let seeded = crate::themes::builtin_themes().unwrap();
        assert_eq!(
            picker.keys.len(),
            seeded.names.len() + 1,
            "one Home button plus one button per theme"
        );
        assert_eq!(picker.keys[0].text.as_deref(), Some("Home"));
        assert_eq!(
            picker.keys[0].action,
            ButtonAction::SwitchLayer("home".into())
        );
        for (button, name) in picker.keys.iter().skip(1).zip(&seeded.names) {
            assert_eq!(button.text.as_deref(), Some(name.as_str()));
            assert_eq!(
                button.action,
                ButtonAction::ApplyThemeLayer {
                    theme: name.clone(),
                    layer: "home".into()
                }
            );
        }
        let more = layout
            .layers
            .iter()
            .find(|layer| layer.name == "more")
            .unwrap();
        let theme = more.keys.last().unwrap();
        assert_eq!(theme.text.as_deref(), Some("Themes"));
        assert_eq!(theme.action, ButtonAction::SwitchLayer("themes".into()));
        assert_eq!(
            more.keys[more.keys.len() - 2].text.as_deref(),
            Some("Theme")
        );
        assert_eq!(
            more.keys[more.keys.len() - 2].action,
            ButtonAction::CycleTheme
        );
        // A user layer named `themes` replaces the synthesized picker,
        // and the more bar still gains the openers unless it carries its
        // own visible buttons for both surfaces.
        let config = format!(
            "{NAMED_WITH_MORE}\n\
[[Layers]]\n\
Name = \"themes\"\n\
Keys = [{{ Text = \"P1\" }}]"
        );
        let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
        let picker = layout
            .layers
            .iter()
            .find(|layer| layer.name == "themes")
            .unwrap();
        assert_eq!(picker.keys.len(), 2);
        assert_eq!(picker.keys[1].text.as_deref(), Some("P1"));
        // Author-provided visible buttons suppress both syntheses.
        let config = NAMED_WITH_MORE.replace(
            "    { Text = \"Sync\", Action = { Layer = \"home\" } },",
            "    { Text = \"Sync\", Action = { Layer = \"home\" } },\n    { Text = \"Palette\", Action = \"CycleTheme\" },\n    { Text = \"Looks\", Action = { Layer = \"themes\" } },",
        );
        let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
        let more = layout
            .layers
            .iter()
            .find(|layer| layer.name == "more")
            .unwrap();
        assert_eq!(
            more.keys
                .iter()
                .filter(|key| matches!(key.action, ButtonAction::CycleTheme))
                .count(),
            1
        );
        assert!(
            !more
                .keys
                .iter()
                .any(|key| key.text.as_deref() == Some("Theme")),
            "the synthesized cycle button yields to the author's own"
        );
        assert!(
            !more
                .keys
                .iter()
                .any(|key| key.text.as_deref() == Some("Themes")),
            "the synthesized opener yields to the author's own"
        );
        // Configs without a `more` bar stay untouched: their home bar gains
        // nothing and only the picker layer exists, reachable through the
        // author's own button.
        let (_, layout) = parse_config(BASE, Some(NAMED), 2008).unwrap();
        let home = &layout.layers[layout.home];
        assert!(!home.keys.iter().any(|key| {
            matches!(key.action, ButtonAction::CycleTheme)
                || key.action.layer_target() == Some("themes")
        }));
        assert!(layout.layers.iter().any(|layer| layer.name == "themes"));
    }

    #[test]
    fn disabling_the_theme_system_removes_its_layer_and_buttons_and_rejects_stale_targets() {
        let disabled = format!("ThemesEnabled = false\n{NAMED_WITH_MORE}");
        let (_, layout) = parse_config(BASE, Some(&disabled), 2008).unwrap();
        assert!(layout.layers.iter().all(|layer| layer.name != "themes"));
        let more = layout
            .layers
            .iter()
            .find(|layer| layer.name == "more")
            .unwrap();
        assert!(!more.keys.iter().any(|key| {
            matches!(key.action, ButtonAction::CycleTheme)
                || key
                    .action
                    .layer_target()
                    .is_some_and(|target| target == "themes")
        }));
        // A button still targeting the removed layer is an unknown target.
        let stale = disabled.replace(
            "    { Text = \"Sync\", Action = { Layer = \"home\" } },",
            "    { Text = \"Sync\", Action = { Layer = \"home\" } },\n    { Text = \"Looks\", Action = { Layer = \"themes\" } },",
        );
        assert!(parse_config(BASE, Some(&stale), 2008).is_err());
    }

    #[test]
    fn the_active_theme_builds_the_palette_and_bands_from_the_config_pin() {
        let tmp = TempDir::new();
        let base_path = tmp.0.join("base.toml");
        fs::write(&base_path, text_only_base()).unwrap();
        let user_path = tmp.0.join("config.toml");
        fs::write(
            &user_path,
            "\nThemesEnabled = true\nActiveTheme = \"Nord\"\n[Themes.Nord.Accents]\ncyan = \"#88C0D0\"\n",
        )
        .unwrap();
        let (cfg, _) = load_config(2008, &base_path, Some(&user_path), None).unwrap();
        assert_eq!(cfg.themes.active.as_deref(), Some("Nord"));
        assert_eq!(
            cfg.palette.accent(ButtonAccent::Cyan),
            (136.0 / 255.0, 192.0 / 255.0, 208.0 / 255.0),
            "the pin's accent override reaches the render palette"
        );
        assert_eq!(
            cfg.palette.accent(ButtonAccent::Amber),
            (235.0 / 255.0, 203.0 / 255.0, 139.0 / 255.0),
            "shadowing a built-in inherits its unspecified accents"
        );
        assert_eq!(
            cfg.day_cycle.background(14),
            (59.0 / 255.0, 66.0 / 255.0, 82.0 / 255.0),
            "the seed Nord bands paint the strip"
        );
        assert_eq!(
            cfg.global_day_cycle.background(14),
            crate::daycycle::DayCycle::default().background(14),
            "the theme-free cycle stays untouched for re-apply"
        );
        // Without a pin and without a state file the deck starts on the
        // live palette exactly as before themes existed.
        fs::write(&user_path, "\nThemesEnabled = true\n").unwrap();
        let (cfg, _) = load_config(2008, &base_path, Some(&user_path), None).unwrap();
        assert_eq!(cfg.themes.active.as_deref(), Some("Default"));
        assert_eq!(cfg.palette, crate::appearance::Palette::default());
        // A disabled system renders the live palette whatever it says.
        fs::write(
            &user_path,
            "ThemesEnabled = false\nActiveTheme = \"Nord\"\n",
        )
        .unwrap();
        let (cfg, _) = load_config(2008, &base_path, Some(&user_path), None).unwrap();
        assert_eq!(cfg.palette, crate::appearance::Palette::default());
    }

    #[test]
    fn the_theme_state_file_seeds_heals_and_yields_to_the_config_pin() {
        let tmp = TempDir::new();
        let base_path = tmp.0.join("base.toml");
        fs::write(&base_path, text_only_base()).unwrap();
        let user_path = tmp.0.join("config.toml");
        let state_path = tmp.0.join("active-theme");
        fs::write(&user_path, "ThemesEnabled = true\n").unwrap();
        // The last on-glass choice, remembered across restarts, applies
        // without any config pin - and is remembered for the next load.
        fs::write(&state_path, "Ember").unwrap();
        let (cfg, _) = load_config(2008, &base_path, Some(&user_path), Some(&state_path)).unwrap();
        assert_eq!(cfg.themes.active.as_deref(), Some("Ember"));
        // A config pin wins over the state file, and the file heals to
        // match so the two sources never fight.
        fs::write(&user_path, "ThemesEnabled = true\nActiveTheme = \"Nord\"\n").unwrap();
        let (cfg, _) = load_config(2008, &base_path, Some(&user_path), Some(&state_path)).unwrap();
        assert_eq!(cfg.themes.active.as_deref(), Some("Nord"));
        assert_eq!(fs::read_to_string(&state_path).unwrap(), "Nord");
        // A state entry naming a removed theme falls back to the first
        // theme and heals the file, so a stale file cannot wedge the bar.
        fs::write(&user_path, "ThemesEnabled = true\n").unwrap();
        fs::write(&state_path, "Atlantis").unwrap();
        let (cfg, _) = load_config(2008, &base_path, Some(&user_path), Some(&state_path)).unwrap();
        assert_eq!(cfg.themes.active.as_deref(), Some("Default"));
        assert_eq!(fs::read_to_string(&state_path).unwrap(), "Default");
        // save_theme is the on-glass path: atomic, and reload picks it up.
        let manager = ConfigManager::with_paths(base_path, user_path, state_path.clone());
        manager.save_theme("Mono").unwrap();
        let (cfg, _) = manager.load_config(2008);
        assert_eq!(cfg.themes.active.as_deref(), Some("Mono"));
    }

    #[test]
    fn invalid_named_layouts_and_actions_are_rejected() {
        for invalid in [
            NAMED.replace("DefaultLayer = \"home\"", "DefaultLayer = \"missing\""),
            NAMED.replace("FnLayer = \"function\"", "FnLayer = \"missing\""),
            NAMED.replace("Layer = \"function\"", "Layer = \"missing\""),
            NAMED.replace("Name = \"function\"", "Name = \"home\""),
            NAMED.replace("Name = \"function\"", "Name = \"\""),
            NAMED.replace("Keys = [{ Text = \"F1\", Action = \"F1\" }]", "Keys = []"),
            NAMED.replace("DefaultLayer = \"home\"", ""),
            "DefaultLayer = \"home\"\nLayers = []".into(),
            "DefaultLayer = \"home\"".into(),
            NAMED.replace("Layer = \"function\"", "Layer = \"function\", Key = \"F1\""),
            NAMED.replace("Action = \"F1\"", "Action = \"NotAKey\""),
            NAMED.replace("Action = \"F1\"", "Action = { Command = \"\" }"),
            NAMED.replace("Action = \"F1\"", "Action = { Command = \"   \" }"),
            NAMED.replace("Action = \"F1\"", "Action = {}"),
            NAMED.replace("Action = \"F1\"", "Action = { Command = 42 }"),
            NAMED.replace(
                "Action = \"F1\"",
                "Action = { Layer = \"home\", Extra = true }",
            ),
            NAMED.replace(
                "Action = \"F1\"",
                "Action = { Command = \"x\", Layer = \"missing\" }",
            ),
        ] {
            assert!(
                parse_config(BASE, Some(&invalid), 2008).is_err(),
                "accepted invalid configuration: {invalid}"
            );
        }
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "dfr-config-test-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn text_only_base() -> String {
        let mut base: toml::Value = toml::from_str(BASE).unwrap();
        let keys: toml::Value = toml::from_str(
            r#"
PrimaryLayerKeys = [{ Text = "F1", Action = "F1" }]
MediaLayerKeys = [{ Text = "Volume", Action = "VolumeUp" }]
"#,
        )
        .unwrap();
        for (name, value) in keys.as_table().unwrap() {
            base[name] = value.clone();
        }
        toml::to_string(&base).unwrap()
    }

    #[test]
    fn battery_cycle_settings_merge_and_legacy_configs_have_defaults() {
        let tmp = TempDir::new();
        let base_path = tmp.0.join("base.toml");
        let user_path = tmp.0.join("config.toml");
        let mut legacy: toml::Value = toml::from_str(&text_only_base()).unwrap();
        legacy.as_table_mut().unwrap().remove("BatteryCycleEnabled");
        legacy
            .as_table_mut()
            .unwrap()
            .remove("BatteryCycleTimeoutSecs");
        fs::write(&base_path, toml::to_string(&legacy).unwrap()).unwrap();
        let (config, deck) = load_config(2008, &base_path, None, None).unwrap();
        assert!(deck.legacy);
        assert!(config.battery_cycle_enabled);
        assert_eq!(config.battery_cycle_timeout_secs, 15);
        fs::write(
            &user_path,
            "BatteryCycleEnabled = false\nBatteryCycleTimeoutSecs = 7",
        )
        .unwrap();
        let (config, _) = load_config(2008, &base_path, Some(&user_path), None).unwrap();
        assert!(!config.battery_cycle_enabled);
        assert_eq!(config.battery_cycle_timeout_secs, 7);
        for invalid in [
            "BatteryCycleTimeoutSecs = 0",
            "BatteryCycleTimeoutSecs = -1",
            "BatteryCycleTimeoutSecs = '15'",
            "BatteryCycleEnabled = 'false'",
        ] {
            assert!(parse_config(BASE, Some(invalid), 2008).is_err());
        }
    }

    #[test]
    fn battery_escalation_settings_merge_and_legacy_configs_have_defaults() {
        let tmp = TempDir::new();
        let base_path = tmp.0.join("base.toml");
        let user_path = tmp.0.join("config.toml");
        // A base config predating the escalation keeps working with defaults.
        let mut legacy: toml::Value = toml::from_str(&text_only_base()).unwrap();
        for key in [
            "BatteryWarningThreshold",
            "BatteryCriticalThreshold",
            "BatteryWarningColor",
            "BatteryCriticalColor",
        ] {
            legacy.as_table_mut().unwrap().remove(key);
        }
        fs::write(&base_path, toml::to_string(&legacy).unwrap()).unwrap();
        let (config, _) = load_config(2008, &base_path, None, None).unwrap();
        assert_eq!(config.battery_warning_threshold, 25);
        assert_eq!(config.battery_critical_threshold, 10);
        assert_eq!(config.battery_warning_color, ButtonAccent::Orange);
        assert_eq!(config.battery_critical_color, ButtonAccent::Red);
        // User overrides win over the base defaults, keys may be set singly.
        fs::write(
            &user_path,
            "BatteryWarningThreshold = 30\nBatteryCriticalColor = \"violet\"",
        )
        .unwrap();
        let (config, _) = load_config(2008, &base_path, Some(&user_path), None).unwrap();
        assert_eq!(config.battery_warning_threshold, 30);
        assert_eq!(config.battery_critical_threshold, 10);
        assert_eq!(config.battery_warning_color, ButtonAccent::Orange);
        assert_eq!(config.battery_critical_color, ButtonAccent::Violet);
        // Thresholds above 100 can never trigger and inverted tiers only
        // surprise, so both are rejected like the other invalid knobs.
        for invalid in [
            "BatteryWarningThreshold = 101",
            "BatteryCriticalThreshold = 101",
            "BatteryCriticalThreshold = 30\nBatteryWarningThreshold = 25",
            "BatteryCriticalThreshold = 30",
            "BatteryWarningThreshold = '25'",
            "BatteryWarningColor = \"mauve\"",
            "BatteryCriticalColor = 42",
        ] {
            assert!(
                parse_config(BASE, Some(invalid), 2008).is_err(),
                "accepted invalid battery escalation configuration: {invalid}"
            );
        }
    }

    #[test]
    fn pet_settings_merge_with_legacy_defaults_and_reject_invalid_values() {
        let tmp = TempDir::new();
        let base_path = tmp.0.join("base.toml");
        let user_path = tmp.0.join("config.toml");
        // A base config predating the pet keeps working with defaults.
        let mut legacy: toml::Value = toml::from_str(&text_only_base()).unwrap();
        legacy.as_table_mut().unwrap().remove("PetEnabled");
        legacy.as_table_mut().unwrap().remove("PetVariant");
        legacy.as_table_mut().unwrap().remove("PetEnergy");
        fs::write(&base_path, toml::to_string(&legacy).unwrap()).unwrap();
        let (config, _) = load_config(2008, &base_path, None, None).unwrap();
        assert!(config.pet_enabled);
        assert_eq!(config.pet_variant, PetVariant::Cat);
        assert_eq!(config.pet_energy, PetEnergy::Calm);
        // User overrides win over the base defaults.
        fs::write(
            &user_path,
            "PetEnabled = false\nPetVariant = \"fox\"\nPetEnergy = \"rowdy\"",
        )
        .unwrap();
        let (config, _) = load_config(2008, &base_path, Some(&user_path), None).unwrap();
        assert!(!config.pet_enabled);
        assert_eq!(config.pet_variant, PetVariant::Fox);
        assert_eq!(config.pet_energy, PetEnergy::Rowdy);
        // Invalid values are rejected like the other knobs.
        for invalid in [
            "PetVariant = \"dog\"",
            "PetEnergy = \"zoomy\"",
            "PetEnabled = 'true'",
            "PetVariant = 42",
        ] {
            assert!(
                parse_config(BASE, Some(invalid), 2008).is_err(),
                "accepted invalid pet configuration: {invalid}"
            );
        }
        // The shipped base config carries valid pet defaults.
        let (config, _) = parse_config(BASE, None, 2008).unwrap();
        assert!(config.pet_enabled.is_some());
        assert_eq!(config.pet_variant.as_deref(), Some("cat"));
        assert_eq!(config.pet_energy.as_deref(), Some("calm"));
    }

    #[test]
    fn day_cycle_settings_merge_with_defaults_overrides_and_reject_invalid_values() {
        let tmp = TempDir::new();
        let base_path = tmp.0.join("base.toml");
        let user_path = tmp.0.join("config.toml");
        // A base config predating the day cycle keeps working with defaults.
        let mut legacy: toml::Value = toml::from_str(&text_only_base()).unwrap();
        legacy.as_table_mut().unwrap().remove("DayCycleEnabled");
        legacy.as_table_mut().unwrap().remove("DayCycleBands");
        fs::write(&base_path, toml::to_string(&legacy).unwrap()).unwrap();
        let (config, _) = load_config(2008, &base_path, None, None).unwrap();
        assert!(config.day_cycle.enabled);
        assert_eq!(config.day_cycle.band_at(14).unwrap().name, "day");
        assert_eq!(
            config.day_cycle.background(2),
            (4.0 / 255.0, 6.0 / 255.0, 13.0 / 255.0),
            "the small hours wrap around midnight to the night band"
        );
        // Disabling keeps the plain static black background at every hour.
        fs::write(&user_path, "DayCycleEnabled = false").unwrap();
        let (config, _) = load_config(2008, &base_path, Some(&user_path), None).unwrap();
        for hour in 0..=23 {
            assert_eq!(config.day_cycle.band_at(hour), None);
            assert_eq!(config.day_cycle.background(hour), (0.0, 0.0, 0.0));
        }
        // Custom bands replace the defaults as a whole and override the color
        // literals case-insensitively, with or without the leading hash.
        fs::write(
            &user_path,
            r##"
[[DayCycleBands]]
Name = "work"
StartHour = 9
Color = "#102030"
[[DayCycleBands]]
Name = "off"
StartHour = 19
Color = "301020"
"##,
        )
        .unwrap();
        let (config, _) = load_config(2008, &base_path, Some(&user_path), None).unwrap();
        let off = config.day_cycle.band_at(2).unwrap();
        assert_eq!((off.name.as_str(), off.start_hour), ("off", 19));
        assert_eq!(off.rgb(), (48.0 / 255.0, 16.0 / 255.0, 32.0 / 255.0));
        let work = config.day_cycle.band_at(18).unwrap();
        assert_eq!(work.name, "work");
        assert_eq!(work.rgb(), (16.0 / 255.0, 32.0 / 255.0, 48.0 / 255.0));
        // Invalid values reject the candidate configuration like the other knobs.
        let band = |name: &str, hour: &str, color: &str| {
            format!(
                "DayCycleEnabled = true\n[[DayCycleBands]]\nName = {name}\nStartHour = {hour}\nColor = {color}\n"
            )
        };
        for invalid in [
            "DayCycleBands = []".to_owned(),
            "DayCycleEnabled = 'true'".to_owned(),
            "DayCycleEnabled = 1".to_owned(),
            band("", "5", "\"#101010\""),
            band("  ", "5", "\"#101010\""),
            band("same", "5", "\"#101010\"") + &band("same", "7", "\"#202020\""),
            band("early", "5", "\"#101010\"") + &band("late", "5", "\"#202020\""),
            band("night", "24", "\"#101010\""),
            band("night", "-1", "\"#101010\""),
            band("night", "5", "\"10101\""),
            band("night", "5", "\"#10g010\""),
            band("night", "5", "1010103"),
            band("night", "5", ""),
            // A missing Color, and an unknown field inside the band table.
            "DayCycleEnabled = true\n[[DayCycleBands]]\nName = \"night\"\nStartHour = 5\n"
                .to_owned(),
            band("night", "5", "\"#101010\"") + "Extra = 1\n",
        ] {
            assert!(
                parse_config(BASE, Some(&invalid), 2008).is_err(),
                "accepted invalid day-cycle configuration: {invalid}"
            );
        }
        // The shipped base config carries the shipped default enablement.
        let (base_cfg, _) = parse_config(BASE, None, 2008).unwrap();
        assert!(base_cfg.day_cycle_enabled.unwrap());
    }

    #[test]
    fn watcher_handles_direct_writes_atomic_saves_rename_away_and_deletion() {
        let tmp = TempDir::new();
        let path = tmp.0.join("config.toml");
        let mut manager = ConfigManager::with_paths(
            tmp.0.join("base.toml"),
            path.clone(),
            tmp.0.join("active-theme"),
        );
        assert!(!manager.pending_reload());
        fs::write(tmp.0.join("unrelated"), "ignored").unwrap();
        assert!(!manager.pending_reload());
        fs::write(&path, NAMED).unwrap();
        assert!(manager.pending_reload());
        assert!(!manager.pending_reload());
        for _ in 0..2 {
            let stage = tmp.0.join("config.new");
            fs::write(&stage, NAMED).unwrap();
            fs::rename(stage, &path).unwrap();
            assert!(manager.pending_reload());
            assert!(!manager.pending_reload());
        }
        let backup = tmp.0.join("config.backup");
        fs::rename(&path, &backup).unwrap();
        assert!(manager.pending_reload());
        assert!(!manager.pending_reload());
        fs::rename(backup, &path).unwrap();
        assert!(manager.pending_reload());
        fs::remove_file(path).unwrap();
        assert!(manager.pending_reload());
        assert!(!manager.pending_reload());
    }

    #[test]
    fn symlinked_config_reloads_in_place_writes_and_target_replacements() {
        let tmp = TempDir::new();
        let base = tmp.0.join("base.toml");
        fs::write(&base, text_only_base()).unwrap();
        let dotfiles = tmp.0.join("dotfiles");
        fs::create_dir(&dotfiles).unwrap();
        let target = dotfiles.join("config.toml");
        fs::write(&target, NAMED).unwrap();
        let path = tmp.0.join("config.toml");
        symlink(&target, &path).unwrap();

        let mut manager = ConfigManager::with_paths(base, path.clone(), tmp.0.join("active-theme"));
        let (_, mut deck) = manager.load_config(2008);
        assert!(!deck.legacy);
        assert_eq!(deck.layers.len(), 4);
        let mut events = Vec::new();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.switch_named("function", &mut emit);
        deck.touch_down(0, 1, 1500.0, 30.0, &mut emit, &mut |_| {}, &mut |_| None);
        drop(emit);
        assert_eq!(events, vec![(vec![Key::F1], true)]);

        fs::write(&path, NAMED.replace("F1", "F5")).unwrap();
        let (_, replacement) = manager.update_config(2008).unwrap();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.replace(replacement, &mut emit);
        drop(emit);
        assert_eq!(events, vec![(vec![Key::F1], true), (vec![Key::F1], false)]);
        events.clear();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.switch_named("function", &mut emit);
        deck.touch_down(0, 1, 1500.0, 30.0, &mut emit, &mut |_| {}, &mut |_| None);
        drop(emit);
        assert_eq!(events, vec![(vec![Key::F5], true)]);

        let stage = dotfiles.join("config.new");
        fs::write(&stage, NAMED.replace("F1", "F6")).unwrap();
        fs::rename(&stage, &target).unwrap();
        let (_, replacement) = manager.update_config(2008).unwrap();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.replace(replacement, &mut emit);
        drop(emit);
        assert_eq!(events, vec![(vec![Key::F5], true), (vec![Key::F5], false)]);
        events.clear();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.switch_named("function", &mut emit);
        deck.touch_down(0, 1, 1500.0, 30.0, &mut emit, &mut |_| {}, &mut |_| None);
        drop(emit);
        assert_eq!(events, vec![(vec![Key::F6], true)]);
    }

    #[test]
    fn invalid_reload_keeps_current_deck_and_next_valid_save_recovers() {
        let tmp = TempDir::new();
        let base = tmp.0.join("base.toml");
        fs::write(&base, text_only_base()).unwrap();
        let path = tmp.0.join("config.toml");
        fs::write(&path, NAMED).unwrap();
        let mut manager = ConfigManager::with_paths(base, path.clone(), tmp.0.join("active-theme"));
        let (_, mut deck) = manager.load_config(2008);
        deck.switch_named("function", &mut |_, _| {});
        let mut keys = Vec::new();
        let mut emit = |codes: &[Key], active| keys.push((codes.to_vec(), active));
        deck.touch_down(7, 1, 1500.0, 30.0, &mut emit, &mut |_| {}, &mut |_| None);
        fs::write(
            &path,
            NAMED.replace("Layer = \"function\"", "Layer = \"missing\""),
        )
        .unwrap();
        assert!(manager.update_config(2008).is_none());
        assert_eq!(deck.active, 1);
        assert!(deck.current().buttons[1].1.active);
        fs::write(&path, NAMED).unwrap();
        let (_, replacement) = manager.update_config(2008).unwrap();
        deck.replace(replacement, &mut emit);
        assert_eq!(deck.active, deck.home);
        assert!(deck.touches.is_empty());
        assert_eq!(keys, vec![(vec![Key::F1], true), (vec![Key::F1], false)]);
    }

    #[test]
    fn invalid_startup_and_deleted_user_config_use_the_base_layers() {
        let tmp = TempDir::new();
        let base = tmp.0.join("base.toml");
        fs::write(&base, text_only_base()).unwrap();
        let path = tmp.0.join("config.toml");
        fs::write(
            &path,
            NAMED.replace("Layer = \"function\"", "Layer = \"missing\""),
        )
        .unwrap();
        let mut manager = ConfigManager::with_paths(base, path.clone(), tmp.0.join("active-theme"));
        let (_, mut deck) = manager.load_config(2008);
        assert!(deck.legacy);
        assert_eq!(deck.active, 0);
        assert_eq!(deck.layers.len(), 4);

        fs::write(&path, NAMED).unwrap();
        let (_, replacement) = manager.update_config(2008).unwrap();
        let mut events = Vec::new();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.replace(replacement, &mut emit);
        assert!(!deck.legacy);
        deck.switch_named("function", &mut emit);
        deck.touch_down(0, 1, 1500.0, 30.0, &mut emit, &mut |_| {}, &mut |_| None);

        fs::remove_file(path).unwrap();
        let (_, replacement) = manager.update_config(2008).unwrap();
        deck.replace(replacement, &mut emit);
        assert!(deck.legacy);
        assert_eq!(deck.active, deck.home);
        assert_eq!(deck.layers.len(), 4);
        assert!(deck.touches.is_empty());
        deck.touch_up(0, &mut emit);
        assert_eq!(events, vec![(vec![Key::F1], true), (vec![Key::F1], false)]);
    }

    #[test]
    fn base_without_double_press_loads_with_the_feature_disabled() {
        // Distro-installed base configs predate DoublePressSwitchLayers and the
        // shipped multibar example does not set it either (README install path).
        let mut installed: toml::Value = toml::from_str(BASE).unwrap();
        assert!(installed
            .as_table_mut()
            .unwrap()
            .remove("DoublePressSwitchLayers")
            .is_some());
        let tmp = TempDir::new();
        let base_path = tmp.0.join("base.toml");
        fs::write(&base_path, toml::to_string(&installed).unwrap()).unwrap();
        let user_path = tmp.0.join("config.toml");
        fs::write(&user_path, include_str!("../share/tiny-dfr/multibar.toml")).unwrap();
        let manager = ConfigManager::with_paths(base_path, user_path, tmp.0.join("active-theme"));
        let (cfg, deck) = manager.load_config(2008);
        assert_eq!(cfg.double_press_switch_layers, 0);
        assert!(!deck.legacy);
        // The multibar's media bar carries the Viz button, so the deck
        // also synthesized its full-bar visualizer view; the named layers
        // are home, function, media, recording, more, and pet, plus the
        // synthesized themes picker.
        assert_eq!(deck.layers.len(), 8);
        assert!(deck.names.contains_key("visualizer:media"));
    }

    #[test]
    fn pet_enabled_synthesizes_a_pet_layer_and_home_button() {
        for width in [2008, 2170] {
            let (_, layout) = parse_config(BASE, Some(NAMED), width).unwrap();
            let pet = layout
                .layers
                .iter()
                .find(|layer| layer.name == "pet")
                .expect("the pet layer is synthesized");
            let offset = usize::from(width >= 2170);
            let home_button = &pet.keys[offset];
            assert_eq!(home_button.text.as_deref(), Some("Home"));
            assert_eq!(home_button.action, ButtonAction::SwitchLayer("home".into()));
            let ground = &pet.keys[offset + 1];
            assert_eq!(ground.stretch, Some(PET_LAYER_STRETCH));
            assert!(ground.text.is_none() && ground.icon.is_none());
            // The home bar gains a Pet button right after its Functions nav.
            let home = &layout.layers[layout.home];
            let functions = home
                .keys
                .iter()
                .position(|key| key.action == ButtonAction::SwitchLayer("function".into()))
                .unwrap();
            let pet_button = &home.keys[functions + 1];
            assert_eq!(pet_button.text.as_deref(), Some("Pet"));
            assert_eq!(pet_button.action, ButtonAction::SwitchLayer("pet".into()));
        }
    }

    #[test]
    fn pet_disabled_restores_the_pre_pet_home_bar_and_layer_list() {
        let config = format!("PetEnabled = false\n{NAMED}");
        let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
        assert_eq!(layout.layers.len(), 3);
        assert!(layout.layers.iter().all(|layer| layer.name != "pet"));
        let home = &layout.layers[layout.home];
        assert_eq!(home.keys.len(), 1);
        assert_eq!(home.keys[0].text.as_deref(), Some("Functions"));
        assert!(!home
            .keys
            .iter()
            .any(|key| key.text.as_deref() == Some("Pet")));
        // The disabled pet is also absent from legacy layouts.
        let (_, legacy) = parse_config(
            &text_only_base().replace("PetEnabled = true", "PetEnabled = false"),
            None,
            2008,
        )
        .unwrap();
        assert_eq!(legacy.layers.len(), 3);
        assert!(legacy.layers.iter().all(|layer| layer.name != "pet"));
    }

    #[test]
    fn a_user_pet_layer_replaces_the_synthesized_playground() {
        let config = format!(
            "{NAMED}\n\
[[Layers]]\n\
Name = \"pet\"\n\
Keys = [{{ Text = \"P1\" }}]\n"
        );
        let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
        assert_eq!(layout.layers.len(), 4);
        let pet = &layout.layers[2];
        assert_eq!(pet.name, "pet");
        // The user's buttons are kept and gain only the generic Home button,
        // never the synthesized playground spacer.
        assert_eq!(pet.keys.len(), 2);
        assert_eq!(pet.keys[0].text.as_deref(), Some("Home"));
        assert_eq!(pet.keys[1].text.as_deref(), Some("P1"));
        // The home bar still gets its Pet button, now targeting the user's
        // layer; an author-provided visible button would replace it.
        let home = &layout.layers[layout.home];
        assert!(home
            .keys
            .iter()
            .any(|key| key.action == ButtonAction::SwitchLayer("pet".into())));
        let with_own_button = config.replace(
            "{ Text = \"Functions\", Action = { Layer = \"function\" } }",
            "{ Text = \"Functions\", Action = { Layer = \"function\" } }, { Text = \"Kitty\", Action = { Layer = \"pet\" } }",
        );
        let (_, layout) = parse_config(BASE, Some(&with_own_button), 2008).unwrap();
        let home = &layout.layers[layout.home];
        assert_eq!(
            home.keys
                .iter()
                .filter(|key| key.action == ButtonAction::SwitchLayer("pet".into()))
                .count(),
            1
        );
        assert!(home
            .keys
            .iter()
            .any(|key| key.text.as_deref() == Some("Kitty")));
        assert!(!home
            .keys
            .iter()
            .any(|key| key.text.as_deref() == Some("Pet")));
    }

    #[test]
    fn disabling_the_pet_rejects_stale_references_to_the_pet_layer() {
        // With the pet disabled the synthesized layer no longer exists, so a
        // button still naming it is an unknown target like any other.
        let config = format!("PetEnabled = false\n{NAMED}").replace(
            "{ Text = \"Functions\", Action = { Layer = \"function\" } }",
            "{ Text = \"Functions\", Action = { Layer = \"function\" } }, { Text = \"Pet\", Action = { Layer = \"pet\" } }",
        );
        assert!(parse_config(BASE, Some(&config), 2008).is_err());
    }
}
