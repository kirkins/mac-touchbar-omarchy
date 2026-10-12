//! Icon candidate resolution, shared by the daemon's loader and the
//! offline `check` path. The candidate list mirrors the daemon's
//! lookup order exactly, so validating here and loading there can
//! never disagree about where an icon comes from.

use crate::defaults::DEFAULT_ICON_SIZE;
use crate::layout::LayerLayout;
use crate::proxy::ButtonConfig;
use anyhow::{anyhow, Result};
use freedesktop_icons::lookup;
use std::path::PathBuf;

/// The fixed-path icon directories searched when a button sets no XDG
/// `Theme`, in priority order. The daemon passes `/etc/tiny-dfr` then
/// `/usr/share/tiny-dfr`; tools can point at a checkout's `share`
/// directory instead. XDG-theme lookups are resolved through the
/// desktop database and do not use these directories.
#[derive(Clone, Debug)]
pub struct IconSearch {
    pub dirs: Vec<PathBuf>,
}

impl IconSearch {
    pub fn new<I, P>(dirs: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        Self {
            dirs: dirs.into_iter().map(Into::into).collect(),
        }
    }

    /// The candidate files for `name`, in priority order: with an XDG
    /// theme set, the theme's SVG icons (icon-size-specific lookup
    /// first); without one, the fixed directories' `.svg` then `.png`.
    pub fn candidates(
        &self,
        name: &str,
        theme: Option<&str>,
        _icon_width: i32,
        icon_height: i32,
    ) -> Vec<PathBuf> {
        if let Some(theme) = theme {
            let candidates = vec![
                lookup(name)
                    .with_cache()
                    .with_theme(theme)
                    .with_size(icon_height as u16)
                    .force_svg()
                    .find(),
                lookup(name)
                    .with_cache()
                    .with_theme(theme)
                    .force_svg()
                    .find(),
            ];
            candidates.into_iter().flatten().collect()
        } else {
            let mut locations = Vec::new();
            for dir in &self.dirs {
                locations.push(dir.join(format!("{name}.svg")));
                locations.push(dir.join(format!("{name}.png")));
            }
            locations
        }
    }

    /// Every icon the battery widget preloads, whatever its display
    /// mode: the bolt, the plain ladder, and the charging ladder.
    pub fn battery_icon_names() -> &'static [&'static str] {
        &[
            "bolt",
            "battery_0_bar",
            "battery_1_bar",
            "battery_2_bar",
            "battery_3_bar",
            "battery_4_bar",
            "battery_5_bar",
            "battery_6_bar",
            "battery_full",
            "battery_charging_20",
            "battery_charging_30",
            "battery_charging_50",
            "battery_charging_60",
            "battery_charging_80",
            "battery_charging_90",
            "battery_charging_full",
        ]
    }

    /// Check that every button in the layout resolves to at least one
    /// existing icon file. This is the offline twin of the daemon's
    /// icon loading: a name that finds no file here would fail to load
    /// there, bricking the bar; here it is a diagnostic. All failures
    /// are collected, not just the first.
    pub fn validate_layout(&self, layout: &LayerLayout) -> Result<()> {
        let mut failures = Vec::new();
        for layer in &layout.layers {
            for (index, button) in layer.keys.iter().enumerate() {
                if let Some(icon) = &button.icon {
                    self.check_icon(layer.name.as_str(), index, button, icon, &mut failures);
                }
                if button.battery.is_some() {
                    for icon in Self::battery_icon_names() {
                        let mut probe = ButtonConfig {
                            theme: button.theme.clone(),
                            ..ButtonConfig::default()
                        };
                        probe.icon = Some((*icon).to_owned());
                        self.check_icon(layer.name.as_str(), index, &probe, icon, &mut failures);
                    }
                }
            }
        }
        if failures.is_empty() {
            return Ok(());
        }
        Err(anyhow!(
            "{} icon(s) in the configuration resolve to no file:\n  {}",
            failures.len(),
            failures.join("\n  ")
        ))
    }

    fn check_icon(
        &self,
        layer: &str,
        index: usize,
        button: &ButtonConfig,
        icon: &str,
        failures: &mut Vec<String>,
    ) {
        let width = button.icon_width.unwrap_or(DEFAULT_ICON_SIZE);
        let height = button.icon_height.unwrap_or(DEFAULT_ICON_SIZE);
        let candidates = self.candidates(icon, button.theme.as_deref(), width, height);
        if candidates.is_empty() || !candidates.iter().any(|path| path.is_file()) {
            let tried = candidates
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            failures.push(format!(
                "layer {layer:?} button {index} (Icon = {icon:?}{}): no candidate file exists (tried: {tried})",
                button
                    .theme
                    .as_deref()
                    .map(|theme| format!(", Theme = {theme:?}"))
                    .unwrap_or_default()
            ));
        }
    }
}
