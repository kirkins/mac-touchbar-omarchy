//! dfr-config: the tiny-dfr configuration model as a contract.
//!
//! The daemon, the `tiny-dfr check` subcommand, CI fixtures, and the
//! future GUI configurator all share this crate, so they cannot
//! disagree about what a configuration file means:
//!
//! - [`proxy`] holds the wire types: every table and key a base or user
//!   TOML document may contain, with unknown fields denied everywhere.
//! - [`model::parse_config`] is the exact parse, merge, validate, and
//!   layer-synthesis path; [`model::LoadedModel`] is its output.
//! - [`defaults`] owns every knob's code-level default and enforced
//!   range, so the shipped base file is a sample, not a dependency.
//! - [`registry`] is the machine-readable knob table the generated
//!   README reference renders from.
//! - [`writer::to_user_toml`] emits the canonical, commented spelling
//!   of a configuration; parsing its output yields an equal model.
//! - [`icons::IconSearch`] and [`fonts::match_font`] resolve the
//!   file-backed resources a configuration names, with the same lookup
//!   order the daemon loads through.
//!
//! The crate never opens the Touch Bar, never needs the daemon's
//! render stack, and hardcodes no absolute paths: callers pass their
//! own base/user files and icon directories in.

pub mod appearance;
pub mod check;
pub mod daycycle;
pub mod defaults;
pub mod fonts;
pub mod icons;
pub mod layout;
pub mod model;
pub mod pets;
pub mod proxy;
pub mod registry;
pub mod themes;
pub mod writer;

pub use appearance::{ButtonAccent, Palette};
pub use daycycle::{DayCycle, TimeBand};
pub use icons::IconSearch;
pub use layout::{LayerLayout, PET_LAYER, PET_LAYER_STRETCH};
pub use model::{parse_config, parse_config_files, LoadedModel};
pub use pets::{PetEnergy, PetVariant};
pub use proxy::{parse_bands, BandConfig, ButtonAction, ButtonConfig, ConfigProxy, LayerConfig};
pub use registry::KNOBS;
pub use themes::{ThemeConfig, ThemeRequest, ThemeSet, MAX_THEMES, THEMES_LAYER};
pub use writer::to_user_toml;
