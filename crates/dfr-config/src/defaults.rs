//! Code-level defaults and enforced ranges for every knob. The shipped
//! base file is a fully commented *sample*; these constants are what
//! the daemon actually falls back to, so a stripped or older base
//! config loads without panicking and the schema is self-describing
//! without file fixtures. The knob registry and the generated README
//! reference render these values; tests pin them.

use crate::appearance::ButtonAccent;

// ---- Defaults (the value used when neither base nor user sets a knob).

pub const SHOW_BUTTON_OUTLINES: bool = true;
pub const ENABLE_PIXEL_SHIFT: bool = false;
pub const FONT_TEMPLATE: &str = ":bold";
pub const ADAPTIVE_BRIGHTNESS: bool = true;
pub const ACTIVE_BRIGHTNESS: u32 = 128;
pub const DOUBLE_PRESS_SWITCH_LAYERS: u32 = 0;
pub const BATTERY_CYCLE_ENABLED: bool = true;
pub const BATTERY_CYCLE_TIMEOUT_SECS: u32 = 15;
pub const BATTERY_WARNING_THRESHOLD: u32 = 25;
pub const BATTERY_CRITICAL_THRESHOLD: u32 = 10;
pub const BATTERY_WARNING_COLOR: ButtonAccent = ButtonAccent::Orange;
pub const BATTERY_CRITICAL_COLOR: ButtonAccent = ButtonAccent::Red;
pub const PET_ENABLED: bool = true;
pub const DAY_CYCLE_ENABLED: bool = true;
pub const THEMES_ENABLED: bool = true;

/// The icon size a button uses when it sets neither `IconWidth` nor
/// `IconHeight`. The daemon's renderer shares this constant.
pub const DEFAULT_ICON_SIZE: i32 = 40;

// ---- Enforced ranges.

/// `ActiveBrightness` is a backlight level: 0-255.
pub const ACTIVE_BRIGHTNESS_MAX: u32 = 255;
/// `Stretch` counts button spaces; a fraction of a space is not one.
pub const STRETCH_MIN: usize = 1;
/// Icon dimensions are pixels on a 60-pixel-tall strip; anything below
/// 8 or above 256 is a typo, not an intent.
pub const ICON_DIM_MIN: i32 = 8;
pub const ICON_DIM_MAX: i32 = 256;
/// The visual lag the waveform widgets can honor, in milliseconds. The
/// renderer always clamped here; the cap is enforced at parse time so
/// a larger value rejects the candidate instead of silently shrinking.
pub const MAX_WAVEFORM_LAG_MS: u32 = 500;
/// Motion smoothing beyond a few seconds makes the graph ignore its
/// input entirely, so the knob is capped at 5 seconds.
pub const MAX_WAVEFORM_SMOOTHING_MS: u32 = 5000;
/// The battery-cycle return timer in seconds: greater than zero (the
/// feature needs a timeout) and at most an hour (anything longer is a
/// forgotten knob, not a choice).
pub const BATTERY_CYCLE_TIMEOUT_SECS_MAX: u32 = 3600;
/// Battery thresholds are percentages.
pub const BATTERY_THRESHOLD_MAX: u32 = 100;
