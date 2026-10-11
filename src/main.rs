use anyhow::{anyhow, Result};
use cairo::{Antialias, Context, Format, ImageSurface, Surface};
use chrono::{
    format::{Item as ChronoItem, StrftimeItems},
    Local, Locale, Timelike,
};
use drm::control::ClipRect;
use freedesktop_icons::lookup;
use input::{
    event::{
        device::DeviceEvent,
        keyboard::{KeyState, KeyboardEvent, KeyboardEventTrait},
        touch::{TouchEvent, TouchEventPosition, TouchEventSlot},
        Event, EventTrait,
    },
    Device as InputDevice, Libinput, LibinputInterface,
};
use input_linux::{uinput::UInputHandle, EventKind, Key, SynchronizeKind};
use input_linux_sys::{input_event, input_id, timeval, uinput_setup};
use libc::{c_char, O_ACCMODE, O_RDONLY, O_RDWR, O_WRONLY};
use librsvg_rebind::{prelude::HandleExt, Handle, Rectangle};
use nix::{
    errno::Errno,
    sys::{
        epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags},
        signal::{SigSet, Signal},
    },
};
use privdrop::PrivDrop;
use std::{
    cmp::min,
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsFd, AsRawFd},
        unix::{fs::OpenOptionsExt, io::OwnedFd, net::UnixStream},
    },
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
    time::{Duration, Instant},
};
use udev::MonitorBuilder;

mod appearance;
mod backlight;
mod command;
mod config;
mod daycycle;
mod display;
mod fonts;
mod level_control;
mod now_playing;
mod pet;
mod pixel_shift;
mod slider;
mod waveform;

use crate::config::ConfigManager;
use appearance::ButtonAccent;
use backlight::BacklightManager;
use command::CommandRunner;
use config::{ButtonAction, ButtonConfig, Config};
#[cfg(test)]
use daycycle::DayCycle;
use display::DrmBackend;
#[cfg(test)]
use level_control::Request as LevelRequest;
use level_control::{LevelControl, State as LevelState};
use now_playing::{Feed, NowPlaying, NowPlayingWidget};
use pet::{Bar as PetBar, PetManager, Span as PetSpan};
use pixel_shift::{PixelShiftManager, PIXEL_SHIFT_WIDTH_PX};
use slider::{Geometry, Kind, MuteWatch, Overlay, Render, DRAG_THRESHOLD_PX, LINGER};
use waveform::{MicLevels, MonitorLevels, Sample as MicSample, Style, WaveformWidget};

const BUTTON_SPACING_PX: i32 = 16;
const BUTTON_COLOR_INACTIVE: f64 = 0.200;
const BUTTON_COLOR_ACTIVE: f64 = 0.400;
const DEFAULT_ICON_SIZE: i32 = 40;
const TIMEOUT_MS: i32 = 10 * 1000;

/// Charging is the only state that changes how a battery renders: charging
/// sources draw the charging icons and keep the charging green, every other
/// state draws the plain icons and escalates by its charge fraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BatteryState {
    NotCharging,
    Charging,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BatterySource {
    id: String,
    label: String,
    device: bool,
    capacity: u32,
    state: BatteryState,
}

#[derive(Default)]
struct BatteryWidget {
    sources: Vec<BatterySource>,
    // None always means the system battery, even if it is currently absent.
    selected: Option<String>,
    return_at: Option<Instant>,
    cycling: bool,
    timeout: Duration,
}

/// How far the displayed battery has fallen, resolved against the
/// configured thresholds. Strictly-below semantics keep the historical
/// reading at the shipped defaults: a battery exactly at a threshold
/// still shows the milder tier, and the old hardcoded `capacity < 10`
/// critical red stays the critical default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BatteryAlert {
    Healthy,
    Warning,
    Critical,
}

/// The battery widget's escalation knobs, resolved once per config load:
/// where the warning and critical tiers start and the accent each tier
/// paints. The tiers are evaluated against the currently displayed
/// battery, so a cycled device battery escalates independently of the
/// system battery whenever it is the one on show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BatteryEscalation {
    warning_threshold: u32,
    critical_threshold: u32,
    warning_accent: ButtonAccent,
    critical_accent: ButtonAccent,
}

impl BatteryEscalation {
    fn from_config(config: &Config) -> Self {
        Self {
            warning_threshold: config.battery_warning_threshold,
            critical_threshold: config.battery_critical_threshold,
            warning_accent: config.battery_warning_color,
            critical_accent: config.battery_critical_color,
        }
    }

    fn alert(&self, capacity: u32) -> BatteryAlert {
        if capacity < self.critical_threshold {
            BatteryAlert::Critical
        } else if capacity < self.warning_threshold {
            BatteryAlert::Warning
        } else {
            BatteryAlert::Healthy
        }
    }
}

impl BatteryWidget {
    fn source(&self) -> Option<&BatterySource> {
        self.sources.iter().find(|source| match &self.selected {
            Some(id) => source.device && source.id == *id,
            None => !source.device,
        })
    }
    fn can_cycle(&self) -> bool {
        self.cycling && self.sources.iter().any(|source| source.device)
    }

    fn label(&self) -> Option<&str> {
        if let Some(source) = self.source().filter(|source| source.device) {
            Some(&source.label)
        } else if self.can_cycle() {
            Some("SYS")
        } else {
            None
        }
    }

    fn cycle(&mut self, now: Instant) -> bool {
        if !self.can_cycle() {
            return false;
        }
        let mut devices = self.sources.iter().filter(|source| source.device);
        if let Some(id) = &self.selected {
            devices.find(|source| source.id == *id);
        }
        self.selected = devices.next().map(|source| source.id.clone());
        self.return_at = self.selected.as_ref().map(|_| now + self.timeout);
        true
    }

    // Return whether a source switch occurred, so the input owner can release
    // held keys and discard touches just as it does for a layer switch.
    fn refresh(
        &mut self,
        sources: &[BatterySource],
        enabled: bool,
        timeout: Duration,
        now: Instant,
    ) -> bool {
        if self.sources != sources {
            self.sources = sources.to_vec();
        }
        self.cycling = enabled;
        self.timeout = timeout;
        if self.selected.is_some()
            && (!enabled
                || self.source().is_none()
                || self.return_at.is_some_and(|deadline| now >= deadline))
        {
            self.selected = None;
            self.return_at = None;
            return true;
        }
        false
    }
}

fn rounded_rect(c: &Context, left: f64, top: f64, width: f64, height: f64, radius: f64) {
    let radius = radius.min(width / 2.0).min(height / 2.0).max(0.0);
    let right = left + width;
    let bottom = top + height;
    c.new_sub_path();
    c.arc(
        right - radius,
        top + radius,
        radius,
        (-90.0f64).to_radians(),
        (0.0f64).to_radians(),
    );
    c.arc(
        right - radius,
        bottom - radius,
        radius,
        (0.0f64).to_radians(),
        (90.0f64).to_radians(),
    );
    c.arc(
        left + radius,
        bottom - radius,
        radius,
        (90.0f64).to_radians(),
        (180.0f64).to_radians(),
    );
    c.arc(
        left + radius,
        top + radius,
        radius,
        (180.0f64).to_radians(),
        (270.0f64).to_radians(),
    );
    c.close_path();
}

// Sysfs enumeration and reads stay on this worker. Rendering and touch input
// only consume cached snapshots; a nonblocking socket wakes the existing epoll.
struct BatteryMonitor {
    requests: SyncSender<()>,
    updates: Receiver<Vec<BatterySource>>,
    wake: UnixStream,
    sources: Vec<BatterySource>,
}

impl BatteryMonitor {
    fn new(path: PathBuf) -> Self {
        let (requests, refresh) = mpsc::sync_channel(1);
        let (snapshots, updates) = mpsc::channel();
        let (wake, mut notify) = UnixStream::pair().unwrap();
        wake.set_nonblocking(true).unwrap();
        notify.set_nonblocking(true).unwrap();
        thread::spawn(move || loop {
            if snapshots.send(read_battery_sources(&path)).is_err() {
                break;
            }
            let _ = notify.write(&[1]);
            match refresh.recv_timeout(Duration::from_secs(10)) {
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                _ => {
                    // Coalesce hotplug/change bursts into one fresh snapshot.
                    while refresh.try_recv().is_ok() {}
                }
            }
        });
        Self {
            requests,
            updates,
            wake,
            sources: Vec::new(),
        }
    }

    fn request_refresh(&self) {
        let _ = self.requests.try_send(());
    }

    fn update(&mut self) {
        while matches!(self.wake.read(&mut [0; 64]), Ok(n) if n > 0) {}
        if let Some(sources) = self.updates.try_iter().last() {
            self.sources = sources;
        }
    }
}

struct BatteryImages {
    plain: Vec<Handle>,
    charging: Vec<Handle>,
    bolt: Handle,
}

#[derive(Eq, PartialEq, Copy, Clone)]
enum BatteryIconMode {
    Percentage,
    Icon,
    Both,
}

impl BatteryIconMode {
    fn should_draw_icon(self) -> bool {
        self != BatteryIconMode::Percentage
    }
    fn should_draw_text(self) -> bool {
        self != BatteryIconMode::Icon
    }
}

enum ButtonImage {
    Text(String),
    Svg(Handle),
    Bitmap(ImageSurface),
    Time(Vec<ChronoItem<'static>>, Locale),
    Battery(BatteryWidget, BatteryIconMode, BatteryImages),
    Waveform(WaveformWidget),
    NowPlaying(NowPlayingWidget),
    Spacer,
}

struct Button {
    image: ButtonImage,
    changed: bool,
    active: bool,
    action: ButtonAction,
    icon_width: f64,
    icon_height: f64,
    accent: Option<ButtonAccent>,
    // The closed-slider mute mark: set on volume slider buttons from the
    // deck's mute watch, drawn as a slash over the button's centered
    // content. Layout and hit targets are never touched by it.
    muted: bool,
}

fn try_load_svg(path: &str) -> Result<ButtonImage> {
    Ok(ButtonImage::Svg(
        Handle::from_file(path).map_err(|_| anyhow!("failed to load image"))?,
    ))
}

fn try_load_png(path: impl AsRef<Path>, icon_width: i32, icon_height: i32) -> Result<ButtonImage> {
    let mut file = File::open(path)?;
    let surf = ImageSurface::create_from_png(&mut file)?;
    if surf.height() == icon_height && surf.width() == icon_width {
        return Ok(ButtonImage::Bitmap(surf));
    }
    let resized = ImageSurface::create(Format::ARgb32, icon_width, icon_height).unwrap();
    let c = Context::new(&resized).unwrap();
    c.scale(
        icon_width as f64 / surf.width() as f64,
        icon_height as f64 / surf.height() as f64,
    );
    c.set_source_surface(surf, 0.0, 0.0).unwrap();
    c.set_antialias(Antialias::Best);
    c.paint().unwrap();
    Ok(ButtonImage::Bitmap(resized))
}

fn try_load_image(
    name: impl AsRef<str>,
    theme: Option<impl AsRef<str>>,
    icon_width: i32,
    icon_height: i32,
) -> Result<ButtonImage> {
    let name = name.as_ref();
    let locations;

    // Load list of candidate locations
    if let Some(theme) = theme {
        // Freedesktop icons
        let theme = theme.as_ref();
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

        // .flatten() removes `None` and unwraps `Some` values
        locations = candidates.into_iter().flatten().collect();
    } else {
        // Standard file icons
        locations = vec![
            PathBuf::from(format!("/etc/tiny-dfr/{name}.svg")),
            PathBuf::from(format!("/etc/tiny-dfr/{name}.png")),
            PathBuf::from(format!("/usr/share/tiny-dfr/{name}.svg")),
            PathBuf::from(format!("/usr/share/tiny-dfr/{name}.png")),
        ];
    };

    // Try to load each candidate
    let mut last_err = anyhow!("no suitable icon path was found"); // in case locations is empty

    for location in locations {
        let result = match location.extension().and_then(|s| s.to_str()) {
            Some("png") => try_load_png(&location, icon_width, icon_height),
            Some("svg") => try_load_svg(
                location
                    .to_str()
                    .ok_or(anyhow!("image path is not unicode"))?,
            ),
            _ => Err(anyhow!("invalid file extension")),
        };

        match result {
            Ok(image) => return Ok(image),
            Err(err) => {
                last_err = err.context(format!("while loading path {}", location.display()));
            }
        };
    }

    // if function hasn't returned by now, all sources have been exhausted
    Err(last_err.context(format!("failed loading all possible paths for icon {name}")))
}

fn read_battery_sources(root: &Path) -> Vec<BatterySource> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut sources = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let read = |name| fs::read_to_string(path.join(name)).ok();
        if read("type").is_none_or(|kind| kind.trim() != "Battery")
            || read("present").is_some_and(|present| present.trim() == "0")
        {
            continue;
        }
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let scope = read("scope");
        let priority = match scope.as_deref().map(str::trim) {
            Some("System") => 0,
            None => 1, // Older system batteries may omit scope.
            Some("Device") => 2,
            _ => continue,
        };
        let device = priority == 2;
        // hidpp supplies can remain in sysfs with a cached capacity while the
        // mouse is disconnected; those drivers expose online rather than present.
        if device && read("online").is_some_and(|online| online.trim() == "0") {
            continue;
        }
        let capacity = read("capacity").and_then(|s| s.trim().parse::<u32>().ok());
        let charge = || {
            let now = read("charge_now")?.trim().parse::<f64>().ok()?;
            let full = read("charge_full")?.trim().parse::<f64>().ok()?;
            (now.is_finite() && full.is_finite() && now >= 0.0 && full > 0.0)
                .then(|| ((now / full) * 100.0).round() as u32)
        };
        // Keep the T2 system-battery charge ratio; peripherals generally only
        // expose capacity, including hidpp mice on x86_64.
        let capacity = if cfg!(target_arch = "x86_64") && !device {
            charge().or(capacity)
        } else {
            capacity.or_else(charge)
        };
        let Some(capacity) = capacity else {
            continue;
        };
        let capacity = capacity.min(100);
        let state = match read("status").as_deref().map(str::trim) {
            Some("Charging" | "Full") => BatteryState::Charging,
            _ => BatteryState::NotCharging,
        };
        let label: String = read("model_name")
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| id.clone())
            .trim()
            .chars()
            .filter(|ch| !ch.is_control())
            .take(10)
            .collect();
        sources.push((
            priority,
            BatterySource {
                id,
                label,
                device,
                capacity,
                state,
            },
        ));
    }
    sources.sort_by(|(a, left), (b, right)| (a, &left.id).cmp(&(b, &right.id)));
    let mut sources: Vec<_> = sources.into_iter().map(|(_, source)| source).collect();
    let mut system_seen = false;
    sources.retain(|source| {
        if source.device {
            true
        } else if system_seen {
            false
        } else {
            system_seen = true;
            true
        }
    });
    // Distinguish devices even when their model names are identical.
    for (i, source) in sources
        .iter_mut()
        .filter(|source| source.device)
        .enumerate()
    {
        source.label = format!("{} {}", source.label.trim_end(), i + 1);
    }
    sources
}

impl Button {
    fn with_config(cfg: ButtonConfig) -> Button {
        let mut button = if let Some(text) = cfg.text {
            Button::new_text(text, cfg.action)
        } else if let Some(icon) = cfg.icon {
            Button::new_icon(
                &icon,
                cfg.theme,
                cfg.action,
                cfg.icon_width.unwrap_or(DEFAULT_ICON_SIZE),
                cfg.icon_height.unwrap_or(DEFAULT_ICON_SIZE),
            )
        } else if let Some(time) = cfg.time {
            Button::new_time(cfg.action, &time, cfg.locale.as_deref())
        } else if let Some(battery_mode) = cfg.battery {
            Button::new_battery(cfg.action, battery_mode, cfg.theme)
        } else if cfg.waveform.unwrap_or(false) {
            Button::new_waveform(
                cfg.action,
                cfg.waveform_style.as_deref(),
                cfg.accent,
                cfg.waveform_smoothing,
                cfg.waveform_lag,
            )
        } else if cfg.visualizer.unwrap_or(false) {
            Button::new_visualizer(
                cfg.action,
                cfg.visualizer_style.as_deref(),
                cfg.visualizer_smoothing,
                cfg.visualizer_lag,
            )
        } else if cfg.now_playing.unwrap_or(false) {
            Button::new_now_playing(cfg.action)
        } else {
            Button::new_spacer()
        };
        button.accent = cfg.accent;
        button
    }
    fn new_spacer() -> Button {
        Button {
            action: ButtonAction::default(),
            active: false,
            changed: false,
            image: ButtonImage::Spacer,
            icon_width: 0.0,
            icon_height: 0.0,
            accent: None,
            muted: false,
        }
    }
    fn new_text(text: String, action: ButtonAction) -> Button {
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Text(text),
            icon_width: 0.0,
            icon_height: 0.0,
            accent: None,
            muted: false,
        }
    }
    fn new_icon(
        path: impl AsRef<str>,
        theme: Option<impl AsRef<str>>,
        action: ButtonAction,
        icon_width: i32,
        icon_height: i32,
    ) -> Button {
        let image =
            try_load_image(path, theme, icon_width, icon_height).expect("failed to load icon");
        Button {
            action,
            image,
            icon_width: icon_width as f64,
            icon_height: icon_height as f64,
            active: false,
            changed: false,
            accent: None,
            muted: false,
        }
    }
    fn load_battery_image(icon: &str, theme: Option<impl AsRef<str>>) -> Handle {
        if let ButtonImage::Svg(svg) =
            try_load_image(icon, theme, DEFAULT_ICON_SIZE, DEFAULT_ICON_SIZE).unwrap()
        {
            return svg;
        }
        panic!("failed to load icon");
    }
    fn new_battery(
        action: ButtonAction,
        battery_mode: String,
        theme: Option<impl AsRef<str>>,
    ) -> Button {
        let bolt = Self::load_battery_image("bolt", theme.as_ref());
        let mut plain = Vec::new();
        let mut charging = Vec::new();
        for icon in [
            "battery_0_bar",
            "battery_1_bar",
            "battery_2_bar",
            "battery_3_bar",
            "battery_4_bar",
            "battery_5_bar",
            "battery_6_bar",
            "battery_full",
        ] {
            plain.push(Self::load_battery_image(icon, theme.as_ref()));
        }
        for icon in [
            "battery_charging_20",
            "battery_charging_30",
            "battery_charging_50",
            "battery_charging_60",
            "battery_charging_80",
            "battery_charging_90",
            "battery_charging_full",
        ] {
            charging.push(Self::load_battery_image(icon, theme.as_ref()));
        }
        let battery_mode = match battery_mode.as_str() {
            "icon" => BatteryIconMode::Icon,
            "percentage" => BatteryIconMode::Percentage,
            "both" => BatteryIconMode::Both,
            _ => panic!("invalid battery mode, accepted modes: icon, percentage, both"),
        };
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Battery(
                BatteryWidget::default(),
                battery_mode,
                BatteryImages {
                    plain,
                    bolt,
                    charging,
                },
            ),
            icon_width: 0.0,
            icon_height: 0.0,
            accent: None,
            muted: false,
        }
    }

    fn new_waveform(
        action: ButtonAction,
        style: Option<&str>,
        accent: Option<ButtonAccent>,
        smoothing: Option<u32>,
        lag: Option<u32>,
    ) -> Button {
        let style = style.map_or(Style::Line, |value| {
            Style::parse(value)
                .unwrap_or_else(|| panic!("invalid waveform style, accepted styles: line, bars"))
        });
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Waveform(
                WaveformWidget::new(style)
                    .with_accent(accent)
                    .with_motion(smoothing, lag),
            ),
            icon_width: 0.0,
            icon_height: 0.0,
            accent: None,
            muted: false,
        }
    }

    fn new_visualizer(
        action: ButtonAction,
        style: Option<&str>,
        smoothing: Option<u32>,
        lag: Option<u32>,
    ) -> Button {
        let style = style.map_or(Style::Line, |value| {
            Style::parse(value)
                .unwrap_or_else(|| panic!("invalid visualizer style, accepted styles: line, bars"))
        });
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Waveform(
                WaveformWidget::new_music(style).with_motion(smoothing, lag),
            ),
            icon_width: 0.0,
            icon_height: 0.0,
            accent: None,
            muted: false,
        }
    }

    /// The media bar's now-playing row: display-only, fed by the
    /// desktop now-playing receiver at the slow metadata cadence. No
    /// keys ever fire from it and it carries no session state - every
    /// poll replaces the whole label, so the row can never outlive the
    /// track it names.
    fn new_now_playing(action: ButtonAction) -> Button {
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::NowPlaying(NowPlayingWidget::default()),
            icon_width: 0.0,
            icon_height: 0.0,
            accent: None,
            muted: false,
        }
    }

    fn new_time(action: ButtonAction, format: &str, locale_str: Option<&str>) -> Button {
        let format_str = if format == "24hr" {
            "%H:%M    %a %-e %b"
        } else if format == "12hr" {
            "%-l:%M %p    %a %-e %b"
        } else {
            format
        };

        let format_items = match StrftimeItems::new(format_str).parse_to_owned() {
            Ok(s) => s,
            Err(e) => panic!("Invalid time format, consult the configuration file for examples of correct ones: {e:?}"),
        };

        let locale = locale_str
            .and_then(|l| Locale::try_from(l).ok())
            .unwrap_or(Locale::POSIX);
        Button {
            action,
            active: false,
            changed: false,
            image: ButtonImage::Time(format_items, locale),
            icon_width: 0.0,
            icon_height: 0.0,
            accent: None,
            muted: false,
        }
    }
    fn needs_faster_refresh(&self) -> bool {
        match &self.image {
            ButtonImage::Time(items, _) => items.iter().any(|item| {
                use chrono::format::{Item, Numeric};
                matches!(
                    item,
                    Item::Numeric(Numeric::Second, _)
                        | Item::Numeric(Numeric::Nanosecond, _)
                        | Item::Numeric(Numeric::Timestamp, _)
                )
            }),
            _ => false,
        }
    }
    fn render(
        &self,
        c: &Context,
        height: i32,
        button_left_edge: f64,
        button_width: u64,
        y_shift: f64,
    ) {
        match &self.image {
            ButtonImage::Text(text) => {
                render_centered_text(
                    c,
                    text,
                    button_left_edge,
                    button_width as f64,
                    height,
                    y_shift,
                );
            }
            ButtonImage::Svg(svg) => {
                let scale = ((button_width as f64 - 20.0).max(1.0) / self.icon_width)
                    .min((height as f64 - 16.0).max(1.0) / self.icon_height)
                    .min(1.0);
                let (icon_width, icon_height) = (self.icon_width * scale, self.icon_height * scale);
                let x = button_left_edge + (button_width as f64 / 2.0 - icon_width / 2.0).round();
                let y = y_shift + ((height as f64 - icon_height) / 2.0).round();

                svg.render_document(c, &Rectangle::new(x, y, icon_width, icon_height))
                    .unwrap();
            }
            ButtonImage::Bitmap(surf) => {
                let scale = ((button_width as f64 - 20.0).max(1.0) / self.icon_width)
                    .min((height as f64 - 16.0).max(1.0) / self.icon_height)
                    .min(1.0);
                let x = button_left_edge
                    + (button_width as f64 / 2.0 - self.icon_width * scale / 2.0).round();
                let y = y_shift + ((height as f64 - self.icon_height * scale) / 2.0).round();
                c.save().unwrap();
                c.translate(x, y);
                c.scale(scale, scale);
                c.set_source_surface(surf, 0.0, 0.0).unwrap();
                c.rectangle(0.0, 0.0, self.icon_width, self.icon_height);
                c.fill().unwrap();
                c.restore().unwrap();
            }
            ButtonImage::Time(format, locale) => {
                let current_time = Local::now();
                let formatted_time = current_time
                    .format_localized_with_items(format.iter(), *locale)
                    .to_string();
                render_centered_text(
                    c,
                    &formatted_time,
                    button_left_edge,
                    button_width as f64,
                    height,
                    y_shift,
                );
            }
            ButtonImage::Battery(battery, battery_mode, icons) => {
                let Some(source) = battery.source() else {
                    render_centered_text(
                        c,
                        "Battery N/A",
                        button_left_edge,
                        button_width as f64,
                        height,
                        y_shift,
                    );
                    return;
                };
                let (capacity, state) = (source.capacity, source.state);
                c.save().unwrap();
                let label = battery.label();
                // A device label turns this button deliberately two-tier,
                // smaller than the bar's primary label size: the 12 px
                // caption names the battery up top, and the 24 px percent
                // pairs with a 32 px icon below. Without a label the
                // percent alone rides at the battery composition's own
                // size beside the same icon.
                let icon_size = if label.is_some() {
                    32.0
                } else {
                    DEFAULT_ICON_SIZE as f64
                };
                let content_center = if label.is_some() {
                    height as f64 * 0.65
                } else {
                    height as f64 / 2.0
                };
                if let Some(label) = label {
                    c.set_font_size(12.0); // The caption tier.
                    let extents = c.text_extents(label).unwrap();
                    let scale = ((button_width as f64 - 8.0).max(1.0) / extents.width()).min(1.0);
                    c.set_font_size(12.0 * scale);
                    let extents = c.text_extents(label).unwrap();
                    c.move_to(
                        button_left_edge + (button_width as f64 - extents.width()) / 2.0,
                        y_shift + 15.0,
                    );
                    c.show_text(label).unwrap();
                    c.set_font_size(24.0);
                }
                let icon = if battery_mode.should_draw_icon() {
                    Some(match state {
                        BatteryState::Charging => match capacity {
                            0..=20 => &icons.charging[0],
                            21..=30 => &icons.charging[1],
                            31..=50 => &icons.charging[2],
                            51..=60 => &icons.charging[3],
                            61..=80 => &icons.charging[4],
                            81..=99 => &icons.charging[5],
                            _ => &icons.charging[6],
                        },
                        _ => match capacity {
                            0 => &icons.plain[0],
                            1..=20 => &icons.plain[1],
                            21..=30 => &icons.plain[2],
                            31..=50 => &icons.plain[3],
                            51..=60 => &icons.plain[4],
                            61..=80 => &icons.plain[5],
                            81..=99 => &icons.plain[6],
                            _ => &icons.plain[7],
                        },
                    })
                } else if state == BatteryState::Charging {
                    Some(&icons.bolt)
                } else {
                    None
                };
                let percent_str = format!("{}%", capacity);
                let extents = c.text_extents(&percent_str).unwrap();
                let text_width = if battery_mode.should_draw_text() {
                    extents.width()
                } else {
                    0.0
                };
                let width = text_width + if icon.is_some() { icon_size } else { 0.0 };
                let scale = if label.is_some() {
                    ((button_width as f64 - 8.0).max(1.0) / width).min(1.0)
                } else {
                    1.0
                };
                let left = button_left_edge + ((button_width as f64 - width * scale) / 2.0).round();
                let mut text_offset = 0.0;
                if let Some(svg) = icon {
                    let size = icon_size * scale;
                    svg.render_document(
                        c,
                        &Rectangle::new(
                            left,
                            y_shift + (content_center - size / 2.0).round(),
                            size,
                            size,
                        ),
                    )
                    .unwrap();
                    text_offset = size;
                }
                if battery_mode.should_draw_text() {
                    if label.is_some() {
                        c.set_font_size(24.0 * scale);
                    }
                    let extents = c.text_extents(&percent_str).unwrap();
                    c.move_to(
                        left + text_offset,
                        y_shift + (content_center + extents.height() / 2.0).round(),
                    );
                    c.show_text(&percent_str).unwrap();
                }
                c.restore().unwrap();
            }
            ButtonImage::Waveform(wave) => {
                wave.render(c, height, button_left_edge, button_width as f64, y_shift);
            }
            ButtonImage::NowPlaying(widget) => {
                widget.render(c, height, button_left_edge, button_width as f64, y_shift);
            }
            ButtonImage::Spacer => (),
        }
        if self.muted && matches!(self.action, ButtonAction::VolumeSlider) {
            // The closed slider's mute mark: the same black-backed white
            // slash the drawn slider lays over its speaker glyph, centered
            // over this button's icon or label. The mark only overdraws;
            // the button's size and hit target stay exactly as configured.
            let mid = height as f64 / 2.0 + y_shift;
            let center = button_left_edge + button_width as f64 / 2.0;
            let reach = (height as f64 * 0.35)
                .min(button_width as f64 / 4.0)
                .max(6.0);
            for (line_width, color) in [(5.0, 0.0), (2.0, 1.0)] {
                c.set_line_width(line_width);
                c.set_source_rgb(color, color, color);
                c.move_to(center - reach, mid + reach);
                c.line_to(center + reach, mid - reach);
                c.stroke().unwrap();
            }
        }
    }
    fn set_active(&mut self, active: bool, emit_keys: &mut impl FnMut(&[Key], bool)) {
        if self.active != active {
            self.active = active;
            self.changed = true;

            if let ButtonAction::Keys(keys) = &self.action {
                emit_keys(keys, active);
            }
        }
    }
    fn effective_accent(&self, escalation: &BatteryEscalation) -> ButtonAccent {
        let fallback = self
            .accent
            .unwrap_or_else(|| ButtonAccent::for_action(&self.action));
        if let ButtonImage::Battery(battery, _, _) = &self.image {
            // The displayed source alone decides the tier: a cycled device
            // battery escalates independently whenever it is the one shown.
            // A charging battery keeps the charging green at every level,
            // and no source ("Battery N/A") stays on the widget's accent.
            if let Some(source) = battery.source() {
                if source.state == BatteryState::Charging {
                    return ButtonAccent::Green;
                }
                return match escalation.alert(source.capacity) {
                    BatteryAlert::Critical => escalation.critical_accent,
                    BatteryAlert::Warning => escalation.warning_accent,
                    BatteryAlert::Healthy => fallback,
                };
            }
        }
        fallback
    }
}

fn render_centered_text(c: &Context, text: &str, left: f64, width: f64, height: i32, y_shift: f64) {
    c.save().unwrap();
    let extents = c.text_extents(text).unwrap();
    if extents.width() > 0.0 {
        let scale = ((width - 20.0).max(1.0) / extents.width()).min(1.0);
        c.set_font_size(c.font_matrix().xx() * scale);
    }
    let extents = c.text_extents(text).unwrap();
    c.move_to(
        left + (width - extents.width()) / 2.0 - extents.x_bearing(),
        y_shift + (height as f64 - extents.height()) / 2.0 - extents.y_bearing(),
    );
    c.show_text(text).unwrap();
    c.restore().unwrap();
}

#[derive(Default)]
pub struct FunctionLayer {
    displays_time: bool,
    buttons: Vec<(usize, Button)>,
    virtual_button_count: usize,
    faster_refresh: bool,
    // The button index of the live microphone waveform widget, if this
    // layer carries one; it owns the fast repaint cadence while displayed.
    waveform: Option<usize>,
    // The button index of this bar's opt-in music visualizer toggle -
    // on a configured bar the idle `Viz` button whose tap opens the
    // deck's full-bar view, and inside that view the graph button that
    // draws it.
    visualizer: Option<usize>,
    // The button index of this bar's now-playing row, if it carries one;
    // only a displayed now-playing bar arms its metadata poller.
    now_playing: Option<usize>,
    // Whether this layer is a synthesized full-bar visualizer view rather
    // than a configured bar: its visualizer button draws the graph and its
    // one remaining button exits back to the bar that opened the view.
    viz_view: bool,
    // The shared region the last draw covered with the slider, so a
    // later draw without it can erase and repaint the whole region.
    slider_region_drawn: Option<(usize, usize)>,
}

impl FunctionLayer {
    fn with_config(cfg: Vec<ButtonConfig>) -> FunctionLayer {
        if cfg.is_empty() {
            panic!("Invalid configuration, layer has 0 buttons");
        }

        let mut virtual_button_count = 0;
        let displays_time = cfg.iter().any(|cfg| cfg.time.is_some());
        let buttons = cfg
            .into_iter()
            .scan(&mut virtual_button_count, |state, cfg| {
                let i = **state;
                let mut stretch = cfg.stretch.unwrap_or(1);
                if stretch < 1 {
                    println!("Stretch value must be at least 1, setting to 1.");
                    stretch = 1;
                }
                **state += stretch;
                Some((i, Button::with_config(cfg)))
            })
            .collect::<Vec<_>>();
        let faster_refresh = buttons.iter().any(|(_, b)| b.needs_faster_refresh());
        let waveform = buttons
            .iter()
            .position(|(_, b)| matches!(&b.image, ButtonImage::Waveform(wave) if !wave.is_music()));
        let visualizer = buttons
            .iter()
            .position(|(_, b)| matches!(&b.image, ButtonImage::Waveform(wave) if wave.is_music()));
        let now_playing = buttons
            .iter()
            .position(|(_, b)| matches!(&b.image, ButtonImage::NowPlaying(_)));
        FunctionLayer {
            displays_time,
            buttons,
            virtual_button_count,
            faster_refresh,
            waveform,
            visualizer,
            now_playing,
            viz_view: false,
            slider_region_drawn: None,
        }
    }

    /// The full-bar visualizer view a media bar's Viz button toggles
    /// into: the same drawn graph, spanning the whole bar, plus exactly
    /// one button that switches back to the bar that opened the view.
    /// Built from the toggle button's own widget so the view inherits its
    /// style and motion knobs; nothing here ever emits a key.
    fn viz_view(toggle: &Button, owner: &str) -> FunctionLayer {
        let ButtonImage::Waveform(graph) = &toggle.image else {
            panic!("visualizer view built from a non-visualizer button");
        };
        let graph = graph.clone();
        FunctionLayer {
            displays_time: false,
            buttons: vec![
                (
                    0,
                    Button {
                        action: ButtonAction::default(),
                        active: false,
                        changed: false,
                        image: ButtonImage::Waveform(graph),
                        icon_width: 0.0,
                        icon_height: 0.0,
                        accent: None,
                        muted: false,
                    },
                ),
                (
                    5,
                    Button {
                        action: ButtonAction::SwitchLayer(owner.into()),
                        active: false,
                        changed: false,
                        image: ButtonImage::Text("Off".into()),
                        icon_width: 0.0,
                        icon_height: 0.0,
                        accent: None,
                        muted: false,
                    },
                ),
            ],
            virtual_button_count: 6,
            faster_refresh: false,
            waveform: None,
            visualizer: Some(0),
            now_playing: None,
            viz_view: true,
            slider_region_drawn: None,
        }
    }
    #[allow(clippy::too_many_arguments)] // a full frame: layout, shift, overlay, and strip background
    fn draw(
        &mut self,
        config: &Config,
        (width, height): (i32, i32),
        surface: &Surface,
        pixel_shift: (f64, f64),
        complete_redraw: bool,
        slider: Option<Render>,
        background: (f64, f64, f64),
    ) -> Vec<ClipRect> {
        let c = Context::new(surface).unwrap();
        let mut modified_regions = if complete_redraw {
            vec![ClipRect::new(0, 0, height as u16, width as u16)]
        } else {
            Vec::new()
        };
        c.translate(height as f64, 0.0);
        c.rotate((90.0f64).to_radians());
        let pixel_shift_width = if config.enable_pixel_shift {
            PIXEL_SHIFT_WIDTH_PX
        } else {
            0
        };
        let virtual_button_width = ((width - pixel_shift_width as i32)
            - (BUTTON_SPACING_PX * (self.virtual_button_count - 1) as i32))
            as f64
            / self.virtual_button_count as f64;
        let (pixel_shift_x, pixel_shift_y) = pixel_shift;

        if complete_redraw {
            c.set_source_rgb(background.0, background.1, background.2);
            c.paint().unwrap();
        }
        c.set_font_face(&config.font_face);
        c.set_font_size(appearance::LABEL_FONT);

        // The slider no longer covers a span it was drawn over: either it
        // collapsed, or it moved to another cluster. Erase that whole column
        // and make its buttons repaint over it, gaps included.
        let uncovered = match (self.slider_region_drawn.take(), slider) {
            (Some((first, last)), None) => Some((first, last)),
            (Some((first, last)), Some(v)) if (first, last) != (v.first, v.last) => {
                Some((first, last))
            }
            _ => None,
        };
        if let Some((first, last)) = uncovered {
            let geometry =
                self.slider_geometry(width, first, last, pixel_shift_width, pixel_shift_x);
            let left_edge = geometry.left;
            let region_width = geometry.right - geometry.left;
            c.set_source_rgb(background.0, background.1, background.2);
            c.rectangle(
                (left_edge - 2.0).max(0.0),
                0.0,
                region_width + 4.0,
                height as f64,
            );
            c.fill().unwrap();
            for i in first..=last {
                self.buttons[i].1.changed = true;
            }
            if !complete_redraw {
                modified_regions.push(ClipRect::new(
                    0,
                    (left_edge - 2.0).max(0.0) as u16,
                    height as u16,
                    (left_edge + region_width + 2.0).min(width as f64) as u16,
                ));
            }
        }

        // One resolved set of escalation knobs per frame; the tiers are
        // evaluated per displayed battery inside the loop.
        let escalation = BatteryEscalation::from_config(config);
        for i in 0..self.buttons.len() {
            let end = if i + 1 < self.buttons.len() {
                self.buttons[i + 1].0
            } else {
                self.virtual_button_count
            };
            let (start, button) = &mut self.buttons[i];
            let start = *start;

            if slider.is_some_and(|v| i >= v.first && i <= v.last) {
                // The summoned slider covers the cluster; the buttons
                // beneath stay hidden until it collapses or moves to
                // another cluster.
                button.changed = false;
                continue;
            }

            if !button.changed && !complete_redraw {
                continue;
            };

            let left_edge = (start as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64))
                .floor()
                + pixel_shift_x
                + (pixel_shift_width / 2) as f64;

            let button_width = virtual_button_width
                + ((end - start - 1) as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64))
                    .floor();

            if !complete_redraw {
                // Erase the entire column so fitted labels and pressed borders
                // cannot leave pixels behind after an incremental redraw.
                c.set_source_rgb(background.0, background.1, background.2);
                c.rectangle(left_edge, 0.0, button_width.ceil(), height as f64);
                c.fill().unwrap();
            }
            if !matches!(button.image, ButtonImage::Spacer)
                && (button.active || config.show_button_outlines)
            {
                let accent = button.effective_accent(&escalation);
                accent.surface(&c, button.active, button.action.command().is_some());
                // Visual inset only: layout, spacing and hit targets stay the same.
                rounded_rect(
                    &c,
                    left_edge + 0.75,
                    height as f64 * 0.1 + 0.75,
                    (button_width.ceil() - 1.5).max(1.0),
                    height as f64 * 0.8 - 1.5,
                    10.0_f64.min(button_width / 2.0).min(height as f64 * 0.4),
                );
                c.fill_preserve().unwrap();
                accent.outline(&c, button.active);
                c.set_line_width(1.5);
                c.stroke().unwrap();
            }
            c.save().unwrap();
            c.rectangle(left_edge, 0.0, button_width.ceil(), height as f64);
            c.clip();
            c.set_source_rgb(1.0, 1.0, 1.0);
            button.render(
                &c,
                height,
                left_edge,
                button_width.ceil() as u64,
                pixel_shift_y,
            );
            c.restore().unwrap();

            button.changed = false;

            if !complete_redraw {
                modified_regions.push(ClipRect::new(
                    0,
                    left_edge.max(0.0) as u16,
                    height as u16,
                    (left_edge + button_width.ceil()).min(width as f64) as u16,
                ));
            }
        }

        if let Some(slider) = slider {
            self.slider_region_drawn = Some((slider.first, slider.last));
            let geometry = self.slider_geometry(
                width,
                slider.first,
                slider.last,
                pixel_shift_width,
                pixel_shift_x,
            );
            let left_edge = geometry.left;
            let right_edge = geometry.right;
            let region_width = right_edge - left_edge;

            if !complete_redraw {
                // Clear the whole cluster column, icons and outlines included.
                c.set_source_rgb(background.0, background.1, background.2);
                c.rectangle(
                    (left_edge - 2.0).max(0.0),
                    0.0,
                    region_width + 4.0,
                    height as f64,
                );
                c.fill().unwrap();
            }

            geometry.draw(&c, height as f64, pixel_shift_y, slider);

            if !complete_redraw {
                modified_regions.push(ClipRect::new(
                    0,
                    (left_edge - 2.0).max(0.0) as u16,
                    height as u16,
                    (right_edge + 2.0).min(width as f64) as u16,
                ));
            }
        }

        modified_regions
    }

    fn slider_geometry(
        &self,
        width: i32,
        first: usize,
        last: usize,
        shift_width: u64,
        shift_x: f64,
    ) -> Geometry {
        let end = self
            .buttons
            .get(last + 1)
            .map_or(self.virtual_button_count, |b| b.0);
        Geometry::new(
            width,
            self.virtual_button_count,
            self.buttons[first].0,
            end,
            shift_width,
            shift_x,
        )
    }

    /// Occupied widget spans of this layer in layout coordinates, mirroring
    /// the drawn button geometry including the static pixel-shift inset.
    /// Spacers are free space; `clock` marks the time widget the pet naps
    /// next to.
    fn layout_spans(&self, width: i32, pixel_shift_width: u64) -> Vec<PetSpan> {
        let virtual_button_width = ((width - pixel_shift_width as i32)
            - (BUTTON_SPACING_PX * (self.virtual_button_count - 1) as i32))
            as f64
            / self.virtual_button_count as f64;
        let mut spans = Vec::new();
        for (i, (start, button)) in self.buttons.iter().enumerate() {
            if matches!(button.image, ButtonImage::Spacer) {
                continue;
            }
            let end = if i + 1 < self.buttons.len() {
                self.buttons[i + 1].0
            } else {
                self.virtual_button_count
            };
            let left_edge = (*start as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64))
                .floor()
                + (pixel_shift_width / 2) as f64;
            let button_width = virtual_button_width
                + ((end - start - 1) as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64))
                    .floor();
            spans.push(PetSpan {
                left: left_edge,
                right: left_edge + button_width,
                clock: matches!(button.image, ButtonImage::Time(..)),
            });
        }
        spans
    }

    fn hit(&self, width: u16, height: u16, x: f64, y: f64, i: Option<usize>) -> Option<usize> {
        let virtual_button_width =
            (width as i32 - (BUTTON_SPACING_PX * (self.virtual_button_count - 1) as i32)) as f64
                / self.virtual_button_count as f64;

        let i = i.unwrap_or_else(|| {
            let virtual_i = (x / (width as f64 / self.virtual_button_count as f64)) as usize;
            self.buttons
                .iter()
                .position(|(start, _)| *start > virtual_i)
                .unwrap_or(self.buttons.len())
                - 1
        });
        if i >= self.buttons.len() {
            return None;
        }

        let start = self.buttons[i].0;
        let end = if i + 1 < self.buttons.len() {
            self.buttons[i + 1].0
        } else {
            self.virtual_button_count
        };

        let left_edge = (start as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64)).floor();

        let button_width = virtual_button_width
            + ((end - start - 1) as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64))
                .floor();

        if x < left_edge
            || x > (left_edge + button_width)
            || y < 0.1 * height as f64
            || y > 0.9 * height as f64
        {
            return None;
        }

        Some(i)
    }
}

struct TouchState {
    button: usize,
    inside: bool,
    // Set for presses on a slider control; crossing the drag
    // threshold converts the press into a slider drag.
    slider: Option<u64>,
    start_x: f64,
    start_y: f64,
}

// Input state belongs to the deck, so transitions always release the old layer
// before changing the displayed layer or discarding its buttons on reload.
pub struct LayerDeck {
    layers: Vec<FunctionLayer>,
    names: HashMap<String, usize>,
    home: usize,
    fn_layer: Option<usize>,
    pet_layer: Option<usize>,
    legacy: bool,
    active: usize,
    fn_pressed: bool,
    fn_return: Option<usize>,
    touches: HashMap<u32, TouchState>,
    slider: Overlay,
    // The closed-slider mute watch: a slow background probe keeps the
    // volume buttons' mute mark current while no slider is drawn. Armed
    // only on decks that actually carry a volume slider control.
    mute: MuteWatch,
    volume_sliders: bool,
    // For each configured bar carrying a Viz button, the deck index of
    // its synthesized full-bar visualizer view; the map is the toggle's
    // whole routing table.
    viz_view_of: HashMap<usize, usize>,
}

impl LayerDeck {
    fn new(
        layers: Vec<(String, FunctionLayer)>,
        home: usize,
        fn_layer: Option<usize>,
        pet_layer: Option<usize>,
        legacy: bool,
    ) -> Self {
        let mut names = layers
            .iter()
            .enumerate()
            .map(|(i, (name, _))| (name.clone(), i))
            .collect::<HashMap<_, _>>();
        // Every bar carrying a Viz button gets its dedicated full-bar
        // visualizer view appended to the deck: tapping the button
        // switches into it, its single Off button switches back, and the
        // view lives and dies with the deck that synthesized it.
        let mut viz_view_of = HashMap::new();
        let mut views = Vec::new();
        for (index, (name, layer)) in layers.iter().enumerate() {
            if let Some(button) = layer.visualizer {
                let view_name = format!("visualizer:{name}");
                // A pathological config naming its own layer like a
                // synthesized view keeps that name; the map still routes
                // the toggle, only the lookup name is yielded.
                if !names.contains_key(&view_name) {
                    let view = FunctionLayer::viz_view(&layer.buttons[button].1, name);
                    viz_view_of.insert(index, names.len() + views.len());
                    views.push((view_name, view));
                }
            }
        }
        let mut layers = layers;
        for (name, view) in views {
            names.insert(name.clone(), layers.len());
            layers.push((name, view));
        }
        let volume_sliders = layers.iter().any(|(_, layer)| {
            layer
                .buttons
                .iter()
                .any(|(_, button)| button.action == ButtonAction::VolumeSlider)
        });
        Self {
            layers: layers.into_iter().map(|(_, layer)| layer).collect(),
            names,
            viz_view_of,
            home,
            fn_layer,
            pet_layer,
            legacy,
            active: home,
            fn_pressed: false,
            fn_return: None,
            touches: HashMap::new(),
            slider: Overlay::default(),
            mute: MuteWatch::default(),
            volume_sliders,
        }
    }

    fn current(&self) -> &FunctionLayer {
        &self.layers[self.active]
    }

    fn current_mut(&mut self) -> &mut FunctionLayer {
        &mut self.layers[self.active]
    }

    /// Whether the pet's own layer is the one displayed; the pet exists
    /// and animates only there.
    fn pet_active(&self) -> bool {
        self.pet_layer == Some(self.active)
    }

    /// Whether the displayed layer carries the live microphone waveform;
    /// this is the recording layer, and only it arms the level poller.
    fn waveform_active(&self) -> bool {
        self.current().waveform.is_some()
    }

    /// Whether the full-bar visualizer view is the layer displayed; only
    /// that arms its monitor-level poller, so the desktop capture stream
    /// exists exactly while the view is there to show it. Tapping Viz on
    /// the media bar switches into the view and any layer switch - the
    /// Off button, Fn, navigation - leaves it, so the visualizer is never
    /// sticky and its capture never outlives the view.
    fn viz_active(&self) -> bool {
        self.current().viz_view && self.current().visualizer.is_some()
    }

    /// Whether the displayed layer carries a now-playing row; only that
    /// arms its metadata poller, so the session-bus reads exist exactly
    /// while a bar displays the row to keep current.
    fn now_playing_active(&self) -> bool {
        self.current().now_playing.is_some()
    }

    /// Feed one now-playing poll result, when the poller produced one,
    /// into the displayed layer's now-playing row. Every poll replaces
    /// the whole label, so a track change lands live and a paused or
    /// stopped player clears the row instead of leaving the last song
    /// behind. On any other layer every now-playing widget resets to the
    /// empty state, so a return starts from the next poll, never from an
    /// old track. Returns whether the bar needs an incremental repaint.
    fn now_playing_tick(&mut self, feed: Option<Feed>) -> bool {
        let Some(index) = self.layers[self.active].now_playing else {
            for layer in &mut self.layers {
                if let Some(i) = layer.now_playing {
                    if let ButtonImage::NowPlaying(widget) = &mut layer.buttons[i].1.image {
                        widget.reset();
                    }
                }
            }
            return false;
        };
        let Some(feed) = feed else {
            return false;
        };
        let button = &mut self.layers[self.active].buttons[index].1;
        let ButtonImage::NowPlaying(widget) = &mut button.image else {
            return false;
        };
        if widget.apply(&feed) {
            button.changed = true;
            return true;
        }
        false
    }

    /// Toggle the visualizer on a touch-down of its button: the press
    /// switches into this bar's dedicated full-bar view - the graph plus
    /// one Off button - as a pure layer change, so no key ever fires and
    /// media playback is never disturbed. Returns whether the tap was
    /// consumed, which is always the case for this button.
    fn toggle_visualizer(&mut self, emit_keys: &mut impl FnMut(&[Key], bool)) -> bool {
        let Some(&view) = self.viz_view_of.get(&self.active) else {
            return false;
        };
        // Explicit navigation wins over a temporary Fn selection, exactly
        // like any other layer-switching button.
        self.fn_return = None;
        self.switch_to(view, emit_keys)
    }

    /// Feed one microphone level sample, when the poller produced one, and
    /// the shared clock into the displayed layer's waveform widget. The
    /// widget's frame clock interpolates motion between samples (roughly
    /// 30 fps) and returns whether the bar needs an incremental repaint of
    /// that button, plus the epoll timeout until its next frame or the
    /// duration timer's next tick; both are idle on every other layer,
    /// whose widgets and cadence stay untouched.
    fn waveform_tick(&mut self, sample: Option<MicSample>, now: Instant) -> (bool, i32) {
        let Some(index) = self.layers[self.active].waveform else {
            // Leaving the recording layer ends that widget's session so a
            // later return restarts the timer and graph from scratch.
            for layer in &mut self.layers {
                if let Some(i) = layer.waveform {
                    if let ButtonImage::Waveform(wave) = &mut layer.buttons[i].1.image {
                        wave.deactivate();
                    }
                }
            }
            return (false, i32::MAX - 1);
        };
        let button = &mut self.layers[self.active].buttons[index].1;
        let ButtonImage::Waveform(wave) = &mut button.image else {
            return (false, i32::MAX - 1);
        };
        let mut changed = wave.activate(now);
        if let Some(sample) = sample {
            changed |= wave.sample(sample, now);
        }
        changed |= wave.frame(now);
        let timeout = wave.next_timeout(now);
        if changed {
            button.changed = true;
        }
        (changed, timeout)
    }

    /// Feed one music monitor level sample, when the poller produced one,
    /// into the full-bar view's visualizer widget. Mirrors
    /// [`Self::waveform_tick`] but gated on the view: on any other layer -
    /// including the media bar that opened it - every music widget holds
    /// its idle state, any session ends, and no fast cadence runs.
    fn visualizer_tick(&mut self, sample: Option<MicSample>, now: Instant) -> (bool, i32) {
        let index = match (self.current().visualizer, self.current().viz_view) {
            (Some(index), true) => index,
            _ => {
                for layer in &mut self.layers {
                    if let Some(i) = layer.visualizer {
                        if let ButtonImage::Waveform(wave) = &mut layer.buttons[i].1.image {
                            wave.deactivate();
                        }
                    }
                }
                return (false, i32::MAX - 1);
            }
        };
        let button = &mut self.layers[self.active].buttons[index].1;
        let ButtonImage::Waveform(wave) = &mut button.image else {
            return (false, i32::MAX - 1);
        };
        let mut changed = wave.activate(now);
        if let Some(sample) = sample {
            changed |= wave.sample(sample, now);
        }
        changed |= wave.frame(now);
        let timeout = wave.next_timeout(now);
        if changed {
            button.changed = true;
        }
        (changed, timeout)
    }

    fn refresh_batteries(
        &mut self,
        sources: &[BatterySource],
        enabled: bool,
        timeout: Duration,
        now: Instant,
        emit_keys: &mut impl FnMut(&[Key], bool),
    ) -> Option<Duration> {
        let mut switched = false;
        let mut next_timeout = None;
        for layer in &mut self.layers {
            for (_, button) in &mut layer.buttons {
                if let ButtonImage::Battery(battery, _, _) = &mut button.image {
                    let old_source = battery.source().cloned();
                    let old_label = battery.label().map(str::to_owned);
                    switched |= battery.refresh(sources, enabled, timeout, now);
                    button.changed |= old_source.as_ref() != battery.source()
                        || old_label.as_deref() != battery.label();
                    if let Some(deadline) = battery.return_at {
                        let remaining = deadline.saturating_duration_since(now);
                        next_timeout =
                            Some(next_timeout.map_or(remaining, |old| min(old, remaining)));
                    }
                }
            }
        }
        if switched {
            self.release_all(emit_keys);
        }
        next_timeout
    }

    fn release_all(&mut self, emit_keys: &mut impl FnMut(&[Key], bool)) {
        for layer in &mut self.layers {
            for (_, button) in &mut layer.buttons {
                button.set_active(false, emit_keys);
            }
        }
        self.touches.clear();
        self.slider_collapse();
    }

    fn switch_to(&mut self, target: usize, emit_keys: &mut impl FnMut(&[Key], bool)) -> bool {
        if target >= self.layers.len() || target == self.active {
            return false;
        }
        self.release_all(emit_keys);
        self.active = target;
        true
    }

    fn switch_named(&mut self, name: &str, emit_keys: &mut impl FnMut(&[Key], bool)) -> bool {
        let Some(&target) = self.names.get(name) else {
            return false;
        };
        // Explicit navigation wins over a temporary Fn selection.
        self.fn_return = None;
        self.switch_to(target, emit_keys)
    }

    fn fn_event(
        &mut self,
        pressed: bool,
        double_press: bool,
        emit_keys: &mut impl FnMut(&[Key], bool),
    ) -> bool {
        if self.fn_pressed == pressed {
            return false;
        }
        self.fn_pressed = pressed;
        if pressed {
            if self.legacy && double_press && self.fn_layer.is_some() {
                self.slider_collapse();
                if let Some(alternate) = &mut self.fn_layer {
                    std::mem::swap(&mut self.home, alternate);
                }
            }
            self.fn_return = Some(if self.legacy { self.home } else { self.active });
            self.fn_layer
                .is_some_and(|target| self.switch_to(target, emit_keys))
        } else {
            self.fn_return
                .take()
                .is_some_and(|target| self.switch_to(target, emit_keys))
        }
    }

    fn update_button(&mut self, button: usize, emit_keys: &mut impl FnMut(&[Key], bool)) {
        let active = self
            .touches
            .values()
            .any(|touch| touch.button == button && touch.inside);
        self.current_mut().buttons[button]
            .1
            .set_active(active, emit_keys);
    }

    fn touch_down(
        &mut self,
        slot: u32,
        button: usize,
        x: f64,
        y: f64,
        emit_keys: &mut impl FnMut(&[Key], bool),
        run_command: &mut impl FnMut(&str),
    ) -> bool {
        self.touch_up(slot, emit_keys);
        if matches!(self.current().buttons[button].1.image, ButtonImage::Spacer) {
            return false;
        }
        // A configured bar's visualizer button consumes its own tap: the
        // toggle into the full-bar view is the whole action, and no key
        // ever fires from it. Inside the view itself the graph button is
        // inert - only its Off button leaves the view.
        if !self.current().viz_view && self.current().visualizer == Some(button) {
            return self.toggle_visualizer(emit_keys);
        }
        let selected = Kind::for_action(&self.current().buttons[button].1.action);
        if let Some(kind) = selected {
            self.slider_press(button, kind);
        }
        if let ButtonImage::Battery(battery, _, _) = &mut self.current_mut().buttons[button].1.image
        {
            if battery.cycle(Instant::now()) {
                self.current_mut().buttons[button].1.changed = true;
                self.release_all(emit_keys);
                return true;
            }
        }
        if let ButtonAction::SwitchLayer(name) = &self.current().buttons[button].1.action {
            let name = name.clone();
            return self.switch_named(&name, emit_keys);
        }
        // Command buttons fire exactly once per touch-down, running their
        // command before any layer switch the same press performs.
        if let ButtonAction::CommandLayer { command, layer } =
            &self.current().buttons[button].1.action
        {
            let (command, layer) = (command.clone(), layer.clone());
            run_command(&command);
            return self.switch_named(&layer, emit_keys);
        }
        if let ButtonAction::Command(command) = &self.current().buttons[button].1.action {
            let command = command.clone();
            run_command(&command);
        }
        self.touches.insert(
            slot,
            TouchState {
                button,
                inside: true,
                slider: selected.map(|_| self.slider.opened_revision),
                start_x: x,
                start_y: y,
            },
        );
        self.update_button(button, emit_keys);
        false
    }

    /// Route a fresh touch-down: while the slider is drawn its region owns
    /// the touch, otherwise the press goes to the button under it.
    #[allow(clippy::too_many_arguments)] // a router: geometry plus the two effects
    fn touch_down_at(
        &mut self,
        slot: u32,
        x: f64,
        y: f64,
        width: u16,
        height: u16,
        emit_keys: &mut impl FnMut(&[Key], bool),
        run_command: &mut impl FnMut(&str),
    ) -> bool {
        if self.slider_drag_start(slot, x, y, width, height, emit_keys) {
            return false;
        }
        if self.slider.active
            && self.slider.confirmed
            && self.slider.layer == self.active
            && self
                .slider_geometry(width)
                .is_some_and(|g| g.contains(x, y, height as f64))
        {
            return false;
        }
        match self.current().hit(width, height, x, y, None) {
            Some(button) => self.touch_down(slot, button, x, y, emit_keys, run_command),
            None => false,
        }
    }

    fn touch_motion(
        &mut self,
        slot: u32,
        x: f64,
        y: f64,
        width: u16,
        height: u16,
        emit_keys: &mut impl FnMut(&[Key], bool),
    ) {
        let Some((button, volume, start_x, start_y)) = self
            .touches
            .get(&slot)
            .map(|touch| (touch.button, touch.slider, touch.start_x, touch.start_y))
        else {
            return;
        };
        if volume == Some(self.slider.opened_revision)
            && self.slider.active
            && (x - start_x).hypot(y - start_y) > DRAG_THRESHOLD_PX
        {
            // An intentional drag: release the button highlight, then set
            // the absolute level directly from the finger position.
            self.touch_up(slot, emit_keys);
            if let Some(geometry) = self.slider_geometry(width) {
                if self.slider.begin_drag(slot, x, &geometry) {
                    self.mark_slider_changed();
                }
            }
            return;
        }
        let inside = self
            .current()
            .hit(width, height, x, y, Some(button))
            .is_some();
        if let Some(touch) = self.touches.get_mut(&slot) {
            touch.inside = inside;
        }
        self.update_button(button, emit_keys);
    }

    fn touch_up(&mut self, slot: u32, emit_keys: &mut impl FnMut(&[Key], bool)) {
        if self.slider.release_drag(slot) {
            return;
        }
        if let Some(touch) = self.touches.remove(&slot) {
            if touch.slider == Some(self.slider.opened_revision) && self.slider.active {
                self.slider.deadline = Some(Instant::now() + LINGER);
            }
            self.update_button(touch.button, emit_keys);
        }
    }

    fn replace(&mut self, new: Self, emit_keys: &mut impl FnMut(&[Key], bool)) {
        self.release_all(emit_keys);
        *self = new;
    }

    /// Adjacent level controls share their complete region, including the gap.
    fn slider_press(&mut self, button: usize, kind: Kind) {
        let layer = self.current();
        let mut first = button;
        while first > 0 && Kind::for_action(&layer.buttons[first - 1].1.action).is_some() {
            first -= 1;
        }
        let mut last = button;
        while last + 1 < layer.buttons.len()
            && Kind::for_action(&layer.buttons[last + 1].1.action).is_some()
        {
            last += 1;
        }
        self.mark_slider_changed();
        self.slider.open(kind, self.active, (first, last));
        self.mark_slider_changed();
    }

    fn slider_reply(&mut self, revision: u64, kind: Kind, result: std::io::Result<LevelState>) {
        if self.slider.reply(revision, kind, result) {
            self.mark_slider_changed();
        }
    }

    fn slider_collapse(&mut self) {
        if self.slider.collapse() {
            self.mark_slider_changed();
            // The closed bar is the mute mark's canvas again; re-read the
            // mixer right away instead of waiting out the probe cadence.
            self.mute.settle(Instant::now());
        }
    }

    /// The due background mute probe, when this deck carries a volume
    /// slider control and the shared slider is closed.
    fn mute_probe(&mut self, now: Instant) -> Option<(u64, level_control::Request)> {
        if !self.volume_sliders {
            return None;
        }
        self.mute.tick(now, self.slider.active)
    }

    /// Whether this volume reply revision belongs to the mute watch's
    /// outstanding probe; such replies never reach the overlay.
    fn mute_owns(&self, revision: u64) -> bool {
        self.volume_sliders && self.mute.owns(revision)
    }

    /// Route a volume reply through the mute watch first; the overlay only
    /// sees replies its own revisions produced.
    fn mute_reply(&mut self, revision: u64, result: std::io::Result<LevelState>) {
        if !self.mute.owns(revision) {
            return;
        }
        if self.mute.reply(revision, result, Instant::now()) {
            self.show_volume_muted(self.mute.muted() == Some(true));
        }
    }

    /// Push the closed-slider mute mark onto every volume slider button on
    /// every layer, repainting only buttons whose mark actually changes.
    fn show_volume_muted(&mut self, muted: bool) {
        for layer in &mut self.layers {
            for (_, button) in &mut layer.buttons {
                if button.action == ButtonAction::VolumeSlider && button.muted != muted {
                    button.muted = muted;
                    button.changed = true;
                }
            }
        }
    }

    fn mark_slider_changed(&mut self) {
        if let Some((first, last)) = self.slider.region {
            for (_, button) in &mut self.layers[self.slider.layer].buttons[first..=last] {
                button.changed = true;
            }
        }
    }

    fn slider_geometry(&self, width: u16) -> Option<Geometry> {
        let (first, last) = self.slider.region?;
        Some(self.layers[self.slider.layer].slider_geometry(
            width as i32,
            first,
            last,
            self.slider.pixel_shift_width,
            self.slider.pixel_shift_x,
        ))
    }

    fn slider_region(&self, width: u16) -> Option<(f64, f64)> {
        self.slider_geometry(width).map(|g| (g.left, g.right))
    }

    #[cfg(test)]
    fn slider_track(&self, width: u16) -> Option<(f64, f64)> {
        self.slider_geometry(width)
            .map(|g| (g.track_left(), g.track_right()))
    }

    #[cfg(test)]
    fn slider_set_level(&mut self, target: u8, _emit_keys: &mut impl FnMut(&[Key], bool)) {
        self.slider.set_level(target);
        self.mark_slider_changed();
    }

    /// A touch-down on the drawn slider grabs it right away at the touched
    /// level; touching its left speaker glyph selects zero too.
    fn slider_drag_start(
        &mut self,
        slot: u32,
        x: f64,
        y: f64,
        width: u16,
        height: u16,
        _emit_keys: &mut impl FnMut(&[Key], bool),
    ) -> bool {
        if !self.slider.active
            || !self.slider.confirmed
            || self.active != self.slider.layer
            || self.slider.drag.is_some()
            || self.touches.contains_key(&slot)
        {
            return false;
        }
        let Some((left, right)) = self.slider_region(width) else {
            return false;
        };
        if x < left || x > right || y < 0.1 * height as f64 || y > 0.9 * height as f64 {
            return false;
        }
        let geometry = self.slider_geometry(width).unwrap();
        let changed = self.slider.begin_drag(slot, x, &geometry);
        if changed {
            self.mark_slider_changed();
        }
        changed
    }

    /// Forward motion only for the finger that owns this overlay's drag.
    fn slider_drag_motion(
        &mut self,
        slot: u32,
        x: f64,
        _y: f64,
        width: u16,
        _emit_keys: &mut impl FnMut(&[Key], bool),
    ) -> bool {
        let Some(geometry) = self.slider_geometry(width) else {
            return false;
        };
        let changed = self.slider.drag_motion(slot, x, &geometry);
        if changed {
            self.mark_slider_changed();
        }
        changed
    }

    /// A drag or a still-held activation is ongoing interaction: the
    /// slider stays up under the finger regardless of the deadline.
    fn slider_interaction_held(&self) -> bool {
        self.slider.drag.is_some()
            || self
                .touches
                .values()
                .any(|touch| touch.slider == Some(self.slider.opened_revision))
    }

    /// Collapse the slider once its linger deadline passes with no
    /// interaction held.
    fn slider_refresh(&mut self, now: Instant) -> bool {
        if !self.slider.active
            || self.slider_interaction_held()
            || !self.slider.deadline.is_some_and(|deadline| now >= deadline)
        {
            return false;
        }
        self.slider_collapse();
        true
    }

    /// Time remaining until the slider collapses, for the epoll timeout.
    fn slider_timeout(&self, now: Instant) -> Option<Duration> {
        if !self.slider.active || self.slider_interaction_held() {
            return None;
        }
        self.slider
            .deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    /// Slider rendering info, only while its owning bar is displayed.
    fn slider_render(&self) -> Option<Render> {
        if !self.slider.active || !self.slider.confirmed || self.active != self.slider.layer {
            return None;
        }
        let (first, last) = self.slider.region?;
        Some(Render {
            first,
            last,
            level: self.slider.level,
            muted: self.slider.muted,
            kind: self.slider.kind,
        })
    }
}

struct Interface;

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        let mode = flags & O_ACCMODE;

        OpenOptions::new()
            .custom_flags(flags)
            .read(mode == O_RDONLY || mode == O_RDWR)
            .write(mode == O_WRONLY || mode == O_RDWR)
            .open(path)
            .map(|file| file.into())
            .map_err(|err| err.raw_os_error().unwrap())
    }
    fn close_restricted(&mut self, fd: OwnedFd) {
        _ = File::from(fd);
    }
}

fn emit<F>(uinput: &mut UInputHandle<F>, ty: EventKind, code: u16, value: i32)
where
    F: AsRawFd,
{
    uinput
        .write(&[input_event {
            value,
            type_: ty as u16,
            code,
            time: timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
        }])
        .unwrap();
}

fn toggle_keys<F>(uinput: &mut UInputHandle<F>, codes: &[Key], value: i32)
where
    F: AsRawFd,
{
    if codes.is_empty() {
        return;
    }
    for kc in codes {
        emit(uinput, EventKind::Key, *kc as u16, value);
    }
    emit(
        uinput,
        EventKind::Synchronize,
        SynchronizeKind::Report as u16,
        0,
    );
}

fn main() {
    let mut drm = DrmBackend::open_card().unwrap();
    let (height, width) = drm.mode().size();
    let _ = panic::catch_unwind(AssertUnwindSafe(|| real_main(&mut drm)));
    let crash_bitmap = include_bytes!("crash_bitmap.raw");
    let mut map = drm.map().unwrap();
    let data = map.as_mut();
    let mut wptr = 0;
    for byte in crash_bitmap {
        for i in 0..8 {
            let bit = ((byte >> i) & 0x1) == 0;
            let color = if bit { 0xFF } else { 0x0 };
            data[wptr] = color;
            data[wptr + 1] = color;
            data[wptr + 2] = color;
            data[wptr + 3] = color;
            wptr += 4;
        }
    }
    drop(map);
    drm.dirty(&[ClipRect::new(0, 0, height, width)]).unwrap();
    let mut sigset = SigSet::empty();
    sigset.add(Signal::SIGTERM);
    sigset.wait().unwrap();
}

fn real_main(drm: &mut DrmBackend) {
    let (height, width) = drm.mode().size();
    let (db_width, db_height) = drm.fb_info().unwrap().size();
    let mut uinput = UInputHandle::new(OpenOptions::new().write(true).open("/dev/uinput").unwrap());
    let mut backlight = BacklightManager::new();
    let mut cfg_mgr = ConfigManager::new();
    let (mut cfg, mut layers) = cfg_mgr.load_config(width);
    let mut pixel_shift = PixelShiftManager::new();
    let mut pet = PetManager::new(cfg.pet_enabled, cfg.pet_variant, cfg.pet_energy);
    let mut last_fn_press: Option<Instant> = None;

    // drop privileges to input and video group
    let groups = ["input", "video"];

    PrivDrop::default()
        .user("nobody")
        .group_list(&groups)
        .apply()
        .unwrap_or_else(|e| panic!("Failed to drop privileges: {}", e));

    let mut battery_monitor = BatteryMonitor::new(PathBuf::from("/sys/class/power_supply"));
    // Command buttons spawn their configured commands through this runner,
    // detached from the render loop.
    let commands = CommandRunner::new();
    let mut volume_control = LevelControl::volume();
    let mut brightness_control = LevelControl::brightness();
    // Live microphone levels for the recording layer's waveform widget.
    let mut mic_levels = MicLevels::levels();
    // Live output-monitor levels for the media layer's visualizer, which
    // exist only while the visualizer is on and its layer is displayed.
    let mut viz_levels = MonitorLevels::visualizer();
    // Now-playing metadata for the media bar's text row, polled only at
    // its slow cadence while a now-playing layer is displayed.
    let mut now_playing = NowPlaying::client();

    let mut surface =
        ImageSurface::create(Format::ARgb32, db_width as i32, db_height as i32).unwrap();
    let mut needs_complete_redraw = true;

    let mut input_tb = Libinput::new_with_udev(Interface);
    let mut input_main = Libinput::new_with_udev(Interface);
    input_tb.udev_assign_seat("seat-touchbar").unwrap();
    input_main.udev_assign_seat("seat0").unwrap();
    let udev_monitor = MonitorBuilder::new()
        .unwrap()
        .match_subsystem("power_supply")
        .unwrap()
        .listen()
        .unwrap();
    let epoll = Epoll::new(EpollCreateFlags::empty()).unwrap();
    epoll
        .add(input_main.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN, 0))
        .unwrap();
    epoll
        .add(input_tb.as_fd(), EpollEvent::new(EpollFlags::EPOLLIN, 1))
        .unwrap();
    epoll
        .add(cfg_mgr.fd(), EpollEvent::new(EpollFlags::EPOLLIN, 2))
        .unwrap();
    epoll
        .add(&udev_monitor, EpollEvent::new(EpollFlags::EPOLLIN, 3))
        .unwrap();
    epoll
        .add(
            &battery_monitor.wake,
            EpollEvent::new(EpollFlags::EPOLLIN, 4),
        )
        .unwrap();
    epoll
        .add(
            &volume_control.wake,
            EpollEvent::new(EpollFlags::EPOLLIN, 5),
        )
        .unwrap();
    epoll
        .add(
            &brightness_control.wake,
            EpollEvent::new(EpollFlags::EPOLLIN, 6),
        )
        .unwrap();
    epoll
        .add(&mic_levels.wake, EpollEvent::new(EpollFlags::EPOLLIN, 7))
        .unwrap();
    epoll
        .add(&viz_levels.wake, EpollEvent::new(EpollFlags::EPOLLIN, 8))
        .unwrap();
    epoll
        .add(&now_playing.wake, EpollEvent::new(EpollFlags::EPOLLIN, 9))
        .unwrap();
    uinput.set_evbit(EventKind::Key).unwrap();
    for k in Key::iter() {
        uinput.set_keybit(k).unwrap();
    }
    let mut dev_name_c = [0 as c_char; 80];
    let dev_name = "Dynamic Function Row Virtual Input Device".as_bytes();
    for i in 0..dev_name.len() {
        dev_name_c[i] = dev_name[i] as c_char;
    }
    uinput
        .dev_setup(&uinput_setup {
            id: input_id {
                bustype: 0x19,
                vendor: 0x1209,
                product: 0x316E,
                version: 1,
            },
            ff_effects_max: 0,
            name: dev_name_c,
        })
        .unwrap();
    uinput.dev_create().unwrap();

    let mut digitizer: Option<InputDevice> = None;
    let mut last_redraw_ts = if layers.current().faster_refresh {
        Local::now().second()
    } else {
        Local::now().minute()
    };
    // The time-of-day background repaints once when its band changes, on
    // the same wake cadence as the clock and battery widgets.
    let mut last_day_band = cfg.day_cycle.band_at(Local::now().hour()).cloned();
    loop {
        if let Some((new_cfg, new_layers)) = cfg_mgr.update_config(width) {
            layers.replace(new_layers, &mut |keys, active| {
                toggle_keys(&mut uinput, keys, active as i32)
            });
            cfg = new_cfg;
            pet.apply_config(cfg.pet_enabled, cfg.pet_variant, cfg.pet_energy);
            last_fn_press = None;
            needs_complete_redraw = true;
        }

        let now = Local::now();
        let ms_left = ((60 - now.second()) * 1000) as i32;
        let mut next_timeout_ms = min(ms_left, TIMEOUT_MS);

        // A new time-of-day band repaints the whole strip once; an
        // unchanged band adds no redraw churn, and the disabled cycle
        // stays `None` without ever waking the bar.
        let day_band = cfg.day_cycle.band_at(now.hour());
        if day_band != last_day_band.as_ref() {
            last_day_band = day_band.cloned();
            needs_complete_redraw = true;
        }

        battery_monitor.update();
        if let Some(remaining) = layers.refresh_batteries(
            &battery_monitor.sources,
            cfg.battery_cycle_enabled,
            Duration::from_secs(cfg.battery_cycle_timeout_secs.into()),
            Instant::now(),
            &mut |keys, active| toggle_keys(&mut uinput, keys, active as i32),
        ) {
            next_timeout_ms = min(
                next_timeout_ms,
                remaining.as_millis().max(1).min(TIMEOUT_MS as u128) as i32,
            );
        }

        // The closed-slider mute watch probes the volume receiver on a slow
        // cadence, and re-probes right after a collapse, so the volume
        // button's mute mark stays current without any slider drawn.
        if let Some((revision, request)) = layers.mute_probe(Instant::now()) {
            volume_control.send(revision, request);
        }
        for (revision, kind, request) in std::mem::take(&mut layers.slider.requests) {
            match kind {
                Kind::Volume => volume_control.send(revision, request),
                Kind::Brightness => brightness_control.send(revision, request),
            }
        }
        for (revision, result) in volume_control.update() {
            if layers.mute_owns(revision) {
                layers.mute_reply(revision, result);
            } else {
                layers.slider_reply(revision, Kind::Volume, result);
            }
        }
        for (revision, result) in brightness_control.update() {
            layers.slider_reply(revision, Kind::Brightness, result);
        }

        // The recording layer's waveform widget polls live microphone input
        // only while that layer is displayed, and its frame clock repaints
        // the interpolated graph at roughly 30 fps while it moves; elsewhere
        // the poller sleeps and this stays at the ordinary low-cadence
        // timeouts.
        let mic_sample = mic_levels.update();
        mic_levels.set_armed(layers.waveform_active());
        let (_, waveform_timeout_ms) = layers.waveform_tick(mic_sample, Instant::now());
        next_timeout_ms = min(next_timeout_ms, waveform_timeout_ms);

        // The visualizer polls the desktop output monitor only while the
        // full-bar view is the displayed layer; arming the poller starts
        // the helper's capture and disarming it - the Off button, any
        // navigation away, a reload - stops the stream.
        let viz_sample = viz_levels.update();
        viz_levels.set_armed(layers.viz_active());
        let (_, visualizer_timeout_ms) = layers.visualizer_tick(viz_sample, Instant::now());
        next_timeout_ms = min(next_timeout_ms, visualizer_timeout_ms);

        // The now-playing row polls the desktop receiver at its slow
        // metadata cadence only while a now-playing layer is displayed;
        // elsewhere the poller sleeps and every row holds the empty state
        // until its layer returns.
        let now_playing_feed = now_playing.update();
        now_playing.set_armed(layers.now_playing_active());
        layers.now_playing_tick(now_playing_feed);

        // The shared slider collapses on its own after a moment.
        layers.slider_refresh(Instant::now());
        if let Some(remaining) = layers.slider_timeout(Instant::now()) {
            next_timeout_ms = min(
                next_timeout_ms,
                remaining.as_millis().max(1).min(TIMEOUT_MS as u128) as i32,
            );
        }

        if cfg.enable_pixel_shift {
            let (pixel_shift_needs_redraw, pixel_shift_next_timeout_ms) = pixel_shift.update();
            if pixel_shift_needs_redraw {
                needs_complete_redraw = true;
            }
            next_timeout_ms = min(next_timeout_ms, pixel_shift_next_timeout_ms);
        }

        // The pet lives on its own layer, reached through the home bar's
        // Pet button; everywhere else it waits unseen.
        let pet_active = layers.pet_active();
        let pet_bar = PetBar {
            width: width as f64,
            height: height as f64,
            active: pet_active,
            // The pet's mood and scene follow the same local hour as the
            // time-of-day background tint.
            hour: now.hour(),
        };
        let pet_pixel_shift_width = if cfg.enable_pixel_shift {
            PIXEL_SHIFT_WIDTH_PX
        } else {
            0
        };
        let pet_spans = if pet_active {
            layers
                .current()
                .layout_spans(width as i32, pet_pixel_shift_width)
        } else {
            Vec::new()
        };
        let (pet_needs_redraw, pet_next_timeout_ms) =
            pet.update(Instant::now(), &pet_bar, &pet_spans);
        if pet_needs_redraw {
            needs_complete_redraw = true;
        }
        next_timeout_ms = min(next_timeout_ms, pet_next_timeout_ms);

        let current_ts = if layers.current().faster_refresh {
            Local::now().second()
        } else {
            Local::now().minute()
        };
        if layers.current().displays_time && (current_ts != last_redraw_ts) {
            needs_complete_redraw = true;
            last_redraw_ts = current_ts;
        }
        if needs_complete_redraw || layers.current().buttons.iter().any(|b| b.1.changed) {
            let shift = if cfg.enable_pixel_shift {
                pixel_shift.get()
            } else {
                (0.0, 0.0)
            };
            // Input uses the exact geometry of the last rendered frame, not
            // an unshifted estimate of where the visible endpoints lie.
            layers.slider.pixel_shift_width = if cfg.enable_pixel_shift {
                PIXEL_SHIFT_WIDTH_PX
            } else {
                0
            };
            layers.slider.pixel_shift_x = shift.0;
            let slider_render = layers.slider_render();
            let background = cfg.day_cycle.background(now.hour());
            let clips = layers.current_mut().draw(
                &cfg,
                (width as i32, height as i32),
                &surface,
                shift,
                needs_complete_redraw,
                slider_render,
                background,
            );
            if pet.is_visible() {
                let pixel_shift_width = if cfg.enable_pixel_shift {
                    PIXEL_SHIFT_WIDTH_PX as f64
                } else {
                    0.0
                };
                let pet_c = Context::new(&surface).unwrap();
                pet_c.translate(height as f64, 0.0);
                pet_c.rotate((90.0f64).to_radians());
                pet.draw(
                    &pet_c,
                    height as i32,
                    (shift.0 + pixel_shift_width / 2.0, shift.1),
                    Instant::now(),
                );
            }
            let data = surface.data().unwrap();
            drm.map().unwrap().as_mut()[..data.len()].copy_from_slice(&data);
            drm.dirty(&clips).unwrap();
            needs_complete_redraw = false;
        }

        match epoll.wait(
            &mut [EpollEvent::new(EpollFlags::EPOLLIN, 0)],
            next_timeout_ms as u16,
        ) {
            Err(Errno::EINTR) | Ok(_) => 0,
            e => e.unwrap(),
        };

        if udev_monitor.iter().last().is_some() {
            battery_monitor.request_refresh();
        }

        input_tb.dispatch().unwrap();
        input_main.dispatch().unwrap();
        for event in &mut input_tb.clone().chain(input_main.clone()) {
            backlight.process_event(&event);
            match event {
                Event::Device(DeviceEvent::Added(evt)) => {
                    let dev = evt.device();
                    if dev.name().contains(" Touch Bar") {
                        digitizer = Some(dev);
                    }
                }
                Event::Device(DeviceEvent::Removed(evt)) => {
                    if Some(evt.device()) == digitizer {
                        layers.release_all(&mut |keys, active| {
                            toggle_keys(&mut uinput, keys, active as i32)
                        });
                        digitizer = None;
                    }
                }
                Event::Keyboard(KeyboardEvent::Key(key)) => {
                    if key.key() == Key::Fn as u32 {
                        let pressed = key.key_state() == KeyState::Pressed;
                        let double_press = pressed
                            && cfg.double_press_switch_layers > 0
                            && last_fn_press.is_some_and(|last| {
                                last.elapsed()
                                    < Duration::from_millis(cfg.double_press_switch_layers.into())
                            });
                        if pressed {
                            last_fn_press = Some(Instant::now());
                        }
                        needs_complete_redraw |=
                            layers.fn_event(pressed, double_press, &mut |keys, active| {
                                toggle_keys(&mut uinput, keys, active as i32)
                            });
                    }
                }
                Event::Touch(te) => {
                    if Some(te.device()) != digitizer || backlight.current_bl() == 0 {
                        continue;
                    }
                    match te {
                        TouchEvent::Down(dn) => {
                            let x = dn.x_transformed(width as u32);
                            let y = dn.y_transformed(height as u32);
                            let pet_shift_x = if cfg.enable_pixel_shift {
                                pixel_shift.get().0 + (PIXEL_SHIFT_WIDTH_PX / 2) as f64
                            } else {
                                0.0
                            };
                            if pet.tap(x, y, Instant::now(), pet_shift_x) {
                                // The pet consumed the tap; the widget
                                // underneath must not fire.
                                needs_complete_redraw = true;
                            } else {
                                needs_complete_redraw |= layers.touch_down_at(
                                    dn.seat_slot(),
                                    x,
                                    y,
                                    width,
                                    height,
                                    &mut |keys, active| {
                                        toggle_keys(&mut uinput, keys, active as i32)
                                    },
                                    &mut |command| {
                                        commands.launch(command);
                                    },
                                );
                            }
                        }
                        TouchEvent::Motion(mtn) => {
                            let x = mtn.x_transformed(width as u32);
                            let y = mtn.y_transformed(height as u32);
                            if layers.slider_drag_motion(
                                mtn.seat_slot(),
                                x,
                                y,
                                width,
                                &mut |keys, active| toggle_keys(&mut uinput, keys, active as i32),
                            ) {
                                continue;
                            }
                            layers.touch_motion(
                                mtn.seat_slot(),
                                x,
                                y,
                                width,
                                height,
                                &mut |keys, active| toggle_keys(&mut uinput, keys, active as i32),
                            );
                        }
                        TouchEvent::Up(up) => {
                            layers.touch_up(up.seat_slot(), &mut |keys, active| {
                                toggle_keys(&mut uinput, keys, active as i32)
                            });
                        }
                        TouchEvent::Cancel(_) => layers.release_all(&mut |keys, active| {
                            toggle_keys(&mut uinput, keys, active as i32)
                        }),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        backlight.update_backlight(&cfg);
        if backlight.current_bl() == 0 {
            layers.release_all(&mut |keys, active| toggle_keys(&mut uinput, keys, active as i32));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PowerSupplyFixture(PathBuf);
    impl PowerSupplyFixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "dfr-battery-test-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn add(&self, name: &str, fields: &[(&str, &str)]) {
            let path = self.0.join(name);
            fs::create_dir(&path).unwrap();
            for (field, value) in fields {
                fs::write(path.join(field), value).unwrap();
            }
        }
    }
    impl Drop for PowerSupplyFixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn sample_sources() -> Vec<BatterySource> {
        [
            ("BAT0", false, 53),
            ("hidpp_mouse", true, 80),
            ("keyboard", true, 4),
        ]
        .into_iter()
        .map(|(id, device, capacity)| BatterySource {
            id: id.into(),
            label: id.into(),
            device,
            capacity,
            state: BatteryState::NotCharging,
        })
        .collect()
    }

    fn battery_button(action: ButtonAction) -> Button {
        let icon = Handle::from_file(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/share/tiny-dfr/bolt.svg"
        ))
        .unwrap();
        let mut button = Button::new_text(String::new(), action);
        button.image = ButtonImage::Battery(
            BatteryWidget::default(),
            BatteryIconMode::Both,
            BatteryImages {
                plain: vec![icon.clone(); 8],
                charging: vec![icon.clone(); 7],
                bolt: icon,
            },
        );
        button
    }

    fn battery_deck() -> LayerDeck {
        let mut layer = FunctionLayer::with_config(vec![
            keys("Held", &[Key::LeftCtrl, Key::F1]),
            keys("Battery", &[Key::Battery]),
        ]);
        layer.buttons[1].1 = battery_button(ButtonAction::Keys(vec![Key::Battery]));
        LayerDeck::new(vec![("home".into(), layer)], 0, None, None, false)
    }

    fn widget(deck: &LayerDeck) -> &BatteryWidget {
        let ButtonImage::Battery(widget, _, _) = &deck.current().buttons[1].1.image else {
            panic!()
        };
        widget
    }

    #[test]
    fn power_supplies_distinguish_system_and_devices_and_skip_absent_or_unreadable() {
        let fixture = PowerSupplyFixture::new();
        fixture.add(
            "hidpp_mouse",
            &[
                ("type", "Battery\n"),
                ("scope", "Device\n"),
                ("capacity", "80\n"),
                ("model_name", "MX Master 3S\n"),
            ],
        );
        fixture.add(
            "keyboard",
            &[
                ("type", "Battery"),
                ("scope", "Device"),
                ("capacity", "4"),
                ("status", "Discharging"),
            ],
        );
        fixture.add(
            "BAT0",
            &[
                ("type", "Battery"),
                ("scope", "System"),
                ("capacity", "53"),
                ("charge_now", "60"),
                ("charge_full", "100"),
                ("status", "Charging"),
            ],
        );
        fixture.add("BAT1", &[("type", "Battery"), ("capacity", "90")]);
        fixture.add("A_no_scope", &[("type", "Battery"), ("capacity", "90")]);
        fixture.add(
            "offline_hidpp",
            &[
                ("type", "Battery"),
                ("scope", "Device"),
                ("online", "0"),
                ("capacity", "80"),
            ],
        );
        fixture.add(
            "absent",
            &[
                ("type", "Battery"),
                ("scope", "Device"),
                ("present", "0"),
                ("capacity", "70"),
            ],
        );
        fixture.add(
            "unreadable",
            &[
                ("type", "Battery"),
                ("scope", "Device"),
                ("capacity", "Unknown"),
            ],
        );
        fixture.add("AC", &[("type", "Mains"), ("capacity", "100")]);
        let sources = read_battery_sources(&fixture.0);
        assert_eq!(
            sources.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["BAT0", "hidpp_mouse", "keyboard"]
        );
        assert!(!sources[0].device);
        assert_eq!(
            sources[0].capacity,
            if cfg!(target_arch = "x86_64") { 60 } else { 53 }
        );
        assert_eq!(sources[0].state, BatteryState::Charging);
        assert!(sources[1].device);
        assert_eq!(sources[1].capacity, 80); // Capacity-only peripherals work on x86_64 too.
        assert_eq!(sources[1].label, "MX Master 1");
        assert_eq!(sources[2].state, BatteryState::NotCharging);
        fs::remove_dir_all(fixture.0.join("hidpp_mouse")).unwrap();
        fs::write(fixture.0.join("keyboard/present"), "0").unwrap();
        assert_eq!(read_battery_sources(&fixture.0).len(), 1);
        assert!(read_battery_sources(&fixture.0.join("missing")).is_empty());
    }

    #[test]
    fn battery_alert_steps_through_the_configured_thresholds() {
        let escalation = BatteryEscalation {
            warning_threshold: 25,
            critical_threshold: 10,
            warning_accent: ButtonAccent::Orange,
            critical_accent: ButtonAccent::Red,
        };
        // Healthy at and above the warning point, warning strictly below it.
        for healthy in [100, 26, 25] {
            assert_eq!(escalation.alert(healthy), BatteryAlert::Healthy);
        }
        for warning in [24, 11, 10] {
            assert_eq!(escalation.alert(warning), BatteryAlert::Warning);
        }
        // Critical strictly below the critical point, boundary included.
        for critical in [9, 1, 0] {
            assert_eq!(escalation.alert(critical), BatteryAlert::Critical);
        }
        // Equal tiers collapse into a single critical step...
        let collapsed = BatteryEscalation {
            warning_threshold: 40,
            critical_threshold: 40,
            ..escalation
        };
        assert_eq!(collapsed.alert(40), BatteryAlert::Healthy);
        assert_eq!(collapsed.alert(39), BatteryAlert::Critical);
        // ...and zero thresholds silence the escalation entirely.
        let disabled = BatteryEscalation {
            warning_threshold: 0,
            critical_threshold: 0,
            ..escalation
        };
        for capacity in [0, 10, 100] {
            assert_eq!(disabled.alert(capacity), BatteryAlert::Healthy);
        }
    }

    #[test]
    fn battery_accent_escalates_only_the_displayed_battery() {
        let escalation = BatteryEscalation::from_config(&outline_config());
        assert_eq!(escalation.warning_threshold, 25);
        assert_eq!(escalation.critical_threshold, 10);
        let now = Instant::now();
        let timeout = Duration::from_secs(15);
        let mut button = battery_button(ButtonAction::Keys(vec![Key::Battery]));
        let battery_neutral = ButtonAccent::for_action(&button.action);
        let source_at = |id: &str, device: bool, capacity: u32| BatterySource {
            id: id.into(),
            label: id.into(),
            device,
            capacity,
            state: BatteryState::NotCharging,
        };
        let refresh = |button: &mut Button, sources: &[BatterySource]| {
            let ButtonImage::Battery(battery, _, _) = &mut button.image else {
                panic!()
            };
            battery.refresh(sources, true, timeout, now);
        };
        let cycle = |button: &mut Button| {
            let ButtonImage::Battery(battery, _, _) = &mut button.image else {
                panic!()
            };
            battery.cycle(now)
        };
        let accent = |button: &Button| button.effective_accent(&escalation);

        // A healthy system battery renders exactly as before: the widget's
        // own accent, escalations silent above the warning point.
        refresh(&mut button, &[source_at("BAT0", false, 80)]);
        assert_eq!(accent(&button), battery_neutral);
        // Below the warning point the displayed battery turns the warning
        // color; below the critical point, the critical color.
        refresh(&mut button, &[source_at("BAT0", false, 20)]);
        assert_eq!(accent(&button), ButtonAccent::Orange);
        refresh(&mut button, &[source_at("BAT0", false, 5)]);
        assert_eq!(accent(&button), ButtonAccent::Red);

        // The tier follows whichever battery is on show: the system stays
        // healthy while the cycled device battery reads warning on its own.
        refresh(
            &mut button,
            &[
                source_at("BAT0", false, 80),
                source_at("hidpp_mouse", true, 20),
            ],
        );
        assert_eq!(accent(&button), battery_neutral);
        assert!(cycle(&mut button));
        assert_eq!(accent(&button), ButtonAccent::Orange);

        // A charging battery keeps the charging green at every level.
        let mut charging = source_at("BAT0", false, 5);
        charging.state = BatteryState::Charging;
        refresh(&mut button, &[charging]);
        assert_eq!(accent(&button), ButtonAccent::Green);

        // The configured escalation colors replace the shipped defaults.
        let custom = BatteryEscalation {
            warning_accent: ButtonAccent::Violet,
            critical_accent: ButtonAccent::Blue,
            ..escalation
        };
        refresh(&mut button, &[source_at("BAT0", false, 20)]);
        assert_eq!(button.effective_accent(&custom), ButtonAccent::Violet);
        refresh(&mut button, &[source_at("BAT0", false, 5)]);
        assert_eq!(button.effective_accent(&custom), ButtonAccent::Blue);

        // With no displayed battery ("Battery N/A") the accent is the
        // widget's own, as before.
        refresh(&mut button, &[]);
        assert_eq!(accent(&button), battery_neutral);
    }

    #[test]
    fn battery_cycles_system_devices_system_and_timeout_is_from_last_tap() {
        let sources = sample_sources();
        let now = Instant::now();
        let timeout = Duration::from_secs(15);
        let mut battery = BatteryWidget::default();
        battery.refresh(&sources, true, timeout, now);
        assert_eq!(battery.source().unwrap().id, "BAT0");
        assert_eq!(battery.label(), Some("SYS"));
        assert!(battery.cycle(now));
        assert_eq!(battery.source().unwrap().capacity, 80);
        assert_eq!(battery.label(), Some("hidpp_mouse"));
        assert!(battery.cycle(now + Duration::from_secs(10)));
        assert_eq!(battery.source().unwrap().id, "keyboard");
        assert!(!battery.refresh(&sources, true, timeout, now + timeout));
        assert!(battery.refresh(&sources, true, timeout, now + Duration::from_secs(25)));
        assert_eq!(battery.source().unwrap().id, "BAT0");
        assert!(battery.return_at.is_none());
        for expected in ["hidpp_mouse", "keyboard", "BAT0", "hidpp_mouse"] {
            assert!(battery.cycle(now));
            assert_eq!(battery.source().unwrap().id, expected);
        }
    }

    #[test]
    fn hotplug_preserves_source_identity_and_removal_or_disabling_returns_to_system() {
        let now = Instant::now();
        let timeout = Duration::from_secs(15);
        let mut sources = sample_sources();
        let mut battery = BatteryWidget::default();
        battery.refresh(&sources, true, timeout, now);
        battery.cycle(now);
        sources[1].capacity = 75;
        sources.insert(
            1,
            BatterySource {
                id: "new".into(),
                label: "new".into(),
                device: true,
                capacity: 50,
                state: BatteryState::NotCharging,
            },
        );
        assert!(!battery.refresh(&sources, true, timeout, now));
        assert_eq!(battery.source().unwrap().id, "hidpp_mouse");
        assert_eq!(battery.source().unwrap().capacity, 75);
        sources.retain(|source| source.id != "hidpp_mouse");
        assert!(battery.refresh(&sources, true, timeout, now));
        assert_eq!(battery.source().unwrap().id, "BAT0");
        battery.cycle(now);
        assert!(battery.refresh(&sources, false, timeout, now));
        assert!(!battery.cycle(now));
        assert!(battery.label().is_none());
        battery.refresh(&sources[..1], true, timeout, now);
        assert!(!battery.cycle(now));
        assert!(battery.label().is_none());
        battery.refresh(&[], true, timeout, now);
        assert!(battery.source().is_none());
        assert!(!battery.cycle(now));
        battery.refresh(&sources[1..], true, timeout, now);
        assert!(battery.source().is_none()); // A peripheral never masquerades as the computer.
        assert!(battery.cycle(now));
        assert!(battery.source().unwrap().device);
    }

    #[test]
    fn cycling_releases_keys_and_does_not_send_the_battery_action_or_repeat_on_motion() {
        let mut deck = battery_deck();
        let sources = sample_sources();
        deck.refresh_batteries(
            &sources,
            true,
            Duration::from_secs(15),
            Instant::now(),
            &mut |_, _| {},
        );
        let mut events = Vec::new();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.touch_down(0, 0, 100.0, 30.0, &mut emit, &mut |_| {});
        assert!(deck.touch_down(1, 1, 1100.0, 30.0, &mut emit, &mut |_| {}));
        assert!(deck.touches.is_empty());
        assert_eq!(widget(&deck).source().unwrap().id, "hidpp_mouse");
        deck.touch_motion(1, 1100.0, 30.0, 2008, 60, &mut emit);
        deck.touch_motion(0, 100.0, 30.0, 2008, 60, &mut emit);
        deck.touch_up(1, &mut emit);
        assert_eq!(widget(&deck).source().unwrap().id, "hidpp_mouse");
        assert!(deck.touch_down(1, 1, 1100.0, 30.0, &mut emit, &mut |_| {}));
        assert_eq!(widget(&deck).source().unwrap().id, "keyboard");
        assert_eq!(
            events,
            vec![
                (vec![Key::LeftCtrl, Key::F1], true),
                (vec![Key::LeftCtrl, Key::F1], false)
            ]
        );
    }

    #[test]
    fn timed_return_and_selected_device_removal_release_newly_held_keys() {
        for removal in [false, true] {
            let mut deck = battery_deck();
            let sources = sample_sources();
            let timeout = Duration::from_secs(15);
            deck.refresh_batteries(&sources, true, timeout, Instant::now(), &mut |_, _| {});
            deck.touch_down(0, 1, 1100.0, 30.0, &mut |_, _| {}, &mut |_| {});
            let deadline = widget(&deck).return_at.unwrap();
            let mut events = Vec::new();
            let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
            deck.touch_down(1, 0, 100.0, 30.0, &mut emit, &mut |_| {});
            let snapshot = if removal { &sources[..1] } else { &sources[..] };
            let now = if removal {
                deadline - timeout
            } else {
                deadline
            };
            assert!(deck
                .refresh_batteries(snapshot, true, timeout, now, &mut emit)
                .is_none());
            assert_eq!(widget(&deck).source().unwrap().id, "BAT0");
            assert!(deck.touches.is_empty());
            deck.touch_motion(1, 1100.0, 30.0, 2008, 60, &mut emit);
            deck.touch_up(1, &mut emit);
            assert_eq!(
                events,
                vec![
                    (vec![Key::LeftCtrl, Key::F1], true),
                    (vec![Key::LeftCtrl, Key::F1], false)
                ]
            );
        }
    }

    #[test]
    fn disabled_cycling_and_no_device_retain_legacy_battery_key_action() {
        let sources = sample_sources();
        for (enabled, snapshot) in [(false, &sources[..]), (true, &sources[..1])] {
            let mut deck = battery_deck();
            deck.refresh_batteries(
                snapshot,
                enabled,
                Duration::from_secs(15),
                Instant::now(),
                &mut |_, _| {},
            );
            let mut events = Vec::new();
            let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
            assert!(!deck.touch_down(0, 1, 1100.0, 30.0, &mut emit, &mut |_| {}));
            deck.touch_up(0, &mut emit);
            assert_eq!(widget(&deck).source().unwrap().id, "BAT0");
            assert!(widget(&deck).label().is_none());
            assert_eq!(
                events,
                vec![(vec![Key::Battery], true), (vec![Key::Battery], false)]
            );
        }
    }

    #[test]
    fn background_battery_monitor_wakes_epoll_for_initial_and_hotplug_snapshots() {
        let fixture = PowerSupplyFixture::new();
        fixture.add("BAT0", &[("type", "Battery"), ("capacity", "53")]);
        let mut monitor = BatteryMonitor::new(fixture.0.clone());
        let epoll = Epoll::new(EpollCreateFlags::empty()).unwrap();
        epoll
            .add(&monitor.wake, EpollEvent::new(EpollFlags::EPOLLIN, 0))
            .unwrap();
        let mut events = [EpollEvent::empty()];
        assert_eq!(epoll.wait(&mut events, 2000u16).unwrap(), 1);
        monitor.update();
        assert_eq!(monitor.sources.len(), 1);
        fixture.add(
            "hidpp_mouse",
            &[("type", "Battery"), ("scope", "Device"), ("capacity", "80")],
        );
        monitor.request_refresh();
        assert_eq!(epoll.wait(&mut events, 2000u16).unwrap(), 1);
        monitor.update();
        assert_eq!(monitor.sources.len(), 2);
        fs::remove_dir_all(fixture.0.join("hidpp_mouse")).unwrap();
        monitor.request_refresh();
        assert_eq!(epoll.wait(&mut events, 2000u16).unwrap(), 1);
        monitor.update();
        assert_eq!(monitor.sources.len(), 1);
    }

    #[test]
    fn every_battery_mode_renders_source_labels_and_keeps_the_legacy_display() {
        fn render(button: &Button) -> Vec<u8> {
            let mut surface = ImageSurface::create(Format::ARgb32, 200, 60).unwrap();
            {
                let c = Context::new(&surface).unwrap();
                c.set_font_size(32.0);
                c.set_source_rgb(1.0, 1.0, 1.0);
                button.render(&c, 60, 0.0, 200, 0.0);
                assert_eq!(c.font_matrix().xx(), 32.0); // The next widget keeps its font size.
            }
            let pixels = surface.data().unwrap().to_vec();
            pixels
        }
        for mode in [
            BatteryIconMode::Icon,
            BatteryIconMode::Percentage,
            BatteryIconMode::Both,
        ] {
            let mut button = battery_button(ButtonAction::default());
            let ButtonImage::Battery(battery, battery_mode, _) = &mut button.image else {
                panic!()
            };
            *battery_mode = mode;
            let mut sources = sample_sources();
            for source in &mut sources {
                source.capacity = 50;
                source.state = BatteryState::NotCharging;
            }
            let now = Instant::now();
            let timeout = Duration::from_secs(15);
            battery.refresh(&sources, true, timeout, now);
            let system = render(&button);
            let ButtonImage::Battery(battery, _, _) = &mut button.image else {
                panic!()
            };
            battery.cycle(now);
            let mouse = render(&button);
            let ButtonImage::Battery(battery, _, _) = &mut button.image else {
                panic!()
            };
            battery.cycle(now);
            let keyboard = render(&button);
            // Equal readings and icons still render differently for each source.
            assert_ne!(system, mouse);
            assert_ne!(mouse, keyboard);
            let ButtonImage::Battery(battery, _, _) = &mut button.image else {
                panic!()
            };
            battery.refresh(&sources, false, timeout, now);
            let disabled = render(&button);
            let ButtonImage::Battery(battery, _, _) = &mut button.image else {
                panic!()
            };
            battery.refresh(&sources[..1], true, timeout, now);
            assert_eq!(disabled, render(&button));
        }
    }

    fn text(label: &str, action: ButtonAction) -> ButtonConfig {
        ButtonConfig {
            text: Some(label.into()),
            action,
            ..ButtonConfig::default()
        }
    }
    fn keys(label: &str, keys: &[Key]) -> ButtonConfig {
        text(label, ButtonAction::Keys(keys.to_vec()))
    }
    fn nav(label: &str, layer: &str) -> ButtonConfig {
        text(label, ButtonAction::SwitchLayer(layer.into()))
    }
    fn named_deck() -> LayerDeck {
        LayerDeck::new(
            vec![
                (
                    "home".into(),
                    FunctionLayer::with_config(vec![
                        keys("Combo", &[Key::LeftCtrl, Key::F1]),
                        nav("Functions", "function"),
                        nav("Media", "media"),
                    ]),
                ),
                (
                    "function".into(),
                    FunctionLayer::with_config(vec![nav("Home", "home"), keys("F1", &[Key::F1])]),
                ),
                (
                    "media".into(),
                    FunctionLayer::with_config(vec![
                        nav("Home", "home"),
                        keys("Play", &[Key::PlayPause]),
                    ]),
                ),
            ],
            0,
            Some(1),
            None,
            false,
        )
    }

    /// A deck shaped like the daemon builds it with the pet enabled: the
    /// home bar ends with a Pet button opening the synthesized pet layer.
    fn pet_deck() -> LayerDeck {
        LayerDeck::new(
            vec![
                (
                    "home".into(),
                    FunctionLayer::with_config(vec![
                        keys("Combo", &[Key::LeftCtrl, Key::F1]),
                        nav("Pet", "pet"),
                    ]),
                ),
                (
                    "pet".into(),
                    FunctionLayer::with_config(vec![
                        nav("Home", "home"),
                        ButtonConfig {
                            stretch: Some(7),
                            ..ButtonConfig::default()
                        },
                    ]),
                ),
            ],
            0,
            None,
            Some(1),
            false,
        )
    }

    /// A deck shaped like the multibar example's voice-dictation flow: a
    /// home-bar mic button combining a start command with the layer switch,
    /// and a recording layer whose Cancel and Stop buttons run their command
    /// and return home. One plain command button rides the home bar.
    fn voice_deck() -> LayerDeck {
        let command_layer = |command: &str, layer: &str| {
            text(
                "",
                ButtonAction::CommandLayer {
                    command: command.into(),
                    layer: layer.into(),
                },
            )
        };
        LayerDeck::new(
            vec![
                (
                    "home".into(),
                    FunctionLayer::with_config(vec![
                        keys("Combo", &[Key::LeftCtrl, Key::F1]),
                        text(
                            "Mic",
                            ButtonAction::CommandLayer {
                                command: "voice-dictate start".into(),
                                layer: "recording".into(),
                            },
                        ),
                        text("Sync", ButtonAction::Command("sync-now".into())),
                    ]),
                ),
                (
                    "recording".into(),
                    FunctionLayer::with_config(vec![
                        command_layer("voice-dictate cancel", "home"),
                        text("Recording", ButtonAction::default()),
                        command_layer("voice-dictate stop", "home"),
                    ]),
                ),
            ],
            0,
            None,
            None,
            false,
        )
    }

    #[test]
    fn command_buttons_fire_on_touch_down_and_combined_presses_switch_layers() {
        let mut deck = voice_deck();
        let mut events = Vec::new();
        let mut commands = Vec::new();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        let mut fired = |command: &str| commands.push(command.to_owned());

        // One mic press starts dictation and opens the recording layer; the
        // touch is released with the switch, like any navigation button.
        assert!(deck.touch_down(0, 1, 1100.0, 30.0, &mut emit, &mut fired));
        assert_eq!(deck.active, deck.names["recording"]);
        assert!(deck.touches.is_empty());
        // Cancel discards a mis-start and returns home.
        assert!(deck.touch_down(1, 0, 100.0, 30.0, &mut emit, &mut fired));
        assert_eq!(deck.active, deck.home);
        // Stop keeps the recording and returns home the same way.
        assert!(deck.touch_down(0, 1, 1100.0, 30.0, &mut emit, &mut fired));
        assert!(deck.touch_down(1, 2, 1900.0, 30.0, &mut emit, &mut fired));
        assert_eq!(deck.active, deck.home);
        // A plain command button fires on touch-down and keeps its momentary
        // highlight while held, without sending any key events.
        assert!(!deck.touch_down(2, 2, 1900.0, 30.0, &mut emit, &mut fired));
        assert!(deck.current().buttons[2].1.active);
        deck.touch_up(2, &mut emit);
        assert!(!deck.current().buttons[2].1.active);
        // Each press fired exactly its own command, in order.
        assert_eq!(
            commands,
            vec![
                "voice-dictate start",
                "voice-dictate cancel",
                "voice-dictate start",
                "voice-dictate stop",
                "sync-now",
            ]
        );
        assert!(events.is_empty());
    }

    /// A deck shaped like the multibar voice flow, with the recording
    /// layer's middle button carrying the live waveform widget.
    fn waveform_deck() -> LayerDeck {
        let command_layer = |command: &str, layer: &str| {
            text(
                "",
                ButtonAction::CommandLayer {
                    command: command.into(),
                    layer: layer.into(),
                },
            )
        };
        LayerDeck::new(
            vec![
                (
                    "home".into(),
                    FunctionLayer::with_config(vec![
                        keys("Combo", &[Key::LeftCtrl, Key::F1]),
                        text(
                            "Mic",
                            ButtonAction::CommandLayer {
                                command: "voice-dictate start".into(),
                                layer: "recording".into(),
                            },
                        ),
                    ]),
                ),
                (
                    "recording".into(),
                    FunctionLayer::with_config(vec![
                        command_layer("voice-dictate cancel", "home"),
                        ButtonConfig {
                            waveform: Some(true),
                            stretch: Some(3),
                            ..ButtonConfig::default()
                        },
                        command_layer("voice-dictate stop", "home"),
                    ]),
                ),
            ],
            0,
            None,
            None,
            false,
        )
    }

    #[test]
    fn the_waveform_runs_a_fast_cadence_only_on_the_recording_layer() {
        let mut deck = waveform_deck();
        let now = Instant::now();
        // On the home bar nothing runs fast and no button is flagged.
        assert!(!deck.waveform_active());
        let (changed, timeout) = deck.waveform_tick(None, now);
        assert!(!changed);
        assert_eq!(timeout, i32::MAX - 1);
        assert!(deck
            .layers
            .iter()
            .all(|layer| layer.buttons.iter().all(|(_, b)| !b.changed)));

        // The mic press opens the recording layer and its widget session.
        assert!(deck.touch_down(0, 1, 1100.0, 30.0, &mut |_, _| {}, &mut |_| {}));
        assert_eq!(deck.active, deck.names["recording"]);
        assert!(deck.waveform_active());
        let (changed, timeout) = deck.waveform_tick(None, now);
        assert!(changed); // the first activation frame
        assert!((1..=1000).contains(&timeout)); // the timer's second cadence
        assert!(deck.current().buttons[1].1.changed);

        // Live samples stream into the graph while the layer is displayed.
        for level in [10, 45, 80, 22] {
            let (changed, _) = deck.waveform_tick(
                Some(MicSample {
                    level,
                    live: true,
                    dead: false,
                }),
                now,
            );
            assert!(changed);
        }
        let ButtonImage::Waveform(wave) = &deck.current().buttons[1].1.image else {
            panic!()
        };
        assert_eq!(
            wave.history().oldest_first().collect::<Vec<_>>(),
            vec![10, 45, 80, 22]
        );
        deck.current_mut().buttons[1].1.changed = false;

        // The duration timer follows the monotonic clock from layer entry.
        let later = now + Duration::from_millis(61_400);
        let (changed, timeout) = deck.waveform_tick(None, later);
        assert!(changed);
        assert!((1..=1000).contains(&timeout));
        let ButtonImage::Waveform(wave) = &deck.current().buttons[1].1.image else {
            panic!()
        };
        assert_eq!(wave.elapsed(later), Duration::from_millis(61_400));
        assert_eq!(waveform::format_duration(wave.elapsed(later)), "1:01");
        // The repaint consumes the change flag.
        deck.current_mut().buttons[1].1.changed = false;

        // Leaving the recording layer ends the session and the fast cadence;
        // a late in-flight sample never repaints any other layer.
        assert!(deck.touch_down(1, 0, 100.0, 30.0, &mut |_, _| {}, &mut |_| {}));
        assert_eq!(deck.active, deck.home);
        let later = later + Duration::from_secs(5);
        let (changed, timeout) = deck.waveform_tick(
            Some(MicSample {
                level: 90,
                live: true,
                dead: false,
            }),
            later,
        );
        assert!(!changed);
        assert_eq!(timeout, i32::MAX - 1);
        assert!(deck
            .layers
            .iter()
            .all(|layer| layer.buttons.iter().all(|(_, b)| !b.changed)));

        // Re-entering starts the timer and graph fresh.
        assert!(deck.touch_down(2, 1, 1100.0, 30.0, &mut |_, _| {}, &mut |_| {}));
        let (changed, _) = deck.waveform_tick(None, later);
        assert!(changed);
        let ButtonImage::Waveform(wave) = &deck.current().buttons[1].1.image else {
            panic!()
        };
        assert!(wave.history().is_empty());
        assert_eq!(wave.elapsed(later), Duration::ZERO);
    }

    #[test]
    fn a_dead_feed_marks_its_button_once_and_rests_at_the_timer_cadence() {
        let mut deck = waveform_deck();
        let now = Instant::now();
        // The mic press opens the recording layer and its widget session.
        assert!(deck.touch_down(0, 1, 1100.0, 30.0, &mut |_, _| {}, &mut |_| {}));
        assert_eq!(deck.active, deck.names["recording"]);
        let (changed, _) = deck.waveform_tick(None, now);
        assert!(changed); // the first activation frame
        deck.current_mut().buttons[1].1.changed = false;

        // A live sample, then the feed dies: the flip requests exactly one
        // repaint of the waveform button, so the dead state reaches glass.
        let (changed, _) = deck.waveform_tick(
            Some(MicSample {
                level: 60,
                live: true,
                dead: false,
            }),
            now,
        );
        assert!(changed);
        deck.current_mut().buttons[1].1.changed = false;
        let dead_at = now + Duration::from_millis(50);
        let (changed, timeout) = deck.waveform_tick(Some(MicSample::dead()), dead_at);
        assert!(changed, "the dead flip must repaint the button");
        // The inherited pulse is still settling, so the frame clock runs
        // on; the dead state itself animates nothing.
        assert_eq!(timeout, waveform::FRAME_INTERVAL_MS);
        deck.current_mut().buttons[1].1.changed = false;

        // A repeated dead poll at the same instant adds no repaint of its
        // own - the state is already on glass.
        let (changed, _) = deck.waveform_tick(Some(MicSample::dead()), dead_at);
        assert!(!changed);

        // A dead feed is a state, not an animation: past the inherited
        // pulse decay nothing moves, and repeated dead polls change
        // nothing further.
        let mut settle = dead_at + Duration::from_millis(waveform::FRAME_INTERVAL_MS as u64);
        let mut settling = 0;
        while deck.waveform_tick(None, settle).0 {
            settle += Duration::from_millis(waveform::FRAME_INTERVAL_MS as u64);
            settling += 1;
            assert!(settling < 100, "dead widget never settled");
        }
        // The last settling frame left its repaint queued; consume it, and
        // a settled dead widget then requests nothing further.
        deck.current_mut().buttons[1].1.changed = false;
        let (changed, _) = deck.waveform_tick(Some(MicSample::dead()), settle);
        assert!(!changed);
        assert!(deck
            .layers
            .iter()
            .all(|layer| layer.buttons.iter().all(|(_, b)| !b.changed)));

        // Recovery flips the state back with one repaint.
        let (changed, _) = deck.waveform_tick(
            Some(MicSample {
                level: 40,
                live: true,
                dead: false,
            }),
            settle + Duration::from_millis(50),
        );
        assert!(changed);
    }

    /// A deck shaped like the multibar example's media flow: the media bar
    /// with its play control and the opt-in visualizer button at the end.
    fn media_deck() -> LayerDeck {
        LayerDeck::new(
            vec![
                (
                    "home".into(),
                    FunctionLayer::with_config(vec![
                        keys("Combo", &[Key::LeftCtrl, Key::F1]),
                        nav("Media", "media"),
                    ]),
                ),
                (
                    "media".into(),
                    FunctionLayer::with_config(vec![
                        nav("Home", "home"),
                        keys("Play", &[Key::PlayPause]),
                        ButtonConfig {
                            visualizer: Some(true),
                            stretch: Some(2),
                            ..ButtonConfig::default()
                        },
                    ]),
                ),
            ],
            0,
            Some(1),
            None,
            false,
        )
    }

    /// A deck shaped like the shipped media bar's now-playing row: the
    /// media bar carries the row after the transport, before the opt-in
    /// visualizer button.
    fn now_playing_deck() -> LayerDeck {
        LayerDeck::new(
            vec![
                (
                    "home".into(),
                    FunctionLayer::with_config(vec![
                        keys("Combo", &[Key::LeftCtrl, Key::F1]),
                        nav("Media", "media"),
                    ]),
                ),
                (
                    "media".into(),
                    FunctionLayer::with_config(vec![
                        nav("Home", "home"),
                        keys("Play", &[Key::PlayPause]),
                        ButtonConfig {
                            now_playing: Some(true),
                            stretch: Some(2),
                            ..ButtonConfig::default()
                        },
                        ButtonConfig {
                            visualizer: Some(true),
                            stretch: Some(2),
                            ..ButtonConfig::default()
                        },
                    ]),
                ),
            ],
            0,
            Some(1),
            None,
            false,
        )
    }

    #[test]
    fn the_now_playing_row_replaces_its_label_and_never_holds_a_stale_track() {
        use now_playing::Track;
        let mut deck = now_playing_deck();
        let track = |title: &str| {
            Feed::Playing(Track {
                title: title.into(),
                artist: Some("Artist".into()),
            })
        };

        // On the home bar the poller is disarmed and no row changes.
        assert!(!deck.now_playing_active());
        assert!(!deck.now_playing_tick(Some(track("Elsewhere"))));

        // Enter the media bar: the row is present and its layer arms.
        deck.switch_named("media", &mut |_, _| {});
        assert!(deck.now_playing_active());
        let index = deck.current().now_playing.unwrap();

        // The first track lands and marks the button for repaint.
        assert!(deck.now_playing_tick(Some(track("First"))));
        assert!(deck.current().buttons[index].1.changed);
        deck.current_mut().buttons[index].1.changed = false;
        // The same track again adds no repaint.
        assert!(!deck.now_playing_tick(Some(track("First"))));
        // A track change lands live.
        assert!(deck.now_playing_tick(Some(track("Second"))));
        deck.current_mut().buttons[index].1.changed = false;

        // Pausing clears the row: the empty state, never the stale
        // track; repeated empty polls stay quiet.
        assert!(deck.now_playing_tick(Some(Feed::Idle)));
        assert!(deck.current().buttons[index].1.changed);
        deck.current_mut().buttons[index].1.changed = false;
        assert!(!deck.now_playing_tick(Some(Feed::Idle)));
        // The clear was real: the same track the row had before pausing
        // repaints on reapplied arrival instead of matching on glass.
        assert!(deck.now_playing_tick(Some(track("Second"))));
        deck.current_mut().buttons[index].1.changed = false;

        // Leaving the media bar resets every row, so a later return
        // starts from the next poll instead of an old track; ticks on
        // other layers change nothing.
        assert!(deck.now_playing_tick(Some(track("Third"))));
        deck.switch_named("home", &mut |_, _| {});
        assert!(!deck.now_playing_active());
        assert!(!deck.now_playing_tick(None));
        // The leave was a real reset: "Third" was already applied before
        // it, and its reappearance back on the media bar repaints.
        deck.switch_named("media", &mut |_, _| {});
        assert!(deck.now_playing_tick(Some(track("Third"))));
    }

    #[test]
    fn the_visualizer_view_is_off_by_default_full_bar_and_shuts_down_cleanly() {
        let mut deck = media_deck();
        let now = Instant::now();
        let mut emit = |_: &[Key], _: bool| panic!("visualizer toggle emitted keys");

        // The deck synthesized exactly one full-bar view for the media bar.
        let view = deck.names["visualizer:media"];
        assert_eq!(deck.layers.len(), 3);
        assert_eq!(
            deck.viz_view_of,
            HashMap::from([(deck.names["media"], view)])
        );
        assert_eq!(deck.layers[view].buttons.len(), 2);
        assert_eq!(deck.layers[view].virtual_button_count, 6);
        assert!(deck.layers[view].viz_view);
        assert_eq!(deck.layers[view].visualizer, Some(0));
        assert!(matches!(
            &deck.layers[view].buttons[1].1,
            Button { image: ButtonImage::Text(text), action: ButtonAction::SwitchLayer(target), .. }
                if text == "Off" && target == "media"
        ));
        // The graph button itself is inert: only Off leaves the view.
        assert_eq!(
            deck.layers[view].buttons[0].1.action,
            ButtonAction::Keys(vec![])
        );

        // On the home bar nothing runs.
        assert!(!deck.viz_active());
        let (changed, timeout) = deck.visualizer_tick(None, now);
        assert!(!changed);
        assert_eq!(timeout, i32::MAX - 1);

        // Enter the media bar: the visualizer button is present but off -
        // it never shows by default, and the media bar is exactly the
        // configured layout it will return to.
        assert!(deck.touch_down(0, 1, 1100.0, 30.0, &mut emit, &mut |_| {}));
        assert_eq!(deck.active, deck.names["media"]);
        assert_eq!(deck.current().visualizer, Some(2));
        assert!(!deck.viz_active());
        let before = deck.current().layout_spans(2008, 0);
        let (changed, timeout) = deck.visualizer_tick(None, now);
        assert!(!changed);
        assert_eq!(timeout, i32::MAX - 1);

        // The Viz tap is a pure layer switch: no keys, no commands, no
        // held touch, and the full-bar view is what comes up.
        assert!(deck.touch_down(1, 2, 1600.0, 30.0, &mut emit, &mut |_| {}));
        assert!(deck.touches.is_empty());
        assert_eq!(deck.active, view);
        assert!(deck.viz_active());

        // Live monitor samples stream into the full-bar graph at the
        // frame cadence, filling everything but the Off button.
        for level in [12, 47, 78, 33] {
            let (changed, timeout) = deck.visualizer_tick(
                Some(MicSample {
                    level,
                    live: true,
                    dead: false,
                }),
                now,
            );
            assert!(changed);
            assert_eq!(timeout, waveform::FRAME_INTERVAL_MS);
        }
        let ButtonImage::Waveform(viz) = &deck.current().buttons[0].1.image else {
            panic!()
        };
        assert_eq!(
            viz.history().oldest_first().collect::<Vec<_>>(),
            vec![12, 47, 78, 33]
        );
        let spans = deck.current().layout_spans(2008, 0);
        assert_eq!(spans.len(), 2);
        assert!(spans[0].right - spans[0].left > 0.8 * 2008.0);

        // Tapping the graph itself is inert: it holds no keys, so the
        // press may highlight it but nothing ever fires, and the view
        // stays up.
        let mut silent =
            |keys: &[Key], _: bool| assert!(keys.is_empty(), "the graph button emitted {keys:?}");
        assert!(!deck.touch_down(2, 0, 1000.0, 30.0, &mut silent, &mut |_| {}));
        assert_eq!(deck.active, view);
        assert!(deck.viz_active());
        deck.touch_up(2, &mut silent);

        // The Off tap returns to the media bar unchanged: same buttons,
        // same hit spans, the toggle never sticky.
        assert!(deck.touch_down(3, 1, 1850.0, 30.0, &mut emit, &mut |_| {}));
        assert_eq!(deck.active, deck.names["media"]);
        assert!(!deck.viz_active());
        assert_eq!(deck.current().layout_spans(2008, 0), before);
        let later = now + Duration::from_secs(4);
        let (changed, timeout) = deck.visualizer_tick(
            Some(MicSample {
                level: 90,
                live: true,
                dead: false,
            }),
            later,
        );
        assert!(!changed);
        assert_eq!(timeout, i32::MAX - 1);
        // The restored media bar is fully settled - no button waits on a
        // repaint, and a late in-flight sample marks nothing changed.
        assert!(deck.current().buttons.iter().all(|(_, b)| !b.changed));
        let ButtonImage::Waveform(viz) = &deck.current().buttons[2].1.image else {
            panic!()
        };
        assert!(viz.history().is_empty());

        // Leaving the view by any other navigation - here, straight to
        // home - ends the session the same way, and coming back to the
        // media bar starts from off again.
        assert!(deck.touch_down(4, 2, 1600.0, 30.0, &mut emit, &mut |_| {}));
        assert!(deck.viz_active());
        assert!(deck.switch_named("home", &mut emit));
        assert!(!deck.viz_active());
        let (changed, _) = deck.visualizer_tick(
            Some(MicSample {
                level: 90,
                live: true,
                dead: false,
            }),
            later,
        );
        assert!(!changed);
        assert!(deck.switch_named("media", &mut emit));
        assert!(!deck.viz_active());

        // A reload replaces the deck wholesale: the fresh one is off.
        assert!(deck.touch_down(5, 2, 1600.0, 30.0, &mut emit, &mut |_| {}));
        assert!(deck.viz_active());
        deck.replace(media_deck(), &mut |_, _| {});
        assert!(!deck.viz_active());
        let (changed, timeout) = deck.visualizer_tick(None, later);
        assert!(!changed);
        assert_eq!(timeout, i32::MAX - 1);
    }

    #[test]
    fn waveform_layers_mix_freely_with_other_widgets_and_keep_their_behavior() {
        let mut deck = LayerDeck::new(
            vec![
                (
                    "home".into(),
                    FunctionLayer::with_config(vec![
                        keys("Combo", &[Key::LeftCtrl, Key::F1]),
                        ButtonConfig {
                            time: Some("%H:%M:%S".into()),
                            ..ButtonConfig::default()
                        },
                        nav("Rec", "recording"),
                    ]),
                ),
                (
                    "recording".into(),
                    FunctionLayer::with_config(vec![
                        nav("Home", "home"),
                        ButtonConfig {
                            waveform: Some(true),
                            stretch: Some(2),
                            ..ButtonConfig::default()
                        },
                        keys("F1", &[Key::F1]),
                    ]),
                ),
            ],
            0,
            None,
            None,
            false,
        );
        // The time widget keeps its own refresh cadence flag; the waveform
        // layer neither inherits nor disturbs it.
        assert!(deck.layers[0].faster_refresh);
        assert!(!deck.layers[1].faster_refresh);
        assert_eq!(deck.layers[0].waveform, None);
        assert_eq!(deck.layers[1].waveform, Some(1));
        // Keys and navigation on the recording layer still work around the widget.
        let mut events = Vec::new();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        assert!(deck.switch_named("recording", &mut emit));
        deck.waveform_tick(
            Some(MicSample {
                level: 50,
                live: true,
                dead: false,
            }),
            Instant::now(),
        );
        deck.touch_down(0, 2, 1500.0, 30.0, &mut emit, &mut |_| {});
        deck.touch_up(0, &mut emit);
        assert_eq!(events, vec![(vec![Key::F1], true), (vec![Key::F1], false)]);
        // The pet sees the waveform button like any other occupied span.
        let spans = deck.current().layout_spans(2008, 0);
        assert_eq!(spans.len(), 3);
        assert!(!spans.iter().any(|span| span.clock));
    }

    #[test]
    fn the_waveform_repaints_incrementally_and_only_within_its_button() {
        let config = outline_config();
        let mut layer = FunctionLayer::with_config(vec![
            nav("Home", "home"),
            ButtonConfig {
                waveform: Some(true),
                stretch: Some(2),
                ..ButtonConfig::default()
            },
        ]);
        let (width, height) = (2008, 60);
        // Draw over a time-of-day band, like the strip actually renders.
        let evening = crate::daycycle::DayCycle::default().background(19);
        let mut surface = ImageSurface::create(Format::ARgb32, height, width).unwrap();
        layer.draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            true,
            None,
            evening,
        );
        let idle = surface.data().unwrap().to_vec();
        // One live sample marks only the waveform button changed.
        let ButtonImage::Waveform(wave) = &mut layer.buttons[1].1.image else {
            panic!()
        };
        let now = Instant::now();
        wave.activate(now);
        // The sample is stamped a full period old, so the interpolation
        // rests at phase 1 and the two draws below compare identical
        // pixels instead of a frame that glides between them.
        assert!(wave.sample(
            MicSample {
                level: 77,
                live: true,
                dead: false
            },
            now - Duration::from_millis(60)
        ));
        // The deck marks the button changed; a direct widget feed does not.
        layer.buttons[1].1.changed = true;
        assert!(!layer.buttons[0].1.changed);
        let spans = layer.layout_spans(width, 0);
        let clips = layer.draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            false,
            None,
            evening,
        );
        assert_eq!(clips.len(), 1);
        let clip = clips[0];
        assert!(f64::from(clip.y1()) >= spans[1].left - 1.0);
        assert!(f64::from(clip.y2()) <= spans[1].right + 1.0);
        assert!(i32::from(clip.x2()) <= height);
        assert_ne!(&*surface.data().unwrap(), idle.as_slice());
        // The incremental frame matches a complete redraw of the same state,
        // so the fast cadence never leaves pixels behind.
        let mut reference = ImageSurface::create(Format::ARgb32, height, width).unwrap();
        layer.draw(
            &config,
            (width, height),
            &reference,
            (0.0, 0.0),
            true,
            None,
            evening,
        );
        assert_eq!(
            &*surface.data().unwrap(),
            reference.data().unwrap().to_vec().as_slice()
        );
    }

    #[test]
    fn the_pet_button_opens_the_pet_layer_and_home_returns_with_input_release() {
        let mut deck = pet_deck();
        assert!(!deck.pet_active());
        let mut events = Vec::new();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        // Hold a key on the home bar, then open the pet layer: the held key
        // is released and the old touch cannot act on the new layer.
        assert!(!deck.touch_down(0, 0, 100.0, 30.0, &mut emit, &mut |_| {}));
        assert!(deck.touch_down(1, 1, 1100.0, 30.0, &mut emit, &mut |_| {}));
        assert_eq!(deck.active, 1);
        assert!(deck.pet_active());
        assert!(deck.touches.is_empty());
        deck.touch_motion(0, 100.0, 30.0, 2008, 60, &mut emit);
        deck.touch_up(0, &mut emit);
        // The pet layer's Home button returns to the home bar.
        assert!(deck.touch_down(2, 0, 100.0, 30.0, &mut emit, &mut |_| {}));
        assert_eq!(deck.active, deck.home);
        assert!(!deck.pet_active());
        assert!(deck
            .layers
            .iter()
            .all(|layer| layer.buttons.iter().all(|(_, button)| !button.active)));
        assert_eq!(
            events,
            vec![
                (vec![Key::LeftCtrl, Key::F1], true),
                (vec![Key::LeftCtrl, Key::F1], false)
            ]
        );
    }

    #[test]
    fn the_pet_pauses_and_resumes_across_layer_switches() {
        // The daemon drives the pet with Bar.active taken from the deck; the
        // pet only exists while its own layer is displayed, and its state
        // survives round trips through other layers.
        let mut deck = pet_deck();
        let mut pet = pet::PetManager::new(true, pet::PetVariant::Cat, pet::PetEnergy::Calm);
        let bar = |active: bool| pet::Bar {
            width: 2008.0,
            height: 60.0,
            active,
            hour: 14,
        };
        let spans_of = |deck: &LayerDeck| {
            if deck.pet_active() {
                deck.current().layout_spans(2008, 0)
            } else {
                Vec::new()
            }
        };
        let mut now = Instant::now();
        let step = |deck: &mut LayerDeck, pet: &mut pet::PetManager, now: &mut Instant| {
            *now += Duration::from_millis(60);
            let active = deck.pet_active();
            let spans = spans_of(deck);
            pet.update(*now, &bar(active), &spans);
        };
        // Settle on the home bar: the pet stays hidden there.
        for _ in 0..10 {
            step(&mut deck, &mut pet, &mut now);
        }
        assert!(!pet.is_visible());
        // Open the pet layer: the pet appears and wanders its own layer.
        assert!(deck.switch_named("pet", &mut |_, _| {}));
        for _ in 0..20 {
            step(&mut deck, &mut pet, &mut now);
        }
        assert!(pet.is_visible());
        let mid_pos = pet.position();
        // Back to home: hidden again, position kept for the return.
        assert!(deck.switch_named("home", &mut |_, _| {}));
        for _ in 0..20 {
            step(&mut deck, &mut pet, &mut now);
        }
        assert!(!pet.is_visible());
        // Reopen: still alive where it was, wandering on.
        assert!(deck.switch_named("pet", &mut |_, _| {}));
        step(&mut deck, &mut pet, &mut now);
        assert!(pet.is_visible());
        assert!(
            (pet.position() - mid_pos).abs() < 200.0,
            "no teleport: {} vs {}",
            pet.position(),
            mid_pos
        );
    }

    #[test]
    fn navigation_hit_testing_and_redraw_display_the_selected_bar() {
        let mut deck = named_deck();
        let config = Config {
            show_button_outlines: true,
            enable_pixel_shift: false,
            font_face: cairo::FontFace::toy_create(
                "sans",
                cairo::FontSlant::Normal,
                cairo::FontWeight::Bold,
            )
            .unwrap(),
            adaptive_brightness: false,
            active_brightness: 128,
            double_press_switch_layers: 0,
            battery_cycle_enabled: true,
            battery_cycle_timeout_secs: 15,
            battery_warning_threshold: 25,
            battery_critical_threshold: 10,
            battery_warning_color: ButtonAccent::Orange,
            battery_critical_color: ButtonAccent::Red,
            pet_enabled: false,
            pet_variant: Default::default(),
            pet_energy: Default::default(),
            day_cycle: DayCycle::default(),
        };
        let (width, height) = (2008, 60);
        let mut surface = ImageSurface::create(Format::ARgb32, height, width).unwrap();
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            true,
            None,
            crate::daycycle::BLACK,
        );
        let home_image = surface.data().unwrap().to_vec();

        // The center navigation button on home opens the function bar.
        let button = deck
            .current()
            .hit(width as u16, height as u16, (width / 2) as f64, 30.0, None)
            .unwrap();
        assert!(deck.touch_down(0, button, 1004.0, 30.0, &mut |_, _| {}, &mut |_| {}));
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            true,
            None,
            crate::daycycle::BLACK,
        );
        assert_ne!(&*surface.data().unwrap(), home_image.as_slice());

        // The first button on the function bar returns to the same home image.
        let button = deck
            .current()
            .hit(width as u16, height as u16, 100.0, 30.0, None)
            .unwrap();
        assert!(deck.touch_down(1, button, 100.0, 30.0, &mut |_, _| {}, &mut |_| {}));
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            true,
            None,
            crate::daycycle::BLACK,
        );
        assert_eq!(&*surface.data().unwrap(), home_image.as_slice());
    }

    #[test]
    fn expressive_controls_redraw_without_residue_and_keep_the_touch_geometry() {
        for width in [2008, 2170] {
            for outlines in [true, false] {
                let mut config = outline_config();
                config.show_button_outlines = outlines;
                let mut layer = FunctionLayer::with_config(vec![
                    keys(
                        "A deliberately long label that must fit its control",
                        &[Key::F1],
                    ),
                    keys("Brightness", &[Key::BrightnessUp]),
                    nav("Home", "home"),
                ]);
                let mut surface = ImageSurface::create(Format::ARgb32, 60, width).unwrap();
                let mut reference = ImageSurface::create(Format::ARgb32, 60, width).unwrap();
                layer.draw(
                    &config,
                    (width, 60),
                    &surface,
                    (0.0, 0.0),
                    true,
                    None,
                    crate::daycycle::BLACK,
                );
                let idle = surface.data().unwrap().to_vec();
                let spans = layer.layout_spans(width, 0);
                for (i, span) in spans.iter().enumerate() {
                    let x = (span.left + span.right) / 2.0;
                    for y in [6.0, 30.0, 54.0] {
                        assert_eq!(layer.hit(width as u16, 60, x, y, None), Some(i));
                    }
                    assert_eq!(layer.hit(width as u16, 60, x, 5.0, None), None);
                    layer.buttons[i].1.set_active(true, &mut |_, _| {});
                    let clips = layer.draw(
                        &config,
                        (width, 60),
                        &surface,
                        (0.0, 0.0),
                        false,
                        None,
                        crate::daycycle::BLACK,
                    );
                    assert!(clips
                        .iter()
                        .all(|clip| clip.x2() <= 60 && i32::from(clip.y2()) <= width));
                    assert_ne!(&*surface.data().unwrap(), idle.as_slice());
                    layer.draw(
                        &config,
                        (width, 60),
                        &reference,
                        (0.0, 0.0),
                        true,
                        None,
                        crate::daycycle::BLACK,
                    );
                    let pressed = surface.data().unwrap().to_vec();
                    assert_eq!(pressed, reference.data().unwrap().to_vec());
                    layer.buttons[i].1.set_active(false, &mut |_, _| {});
                    layer.draw(
                        &config,
                        (width, 60),
                        &surface,
                        (0.0, 0.0),
                        false,
                        None,
                        crate::daycycle::BLACK,
                    );
                    assert_eq!(&*surface.data().unwrap(), idle.as_slice());
                }
                // Long text must not leak into the gaps or neighboring controls.
                let stride = surface.stride() as usize;
                let pixels = surface.data().unwrap();
                for span in spans.windows(2) {
                    let x = ((span[0].right + span[1].left) / 2.0) as usize;
                    for y in 0..60 {
                        let offset = x * stride + (59 - y) * 4;
                        assert_eq!(&pixels[offset..offset + 3], &[0, 0, 0]);
                    }
                }
            }
        }
    }

    #[test]
    fn the_day_cycle_background_paints_the_strip_behind_widgets_on_every_layer() {
        let day_cycle = DayCycle::default();
        let evening = day_cycle.background(19);
        assert_eq!(evening, (23.0 / 255.0, 13.0 / 255.0, 7.0 / 255.0));
        let config = outline_config();
        let mut layer = FunctionLayer::with_config(vec![
            keys(
                "A deliberately long label that must fit its control",
                &[Key::F1],
            ),
            keys("Brightness", &[Key::BrightnessUp]),
            nav("Home", "home"),
        ]);
        let width = 2008;
        let mut surface = ImageSurface::create(Format::ARgb32, 60, width).unwrap();
        let mut plain = ImageSurface::create(Format::ARgb32, 60, width).unwrap();
        layer.draw(
            &config,
            (width, 60),
            &surface,
            (0.0, 0.0),
            true,
            None,
            evening,
        );
        // The same widgets over the plain static background, to prove the
        // band only changes the strip behind them.
        layer.draw(
            &config,
            (width, 60),
            &plain,
            (0.0, 0.0),
            true,
            None,
            crate::daycycle::BLACK,
        );
        let spans = layer.layout_spans(width, 0);
        let stride = surface.stride() as usize;
        let pixels = surface.data().unwrap().to_vec();
        let static_pixels = plain.data().unwrap().to_vec();
        // The gap between widgets shows the evening band color at every
        // height, not the plain black strip.
        let gap = ((spans[0].right + spans[1].left) / 2.0) as usize;
        for y in [0, 30, 59] {
            let i = gap * stride + (59 - y) * 4;
            assert_eq!((pixels[i + 2], pixels[i + 1], pixels[i]), (23, 13, 7));
            assert_eq!(
                (static_pixels[i + 2], static_pixels[i + 1], static_pixels[i]),
                (0, 0, 0)
            );
        }
        // The widget's own pixels - fill, border, and label - render exactly
        // the same over either background. The scanned area stays inside the
        // inset rounded fill so no plain strip rows above and below it sneak
        // into the comparison.
        for x in ((spans[1].left + 12.0) as usize)..((spans[1].right - 12.0) as usize) {
            for y in (8..52).step_by(3) {
                let i = x * stride + (59 - y) * 4;
                assert_eq!(pixels[i..i + 4], static_pixels[i..i + 4]);
            }
        }
        // An incremental redraw over the band color leaves no plain-black
        // residue and matches a complete redraw with the same background.
        let mut reference = ImageSurface::create(Format::ARgb32, 60, width).unwrap();
        layer.buttons[1].1.set_active(true, &mut |_, _| {});
        layer.draw(
            &config,
            (width, 60),
            &surface,
            (0.0, 0.0),
            false,
            None,
            evening,
        );
        let pressed = surface.data().unwrap().to_vec();
        layer.draw(
            &config,
            (width, 60),
            &reference,
            (0.0, 0.0),
            true,
            None,
            evening,
        );
        assert_eq!(pressed, reference.data().unwrap().to_vec());

        // The band covers the pet layer's open ground too, behind the scene
        // the pet paints on top of it.
        let mut deck = pet_deck();
        let mut pet_surface = ImageSurface::create(Format::ARgb32, 60, width).unwrap();
        deck.switch_to(1, &mut |_, _| {});
        let pet_layer = deck.current_mut();
        pet_layer.draw(
            &config,
            (width, 60),
            &pet_surface,
            (0.0, 0.0),
            true,
            None,
            evening,
        );
        let spans = pet_layer.layout_spans(width, 0);
        let ground_x = ((spans[0].right + width as f64) / 2.0) as usize;
        let i = ground_x * stride + 30 * 4;
        let pixels = pet_surface.data().unwrap();
        assert_eq!((pixels[i + 2], pixels[i + 1], pixels[i]), (23, 13, 7));

        // The disabled cycle keeps today's plain static background.
        let disabled = DayCycle::disabled();
        for hour in [0, 9, 14, 19, 23] {
            assert_eq!(disabled.background(hour), crate::daycycle::BLACK);
        }
    }

    #[test]
    fn on_bar_navigation_releases_keys_ignores_old_touches_and_returns_home() {
        let mut deck = named_deck();
        let mut events = Vec::new();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        assert!(!deck.touch_down(0, 0, 330.0, 30.0, &mut emit, &mut |_| {}));
        assert!(deck.touch_down(1, 1, 1004.0, 30.0, &mut emit, &mut |_| {}));
        assert_eq!(deck.active, 1);
        assert!(deck.touches.is_empty());
        deck.touch_motion(0, 330.0, 30.0, 2008, 60, &mut emit);
        deck.touch_up(0, &mut emit);
        deck.touch_motion(1, 1004.0, 30.0, 2008, 60, &mut emit);
        deck.touch_up(1, &mut emit);
        assert!(deck.touch_down(2, 0, 330.0, 30.0, &mut emit, &mut |_| {}));
        assert_eq!(deck.active, deck.home);
        assert!(deck.touch_down(3, 2, 1679.0, 30.0, &mut emit, &mut |_| {}));
        assert_eq!(deck.active, 2);
        assert!(deck.touch_down(4, 0, 330.0, 30.0, &mut emit, &mut |_| {}));
        assert_eq!(deck.active, deck.home);
        assert!(deck
            .layers
            .iter()
            .all(|layer| layer.buttons.iter().all(|(_, button)| !button.active)));
        assert_eq!(
            events,
            vec![
                (vec![Key::LeftCtrl, Key::F1], true),
                (vec![Key::LeftCtrl, Key::F1], false)
            ]
        );
    }

    #[test]
    fn named_fn_is_temporary_and_on_bar_navigation_wins() {
        let mut deck = named_deck();
        let mut events = Vec::new();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.switch_named("media", &mut emit);
        deck.touch_down(0, 1, 1100.0, 30.0, &mut emit, &mut |_| {});
        assert!(deck.fn_event(true, false, &mut emit));
        assert_eq!(deck.active, 1);
        deck.touch_down(1, 1, 1100.0, 30.0, &mut emit, &mut |_| {});
        assert!(deck.fn_event(false, false, &mut emit));
        assert_eq!(deck.active, 2);
        assert!(deck.fn_event(true, false, &mut emit));
        assert!(deck.touch_down(2, 0, 100.0, 30.0, &mut emit, &mut |_| {})); // Home while Fn is held
        assert_eq!(deck.active, deck.home);
        assert!(!deck.fn_event(false, false, &mut emit));
        assert_eq!(deck.active, deck.home);
        assert_eq!(
            events,
            vec![
                (vec![Key::PlayPause], true),
                (vec![Key::PlayPause], false),
                (vec![Key::F1], true),
                (vec![Key::F1], false),
            ]
        );
    }

    #[test]
    fn legacy_fn_and_double_press_keep_both_default_orders() {
        for home in [0, 1] {
            let mut deck = LayerDeck::new(
                vec![
                    (
                        "primary".into(),
                        FunctionLayer::with_config(vec![keys("F1", &[Key::F1])]),
                    ),
                    (
                        "media".into(),
                        FunctionLayer::with_config(vec![keys("Volume", &[Key::VolumeUp])]),
                    ),
                ],
                home,
                Some(1 - home),
                None,
                true,
            );
            let mut events = Vec::new();
            let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
            deck.touch_down(0, 0, 100.0, 30.0, &mut emit, &mut |_| {});
            assert!(deck.fn_event(true, false, &mut emit));
            assert_eq!(deck.active, 1 - home);
            assert!(deck.fn_event(false, false, &mut emit));
            assert_eq!(deck.active, home);
            // Second quick Fn press changes the default, without swapping button storage.
            assert!(!deck.fn_event(true, true, &mut emit));
            assert_eq!(deck.active, home);
            assert!(deck.fn_event(false, false, &mut emit));
            assert_eq!(deck.active, 1 - home);
            assert_eq!(deck.home, 1 - home);
            assert_eq!(events.len(), 2);
            assert!(events[0].1);
            assert!(!events[1].1);
            assert_eq!(events[0].0, events[1].0);
        }
    }

    #[test]
    fn reload_to_a_smaller_deck_releases_input_and_resets_to_new_home() {
        let mut deck = named_deck();
        let mut events = Vec::new();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.fn_event(true, false, &mut emit);
        deck.touch_down(0, 1, 1004.0, 30.0, &mut emit, &mut |_| {});
        let replacement = LayerDeck::new(
            vec![(
                "new-home".into(),
                FunctionLayer::with_config(vec![keys("Volume", &[Key::VolumeUp])]),
            )],
            0,
            None,
            None,
            false,
        );
        deck.replace(replacement, &mut emit);
        assert_eq!(deck.active, deck.home);
        assert_eq!(deck.layers.len(), 1);
        assert!(deck.touches.is_empty());
        deck.touch_motion(0, 1004.0, 30.0, 2008, 60, &mut emit);
        deck.touch_up(0, &mut emit);
        assert!(!deck.fn_event(false, false, &mut emit));
        deck.touch_down(1, 0, 100.0, 30.0, &mut emit, &mut |_| {});
        deck.touch_up(1, &mut emit);
        assert_eq!(
            events,
            vec![
                (vec![Key::F1], true),
                (vec![Key::F1], false),
                (vec![Key::VolumeUp], true),
                (vec![Key::VolumeUp], false),
            ]
        );
    }

    #[test]
    fn motion_releases_and_represses_keys_and_up_ends_capture() {
        let mut deck = named_deck();
        let mut events = Vec::new();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.touch_down(0, 0, 330.0, 30.0, &mut emit, &mut |_| {});
        deck.touch_motion(0, 2000.0, 30.0, 2008, 60, &mut emit);
        deck.touch_motion(0, 330.0, 30.0, 2008, 60, &mut emit);
        deck.touch_up(0, &mut emit);
        deck.touch_motion(0, 330.0, 30.0, 2008, 60, &mut emit);
        deck.touch_up(0, &mut emit);
        assert!(deck.touches.is_empty());
        assert_eq!(
            events.iter().map(|(_, active)| *active).collect::<Vec<_>>(),
            vec![true, false, true, false]
        );
    }

    #[test]
    fn multiple_fingers_on_one_button_release_after_the_last_finger() {
        let mut deck = named_deck();
        let mut events = Vec::new();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.touch_down(0, 0, 330.0, 30.0, &mut emit, &mut |_| {});
        deck.touch_down(1, 0, 330.0, 30.0, &mut emit, &mut |_| {});
        deck.touch_motion(0, 2000.0, 30.0, 2008, 60, &mut emit);
        deck.touch_up(1, &mut emit);
        assert!(!deck.current().buttons[0].1.active);
        deck.touch_motion(0, 330.0, 30.0, 2008, 60, &mut emit);
        deck.release_all(&mut emit); // cancel, device removal, or display sleeping
        deck.touch_motion(0, 330.0, 30.0, 2008, 60, &mut emit);
        assert!(deck.touches.is_empty());
        assert_eq!(
            events.iter().map(|(_, active)| *active).collect::<Vec<_>>(),
            vec![true, false, true, false]
        );
    }

    #[test]
    fn invalid_target_does_not_change_state_or_release_held_keys() {
        let mut deck = named_deck();
        let mut events = Vec::new();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        deck.touch_down(0, 0, 330.0, 30.0, &mut emit, &mut |_| {});
        assert!(!deck.switch_named("missing", &mut emit));
        assert_eq!(deck.active, deck.home);
        assert_eq!(deck.touches.len(), 1);
        deck.touch_up(0, &mut emit);
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn fn_without_a_target_and_named_double_press_do_not_change_the_home() {
        let mut deck = named_deck();
        deck.fn_layer = None;
        assert!(!deck.fn_event(true, true, &mut |_, _| {}));
        assert!(!deck.fn_event(false, false, &mut |_, _| {}));
        assert_eq!(deck.active, deck.home);
        assert_eq!(deck.home, 0);
    }

    fn slider_button(label: &str) -> ButtonConfig {
        ButtonConfig {
            text: Some(label.into()),
            action: ButtonAction::VolumeSlider,
            ..ButtonConfig::default()
        }
    }

    fn slider_deck() -> LayerDeck {
        LayerDeck::new(
            vec![
                (
                    "home".into(),
                    FunctionLayer::with_config(vec![
                        slider_button("V-"),
                        slider_button("Volume"),
                        slider_button("V+"),
                        nav("Functions", "function"),
                    ]),
                ),
                (
                    "function".into(),
                    FunctionLayer::with_config(vec![
                        nav("Home", "home"),
                        keys("Volume", &[Key::VolumeUp]),
                    ]),
                ),
            ],
            0,
            None,
            None,
            false,
        )
    }

    fn apply_mixer_requests(deck: &mut LayerDeck, actual: &mut u8) {
        for (revision, kind, request) in std::mem::take(&mut deck.slider.requests) {
            assert_eq!(kind, Kind::Volume);
            match request {
                LevelRequest::Get => {}
                LevelRequest::Set(percent) => *actual = percent,
            }
            deck.slider_reply(
                revision,
                Kind::Volume,
                Ok(LevelState {
                    percent: *actual,
                    muted: false,
                }),
            );
        }
    }

    #[test]
    fn equal_stretch_slider_pair_draws_equal_width_buttons() {
        // The layout engine has one width path: a button spans exactly its
        // Stretch in unit widths. Whatever equal value the shipped pair is
        // given, the volume and brightness sliders must come out
        // pixel-identical in width.
        for stretch in [1, 2, 3, 5] {
            let slider = |label: &str, action: ButtonAction| ButtonConfig {
                text: Some(label.into()),
                stretch: Some(stretch),
                action,
                ..ButtonConfig::default()
            };
            let layer = FunctionLayer::with_config(vec![
                slider("Volume", ButtonAction::VolumeSlider),
                slider("Brightness", ButtonAction::BrightnessSlider),
                keys("Play", &[Key::PlayPause]),
                nav("Functions", "function"),
            ]);
            let spans = layer.layout_spans(2008, 0);
            let width = |span: &PetSpan| span.right - span.left;
            // Equal Stretch runs the identical width formula for both
            // buttons; recover each width from its own left edge, which can
            // drift one float ULP, far below a drawn pixel.
            assert!(
                (width(&spans[0]) - width(&spans[1])).abs() < 1e-6,
                "equal Stretch must draw equal widths at stretch {stretch}"
            );
            assert!(width(&spans[0]) > 0.0);
        }
    }

    fn shared_slider_deck() -> LayerDeck {
        let mut volume = slider_button("Volume");
        volume.stretch = Some(3);
        let brightness = ButtonConfig {
            text: Some("Brightness".into()),
            stretch: Some(2),
            action: ButtonAction::BrightnessSlider,
            ..ButtonConfig::default()
        };
        LayerDeck::new(
            vec![
                (
                    "home".into(),
                    FunctionLayer::with_config(vec![
                        volume,
                        brightness,
                        keys("Play", &[Key::PlayPause]),
                        nav("Functions", "function"),
                    ]),
                ),
                (
                    "function".into(),
                    FunctionLayer::with_config(vec![nav("Home", "home")]),
                ),
            ],
            0,
            Some(1),
            None,
            false,
        )
    }

    fn apply_level_requests(deck: &mut LayerDeck, volume: &mut u8, brightness: &mut u8) {
        for (revision, kind, request) in std::mem::take(&mut deck.slider.requests) {
            let actual = match kind {
                Kind::Volume => &mut *volume,
                Kind::Brightness => &mut *brightness,
            };
            if let LevelRequest::Set(value) = request {
                *actual = value;
            }
            deck.slider_reply(
                revision,
                kind,
                Ok(LevelState {
                    percent: *actual,
                    muted: false,
                }),
            );
        }
    }

    #[test]
    fn either_selection_uses_the_combined_region_without_jumping_or_cross_writes() {
        for width in [2008, 2170] {
            for shift in [-11.0, 0.0, 11.0] {
                let mut deck = shared_slider_deck();
                deck.slider.pixel_shift_width = PIXEL_SHIFT_WIDTH_PX;
                deck.slider.pixel_shift_x = shift;
                let spans = deck.current().layout_spans(width, PIXEL_SHIFT_WIDTH_PX);
                let expected_region = (spans[0].left + shift, spans[1].right + shift);
                let (mut volume, mut brightness) = (100, 93);
                let mut emit = |_: &[Key], _: bool| panic!("level control emitted keys");
                for (button, kind, other) in [
                    (0, Kind::Volume, 93),
                    (1, Kind::Brightness, 130),
                    (0, Kind::Volume, 100),
                ] {
                    let x = (spans[button].left + spans[button].right) / 2.0 + shift;
                    deck.touch_down_at(0, x, 30.0, width as u16, 60, &mut emit, &mut |_| {});
                    assert_eq!(deck.slider.kind, kind);
                    assert_eq!(deck.slider.region, Some((0, 1)));
                    assert_eq!(deck.slider_region(width as u16).unwrap(), expected_region);
                    assert!(deck.slider_render().is_none());
                    assert_eq!(deck.slider.requests.len(), 1);
                    assert_eq!(deck.slider.requests[0].1, kind);
                    assert_eq!(deck.slider.requests[0].2, LevelRequest::Get);
                    apply_level_requests(&mut deck, &mut volume, &mut brightness);
                    assert_eq!(
                        deck.slider.level,
                        if kind == Kind::Volume {
                            volume
                        } else {
                            brightness
                        }
                    );
                    // Small motion while opening never becomes an absolute write.
                    deck.touch_motion(0, x + 2.0, 30.0, width as u16, 60, &mut emit);
                    assert!(deck.slider.requests.is_empty());
                    let (left, right) = deck.slider_track(width as u16).unwrap();
                    deck.touch_motion(0, left, 30.0, width as u16, 60, &mut emit);
                    apply_level_requests(&mut deck, &mut volume, &mut brightness);
                    assert_eq!(deck.slider.level, kind.range().0);
                    assert_eq!(
                        if kind == Kind::Volume {
                            brightness
                        } else {
                            volume
                        },
                        other
                    );
                    for target in [kind.range().0, 25, 65, kind.range().1] {
                        let (min, max) = kind.range();
                        let x = left + (right - left) * (target - min) as f64 / (max - min) as f64;
                        let previous = deck.slider.level;
                        assert!(deck.slider_drag_motion(0, x, 30.0, width as u16, &mut emit));
                        assert_eq!(deck.slider_render().unwrap().level, previous);
                        apply_level_requests(&mut deck, &mut volume, &mut brightness);
                        assert_eq!(deck.slider.level, target);
                        assert_eq!(
                            if kind == Kind::Volume {
                                brightness
                            } else {
                                volume
                            },
                            other
                        );
                    }
                    deck.touch_up(0, &mut emit);
                    let deadline = deck.slider.deadline.unwrap();
                    assert!(deck.slider_refresh(deadline));
                }
            }
        }
    }

    #[test]
    fn changing_selection_discards_old_replies_and_old_held_finger() {
        let mut deck = shared_slider_deck();
        let spans = deck.current().layout_spans(2008, 0);
        let center = |index: usize| (spans[index].left + spans[index].right) / 2.0;
        // Both buttons are still visible while the first read is pending.
        deck.touch_down_at(0, center(0), 30.0, 2008, 60, &mut |_, _| {}, &mut |_| {});
        let old = deck.slider.revision;
        deck.touch_down_at(1, center(1), 30.0, 2008, 60, &mut |_, _| {}, &mut |_| {});
        let selected = deck.slider.revision;
        assert_eq!(deck.slider.kind, Kind::Brightness);
        deck.slider_reply(
            old,
            Kind::Volume,
            Ok(LevelState {
                percent: 130,
                muted: true,
            }),
        );
        deck.slider_reply(
            old,
            Kind::Volume,
            Err(std::io::Error::other("old volume failed")),
        );
        assert!(deck.slider.active);
        assert!(deck.slider_render().is_none());
        deck.touch_motion(0, 0.0, 30.0, 2008, 60, &mut |_, _| {});
        assert!(deck
            .slider
            .requests
            .iter()
            .all(|(_, _, r)| *r == LevelRequest::Get));
        deck.slider_reply(
            selected,
            Kind::Brightness,
            Ok(LevelState {
                percent: 47,
                muted: false,
            }),
        );
        assert_eq!(deck.slider_render().unwrap().level, 47);
        deck.touch_motion(1, 0.0, 30.0, 2008, 60, &mut |_, _| {});
        let pending = deck.slider.revision;
        assert_eq!(deck.slider_render().unwrap().level, 47);
        deck.slider_reply(
            pending,
            Kind::Brightness,
            Err(std::io::Error::other("permission denied")),
        );
        assert!(!deck.slider.active);
        assert_eq!(deck.slider.level, 47);
        deck.slider_reply(
            pending,
            Kind::Brightness,
            Ok(LevelState {
                percent: 1,
                muted: false,
            }),
        );
        assert!(deck.slider_render().is_none());
        deck.release_all(&mut |_, _| {});
        deck.touch_down_at(2, center(0), 30.0, 2008, 60, &mut |_, _| {}, &mut |_| {});
        deck.slider_reply(
            selected,
            Kind::Brightness,
            Ok(LevelState {
                percent: 47,
                muted: false,
            }),
        );
        assert!(deck.slider_render().is_none());
        assert_eq!(deck.slider.kind, Kind::Volume);
    }

    #[test]
    fn open_slider_consumes_the_gap_and_second_finger_but_leaves_other_controls_accessible() {
        for kind in [Kind::Volume, Kind::Brightness] {
            let mut deck = shared_slider_deck();
            let spans = deck.current().layout_spans(2008, 0);
            let button = usize::from(kind == Kind::Brightness);
            let x = (spans[button].left + spans[button].right) / 2.0;
            deck.touch_down_at(0, x, 30.0, 2008, 60, &mut |_, _| {}, &mut |_| {});
            let (mut volume, mut brightness) = (65, 93);
            apply_level_requests(&mut deck, &mut volume, &mut brightness);
            deck.touch_up(0, &mut |_, _| {});
            let gap = (spans[0].right + spans[1].left) / 2.0;
            assert!(deck.current().hit(2008, 60, gap, 30.0, None).is_none());
            deck.touch_down_at(1, gap, 30.0, 2008, 60, &mut |_, _| {}, &mut |_| {});
            let queued = deck.slider.requests.len();
            deck.touch_down_at(2, x, 30.0, 2008, 60, &mut |_, _| {}, &mut |_| {});
            assert_eq!(deck.slider.requests.len(), queued);
            assert_eq!(deck.slider.kind, kind);
            assert_eq!(deck.slider.drag.as_ref().unwrap().slot, 1);
            let mut events = Vec::new();
            let play = (spans[2].left + spans[2].right) / 2.0;
            deck.touch_down_at(
                3,
                play,
                30.0,
                2008,
                60,
                &mut |keys, state| events.push((keys.to_vec(), state)),
                &mut |_| {},
            );
            deck.touch_up(3, &mut |keys, state| events.push((keys.to_vec(), state)));
            assert_eq!(
                events,
                vec![(vec![Key::PlayPause], true), (vec![Key::PlayPause], false)]
            );
            assert!(deck.slider.active);
            assert!(deck.fn_event(true, false, &mut |_, _| {}));
            assert!(!deck.slider.active);
        }
    }

    #[test]
    fn drag_to_left_silences_an_output_changed_outside_the_touch_bar() {
        // These initial states exposed the old midpoint estimate bug. The
        // pre-fix run is retained in the task proof; the 100% case also failed
        // when the helper ceiling was100%, independently of the130% setting.
        for initial in [40, 100, 130] {
            let mut deck = slider_deck();
            let mut actual = initial;
            let mut emit = |_: &[Key], _: bool| panic!("slider emitted relative keys");
            deck.touch_down_at(0, 1260.0, 30.0, 2008, 60, &mut emit, &mut |_| {});
            apply_mixer_requests(&mut deck, &mut actual);
            deck.touch_motion(0, 0.0, 30.0, 2008, 60, &mut emit);
            apply_mixer_requests(&mut deck, &mut actual);
            deck.touch_up(0, &mut emit);
            assert_eq!(deck.slider.level, 0);
            assert_eq!(actual, 0, "left endpoint left audible volume");
            // A mixer change outside the bar must not defeat a repeated zero.
            actual = 100;
            deck.touch_down_at(1, 10.0, 30.0, 2008, 60, &mut emit, &mut |_| {});
            apply_mixer_requests(&mut deck, &mut actual);
            deck.touch_up(1, &mut emit);
            assert_eq!(actual, 0);
        }
    }

    #[test]
    fn single_slider_control_opens_without_stepping_and_drags_the_full_range() {
        for (width, layer) in [(2008, 0), (2170, 0), (2008, 1)] {
            let slider = || ButtonConfig {
                text: Some("Volume".into()),
                stretch: Some(3),
                action: ButtonAction::VolumeSlider,
                ..ButtonConfig::default()
            };
            let mut deck = LayerDeck::new(
                vec![
                    (
                        "home".into(),
                        FunctionLayer::with_config(vec![slider(), nav("Media", "media")]),
                    ),
                    (
                        "media".into(),
                        FunctionLayer::with_config(vec![nav("Home", "home"), slider()]),
                    ),
                ],
                0,
                None,
                None,
                false,
            );
            deck.active = layer;
            let index = usize::from(layer == 1);
            let spans = deck.current().layout_spans(width, 0);
            let x = (spans[index].left + spans[index].right) / 2.0;
            let mut actual = 100;
            let mut emit = |_: &[Key], _: bool| panic!("slider emitted keys");
            deck.touch_down_at(0, x, 30.0, width as u16, 60, &mut emit, &mut |_| {});
            assert_eq!(deck.slider.requests.last().unwrap().2, LevelRequest::Get);
            apply_mixer_requests(&mut deck, &mut actual);
            assert_eq!(actual, 100);
            assert_eq!(deck.slider.level, 100);
            let (left, right) = deck.slider_track(width as u16).unwrap();
            deck.touch_motion(0, left, 30.0, width as u16, 60, &mut emit);
            apply_mixer_requests(&mut deck, &mut actual);
            assert_eq!(actual, 0);
            for expected in [1, 25, 65, 100, 129, 130] {
                let x = left + (right - left) * expected as f64 / 130.0;
                assert!(deck.slider_drag_motion(0, x, 30.0, width as u16, &mut emit));
                apply_mixer_requests(&mut deck, &mut actual);
                assert_eq!(actual, expected);
                assert_eq!(deck.slider.level, expected);
            }
            assert!(!deck.slider_drag_start(1, x, 30.0, width as u16, 60, &mut emit));
            deck.touch_up(0, &mut emit);
            assert!(deck.slider.drag.is_none());
        }
    }

    #[test]
    fn drawn_slider_owns_covered_buttons_and_the_speaker_selects_zero() {
        let mut deck = slider_deck();
        let mut actual = 100;
        let mut emit = |_: &[Key], _: bool| panic!("slider emitted keys");
        deck.touch_down_at(0, 1260.0, 30.0, 2008, 60, &mut emit, &mut |_| {});
        deck.touch_up(0, &mut emit);
        apply_mixer_requests(&mut deck, &mut actual);
        assert_eq!(deck.current().hit(2008, 60, 751.0, 30.0, None), Some(1));
        deck.touch_down_at(1, 751.0, 30.0, 2008, 60, &mut emit, &mut |_| {});
        apply_mixer_requests(&mut deck, &mut actual);
        assert_eq!(actual, Kind::Volume.level_at(751.0, 30.0, 1500.0));
        deck.touch_up(1, &mut emit);
        deck.touch_down_at(2, 10.0, 30.0, 2008, 60, &mut emit, &mut |_| {});
        apply_mixer_requests(&mut deck, &mut actual);
        deck.touch_up(2, &mut emit);
        assert_eq!(actual, 0);
        assert!(!deck.slider.muted);
    }

    #[test]
    fn old_readbacks_and_failed_writes_cannot_display_a_false_success() {
        let mut deck = slider_deck();
        deck.touch_down_at(0, 1260.0, 30.0, 2008, 60, &mut |_, _| {}, &mut |_| {});
        assert!(deck.slider_render().is_none()); // no guessed zero while reading
        let old = deck.slider.revision;
        deck.slider_reply(
            old,
            Kind::Volume,
            Ok(LevelState {
                percent: 100,
                muted: false,
            }),
        );
        deck.touch_motion(0, 0.0, 30.0, 2008, 60, &mut |_, _| {});
        deck.slider_reply(
            old,
            Kind::Volume,
            Ok(LevelState {
                percent: 130,
                muted: false,
            }),
        );
        assert_eq!(deck.slider_render().unwrap().level, 100); // stale reply ignored
        let current = deck.slider.revision;
        deck.slider_reply(
            current,
            Kind::Volume,
            Err(std::io::Error::other("receiver unavailable")),
        );
        assert!(!deck.slider.active);
        assert!(deck.slider_render().is_none());
        assert_eq!(deck.slider.level, 100); // failed zero was never displayed
        deck.slider_reply(
            current,
            Kind::Volume,
            Ok(LevelState {
                percent: 0,
                muted: false,
            }),
        );
        assert!(!deck.slider.active);
    }

    #[test]
    fn confirmed_drag_progress_renders_while_a_newer_target_is_pending() {
        let mut deck = slider_deck();
        deck.touch_down_at(0, 1260.0, 30.0, 2008, 60, &mut |_, _| {}, &mut |_| {});
        let opening = deck.slider.revision;
        deck.slider_set_level(10, &mut |_, _| {});
        let first = deck.slider.revision;
        deck.slider_set_level(20, &mut |_, _| {});
        let latest = deck.slider.revision;
        deck.slider_reply(
            first,
            Kind::Volume,
            Ok(LevelState {
                percent: 10,
                muted: false,
            }),
        );
        assert_eq!(deck.slider_render().unwrap().level, 10);
        deck.slider_reply(
            opening,
            Kind::Volume,
            Ok(LevelState {
                percent: 100,
                muted: false,
            }),
        );
        assert_eq!(deck.slider_render().unwrap().level, 10);
        deck.slider_reply(
            latest,
            Kind::Volume,
            Ok(LevelState {
                percent: 20,
                muted: false,
            }),
        );
        deck.slider_reply(
            first,
            Kind::Volume,
            Ok(LevelState {
                percent: 10,
                muted: false,
            }),
        );
        assert_eq!(deck.slider_render().unwrap().level, 20);
        deck.slider_collapse();
        deck.touch_down_at(1, 1260.0, 30.0, 2008, 60, &mut |_, _| {}, &mut |_| {});
        deck.slider_reply(
            latest,
            Kind::Volume,
            Ok(LevelState {
                percent: 20,
                muted: false,
            }),
        );
        assert!(deck.slider_render().is_none());
    }

    #[test]
    fn repeated_open_before_readback_does_not_change_volume() {
        let mut deck = slider_deck();
        for slot in 0..2 {
            deck.touch_down_at(
                slot,
                1260.0,
                30.0,
                2008,
                60,
                &mut |_, _| panic!("volume button emitted keys"),
                &mut |_| {},
            );
            deck.touch_up(slot, &mut |_, _| {});
        }
        assert!(deck
            .slider
            .requests
            .iter()
            .all(|(_, kind, request)| *kind == Kind::Volume && *request == LevelRequest::Get));
        let mut actual = 100;
        apply_mixer_requests(&mut deck, &mut actual);
        assert_eq!(actual, 100);
    }

    #[test]
    fn pixel_shifted_drawn_endpoints_still_set_zero_and_130() {
        for width in [2008, 2170] {
            for shift in [-11.0, 0.0, 11.0] {
                let mut deck = slider_deck();
                deck.slider.pixel_shift_width = PIXEL_SHIFT_WIDTH_PX;
                deck.slider.pixel_shift_x = shift;
                let spans = deck.current().layout_spans(width, PIXEL_SHIFT_WIDTH_PX);
                // layout_spans is the actual widget geometry consumer. The
                // drawn speaker zone and track insets remain30px /2px.
                let left = spans[0].left + shift + 30.0;
                let right = spans[2].right + shift - 2.0;
                deck.touch_down_at(
                    0,
                    1260.0,
                    30.0,
                    width as u16,
                    60,
                    &mut |_, _| {},
                    &mut |_| {},
                );
                let mut actual = 100;
                apply_mixer_requests(&mut deck, &mut actual);
                deck.touch_motion(0, left, 30.0, width as u16, 60, &mut |_, _| {});
                apply_mixer_requests(&mut deck, &mut actual);
                assert_eq!(actual, 0, "visible left endpoint at shift{shift}");
                deck.slider_drag_motion(0, right, 30.0, width as u16, &mut |_, _| {});
                apply_mixer_requests(&mut deck, &mut actual);
                assert_eq!(actual, 130, "visible right endpoint at shift{shift}");
            }
        }
    }

    #[test]
    fn slider_level_math_rounds_and_clamps() {
        assert_eq!(Kind::Volume.level_at(-50.0, 0.0, 1502.0), 0);
        assert_eq!(Kind::Volume.level_at(0.0, 0.0, 1502.0), 0);
        assert_eq!(Kind::Volume.level_at(751.0, 0.0, 1502.0), 65);
        assert_eq!(Kind::Volume.level_at(1502.0, 0.0, 1502.0), 130);
        assert_eq!(Kind::Volume.level_at(5000.0, 0.0, 1502.0), 130);
    }

    #[test]
    fn the_slider_collapses_after_a_lull_and_interaction_resets_the_timer() {
        let mut deck = slider_deck();
        deck.touch_down(0, 2, 1260.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(0, &mut |_, _| {});
        let deadline = deck.slider.deadline.unwrap();

        // Just short of the deadline it is still up and still counting down.
        assert!(!deck.slider_refresh(deadline - Duration::from_millis(1)));
        assert!(deck.slider.active);
        assert!(deck
            .slider_timeout(deadline - Duration::from_secs(1))
            .is_some());

        // A fresh press restarts the clock.
        deck.touch_down(1, 2, 1260.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(1, &mut |_, _| {});
        assert!(deck.slider.deadline.unwrap() > deadline);

        // Past the deadline with no interaction it collapses and stops timing.
        let deadline = deck.slider.deadline.unwrap();
        assert!(deck.slider_refresh(deadline + Duration::from_millis(1)));
        assert!(!deck.slider.active);
        assert!(deck.slider_render().is_none());
        assert_eq!(deck.slider_timeout(deadline), None);
        // The collapse flags the cluster so the bar repaints it.
        assert!(deck.layers[deck.home].buttons[..3]
            .iter()
            .all(|(_, button)| button.changed));
    }

    #[test]
    fn a_held_slider_press_keeps_the_slider_up_until_release() {
        let mut deck = slider_deck();
        // The summoning press stays held: the slider must not collapse
        // under the finger even past the deadline.
        deck.touch_down(0, 2, 1260.0, 30.0, &mut |_, _| {}, &mut |_| {});
        let deadline = deck.slider.deadline.unwrap();
        assert!(!deck.slider_refresh(deadline + Duration::from_secs(10)));
        assert!(deck.slider.active);
        // The wait hint reports no pending collapse, not a passed one.
        assert_eq!(
            deck.slider_timeout(deadline + Duration::from_secs(10)),
            None
        );
        // A still-held second-finger press on the cluster holds it too.
        deck.touch_down(1, 0, 200.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(0, &mut |_, _| {});
        assert!(!deck.slider_refresh(deadline + Duration::from_secs(20)));
        assert!(deck.slider.active);
        // Once every finger lifts, the release restarts the clock and the
        // quiet period collapses it.
        deck.touch_up(1, &mut |_, _| {});
        let deadline = deck.slider.deadline.unwrap();
        assert!(deck.slider_refresh(deadline + Duration::from_millis(1)));
        assert!(!deck.slider.active);
    }

    fn outline_config() -> Config {
        Config {
            show_button_outlines: true,
            enable_pixel_shift: false,
            font_face: cairo::FontFace::toy_create(
                "sans",
                cairo::FontSlant::Normal,
                cairo::FontWeight::Bold,
            )
            .unwrap(),
            adaptive_brightness: false,
            active_brightness: 128,
            double_press_switch_layers: 0,
            battery_cycle_enabled: true,
            battery_cycle_timeout_secs: 15,
            battery_warning_threshold: 25,
            battery_critical_threshold: 10,
            battery_warning_color: ButtonAccent::Orange,
            battery_critical_color: ButtonAccent::Red,
            pet_enabled: false,
            pet_variant: Default::default(),
            pet_energy: Default::default(),
            day_cycle: DayCycle::default(),
        }
    }

    #[test]
    fn the_slider_covers_the_cluster_renders_its_level_and_restores_the_bar() {
        let mut deck = slider_deck();
        let config = outline_config();
        // The slider overlay and its collapse run over a time-of-day band,
        // proving the cluster erases repaint with the band color, not black.
        let evening = DayCycle::default().background(19);
        let (width, height) = (2008, 60);
        let mut surface = ImageSurface::create(Format::ARgb32, height, width).unwrap();
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            true,
            None,
            evening,
        );
        let home_image = surface.data().unwrap().to_vec();

        // Pressing a volume control summons the slider over the whole cluster.
        deck.touch_down(0, 2, 1260.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(0, &mut |_, _| {});
        apply_mixer_requests(&mut deck, &mut 65);
        let volume = deck.slider_render();
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            false,
            volume,
            evening,
        );
        let low_level = surface.data().unwrap().to_vec();
        assert_ne!(&*low_level, home_image.as_slice());

        // A higher level fills more of the track.
        deck.slider.level = level_control::MAX_VOLUME_PERCENT;
        deck.mark_slider_changed();
        apply_mixer_requests(&mut deck, &mut 65);
        let volume = deck.slider_render();
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            false,
            volume,
            evening,
        );
        assert_ne!(&*surface.data().unwrap(), low_level.as_slice());

        // The muted state renders differently at the same level.
        let full_level = surface.data().unwrap().to_vec();
        deck.slider.muted = true;
        deck.mark_slider_changed();
        apply_mixer_requests(&mut deck, &mut 65);
        let volume = deck.slider_render();
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            false,
            volume,
            evening,
        );
        assert_ne!(&*surface.data().unwrap(), full_level.as_slice());

        // Collapsing restores exactly the original home bar.
        deck.slider_collapse();
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            false,
            None,
            evening,
        );
        assert_eq!(&*surface.data().unwrap(), home_image.as_slice());
    }

    #[test]
    fn slider_clip_rects_stay_inside_the_framebuffer_when_the_cluster_ends_the_bar() {
        let mut deck = LayerDeck::new(
            vec![(
                "home".into(),
                FunctionLayer::with_config(vec![
                    slider_button("V-"),
                    slider_button("Mute"),
                    slider_button("V+"),
                ]),
            )],
            0,
            None,
            None,
            false,
        );
        let config = outline_config();
        let (width, height) = (2008, 60);
        let surface = ImageSurface::create(Format::ARgb32, height, width).unwrap();
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            true,
            None,
            crate::daycycle::BLACK,
        );

        // The cluster spans the whole bar, so the slider's edges touch the
        // framebuffer borders; its dirty clips must still fit inside them.
        deck.touch_down(0, 2, 1800.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(0, &mut |_, _| {});
        apply_mixer_requests(&mut deck, &mut 65);
        let volume = deck.slider_render().unwrap();
        assert_eq!((volume.first, volume.last), (0, 2));
        let clips = deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            false,
            Some(volume),
            crate::daycycle::BLACK,
        );
        assert!(!clips.is_empty());
        for clip in clips {
            assert!(
                i32::from(clip.x2()) <= height,
                "clip {clip:?} exceeds the bar height"
            );
            assert!(
                i32::from(clip.y2()) <= width,
                "clip {clip:?} exceeds the bar width"
            );
        }

        // The recovery clips after collapse obey the same bound.
        deck.slider_collapse();
        let clips = deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            false,
            None,
            crate::daycycle::BLACK,
        );
        for clip in clips {
            assert!(
                i32::from(clip.x2()) <= height,
                "clip {clip:?} exceeds the bar height"
            );
            assert!(
                i32::from(clip.y2()) <= width,
                "clip {clip:?} exceeds the bar width"
            );
        }
    }

    #[test]
    fn moving_the_slider_to_another_cluster_restores_the_previous_span() {
        let mut deck = LayerDeck::new(
            vec![(
                "home".into(),
                FunctionLayer::with_config(vec![
                    slider_button("V-"),
                    slider_button("Mute"),
                    slider_button("V+"),
                    keys("Esc", &[Key::Esc]),
                    slider_button("W-"),
                    slider_button("W+"),
                ]),
            )],
            0,
            None,
            None,
            false,
        );
        let config = outline_config();
        let (width, height) = (2008, 60);
        let mut surface = ImageSurface::create(Format::ARgb32, height, width).unwrap();
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            true,
            None,
            crate::daycycle::BLACK,
        );
        let home_image = surface.data().unwrap().to_vec();

        // Summon on the first cluster: its span is drawn over.
        deck.touch_down(0, 0, 100.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(0, &mut |_, _| {});
        assert_eq!(deck.slider.region, Some((0, 2)));
        apply_mixer_requests(&mut deck, &mut 65);
        let volume = deck.slider_render();
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            false,
            volume,
            crate::daycycle::BLACK,
        );

        // Pressing the second, separated cluster moves the slider there.
        deck.touch_down(1, 4, 1500.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(1, &mut |_, _| {});
        assert_eq!(deck.slider.region, Some((4, 5)));
        apply_mixer_requests(&mut deck, &mut 65);
        let volume = deck.slider_render();
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            false,
            volume,
            crate::daycycle::BLACK,
        );

        // Collapsing restores the whole bar, first cluster included.
        deck.slider_collapse();
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            false,
            None,
            crate::daycycle::BLACK,
        );
        assert_eq!(&*surface.data().unwrap(), home_image.as_slice());
    }

    #[test]
    fn layer_changes_end_the_slider_and_legacy_keys_still_emit_keys() {
        let mut deck = slider_deck();
        deck.touch_down(0, 2, 1260.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(0, &mut |_, _| {});
        assert!(deck.slider.active);

        // Navigating away collapses it...
        assert!(deck.touch_down(1, 3, 1750.0, 30.0, &mut |_, _| {}, &mut |_| {}));
        assert_eq!(deck.active, 1);
        assert!(!deck.slider.active);

        // ...and volume buttons on other layers never summon it.
        deck.touch_down(2, 1, 1500.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(2, &mut |_, _| {});
        assert!(!deck.slider.active);
        assert!(!deck.slider_drag_start(3, 700.0, 30.0, 2008, 60, &mut |_, _| {}));

        // Back home it summons again; a reload replaces it with a fresh deck.
        assert!(deck.touch_down(4, 0, 100.0, 30.0, &mut |_, _| {}, &mut |_| {}));
        assert_eq!(deck.active, deck.home);
        deck.replace(slider_deck(), &mut |_, _| {});
        assert!(!deck.slider.active);

        // Summoning once more, input release (cancel, display sleep, device
        // removal) collapses it and drops any drag.
        deck.touch_down(5, 2, 1260.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_motion(5, 1280.0, 30.0, 2008, 60, &mut |_, _| {});
        assert!(deck.slider.drag.is_some());
        deck.release_all(&mut |_, _| {});
        assert!(!deck.slider.active);
        assert!(deck.slider.drag.is_none());
    }

    #[test]
    fn a_cancelled_drag_leaves_every_key_released() {
        let mut deck = slider_deck();
        let mut emit = |_: &[Key], _: bool| panic!("volume route emitted keys");
        deck.touch_down(0, 2, 1260.0, 30.0, &mut emit, &mut |_| {});
        deck.touch_motion(0, 1300.0, 30.0, 2008, 60, &mut emit);
        deck.release_all(&mut emit);
        assert!(deck.slider.drag.is_none());
        assert!(!deck.slider.active);
        assert!(deck.touches.is_empty());
        assert!(deck.current().buttons.iter().all(|(_, b)| !b.active));
    }

    #[test]
    fn legacy_double_fn_swap_collapses_the_slider_before_the_home_changes() {
        let mut deck = LayerDeck::new(
            vec![
                (
                    "media".into(),
                    FunctionLayer::with_config(vec![
                        slider_button("Volume"),
                        slider_button("V-"),
                        slider_button("V+"),
                    ]),
                ),
                (
                    "primary".into(),
                    FunctionLayer::with_config(vec![keys("F1", &[Key::F1])]),
                ),
            ],
            0,
            Some(1),
            None,
            true,
        );
        let mut emit = |_: &[Key], _: bool| {};
        // Cycle Fn once so the next press counts as a double press.
        assert!(deck.fn_event(true, false, &mut emit));
        assert!(deck.fn_event(false, false, &mut emit));
        assert_eq!(deck.active, deck.home);

        // Summon the slider on the media home bar.
        assert!(!deck.touch_down_at(0, 1400.0, 30.0, 2008, 60, &mut emit, &mut |_| {}));
        deck.touch_up(0, &mut emit);
        assert!(deck.slider.active);
        assert_eq!(deck.slider.region, Some((0, 2)));

        // The legacy double press swaps the home to the one-button primary;
        // the slider collapses while its region still matches the old home,
        // so releasing Fn and the linger deadline cannot touch stale indices.
        assert!(!deck.fn_event(true, true, &mut emit));
        assert!(deck.fn_event(false, false, &mut emit));
        assert_eq!(deck.home, 1);
        assert_eq!(deck.active, 1);
        assert!(!deck.slider.active);
        assert!(deck.layers[0]
            .buttons
            .iter()
            .all(|(_, button)| button.changed));
        assert_eq!(deck.slider.deadline, None);
        assert!(!deck.slider_refresh(Instant::now() + Duration::from_secs(60)));
    }

    #[test]
    fn decks_without_a_slider_control_never_summon_a_slider() {
        let mut deck = named_deck();
        deck.touch_down(0, 0, 330.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(0, &mut |_, _| {});
        assert!(!deck.slider.active);
        assert!(!deck.slider_drag_start(1, 500.0, 30.0, 2008, 60, &mut |_, _| {}));
    }

    /// Answer the deck's due mute probe with the given mixer state.
    fn answer_mute_probe(deck: &mut LayerDeck, now: Instant, muted: bool) {
        let (revision, request) = deck
            .mute_probe(now)
            .expect("a volume-slider deck owes the mixer a mute read");
        assert_eq!(request, LevelRequest::Get);
        deck.mute_reply(revision, Ok(LevelState { percent: 65, muted }));
    }

    #[test]
    fn the_closed_slider_wears_a_live_mute_mark_on_its_volume_button() {
        let mut deck = slider_deck();
        answer_mute_probe(&mut deck, Instant::now(), true);
        let buttons = &deck.current().buttons;
        assert!(buttons[1].1.muted, "the volume button carries the mark");
        // The mark rides exactly the volume slider controls of the deck:
        // every button that would open the volume slider shows it, and
        // nothing else does.
        assert!(buttons.iter().enumerate().all(|(i, (_, button))| {
            let is_volume = button.action == ButtonAction::VolumeSlider;
            button.muted == (is_volume && i != 3) && (!button.muted || is_volume)
        }));
        assert!(deck.layers.iter().all(|layer| {
            layer
                .buttons
                .iter()
                .all(|(_, button)| !button.muted || button.action == ButtonAction::VolumeSlider)
        }));
        // One probe per cadence window, and unmuting clears the mark
        // through the same probe path.
        let later = Instant::now() + slider::MUTE_PROBE_INTERVAL;
        assert!(deck.mute_probe(later - Duration::from_millis(1)).is_none());
        answer_mute_probe(&mut deck, later, false);
        assert!(deck
            .current()
            .buttons
            .iter()
            .all(|(_, button)| !button.muted));
    }

    #[test]
    fn decks_without_a_volume_slider_never_probe_the_mixer() {
        let mut deck = named_deck();
        for _ in 0..3 {
            assert!(deck.mute_probe(Instant::now()).is_none());
        }
        // A stray reply can never invent a mark, either.
        deck.mute_reply(
            7,
            Ok(LevelState {
                percent: 65,
                muted: true,
            }),
        );
        assert!(deck
            .layers
            .iter()
            .all(|layer| layer.buttons.iter().all(|(_, button)| !button.muted)));
    }

    #[test]
    fn an_outstanding_probe_reply_never_disturbs_a_drawn_slider() {
        let mut deck = slider_deck();
        // A probe is outstanding when the user summons the slider.
        let (probe, _) = deck.mute_probe(Instant::now()).unwrap();
        let mut emit = |_: &[Key], _: bool| panic!("slider emitted keys");
        deck.touch_down_at(0, 1260.0, 30.0, 2008, 60, &mut emit, &mut |_| {});
        assert!(deck.slider.active);
        let pending = deck.slider.requests.len();
        let revision_before = deck.slider.revision;
        // The probe reply lands mid-read: the watch claims it and the
        // drawn slider keeps its own read pending.
        deck.mute_reply(
            probe,
            Ok(LevelState {
                percent: 65,
                muted: true,
            }),
        );
        assert!(deck.slider.active);
        assert_eq!(deck.slider.revision, revision_before);
        assert_eq!(deck.slider.requests.len(), pending);
        assert!(deck.slider_render().is_none());
        // With the probe settled, the open slider itself is what pauses
        // the watch.
        assert!(deck.mute_probe(Instant::now()).is_none());
    }

    #[test]
    fn collapsing_the_slider_hands_the_canvas_back_to_the_mute_mark() {
        let mut deck = slider_deck();
        let mut actual = 100;
        deck.touch_down_at(0, 1260.0, 30.0, 2008, 60, &mut |_, _| {}, &mut |_| {});
        apply_mixer_requests(&mut deck, &mut actual);
        assert!(deck.slider_render().is_some());
        // While the slider is drawn, the mark's watch stays paused.
        assert!(deck.mute_probe(Instant::now()).is_none());
        // Collapsing re-probes immediately, and the reply marks the button.
        deck.slider_collapse();
        answer_mute_probe(&mut deck, Instant::now(), true);
        assert!(deck.current().buttons[1].1.muted);
        assert!(deck.current().buttons[1].1.changed);
    }

    #[test]
    fn mute_mark_renders_in_both_slider_states_and_restores_exactly() {
        let config = outline_config();
        let (width, height) = (2008, 60);
        let mut deck = slider_deck();
        let mut surface = ImageSurface::create(Format::ARgb32, height, width).unwrap();
        let background = crate::daycycle::BLACK;
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            true,
            None,
            background,
        );
        let closed_unmuted = surface.data().unwrap().to_vec();
        let spans_before: Vec<(f64, f64)> = deck
            .current()
            .layout_spans(width, 0)
            .iter()
            .map(|span| (span.left, span.right))
            .collect();

        // Muted with the slider closed: the incremental redraw of just the
        // volume button lays down the mark; the pair's geometry is frozen.
        answer_mute_probe(&mut deck, Instant::now(), true);
        let spans_after: Vec<(f64, f64)> = deck
            .current()
            .layout_spans(width, 0)
            .iter()
            .map(|span| (span.left, span.right))
            .collect();
        assert_eq!(spans_before, spans_after);
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            false,
            None,
            background,
        );
        assert_ne!(&*surface.data().unwrap(), closed_unmuted.as_slice());
        let closed_muted = surface.data().unwrap().to_vec();

        // Muted with the slider open: the drawn track keeps its own muted
        // look at the same level, distinct from the unmuted draw. A fresh
        // open queues a fresh read; a replayed revision is stale.
        let mut emit = |_: &[Key], _: bool| panic!("slider emitted keys");
        deck.touch_down_at(0, 1260.0, 30.0, width as u16, 60, &mut emit, &mut |_| {});
        for (revision, kind, request) in std::mem::take(&mut deck.slider.requests) {
            assert_eq!(kind, Kind::Volume);
            assert_eq!(request, LevelRequest::Get);
            deck.slider_reply(
                revision,
                kind,
                Ok(LevelState {
                    percent: 65,
                    muted: false,
                }),
            );
        }
        let slider = deck.slider_render();
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            false,
            slider,
            background,
        );
        let open_unmuted = surface.data().unwrap().to_vec();
        deck.touch_down(0, 1, 1260.0, 30.0, &mut emit, &mut |_| {});
        deck.touch_up(0, &mut emit);
        for (revision, kind, request) in std::mem::take(&mut deck.slider.requests) {
            assert_eq!(kind, Kind::Volume);
            assert_eq!(request, LevelRequest::Get);
            deck.slider_reply(
                revision,
                kind,
                Ok(LevelState {
                    percent: 65,
                    muted: true,
                }),
            );
        }
        assert!(deck.slider_render().unwrap().muted);
        let slider = deck.slider_render();
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            false,
            slider,
            background,
        );
        assert_ne!(&*surface.data().unwrap(), open_unmuted.as_slice());

        // Collapsing restores exactly the closed muted bar.
        deck.slider_collapse();
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            false,
            None,
            background,
        );
        assert_eq!(&*surface.data().unwrap(), closed_muted.as_slice());
    }
}
