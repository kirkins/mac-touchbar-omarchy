use crate::appearance::ButtonAccent;
use crate::daycycle::{self, DayCycle, TimeBand};
use crate::fonts::{FontConfig, Pattern};
use crate::pet::{PetEnergy, PetVariant};
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
    collections::HashSet,
    fs::read_to_string,
    os::fd::AsFd,
    path::{Path, PathBuf},
};

const USER_CFG_PATH: &str = "/etc/tiny-dfr/config.toml";
const BASE_CFG_PATH: &str = "/usr/share/tiny-dfr/config.toml";

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
    pub pet_enabled: bool,
    pub pet_variant: PetVariant,
    pub pet_energy: PetEnergy,
    pub day_cycle: DayCycle,
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
    pet_enabled: Option<bool>,
    pet_variant: Option<String>,
    pet_energy: Option<String>,
    day_cycle_enabled: Option<bool>,
    day_cycle_bands: Option<Vec<BandConfig>>,
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
}

impl ButtonAction {
    /// The layer this action switches to on touch-down, if any.
    pub fn layer_target(&self) -> Option<&str> {
        match self {
            Self::SwitchLayer(layer) | Self::CommandLayer { layer, .. } => Some(layer),
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
            Slider(SliderAction),
            Key(Key),
            Keys(Vec<Key>),
            Table(TableAction),
        }
        #[derive(Deserialize)]
        enum SliderAction {
            VolumeSlider,
            BrightnessSlider,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase", deny_unknown_fields)]
        struct TableAction {
            layer: Option<String>,
            command: Option<String>,
        }
        Ok(match Action::deserialize(deserializer)? {
            Action::Slider(SliderAction::VolumeSlider) => Self::VolumeSlider,
            Action::Slider(SliderAction::BrightnessSlider) => Self::BrightnessSlider,
            Action::Key(key) => Self::Keys(vec![key]),
            Action::Keys(keys) => Self::Keys(keys),
            Action::Table(action) => match (action.layer, action.command) {
                (Some(layer), None) => Self::SwitchLayer(layer),
                (None, Some(command)) => Self::Command(command),
                (Some(layer), Some(command)) => Self::CommandLayer { command, layer },
                (None, None) => {
                    return Err(serde::de::Error::custom(
                        "an Action table needs Layer or Command",
                    ))
                }
            },
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
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct BandConfig {
    pub name: String,
    pub start_hour: u8,
    pub color: String,
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
            pet_enabled,
            pet_variant,
            pet_energy,
            day_cycle_enabled,
            day_cycle_bands,
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
            Some(bands) if bands.is_empty() => {
                bail!("DayCycleBands must define at least one band")
            }
            Some(bands) => {
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
                parsed
            }
        };
        Ok(DayCycle::new(enabled, bands))
    }

    fn layer_layout(&mut self, width: u16) -> Result<LayerLayout> {
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
    let layers = base.layer_layout(width)?;
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
) -> Result<(Config, LayerDeck)> {
    let user = user_path.map(read_to_string).transpose();
    let user = match user {
        Ok(user) => user,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err.into()),
    };
    let (base, layout) = parse_config(&read_to_string(base_path)?, user.as_deref(), width)?;
    let day_cycle = base.day_cycle()?;
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
    let cfg = Config {
        show_button_outlines: base.show_button_outlines.unwrap(),
        enable_pixel_shift: base.enable_pixel_shift.unwrap(),
        adaptive_brightness: base.adaptive_brightness.unwrap(),
        font_face: load_font(&base.font_template.unwrap()),
        active_brightness: base.active_brightness.unwrap(),
        double_press_switch_layers: base.double_press_switch_layers.unwrap_or(0),
        battery_cycle_enabled: base.battery_cycle_enabled.unwrap_or(true),
        battery_cycle_timeout_secs: base.battery_cycle_timeout_secs.unwrap_or(15),
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
        day_cycle,
    };
    Ok((cfg, layers))
}

pub struct ConfigManager {
    inotify_fd: Inotify,
    dir_watch: Option<WatchDescriptor>,
    file_watch: Option<WatchDescriptor>,
    user_path: PathBuf,
    base_path: PathBuf,
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
        Self::with_paths(PathBuf::from(BASE_CFG_PATH), PathBuf::from(USER_CFG_PATH))
    }
    fn with_paths(base_path: PathBuf, user_path: PathBuf) -> ConfigManager {
        let inotify_fd = Inotify::init(InitFlags::IN_NONBLOCK).unwrap();
        let dir_watch = arm_dir_watch(&inotify_fd, &user_path);
        let file_watch = arm_file_watch(&inotify_fd, &user_path);
        ConfigManager {
            inotify_fd,
            dir_watch,
            file_watch,
            user_path,
            base_path,
        }
    }
    pub fn load_config(&self, width: u16) -> (Config, LayerDeck) {
        load_config(width, &self.base_path, Some(&self.user_path)).unwrap_or_else(|err| {
            eprintln!("Ignoring user configuration: {err:#}");
            load_config(width, &self.base_path, None).expect("failed to load base configuration")
        })
    }
    pub fn update_config(&mut self, width: u16) -> Option<(Config, LayerDeck)> {
        if !self.pending_reload() {
            return None;
        }
        match load_config(width, &self.base_path, Some(&self.user_path)) {
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

    #[test]
    fn button_accents_are_optional_and_invalid_values_reject_the_candidate() {
        let (_, old) = parse_config(BASE, Some(NAMED), 2008).unwrap();
        assert_eq!(old.layers[0].keys[0].accent, None);
        for (name, accent) in [
            ("neutral", ButtonAccent::Neutral),
            ("cyan", ButtonAccent::Cyan),
            ("amber", ButtonAccent::Amber),
            ("violet", ButtonAccent::Violet),
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
        let (cfg, mut deck) = load_config(2008, &base_path, Some(&user_path)).unwrap();

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
            deck.touch_down_at(0, x, 30.0, 2008, 60, &mut emit, &mut |command| {
                commands.push(command.into())
            });
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
        deck.switch_to(deck.home, &mut |_, _| {});
        let spans = deck.current().layout_spans(2008, 0);
        let x = (spans[0].left + spans[0].right) / 2.0;
        let mut emit = |_: &[Key], _: bool| panic!("slider emitted keys");
        deck.touch_down_at(0, x, 30.0, 2008, 60, &mut emit, &mut |_| {});
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
        deck.touch_down_at(0, brightness_x, 30.0, 2008, 60, &mut emit, &mut |_| {});
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
        render(&mut deck, "implemented-recording.png");
        tap(&mut deck, "Stop", &mut commands);
        assert_eq!(deck.active, deck.home);
        tap(&mut deck, "Mic", &mut commands);
        tap(&mut deck, "Cancel", &mut commands);
        assert_eq!(deck.active, deck.home);
        assert_eq!(commands.len(), 4);
        let actions: Vec<_> = commands
            .iter()
            .map(|command| command.split_whitespace().last().unwrap())
            .collect();
        assert_eq!(actions, ["start", "stop", "start", "cancel"]);
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
            assert_eq!(layout.layers.len(), 3);
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
        assert_eq!(layout.layers.len(), 5);
        let home = &layout.layers[layout.home];
        assert!(home.keys.iter().any(|key| key.time.is_some()));
        assert!(home.keys.iter().any(|key| key.battery.is_some()));
        for name in ["function", "media", "pet"] {
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
        // The Pet button joins the navigation cluster after Functions and
        // Media, matching their styling.
        let positions: Vec<usize> = ["function", "media", "pet"]
            .into_iter()
            .map(|name| {
                home.keys
                    .iter()
                    .position(|key| key.action == ButtonAction::SwitchLayer(name.into()))
                    .unwrap()
            })
            .collect();
        assert_eq!(positions, vec![3, 4, 5]);
        let pet_button = &home.keys[positions[2]];
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
        let (config, deck) = load_config(2008, &base_path, None).unwrap();
        assert!(deck.legacy);
        assert!(config.battery_cycle_enabled);
        assert_eq!(config.battery_cycle_timeout_secs, 15);
        fs::write(
            &user_path,
            "BatteryCycleEnabled = false\nBatteryCycleTimeoutSecs = 7",
        )
        .unwrap();
        let (config, _) = load_config(2008, &base_path, Some(&user_path)).unwrap();
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
        let (config, _) = load_config(2008, &base_path, None).unwrap();
        assert!(config.pet_enabled);
        assert_eq!(config.pet_variant, PetVariant::Cat);
        assert_eq!(config.pet_energy, PetEnergy::Calm);
        // User overrides win over the base defaults.
        fs::write(
            &user_path,
            "PetEnabled = false\nPetVariant = \"fox\"\nPetEnergy = \"rowdy\"",
        )
        .unwrap();
        let (config, _) = load_config(2008, &base_path, Some(&user_path)).unwrap();
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
        let (config, _) = load_config(2008, &base_path, None).unwrap();
        assert!(config.day_cycle.enabled);
        assert_eq!(config.day_cycle.band_at(14).unwrap().name, "day");
        assert_eq!(
            config.day_cycle.background(2),
            (4.0 / 255.0, 6.0 / 255.0, 13.0 / 255.0),
            "the small hours wrap around midnight to the night band"
        );
        // Disabling keeps the plain static black background at every hour.
        fs::write(&user_path, "DayCycleEnabled = false").unwrap();
        let (config, _) = load_config(2008, &base_path, Some(&user_path)).unwrap();
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
        let (config, _) = load_config(2008, &base_path, Some(&user_path)).unwrap();
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
        let mut manager = ConfigManager::with_paths(tmp.0.join("base.toml"), path.clone());
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

        let mut manager = ConfigManager::with_paths(base, path.clone());
        let (_, mut deck) = manager.load_config(2008);
        assert!(!deck.legacy);
        assert_eq!(deck.layers.len(), 3);
        let mut events = Vec::new();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.switch_named("function", &mut emit);
        deck.touch_down(0, 1, 1500.0, 30.0, &mut emit, &mut |_| {});
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
        deck.touch_down(0, 1, 1500.0, 30.0, &mut emit, &mut |_| {});
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
        deck.touch_down(0, 1, 1500.0, 30.0, &mut emit, &mut |_| {});
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
        let mut manager = ConfigManager::with_paths(base, path.clone());
        let (_, mut deck) = manager.load_config(2008);
        deck.switch_named("function", &mut |_, _| {});
        let mut keys = Vec::new();
        let mut emit = |codes: &[Key], active| keys.push((codes.to_vec(), active));
        deck.touch_down(7, 1, 1500.0, 30.0, &mut emit, &mut |_| {});
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
        let mut manager = ConfigManager::with_paths(base, path.clone());
        let (_, mut deck) = manager.load_config(2008);
        assert!(deck.legacy);
        assert_eq!(deck.active, 0);
        assert_eq!(deck.layers.len(), 3);

        fs::write(&path, NAMED).unwrap();
        let (_, replacement) = manager.update_config(2008).unwrap();
        let mut events = Vec::new();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.replace(replacement, &mut emit);
        assert!(!deck.legacy);
        deck.switch_named("function", &mut emit);
        deck.touch_down(0, 1, 1500.0, 30.0, &mut emit, &mut |_| {});

        fs::remove_file(path).unwrap();
        let (_, replacement) = manager.update_config(2008).unwrap();
        deck.replace(replacement, &mut emit);
        assert!(deck.legacy);
        assert_eq!(deck.active, deck.home);
        assert_eq!(deck.layers.len(), 3);
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
        let manager = ConfigManager::with_paths(base_path, user_path);
        let (cfg, deck) = manager.load_config(2008);
        assert_eq!(cfg.double_press_switch_layers, 0);
        assert!(!deck.legacy);
        assert_eq!(deck.layers.len(), 5);
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
        assert_eq!(layout.layers.len(), 2);
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
        assert_eq!(legacy.layers.len(), 2);
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
        assert_eq!(layout.layers.len(), 3);
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
