use crate::appearance::ButtonAccent;
use crate::{FunctionLayer, LayerDeck};
use anyhow::{Context, Result};
use cairo::FontFace;
use dfr_config::defaults;
use dfr_config::{parse_config_files, IconSearch, PetEnergy, PetVariant, PET_LAYER};
use freetype::Library as FtLibrary;
use std::{fs, path::Path};

/// The configuration wire types and the whole parse/merge/validate path
/// live in the `dfr-config` crate, shared with `tiny-dfr check` and the
/// tests; re-exported here for the render stack's `crate::config` paths.
pub use dfr_config::{ButtonAction, ButtonConfig};

pub const USER_CFG_PATH: &str = "/etc/tiny-dfr/config.toml";
pub const BASE_CFG_PATH: &str = "/usr/share/tiny-dfr/config.toml";
/// Where the daemon remembers the theme chosen on the glass. The
/// directory is prepared by the service install (see the README); the
/// daemon itself only reads and atomically rewrites this one file.
pub const THEME_STATE_PATH: &str = "/var/lib/tiny-dfr/active-theme";
/// The fixed-path icon lookup directories, in priority order, shared
/// by the button loader and the configuration validator.
pub const ICON_DIRS: &[&str] = &["/etc/tiny-dfr", "/usr/share/tiny-dfr"];

pub struct Config {
    pub show_button_outlines: bool,
    pub enable_pixel_shift: bool,
    pub font_face: FontFace,
    pub adaptive_brightness: bool,
    pub active_brightness: u32,
    pub double_press_switch_layers: u32,
    pub battery_cycle_enabled: bool,
    pub battery_cycle_timeout_secs: u32,
    pub battery_warning_threshold: u32,
    pub battery_critical_threshold: u32,
    pub battery_warning_color: ButtonAccent,
    pub battery_critical_color: ButtonAccent,
    pub pet_enabled: bool,
    pub pet_variant: PetVariant,
    pub pet_energy: PetEnergy,
    /// The global (theme-free) day cycle as configured; the active
    /// theme's bands, if any, are applied on top for `day_cycle`, and
    /// re-applying a theme at runtime rebuilds from this copy.
    pub global_day_cycle: crate::daycycle::DayCycle,
    pub day_cycle: crate::daycycle::DayCycle,
    /// The accent palette the bar renders with, resolved from the
    /// active theme (or the shipped live palette with none).
    pub palette: crate::appearance::Palette,
    pub themes: crate::themes::ThemeSet,
}

/// Load the font a `FontTemplate` names. An unmatchable pattern or an
/// unloadable file is a configuration error carried in the `Result`,
/// never a panic: the caller's keep-current-configuration path absorbs
/// it exactly like every other invalid knob.
fn load_font(name: &str) -> Result<FontFace> {
    let (file_name, file_idx) = dfr_config::fonts::match_font(name).map_err(anyhow::Error::msg)?;
    let ft_library = FtLibrary::init().context("initializing freetype")?;
    let face = ft_library.new_face(&file_name, file_idx).with_context(|| {
        format!("loading the font file {file_name:?} matched for FontTemplate {name:?}")
    })?;
    FontFace::create_from_ft(&face)
        .with_context(|| format!("creating a cairo face for the font file {file_name:?}"))
}

pub fn load_config(
    width: u16,
    base_path: &Path,
    user_path: Option<&Path>,
    state_path: Option<&Path>,
) -> Result<(Config, LayerDeck)> {
    let dfr_config::LoadedModel {
        proxy: base,
        layout,
        day_cycle: global_day_cycle,
        mut themes,
    } = parse_config_files(base_path, user_path, width)?;
    // Resolve the file-backed resources the configuration names with
    // the same lookup order the button loader below loads through, so
    // what validates here is exactly what loads there.
    IconSearch::new(ICON_DIRS.iter().copied()).validate_layout(&layout)?;
    // Resolve the active theme: a config `ActiveTheme` pins the choice,
    // and with no pin the state file's last on-glass choice decides. A
    // state entry that no longer names a theme (the theme was removed
    // or renamed) falls back to the first theme and the file heals to
    // match, so a stale file can never wedge the bar.
    themes.active = base.active_theme.clone().filter(|_| themes.enabled);
    if themes.enabled && themes.active.is_none() {
        match state_path.and_then(read_persisted_theme) {
            Some(name) if themes.contains(&name) => themes.active = Some(name),
            Some(stale) => {
                eprintln!(
                    "persisted theme {stale:?} is no longer defined; using {:?}",
                    themes.names.first()
                );
                themes.active = themes.names.first().cloned();
            }
            None => {}
        }
    }
    if themes.enabled && themes.active.is_none() {
        themes.active = themes.names.first().cloned();
    }
    if themes.enabled {
        if let Some(active) = &themes.active {
            persist_active_theme(state_path, active);
        }
    }
    let palette = themes.palette();
    let pet_enabled = base.pet_enabled.unwrap_or(defaults::PET_ENABLED);
    let pet_layer = pet_enabled
        .then(|| {
            layout
                .layers
                .iter()
                .position(|layer| layer.name == PET_LAYER)
        })
        .flatten();
    let layers = LayerDeck::new(
        layout
            .layers
            .into_iter()
            .map(|layer| {
                let name = layer.name;
                FunctionLayer::with_config(layer.keys)
                    .with_context(|| format!("building layer {name:?}"))
                    .map(|built| (name, built))
            })
            .collect::<Result<Vec<_>>>()?,
        layout.home,
        layout.fn_layer,
        pet_layer,
        layout.legacy,
    );
    // Buttons resolve accent colors at draw time, except the waveform
    // widgets' live-dot tint, which is resolved once per build; retint
    // every widget so the active theme paints them too.
    let mut layers = layers;
    layers.apply_palette(&palette);
    let day_cycle = themes.day_cycle(&global_day_cycle);
    let cfg = Config {
        show_button_outlines: base
            .show_button_outlines
            .unwrap_or(defaults::SHOW_BUTTON_OUTLINES),
        enable_pixel_shift: base
            .enable_pixel_shift
            .unwrap_or(defaults::ENABLE_PIXEL_SHIFT),
        adaptive_brightness: base
            .adaptive_brightness
            .unwrap_or(defaults::ADAPTIVE_BRIGHTNESS),
        font_face: load_font(
            &base
                .font_template
                .clone()
                .unwrap_or_else(|| defaults::FONT_TEMPLATE.to_owned()),
        )?,
        active_brightness: base
            .active_brightness
            .unwrap_or(defaults::ACTIVE_BRIGHTNESS),
        double_press_switch_layers: base
            .double_press_switch_layers
            .unwrap_or(defaults::DOUBLE_PRESS_SWITCH_LAYERS),
        battery_cycle_enabled: base
            .battery_cycle_enabled
            .unwrap_or(defaults::BATTERY_CYCLE_ENABLED),
        battery_cycle_timeout_secs: base
            .battery_cycle_timeout_secs
            .unwrap_or(defaults::BATTERY_CYCLE_TIMEOUT_SECS),
        battery_warning_threshold: base
            .battery_warning_threshold
            .unwrap_or(defaults::BATTERY_WARNING_THRESHOLD),
        battery_critical_threshold: base
            .battery_critical_threshold
            .unwrap_or(defaults::BATTERY_CRITICAL_THRESHOLD),
        battery_warning_color: base
            .battery_warning_color
            .unwrap_or(defaults::BATTERY_WARNING_COLOR),
        battery_critical_color: base
            .battery_critical_color
            .unwrap_or(defaults::BATTERY_CRITICAL_COLOR),
        pet_enabled,
        pet_variant: base
            .pet_variant
            .as_deref()
            .and_then(|value| PetVariant::parse(value).ok())
            .unwrap_or_default(),
        pet_energy: base
            .pet_energy
            .as_deref()
            .and_then(|value| PetEnergy::parse(value).ok())
            .unwrap_or_default(),
        global_day_cycle,
        day_cycle,
        palette,
        themes,
    };
    Ok((cfg, layers))
}

/// Read the persisted active-theme choice, trimmed; `None` when the
/// file is missing or blank.
fn read_persisted_theme(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let name = text.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// Best-effort persistence of the active theme: written atomically
/// (staged then renamed) only when the file does not already say it.
/// Failures are logged, never fatal - the bar still applies the theme
/// live without the file.
fn persist_active_theme(state_path: Option<&Path>, name: &str) {
    let Some(path) = state_path else {
        return;
    };
    if read_persisted_theme(path).as_deref() == Some(name) {
        return;
    }
    if let Err(err) = write_persisted_theme(path, name) {
        eprintln!("could not persist the active theme {name:?}: {err}");
    }
}

pub fn write_persisted_theme(path: &Path, name: &str) -> std::io::Result<()> {
    let stage = path.with_extension("tmp");
    fs::write(&stage, name)?;
    fs::rename(&stage, path)
}
#[cfg(test)]
mod tests {
    use super::*;
    use dfr_config::{ConfigProxy, LayerLayout, LoadedModel, ThemeSet, PET_LAYER_STRETCH};
    use input_linux::Key;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    /// The content-level parse used by these tests: the shared
    /// parse/validate path, mapped to the (proxy, layout) pair the
    /// assertions read.
    fn parse_config(
        base: &str,
        user: Option<&str>,
        width: u16,
    ) -> anyhow::Result<(ConfigProxy, LayerLayout)> {
        dfr_config::parse_config(base, user, width)
            .map(|LoadedModel { proxy, layout, .. }| (proxy, layout))
    }

    const BASE: &str = include_str!("../../../share/tiny-dfr/config.toml");

    // Pixel digests of the golden frames (FNV-1a over the 64x2048
    // portrait buffer); see shipped_multibar_frames_match_the_pinned_pixel_digests.
    const GOLDEN_HOME: u64 = 5630012103463864515;
    const GOLDEN_MEDIA: u64 = 9540707964474980499;
    const GOLDEN_FUNCTION: u64 = 8649432053679081575;
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
    /// The minimal named layout plus a quick-keys `more` bar, so the theme
    /// system's synthesized buttons have their home.
    const NAMED_WITH_MORE: &str = r#"
DefaultLayer = "home"
FnLayer = "function"
[[Layers]]
Name = "home"
Keys = [{ Text = "Functions", Action = { Layer = "function" } }]
[[Layers]]
Name = "function"
Keys = [{ Text = "F1", Action = "F1" }]
[[Layers]]
Name = "more"
Keys = [
    { Text = "Sync", Action = { Layer = "home" } },
]
"#;

    #[test]
    fn button_accents_are_optional_and_invalid_values_reject_the_candidate() {
        let (_, old) = parse_config(BASE, Some(NAMED), 2008).unwrap();
        assert_eq!(old.layers[0].keys[0].accent, None);
        for (name, accent) in [
            ("neutral", ButtonAccent::Neutral),
            ("cyan", ButtonAccent::Cyan),
            ("amber", ButtonAccent::Amber),
            ("blue", ButtonAccent::Blue),
            ("violet", ButtonAccent::Violet),
            ("orange", ButtonAccent::Orange),
            ("red", ButtonAccent::Red),
            ("green", ButtonAccent::Green),
        ] {
            let candidate = NAMED.replace(
                "Text = \"F1\"",
                &format!("Accent = \"{name}\", Text = \"F1\""),
            );
            let (_, styled) = parse_config(BASE, Some(&candidate), 2008).unwrap();
            let key = styled.layers[1].keys.last().unwrap();
            assert_eq!(key.accent, Some(accent));
            assert_eq!(key.action, ButtonAction::Keys(vec![Key::F1]));
        }
        let invalid = NAMED.replace("Text = \"F1\"", "Accent = \"magenta\", Text = \"F1\"");
        assert!(parse_config(BASE, Some(&invalid), 2008).is_err());
    }

    #[test]
    fn waveform_buttons_carry_an_optional_graph_style() {
        for (value, style) in [
            ("line", crate::waveform::Style::Line),
            ("bars", crate::waveform::Style::Bars),
        ] {
            let candidate = NAMED.replace(
                "Text = \"F1\"",
                &format!("Waveform = true, WaveformStyle = \"{value}\""),
            );
            let (_, styled) = parse_config(BASE, Some(&candidate), 2008).unwrap();
            let key = styled.layers[1].keys.last().unwrap();
            assert_eq!(key.waveform, Some(true));
            assert_eq!(key.waveform_style.as_deref(), Some(value));
            assert_eq!(crate::waveform::Style::parse(value), Some(style));
        }
        // A style without a Waveform button stays inert; an unknown value
        // maps to no style, which the button builder reports as a
        // configuration error.
        let inert = NAMED.replace("Text = \"F1\"", "Text = \"F1\", WaveformStyle = \"line\"");
        let (_, styled) = parse_config(BASE, Some(&inert), 2008).unwrap();
        assert_eq!(styled.layers[1].keys.last().unwrap().waveform, None);
        assert!(crate::waveform::Style::parse("dots").is_none());
    }

    #[test]
    fn visualizer_buttons_carry_their_own_knobs_and_stay_inert_without_one() {
        for (value, style) in [
            ("line", crate::waveform::Style::Line),
            ("bars", crate::waveform::Style::Bars),
        ] {
            let candidate = NAMED.replace(
                "Text = \"F1\"",
                &format!(
                    "Visualizer = true, VisualizerStyle = \"{value}\", \
                     VisualizerSmoothing = 240, VisualizerLag = 80"
                ),
            );
            let (_, styled) = parse_config(BASE, Some(&candidate), 2008).unwrap();
            let key = styled.layers[1].keys.last().unwrap();
            assert_eq!(key.visualizer, Some(true));
            assert_eq!(key.visualizer_style.as_deref(), Some(value));
            assert_eq!(key.visualizer_smoothing, Some(240));
            assert_eq!(key.visualizer_lag, Some(80));
            assert_eq!(crate::waveform::Style::parse(value), Some(style));
        }
        // The knobs without a Visualizer button stay inert, and an
        // unknown style maps to no style, which the button builder
        // reports as a configuration error.
        let inert = NAMED.replace("Text = \"F1\"", "Text = \"F1\", VisualizerStyle = \"line\"");
        let (_, styled) = parse_config(BASE, Some(&inert), 2008).unwrap();
        assert_eq!(styled.layers[1].keys.last().unwrap().visualizer, None);
        assert!(crate::waveform::Style::parse("dots").is_none());
        // A waveform button and a visualizer button coexist on one bar as
        // distinct widgets: the first meters the mic, the second music. The
        // synthesized Home button leads the sub-bar as usual.
        let both = NAMED.replace("Text = \"F1\"", "Waveform = true, Text = \"F1\"")
            + "\n[[Layers]]\nName = \"media\"\nKeys = [{ Visualizer = true, Stretch = 2 }]\n";
        let (_, styled) = parse_config(BASE, Some(&both), 2008).unwrap();
        let media = styled
            .layers
            .iter()
            .find(|layer| layer.name == "media")
            .unwrap();
        assert_eq!(media.keys.len(), 2);
        assert_eq!(media.keys[0].text.as_deref(), Some("Home"));
        assert_eq!(media.keys[1].visualizer, Some(true));
        assert_eq!(media.keys[1].waveform, None);
    }

    #[test]
    fn now_playing_buttons_parse_and_coexist_with_their_neighbors() {
        // The knob lands on the button that declares it.
        let candidate = NAMED.replace("Text = \"F1\"", "NowPlaying = true, Text = \"F1\"");
        let (_, styled) = parse_config(BASE, Some(&candidate), 2008).unwrap();
        let key = styled.layers[1].keys.last().unwrap();
        assert_eq!(key.now_playing, Some(true));
        assert_eq!(key.visualizer, None);
        // Buttons without the knob stay inert.
        let (_, styled) = parse_config(BASE, Some(NAMED), 2008).unwrap();
        assert_eq!(styled.layers[1].keys.last().unwrap().now_playing, None);
        // A now-playing row and a visualizer button coexist on one bar
        // as distinct widgets, each display-only, alongside transport:
        // the shipped media bar's shape. The synthesized Home button
        // leads the sub-bar as usual.
        let media = "\n[[Layers]]\nName = \"media\"\nKeys = [{ Icon = \"play_pause\", Action = \"PlayPause\" }, \
            { NowPlaying = true, Stretch = 2 }, { Visualizer = true }]\n";
        let (_, styled) = parse_config(BASE, Some(&(NAMED.to_owned() + media)), 2008).unwrap();
        let media = styled
            .layers
            .iter()
            .find(|layer| layer.name == "media")
            .unwrap();
        assert_eq!(media.keys.len(), 4);
        assert_eq!(media.keys[0].text.as_deref(), Some("Home"));
        assert_eq!(
            media.keys[1].action,
            ButtonAction::Keys(vec![Key::PlayPause])
        );
        assert_eq!(media.keys[2].now_playing, Some(true));
        assert_eq!(media.keys[2].stretch, Some(2));
        assert_eq!(media.keys[3].visualizer, Some(true));
    }

    #[test]
    fn approved_multibar_preserves_voice_dispatch_and_renders_offline() {
        render_multibar_profile_offline(true);
    }

    #[test]
    fn consumer_multibar_settings_dispatch_and_renders_offline() {
        render_multibar_profile_offline(false);
    }

    fn render_multibar_profile_offline(voice: bool) {
        use chrono::Timelike;
        let tmp = TempDir::new();
        let base_path = tmp.0.join("base.toml");
        let user_path = tmp.0.join("config.toml");
        fs::write(&base_path, BASE).unwrap();
        let named = std::env::var_os("DFR_DECK_CONFIG_REVIEW_INPUT")
            .map(fs::read_to_string)
            .transpose()
            .unwrap()
            .unwrap_or_else(|| {
                if voice {
                    include_str!("../../../share/tiny-dfr/multibar-voice.toml").into()
                } else {
                    include_str!("../../../share/tiny-dfr/multibar.toml").into()
                }
            });
        fs::write(&user_path, named).unwrap();
        let (mut cfg, mut deck) = load_config(2008, &base_path, Some(&user_path), None).unwrap();

        // Renders one frame with an explicit background color, used for the
        // time-of-day band review frames below.
        let render_with_background =
            |deck: &mut LayerDeck, name: &str, cfg: &Config, background: (f64, f64, f64)| {
                let portrait =
                    cairo::ImageSurface::create(cairo::Format::ARgb32, 64, 2048).unwrap();
                deck.slider.pixel_shift_width = if cfg.enable_pixel_shift {
                    crate::PIXEL_SHIFT_WIDTH_PX
                } else {
                    0
                };
                deck.slider.pixel_shift_x = 0.0;
                let volume = deck.slider_render();
                deck.current_mut().draw(
                    cfg,
                    (2008, 60),
                    &portrait,
                    (0.0, 0.0),
                    true,
                    volume,
                    background,
                );
                if let Some(dir) = std::env::var_os("DFR_DECK_REVIEW_OUTPUT") {
                    fs::create_dir_all(&dir).unwrap();
                    let horizontal =
                        cairo::ImageSurface::create(cairo::Format::ARgb32, 2008, 60).unwrap();
                    let c = cairo::Context::new(&horizontal).unwrap();
                    c.translate(0.0, 60.0);
                    c.rotate((-90.0_f64).to_radians());
                    c.set_source_surface(&portrait, 0.0, 0.0).unwrap();
                    c.paint().unwrap();
                    horizontal
                        .write_to_png(
                            &mut fs::File::create(Path::new(&dir).join(if voice {
                                format!("voice-{name}")
                            } else {
                                name.to_owned()
                            }))
                            .unwrap(),
                        )
                        .unwrap();
                }
            };
        let render = |deck: &mut LayerDeck, name: &str| {
            let background = cfg.day_cycle.background(chrono::Local::now().hour());
            render_with_background(deck, name, &cfg, background);
        };
        let tap = |deck: &mut LayerDeck, label: &str, commands: &mut Vec<String>| {
            let index = deck.current().buttons.iter().position(|(_, button)| {
                matches!(&button.image, crate::ButtonImage::Text(text) if text == label)
            }).unwrap();
            let spans = deck.current().layout_spans(2008, 0);
            let x = (spans[index].left + spans[index].right) / 2.0;
            let mut emit = |keys: &[Key], _| assert!(keys.is_empty(), "voice button emitted keys");
            deck.touch_down_at(
                0,
                x,
                30.0,
                2008,
                60,
                &mut emit,
                &mut |command| commands.push(command.into()),
                &mut |_| None,
            );
            deck.touch_up(0, &mut emit);
        };
        let mut commands = Vec::new();
        assert!(matches!(&deck.current().buttons.last().unwrap().1.image,
            crate::ButtonImage::Text(text) if text == if voice { "Mic" } else { "Settings" }));
        for layer in [deck.home, deck.names["media"]] {
            assert_eq!(
                deck.layers[layer]
                    .buttons
                    .iter()
                    .filter(|(_, button)| button.action == ButtonAction::VolumeSlider)
                    .count(),
                1
            );
            assert!(!deck.layers[layer].buttons.iter().any(|(_, button)|
                matches!(&button.action, ButtonAction::Keys(keys)
                    if keys.iter().any(|key| matches!(key, Key::VolumeUp | Key::VolumeDown | Key::Mute)))));
        }
        let brightness_index = deck
            .current()
            .buttons
            .iter()
            .position(|(_, button)| button.action == ButtonAction::BrightnessSlider)
            .unwrap();
        assert_eq!(brightness_index, 1);
        assert_eq!(
            deck.current()
                .buttons
                .iter()
                .filter(|(_, b)| b.action == ButtonAction::BrightnessSlider)
                .count(),
            1
        );
        assert!(!deck.current().buttons.iter().any(|(_, b)| matches!(&b.action, ButtonAction::Keys(keys) if keys.iter().any(|k| matches!(k, Key::BrightnessDown | Key::BrightnessUp)))));
        render(&mut deck, "implemented-home.png");
        // The persistent closed-slider mute mark on the same home bar: the
        // watch's probe read a muted mixer, so the volume button carries
        // the slash the drawn slider would show; the bar is otherwise the
        // identical plain frame above, at the same geometry.
        let (revision, request) = deck
            .mute_probe(Instant::now())
            .expect("a volume-slider deck probes the mixer for the closed-bar mute mark");
        assert_eq!(request, crate::level::Request::Get);
        deck.mute_reply(
            revision,
            Ok(crate::level::State {
                percent: 65,
                muted: true,
            }),
        );
        assert!(deck
            .current()
            .buttons
            .iter()
            .any(|(_, button)| button.muted && button.action == ButtonAction::VolumeSlider));
        render(&mut deck, "implemented-home-muted.png");
        deck.show_volume_muted(false);
        // One review frame per time-of-day band, at an hour inside each
        // default band, showing the strip background behind the same home bar.
        for (hour, name) in [(7, "morning"), (14, "day"), (19, "evening"), (23, "night")] {
            let background = cfg.day_cycle.background(hour);
            render_with_background(
                &mut deck,
                &format!("implemented-daycycle-{name}.png"),
                &cfg,
                background,
            );
        }
        deck.switch_to(deck.names["media"], &mut |_, _| {});
        render(&mut deck, "implemented-media.png");
        // The now-playing row doing its job on the same bar: a playing
        // track's title - a long one, truncated to the row's budget and
        // fitted - above its artist caption, then the graceful empty
        // state again: nothing playing, nothing stale. Without the
        // desktop receiver every poll is empty, so the shipped frame is
        // the dim `Nothing playing` state exactly as a missing helper
        // leaves it.
        assert!(
            deck.now_playing_tick(Some(crate::now_playing::Feed::Playing(
                crate::now_playing::Track {
                    title: "Lucky Star Episode 1 [English Dubbed]".into(),
                    artist: Some("Weaban".into()),
                },
            )))
        );
        render(&mut deck, "implemented-media-playing.png");
        assert!(deck.now_playing_tick(Some(crate::now_playing::Feed::Idle)));
        // The media bar's visualizer button: off by default with only its
        // glyph-and-Viz label group, then expanded into the full-bar view
        // with live monitor levels, then Off returning the plain media
        // bar - each exported for review.
        let viz_index = deck.current().visualizer.unwrap();
        let spans = deck.current().layout_spans(2008, 0);
        let viz_x = (spans[viz_index].left + spans[viz_index].right) / 2.0;
        assert!(!deck.viz_active());
        let mut viz_emit = |_: &[Key], _: bool| panic!("visualizer emitted keys");
        deck.touch_down_at(
            0,
            viz_x,
            30.0,
            2008,
            60,
            &mut viz_emit,
            &mut |_| {},
            &mut |_| None,
        );
        assert!(deck.viz_active());
        assert!(deck.current().viz_view);
        assert_eq!(deck.current().layout_spans(2008, 0).len(), 2);
        let mut viz_now = Instant::now();
        for level in [
            12, 38, 8, 61, 84, 30, 55, 72, 21, 47, 66, 15, 52, 33, 70, 44,
        ] {
            viz_now += Duration::from_millis(50);
            deck.visualizer_tick(
                Some(crate::MicSample {
                    level,
                    live: true,
                    dead: false,
                }),
                viz_now,
            );
        }
        render(&mut deck, "implemented-media-viz.png");
        // The view's single control is its Off button; tapping it restores
        // the exact media bar that opened the view.
        let off_spans = deck.current().layout_spans(2008, 0);
        let off_x = (off_spans[1].left + off_spans[1].right) / 2.0;
        deck.touch_down_at(
            1,
            off_x,
            30.0,
            2008,
            60,
            &mut viz_emit,
            &mut |_| {},
            &mut |_| None,
        );
        assert!(!deck.viz_active());
        assert!(!deck.current().viz_view);
        assert_eq!(deck.current().visualizer, Some(viz_index));
        deck.visualizer_tick(
            Some(crate::MicSample {
                level: 90,
                live: true,
                dead: false,
            }),
            viz_now,
        );
        render(&mut deck, "implemented-media-off.png");
        deck.switch_to(deck.home, &mut |_, _| {});
        let spans = deck.current().layout_spans(2008, 0);
        let x = (spans[0].left + spans[0].right) / 2.0;
        let mut emit = |_: &[Key], _: bool| panic!("slider emitted keys");
        deck.touch_down_at(0, x, 30.0, 2008, 60, &mut emit, &mut |_| {}, &mut |_| None);
        assert_eq!(deck.slider.region, Some((0, 1)));
        assert_eq!(
            deck.slider.requests.last().unwrap().2,
            crate::level::Request::Get
        );
        deck.slider_reply(
            deck.slider.revision,
            crate::slider::Kind::Volume,
            Ok(crate::LevelState {
                percent: 65,
                muted: false,
            }),
        );
        render(&mut deck, "implemented-volume-mid.png");
        let (left, right) = deck.slider_track(2008).unwrap();
        deck.touch_motion(0, left, 30.0, 2008, 60, &mut emit);
        deck.slider_reply(
            deck.slider.revision,
            crate::slider::Kind::Volume,
            Ok(crate::LevelState {
                percent: 0,
                muted: false,
            }),
        );
        render(&mut deck, "implemented-volume-zero.png");
        deck.slider_drag_motion(0, right, 30.0, 2008, &mut emit);
        deck.slider_reply(
            deck.slider.revision,
            crate::slider::Kind::Volume,
            Ok(crate::LevelState {
                percent: 130,
                muted: false,
            }),
        );
        assert_eq!(deck.slider.level, 130);
        render(&mut deck, "implemented-volume-max.png");
        deck.touch_up(0, &mut emit);
        deck.slider_collapse();
        let spans = deck.current().layout_spans(2008, 0);
        let brightness_x = (spans[brightness_index].left + spans[brightness_index].right) / 2.0;
        deck.touch_down_at(
            0,
            brightness_x,
            30.0,
            2008,
            60,
            &mut emit,
            &mut |_| {},
            &mut |_| None,
        );
        assert_eq!(deck.slider.region, Some((0, 1)));
        assert_eq!(deck.slider.kind, crate::slider::Kind::Brightness);
        assert!(!deck.slider.confirmed);
        assert_eq!(
            deck.slider.requests.last().unwrap().2,
            crate::level::Request::Get
        );
        deck.slider_reply(
            deck.slider.revision,
            crate::slider::Kind::Brightness,
            Ok(crate::LevelState {
                percent: 93,
                muted: false,
            }),
        );
        render(&mut deck, "implemented-brightness.png");
        deck.touch_motion(0, left, 30.0, 2008, 60, &mut emit);
        deck.slider_reply(
            deck.slider.revision,
            crate::slider::Kind::Brightness,
            Ok(crate::LevelState {
                percent: 1,
                muted: false,
            }),
        );
        render(&mut deck, "implemented-brightness-min.png");
        deck.slider_drag_motion(0, right, 30.0, 2008, &mut emit);
        deck.slider_reply(
            deck.slider.revision,
            crate::slider::Kind::Brightness,
            Ok(crate::LevelState {
                percent: 100,
                muted: false,
            }),
        );
        render(&mut deck, "implemented-brightness-max.png");
        deck.touch_up(0, &mut emit);
        if voice {
            // Voice coverage deliberately selects the opt-in profile, never executes commands.
            // Mic stays reachable while the brightness overlay is still open.
            tap(&mut deck, "Mic", &mut commands);
            assert_eq!(deck.active, deck.names["recording"]);
            // The waveform widget shows live levels and its duration timer while
            // this layer is displayed; entering starts the session.
            let mut feed_now = Instant::now();
            let (activated, _) = deck.waveform_tick(None, feed_now);
            assert!(activated);
            for level in [
                8, 24, 12, 40, 66, 88, 35, 19, 57, 74, 28, 90, 46, 61, 33, 15, 52, 70,
            ] {
                feed_now += Duration::from_millis(50);
                deck.waveform_tick(
                    Some(crate::MicSample {
                        level,
                        live: true,
                        dead: false,
                    }),
                    feed_now,
                );
            }
            // Mid-period frame: the graph is caught halfway between two samples,
            // with the newest level eased to half its measured value and the
            // whole curve glided half a slot left of its sample positions.
            let mid = Instant::now() - Duration::from_millis(25);
            deck.waveform_tick(
                Some(crate::MicSample {
                    level: 88,
                    live: true,
                    dead: false,
                }),
                mid,
            );
            render(&mut deck, "implemented-recording-mid.png");
            // Settled frame: a sample one full period old rests the interpolation,
            // so the exported graph shows its sample positions and full heights.
            let settled = Instant::now() - Duration::from_millis(60);
            deck.waveform_tick(
                Some(crate::MicSample {
                    level: 70,
                    live: true,
                    dead: false,
                }),
                settled,
            );
            feed_now += Duration::from_secs(73);
            deck.waveform_tick(None, feed_now);
            render(&mut deck, "implemented-recording-live.png");
            // Without the desktop level feed every poll fails: the widget shows
            // the dead-feed state - the red slashed dot and the `No feed` label
            // - never the calm idle look of a merely quiet feed.
            for _ in 0..4 {
                feed_now += Duration::from_millis(50);
                deck.waveform_tick(Some(crate::MicSample::dead()), feed_now);
            }
            render(&mut deck, "implemented-recording-dead.png");
            tap(&mut deck, "Stop", &mut commands);
            assert_eq!(deck.active, deck.home);
            tap(&mut deck, "Mic", &mut commands);
            tap(&mut deck, "Cancel", &mut commands);
            assert_eq!(deck.active, deck.home);
        } else {
            // Settings remains reachable while the slider is expanded; handoff
            // is a captured string, not a subprocess or evidence of a mapped GUI.
            tap(&mut deck, "Settings", &mut commands);
            assert_eq!(deck.active, deck.home);
            assert_eq!(commands, ["touchbar-capture-user settings"]);
            assert!(!deck.waveform_active());
            assert!(!deck.names.contains_key("recording"));
        }
        // The home bar's More button opens the quick-keys bar; its
        // Screenshot entry fires the screenshot flow and returns home in
        // one press.
        tap(&mut deck, "More", &mut commands);
        assert_eq!(deck.active, deck.names["more"]);
        render(&mut deck, "implemented-more.png");
        // The shipped More bar has no theme controls. Opt in through the
        // real configuration consumer before exercising those optional taps.
        let mut themed: dfr_config::ConfigProxy =
            toml::from_str(include_str!("../../../share/tiny-dfr/multibar.toml")).unwrap();
        let more = themed
            .layers
            .as_mut()
            .unwrap()
            .iter_mut()
            .find(|layer| layer.name == "more")
            .unwrap();
        more.keys.extend([
            dfr_config::ButtonConfig {
                text: Some("Theme".into()),
                action: ButtonAction::CycleTheme,
                ..Default::default()
            },
            dfr_config::ButtonConfig {
                text: Some("Themes".into()),
                action: ButtonAction::SwitchLayer("themes".into()),
                ..Default::default()
            },
        ]);
        fs::write(&user_path, toml::to_string(&themed).unwrap()).unwrap();
        let (_, themed_deck) = load_config(2008, &base_path, Some(&user_path), None).unwrap();
        deck = themed_deck;
        deck.switch_to(deck.names["more"], &mut |_, _| {});
        // The theme system: the Themes opener switches into the picker, a
        // full-bar takeover listing every theme name; tapping one applies
        // it live - the palette and the day-cycle bands swap in place, no
        // restart, no reload - and returns home. The exported frames: the
        // picker itself, the home bar under the applied theme, and the
        // more bar's cycle button briefly wearing the applied name.
        let themes_button = deck
            .current()
            .buttons
            .iter()
            .position(|(_, button)| {
                matches!(&button.action, ButtonAction::SwitchLayer(name) if name == "themes")
            })
            .unwrap();
        let spans = deck.current().layout_spans(2008, 0);
        let themes_x = (spans[themes_button].left + spans[themes_button].right) / 2.0;
        deck.touch_down_at(
            0,
            themes_x,
            30.0,
            2008,
            60,
            &mut viz_emit,
            &mut |_| {},
            &mut |_| None,
        );
        assert_eq!(deck.active, deck.names["themes"]);
        assert!(deck.current().buttons.iter().any(|(_, button)| matches!(
            (&button.image, &button.action),
            (crate::ButtonImage::Text(text), ButtonAction::ApplyThemeLayer { theme, .. })
                if text == theme
        )));
        render(&mut deck, "implemented-themes.png");
        // Apply Nord by tapping its button through the same callback the
        // daemon wires, then watch the home bar render in its colors.
        let nord_button = deck
            .current()
            .buttons
            .iter()
            .position(
                |(_, button)| matches!(&button.action, ButtonAction::ApplyThemeLayer { theme, .. } if theme == "Nord"),
            )
            .unwrap();
        let spans = deck.current().layout_spans(2008, 0);
        let nord_x = (spans[nord_button].left + spans[nord_button].right) / 2.0;
        // The same apply callback the daemon wires: the palette, the day
        // cycle, and the active choice live on the config, and each tap's
        // callback borrows it only for that tap.
        let apply_theme = |cfg: &mut Config,
                           request: crate::themes::ThemeRequest|
         -> Option<(String, crate::appearance::Palette)> {
            let name = match request {
                crate::themes::ThemeRequest::Next => cfg.themes.cycle_next()?,
                crate::themes::ThemeRequest::Named(name) => name.to_owned(),
            };
            if !cfg.themes.apply(&name) {
                return None;
            }
            cfg.palette = cfg.themes.palette();
            cfg.day_cycle = cfg.themes.day_cycle(&cfg.global_day_cycle);
            Some((name, cfg.palette))
        };
        deck.touch_down_at(
            1,
            nord_x,
            30.0,
            2008,
            60,
            &mut viz_emit,
            &mut |_| {},
            &mut |request| apply_theme(&mut cfg, request),
        );
        assert_eq!(deck.active, deck.home);
        assert_eq!(cfg.themes.active.as_deref(), Some("Nord"));
        // The frame helper paints with whatever theme is live.
        let render = |deck: &mut LayerDeck, name: &str| {
            let background = cfg.day_cycle.background(chrono::Local::now().hour());
            render_with_background(deck, name, &cfg, background);
        };
        render(&mut deck, "implemented-home-nord.png");
        // The cycle button on the more bar: one tap applies the next theme
        // in place and briefly shows its name on the button.
        tap(&mut deck, "More", &mut commands);
        let theme_button = deck
            .current()
            .buttons
            .iter()
            .position(|(_, button)| matches!(button.action, ButtonAction::CycleTheme))
            .unwrap();
        let spans = deck.current().layout_spans(2008, 0);
        let theme_x = (spans[theme_button].left + spans[theme_button].right) / 2.0;
        deck.touch_down_at(
            2,
            theme_x,
            30.0,
            2008,
            60,
            &mut viz_emit,
            &mut |_| {},
            &mut |request| apply_theme(&mut cfg, request),
        );
        assert!(matches!(
            &deck.current().buttons[theme_button].1.image,
            crate::ButtonImage::Text(text) if text == "Ember"
        ));
        let render = |deck: &mut LayerDeck, name: &str| {
            let background = cfg.day_cycle.background(chrono::Local::now().hour());
            render_with_background(deck, name, &cfg, background);
        };
        render(&mut deck, "implemented-more-theme-flash.png");
        let (restored, _) = deck.theme_flash_tick(Instant::now() + Duration::from_millis(1600));
        assert!(restored);
        tap(&mut deck, "Screenshot", &mut commands);
        assert_eq!(deck.active, deck.home);
        assert_eq!(commands.len(), if voice { 5 } else { 2 });
        let actions: Vec<_> = commands
            .iter()
            .map(|command| command.split_whitespace().last().unwrap())
            .collect();
        if voice {
            assert_eq!(actions, ["start", "stop", "start", "cancel", "screenshot"]);
        } else {
            assert_eq!(actions, ["settings", "screenshot"]);
        }
    }

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
            // primary, media, the synthesized pet layer, and the
            // synthesized themes picker.
            assert_eq!(layout.layers.len(), 4);
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
        assert_eq!(stock.layers[0].keys.len(), 13);
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
        assert!(legacy
            .layers
            .iter()
            .all(|layer| layer.keys[0].action == ButtonAction::Keys(vec![Key::Esc])));
        // The legacy primary bar ends with the synthesized Pet button.
        let primary = &legacy.layers[0];
        assert_eq!(primary.keys.len(), 14);
        let pet_button = primary.keys.last().unwrap();
        assert_eq!(pet_button.text.as_deref(), Some("Pet"));
        assert_eq!(pet_button.action, ButtonAction::SwitchLayer("pet".into()));
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
        assert_eq!(layout.layers[0].keys[0].text.as_deref(), Some("Functions"));
        assert_eq!(layout.layers[0].keys.len(), 1);
        // The Pet button joins whatever layer is the home bar, here "function".
        let home = &layout.layers[layout.home];
        assert_eq!(home.keys.len(), 2);
        assert_eq!(home.keys.last().unwrap().text.as_deref(), Some("Pet"));
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
            Some(include_str!("../../../share/tiny-dfr/multibar.toml")),
            2008,
        )
        .unwrap();
        assert_eq!(layout.layers.len(), 6);
        assert!(layout.layers.iter().all(|layer| layer.name != "recording"
            && layer.keys.iter().all(|key| key.waveform != Some(true)
                && !key
                    .action
                    .command()
                    .is_some_and(|command| command.contains("voice")))));
        let home = &layout.layers[layout.home];
        // The clock is gone from the shipped home bar; its width goes to
        // the remaining buttons.
        assert!(!home.keys.iter().any(|key| key.time.is_some()));
        assert!(home.keys.iter().any(|key| key.battery.is_some()));
        for name in ["function", "media", "more"] {
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
        assert!(!home
            .keys
            .iter()
            .any(|key| key.action == ButtonAction::SwitchLayer("pet".into())));
        let more = layout
            .layers
            .iter()
            .find(|layer| layer.name == "more")
            .unwrap();
        assert!(more
            .keys
            .iter()
            .any(|key| key.text.as_deref() == Some("Pet")
                && key.action == ButtonAction::SwitchLayer("pet".into())));
        assert_eq!(home.keys.len(), 8);
        assert_eq!(home.keys[6].battery.as_deref(), Some("both"));
        assert_eq!(home.keys[6].stretch, Some(2));
        assert_eq!(home.keys[7].text.as_deref(), Some("Settings"));
        assert_eq!(
            home.keys[7].action,
            ButtonAction::Command("touchbar-capture-user settings".into())
        );
        // The quick-keys bar: its one shipped entry fires the screenshot
        // flow and returns home, behind an explicit Home button that
        // leaves without running anything.
        let more = layout
            .layers
            .iter()
            .find(|layer| layer.name == "more")
            .unwrap();
        // Home, Screenshot, Lock, and the synthesized Pet opener.
        // Settings is on Home; theme buttons remain opt-in.
        assert_eq!(more.keys.len(), 4);
        assert_eq!(more.keys[0].text.as_deref(), Some("Home"));
        assert_eq!(
            more.keys[0].action,
            ButtonAction::SwitchLayer("home".into())
        );
        assert_eq!(more.keys[1].text.as_deref(), Some("Screenshot"));
        assert_eq!(
            more.keys[1].action,
            ButtonAction::CommandLayer {
                command: "touchbar-capture-user screenshot".into(),
                layer: "home".into()
            }
        );
        // Pet is tucked into More, leaving the home navigation cluster
        // focused on the primary deck layers.
        let positions: Vec<usize> = ["function", "media", "more"]
            .into_iter()
            .map(|name| {
                home.keys
                    .iter()
                    .position(|key| key.action == ButtonAction::SwitchLayer(name.into()))
                    .unwrap()
            })
            .collect();
        assert_eq!(positions, vec![3, 4, 5]);
        // The media bar keeps its existing controls and gains two
        // buttons: the now-playing row after the transport cluster, and
        // the opt-in music visualizer at the end, off by default every
        // time.
        let media = layout
            .layers
            .iter()
            .find(|layer| layer.name == "media")
            .unwrap();
        assert_eq!(media.keys.len(), 7);
        assert_eq!(media.keys[1].icon.as_deref(), Some("fast_rewind"));
        assert_eq!(media.keys[2].icon.as_deref(), Some("play_pause"));
        assert_eq!(media.keys[3].icon.as_deref(), Some("fast_forward"));
        let now_playing = &media.keys[4];
        assert_eq!(now_playing.now_playing, Some(true));
        assert_eq!(now_playing.stretch, Some(2));
        assert_eq!(now_playing.action, ButtonAction::Keys(vec![]));
        assert_eq!(now_playing.text, None);
        assert_eq!(now_playing.icon, None);
        assert_eq!(media.keys[5].action, ButtonAction::VolumeSlider);
        assert_eq!(media.keys[5].stretch, Some(3));
        let viz = media.keys.last().unwrap();
        assert_eq!(viz.visualizer, Some(true));
        assert_eq!(viz.waveform, None);
        assert_eq!(viz.stretch, Some(2));
        assert_eq!(viz.action, ButtonAction::Keys(vec![]));
        let pet_button = more
            .keys
            .iter()
            .find(|key| key.action == ButtonAction::SwitchLayer("pet".into()))
            .unwrap();
        assert_eq!(pet_button.text.as_deref(), Some("Pet"));
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
    fn optional_voice_profile_preserves_recording_controls() {
        let (_, layout) = parse_config(
            BASE,
            Some(include_str!("../../../share/tiny-dfr/multibar-voice.toml")),
            2008,
        )
        .unwrap();
        let home = &layout.layers[layout.home];
        let mic = home.keys.last().unwrap();
        assert_eq!(mic.text.as_deref(), Some("Mic"));
        assert_eq!(mic.accent, Some(ButtonAccent::Cyan));
        assert_eq!(
            mic.action,
            ButtonAction::CommandLayer {
                command: "voice-dictate-user start".into(),
                layer: "recording".into(),
            }
        );
        let recording = layout
            .layers
            .iter()
            .find(|layer| layer.name == "recording")
            .unwrap();
        assert_eq!(recording.keys.len(), 3);
        for (index, command) in [(0, "cancel"), (2, "stop")] {
            assert_eq!(
                recording.keys[index].action,
                ButtonAction::CommandLayer {
                    command: format!("voice-dictate-user {command}"),
                    layer: "home".into(),
                }
            );
        }
        assert_eq!(recording.keys[1].waveform, Some(true));
        assert_eq!(recording.keys[1].waveform_style, None);
        assert!(recording.keys[1].text.is_none());
        assert_eq!(recording.keys[1].stretch, Some(3));
    }

    #[test]
    fn an_authored_more_pet_opener_overrides_the_synthesized_default() {
        let example = include_str!("../../../share/tiny-dfr/multibar.toml");
        let config = example.replace(
            "{ Text = \"Home\", Action = { Layer = \"home\" } },\n    { Text = \"Screenshot\"",
            "{ Text = \"Home\", Action = { Layer = \"home\" } },\n    { Text = \"My Pet\", Action = { Layer = \"pet\" } },\n    { Text = \"Screenshot\"",
        );
        let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
        let more = layout
            .layers
            .iter()
            .find(|layer| layer.name == "more")
            .unwrap();
        let pet_openers: Vec<_> = more
            .keys
            .iter()
            .filter(|key| key.action == ButtonAction::SwitchLayer("pet".into()))
            .collect();
        assert_eq!(pet_openers.len(), 1);
        assert_eq!(pet_openers[0].text.as_deref(), Some("My Pet"));
        let home = &layout.layers[layout.home];
        assert!(!home
            .keys
            .iter()
            .any(|key| key.action == ButtonAction::SwitchLayer("pet".into())));
    }

    #[test]
    fn shipped_home_slider_pair_sizes_identically() {
        // The volume and brightness sliders are one pair of equal twins:
        // the sizing knob is the per-button Stretch, and the shipped home
        // bar must give both the same explicit value so they render the
        // same width.
        let (_, layout) = parse_config(
            BASE,
            Some(include_str!("../../../share/tiny-dfr/multibar.toml")),
            2008,
        )
        .unwrap();
        let home = &layout.layers[layout.home];
        for action in [ButtonAction::VolumeSlider, ButtonAction::BrightnessSlider] {
            assert_eq!(
                home.keys.iter().filter(|key| key.action == action).count(),
                1
            );
        }
        let volume = home
            .keys
            .iter()
            .position(|key| key.action == ButtonAction::VolumeSlider)
            .unwrap();
        let brightness = home
            .keys
            .iter()
            .position(|key| key.action == ButtonAction::BrightnessSlider)
            .unwrap();
        // Adjacent icon buttons sharing one explicit width.
        assert_eq!(brightness, volume + 1);
        assert!(home.keys[volume].icon.is_some());
        assert!(home.keys[brightness].icon.is_some());
        assert_eq!(home.keys[volume].stretch, home.keys[brightness].stretch);
        assert!(home.keys[volume].stretch.is_some());
    }

    #[test]
    fn command_actions_parse_alone_and_combined_with_layer_switches() {
        let config = format!(
            "{NAMED}\n\
[[Layers]]\n\
Name = \"recording\"\n\
Keys = [\n\
    {{ Text = \"Sync\", Action = {{ Command = \"sync-now\" }} }},\n\
    {{ Text = \"Mic\", Action = {{ Command = \"voice-dictate start\", Layer = \"recording\" }} }},\n\
]\n"
        );
        let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
        let recording = layout
            .layers
            .iter()
            .find(|layer| layer.name == "recording")
            .unwrap();
        // No button visibly returns home, so the generic Home button is
        // still synthesized ahead of the configured keys.
        assert_eq!(recording.keys.len(), 3);
        assert_eq!(
            recording.keys[0].action,
            ButtonAction::SwitchLayer("home".into())
        );
        assert_eq!(
            recording.keys[1].action,
            ButtonAction::Command("sync-now".into())
        );
        assert_eq!(
            recording.keys[2].action,
            ButtonAction::CommandLayer {
                command: "voice-dictate start".into(),
                layer: "recording".into()
            }
        );
        // The helpers see through both action shapes.
        assert_eq!(recording.keys[1].action.layer_target(), None);
        assert_eq!(recording.keys[1].action.command(), Some("sync-now"));
        assert_eq!(recording.keys[2].action.layer_target(), Some("recording"));
        assert_eq!(
            recording.keys[2].action.command(),
            Some("voice-dictate start")
        );
    }

    #[test]
    fn a_combined_command_button_counts_as_visible_home_navigation() {
        // A sub-bar whose buttons visibly return home through a command
        // needs no synthesized Home button, exactly like pure navigation.
        let config = NAMED.replace(
            "{ Text = \"F1\", Action = \"F1\" }",
            "{ Text = \"Stop\", Action = { Command = \"voice-dictate stop\", Layer = \"home\" } }",
        );
        let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
        let function = &layout.layers[1];
        assert_eq!(function.keys.len(), 1);
        assert_eq!(function.keys[0].text.as_deref(), Some("Stop"));
        // An invisible combined button still leaves room for the Home button.
        let config = config.replace("Text = \"Stop\"", "Text = \" \"");
        let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
        let function = &layout.layers[1];
        assert_eq!(function.keys.len(), 2);
        assert_eq!(function.keys[0].text.as_deref(), Some("Home"));
    }

    #[test]
    fn quick_key_entries_carry_a_label_an_optional_glyph_and_a_command() {
        // The shipped quick-keys bar: one example entry firing the
        // screenshot flow and returning home, behind an explicit Home
        // button that leaves without running anything.
        let example = include_str!("../../../share/tiny-dfr/multibar.toml");
        let (_, layout) = parse_config(BASE, Some(example), 2008).unwrap();
        let more = layout
            .layers
            .iter()
            .find(|layer| layer.name == "more")
            .unwrap();
        assert_eq!(more.keys.len(), 4);
        assert!(!more
            .keys
            .iter()
            .any(|key| matches!(key.action, ButtonAction::CycleTheme)
                || key.action.layer_target() == Some("themes")));
        assert_eq!(more.keys[0].text.as_deref(), Some("Home"));
        assert_eq!(
            more.keys[0].action,
            ButtonAction::SwitchLayer("home".into())
        );
        assert!(!more
            .keys
            .iter()
            .any(|key| key.text.as_deref() == Some("Settings")));
        let settings = layout.layers[layout.home]
            .keys
            .iter()
            .find(|key| key.text.as_deref() == Some("Settings"))
            .unwrap();
        assert_eq!(
            settings.action,
            ButtonAction::Command("touchbar-capture-user settings".into())
        );
        let screenshot = &more.keys[1];
        assert_eq!(screenshot.text.as_deref(), Some("Screenshot"));
        assert_eq!(screenshot.icon, None);
        assert_eq!(
            screenshot.action,
            ButtonAction::CommandLayer {
                command: "touchbar-capture-user screenshot".into(),
                layer: "home".into()
            }
        );

        // A captain's own quick keys are further entries in the same Keys
        // array - a Text label or an Icon glyph, an optional Accent, and
        // the Command to run, with or without a Layer switch. Entries
        // without a Layer switch stay on the bar, and the visible
        // home-targeting buttons keep the synthesized Home button from
        // duplicating the explicit one.
        let screenshot_line = "    { Text = \"Screenshot\", Action = { Command = \"touchbar-capture-user screenshot\", Layer = \"home\" } },\n";
        let with_own_keys = concat!(
            "    { Text = \"Screenshot\", Action = { Command = \"touchbar-capture-user screenshot\", Layer = \"home\" } },\n",
            "    { Icon = \"search\", Action = { Command = \"my-quick-tool\" } },\n",
            "    { Text = \"Lock\", Accent = \"amber\", Action = { Command = \"loginctl lock-session\" } },\n",
        );
        let config = example.replace(screenshot_line, with_own_keys);
        let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
        let more = layout
            .layers
            .iter()
            .find(|layer| layer.name == "more")
            .unwrap();
        assert_eq!(more.keys.len(), 6);
        assert_eq!(more.keys[2].icon.as_deref(), Some("search"));
        assert!(more.keys[2].text.is_none());
        assert_eq!(
            more.keys[2].action,
            ButtonAction::Command("my-quick-tool".into())
        );
        assert_eq!(more.keys[3].text.as_deref(), Some("Lock"));
        assert_eq!(more.keys[3].accent, Some(ButtonAccent::Amber));
        assert_eq!(
            more.keys[3].action,
            ButtonAction::Command("loginctl lock-session".into())
        );
        // A quick key naming an unknown layer is rejected like any other
        // button target.
        let invalid = config.replace(
            "Command = \"my-quick-tool\"",
            "Command = \"my-quick-tool\", Layer = \"missing\"",
        );
        assert!(parse_config(BASE, Some(&invalid), 2008).is_err());
    }

    #[test]
    fn theme_actions_parse_in_all_three_shapes_and_reject_bad_combinations() {
        let config = format!(
            "{NAMED}\n\
[[Layers]]\n\
Name = \"restyler\"\n\
Keys = [\n\
    {{ Text = \"Theme\", Action = \"CycleTheme\" }},\n\
    {{ Text = \"Nord\", Action = {{ Theme = \"Nord\" }} }},\n\
    {{ Text = \"Pick\", Action = {{ Theme = \"Nord\", Layer = \"home\" }} }},\n\
]"
        );
        let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
        let restyler = layout
            .layers
            .iter()
            .find(|layer| layer.name == "restyler")
            .unwrap();
        // The restyler bar visibly returns home through its Pick button,
        // so no extra Home button is synthesized.
        assert_eq!(restyler.keys.len(), 3);
        assert_eq!(restyler.keys[0].action, ButtonAction::CycleTheme);
        assert_eq!(
            restyler.keys[1].action,
            ButtonAction::ApplyTheme("Nord".into())
        );
        assert_eq!(
            restyler.keys[2].action,
            ButtonAction::ApplyThemeLayer {
                theme: "Nord".into(),
                layer: "home".into()
            }
        );
        // The helpers see through the theme shapes like the others.
        assert_eq!(restyler.keys[0].action.layer_target(), None);
        assert_eq!(restyler.keys[1].action.command(), None);
        assert_eq!(restyler.keys[2].action.layer_target(), Some("home"));
        // A theme combined with a command, an empty theme name, or an
        // unknown layer target is rejected like any other bad action.
        for invalid in [
            config.replace(
                "Action = { Theme = \"Nord\" }",
                "Action = { Theme = \"Nord\", Command = \"x\" }",
            ),
            config.replace(
                "Action = { Theme = \"Nord\" }",
                "Action = { Theme = \" \" }",
            ),
            config.replace(
                "Theme = \"Nord\", Layer = \"home\"",
                "Theme = \"Nord\", Layer = \"missing\"",
            ),
            config.replace("Action = \"CycleTheme\"", "Action = \"CycleThemes\""),
        ] {
            assert!(
                parse_config(BASE, Some(&invalid), 2008).is_err(),
                "accepted invalid theme action: {invalid}"
            );
        }
    }

    #[test]
    fn theme_tables_parse_into_the_resolved_set_and_reject_invalid_knobs() {
        let user = "\nThemesEnabled = true\nActiveTheme = \"Mine\"\n\
[Themes.Mine.Accents]\ncyan = \"#40E0D0\"\namber = \"40e0a0\"\n";
        let (base, _) = parse_config(BASE, Some(user), 2008).unwrap();
        let themes =
            ThemeSet::resolve(base.themes_enabled.unwrap_or(true), base.themes.clone()).unwrap();
        assert!(themes.contains("Mine"));
        assert!(themes.contains("Nord"), "the seeded set always rides along");
        // An unknown or blank ActiveTheme rejects the candidate config.
        for invalid in [
            "ActiveTheme = \"Nope\"\n",
            "ActiveTheme = \" \"\n",
            "ThemesEnabled = 'true'\n",
            "[Themes.Bad.Accents]\nmauve = \"#101010\"\n",
            "[Themes.Bad.Accents]\ncyan = \"10g010\"\n",
            "[Themes.Bad]\n[[Themes.Bad.DayCycleBands]]\nName = \"x\"\nStartHour = 24\nColor = \"#101010\"\n",
            "[Themes.Bad]\nSurprise = true\n",
            "[Themes.\" \"].Accents\n",
        ] {
            assert!(
                parse_config(BASE, Some(invalid), 2008).is_err(),
                "accepted invalid theme configuration: {invalid}"
            );
        }
    }

    #[test]
    fn the_theme_picker_is_synthesized_but_theme_buttons_are_opt_in() {
        // With the theme system on (the default), a `themes` layer is
        // synthesized listing every theme. Activation buttons are opt-in.
        let (_, layout) = parse_config(BASE, Some(NAMED_WITH_MORE), 2008).unwrap();
        let picker = layout
            .layers
            .iter()
            .find(|layer| layer.name == "themes")
            .unwrap();
        let seeded = crate::themes::builtin_themes().unwrap();
        assert_eq!(
            picker.keys.len(),
            seeded.names.len() + 1,
            "one Home button plus one button per theme"
        );
        assert_eq!(picker.keys[0].text.as_deref(), Some("Home"));
        assert_eq!(
            picker.keys[0].action,
            ButtonAction::SwitchLayer("home".into())
        );
        for (button, name) in picker.keys.iter().skip(1).zip(&seeded.names) {
            assert_eq!(button.text.as_deref(), Some(name.as_str()));
            assert_eq!(
                button.action,
                ButtonAction::ApplyThemeLayer {
                    theme: name.clone(),
                    layer: "home".into()
                }
            );
        }
        let more = layout
            .layers
            .iter()
            .find(|layer| layer.name == "more")
            .unwrap();
        assert!(!more.keys.iter().any(|key| {
            matches!(key.action, ButtonAction::CycleTheme)
                || key.action.layer_target() == Some("themes")
        }));
        // A user layer named `themes` replaces the synthesized picker,
        // while the default more bar remains free of theme controls.
        let config = format!(
            "{NAMED_WITH_MORE}\n\
[[Layers]]\n\
Name = \"themes\"\n\
Keys = [{{ Text = \"P1\" }}]"
        );
        let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
        let picker = layout
            .layers
            .iter()
            .find(|layer| layer.name == "themes")
            .unwrap();
        assert_eq!(picker.keys.len(), 2);
        assert_eq!(picker.keys[1].text.as_deref(), Some("P1"));
        // Explicitly configured buttons render on the more bar.
        let config = NAMED_WITH_MORE.replace(
            "    { Text = \"Sync\", Action = { Layer = \"home\" } },",
            "    { Text = \"Sync\", Action = { Layer = \"home\" } },\n    { Text = \"Palette\", Action = \"CycleTheme\" },\n    { Text = \"Looks\", Action = { Layer = \"themes\" } },",
        );
        let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
        let more = layout
            .layers
            .iter()
            .find(|layer| layer.name == "more")
            .unwrap();
        assert!(more
            .keys
            .iter()
            .any(|key| matches!(key.action, ButtonAction::CycleTheme)));
        assert!(more
            .keys
            .iter()
            .any(|key| { key.action.layer_target() == Some("themes") }));
        assert_eq!(
            more.keys
                .iter()
                .filter(|key| matches!(key.action, ButtonAction::CycleTheme))
                .count(),
            1
        );
        assert!(
            !more
                .keys
                .iter()
                .any(|key| key.text.as_deref() == Some("Theme")),
            "the synthesized cycle button yields to the author's own"
        );
        assert!(
            !more
                .keys
                .iter()
                .any(|key| key.text.as_deref() == Some("Themes")),
            "the synthesized opener yields to the author's own"
        );
        // Configs without a `more` bar stay untouched: their home bar gains
        // nothing and only the picker layer exists, reachable through the
        // author's own button.
        let (_, layout) = parse_config(BASE, Some(NAMED), 2008).unwrap();
        let home = &layout.layers[layout.home];
        assert!(!home.keys.iter().any(|key| {
            matches!(key.action, ButtonAction::CycleTheme)
                || key.action.layer_target() == Some("themes")
        }));
        assert!(layout.layers.iter().any(|layer| layer.name == "themes"));
    }

    #[test]
    fn disabling_the_theme_system_removes_its_layer_and_buttons_and_rejects_stale_targets() {
        let disabled = format!("ThemesEnabled = false\n{NAMED_WITH_MORE}");
        let (_, layout) = parse_config(BASE, Some(&disabled), 2008).unwrap();
        assert!(layout.layers.iter().all(|layer| layer.name != "themes"));
        let more = layout
            .layers
            .iter()
            .find(|layer| layer.name == "more")
            .unwrap();
        assert!(!more.keys.iter().any(|key| {
            matches!(key.action, ButtonAction::CycleTheme)
                || key
                    .action
                    .layer_target()
                    .is_some_and(|target| target == "themes")
        }));
        // A button still targeting the removed layer is an unknown target.
        let stale = disabled.replace(
            "    { Text = \"Sync\", Action = { Layer = \"home\" } },",
            "    { Text = \"Sync\", Action = { Layer = \"home\" } },\n    { Text = \"Looks\", Action = { Layer = \"themes\" } },",
        );
        assert!(parse_config(BASE, Some(&stale), 2008).is_err());
    }

    #[test]
    fn the_active_theme_builds_the_palette_and_bands_from_the_config_pin() {
        let tmp = TempDir::new();
        let base_path = tmp.0.join("base.toml");
        fs::write(&base_path, text_only_base()).unwrap();
        let user_path = tmp.0.join("config.toml");
        fs::write(
            &user_path,
            "\nThemesEnabled = true\nActiveTheme = \"Nord\"\n[Themes.Nord.Accents]\ncyan = \"#88C0D0\"\n",
        )
        .unwrap();
        let (cfg, _) = load_config(2008, &base_path, Some(&user_path), None).unwrap();
        assert_eq!(cfg.themes.active.as_deref(), Some("Nord"));
        assert_eq!(
            cfg.palette.accent(ButtonAccent::Cyan),
            (136.0 / 255.0, 192.0 / 255.0, 208.0 / 255.0),
            "the pin's accent override reaches the render palette"
        );
        assert_eq!(
            cfg.palette.accent(ButtonAccent::Amber),
            (235.0 / 255.0, 203.0 / 255.0, 139.0 / 255.0),
            "shadowing a built-in inherits its unspecified accents"
        );
        assert_eq!(
            cfg.day_cycle.background(14),
            (59.0 / 255.0, 66.0 / 255.0, 82.0 / 255.0),
            "the seed Nord bands paint the strip"
        );
        assert_eq!(
            cfg.global_day_cycle.background(14),
            crate::daycycle::DayCycle::default().background(14),
            "the theme-free cycle stays untouched for re-apply"
        );
        // Without a pin and without a state file the deck starts on the
        // live palette exactly as before themes existed.
        fs::write(&user_path, "\nThemesEnabled = true\n").unwrap();
        let (cfg, _) = load_config(2008, &base_path, Some(&user_path), None).unwrap();
        assert_eq!(cfg.themes.active.as_deref(), Some("Default"));
        assert_eq!(cfg.palette, crate::appearance::Palette::default());
        // A disabled system renders the live palette whatever it says.
        fs::write(
            &user_path,
            "ThemesEnabled = false\nActiveTheme = \"Nord\"\n",
        )
        .unwrap();
        let (cfg, _) = load_config(2008, &base_path, Some(&user_path), None).unwrap();
        assert_eq!(cfg.palette, crate::appearance::Palette::default());
    }

    #[test]
    fn the_theme_state_file_seeds_heals_and_yields_to_the_config_pin() {
        let tmp = TempDir::new();
        let base_path = tmp.0.join("base.toml");
        fs::write(&base_path, text_only_base()).unwrap();
        let user_path = tmp.0.join("config.toml");
        let state_path = tmp.0.join("active-theme");
        fs::write(&user_path, "ThemesEnabled = true\n").unwrap();
        // The last on-glass choice, remembered across restarts, applies
        // without any config pin - and is remembered for the next load.
        fs::write(&state_path, "Ember").unwrap();
        let (cfg, _) = load_config(2008, &base_path, Some(&user_path), Some(&state_path)).unwrap();
        assert_eq!(cfg.themes.active.as_deref(), Some("Ember"));
        // A config pin wins over the state file, and the file heals to
        // match so the two sources never fight.
        fs::write(&user_path, "ThemesEnabled = true\nActiveTheme = \"Nord\"\n").unwrap();
        let (cfg, _) = load_config(2008, &base_path, Some(&user_path), Some(&state_path)).unwrap();
        assert_eq!(cfg.themes.active.as_deref(), Some("Nord"));
        assert_eq!(fs::read_to_string(&state_path).unwrap(), "Nord");
        // A state entry naming a removed theme falls back to the first
        // theme and heals the file, so a stale file cannot wedge the bar.
        fs::write(&user_path, "ThemesEnabled = true\n").unwrap();
        fs::write(&state_path, "Atlantis").unwrap();
        let (cfg, _) = load_config(2008, &base_path, Some(&user_path), Some(&state_path)).unwrap();
        assert_eq!(cfg.themes.active.as_deref(), Some("Default"));
        assert_eq!(fs::read_to_string(&state_path).unwrap(), "Default");
        // write_persisted_theme is the on-glass path (the daemon's
        // ConfigManager::save_theme is its thin wrapper): atomic, and
        // the next load picks it up.
        write_persisted_theme(&state_path, "Mono").unwrap();
        let (cfg, _) = load_config(2008, &base_path, Some(&user_path), Some(&state_path)).unwrap();
        assert_eq!(cfg.themes.active.as_deref(), Some("Mono"));
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
            NAMED.replace("Action = \"F1\"", "Action = { Command = \"\" }"),
            NAMED.replace("Action = \"F1\"", "Action = { Command = \"   \" }"),
            NAMED.replace("Action = \"F1\"", "Action = {}"),
            NAMED.replace("Action = \"F1\"", "Action = { Command = 42 }"),
            NAMED.replace(
                "Action = \"F1\"",
                "Action = { Layer = \"home\", Extra = true }",
            ),
            NAMED.replace(
                "Action = \"F1\"",
                "Action = { Command = \"x\", Layer = \"missing\" }",
            ),
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
    fn battery_cycle_settings_merge_and_legacy_configs_have_defaults() {
        let tmp = TempDir::new();
        let base_path = tmp.0.join("base.toml");
        let user_path = tmp.0.join("config.toml");
        let mut legacy: toml::Value = toml::from_str(&text_only_base()).unwrap();
        legacy.as_table_mut().unwrap().remove("BatteryCycleEnabled");
        legacy
            .as_table_mut()
            .unwrap()
            .remove("BatteryCycleTimeoutSecs");
        fs::write(&base_path, toml::to_string(&legacy).unwrap()).unwrap();
        let (config, deck) = load_config(2008, &base_path, None, None).unwrap();
        assert!(deck.legacy);
        assert!(config.battery_cycle_enabled);
        assert_eq!(config.battery_cycle_timeout_secs, 15);
        fs::write(
            &user_path,
            "BatteryCycleEnabled = false\nBatteryCycleTimeoutSecs = 7",
        )
        .unwrap();
        let (config, _) = load_config(2008, &base_path, Some(&user_path), None).unwrap();
        assert!(!config.battery_cycle_enabled);
        assert_eq!(config.battery_cycle_timeout_secs, 7);
        for invalid in [
            "BatteryCycleTimeoutSecs = 0",
            "BatteryCycleTimeoutSecs = -1",
            "BatteryCycleTimeoutSecs = '15'",
            "BatteryCycleEnabled = 'false'",
        ] {
            assert!(parse_config(BASE, Some(invalid), 2008).is_err());
        }
    }

    #[test]
    fn battery_escalation_settings_merge_and_legacy_configs_have_defaults() {
        let tmp = TempDir::new();
        let base_path = tmp.0.join("base.toml");
        let user_path = tmp.0.join("config.toml");
        // A base config predating the escalation keeps working with defaults.
        let mut legacy: toml::Value = toml::from_str(&text_only_base()).unwrap();
        for key in [
            "BatteryWarningThreshold",
            "BatteryCriticalThreshold",
            "BatteryWarningColor",
            "BatteryCriticalColor",
        ] {
            legacy.as_table_mut().unwrap().remove(key);
        }
        fs::write(&base_path, toml::to_string(&legacy).unwrap()).unwrap();
        let (config, _) = load_config(2008, &base_path, None, None).unwrap();
        assert_eq!(config.battery_warning_threshold, 25);
        assert_eq!(config.battery_critical_threshold, 10);
        assert_eq!(config.battery_warning_color, ButtonAccent::Orange);
        assert_eq!(config.battery_critical_color, ButtonAccent::Red);
        // User overrides win over the base defaults, keys may be set singly.
        fs::write(
            &user_path,
            "BatteryWarningThreshold = 30\nBatteryCriticalColor = \"violet\"",
        )
        .unwrap();
        let (config, _) = load_config(2008, &base_path, Some(&user_path), None).unwrap();
        assert_eq!(config.battery_warning_threshold, 30);
        assert_eq!(config.battery_critical_threshold, 10);
        assert_eq!(config.battery_warning_color, ButtonAccent::Orange);
        assert_eq!(config.battery_critical_color, ButtonAccent::Violet);
        // Thresholds above 100 can never trigger and inverted tiers only
        // surprise, so both are rejected like the other invalid knobs.
        for invalid in [
            "BatteryWarningThreshold = 101",
            "BatteryCriticalThreshold = 101",
            "BatteryCriticalThreshold = 30\nBatteryWarningThreshold = 25",
            "BatteryCriticalThreshold = 30",
            "BatteryWarningThreshold = '25'",
            "BatteryWarningColor = \"mauve\"",
            "BatteryCriticalColor = 42",
        ] {
            assert!(
                parse_config(BASE, Some(invalid), 2008).is_err(),
                "accepted invalid battery escalation configuration: {invalid}"
            );
        }
    }

    #[test]
    fn pet_settings_merge_with_legacy_defaults_and_reject_invalid_values() {
        let tmp = TempDir::new();
        let base_path = tmp.0.join("base.toml");
        let user_path = tmp.0.join("config.toml");
        // A base config predating the pet keeps working with defaults.
        let mut legacy: toml::Value = toml::from_str(&text_only_base()).unwrap();
        legacy.as_table_mut().unwrap().remove("PetEnabled");
        legacy.as_table_mut().unwrap().remove("PetVariant");
        legacy.as_table_mut().unwrap().remove("PetEnergy");
        fs::write(&base_path, toml::to_string(&legacy).unwrap()).unwrap();
        let (config, _) = load_config(2008, &base_path, None, None).unwrap();
        assert!(config.pet_enabled);
        assert_eq!(config.pet_variant, PetVariant::Cat);
        assert_eq!(config.pet_energy, PetEnergy::Calm);
        // User overrides win over the base defaults.
        fs::write(
            &user_path,
            "PetEnabled = false\nPetVariant = \"fox\"\nPetEnergy = \"rowdy\"",
        )
        .unwrap();
        let (config, _) = load_config(2008, &base_path, Some(&user_path), None).unwrap();
        assert!(!config.pet_enabled);
        assert_eq!(config.pet_variant, PetVariant::Fox);
        assert_eq!(config.pet_energy, PetEnergy::Rowdy);
        // Invalid values are rejected like the other knobs.
        for invalid in [
            "PetVariant = \"dog\"",
            "PetEnergy = \"zoomy\"",
            "PetEnabled = 'true'",
            "PetVariant = 42",
        ] {
            assert!(
                parse_config(BASE, Some(invalid), 2008).is_err(),
                "accepted invalid pet configuration: {invalid}"
            );
        }
        // The shipped base config carries valid pet defaults.
        let (config, _) = parse_config(BASE, None, 2008).unwrap();
        assert!(config.pet_enabled.is_some());
        assert_eq!(config.pet_variant.as_deref(), Some("cat"));
        assert_eq!(config.pet_energy.as_deref(), Some("calm"));
    }

    #[test]
    fn day_cycle_settings_merge_with_defaults_overrides_and_reject_invalid_values() {
        let tmp = TempDir::new();
        let base_path = tmp.0.join("base.toml");
        let user_path = tmp.0.join("config.toml");
        // A base config predating the day cycle keeps working with defaults.
        let mut legacy: toml::Value = toml::from_str(&text_only_base()).unwrap();
        legacy.as_table_mut().unwrap().remove("DayCycleEnabled");
        legacy.as_table_mut().unwrap().remove("DayCycleBands");
        fs::write(&base_path, toml::to_string(&legacy).unwrap()).unwrap();
        let (config, _) = load_config(2008, &base_path, None, None).unwrap();
        assert!(config.day_cycle.enabled);
        assert_eq!(config.day_cycle.band_at(14).unwrap().name, "day");
        assert_eq!(
            config.day_cycle.background(2),
            (4.0 / 255.0, 6.0 / 255.0, 13.0 / 255.0),
            "the small hours wrap around midnight to the night band"
        );
        // Disabling keeps the plain static black background at every hour.
        fs::write(&user_path, "DayCycleEnabled = false").unwrap();
        let (config, _) = load_config(2008, &base_path, Some(&user_path), None).unwrap();
        for hour in 0..=23 {
            assert_eq!(config.day_cycle.band_at(hour), None);
            assert_eq!(config.day_cycle.background(hour), (0.0, 0.0, 0.0));
        }
        // Custom bands replace the defaults as a whole and override the color
        // literals case-insensitively, with or without the leading hash.
        fs::write(
            &user_path,
            r##"
[[DayCycleBands]]
Name = "work"
StartHour = 9
Color = "#102030"
[[DayCycleBands]]
Name = "off"
StartHour = 19
Color = "301020"
"##,
        )
        .unwrap();
        let (config, _) = load_config(2008, &base_path, Some(&user_path), None).unwrap();
        let off = config.day_cycle.band_at(2).unwrap();
        assert_eq!((off.name.as_str(), off.start_hour), ("off", 19));
        assert_eq!(off.rgb(), (48.0 / 255.0, 16.0 / 255.0, 32.0 / 255.0));
        let work = config.day_cycle.band_at(18).unwrap();
        assert_eq!(work.name, "work");
        assert_eq!(work.rgb(), (16.0 / 255.0, 32.0 / 255.0, 48.0 / 255.0));
        // Invalid values reject the candidate configuration like the other knobs.
        let band = |name: &str, hour: &str, color: &str| {
            format!(
                "DayCycleEnabled = true\n[[DayCycleBands]]\nName = {name}\nStartHour = {hour}\nColor = {color}\n"
            )
        };
        for invalid in [
            "DayCycleBands = []".to_owned(),
            "DayCycleEnabled = 'true'".to_owned(),
            "DayCycleEnabled = 1".to_owned(),
            band("", "5", "\"#101010\""),
            band("  ", "5", "\"#101010\""),
            band("same", "5", "\"#101010\"") + &band("same", "7", "\"#202020\""),
            band("early", "5", "\"#101010\"") + &band("late", "5", "\"#202020\""),
            band("night", "24", "\"#101010\""),
            band("night", "-1", "\"#101010\""),
            band("night", "5", "\"10101\""),
            band("night", "5", "\"#10g010\""),
            band("night", "5", "1010103"),
            band("night", "5", ""),
            // A missing Color, and an unknown field inside the band table.
            "DayCycleEnabled = true\n[[DayCycleBands]]\nName = \"night\"\nStartHour = 5\n"
                .to_owned(),
            band("night", "5", "\"#101010\"") + "Extra = 1\n",
        ] {
            assert!(
                parse_config(BASE, Some(&invalid), 2008).is_err(),
                "accepted invalid day-cycle configuration: {invalid}"
            );
        }
        // The shipped base config carries the shipped default enablement.
        let (base_cfg, _) = parse_config(BASE, None, 2008).unwrap();
        assert!(base_cfg.day_cycle_enabled.unwrap());
    }

    #[test]
    fn base_without_double_press_loads_with_the_feature_disabled() {
        // Distro-installed base configs predate DoublePressSwitchLayers and the
        // shipped multibar example does not set it either (README install path).
        let mut installed: toml::Value = toml::from_str(BASE).unwrap();
        assert!(installed
            .as_table_mut()
            .unwrap()
            .remove("DoublePressSwitchLayers")
            .is_some());
        let tmp = TempDir::new();
        let base_path = tmp.0.join("base.toml");
        fs::write(&base_path, toml::to_string(&installed).unwrap()).unwrap();
        let user_path = tmp.0.join("config.toml");
        fs::write(
            &user_path,
            include_str!("../../../share/tiny-dfr/multibar.toml"),
        )
        .unwrap();
        let (cfg, deck) = load_config(2008, &base_path, Some(&user_path), None).unwrap();
        assert_eq!(cfg.double_press_switch_layers, 0);
        assert!(!deck.legacy);
        // The multibar's media bar carries the Viz button, so the deck
        // also synthesized its full-bar visualizer view; the named layers
        // are home, function, media, more, and pet, plus the
        // synthesized themes picker.
        assert_eq!(deck.layers.len(), 7);
        assert!(deck.names.contains_key("visualizer:media"));
    }

    #[test]
    fn pet_enabled_synthesizes_a_pet_layer_and_home_button() {
        for width in [2008, 2170] {
            let (_, layout) = parse_config(BASE, Some(NAMED), width).unwrap();
            let pet = layout
                .layers
                .iter()
                .find(|layer| layer.name == "pet")
                .expect("the pet layer is synthesized");
            let offset = usize::from(width >= 2170);
            let home_button = &pet.keys[offset];
            assert_eq!(home_button.text.as_deref(), Some("Home"));
            assert_eq!(home_button.action, ButtonAction::SwitchLayer("home".into()));
            let ground = &pet.keys[offset + 1];
            assert_eq!(ground.stretch, Some(PET_LAYER_STRETCH));
            assert!(ground.text.is_none() && ground.icon.is_none());
            // Without a More layer, the home bar keeps a Pet opener.
            let home = &layout.layers[layout.home];
            let functions = home
                .keys
                .iter()
                .position(|key| key.action == ButtonAction::SwitchLayer("function".into()))
                .unwrap();
            let pet_button = &home.keys[functions + 1];
            assert_eq!(pet_button.text.as_deref(), Some("Pet"));
            assert_eq!(pet_button.action, ButtonAction::SwitchLayer("pet".into()));
        }
    }

    #[test]
    fn pet_disabled_restores_the_pre_pet_home_bar_and_layer_list() {
        let config = format!("PetEnabled = false\n{NAMED}");
        let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
        assert_eq!(layout.layers.len(), 3);
        assert!(layout.layers.iter().all(|layer| layer.name != "pet"));
        let home = &layout.layers[layout.home];
        assert_eq!(home.keys.len(), 1);
        assert_eq!(home.keys[0].text.as_deref(), Some("Functions"));
        assert!(!home
            .keys
            .iter()
            .any(|key| key.text.as_deref() == Some("Pet")));
        // The disabled pet is also absent from legacy layouts.
        let (_, legacy) = parse_config(
            &text_only_base().replace("PetEnabled = true", "PetEnabled = false"),
            None,
            2008,
        )
        .unwrap();
        assert_eq!(legacy.layers.len(), 3);
        assert!(legacy.layers.iter().all(|layer| layer.name != "pet"));
    }

    #[test]
    fn a_user_pet_layer_replaces_the_synthesized_playground() {
        let config = format!(
            "{NAMED}\n\
[[Layers]]\n\
Name = \"pet\"\n\
Keys = [{{ Text = \"P1\" }}]\n"
        );
        let (_, layout) = parse_config(BASE, Some(&config), 2008).unwrap();
        assert_eq!(layout.layers.len(), 4);
        let pet = &layout.layers[2];
        assert_eq!(pet.name, "pet");
        // The user's buttons are kept and gain only the generic Home button,
        // never the synthesized playground spacer.
        assert_eq!(pet.keys.len(), 2);
        assert_eq!(pet.keys[0].text.as_deref(), Some("Home"));
        assert_eq!(pet.keys[1].text.as_deref(), Some("P1"));
        // The home bar still gets its Pet button, now targeting the user's
        // layer; an author-provided visible button would replace it.
        let home = &layout.layers[layout.home];
        assert!(home
            .keys
            .iter()
            .any(|key| key.action == ButtonAction::SwitchLayer("pet".into())));
        let with_own_button = config.replace(
            "{ Text = \"Functions\", Action = { Layer = \"function\" } }",
            "{ Text = \"Functions\", Action = { Layer = \"function\" } }, { Text = \"Kitty\", Action = { Layer = \"pet\" } }",
        );
        let (_, layout) = parse_config(BASE, Some(&with_own_button), 2008).unwrap();
        let home = &layout.layers[layout.home];
        assert_eq!(
            home.keys
                .iter()
                .filter(|key| key.action == ButtonAction::SwitchLayer("pet".into()))
                .count(),
            1
        );
        assert!(home
            .keys
            .iter()
            .any(|key| key.text.as_deref() == Some("Kitty")));
        assert!(!home
            .keys
            .iter()
            .any(|key| key.text.as_deref() == Some("Pet")));
    }

    #[test]
    fn disabling_the_pet_rejects_stale_references_to_the_pet_layer() {
        // With the pet disabled the synthesized layer no longer exists, so a
        // button still naming it is an unknown target like any other.
        let config = format!("PetEnabled = false\n{NAMED}").replace(
            "{ Text = \"Functions\", Action = { Layer = \"function\" } }",
            "{ Text = \"Functions\", Action = { Layer = \"function\" } }, { Text = \"Pet\", Action = { Layer = \"pet\" } }",
        );
        assert!(parse_config(BASE, Some(&config), 2008).is_err());
    }

    /// FNV-1a over the rendered pixels: a stable, version-independent
    /// digest so the golden hashes below never depend on the compiler's
    /// std hash implementation.
    fn frame_hash(surface: &mut cairo::ImageSurface) -> u64 {
        let data = surface.data().unwrap();
        let mut hash: u64 = 0xcbf29ce484222325;
        for byte in data.iter() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }

    /// The shipped multibar deck, one frame per deterministic layer
    /// (home with its closed sliders, the media bar with its idle
    /// widgets, the function keys), rendered headless exactly as the
    /// review renderers do, with the pixel digest pinned. The layers a
    /// wall clock or a live feed would move - the pet and the optional
    /// voice profile's recording waveform - stay covered by their own
    /// widget tests instead of this frame pin. Any change to the draw
    /// path that moves a single pixel of these frames breaks this test.
    #[test]
    fn shipped_multibar_frames_match_the_pinned_pixel_digests() {
        let tmp = TempDir::new();
        let base_path = tmp.0.join("base.toml");
        let user_path = tmp.0.join("config.toml");
        fs::write(&base_path, BASE).unwrap();
        fs::write(
            &user_path,
            include_str!("../../../share/tiny-dfr/multibar.toml"),
        )
        .unwrap();
        let (cfg, mut deck) = load_config(2008, &base_path, Some(&user_path), None).unwrap();
        let background = cfg.day_cycle.background(12);
        let frame = |deck: &mut crate::LayerDeck, name: &str| -> u64 {
            let index = deck.names[name];
            deck.switch_to(index, &mut |_, _| {});
            let mut portrait =
                cairo::ImageSurface::create(cairo::Format::ARgb32, 64, 2048).unwrap();
            deck.slider.pixel_shift_width = 0;
            deck.slider.pixel_shift_x = 0.0;
            let volume = deck.slider_render();
            deck.current_mut().draw(
                &cfg,
                (2008, 60),
                &portrait,
                (0.0, 0.0),
                true,
                volume,
                background,
            );
            // Optional review artifact from the exact pinned frame, not a
            // second renderer or a guessed replacement digest. No output by
            // default; the supervised gate supplies an isolated directory.
            if let Some(dir) = std::env::var_os("DFR_DECK_REVIEW_OUTPUT") {
                fs::create_dir_all(&dir).unwrap();
                let horizontal =
                    cairo::ImageSurface::create(cairo::Format::ARgb32, 2008, 60).unwrap();
                let context = cairo::Context::new(&horizontal).unwrap();
                context.translate(0.0, 60.0);
                context.rotate((-90.0_f64).to_radians());
                context.set_source_surface(&portrait, 0.0, 0.0).unwrap();
                context.paint().unwrap();
                horizontal
                    .write_to_png(
                        &mut fs::File::create(Path::new(&dir).join(format!("golden-{name}.png")))
                            .unwrap(),
                    )
                    .unwrap();
            }
            frame_hash(&mut portrait)
        };
        for (name, golden) in [
            ("home", GOLDEN_HOME),
            ("media", GOLDEN_MEDIA),
            ("function", GOLDEN_FUNCTION),
        ] {
            let first = frame(&mut deck, name);
            // Rendering the identical state twice must be pixel-stable
            // within a run before the golden is meaningful.
            let repeat = frame(&mut deck, name);
            assert_eq!(first, repeat, "layer {name} is not pixel-stable");
            assert_eq!(
                first, golden,
                "layer {name} frame drifted from the pinned digest"
            );
        }
    }
}
