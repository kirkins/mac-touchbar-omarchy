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

/// The editing document: the base file it overlays, the user file it
/// will install, and the overlay being edited.
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
    dirty: bool,
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
            user,
            dirty: false,
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
        self.dirty = true;
        &mut self.user
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
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
        self.dirty = true;
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
        self.dirty = false;
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
            let mut migrated: Vec<LayerConfig> = Vec::new();
            let (primary, media) = {
                let mut effective = toml::from_str::<ConfigProxy>(&self.base_text)
                    .expect("the base document parses");
                effective.merge(self.user.clone());
                (
                    effective.primary_layer_keys.unwrap_or_default(),
                    effective.media_layer_keys.unwrap_or_default(),
                )
            };
            if !primary.is_empty() || !media.is_empty() {
                let media_default = self.user.media_layer_default.unwrap_or(false);
                migrated.push(LayerConfig {
                    name: "primary".into(),
                    keys: primary,
                });
                migrated.push(LayerConfig {
                    name: "media".into(),
                    keys: media,
                });
                let (default, function) = if media_default {
                    ("media", "primary")
                } else {
                    ("primary", "media")
                };
                self.user.default_layer = Some(default.into());
                self.user.fn_layer = Some(function.into());
                self.user.primary_layer_keys = None;
                self.user.media_layer_keys = None;
                self.user.media_layer_default = None;
                self.dirty = true;
            }
            self.user.layers = Some(migrated);
        }
        self.user.layers.as_mut().expect("layers present")
    }

    /// Names every layer reachable from `DefaultLayer` through layer
    /// actions and the theme picker, for the orphan-layer hint: valid
    /// but unreachable layers stay editable, the daemon allows them
    /// deliberately.
    pub fn reachable_layers(&self) -> Vec<String> {
        let Ok(effective) = self.effective() else {
            return Vec::new();
        };
        let Some(layers) = &effective.layers else {
            return Vec::new();
        };
        let start = effective.default_layer.clone().unwrap_or_default();
        let themes_enabled = effective.themes_enabled.unwrap_or(true);
        let mut seen: Vec<String> = Vec::new();
        let mut queue = vec![start];
        while let Some(name) = queue.pop() {
            if seen.contains(&name) || name == "themes" {
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
        if let Some(themes) = &self.user.themes {
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
        if !self
            .user
            .themes
            .as_ref()
            .is_some_and(|themes| themes.contains_key(name))
            && self.known_themes().len() >= MAX_THEMES
        {
            bail!("at most {MAX_THEMES} themes may be defined");
        }
        let themes = self.user.themes.get_or_insert_with(Default::default);
        themes.insert(name.to_owned(), theme);
        self.dirty = true;
        Ok(())
    }

    /// Remove a user-defined theme (built-ins cannot be removed, only
    /// overridden; removing an override restores the built-in).
    pub fn remove_theme(&mut self, name: &str) {
        if let Some(themes) = &mut self.user.themes {
            if themes.remove(name).is_some() {
                self.dirty = true;
            }
        }
    }

    /// One band editor row's worth of state, for the day-cycle list.
    pub fn bands(&self) -> Option<&Vec<BandConfig>> {
        self.user.day_cycle_bands.as_ref()
    }

    pub fn set_bands(&mut self, bands: Vec<BandConfig>) {
        self.user.day_cycle_bands = Some(bands);
        self.dirty = true;
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
