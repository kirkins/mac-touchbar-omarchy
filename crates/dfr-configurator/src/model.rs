//! The configurator's headless document model: everything the UI edits
//! lives here as typed state, every change is validated through the
//! exact `dfr-config` path the daemon loads through, and saving means
//! rendering the canonical user file and installing it atomically so
//! the daemon's inotify reload applies it live. No GTK types appear in
//! this module, so the whole editing contract is testable headless.
//!
//! The editing state is the **user overlay** (`ConfigProxy`): the app
//! loads the shipped base file (a sample, never load-bearing), overlays
//! the user file on top exactly as the daemon does, and edits only the
//! overlay. A knob the user never touches stays absent from the saved
//! file, so base-file updates keep flowing through.

use anyhow::{bail, Context as _, Result};
use dfr_config::{
    parse_config, writer::to_user_toml, BandConfig, ButtonAction, ButtonConfig, ConfigProxy,
    IconSearch, LayerConfig, ThemeConfig, MAX_THEMES,
};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Which content kind a button carries, mirroring the priority order
/// the daemon's `Button::with_config` builds with (text, then icon,
/// then the live widgets).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    Spacer,
    Text,
    Icon,
    Time,
    Battery,
    Waveform,
    Visualizer,
    NowPlaying,
}

impl ButtonKind {
    pub const ALL: &[ButtonKind] = &[
        ButtonKind::Text,
        ButtonKind::Icon,
        ButtonKind::Time,
        ButtonKind::Battery,
        ButtonKind::Waveform,
        ButtonKind::Visualizer,
        ButtonKind::NowPlaying,
        ButtonKind::Spacer,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ButtonKind::Text => "Text",
            ButtonKind::Icon => "Icon",
            ButtonKind::Time => "Clock",
            ButtonKind::Battery => "Battery",
            ButtonKind::Waveform => "Waveform",
            ButtonKind::Visualizer => "Visualizer",
            ButtonKind::NowPlaying => "Now playing",
            ButtonKind::Spacer => "Spacer",
        }
    }
}

/// The action grammar's editable forms, one per wire shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionForm {
    None,
    Keys(Vec<input_linux::Key>),
    SwitchLayer(String),
    Command(String),
    CommandLayer { command: String, layer: String },
    ApplyTheme(String),
    ApplyThemeLayer { theme: String, layer: String },
    VolumeSlider,
    BrightnessSlider,
    CycleTheme,
}

impl ActionForm {
    /// Decompose a wire action into the editor's form.
    pub fn of(action: &ButtonAction) -> Self {
        match action {
            ButtonAction::Keys(keys) if keys.is_empty() => Self::None,
            ButtonAction::Keys(keys) => Self::Keys(keys.clone()),
            ButtonAction::SwitchLayer(layer) => Self::SwitchLayer(layer.clone()),
            ButtonAction::Command(command) => Self::Command(command.clone()),
            ButtonAction::CommandLayer { command, layer } => Self::CommandLayer {
                command: command.clone(),
                layer: layer.clone(),
            },
            ButtonAction::ApplyTheme(theme) => Self::ApplyTheme(theme.clone()),
            ButtonAction::ApplyThemeLayer { theme, layer } => Self::ApplyThemeLayer {
                theme: theme.clone(),
                layer: layer.clone(),
            },
            ButtonAction::VolumeSlider => Self::VolumeSlider,
            ButtonAction::BrightnessSlider => Self::BrightnessSlider,
            ButtonAction::CycleTheme => Self::CycleTheme,
        }
    }

    /// Rebuild the wire action; a `Keys` form with no keys is the
    /// action's `None` (empty key list), exactly as a missing `Action`
    /// parses.
    pub fn to_action(&self) -> ButtonAction {
        match self {
            Self::None => ButtonAction::Keys(Vec::new()),
            Self::Keys(keys) => ButtonAction::Keys(keys.clone()),
            Self::SwitchLayer(layer) => ButtonAction::SwitchLayer(layer.clone()),
            Self::Command(command) => ButtonAction::Command(command.clone()),
            Self::CommandLayer { command, layer } => ButtonAction::CommandLayer {
                command: command.clone(),
                layer: layer.clone(),
            },
            Self::ApplyTheme(theme) => ButtonAction::ApplyTheme(theme.clone()),
            Self::ApplyThemeLayer { theme, layer } => ButtonAction::ApplyThemeLayer {
                theme: theme.clone(),
                layer: layer.clone(),
            },
            Self::VolumeSlider => ButtonAction::VolumeSlider,
            Self::BrightnessSlider => ButtonAction::BrightnessSlider,
            Self::CycleTheme => ButtonAction::CycleTheme,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Keys(_) => "Key combo",
            Self::SwitchLayer(_) => "Switch to layer",
            Self::Command(_) => "Run command",
            Self::CommandLayer { .. } => "Run command + switch layer",
            Self::ApplyTheme(_) => "Apply theme",
            Self::ApplyThemeLayer { .. } => "Apply theme + switch layer",
            Self::VolumeSlider => "Volume slider",
            Self::BrightnessSlider => "Brightness slider",
            Self::CycleTheme => "Cycle themes",
        }
    }
}

/// Which button content field a `ButtonConfig` carries, in the
/// daemon's own priority order.
pub fn button_kind(button: &ButtonConfig) -> ButtonKind {
    if button.text.is_some() {
        ButtonKind::Text
    } else if button.icon.is_some() {
        ButtonKind::Icon
    } else if button.time.is_some() {
        ButtonKind::Time
    } else if button.battery.is_some() {
        ButtonKind::Battery
    } else if button.waveform.unwrap_or(false) {
        ButtonKind::Waveform
    } else if button.visualizer.unwrap_or(false) {
        ButtonKind::Visualizer
    } else if button.now_playing.unwrap_or(false) {
        ButtonKind::NowPlaying
    } else {
        ButtonKind::Spacer
    }
}

/// Switch a button's content kind: the chosen kind's field gets a
/// starter value and every other content field is cleared, so the
/// saved file always carries exactly one kind, the way the daemon's
/// builder reads them.
pub fn set_button_kind(button: &mut ButtonConfig, kind: ButtonKind) {
    button.text = None;
    button.icon = None;
    button.time = None;
    button.battery = None;
    button.waveform = None;
    button.visualizer = None;
    button.now_playing = None;
    button.locale = None;
    match kind {
        ButtonKind::Text => button.text = Some("Label".into()),
        ButtonKind::Icon => button.icon = Some("search".into()),
        ButtonKind::Time => button.time = Some("24hr".into()),
        ButtonKind::Battery => button.battery = Some("both".into()),
        ButtonKind::Waveform => button.waveform = Some(true),
        ButtonKind::Visualizer => button.visualizer = Some(true),
        ButtonKind::NowPlaying => button.now_playing = Some(true),
        ButtonKind::Spacer => {}
    }
}

/// Save errors belong to the session, independently of draft validation.
#[derive(Default, Debug)]
pub struct SaveFeedback {
    pub error: Option<String>,
}

impl SaveFeedback {
    pub fn record(&mut self, result: Result<(), String>) {
        self.error = result.err();
    }
}

/// One-shot bypass prevents Discard from re-entering the close dialog.
#[derive(Default)]
pub struct CloseGuard {
    bypass: bool,
    pub dialog_open: bool,
}
impl CloseGuard {
    pub fn request(&mut self, dirty: bool) -> bool {
        self.bypass || !dirty
    }
    pub fn discard(&mut self) {
        self.bypass = true;
    }
}

/// The editing document: the base file it overlays, the user file it
/// will install, and the overlay being edited.
#[derive(Clone)]
pub struct Document {
    /// Kept for diagnostics and re-open paths; the frozen text is
    /// what the model parses.
    _base_path: PathBuf,
    user_path: PathBuf,
    /// The base document frozen at open time, so editing cannot race a
    /// daemon package update mid-session; reopening reloads it.
    base_text: String,
    user: ConfigProxy,
    /// Set when the overlay differs from what the file on disk holds.
    saved_text: String,
    /// The session writes one timestamped backup before the first
    /// install over an existing user file.
    backed_up: bool,
}

impl Document {
    /// Open the document: the base file must parse; a missing or empty
    /// user file starts from an empty overlay (everything inherits the
    /// base, and the app's first save creates the user file).
    pub fn open(base_path: &Path, user_path: &Path) -> Result<Self> {
        let base_text = fs::read_to_string(base_path)
            .with_context(|| format!("reading the base configuration {}", base_path.display()))?;
        parse_config(&base_text, None, 2008).with_context(|| {
            format!("the base configuration {} is invalid", base_path.display())
        })?;
        let user = match fs::read_to_string(user_path) {
            Ok(text) if text.trim().is_empty() => ConfigProxy::default(),
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("parsing the user file {}", user_path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => ConfigProxy::default(),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("reading the user file {}", user_path.display()))
            }
        };
        Ok(Self {
            _base_path: base_path.to_owned(),
            user_path: user_path.to_owned(),
            base_text,
            saved_text: to_user_toml(&user),
            user,
            backed_up: false,
        })
    }

    pub fn user_path(&self) -> &Path {
        &self.user_path
    }

    pub fn user(&self) -> &ConfigProxy {
        &self.user
    }

    pub fn user_mut(&mut self) -> &mut ConfigProxy {
        &mut self.user
    }

    pub fn is_dirty(&self) -> bool {
        self.candidate_text() != self.saved_text
    }

    pub fn has_override(&self, name: &str) -> bool {
        toml::Value::try_from(&self.user)
            .ok()
            .and_then(|value| value.get(name).cloned())
            .is_some()
    }

    pub fn reset_global(&mut self, name: &str) {
        let mut value = toml::Value::try_from(&self.user).expect("serializable overlay");
        value.as_table_mut().expect("overlay table").remove(name);
        self.user = value
            .try_into()
            .expect("removing an optional setting preserves the schema");
    }

    pub fn base_text(&self) -> &str {
        &self.base_text
    }

    /// The merged configuration as the daemon computes it: the base
    /// document with this overlay applied field by field.
    pub fn effective(&self) -> Result<ConfigProxy> {
        let mut base: ConfigProxy = toml::from_str(&self.base_text)?;
        base.merge(self.user.clone());
        Ok(base)
    }

    /// The user overlay in its canonical file spelling - what save
    /// installs and what validation parses.
    pub fn candidate_text(&self) -> String {
        to_user_toml(&self.user)
    }

    /// Render a theme audition against an isolated overlay. Previewing a
    /// candidate must never change the editable document or its dirty state.
    pub fn candidate_text_for_theme(&self, theme: &str) -> String {
        let mut audition = self.user.clone();
        audition.active_theme = Some(theme.to_owned());
        to_user_toml(&audition)
    }

    /// Choose the fixed startup theme written by the configurator's Save path.
    pub fn set_active_theme(&mut self, theme: &str) {
        self.user.active_theme = Some(theme.to_owned());
    }

    /// Validate the current overlay through the exact daemon path at
    /// both bar geometries (2008 and the esc-synthesizing 2170),
    /// including icon resolution against the daemon's search
    /// directories. Empty means valid to install.
    pub fn validate(&self) -> Vec<String> {
        let candidate = self.candidate_text();
        let search = IconSearch::new(dfr_render::config::ICON_DIRS.iter().copied());
        let mut diagnostics = Vec::new();
        for width in [2008u16, 2170] {
            match parse_config(&self.base_text, Some(&candidate), width) {
                Ok(model) => {
                    if let Err(err) = search.validate_layout(&model.layout) {
                        diagnostics.push(format!("at width {width}: {err:#}"));
                    }
                }
                Err(err) => diagnostics.push(format!("at width {width}: {err:#}")),
            }
        }
        diagnostics
    }

    /// Install the overlay: one timestamped backup of any existing
    /// user file (first save of the session), then write the canonical
    /// document to a staged file and rename it into place, which is the
    /// atomic save the daemon's inotify watcher reloads without a
    /// restart. The overlay is validated first: an invalid state never
    /// reaches the file.
    pub fn save(&mut self) -> Result<()> {
        let diagnostics = self.validate();
        if !diagnostics.is_empty() {
            bail!(
                "refusing to save an invalid configuration:\n  {}",
                diagnostics.join("\n  ")
            );
        }
        if !self.backed_up && self.user_path.exists() {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_secs())
                .unwrap_or(0);
            let backup = self.user_path.with_file_name(format!(
                "{}.bak-{stamp}",
                self.user_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("config.toml")
            ));
            fs::copy(&self.user_path, &backup).with_context(|| {
                format!(
                    "backing up {} to {}",
                    self.user_path.display(),
                    backup.display()
                )
            })?;
            self.backed_up = true;
        }
        if let Some(parent) = self.user_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("creating the configuration directory {}", parent.display())
            })?;
        }
        let stage = self.user_path.with_extension("toml.tmp");
        let text = self.candidate_text();
        fs::write(&stage, &text)
            .with_context(|| format!("staging the configuration {}", stage.display()))?;
        fs::rename(&stage, &self.user_path).with_context(|| {
            format!("installing the configuration {}", self.user_path.display())
        })?;
        self.saved_text = self.candidate_text();
        Ok(())
    }

    /// The named layers of the overlay, migrating a legacy two-layer
    /// document to the named syntax on first edit: `primary` and
    /// `media` bars with `DefaultLayer`/`FnLayer` resolved from
    /// `MediaLayerDefault`. The migration is one-way (the app only ever
    /// writes the named form, as the configuration contract
    /// documents).
    pub fn layers(&mut self) -> &mut Vec<LayerConfig> {
        if self.user.layers.is_none() {
            let (startup, function) = self.layer_roles();
            self.user.layers = Some(self.layer_snapshot());
            self.user.default_layer = Some(startup);
            self.user.fn_layer = function;
            self.user.primary_layer_keys = None;
            self.user.media_layer_keys = None;
            self.user.media_layer_default = None;
        }
        self.user.layers.as_mut().expect("layers present")
    }

    /// Inspection never materializes or dirties an inherited layer list.
    pub fn layer_snapshot(&self) -> Vec<LayerConfig> {
        let effective = self.effective().expect("the base document parses");
        effective.layers.unwrap_or_else(|| {
            vec![
                LayerConfig {
                    name: "primary".into(),
                    keys: effective.primary_layer_keys.unwrap_or_default(),
                },
                LayerConfig {
                    name: "media".into(),
                    keys: effective.media_layer_keys.unwrap_or_default(),
                },
            ]
        })
    }

    pub fn layer_roles(&self) -> (String, Option<String>) {
        let effective = self.effective().expect("the base document parses");
        if effective.layers.is_some() {
            (
                effective.default_layer.unwrap_or_default(),
                effective.fn_layer,
            )
        } else if effective.media_layer_default.unwrap_or(false) {
            ("media".into(), Some("primary".into()))
        } else {
            ("primary".into(), Some("media".into()))
        }
    }

    pub fn set_layer_role(&mut self, name: Option<String>, startup: bool) {
        self.layers();
        if startup {
            self.user.default_layer = name;
        } else {
            self.user.fn_layer = name;
        }
    }

    pub fn rename_layer(&mut self, index: usize, name: &str) -> Result<()> {
        if name.trim().is_empty() {
            bail!("Enter a layer name.");
        }
        let layers = self.layer_snapshot();
        let old = layers.get(index).context("Choose a layer.")?.name.clone();
        if layers
            .iter()
            .enumerate()
            .any(|(i, layer)| i != index && layer.name == name)
        {
            bail!("A layer named {name:?} already exists.");
        }
        self.layers()[index].name = name.into();
        self.retarget_layer(&old, name);
        Ok(())
    }

    fn retarget_layer(&mut self, old: &str, new: &str) {
        if self.user.default_layer.as_deref() == Some(old) {
            self.user.default_layer = Some(new.into());
        }
        if self.user.fn_layer.as_deref() == Some(old) {
            self.user.fn_layer = Some(new.into());
        }
        for layer in self.layers() {
            for button in &mut layer.keys {
                match &mut button.action {
                    ButtonAction::SwitchLayer(target)
                    | ButtonAction::CommandLayer { layer: target, .. }
                    | ButtonAction::ApplyThemeLayer { layer: target, .. }
                        if target == old =>
                    {
                        *target = new.into()
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn incoming_links(&self, index: usize) -> Vec<(usize, usize, String)> {
        let layers = self.layer_snapshot();
        let Some(target) = layers.get(index) else {
            return Vec::new();
        };
        layers
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != index)
            .flat_map(|(i, layer)| {
                layer
                    .keys
                    .iter()
                    .enumerate()
                    .filter(|(_, button)| {
                        button.action.layer_target() == Some(target.name.as_str())
                    })
                    .map(|(b, _)| (i, b, layer.name.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub fn delete_layer(&mut self, index: usize, replacement: Option<&str>) -> Result<()> {
        let layers = self.layer_snapshot();
        if layers.len() <= 1 {
            bail!("Keep at least one layer.");
        }
        let name = &layers.get(index).context("Choose a layer.")?.name;
        let (startup, function) = self.layer_roles();
        let needs_repair = startup == *name
            || function.as_deref() == Some(name)
            || !self.incoming_links(index).is_empty();
        if needs_repair && replacement.is_none() {
            bail!("Choose a replacement for this layer’s assignments and incoming links.");
        }
        if let Some(replacement) = replacement {
            if replacement == name || !layers.iter().any(|layer| layer.name == replacement) {
                bail!("Choose another existing layer.");
            }
            self.layers();
            self.retarget_layer(name, replacement);
        }
        self.layers().remove(index);
        Ok(())
    }

    pub fn setting(&self, name: &str) -> toml::Value {
        let effective = self.effective().expect("the base document parses");
        if let Some(value) = toml::Value::try_from(effective)
            .ok()
            .and_then(|value| value.get(name).cloned())
        {
            return value;
        }
        let knob = dfr_config::KNOBS
            .iter()
            .find(|k| k.name == name)
            .expect("registered setting");
        match knob.kind {
            "bool" => toml::Value::Boolean(knob.default == "true"),
            "u32" => toml::Value::Integer(knob.default.parse().unwrap_or(0)),
            _ => toml::Value::String(knob.default.trim_matches('"').into()),
        }
    }

    pub fn set_setting(&mut self, name: &str, setting: Option<toml::Value>) {
        let mut value = toml::Value::try_from(&self.user).expect("serializable overlay");
        let table = value.as_table_mut().expect("overlay table");
        if let Some(setting) = setting {
            table.insert(name.into(), setting);
        } else {
            table.remove(name);
        }
        self.user = value.try_into().expect("typed setting");
    }

    /// Names every layer reachable from `DefaultLayer` through layer
    /// actions and the theme picker, for the orphan-layer hint: valid
    /// but unreachable layers stay editable, the daemon allows them
    /// deliberately.
    pub fn reachable_layers(&self) -> Vec<String> {
        let Ok(effective) = self.effective() else {
            return Vec::new();
        };
        let layers = self.layer_snapshot();
        let (start, function) = self.layer_roles();
        let themes_enabled = effective.themes_enabled.unwrap_or(true);
        let mut seen: Vec<String> = Vec::new();
        let mut queue = vec![start];
        if let Some(function) = function {
            queue.push(function);
        }
        while let Some(name) = queue.pop() {
            if seen.contains(&name) {
                continue;
            }
            if let Some(layer) = layers.iter().find(|layer| layer.name == name) {
                seen.push(name.clone());
                for button in &layer.keys {
                    if let Some(target) = button.action.layer_target() {
                        queue.push(target.to_owned());
                    }
                }
            }
        }
        if themes_enabled && !seen.contains(&"themes".to_owned()) {
            seen.push("themes".into());
        }
        seen
    }

    /// The theme names the document knows: the built-ins (from the
    /// shipped seed) plus user-defined entries, for pickers.
    pub fn known_themes(&self) -> Vec<String> {
        let mut names = dfr_config::themes::builtin_themes()
            .map(|set| set.names)
            .unwrap_or_default();
        if let Some(themes) = self.effective().ok().and_then(|proxy| proxy.themes) {
            for name in themes.keys() {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
        names.truncate(MAX_THEMES);
        names
    }

    /// Define or replace a user theme, refusing to exceed the picker's
    /// theme budget.
    pub fn set_theme(&mut self, name: &str, theme: ThemeConfig) -> Result<()> {
        if !self.known_themes().iter().any(|known| known == name)
            && self.known_themes().len() >= MAX_THEMES
        {
            bail!("at most {MAX_THEMES} themes may be defined");
        }
        if self.user.themes.is_none() {
            self.user.themes = self.effective()?.themes;
        }
        let themes = self.user.themes.get_or_insert_with(Default::default);
        themes.insert(name.to_owned(), theme);
        Ok(())
    }

    /// Remove a user-defined theme (built-ins cannot be removed, only
    /// overridden; removing an override restores the built-in).
    pub fn remove_theme(&mut self, name: &str) {
        if self.user.themes.is_none() {
            self.user.themes = self.effective().ok().and_then(|p| p.themes);
        }
        if let Some(themes) = &mut self.user.themes {
            themes.remove(name);
        }
    }

    pub fn set_bands(&mut self, bands: Vec<BandConfig>) {
        self.user.day_cycle_bands = Some(bands);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = include_str!("../../../share/tiny-dfr/config.toml");
    const MULTIBAR: &str = include_str!("../../../share/tiny-dfr/multibar.toml");

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "dfr-configurator-test-{tag}-{}-{}",
                std::process::id(),
                rand::random_u64()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn write(&self, name: &str, text: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, text).unwrap();
            path
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // A tiny rand stand-in (the crate has no rand dependency): xorshift
    // seeded from the clock is plenty for unique temp dirs.
    mod rand {
        pub fn random_u64() -> u64 {
            use std::time::{SystemTime, UNIX_EPOCH};
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
                .unwrap_or(0x9e3779b9);
            let mut state = nanos | 1;
            state ^= state << 13;
            state ^= 7;
            state ^= state << 17;
            state.wrapping_mul(0x2545F4914F6CDD1D)
        }
    }

    fn document(tag: &str, user: Option<&str>) -> (TempDir, Document) {
        let tmp = TempDir::new(tag);
        let base = tmp.write("base.toml", BASE);
        let user_path = match user {
            Some(text) => tmp.write("config.toml", text),
            None => tmp.0.join("config.toml"),
        };
        let doc = Document::open(&base, &user_path).unwrap();
        (tmp, doc)
    }

    #[test]
    fn inspecting_inherited_named_layers_does_not_create_a_draft() {
        let tmp = TempDir::new("inherited-named");
        let base = tmp.write("base.toml", MULTIBAR);
        let mut doc = Document::open(&base, &tmp.0.join("config.toml")).unwrap();
        assert_eq!(doc.layer_snapshot().len(), 5);
        assert_eq!(doc.layer_roles().0, "home");
        assert!(!doc.is_dirty());
        doc.layers()[0].keys[0].stretch = Some(3);
        assert_eq!(doc.layer_snapshot().len(), 5);
        assert!(doc.is_dirty());
        assert!(doc.validate().is_empty(), "{:?}", doc.validate());
    }

    #[test]
    fn scalar_controls_use_base_values_and_code_defaults_without_writing_them() {
        let tmp = TempDir::new("scalars");
        let base = tmp.write(
            "base.toml",
            &format!("{BASE}\nAdaptiveBrightness = true\nActiveBrightness = 207\n"),
        );
        // Avoid duplicating a key the shipped sample already sets.
        let proxy: ConfigProxy = toml::from_str(BASE).unwrap();
        let mut base_proxy = proxy;
        base_proxy.adaptive_brightness = Some(true);
        base_proxy.active_brightness = Some(207);
        std::fs::write(&base, to_user_toml(&base_proxy)).unwrap();
        let mut doc = Document::open(&base, &tmp.0.join("config.toml")).unwrap();
        assert_eq!(doc.setting("AdaptiveBrightness").as_bool(), Some(true));
        assert_eq!(doc.setting("ActiveBrightness").as_integer(), Some(207));
        assert_eq!(doc.setting("PetEnabled").as_bool(), Some(true));
        assert!(!doc.is_dirty());
        doc.set_setting("ActiveBrightness", Some(toml::Value::Integer(128)));
        assert_eq!(doc.setting("ActiveBrightness").as_integer(), Some(128));
        doc.reset_global("ActiveBrightness");
        assert_eq!(doc.setting("ActiveBrightness").as_integer(), Some(207));
        assert!(!doc.is_dirty());
    }

    #[test]
    fn rename_retargets_roles_and_every_combined_action_atomically() {
        let (_tmp, mut doc) = document("rename", Some(MULTIBAR));
        let old = doc.layer_snapshot()[0].name.clone();
        doc.layers()[1].keys = vec![
            ButtonConfig {
                text: Some("Go".into()),
                action: ButtonAction::SwitchLayer(old.clone()),
                ..Default::default()
            },
            ButtonConfig {
                text: Some("Run".into()),
                action: ButtonAction::CommandLayer {
                    command: "true".into(),
                    layer: old.clone(),
                },
                ..Default::default()
            },
            ButtonConfig {
                text: Some("Theme".into()),
                action: ButtonAction::ApplyThemeLayer {
                    theme: "Default".into(),
                    layer: old.clone(),
                },
                ..Default::default()
            },
        ];
        doc.set_layer_role(Some(old), false);
        doc.rename_layer(0, "Renamed").unwrap();
        assert_eq!(
            doc.layer_roles(),
            ("Renamed".into(), Some("Renamed".into()))
        );
        assert!(doc.layer_snapshot()[1]
            .keys
            .iter()
            .all(|b| b.action.layer_target() == Some("Renamed")));
        assert!(doc.validate().is_empty(), "{:?}", doc.validate());
        let before = doc.candidate_text();
        let duplicate = doc.layer_snapshot()[1].name.clone();
        assert!(doc.rename_layer(0, &duplicate).is_err());
        assert_eq!(before, doc.candidate_text());
    }

    #[test]
    fn deletion_requires_an_explicit_repair_and_keeps_valid_references() {
        let (_tmp, mut doc) = document("delete", Some(MULTIBAR));
        let before = doc.candidate_text();
        assert!(!doc.incoming_links(0).is_empty());
        assert!(doc.delete_layer(0, None).is_err());
        assert_eq!(before, doc.candidate_text());
        let replacement = doc.layer_snapshot()[1].name.clone();
        doc.delete_layer(0, Some(&replacement)).unwrap();
        assert_eq!(doc.layer_roles().0, replacement);
        assert!(doc.validate().is_empty(), "{:?}", doc.validate());
        while doc.layer_snapshot().len() > 1 {
            let target = doc.layer_snapshot()[0].name.clone();
            let index = doc.layer_snapshot().len() - 1;
            doc.delete_layer(index, Some(&target)).unwrap();
        }
        assert!(doc.delete_layer(0, None).is_err());
    }

    #[test]
    fn fn_assigned_layers_are_reachable() {
        let (_tmp, doc) = document("fn-reachability", Some(MULTIBAR));
        let function = doc.layer_roles().1.unwrap();
        assert!(doc.reachable_layers().contains(&function));
    }

    #[test]
    fn an_authored_theme_layer_can_link_to_another_layer() {
        let (_tmp, mut doc) = document("authored-theme-reachability", Some(MULTIBAR));
        let start = doc.layer_roles().0;
        let index = doc
            .layer_snapshot()
            .iter()
            .position(|layer| layer.name == start)
            .unwrap();
        doc.rename_layer(index, "themes").unwrap();
        doc.layers().push(LayerConfig {
            name: "Linked".into(),
            keys: vec![ButtonConfig {
                text: Some("Linked".into()),
                ..Default::default()
            }],
        });
        doc.layers()[index].keys[0].action = ButtonAction::SwitchLayer("Linked".into());
        assert!(doc.reachable_layers().contains(&"Linked".into()));
    }

    #[test]
    fn all_ten_action_forms_round_trip_through_the_ui_model() {
        let forms = [
            ActionForm::None,
            ActionForm::Keys(vec![input_linux::Key::LeftMeta, input_linux::Key::L]),
            ActionForm::SwitchLayer("home".into()),
            ActionForm::Command("true".into()),
            ActionForm::CommandLayer {
                command: "true".into(),
                layer: "home".into(),
            },
            ActionForm::ApplyTheme("Default".into()),
            ActionForm::ApplyThemeLayer {
                theme: "Default".into(),
                layer: "home".into(),
            },
            ActionForm::VolumeSlider,
            ActionForm::BrightnessSlider,
            ActionForm::CycleTheme,
        ];
        for form in forms {
            let button = ButtonConfig {
                text: Some("Test".into()),
                action: form.to_action(),
                ..Default::default()
            };
            let parsed: ButtonConfig = toml::from_str(&toml::to_string(&button).unwrap()).unwrap();
            assert_eq!(ActionForm::of(&parsed.action), form);
        }
    }

    #[test]
    fn editing_and_deleting_the_last_inherited_theme_preserves_map_semantics() {
        let tmp = TempDir::new("base-theme");
        let mut base: ConfigProxy = toml::from_str(BASE).unwrap();
        base.themes = Some(std::collections::BTreeMap::from([(
            "Inherited".into(),
            ThemeConfig::default(),
        )]));
        let base_path = tmp.write("base.toml", &to_user_toml(&base));
        let mut doc = Document::open(&base_path, &tmp.0.join("config.toml")).unwrap();
        assert!(doc.known_themes().contains(&"Inherited".into()));
        doc.set_theme("Default", ThemeConfig::default()).unwrap();
        assert!(doc.known_themes().contains(&"Inherited".into()));
        doc.remove_theme("Default");
        doc.remove_theme("Inherited");
        let parsed: ConfigProxy = toml::from_str(&doc.candidate_text()).unwrap();
        assert_eq!(parsed.themes, Some(Default::default()));
        assert!(!doc.known_themes().contains(&"Inherited".into()));
        assert!(doc.validate().is_empty());
        doc.reset_global("Themes");
        assert!(doc.known_themes().contains(&"Inherited".into()));
    }

    #[test]
    fn overriding_a_builtin_is_permitted_at_the_theme_limit() {
        let (_tmp, mut doc) = document("theme-limit", None);
        while doc.known_themes().len() < MAX_THEMES {
            let name = format!("Extra {}", doc.known_themes().len());
            doc.set_theme(&name, ThemeConfig::default()).unwrap();
        }
        assert!(doc
            .set_theme("One Too Many", ThemeConfig::default())
            .is_err());
        doc.set_theme("Default", ThemeConfig::default()).unwrap();
    }

    #[test]
    fn cancel_and_discard_have_distinct_close_guard_behaviors() {
        let mut guard = CloseGuard::default();
        assert!(!guard.request(true));
        guard.dialog_open = true;
        guard.dialog_open = false; // Cancel keeps the dirty guard armed.
        assert!(!guard.request(true));
        guard.discard();
        assert!(guard.request(true)); // Discard can close a dirty document exactly once.
    }

    #[test]
    fn save_failure_survives_validation_and_retains_the_draft() {
        let (tmp, mut doc) = document("save-failure", None);
        doc.user_mut().active_brightness = Some(201);
        std::fs::create_dir(tmp.0.join("config.toml.tmp")).unwrap();
        let mut feedback = SaveFeedback::default();
        feedback.record(doc.save().map_err(|err| format!("{err:#}")));
        assert!(doc.validate().is_empty());
        assert!(feedback.error.as_ref().unwrap().contains("staging"));
        assert!(doc.is_dirty());
        assert_eq!(doc.user().active_brightness, Some(201));
        std::fs::remove_dir(tmp.0.join("config.toml.tmp")).unwrap();
        feedback.record(doc.save().map_err(|err| format!("{err:#}")));
        assert!(feedback.error.is_none());
        assert!(!doc.is_dirty());
    }

    #[test]
    fn reset_removes_the_override_and_restores_the_base_value() {
        let (_tmp, mut doc) = document("reset", Some("ActiveBrightness = 201\n"));
        assert!(doc.has_override("ActiveBrightness"));
        doc.reset_global("ActiveBrightness");
        assert!(!doc.has_override("ActiveBrightness"));
        let base: ConfigProxy = toml::from_str(BASE).unwrap();
        assert_eq!(
            doc.effective().unwrap().active_brightness,
            base.active_brightness
        );
    }

    #[test]
    fn a_missing_user_file_opens_as_the_empty_overlay_and_saves_validly() {
        let (tmp, mut doc) = document("missing", None);
        assert!(!doc.is_dirty());
        assert!(doc.validate().is_empty(), "{:?}", doc.validate());
        doc.save().unwrap();
        let installed = std::fs::read_to_string(tmp.0.join("config.toml")).unwrap();
        assert_eq!(installed, doc.candidate_text());
        assert!(!doc.is_dirty());
        // The empty overlay saves as an empty document (comments only),
        // so the daemon keeps running on the base file untouched.
        assert!(!installed.contains('='));
    }

    #[test]
    fn the_shipped_multibar_round_trips_through_the_model_and_writer() {
        let (_tmp, mut doc) = document("multibar", Some(MULTIBAR));
        let before = doc.user().clone();
        assert!(doc.validate().is_empty(), "{:?}", doc.validate());
        // The canonical writer emits exactly the overlay's meaning:
        // re-parsing the candidate text yields an equal overlay.
        let round: ConfigProxy = toml::from_str(&doc.candidate_text()).unwrap();
        assert_eq!(round, before);
        assert_eq!(doc.layers().len(), 5);
        assert_eq!(doc.user().default_layer.as_deref(), Some("home"));
    }

    #[test]
    fn editing_layers_buttons_and_actions_updates_the_overlay_deterministically() {
        let (_tmp, mut doc) = document("edit", None);
        // The shipped base uses the legacy syntax, so the first edit
        // migrates its two bars into named layers...
        assert_eq!(doc.layers().len(), 2);
        assert_eq!(doc.layers()[0].name, "primary");
        // ...and then the overlay's own layers are ordinary edits.
        {
            let layers = doc.layers();
            layers.push(LayerConfig {
                name: "home".into(),
                keys: vec![ButtonConfig {
                    text: Some("Lamp".into()),
                    action: ButtonAction::Command("systemctl toggle lamp".into()),
                    stretch: Some(2),
                    ..Default::default()
                }],
            });
        }
        doc.user_mut().default_layer = Some("home".into());
        assert!(doc.is_dirty());
        assert!(doc.validate().is_empty(), "{:?}", doc.validate());
        // A second layer reachable through a switch action.
        {
            let layers = doc.layers();
            layers.push(LayerConfig {
                name: "settings".into(),
                keys: vec![ButtonConfig {
                    text: Some("Back".into()),
                    action: ButtonAction::SwitchLayer("home".into()),
                    ..Default::default()
                }],
            });
        }
        let round: ConfigProxy = toml::from_str(&doc.candidate_text()).unwrap();
        assert_eq!(round.layers.as_ref().unwrap().len(), 4);
        let home = round
            .layers
            .as_ref()
            .unwrap()
            .iter()
            .find(|layer| layer.name == "home")
            .unwrap();
        assert!(matches!(
            &home.keys[0].action,
            ButtonAction::Command(command) if command == "systemctl toggle lamp"
        ));
    }

    #[test]
    fn invalid_states_are_rejected_before_save_and_never_touch_the_file() {
        let (tmp, mut doc) = document("invalid", Some(MULTIBAR));
        // Duplicate layer name: rejected by the shared validation.
        {
            let layers = doc.layers();
            let clone = layers[0].clone();
            layers.push(clone);
        }
        let diagnostics = doc.validate();
        assert!(!diagnostics.is_empty(), "duplicate layer accepted");
        let err = doc.save().unwrap_err();
        assert!(format!("{err:#}").contains("refusing"), "{err:#}");
        // The file on disk still holds the original multibar text.
        assert_eq!(
            std::fs::read_to_string(tmp.0.join("config.toml")).unwrap(),
            MULTIBAR
        );
        // Repair and save: now it installs.
        doc.layers().pop();
        assert!(doc.validate().is_empty(), "{:?}", doc.validate());
        doc.save().unwrap();
    }

    #[test]
    fn battery_thresholds_enforce_the_relation_in_ui_validation() {
        let (_tmp, mut doc) = document("battery", None);
        doc.user_mut().battery_warning_threshold = Some(10);
        doc.user_mut().battery_critical_threshold = Some(25);
        assert!(
            doc.validate()
                .iter()
                .any(|d| d.contains("CriticalThreshold")),
            "{:?}",
            doc.validate()
        );
        doc.user_mut().battery_critical_threshold = Some(5);
        assert!(doc.validate().is_empty(), "{:?}", doc.validate());
    }

    #[test]
    fn legacy_documents_migrate_one_way_to_named_layers() {
        let legacy = "PrimaryLayerKeys = [{ Text = \"F1\", Action = \"F1\" }]\n\
                      MediaLayerKeys = [{ Text = \"Vol\", Action = \"VolumeSlider\" }]\n\
                      MediaLayerDefault = true\n";
        let (_tmp, mut doc) = document("legacy", Some(legacy));
        assert_eq!(doc.layers().len(), 2);
        assert_eq!(doc.layers()[0].name, "primary");
        assert_eq!(doc.layers()[1].name, "media");
        assert_eq!(doc.user().default_layer.as_deref(), Some("media"));
        assert_eq!(doc.user().fn_layer.as_deref(), Some("primary"));
        assert!(doc.user().primary_layer_keys.is_none());
        assert!(doc.validate().is_empty(), "{:?}", doc.validate());
        // The named form is what installs: no legacy keys in the file.
        let text = doc.candidate_text();
        assert!(!text.contains("PrimaryLayerKeys"));
        assert!(text.contains("[[Layers]]"));
    }

    #[test]
    fn orphan_layers_are_flagged_not_rejected() {
        let (_tmp, mut doc) = document("orphan", None);
        {
            let layers = doc.layers();
            layers.push(LayerConfig {
                name: "home".into(),
                keys: vec![ButtonConfig {
                    text: Some("Go".into()),
                    ..Default::default()
                }],
            });
            layers.push(LayerConfig {
                name: "island".into(),
                keys: vec![ButtonConfig {
                    text: Some("Lost".into()),
                    ..Default::default()
                }],
            });
        }
        doc.user_mut().default_layer = Some("home".into());
        assert!(doc.validate().is_empty(), "{:?}", doc.validate());
        let reachable = doc.reachable_layers();
        assert!(reachable.contains(&"home".to_owned()));
        assert!(!reachable.contains(&"island".to_owned()));
    }

    #[test]
    fn save_backs_up_once_and_installs_atomically() {
        let tmp = TempDir::new("backup");
        let base = tmp.write("base.toml", BASE);
        let user = tmp.write("config.toml", "ShowButtonOutlines = false\n");
        let mut doc = Document::open(&base, &user).unwrap();
        doc.user_mut().show_button_outlines = Some(true);
        doc.save().unwrap();
        let backups: Vec<_> = std::fs::read_dir(&tmp.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("config.toml.bak-"))
            .collect();
        assert_eq!(backups.len(), 1, "exactly one backup: {backups:?}");
        assert_eq!(
            std::fs::read_to_string(tmp.0.join(&backups[0])).unwrap(),
            "ShowButtonOutlines = false\n"
        );
        // A second save does not stack another backup.
        doc.user_mut().show_button_outlines = Some(false);
        doc.save().unwrap();
        let count = std::fs::read_dir(&tmp.0)
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .map(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .starts_with("config.toml.bak-")
                    })
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(count, 1);
        // Nothing staged is left behind.
        assert!(!tmp.0.join("config.toml.tmp").exists());
    }

    #[test]
    fn the_effective_mirrors_the_daemons_merge_semantics() {
        let (_tmp, doc) = document("merge", Some("ActiveBrightness = 200\n"));
        let effective = doc.effective().unwrap();
        assert_eq!(effective.active_brightness, Some(200));
        // Unset knobs keep the base's values: the shipped base file
        // sets these, and the overlay inherits them untouched.
        let base: ConfigProxy = toml::from_str(BASE).unwrap();
        assert_eq!(effective.show_button_outlines, base.show_button_outlines);
        assert_eq!(effective.font_template, base.font_template);
    }

    #[test]
    fn theme_budget_and_user_themes_flow_through_the_model() {
        let (_tmp, mut doc) = document("themes", None);
        let names = doc.known_themes();
        assert!(names.len() >= 8, "the built-ins are always known");
        doc.set_theme("Mine", ThemeConfig::default()).unwrap();
        assert!(doc.known_themes().contains(&"Mine".to_owned()));
        doc.remove_theme("Mine");
        assert!(!doc.known_themes().contains(&"Mine".to_owned()));
    }

    #[test]
    fn theme_candidate_preview_is_isolated_and_switch_saves_active_theme() {
        let (_tmp, mut doc) = document("theme-switch", None);
        let before = doc.candidate_text();
        let candidate = doc.candidate_text_for_theme("Nord");
        let audition: ConfigProxy = toml::from_str(&candidate).unwrap();
        assert_eq!(audition.active_theme.as_deref(), Some("Nord"));
        assert_eq!(doc.candidate_text(), before, "preview changed the draft");

        doc.set_active_theme("Nord");
        assert!(doc.is_dirty());
        doc.save().unwrap();
        let installed: ConfigProxy =
            toml::from_str(&fs::read_to_string(doc.user_path()).unwrap()).unwrap();
        assert_eq!(installed.active_theme.as_deref(), Some("Nord"));
        assert!(!doc.is_dirty());
    }

    #[test]
    fn button_kind_switching_keeps_exactly_one_content_field() {
        let mut button = ButtonConfig::default();
        set_button_kind(&mut button, ButtonKind::Icon);
        assert_eq!(button_kind(&button), ButtonKind::Icon);
        set_button_kind(&mut button, ButtonKind::Time);
        assert!(button.icon.is_none());
        assert_eq!(button.time.as_deref(), Some("24hr"));
        set_button_kind(&mut button, ButtonKind::Spacer);
        assert_eq!(button_kind(&button), ButtonKind::Spacer);
    }

    #[test]
    fn key_combo_actions_survive_the_form_round_trip() {
        use input_linux::Key;
        let keys = vec![Key::LeftMeta, Key::LeftCtrl, Key::L];
        let form = ActionForm::Keys(keys.clone());
        let action = form.to_action();
        assert_eq!(ActionForm::of(&action), form);
        // The canonical spelling re-parses to the same action.
        let rendered = toml::to_string(&ButtonConfig {
            action: action.clone(),
            ..Default::default()
        })
        .unwrap();
        let parsed: ButtonConfig = toml::from_str(&rendered).unwrap();
        assert_eq!(parsed.action, action);
        assert!(rendered.contains("LeftMeta"), "{rendered}");
    }
}
