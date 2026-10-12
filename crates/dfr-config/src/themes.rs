//! Named color themes: presets of the bar's existing color knobs.
//!
//! A theme names a set of accent overrides - the colors behind the
//! semantic accent names every button already uses - and optionally a
//! full set of time-of-day background bands. Applying a theme swaps
//! those knobs live: every button fill, outline, slider fill, and strip
//! band repaints on the next frame, and nothing about layout, touch
//! behavior, or the daemon's process changes. Themes that omit a knob
//! inherit it (a theme without bands keeps the configured day cycle;
//! a theme without an accent entry keeps that accent's shipped color),
//! so a theme can be as small as one accent or as complete as the whole
//! palette plus all four bands.
//!
//! The seeded built-ins are defined in the same TOML shape a user's
//! `[Themes.Name]` tables use, so the shipped set is validated by the
//! same code path and can be shown verbatim in the documentation. The
//! first built-in, `Default`, carries no overrides at all: it is the
//! live palette exactly as the deck has always rendered it.

use crate::daycycle::{DayCycle, TimeBand};
use crate::{parse_bands, BandConfig, ButtonAccent, Palette};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// Name of the synthesized theme-picker layer. A user-defined layer
/// with this name replaces the synthesized picker; the quick-keys
/// `more` bar's synthesized buttons then open the user's layer.
pub const THEMES_LAYER: &str = "themes";

/// The picker lists one button per theme beside its Home button, and
/// the escape key may claim one more slot on wide bars; rendering starts
/// to break around 24 keys, so the theme list is capped to keep every
/// name reachable without scrolling.
pub const MAX_THEMES: usize = 21;

/// One resolved theme definition: parsed accent colors and optional
/// background bands, ready to apply without touching the configuration
/// files again.
#[derive(Clone, Debug, Default)]
pub struct Theme {
    pub accents: BTreeMap<ButtonAccent, (u8, u8, u8)>,
    pub bands: Option<Vec<TimeBand>>,
}

/// The ordered theme list, the active choice, and whether the theme
/// system is enabled at all. The display order is the seed order of the
/// built-ins with user-defined themes appended alphabetically; the
/// cycle button walks that order and wraps.
#[derive(Clone, Debug, Default)]
pub struct ThemeSet {
    pub names: Vec<String>,
    themes: HashMap<String, Theme>,
    pub enabled: bool,
    pub active: Option<String>,
}

/// What a theme button asks the daemon to apply: the next theme in the
/// cycle, or one theme by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeRequest<'a> {
    Next,
    Named(&'a str),
}

impl ThemeSet {
    /// Resolve the seed built-ins overlaid with the user's `[Themes]`
    /// tables. A user theme with a built-in's name replaces that
    /// built-in in place; new names append in table (alphabetical)
    /// order.
    pub fn resolve(enabled: bool, user: Option<BTreeMap<String, ThemeConfig>>) -> Result<Self> {
        let mut names = Vec::new();
        let mut themes = HashMap::new();
        for theme in builtin_theme_configs() {
            validate_theme_name(&theme.name)?;
            names.push(theme.name.clone());
            themes.insert(
                theme.name.clone(),
                parse_theme(
                    &theme.accents,
                    &theme.day_cycle_bands,
                    &theme.name,
                    Theme::default(),
                )?,
            );
        }
        if let Some(user) = user {
            for (name, config) in user {
                validate_theme_name(&name)?;
                // A user theme shadowing a built-in inherits the
                // built-in's unspecified knobs, so overriding one accent
                // of a seeded theme keeps the rest of its character.
                let base = themes.get(&name).cloned().unwrap_or_default();
                let theme = parse_theme(&config.accents, &config.day_cycle_bands, &name, base)?;
                if !names.contains(&name) {
                    names.push(name.clone());
                }
                themes.insert(name, theme);
            }
        }
        if names.len() > MAX_THEMES {
            bail!(
                "{} themes exceed the picker bar's {}-button budget",
                names.len(),
                MAX_THEMES
            );
        }
        Ok(Self {
            names,
            themes,
            enabled,
            active: None,
        })
    }

    pub fn contains(&self, name: &str) -> bool {
        self.themes.contains_key(name)
    }

    /// The theme the cycle button applies next: the successor of the
    /// active theme, wrapping to the first; with nothing active, the
    /// first theme in display order.
    pub fn cycle_next(&self) -> Option<String> {
        if self.names.is_empty() {
            return None;
        }
        match &self.active {
            None => self.names.first().cloned(),
            Some(active) => {
                let index = self
                    .names
                    .iter()
                    .position(|name| name == active)
                    .unwrap_or(0);
                self.names.get((index + 1) % self.names.len()).cloned()
            }
        }
    }

    /// Apply a theme by name; returns whether the name was known and
    /// the system enabled. Unknown names leave the active choice alone.
    pub fn apply(&mut self, name: &str) -> bool {
        if !self.enabled || !self.contains(name) {
            return false;
        }
        self.active = Some(name.to_owned());
        true
    }

    /// The palette the bar should render with: the shipped live palette
    /// under the active theme's accent overrides. With the system
    /// disabled or nothing active this is exactly the shipped palette.
    pub fn palette(&self) -> Palette {
        let mut palette = Palette::default();
        if !self.enabled {
            return palette;
        }
        if let Some(theme) = self
            .active
            .as_deref()
            .and_then(|name| self.themes.get(name))
        {
            for (accent, color) in &theme.accents {
                palette.set(*accent, *color);
            }
        }
        palette
    }

    /// The day cycle the bar should render with: the configured global
    /// cycle, its bands replaced by the active theme's when it defines
    /// any. The enabled flag always follows the global setting, so a
    /// theme never reintroduces the background on a deck that disabled
    /// it. With the system disabled or a theme without bands this is
    /// the global cycle unchanged.
    pub fn day_cycle(&self, global: &DayCycle) -> DayCycle {
        if !self.enabled {
            return global.clone();
        }
        match self
            .active
            .as_deref()
            .and_then(|name| self.themes.get(name))
            .and_then(|theme| theme.bands.clone())
        {
            Some(bands) => DayCycle::new(global.enabled, bands),
            None => global.clone(),
        }
    }
}

/// A user theme name becomes a picker button label, so it must be a
/// nonempty, control-free string like every other label on the bar.
fn validate_theme_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        bail!("theme names must be nonempty");
    }
    if name.chars().any(char::is_control) {
        bail!("theme names must not contain control characters: {name:?}");
    }
    Ok(())
}

/// Validate one theme's accent colors and bands into their resolved
/// form, with the theme's name in every error message.
fn parse_theme(
    accents: &Option<BTreeMap<ButtonAccent, String>>,
    bands: &Option<Vec<BandConfig>>,
    name: &str,
    base: Theme,
) -> Result<Theme> {
    let mut resolved = base.accents;
    if let Some(accents) = accents {
        for (accent, color) in accents {
            let color = crate::daycycle::parse_hex_color(color)
                .map_err(|err| err.context(format!("theme {name:?} accent {accent:?}")))?;
            resolved.insert(*accent, color);
        }
    }
    let bands = match bands {
        Some(bands) => {
            Some(parse_bands(bands).map_err(|err| err.context(format!("theme {name:?}")))?)
        }
        None => base.bands,
    };
    Ok(Theme {
        accents: resolved,
        bands,
    })
}

/// The user-facing `[Themes.Name]` table: optional accent overrides and
/// optional background bands, both validated on resolve. Defined here so
/// the shape lives beside the rest of the theme system.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub struct ThemeConfig {
    pub accents: Option<BTreeMap<ButtonAccent, String>>,
    pub day_cycle_bands: Option<Vec<BandConfig>>,
}

/// The built-ins' array-of-tables shape, which preserves seed order.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct NamedThemeConfig {
    name: String,
    accents: Option<BTreeMap<ButtonAccent, String>>,
    day_cycle_bands: Option<Vec<BandConfig>>,
}

/// The seeded themes, in cycle and picker order. `Default` is the live
/// palette with no overrides; the rest are complete presets of the
/// accent palette and the four time-of-day bands. Every accent keeps
/// enough brightness for white labels and dark fills, and the semantic
/// families (cyan audio, amber brightness, violet navigation, warning
/// orange, critical red, charging green) stay pairwise distinct so the
/// deck's color language survives every preset.
const BUILTIN_THEMES: &str = include_str!("../../../share/tiny-dfr/themes.toml");

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct BuiltinFile {
    theme: Vec<NamedThemeConfig>,
}

fn builtin_theme_configs() -> Vec<NamedThemeConfig> {
    let themes: BuiltinFile =
        toml::from_str(BUILTIN_THEMES).expect("the built-in theme definitions must parse");
    themes.theme
}

/// Resolve the full built-in set (used by tests to pin the seed list).
pub fn builtin_themes() -> Result<ThemeSet> {
    ThemeSet::resolve(true, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daycycle::DayCycle;

    /// A `[Themes.Name]` configuration document: parse the `Themes`
    /// table the same field shape the daemon's config proxy sees.
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct ThemesDoc {
        themes: BTreeMap<String, ThemeConfig>,
    }
    fn user_theme(toml: &str) -> BTreeMap<String, ThemeConfig> {
        toml::from_str::<ThemesDoc>(toml).unwrap().themes
    }

    #[test]
    fn the_seeded_set_carries_the_live_palette_first_and_cycles_with_wrap() {
        let themes = builtin_themes().unwrap();
        let names = themes.names.clone();
        assert_eq!(
            names,
            [
                "Default",
                "Midnight",
                "Nord",
                "Ember",
                "Rosewater",
                "Forest",
                "Solarized",
                "Mono",
            ]
        );
        // The live palette ships as the first theme with no overrides of
        // its own: it renders exactly what the deck rendered before themes.
        let global = DayCycle::default();
        let mut applied = themes.clone();
        applied.apply("Default");
        assert_eq!(applied.palette(), Palette::default());
        assert_eq!(
            applied.day_cycle(&global).background(14),
            global.background(14)
        );
        // Every other seed overrides something: the palette moves and the
        // bands move.
        for name in &names[1..] {
            applied.apply(name);
            assert_ne!(
                applied.palette(),
                Palette::default(),
                "{name} shifts at least one accent"
            );
            assert_ne!(
                applied.day_cycle(&global).background(14),
                global.background(14),
                "{name} repaints the strip"
            );
        }
        // The cycle walks the display order and wraps back to Default.
        let mut themes = builtin_themes().unwrap();
        assert_eq!(themes.cycle_next().as_deref(), Some("Default"));
        for (from, next) in [
            ("Default", "Midnight"),
            ("Midnight", "Nord"),
            ("Nord", "Ember"),
            ("Ember", "Rosewater"),
            ("Rosewater", "Forest"),
            ("Forest", "Solarized"),
            ("Solarized", "Mono"),
            ("Mono", "Default"),
        ] {
            themes.active = Some(from.into());
            assert_eq!(themes.cycle_next().as_deref(), Some(next), "after {from}");
        }
    }

    #[test]
    fn every_seeded_accent_stays_readably_bright_and_the_families_stay_distinct() {
        let themes = builtin_themes().unwrap();
        let names = themes.names.clone();
        let mut applied = themes.clone();
        for name in &names {
            applied.apply(name);
            let palette = applied.palette();
            for accent in [
                ButtonAccent::Neutral,
                ButtonAccent::Cyan,
                ButtonAccent::Amber,
                ButtonAccent::Blue,
                ButtonAccent::Violet,
                ButtonAccent::Orange,
                ButtonAccent::Red,
                ButtonAccent::Green,
            ] {
                let (r, g, b) = palette.accent(accent);
                let luminance = 0.2126 * r + 0.7152 * g + 0.0722 * b;
                assert!(
                    (0.3..=1.0).contains(&luminance),
                    "{name} {accent:?} too dim at {luminance}"
                );
            }
            // The deck's color language survives every preset: audio cyan,
            // brightness amber, navigation violet, and the two escalation
            // tiers stay pairwise distinguishable by hue distance. Mono is
            // the deliberate exception - its identity is one grayscale
            // ladder with the escalations at the brightest end - so it is
            // exempt from the hue check but not the brightness one.
            let distance = |a: ButtonAccent, b: ButtonAccent| {
                let (ar, ag, ab) = palette.accent(a);
                let (br, bg, bb) = palette.accent(b);
                ((ar - br).powi(2) + (ag - bg).powi(2) + (ab - bb).powi(2)).sqrt()
            };
            if name != "Mono" {
                for (a, b) in [
                    (ButtonAccent::Cyan, ButtonAccent::Amber),
                    (ButtonAccent::Cyan, ButtonAccent::Violet),
                    (ButtonAccent::Amber, ButtonAccent::Violet),
                    (ButtonAccent::Orange, ButtonAccent::Red),
                    (ButtonAccent::Green, ButtonAccent::Amber),
                ] {
                    assert!(
                        distance(a, b) >= 0.25,
                        "{name} {a:?} vs {b:?} read as the same hue"
                    );
                }
            }
        }
    }

    #[test]
    fn user_themes_merge_by_name_append_alphabetically_and_can_be_partial() {
        let config = user_theme(
            r##"
[Themes.Zephyr.Accents]
cyan = "#40E0D0"
[Themes.Midnight.Accents]
neutral = "#001122"
[Themes.Aloe]
[[Themes.Aloe.DayCycleBands]]
Name = "day"
StartHour = 12
Color = "#002211"
"##,
        );
        let mut themes = ThemeSet::resolve(true, Some(config)).unwrap();
        // A user theme with a built-in's name overrides it in place; new
        // names append in table (alphabetical) order.
        assert_eq!(
            themes.names,
            [
                "Default",
                "Midnight",
                "Nord",
                "Ember",
                "Rosewater",
                "Forest",
                "Solarized",
                "Mono",
                "Aloe",
                "Zephyr",
            ]
        );
        // Partial themes inherit every knob they do not name.
        themes.apply("Zephyr");
        let palette = themes.palette();
        assert_eq!(
            palette.accent(ButtonAccent::Cyan),
            (64.0 / 255.0, 224.0 / 255.0, 208.0 / 255.0)
        );
        assert_eq!(
            palette.accent(ButtonAccent::Amber),
            ButtonAccent::Amber.rgb(),
            "an unnamed accent keeps its shipped color"
        );
        let global = DayCycle::default();
        assert_eq!(
            themes.day_cycle(&global).background(14),
            global.background(14),
            "a theme without bands keeps the configured day cycle"
        );
        // The overridden Midnight keeps its seed position and bands, with
        // only the named accent replaced.
        themes.apply("Midnight");
        assert_eq!(
            themes.palette().accent(ButtonAccent::Neutral),
            (0.0, 17.0 / 255.0, 34.0 / 255.0)
        );
        assert_ne!(
            themes.day_cycle(&global).background(14),
            global.background(14),
            "Midnight's seed bands survive the accent override"
        );
        // A bands-only theme moves only the strip.
        themes.apply("Aloe");
        assert_eq!(themes.palette(), Palette::default());
        assert_eq!(
            themes.day_cycle(&global).background(14),
            (0.0, 34.0 / 255.0, 17.0 / 255.0)
        );
        // A disabled set is inert no matter what it holds.
        let mut themes = ThemeSet::resolve(false, None).unwrap();
        assert!(!themes.apply("Midnight"));
        assert_eq!(themes.palette(), Palette::default());
        let mut disabled = themes.clone();
        disabled.active = Some("Midnight".into());
        assert_eq!(disabled.palette(), Palette::default());
        assert_eq!(
            disabled.day_cycle(&global).background(2),
            global.background(2)
        );
    }

    #[test]
    fn invalid_theme_definitions_reject_the_candidate_set() {
        for invalid in [
            // Unknown accent name as a map key.
            "[Themes.Bad.Accents]\nmauve = \"#101010\"\n",
            // Bad hex, hash or no hash.
            "[Themes.Bad.Accents]\ncyan = \"#10g010\"\n",
            "[Themes.Bad.Accents]\ncyan = \"1010\"\n",
            // Non-string accent value.
            "[Themes.Bad.Accents]\ncyan = 42\n",
            // Invalid band entries ride the same rules as the global bands.
            "[Themes.Bad]\n[[Themes.Bad.DayCycleBands]]\nName = \"x\"\nStartHour = 24\nColor = \"#101010\"\n",
            "[Themes.Bad]\n[[Themes.Bad.DayCycleBands]]\nName = \"x\"\nStartHour = 5\nColor = \"#1010zz\"\n",
            "[Themes.Bad]\nDayCycleBands = []\n",
            // Unknown field in the theme table.
            "[Themes.Bad]\nAccent = \"cyan\"\n",
            // Blank name.
            "[Themes.\"  \"]\n",
        ] {
            let resolved = std::panic::catch_unwind(|| user_theme(invalid));
            let resolved = match resolved {
                Err(_) => None,
                Ok(user) => ThemeSet::resolve(true, Some(user)).ok(),
            };
            assert!(
                resolved.is_none(),
                "accepted invalid theme definition {invalid:?}"
            );
        }
        // The picker bar's button budget caps the theme count.
        let mut many = String::new();
        for i in 0..(MAX_THEMES - 8) {
            many.push_str(&format!("[Themes.T{i}]\n"));
        }
        let user = user_theme(&many);
        assert!(ThemeSet::resolve(true, Some(user)).is_ok());
        many.push_str("[Themes.T9999]\n");
        let user = user_theme(&many);
        assert!(ThemeSet::resolve(true, Some(user)).is_err());
    }
}
