//! The bar background follows the time of day. The strip behind every
//! layer and widget - the plain black surface the buttons, sliders, and
//! pet scene sit on - takes one of a small set of band colors selected
//! by the local hour: a warm dawn gray in the morning, a brighter
//! neutral slate at midday, warm amber toward evening, and a dark cool
//! blue overnight. A band change repaints once on the daemon's existing
//! wake cadence; there is no per-frame animation, and key fills,
//! accents, sliders, and the pet's own sky and ground scene are drawn
//! unchanged on top of the band color.

use anyhow::bail;

/// The plain background used before the day cycle and whenever it is
/// disabled; the strip every other surface is painted over.
pub const BLACK: (f64, f64, f64) = (0.0, 0.0, 0.0);

/// One time band of the day cycle: the background color for every local
/// hour from `start_hour` until the next band starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeBand {
    pub name: String,
    pub start_hour: u8,
    pub color: (u8, u8, u8),
}

impl TimeBand {
    fn new(name: &str, start_hour: u8, color: (u8, u8, u8)) -> Self {
        Self {
            name: name.to_owned(),
            start_hour,
            color,
        }
    }

    pub fn rgb(&self) -> (f64, f64, f64) {
        (
            f64::from(self.color.0) / 255.0,
            f64::from(self.color.1) / 255.0,
            f64::from(self.color.2) / 255.0,
        )
    }
}

/// The configured set of bands, ordered by start hour.
#[derive(Clone)]
pub struct DayCycle {
    pub enabled: bool,
    bands: Vec<TimeBand>,
}

impl DayCycle {
    /// The shipped defaults: morning, day, evening, and night tints that
    /// stay inside the deck's neutral surface palette - dark enough that
    /// white labels, key fills, and the slider keep their contrast, tinted
    /// enough that the strip reads as part of the day.
    pub fn default_bands() -> Vec<TimeBand> {
        vec![
            // Warm dawn gray.
            TimeBand::new("morning", 5, (0x16, 0x10, 0x0C)),
            // Brighter neutral slate at midday.
            TimeBand::new("day", 12, (0x1A, 0x1B, 0x1E)),
            // Warm amber dusk.
            TimeBand::new("evening", 17, (0x17, 0x0D, 0x07)),
            // Dark cool blue overnight.
            TimeBand::new("night", 21, (0x04, 0x06, 0x0D)),
        ]
    }

    pub fn new(enabled: bool, bands: Vec<TimeBand>) -> Self {
        let mut bands = bands;
        bands.sort_by_key(|band| band.start_hour);
        Self { enabled, bands }
    }

    /// The plain pre-day-cycle background: no bands, always black.
    #[cfg(test)]
    pub fn disabled() -> Self {
        Self::new(false, Vec::new())
    }

    /// The band covering `hour` (local, 0-23): the last band that starts
    /// at or before it, wrapping past midnight to the final band. `None`
    /// when the cycle is disabled.
    pub fn band_at(&self, hour: u32) -> Option<&TimeBand> {
        if !self.enabled || self.bands.is_empty() {
            return None;
        }
        let preceding = self
            .bands
            .partition_point(|band| u32::from(band.start_hour) <= hour);
        Some(match preceding {
            0 => self.bands.last().unwrap(),
            index => &self.bands[index - 1],
        })
    }

    /// The background color for `hour`: the band color, or the plain
    /// black background when the cycle is disabled.
    pub fn background(&self, hour: u32) -> (f64, f64, f64) {
        self.band_at(hour).map_or(BLACK, TimeBand::rgb)
    }
}

impl Default for DayCycle {
    fn default() -> Self {
        Self::new(true, Self::default_bands())
    }
}

/// Parse a `"#RRGGBB"` (or bare `RRGGBB`) color literal.
pub fn parse_hex_color(text: &str) -> anyhow::Result<(u8, u8, u8)> {
    let hex = text.strip_prefix('#').unwrap_or(text);
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Color must be six hex digits like \"#1A1B1E\", got {text:?}");
    }
    let channel = |slice: &str| u8::from_str_radix(slice, 16).expect("validated hex digit pair");
    Ok((
        channel(&hex[0..2]),
        channel(&hex[2..4]),
        channel(&hex[4..6]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::RangeInclusive;

    fn band_names(cycle: &DayCycle, hours: RangeInclusive<u32>) -> Vec<&str> {
        hours
            .map(|hour| cycle.band_at(hour).unwrap().name.as_str())
            .collect()
    }

    #[test]
    fn default_bands_select_by_local_hour_with_midnight_wrap() {
        let cycle = DayCycle::default();
        assert_eq!(
            band_names(&cycle, 0..=4),
            vec!["night"; 5],
            "the small hours wrap around midnight to the last band"
        );
        assert_eq!(band_names(&cycle, 5..=11), vec!["morning"; 7]);
        assert_eq!(band_names(&cycle, 12..=16), vec!["day"; 5]);
        assert_eq!(band_names(&cycle, 17..=20), vec!["evening"; 4]);
        assert_eq!(band_names(&cycle, 21..=23), vec!["night"; 3]);
        // The exact boundary hours open each band.
        for (hour, name) in [
            (4, "night"),
            (5, "morning"),
            (11, "morning"),
            (12, "day"),
            (16, "day"),
            (17, "evening"),
            (20, "evening"),
            (21, "night"),
            (0, "night"),
            (23, "night"),
        ] {
            assert_eq!(
                cycle.band_at(hour).unwrap().name,
                name,
                "hour {hour} must select the {name} band"
            );
        }
    }

    #[test]
    fn custom_bands_wrap_and_a_single_band_covers_the_whole_day() {
        let cycle = DayCycle::new(
            true,
            vec![
                TimeBand::new("late", 20, (1, 2, 3)),
                TimeBand::new("early", 8, (4, 5, 6)),
            ],
        );
        assert_eq!(band_names(&cycle, 0..=7), vec!["late"; 8]);
        assert_eq!(band_names(&cycle, 8..=19), vec!["early"; 12]);
        assert_eq!(band_names(&cycle, 20..=23), vec!["late"; 4]);
        // A lone band paints every hour, wrapping or not.
        let fixed = DayCycle::new(true, vec![TimeBand::new("always", 13, (7, 8, 9))]);
        for hour in 0..=23 {
            assert_eq!(fixed.band_at(hour).unwrap().name, "always");
        }
        let from_midnight = DayCycle::new(true, vec![TimeBand::new("always", 0, (7, 8, 9))]);
        for hour in 0..=23 {
            assert_eq!(from_midnight.band_at(hour).unwrap().name, "always");
        }
    }

    #[test]
    fn disabled_cycle_keeps_the_plain_black_background_at_every_hour() {
        let cycle = DayCycle::disabled();
        for hour in 0..=23 {
            assert_eq!(cycle.band_at(hour), None);
            assert_eq!(cycle.background(hour), BLACK);
        }
    }

    #[test]
    fn band_colors_convert_to_unit_rgb_for_cairo() {
        let cycle = DayCycle::default();
        let day = cycle.band_at(14).unwrap();
        assert_eq!(day.rgb(), (26.0 / 255.0, 27.0 / 255.0, 30.0 / 255.0));
        // Every default tint stays a dark, label-friendly surface: below
        // the neutral key fill so buttons and the slider keep contrast.
        for band in DayCycle::default_bands() {
            let (r, g, b) = band.rgb();
            assert!(r < 0.13 && g < 0.13 && b < 0.13, "{} too bright", band.name);
            if band.name == "night" {
                assert!(b > r, "the night tint stays cool");
            }
            if band.name == "evening" {
                assert!(r > b, "the evening tint stays warm");
            }
        }
    }

    #[test]
    fn hex_colors_parse_with_and_without_the_hash_and_reject_junk() {
        assert_eq!(parse_hex_color("#1A2b3C").unwrap(), (0x1A, 0x2B, 0x3C));
        assert_eq!(parse_hex_color("1a2B3c").unwrap(), (0x1A, 0x2B, 0x3C));
        assert_eq!(parse_hex_color("#000000").unwrap(), (0, 0, 0));
        for invalid in [
            "", "#", "1a2b", "#1a2b", "1a2b3", "#1a2b3", "1a2b3c4", "#1a2b3c4", "1a2b3g",
            "#1a2b3g", " 1a2b3c", "1a2b3c ", "#1a 2b3",
        ] {
            assert!(
                parse_hex_color(invalid).is_err(),
                "accepted invalid color {invalid:?}"
            );
        }
    }
}
