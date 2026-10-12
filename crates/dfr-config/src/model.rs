//! Parse-and-validate: the exact path every configuration takes before
//! the daemon (or any tool) may act on it. Base and user documents are
//! deserialized with unknown fields denied, merged field by field,
//! range-checked, and resolved into the layer layout, day cycle, and
//! theme set. Errors carry the file they came from, the TOML parser's
//! line and column where it has one, and a suggested knob name for
//! near-miss typos.

use crate::appearance::ButtonAccent;
use crate::daycycle::DayCycle;
use crate::defaults;
use crate::layout::LayerLayout;
use crate::pets::{PetEnergy, PetVariant};
use crate::proxy::{nearest_knob_name, ConfigProxy};
use crate::themes::ThemeSet;
use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;

/// A fully validated configuration model: the merged proxy, the
/// synthesized layer layout, and the derived day cycle and theme set.
/// Everything here is deterministic in the input text alone; loading
/// fonts and icon files is the caller's step, so this model is what
/// offline tools (and the future configurator) compute.
pub struct LoadedModel {
    pub proxy: ConfigProxy,
    pub layout: LayerLayout,
    pub day_cycle: DayCycle,
    pub themes: ThemeSet,
}

/// Parse and validate a base document, optionally overlaid with a user
/// document, at the bar width the layout is synthesized for. The
/// `what` labels name the two documents in error messages.
pub fn parse_config(base: &str, user: Option<&str>, width: u16) -> Result<LoadedModel> {
    parse_config_named(base, "base", user.map(|user| (user, "user")), width)
}

/// The same path over files: reads both documents, tolerating a missing
/// user file exactly like the daemon, and names the files in errors.
pub fn parse_config_files(
    base_path: &Path,
    user_path: Option<&Path>,
    width: u16,
) -> Result<LoadedModel> {
    let base_label = base_path.display().to_string();
    let base = std::fs::read_to_string(base_path)
        .with_context(|| format!("could not read the base configuration {base_label}"))?;
    let user_label = user_path.map(|path| path.display().to_string());
    let user = match user_path.map(std::fs::read_to_string) {
        None => None,
        Some(Ok(user)) => Some(user),
        Some(Err(err)) if err.kind() == std::io::ErrorKind::NotFound => None,
        Some(Err(err)) => {
            return Err(anyhow!(err)).with_context(|| {
                format!(
                    "could not read the user configuration {}",
                    user_label.unwrap()
                )
            })
        }
    };
    let user_pair = match (user, user_label) {
        (Some(user), Some(label)) => Some((user, label)),
        _ => None,
    };
    let user = user_pair
        .as_ref()
        .map(|(user, label)| (user.as_str(), label.as_str()));
    parse_config_named(&base, &base_label, user, width)
}

fn parse_config_named(
    base: &str,
    base_label: &str,
    user: Option<(&str, &str)>,
    width: u16,
) -> Result<LoadedModel> {
    let mut base: ConfigProxy = parse_proxy(base, base_label)?;
    if let Some((user, label)) = user {
        base.merge(parse_proxy(user, label)?);
    }
    if base
        .battery_cycle_timeout_secs
        .is_some_and(|value| value == 0 || value > defaults::BATTERY_CYCLE_TIMEOUT_SECS_MAX)
    {
        bail!(
            "BatteryCycleTimeoutSecs must be 1-{} seconds, got {}",
            defaults::BATTERY_CYCLE_TIMEOUT_SECS_MAX,
            base.battery_cycle_timeout_secs.unwrap()
        );
    }
    if base
        .battery_warning_threshold
        .is_some_and(|value| value > defaults::BATTERY_THRESHOLD_MAX)
    {
        bail!("BatteryWarningThreshold must be at most 100");
    }
    if base
        .battery_critical_threshold
        .is_some_and(|value| value > defaults::BATTERY_THRESHOLD_MAX)
    {
        bail!("BatteryCriticalThreshold must be at most 100");
    }
    if base
        .battery_critical_threshold
        .unwrap_or(defaults::BATTERY_CRITICAL_THRESHOLD)
        > base
            .battery_warning_threshold
            .unwrap_or(defaults::BATTERY_WARNING_THRESHOLD)
    {
        bail!("BatteryCriticalThreshold must not exceed BatteryWarningThreshold");
    }
    if base
        .active_brightness
        .is_some_and(|value| value > defaults::ACTIVE_BRIGHTNESS_MAX)
    {
        bail!(
            "ActiveBrightness must be 0-{}, got {}",
            defaults::ACTIVE_BRIGHTNESS_MAX,
            base.active_brightness.unwrap()
        );
    }
    if let Some(variant) = base.pet_variant.as_deref() {
        PetVariant::parse(variant).map_err(|err| anyhow!(err))?;
    }
    if let Some(energy) = base.pet_energy.as_deref() {
        PetEnergy::parse(energy).map_err(|err| anyhow!(err))?;
    }
    let day_cycle = base.day_cycle()?;
    let themes = ThemeSet::resolve(
        base.themes_enabled.unwrap_or(defaults::THEMES_ENABLED),
        base.themes.clone(),
    )?;
    if let Some(active) = base.active_theme.as_deref() {
        if active.trim().is_empty() {
            bail!("ActiveTheme must not be empty");
        }
        if !themes.contains(active) {
            bail!("ActiveTheme {active:?} is not a defined theme");
        }
    }
    let layout = base.layer_layout(width, &themes)?;
    Ok(LoadedModel {
        proxy: base,
        layout,
        day_cycle,
        themes,
    })
}

/// Deserialize one configuration document with the friendly-error
/// wrapper: parse errors keep the TOML parser's line and column, and
/// an unknown field gains a did-you-mean suggestion when a known knob
/// name is close.
fn parse_proxy(text: &str, label: &str) -> Result<ConfigProxy> {
    toml::from_str(text).map_err(|err| {
        let message = err.to_string();
        let suggestion = unknown_field_name(&message)
            .and_then(nearest_knob_name)
            .map(|near| format!("  did you mean `{near}`?\n"))
            .unwrap_or_default();
        anyhow!("invalid configuration in {label}:\n{message}\n{suggestion}")
    })
}

/// Extract the field name from serde's `unknown field \`X\`` message,
/// when the message is one.
fn unknown_field_name(message: &str) -> Option<&str> {
    let start = message.find("unknown field `")? + "unknown field `".len();
    let rest = &message[start..];
    let end = rest.find('`')?;
    Some(&rest[..end])
}

/// The accent names accepted wherever a knob names a color, for
/// documentation and tooling; parsing itself is serde's.
pub fn accent_names() -> &'static [&'static str] {
    &[
        "neutral", "cyan", "amber", "blue", "violet", "orange", "red", "green",
    ]
}

/// Convenience for tools resolving a knob's accent default.
pub fn accent_name(accent: ButtonAccent) -> &'static str {
    accent.name()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::defaults;

    const BASE: &str = include_str!("../../../share/tiny-dfr/config.toml");
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

    fn err(user: &str) -> String {
        parse_config(BASE, Some(user), 2008)
            .map(|_| ())
            .unwrap_err()
            .chain()
            .map(|cause| cause.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn unknown_fields_reject_with_a_near_miss_suggestion_and_the_line() {
        // A top-level typo names the line and suggests the knob.
        let error = err("ShowButtonOutline = false");
        assert!(
            error.contains("unknown field `ShowButtonOutline`"),
            "{error}"
        );
        assert!(
            error.contains("did you mean `ShowButtonOutlines`"),
            "{error}"
        );
        assert!(
            error.contains("line 1"),
            "the parser's line rides along: {error}"
        );
        // A button typo suggests the button knob.
        let error = err(NAMED.replace("Text = \"F1\"", "Tex = \"F1\"").as_str());
        assert!(error.contains("unknown field `Tex`"), "{error}");
        assert!(error.contains("did you mean `Text`"), "{error}");
        // An unknown name with no close match still rejects, without a
        // bogus suggestion.
        let error = err("Zzzz = 1");
        assert!(error.contains("unknown field"), "{error}");
        assert!(!error.contains("did you mean"), "{error}");
        // The Svg alias still parses as Icon.
        let aliased = NAMED.replace("Text = \"F1\"", "Svg = \"search\"");
        let model = parse_config(BASE, Some(&aliased), 2008).unwrap();
        let button = &model.layout.layers[1].keys.last().unwrap();
        assert_eq!(button.icon.as_deref(), Some("search"));
    }

    #[test]
    fn documented_ranges_are_enforced_at_parse_time() {
        for (knob, value) in [
            ("ActiveBrightness", "256"),
            ("BatteryCycleTimeoutSecs", "3601"),
            ("BatteryCycleTimeoutSecs", "0"),
        ] {
            let error = err(&format!("{knob} = {value}"));
            assert!(error.contains(knob), "{error}");
        }
        // The boundary values themselves stay valid.
        parse_config(BASE, Some("ActiveBrightness = 255"), 2008).unwrap();
        parse_config(BASE, Some("BatteryCycleTimeoutSecs = 3600"), 2008).unwrap();
    }

    #[test]
    fn button_knob_ranges_reject_the_candidate() {
        let button = |extra: &str| {
            NAMED.replace(
                "Text = \"F1\", Action = \"F1\"",
                &format!("Text = \"F1\", Action = \"F1\", {extra}"),
            )
        };
        for invalid in [
            button("Stretch = 0"),
            button("IconWidth = 7"),
            button("IconWidth = 257"),
            button("IconHeight = 0"),
            button("Waveform = true, WaveformStyle = \"dots\""),
            button("Visualizer = true, VisualizerStyle = \"dots\""),
            button("Waveform = true, WaveformLag = 501"),
            button("Visualizer = true, VisualizerLag = 501"),
            button("Waveform = true, WaveformSmoothing = 5001"),
            button("Visualizer = true, VisualizerSmoothing = 5001"),
            button("Battery = \"icons\""),
            button("Time = \"abc%\""),
        ] {
            assert!(
                parse_config(BASE, Some(&invalid), 2008).is_err(),
                "accepted invalid button configuration: {invalid}"
            );
        }
        // Boundaries stay valid, and inert knobs without their feature
        // stay unvalidated exactly as before.
        parse_config(
            BASE,
            Some(&button("Stretch = 1, IconWidth = 8, IconHeight = 256")),
            2008,
        )
        .unwrap();
        parse_config(
            BASE,
            Some(&button("WaveformLag = 501, WaveformStyle = \"dots\"")),
            2008,
        )
        .unwrap();
    }

    #[test]
    fn legacy_button_lists_enforce_the_same_ranges() {
        let legacy = "PrimaryLayerKeys = [{ Text = \"F1\", Stretch = 0 }]\nMediaLayerKeys = [{ Text = \"V\", Action = \"VolumeUp\" }]\n";
        assert!(parse_config(BASE, Some(legacy), 2008).is_err());
    }

    #[test]
    fn error_messages_name_the_offending_file() {
        let tmp =
            std::env::temp_dir().join(format!("dfr-config-model-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let base = tmp.join("base.toml");
        let user = tmp.join("config.toml");
        std::fs::write(&base, BASE).unwrap();
        // A parse error names the file it came from.
        std::fs::write(&user, "Zzz = 1\n").unwrap();
        let error = parse_config_files(&base, Some(&user), 2008)
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(error.contains("config.toml"), "{error}");
        // A range error names the knob; the caller (check, the daemon's
        // reload log) prefixes the file it is loading.
        std::fs::write(&user, "ActiveBrightness = 999\n").unwrap();
        let error = parse_config_files(&base, Some(&user), 2008)
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(error.contains("ActiveBrightness"), "{error}");
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn a_stripped_base_with_no_globals_still_parses() {
        // Every knob has a code-level default, so a base reduced to the
        // layer lists loads without the historical unwraps.
        let stripped = "PrimaryLayerKeys = [{ Text = \"F1\", Action = \"F1\" }]\nMediaLayerKeys = [{ Text = \"V\", Action = \"VolumeUp\" }]\n";
        let model = parse_config(stripped, None, 2008).unwrap();
        assert_eq!(model.day_cycle.enabled, defaults::DAY_CYCLE_ENABLED);
        assert_eq!(
            model.proxy.active_brightness, None,
            "the default applies at resolution, not by inventing a value"
        );
    }
}
