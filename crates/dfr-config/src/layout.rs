//! Layer synthesis: the validated named-layer (or legacy two-layer)
//! layout the daemon builds from a merged configuration, including the
//! synthesized pet layer, theme picker, Home buttons, and wide-bar
//! escape keys. This module owns the per-button content validation that
//! used to panic deep inside the daemon's button builder: every knob
//! that selects a rendering mode or a size is checked here, against the
//! documented ranges, so an invalid button rejects the candidate
//! configuration with its layer named instead of crashing the bar.

use crate::defaults;
use crate::proxy::{ButtonAction, ButtonConfig, ConfigProxy, LayerConfig};
use crate::themes::{ThemeSet, THEMES_LAYER};
use anyhow::{bail, Context, Result};
use input_linux::Key;
use std::collections::HashSet;

/// Name of the pet's own layer. When the pet is enabled, a layer with this
/// name is synthesized if the configuration does not define one, and the
/// `more` bar (or home bar when no `more` bar exists) gains a Pet button.
pub const PET_LAYER: &str = "pet";
/// Free button spaces after the Home button inside the synthesized pet
/// layer: open ground for the pet to wander.
pub const PET_LAYER_STRETCH: usize = 7;

/// The waveform graph styles shared by the recording waveform and the
/// music visualizer. The daemon's renderer resolves the same two names.
pub const WAVEFORM_STYLES: &[&str] = &["line", "bars"];

/// The battery widget's display modes.
pub const BATTERY_MODES: &[&str] = &["icon", "percentage", "both"];

#[cfg(test)]
mod install_tests {
    use super::*;
    const BASE: &str = include_str!("../../../share/tiny-dfr/config.toml");
    const DECK: &str = include_str!("../../../share/tiny-dfr/multibar.toml");

    #[test]
    fn first_install_pet_is_under_more_and_theme_controls_need_explicit_actions() {
        let loaded = crate::parse_config(BASE, Some(DECK), 2008).unwrap();
        let more = loaded
            .layout
            .layers
            .iter()
            .find(|layer| layer.name == "more")
            .unwrap();
        assert!(more
            .keys
            .iter()
            .any(|button| button.action.layer_target() == Some("pet")));
        let home = &loaded.layout.layers[loaded.layout.home];
        assert!(!home
            .keys
            .iter()
            .any(|button| button.action.layer_target() == Some("pet")));
        assert!(!more
            .keys
            .iter()
            .any(|button| matches!(button.action, ButtonAction::CycleTheme)
                || button.action.layer_target() == Some("themes")));

        let mut deck: ConfigProxy = toml::from_str(DECK).unwrap();
        let more = deck
            .layers
            .as_mut()
            .unwrap()
            .iter_mut()
            .find(|layer| layer.name == "more")
            .unwrap();
        more.keys.push(ButtonConfig {
            text: Some("Theme".into()),
            action: ButtonAction::CycleTheme,
            ..Default::default()
        });
        more.keys.push(ButtonConfig {
            text: Some("Themes".into()),
            action: ButtonAction::SwitchLayer("themes".into()),
            ..Default::default()
        });
        let enabled = toml::to_string(&deck).unwrap();
        let loaded = crate::parse_config(BASE, Some(&enabled), 2008).unwrap();
        let more = loaded
            .layout
            .layers
            .iter()
            .find(|layer| layer.name == "more")
            .unwrap();
        assert!(more
            .keys
            .iter()
            .any(|button| matches!(button.action, ButtonAction::CycleTheme)));
        assert!(more
            .keys
            .iter()
            .any(|button| button.action.layer_target() == Some("themes")));
        assert!(loaded
            .layout
            .layers
            .iter()
            .any(|layer| layer.name == "themes"));
        deck.themes_enabled = Some(false);
        assert!(crate::parse_config(BASE, Some(&toml::to_string(&deck).unwrap()), 2008).is_err());
    }
}

pub struct LayerLayout {
    pub layers: Vec<LayerConfig>,
    pub home: usize,
    pub fn_layer: Option<usize>,
    pub legacy: bool,
}

impl ConfigProxy {
    pub(crate) fn layer_layout(&mut self, width: u16, themes: &ThemeSet) -> Result<LayerLayout> {
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
        // The pet lives on its own layer. When a quick-keys "more" bar is
        // present, its Pet button lives there; otherwise it joins the home
        // bar. A user-defined layer named "pet" replaces the synthesized
        // playground, and an existing visible button targeting it replaces
        // the synthesized Pet button.
        if self.pet_enabled.unwrap_or(defaults::PET_ENABLED) {
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
            let opener = layers
                .iter()
                .position(|layer| layer.name == "more")
                .unwrap_or(home);
            let opener_layer = &mut layers[opener];
            let has_pet_button = opener_layer.keys.iter().any(|button| {
                button.action.layer_target() == Some(PET_LAYER)
                    && button.text.as_deref().map_or(
                        button.icon.is_some() || button.time.is_some() || button.battery.is_some(),
                        |text| !text.trim().is_empty(),
                    )
            });
            if !has_pet_button {
                let at = if opener_layer.name == "more" {
                    opener_layer.keys.len()
                } else {
                    opener_layer
                        .keys
                        .iter()
                        .rposition(|button| matches!(button.action, ButtonAction::SwitchLayer(_)))
                        .map_or(opener_layer.keys.len(), |last| last + 1)
                };
                opener_layer.keys.insert(
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
        // replaces the synthesized picker. Theme activation buttons are
        // opt-in and must be configured on a bar by the author.
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
                validate_button(&layer.name, button)?;
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

/// Validate one button's content knobs, with the owning layer named in
/// every message. These checks used to be unreachable-code panics or
/// silent clamps inside the daemon's renderer; they are configuration
/// errors and reject the candidate here, before the daemon ever
/// switches to it.
fn validate_button(layer: &str, button: &ButtonConfig) -> Result<()> {
    if let Some(stretch) = button.stretch {
        if stretch < defaults::STRETCH_MIN {
            bail!(
                "layer {layer:?} has Stretch = {stretch}; Stretch must be at least {}",
                defaults::STRETCH_MIN
            );
        }
    }
    for (knob, dim) in [
        ("IconWidth", button.icon_width),
        ("IconHeight", button.icon_height),
    ] {
        if let Some(dim) = dim {
            if !(defaults::ICON_DIM_MIN..=defaults::ICON_DIM_MAX).contains(&dim) {
                bail!(
                    "layer {layer:?} has {knob} = {dim}; the icon dimensions must be {}-{}",
                    defaults::ICON_DIM_MIN,
                    defaults::ICON_DIM_MAX
                );
            }
        }
    }
    for (enabled, style, knob) in [
        (
            button.waveform,
            button.waveform_style.as_deref(),
            "WaveformStyle",
        ),
        (
            button.visualizer,
            button.visualizer_style.as_deref(),
            "VisualizerStyle",
        ),
    ] {
        if enabled.unwrap_or(false) {
            if let Some(style) = style {
                if !WAVEFORM_STYLES.contains(&style) {
                    bail!(
                        "layer {layer:?} has {knob} = {style:?}; the accepted styles are {}",
                        WAVEFORM_STYLES.join(", ")
                    );
                }
            }
        }
    }
    for (enabled, lag, knob) in [
        (button.waveform, button.waveform_lag, "WaveformLag"),
        (button.visualizer, button.visualizer_lag, "VisualizerLag"),
    ] {
        if enabled.unwrap_or(false) {
            if let Some(lag) = lag {
                if lag > defaults::MAX_WAVEFORM_LAG_MS {
                    bail!(
                        "layer {layer:?} has {knob} = {lag} ms; the visual lag is capped at {} ms",
                        defaults::MAX_WAVEFORM_LAG_MS
                    );
                }
            }
        }
    }
    for (enabled, smoothing, knob) in [
        (
            button.waveform,
            button.waveform_smoothing,
            "WaveformSmoothing",
        ),
        (
            button.visualizer,
            button.visualizer_smoothing,
            "VisualizerSmoothing",
        ),
    ] {
        if enabled.unwrap_or(false) {
            if let Some(smoothing) = smoothing {
                if smoothing > defaults::MAX_WAVEFORM_SMOOTHING_MS {
                    bail!(
                        "layer {layer:?} has {knob} = {smoothing} ms; the motion smoothing is capped at {} ms",
                        defaults::MAX_WAVEFORM_SMOOTHING_MS
                    );
                }
            }
        }
    }
    if let Some(mode) = &button.battery {
        if !BATTERY_MODES.contains(&mode.as_str()) {
            bail!(
                "layer {layer:?} has Battery = {mode:?}; the accepted modes are {}",
                BATTERY_MODES.join(", ")
            );
        }
    }
    if let Some(time) = &button.time {
        if let Err(err) = time_format_items(time) {
            bail!("layer {layer:?} has an invalid Time format: {err}");
        }
    }
    Ok(())
}

/// Resolve a button `Time` knob to the format items the clock renders
/// with, exactly as the daemon's button builder does: the two named
/// spellings expand to the shipped formats, anything else is a chrono
/// strftime string and must parse.
pub fn time_format_items(format: &str) -> Result<Vec<chrono::format::Item<'static>>, String> {
    let format_str = if format == "24hr" {
        "%H:%M    %a %-e %b"
    } else if format == "12hr" {
        "%-l:%M %p    %a %-e %b"
    } else {
        format
    };
    chrono::format::StrftimeItems::new(format_str)
        .parse_to_owned()
        .map_err(|err| format!("{err:?} (in {format:?})"))
}
