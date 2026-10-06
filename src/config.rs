use crate::fonts::{FontConfig, Pattern};
use crate::{FunctionLayer, LayerDeck};
use anyhow::{bail, Context, Result};
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

pub struct Config {
    pub show_button_outlines: bool,
    pub enable_pixel_shift: bool,
    pub font_face: FontFace,
    pub adaptive_brightness: bool,
    pub active_brightness: u32,
    pub double_press_switch_layers: u32,
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
    primary_layer_keys: Option<Vec<ButtonConfig>>,
    media_layer_keys: Option<Vec<ButtonConfig>>,
    layers: Option<Vec<LayerConfig>>,
    default_layer: Option<String>,
    fn_layer: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ButtonAction {
    Keys(Vec<Key>),
    SwitchLayer(String),
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
            Key(Key),
            Keys(Vec<Key>),
            Layer(LayerAction),
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase", deny_unknown_fields)]
        struct LayerAction {
            layer: String,
        }
        Ok(match Action::deserialize(deserializer)? {
            Action::Key(key) => Self::Keys(vec![key]),
            Action::Keys(keys) => Self::Keys(keys),
            Action::Layer(action) => Self::SwitchLayer(action.layer),
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
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct LayerConfig {
    pub name: String,
    pub keys: Vec<ButtonConfig>,
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
            primary_layer_keys,
            media_layer_keys,
            layers,
            default_layer,
            fn_layer,
        );
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
            if layer.name.trim().is_empty() || !names.insert(layer.name.as_str()) {
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
        for layer in &layers {
            for button in &layer.keys {
                if let ButtonAction::SwitchLayer(target) = &button.action {
                    if !names.contains(target.as_str()) {
                        bail!(
                            "layer {:?} has an unknown button target {target:?}",
                            layer.name
                        );
                    }
                }
            }
        }
        // Every named sub-bar remains navigable even when its author omits Home.
        let home_name = home_name.to_owned();
        for (index, layer) in layers.iter_mut().enumerate() {
            if !legacy
                && index != home
                && !layer.keys.iter().any(|button| {
                    button.action == ButtonAction::SwitchLayer(home_name.clone())
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
    let layers = LayerDeck::new(
        layout
            .layers
            .into_iter()
            .map(|layer| (layer.name, FunctionLayer::with_config(layer.keys)))
            .collect(),
        layout.home,
        layout.fn_layer,
        layout.legacy,
    );
    let cfg = Config {
        show_button_outlines: base.show_button_outlines.unwrap(),
        enable_pixel_shift: base.enable_pixel_shift.unwrap(),
        adaptive_brightness: base.adaptive_brightness.unwrap(),
        font_face: load_font(&base.font_template.unwrap()),
        active_brightness: base.active_brightness.unwrap(),
        double_press_switch_layers: base.double_press_switch_layers.unwrap(),
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
    let flags = AddWatchFlags::IN_CLOSE_WRITE
        | AddWatchFlags::IN_MOVE_SELF
        | AddWatchFlags::IN_DELETE_SELF;
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
                if evt.mask
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
            assert_eq!(layout.layers.len(), 2);
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
        assert_eq!(stock.layers[0].keys.len(), 12);
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
        assert!(legacy.layers.iter().all(|layer| layer.keys.len() == 13));
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
        assert_eq!(layout.layers[0].keys.len(), 1);
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
        assert_eq!(layout.layers.len(), 3);
        let home = &layout.layers[layout.home];
        assert!(home.keys.iter().any(|key| key.time.is_some()));
        assert!(home.keys.iter().any(|key| key.battery.is_some()));
        for name in ["function", "media"] {
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
        assert_eq!(deck.layers.len(), 2);
        let mut events = Vec::new();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.switch_named("function", &mut emit);
        deck.touch_down(0, 1, &mut emit);
        assert_eq!(events, vec![(vec![Key::F1], true)]);

        fs::write(&path, NAMED.replace("F1", "F5")).unwrap();
        let (_, replacement) = manager.update_config(2008).unwrap();
        deck.replace(replacement, &mut emit);
        assert_eq!(events, vec![(vec![Key::F1], true), (vec![Key::F1], false)]);
        events.clear();
        deck.switch_named("function", &mut emit);
        deck.touch_down(0, 1, &mut emit);
        assert_eq!(events, vec![(vec![Key::F5], true)]);

        let stage = dotfiles.join("config.new");
        fs::write(&stage, NAMED.replace("F1", "F6")).unwrap();
        fs::rename(&stage, &target).unwrap();
        let (_, replacement) = manager.update_config(2008).unwrap();
        deck.replace(replacement, &mut emit);
        assert_eq!(events, vec![(vec![Key::F5], true), (vec![Key::F5], false)]);
        events.clear();
        deck.switch_named("function", &mut emit);
        deck.touch_down(0, 1, &mut emit);
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
        deck.touch_down(7, 1, &mut emit);
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
        assert_eq!(deck.layers.len(), 2);

        fs::write(&path, NAMED).unwrap();
        let (_, replacement) = manager.update_config(2008).unwrap();
        let mut events = Vec::new();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.replace(replacement, &mut emit);
        assert!(!deck.legacy);
        deck.switch_named("function", &mut emit);
        deck.touch_down(0, 1, &mut emit);

        fs::remove_file(path).unwrap();
        let (_, replacement) = manager.update_config(2008).unwrap();
        deck.replace(replacement, &mut emit);
        assert!(deck.legacy);
        assert_eq!(deck.active, deck.home);
        assert_eq!(deck.layers.len(), 2);
        assert!(deck.touches.is_empty());
        deck.touch_up(0, &mut emit);
        assert_eq!(events, vec![(vec![Key::F1], true), (vec![Key::F1], false)]);
    }
}
