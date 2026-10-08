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

mod backlight;
mod command;
mod config;
mod display;
mod fonts;
mod pet;
mod pixel_shift;

use crate::config::ConfigManager;
use backlight::BacklightManager;
use command::CommandRunner;
use config::{ButtonAction, ButtonConfig, Config};
use display::DrmBackend;
use pet::{Bar as PetBar, PetManager, Span as PetSpan};
use pixel_shift::{PixelShiftManager, PIXEL_SHIFT_WIDTH_PX};

const BUTTON_SPACING_PX: i32 = 16;
const BUTTON_COLOR_INACTIVE: f64 = 0.200;
const BUTTON_COLOR_ACTIVE: f64 = 0.400;
const DEFAULT_ICON_SIZE: i32 = 48;
const TIMEOUT_MS: i32 = 10 * 1000;

// Transient volume slider, macOS Touch Bar style: pressing a volume button on
// the home bar expands a draggable slider over the volume cluster, which
// collapses again after a moment of inactivity.
/// Discrete volume steps the slider tracks and renders. Every step is one
/// VolumeUp/VolumeDown key tap delivered through the same uinput backend the
/// volume buttons use; the daemon cannot read the desktop mixer (it runs as
/// an unprivileged user and uses no external services), so it tracks the
/// level driven through its own key events from a midpoint estimate.
const VOLUME_LEVELS: u8 = 16;
const VOLUME_INITIAL_LEVEL: u8 = VOLUME_LEVELS / 2;
/// How long the slider stays up after the last interaction.
const VOLUME_SLIDER_LINGER: Duration = Duration::from_millis(2500);
/// Presses on volume buttons become drags once the finger moves this far,
/// so a shaky tap still taps while an intentional drag always drags.
const VOLUME_DRAG_THRESHOLD_PX: f64 = 8.0;
/// Width of the mute affordance anchoring the left end of the slider.
const VOLUME_MUTE_ZONE_PX: f64 = 28.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BatteryState {
    NotCharging,
    Charging,
    Low,
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
    Spacer,
}

struct Button {
    image: ButtonImage,
    changed: bool,
    active: bool,
    action: ButtonAction,
    icon_width: f64,
    icon_height: f64,
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
            Some("Discharging") if capacity < 10 => BatteryState::Low,
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
        if let Some(text) = cfg.text {
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
        } else {
            Button::new_spacer()
        }
    }
    fn new_spacer() -> Button {
        Button {
            action: ButtonAction::default(),
            active: false,
            changed: false,
            image: ButtonImage::Spacer,
            icon_width: 0.0,
            icon_height: 0.0,
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
                let extents = c.text_extents(text).unwrap();
                c.move_to(
                    button_left_edge + (button_width as f64 / 2.0 - extents.width() / 2.0).round(),
                    y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                );
                c.show_text(text).unwrap();
            }
            ButtonImage::Svg(svg) => {
                let x =
                    button_left_edge + (button_width as f64 / 2.0 - self.icon_width / 2.0).round();
                let y = y_shift + ((height as f64 - self.icon_height) / 2.0).round();

                svg.render_document(c, &Rectangle::new(x, y, self.icon_width, self.icon_height))
                    .unwrap();
            }
            ButtonImage::Bitmap(surf) => {
                let x =
                    button_left_edge + (button_width as f64 / 2.0 - self.icon_width / 2.0).round();
                let y = y_shift + ((height as f64 - self.icon_height) / 2.0).round();
                c.set_source_surface(surf, x, y).unwrap();
                c.rectangle(x, y, self.icon_width, self.icon_height);
                c.fill().unwrap();
            }
            ButtonImage::Time(format, locale) => {
                let current_time = Local::now();
                let formatted_time = current_time
                    .format_localized_with_items(format.iter(), *locale)
                    .to_string();
                let time_extents = c.text_extents(&formatted_time).unwrap();
                c.move_to(
                    button_left_edge
                        + (button_width as f64 / 2.0 - time_extents.width() / 2.0).round(),
                    y_shift + (height as f64 / 2.0 + time_extents.height() / 2.0).round(),
                );
                c.show_text(&formatted_time).unwrap();
            }
            ButtonImage::Battery(battery, battery_mode, icons) => {
                let Some(source) = battery.source() else {
                    let text = "Battery N/A";
                    let extents = c.text_extents(text).unwrap();
                    c.move_to(
                        button_left_edge + (button_width as f64 - extents.width()) / 2.0,
                        y_shift + (height as f64 + extents.height()) / 2.0,
                    );
                    c.show_text(text).unwrap();
                    return;
                };
                let (capacity, state) = (source.capacity, source.state);
                c.save().unwrap();
                let label = battery.label();
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
                    c.set_font_size(12.0);
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
            ButtonImage::Spacer => (),
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
    fn set_background_color(&self, c: &Context, color: f64) {
        if let ButtonImage::Battery(battery, _, _) = &self.image {
            match battery
                .source()
                .map(|source| source.state)
                .unwrap_or(BatteryState::NotCharging)
            {
                BatteryState::NotCharging => c.set_source_rgb(color, color, color),
                BatteryState::Charging => c.set_source_rgb(0.0, color, 0.0),
                BatteryState::Low => c.set_source_rgb(color, 0.0, 0.0),
            }
        } else {
            c.set_source_rgb(color, color, color);
        }
    }
}

#[derive(Default)]
pub struct FunctionLayer {
    displays_time: bool,
    buttons: Vec<(usize, Button)>,
    virtual_button_count: usize,
    faster_refresh: bool,
    // The volume-cluster span the last draw covered with the slider, so a
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
        FunctionLayer {
            displays_time,
            buttons,
            virtual_button_count,
            faster_refresh,
            slider_region_drawn: None,
        }
    }
    fn draw(
        &mut self,
        config: &Config,
        (width, height): (i32, i32),
        surface: &Surface,
        pixel_shift: (f64, f64),
        complete_redraw: bool,
        volume: Option<VolumeRender>,
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
        let radius = 8.0f64;
        let bot = (height as f64) * 0.15;
        let top = (height as f64) * 0.85;
        let (pixel_shift_x, pixel_shift_y) = pixel_shift;

        if complete_redraw {
            c.set_source_rgb(0.0, 0.0, 0.0);
            c.paint().unwrap();
        }
        c.set_font_face(&config.font_face);
        c.set_font_size(32.0);

        // The slider no longer covers a span it was drawn over: either it
        // collapsed, or it moved to another cluster. Erase that whole column
        // and make its buttons repaint over it, gaps included.
        let uncovered = match (self.slider_region_drawn.take(), volume) {
            (Some((first, last)), None) => Some((first, last)),
            (Some((first, last)), Some(v)) if (first, last) != (v.first, v.last) => {
                Some((first, last))
            }
            _ => None,
        };
        if let Some((first, last)) = uncovered {
            let first_start = self.buttons[first].0;
            let last_end = if last + 1 < self.buttons.len() {
                self.buttons[last + 1].0
            } else {
                self.virtual_button_count
            };
            let left_edge =
                (first_start as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64)).floor()
                    + pixel_shift_x
                    + (pixel_shift_width / 2) as f64;
            let region_width = virtual_button_width
                + ((last_end - first_start - 1) as f64
                    * (virtual_button_width + BUTTON_SPACING_PX as f64))
                    .floor();
            c.set_source_rgb(0.0, 0.0, 0.0);
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

        for i in 0..self.buttons.len() {
            let end = if i + 1 < self.buttons.len() {
                self.buttons[i + 1].0
            } else {
                self.virtual_button_count
            };
            let (start, button) = &mut self.buttons[i];
            let start = *start;

            if volume.is_some_and(|v| i >= v.first && i <= v.last) {
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

            let color = if button.active {
                BUTTON_COLOR_ACTIVE
            } else if config.show_button_outlines {
                BUTTON_COLOR_INACTIVE
            } else {
                0.0
            };
            if !complete_redraw {
                c.set_source_rgb(0.0, 0.0, 0.0);
                c.rectangle(
                    left_edge,
                    bot - radius,
                    button_width,
                    top - bot + radius * 2.0,
                );
                c.fill().unwrap();
            }
            if !matches!(button.image, ButtonImage::Spacer) {
                button.set_background_color(&c, color);
                // draw box with rounded corners
                c.new_sub_path();
                let left = left_edge + radius;
                let right = (left_edge + button_width.ceil()) - radius;
                c.arc(
                    right,
                    bot,
                    radius,
                    (-90.0f64).to_radians(),
                    (0.0f64).to_radians(),
                );
                c.arc(
                    right,
                    top,
                    radius,
                    (0.0f64).to_radians(),
                    (90.0f64).to_radians(),
                );
                c.arc(
                    left,
                    top,
                    radius,
                    (90.0f64).to_radians(),
                    (180.0f64).to_radians(),
                );
                c.arc(
                    left,
                    bot,
                    radius,
                    (180.0f64).to_radians(),
                    (270.0f64).to_radians(),
                );
                c.close_path();
                c.fill().unwrap();
            }
            c.set_source_rgb(1.0, 1.0, 1.0);
            button.render(
                &c,
                height,
                left_edge,
                button_width.ceil() as u64,
                pixel_shift_y,
            );

            button.changed = false;

            if !complete_redraw {
                modified_regions.push(ClipRect::new(
                    height as u16 - top as u16 - radius as u16,
                    left_edge as u16,
                    height as u16 - bot as u16 + radius as u16,
                    left_edge as u16 + button_width as u16,
                ));
            }
        }

        if let Some(volume) = volume {
            self.slider_region_drawn = Some((volume.first, volume.last));
            let first_start = self.buttons[volume.first].0;
            let last_end = if volume.last + 1 < self.buttons.len() {
                self.buttons[volume.last + 1].0
            } else {
                self.virtual_button_count
            };
            let left_edge =
                (first_start as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64)).floor()
                    + pixel_shift_x
                    + (pixel_shift_width / 2) as f64;
            let region_width = virtual_button_width
                + ((last_end - first_start - 1) as f64
                    * (virtual_button_width + BUTTON_SPACING_PX as f64))
                    .floor();
            let right_edge = left_edge + region_width;

            if !complete_redraw {
                // Clear the whole cluster column, icons and outlines included.
                c.set_source_rgb(0.0, 0.0, 0.0);
                c.rectangle(
                    (left_edge - 2.0).max(0.0),
                    0.0,
                    region_width + 4.0,
                    height as f64,
                );
                c.fill().unwrap();
            }

            // The track and fill replace the cluster, macOS-style: a slim
            // rounded bar across the region with a fill up to the level,
            // anchored left by the mute affordance.
            let track_left = left_edge + VOLUME_MUTE_ZONE_PX + 2.0;
            let track_right = right_edge - 2.0;
            let track_height = 14.0;
            let track_top = (height as f64 - track_height) / 2.0 + pixel_shift_y;
            c.set_source_rgb(
                BUTTON_COLOR_INACTIVE,
                BUTTON_COLOR_INACTIVE,
                BUTTON_COLOR_INACTIVE,
            );
            rounded_rect(
                &c,
                track_left,
                track_top,
                track_right - track_left,
                track_height,
                track_height / 2.0,
            );
            c.fill().unwrap();

            let frac = volume.level as f64 / VOLUME_LEVELS as f64;
            let fill_width = (track_right - track_left - 2.0) * frac;
            if fill_width > 0.0 {
                let fill_height = track_height - 2.0;
                let fill = if volume.muted {
                    BUTTON_COLOR_ACTIVE
                } else {
                    1.0
                };
                c.set_source_rgb(fill, fill, fill);
                rounded_rect(
                    &c,
                    track_left + 1.0,
                    track_top + 1.0,
                    fill_width,
                    fill_height,
                    fill_height / 2.0,
                );
                c.fill().unwrap();
            }

            // The mute affordance: a speaker glyph on the bare segment left
            // of the track; muted adds a slash.
            let mid = track_top + track_height / 2.0;
            let glyph_x = left_edge + 8.0;
            c.set_source_rgb(1.0, 1.0, 1.0);
            c.rectangle(glyph_x, mid - 3.0, 4.0, 6.0);
            c.fill().unwrap();
            c.new_sub_path();
            c.move_to(glyph_x + 4.0, mid - 3.0);
            c.line_to(glyph_x + 12.0, mid - 7.0);
            c.line_to(glyph_x + 12.0, mid + 7.0);
            c.line_to(glyph_x + 4.0, mid + 3.0);
            c.close_path();
            c.fill().unwrap();
            if volume.muted {
                c.set_line_width(5.0);
                c.set_source_rgb(0.0, 0.0, 0.0);
                c.move_to(glyph_x - 2.0, mid + 9.0);
                c.line_to(glyph_x + 14.0, mid - 9.0);
                c.stroke().unwrap();
                c.set_line_width(2.0);
                c.set_source_rgb(1.0, 1.0, 1.0);
                c.move_to(glyph_x - 2.0, mid + 9.0);
                c.line_to(glyph_x + 14.0, mid - 9.0);
                c.stroke().unwrap();
            }

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
    // Set for presses on the home bar's volume cluster; crossing the drag
    // threshold converts the press into a slider drag.
    volume: bool,
    start_x: f64,
    start_y: f64,
}

/// A finger currently dragging the volume slider. A touch that began on
/// the mute affordance keeps its start position: released in place it
/// toggles mute, moved past the drag threshold it becomes a level drag.
struct VolumeDrag {
    slot: u32,
    mute_start: Option<(f64, f64)>,
}

/// State of the transient volume slider on the home bar.
struct VolumeOverlay {
    active: bool,
    level: u8,
    muted: bool,
    /// Contiguous home-layer button indices (first, last) the slider covers.
    region: Option<(usize, usize)>,
    deadline: Option<Instant>,
    drag: Option<VolumeDrag>,
}

impl Default for VolumeOverlay {
    fn default() -> Self {
        Self {
            active: false,
            level: VOLUME_INITIAL_LEVEL,
            muted: false,
            region: None,
            deadline: None,
            drag: None,
        }
    }
}

/// What `FunctionLayer::draw` needs to render the summoned slider.
#[derive(Clone, Copy)]
struct VolumeRender {
    first: usize,
    last: usize,
    level: u8,
    muted: bool,
}

/// The volume actions that summon and drive the slider.
fn volume_key(action: &ButtonAction) -> Option<Key> {
    if let ButtonAction::Keys(keys) = action {
        match keys.as_slice() {
            [Key::VolumeUp] | [Key::VolumeDown] | [Key::Mute] => Some(keys[0]),
            _ => None,
        }
    } else {
        None
    }
}

/// Map a touch position inside the slider region to a volume level.
fn volume_level_at(x: f64, left: f64, right: f64) -> u8 {
    let width = (right - left).max(1.0);
    let frac = ((x - left) / width).clamp(0.0, 1.0);
    (frac * VOLUME_LEVELS as f64)
        .round()
        .min(VOLUME_LEVELS as f64) as u8
}

/// Move the real volume by delivering whole key taps through uinput.
fn emit_volume_steps(emit_keys: &mut impl FnMut(&[Key], bool), key: Key, count: usize) {
    for _ in 0..count {
        emit_keys(&[key], true);
        emit_keys(&[key], false);
    }
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
    volume: VolumeOverlay,
}

impl LayerDeck {
    fn new(
        layers: Vec<(String, FunctionLayer)>,
        home: usize,
        fn_layer: Option<usize>,
        pet_layer: Option<usize>,
        legacy: bool,
    ) -> Self {
        let names = layers
            .iter()
            .enumerate()
            .map(|(i, (name, _))| (name.clone(), i))
            .collect();
        Self {
            layers: layers.into_iter().map(|(_, layer)| layer).collect(),
            names,
            home,
            fn_layer,
            pet_layer,
            legacy,
            active: home,
            fn_pressed: false,
            fn_return: None,
            touches: HashMap::new(),
            volume: VolumeOverlay::default(),
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
        self.volume_collapse();
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
                self.volume_collapse();
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
        // Pressing a volume-cluster button on the home bar summons the
        // transient slider and steps its level; the key itself still follows
        // the normal momentary press/release path below.
        let volume = if self.active == self.home {
            volume_key(&self.current().buttons[button].1.action)
        } else {
            None
        };
        if let Some(key) = volume {
            self.volume_press(button, key);
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
                volume: volume.is_some(),
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
        if self.volume_drag_start(slot, x, y, width, height, emit_keys) {
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
            .map(|touch| (touch.button, touch.volume, touch.start_x, touch.start_y))
        else {
            return;
        };
        if volume
            && self.volume.active
            && (x - start_x).hypot(y - start_y) > VOLUME_DRAG_THRESHOLD_PX
        {
            // An intentional drag: release the held key first, then set
            // the level directly from the finger position.
            self.touch_up(slot, emit_keys);
            if self.volume.drag.is_none() {
                self.volume.drag = Some(VolumeDrag {
                    slot,
                    mute_start: None,
                });
                if let Some((left, right)) = self.volume_track(width) {
                    self.volume_set_level(volume_level_at(x, left, right), emit_keys);
                }
                self.volume.deadline = Some(Instant::now() + VOLUME_SLIDER_LINGER);
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
        if self
            .volume
            .drag
            .as_ref()
            .is_some_and(|drag| drag.slot == slot)
        {
            // Released in place on the mute affordance: the tap toggles
            // mute with one momentary key pair.
            if self
                .volume
                .drag
                .as_ref()
                .is_some_and(|drag| drag.mute_start.is_some())
            {
                self.volume.muted = !self.volume.muted;
                emit_keys(&[Key::Mute], true);
                emit_keys(&[Key::Mute], false);
                self.mark_volume_changed();
            }
            self.volume.drag = None;
            self.volume.deadline = Some(Instant::now() + VOLUME_SLIDER_LINGER);
            return;
        }
        if let Some(touch) = self.touches.remove(&slot) {
            if touch.volume && self.volume.active {
                self.volume.deadline = Some(Instant::now() + VOLUME_SLIDER_LINGER);
            }
            self.update_button(touch.button, emit_keys);
        }
    }

    fn replace(&mut self, new: Self, emit_keys: &mut impl FnMut(&[Key], bool)) {
        self.release_all(emit_keys);
        *self = new;
    }

    /// Expand the slider over the contiguous volume-cluster run around the
    /// pressed home-layer button and apply the press to the tracked level.
    fn volume_press(&mut self, button: usize, key: Key) {
        let layer = &self.layers[self.home];
        let mut first = button;
        while first > 0 && volume_key(&layer.buttons[first - 1].1.action).is_some() {
            first -= 1;
        }
        let mut last = button;
        while last + 1 < layer.buttons.len()
            && volume_key(&layer.buttons[last + 1].1.action).is_some()
        {
            last += 1;
        }
        self.volume.active = true;
        self.volume.region = Some((first, last));
        match key {
            Key::VolumeUp => {
                self.volume.muted = false;
                self.volume.level = (self.volume.level + 1).min(VOLUME_LEVELS);
            }
            Key::VolumeDown => {
                self.volume.muted = false;
                self.volume.level = self.volume.level.saturating_sub(1);
            }
            _ => self.volume.muted = !self.volume.muted,
        }
        self.volume.deadline = Some(Instant::now() + VOLUME_SLIDER_LINGER);
        self.mark_volume_changed();
    }

    fn volume_collapse(&mut self) {
        if !self.volume.active {
            self.volume.drag = None;
            return;
        }
        self.volume.active = false;
        self.volume.drag = None;
        self.volume.deadline = None;
        self.mark_volume_changed();
    }

    fn mark_volume_changed(&mut self) {
        if let Some((first, last)) = self.volume.region {
            for (_, button) in &mut self.layers[self.home].buttons[first..=last] {
                button.changed = true;
            }
        }
    }

    /// Left/right pixel bounds of the slider region in hit-test coordinates.
    fn volume_region(&self, width: u16) -> Option<(f64, f64)> {
        let (first, last) = self.volume.region?;
        let layer = &self.layers[self.home];
        let virtual_button_width =
            (width as i32 - (BUTTON_SPACING_PX * (layer.virtual_button_count - 1) as i32)) as f64
                / layer.virtual_button_count as f64;
        let left =
            layer.buttons[first].0 as f64 * (virtual_button_width + BUTTON_SPACING_PX as f64);
        let end = if last + 1 < layer.buttons.len() {
            layer.buttons[last + 1].0
        } else {
            layer.virtual_button_count
        };
        let region_width = virtual_button_width
            + ((end - layer.buttons[first].0 - 1) as f64
                * (virtual_button_width + BUTTON_SPACING_PX as f64))
                .floor();
        Some((left, left + region_width))
    }

    /// Pixel bounds of the draggable track drawn inside the slider region,
    /// after the mute affordance that anchors its left end.
    fn volume_track(&self, width: u16) -> Option<(f64, f64)> {
        let (left, right) = self.volume_region(width)?;
        Some((left + VOLUME_MUTE_ZONE_PX + 2.0, right - 2.0))
    }

    fn volume_set_level(&mut self, target: u8, emit_keys: &mut impl FnMut(&[Key], bool)) {
        let target = target.min(VOLUME_LEVELS);
        let delta = target as i32 - self.volume.level as i32;
        if delta > 0 {
            emit_volume_steps(emit_keys, Key::VolumeUp, delta as usize);
        } else if delta < 0 {
            emit_volume_steps(emit_keys, Key::VolumeDown, (-delta) as usize);
        }
        self.volume.level = target;
        self.volume.muted = false;
        self.mark_volume_changed();
    }

    /// A touch-down on the drawn slider grabs it right away at the touched
    /// level; the speaker icon at its left end waits for a tap or a drag.
    fn volume_drag_start(
        &mut self,
        slot: u32,
        x: f64,
        y: f64,
        width: u16,
        height: u16,
        emit_keys: &mut impl FnMut(&[Key], bool),
    ) -> bool {
        if !self.volume.active
            || self.active != self.home
            || self.volume.drag.is_some()
            || self.touches.contains_key(&slot)
        {
            return false;
        }
        let Some((left, right)) = self.volume_region(width) else {
            return false;
        };
        if x < left || x > right || y < 0.1 * height as f64 || y > 0.9 * height as f64 {
            return false;
        }
        // The speaker icon at the left end waits for a tap or a drag;
        // everywhere else the track grabs the touch at its level.
        let mute_start = if x < left + VOLUME_MUTE_ZONE_PX {
            Some((x, y))
        } else {
            None
        };
        self.volume.drag = Some(VolumeDrag { slot, mute_start });
        self.volume.deadline = Some(Instant::now() + VOLUME_SLIDER_LINGER);
        if mute_start.is_none() {
            if let Some((left, right)) = self.volume_track(width) {
                self.volume_set_level(volume_level_at(x, left, right), emit_keys);
            }
        }
        true
    }

    /// Drag motion sets the level directly; returns whether this slot owns
    /// the drag so the caller can skip normal button motion handling.
    fn volume_drag_motion(
        &mut self,
        slot: u32,
        x: f64,
        y: f64,
        width: u16,
        emit_keys: &mut impl FnMut(&[Key], bool),
    ) -> bool {
        if !self.volume.active || !self.volume.drag.as_ref().is_some_and(|d| d.slot == slot) {
            return false;
        }
        if let Some((start_x, start_y)) = self.volume.drag.as_ref().and_then(|drag| drag.mute_start)
        {
            if (x - start_x).hypot(y - start_y) <= VOLUME_DRAG_THRESHOLD_PX {
                self.volume.deadline = Some(Instant::now() + VOLUME_SLIDER_LINGER);
                return true;
            }
            if let Some(drag) = self.volume.drag.as_mut() {
                drag.mute_start = None;
            }
        }
        if let Some((left, right)) = self.volume_track(width) {
            self.volume_set_level(volume_level_at(x, left, right), emit_keys);
        }
        self.volume.deadline = Some(Instant::now() + VOLUME_SLIDER_LINGER);
        true
    }

    /// A drag or a still-held volume touch is ongoing interaction: the
    /// slider stays up under the finger regardless of the deadline.
    fn volume_interaction_held(&self) -> bool {
        self.volume.drag.is_some() || self.touches.values().any(|touch| touch.volume)
    }

    /// Collapse the slider once its linger deadline passes with no
    /// interaction held.
    fn volume_refresh(&mut self, now: Instant) -> bool {
        if !self.volume.active
            || self.volume_interaction_held()
            || !self.volume.deadline.is_some_and(|deadline| now >= deadline)
        {
            return false;
        }
        self.volume_collapse();
        true
    }

    /// Time remaining until the slider collapses, for the epoll timeout.
    fn volume_timeout(&self, now: Instant) -> Option<Duration> {
        if !self.volume.active || self.volume_interaction_held() {
            return None;
        }
        self.volume
            .deadline
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    /// Slider rendering info, only while summoned on the home bar itself.
    fn volume_render(&self) -> Option<VolumeRender> {
        if !self.volume.active || self.active != self.home {
            return None;
        }
        let (first, last) = self.volume.region?;
        Some(VolumeRender {
            first,
            last,
            level: self.volume.level,
            muted: self.volume.muted,
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

        // The transient volume slider collapses on its own after a moment.
        layers.volume_refresh(Instant::now());
        if let Some(remaining) = layers.volume_timeout(Instant::now()) {
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
            let volume_render = layers.volume_render();
            let clips = layers.current_mut().draw(
                &cfg,
                (width as i32, height as i32),
                &surface,
                shift,
                needs_complete_redraw,
                volume_render,
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
                            if layers.volume_drag_motion(
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
            state: if capacity < 10 {
                BatteryState::Low
            } else {
                BatteryState::NotCharging
            },
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
        assert_eq!(sources[2].state, BatteryState::Low);
        fs::remove_dir_all(fixture.0.join("hidpp_mouse")).unwrap();
        fs::write(fixture.0.join("keyboard/present"), "0").unwrap();
        assert_eq!(read_battery_sources(&fixture.0).len(), 1);
        assert!(read_battery_sources(&fixture.0.join("missing")).is_empty());
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
            pet_enabled: false,
            pet_variant: Default::default(),
            pet_energy: Default::default(),
        };
        let (width, height) = (2008, 60);
        let mut surface = ImageSurface::create(Format::ARgb32, height, width).unwrap();
        deck.current_mut()
            .draw(&config, (width, height), &surface, (0.0, 0.0), true, None);
        let home_image = surface.data().unwrap().to_vec();

        // The center navigation button on home opens the function bar.
        let button = deck
            .current()
            .hit(width as u16, height as u16, (width / 2) as f64, 30.0, None)
            .unwrap();
        assert!(deck.touch_down(0, button, 1004.0, 30.0, &mut |_, _| {}, &mut |_| {}));
        deck.current_mut()
            .draw(&config, (width, height), &surface, (0.0, 0.0), true, None);
        assert_ne!(&*surface.data().unwrap(), home_image.as_slice());

        // The first button on the function bar returns to the same home image.
        let button = deck
            .current()
            .hit(width as u16, height as u16, 100.0, 30.0, None)
            .unwrap();
        assert!(deck.touch_down(1, button, 100.0, 30.0, &mut |_, _| {}, &mut |_| {}));
        deck.current_mut()
            .draw(&config, (width, height), &surface, (0.0, 0.0), true, None);
        assert_eq!(&*surface.data().unwrap(), home_image.as_slice());
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

    fn volume_deck() -> LayerDeck {
        LayerDeck::new(
            vec![
                (
                    "home".into(),
                    FunctionLayer::with_config(vec![
                        keys("V-", &[Key::VolumeDown]),
                        keys("Mute", &[Key::Mute]),
                        keys("V+", &[Key::VolumeUp]),
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

    // 4 buttons on 2008px: b0 0..490, b1 506..996, b2 1012..1502, b3 1518..2008,
    // so the volume cluster spans 0..1502 and its midpoint maps to level 8.

    #[test]
    fn volume_presses_summon_the_slider_and_grab_it_once_it_is_drawn() {
        let mut deck = volume_deck();
        let mut events = Vec::new();
        assert!(!deck.volume.active);
        assert!(!deck.touch_down_at(
            0,
            1260.0,
            30.0,
            2008,
            60,
            &mut |k, a| { events.push((k.to_vec(), a)) },
            &mut |_| {}
        ));
        assert!(deck.volume.active);
        assert_eq!(deck.volume.level, VOLUME_INITIAL_LEVEL + 1);
        assert_eq!(deck.volume.region, Some((0, 2)));
        deck.touch_up(0, &mut |k, a| events.push((k.to_vec(), a)));
        assert_eq!(
            events,
            vec![(vec![Key::VolumeUp], true), (vec![Key::VolumeUp], false)]
        );
        events.clear();
        // Once drawn, the slider owns the same spot: the press lands on
        // the track and jumps to the touched level (1260px -> 13) instead
        // of stepping, with whole key taps driving the change.
        assert!(!deck.touch_down_at(
            1,
            1260.0,
            30.0,
            2008,
            60,
            &mut |k, a| { events.push((k.to_vec(), a)) },
            &mut |_| {}
        ));
        assert_eq!(deck.volume.level, 13);
        assert!(deck.volume.drag.is_some());
        deck.touch_up(1, &mut |_, _| {});
        assert_eq!(events.len(), 8);
        assert!(events.iter().all(|(keys, _)| keys == &vec![Key::VolumeUp]));
        events.clear();
        // A press at the other end of the cluster jumps low: 200px -> 2.
        assert!(!deck.touch_down_at(
            2,
            200.0,
            30.0,
            2008,
            60,
            &mut |k, a| { events.push((k.to_vec(), a)) },
            &mut |_| {}
        ));
        assert_eq!(deck.volume.level, 2);
        deck.touch_up(2, &mut |_, _| {});
        assert!(events
            .iter()
            .all(|(keys, _)| keys == &vec![Key::VolumeDown]));
    }

    #[test]
    fn the_drawn_slider_grabs_touches_on_its_covered_buttons() {
        let mut deck = volume_deck();
        deck.touch_down_at(0, 1260.0, 30.0, 2008, 60, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(0, &mut |_, _| {});
        assert!(deck.volume.active);
        // 751px is the middle of the covered Mute button, not a gap.
        assert_eq!(deck.current().hit(2008, 60, 751.0, 30.0, None), Some(1));
        // The slider owns the touch anyway: it jumps to the touched level
        // without pressing the button or emitting its key.
        let mut events = Vec::new();
        assert!(!deck.touch_down_at(
            1,
            751.0,
            30.0,
            2008,
            60,
            &mut |k, a| { events.push((k.to_vec(), a)) },
            &mut |_| {}
        ));
        assert_eq!(deck.volume.level, 8);
        assert!(deck.volume.drag.is_some());
        deck.touch_up(1, &mut |_, _| {});
        assert_eq!(
            events,
            vec![
                (vec![Key::VolumeDown], true),
                (vec![Key::VolumeDown], false)
            ]
        );
    }

    #[test]
    fn the_mute_affordance_taps_to_toggle_and_still_drags() {
        let mut deck = volume_deck();
        deck.touch_down_at(0, 1260.0, 30.0, 2008, 60, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(0, &mut |_, _| {});
        assert!(deck.volume.active);
        assert!(!deck.volume.muted);

        // A tap on the speaker icon toggles mute, emits the key pair, and
        // keeps the slider up without jumping the level.
        let mut events = Vec::new();
        assert!(!deck.touch_down_at(1, 10.0, 30.0, 2008, 60, &mut |_, _| {}, &mut |_| {}));
        assert!(deck
            .volume
            .drag
            .as_ref()
            .is_some_and(|drag| drag.mute_start.is_some()));
        assert_eq!(deck.volume.level, VOLUME_INITIAL_LEVEL + 1);
        deck.touch_up(1, &mut |k, a| events.push((k.to_vec(), a)));
        assert!(deck.volume.muted);
        assert!(deck.volume.active);
        assert_eq!(
            events,
            vec![(vec![Key::Mute], true), (vec![Key::Mute], false)]
        );

        // Jitter under the threshold stays a pending mute tap.
        assert!(!deck.touch_down_at(2, 10.0, 30.0, 2008, 60, &mut |_, _| {}, &mut |_| {}));
        assert!(deck.volume_drag_motion(2, 14.0, 31.0, 2008, &mut |_, _| {}));
        assert!(deck
            .volume
            .drag
            .as_ref()
            .is_some_and(|drag| drag.mute_start.is_some()));
        // Crossing the threshold converts to a grab: the touched level
        // takes over and clears the muted display.
        assert!(deck.volume_drag_motion(2, 60.0, 33.0, 2008, &mut |_, _| {}));
        assert!(deck
            .volume
            .drag
            .as_ref()
            .is_some_and(|drag| drag.mute_start.is_none()));
        assert!(!deck.volume.muted);
        assert_eq!(deck.volume.level, 0);
        deck.touch_up(2, &mut |_, _| {});
        assert!(!deck.volume.muted);
    }

    #[test]
    fn volume_level_math_rounds_and_clamps() {
        assert_eq!(volume_level_at(-50.0, 0.0, 1502.0), 0);
        assert_eq!(volume_level_at(0.0, 0.0, 1502.0), 0);
        assert_eq!(volume_level_at(94.0, 0.0, 1502.0), 1);
        assert_eq!(volume_level_at(141.5, 0.0, 1502.0), 2);
        assert_eq!(volume_level_at(751.0, 0.0, 1502.0), 8);
        assert_eq!(volume_level_at(1502.0, 0.0, 1502.0), 16);
        assert_eq!(volume_level_at(5000.0, 0.0, 1502.0), 16);
    }

    #[test]
    fn slider_touches_drag_right_away_and_emit_step_taps() {
        let mut deck = volume_deck();
        let mut events = Vec::new();
        deck.touch_down(
            0,
            2,
            1260.0,
            30.0,
            &mut |k, a| events.push((k.to_vec(), a)),
            &mut |_| {},
        ); // summon at level 9
        deck.touch_up(0, &mut |_, _| {});
        events.clear();

        // Touching the slider itself (here the gap between cluster buttons)
        // jumps straight to the touched level: 498px -> 5, four taps down.
        assert!(
            deck.volume_drag_start(1, 498.0, 30.0, 2008, 60, &mut |k, a| {
                events.push((k.to_vec(), a))
            })
        );
        assert_eq!(deck.volume.level, 5);
        assert_eq!(events.len(), 8);
        assert!(events
            .iter()
            .all(|(keys, _)| keys == &vec![Key::VolumeDown]));

        // A second finger cannot steal the drag.
        assert!(!deck.volume_drag_start(2, 300.0, 30.0, 2008, 60, &mut |_, _| {}));

        // Motion drives the level live; equal positions emit nothing.
        assert!(
            deck.volume_drag_motion(1, 751.0, 30.0, 2008, &mut |k, a| events
                .push((k.to_vec(), a)))
        );
        assert_eq!(deck.volume.level, 8);
        let taps = events.len();
        assert!(deck.volume_drag_motion(1, 751.0, 30.0, 2008, &mut |_, _| {}));
        assert_eq!(events.len(), taps);
        // Off the ends the level clamps instead of leaving the slider.
        assert!(deck.volume_drag_motion(1, 1900.0, 30.0, 2008, &mut |_, _| {}));
        assert_eq!(deck.volume.level, VOLUME_LEVELS);
        assert!(deck.volume_drag_motion(1, -10.0, 30.0, 2008, &mut |_, _| {}));
        assert_eq!(deck.volume.level, 0);

        deck.touch_up(1, &mut |_, _| {});
        assert!(deck.volume.drag.is_none());
        assert!(deck.volume.active);
    }

    #[test]
    fn a_volume_press_converts_to_a_drag_and_releases_the_held_key() {
        let mut deck = volume_deck();
        let mut events = Vec::new();
        deck.touch_down(
            0,
            2,
            1260.0,
            30.0,
            &mut |k, a| events.push((k.to_vec(), a)),
            &mut |_| {},
        ); // VolumeUp held, level 9
        events.clear();

        // A jitter under the threshold stays a press; the key stays held.
        deck.touch_motion(0, 1264.0, 31.0, 2008, 60, &mut |_, _| {});
        assert!(events.is_empty());
        assert!(deck.touches.contains_key(&0));

        // Crossing the threshold releases the key and starts dragging:
        // 1280px -> level 14, five VolumeUp taps after the release.
        deck.touch_motion(0, 1280.0, 30.0, 2008, 60, &mut |k, a| {
            events.push((k.to_vec(), a))
        });
        assert_eq!(events[0], (vec![Key::VolumeUp], false));
        assert_eq!(deck.volume.level, 14);
        assert_eq!(events.len(), 11);
        assert!(!deck.touches.contains_key(&0));

        // The drag continues through the drag path until the finger lifts.
        assert!(deck.volume_drag_motion(0, 1200.0, 30.0, 2008, &mut |_, _| {}));
        assert_eq!(deck.volume.level, 13);
        deck.touch_up(0, &mut |_, _| {});
        assert!(deck.volume.drag.is_none());
    }

    #[test]
    fn mute_toggles_the_displayed_state_and_volume_or_drag_clears_it() {
        let mut deck = volume_deck();
        let mut events = Vec::new();
        deck.touch_down(
            0,
            1,
            750.0,
            30.0,
            &mut |k, a| events.push((k.to_vec(), a)),
            &mut |_| {},
        );
        assert!(deck.volume.active); // tapping mute can summon it too
        assert!(deck.volume.muted);
        deck.touch_up(0, &mut |k, a| events.push((k.to_vec(), a)));
        assert_eq!(
            events,
            vec![(vec![Key::Mute], true), (vec![Key::Mute], false)]
        );
        let shown = deck.volume_render().unwrap();
        assert!(shown.muted);
        assert_eq!((shown.first, shown.last), (0, 2));

        // A volume press clears the muted display, as the desktop does.
        deck.touch_down(1, 0, 200.0, 30.0, &mut |_, _| {}, &mut |_| {});
        assert!(!deck.volume.muted);
        deck.touch_up(1, &mut |_, _| {});

        // Muting again shows muted; dragging clears it once more.
        deck.touch_down(2, 1, 750.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(2, &mut |_, _| {});
        assert!(deck.volume.muted);
        assert!(deck.volume_drag_start(3, 751.0, 30.0, 2008, 60, &mut |_, _| {}));
        assert!(!deck.volume.muted);
    }

    #[test]
    fn the_slider_collapses_after_a_lull_and_interaction_resets_the_timer() {
        let mut deck = volume_deck();
        deck.touch_down(0, 2, 1260.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(0, &mut |_, _| {});
        let deadline = deck.volume.deadline.unwrap();

        // Just short of the deadline it is still up and still counting down.
        assert!(!deck.volume_refresh(deadline - Duration::from_millis(1)));
        assert!(deck.volume.active);
        assert!(deck
            .volume_timeout(deadline - Duration::from_secs(1))
            .is_some());

        // A fresh press restarts the clock.
        deck.touch_down(1, 2, 1260.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(1, &mut |_, _| {});
        assert!(deck.volume.deadline.unwrap() > deadline);

        // Past the deadline with no interaction it collapses and stops timing.
        let deadline = deck.volume.deadline.unwrap();
        assert!(deck.volume_refresh(deadline + Duration::from_millis(1)));
        assert!(!deck.volume.active);
        assert!(deck.volume_render().is_none());
        assert_eq!(deck.volume_timeout(deadline), None);
        // The collapse flags the cluster so the bar repaints it.
        assert!(deck.layers[deck.home].buttons[..3]
            .iter()
            .all(|(_, button)| button.changed));
    }

    #[test]
    fn a_held_volume_press_keeps_the_slider_up_until_release() {
        let mut deck = volume_deck();
        // The summoning press stays held: the key is down and the slider
        // must not collapse under the finger even past the deadline.
        deck.touch_down(0, 2, 1260.0, 30.0, &mut |_, _| {}, &mut |_| {});
        let deadline = deck.volume.deadline.unwrap();
        assert!(!deck.volume_refresh(deadline + Duration::from_secs(10)));
        assert!(deck.volume.active);
        // The wait hint reports no pending collapse, not a passed one.
        assert_eq!(
            deck.volume_timeout(deadline + Duration::from_secs(10)),
            None
        );
        // A still-held second-finger press on the cluster holds it too.
        deck.touch_down(1, 0, 200.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(0, &mut |_, _| {});
        assert!(!deck.volume_refresh(deadline + Duration::from_secs(20)));
        assert!(deck.volume.active);
        // Once every finger lifts, the release restarts the clock and the
        // quiet period collapses it.
        deck.touch_up(1, &mut |_, _| {});
        let deadline = deck.volume.deadline.unwrap();
        assert!(deck.volume_refresh(deadline + Duration::from_millis(1)));
        assert!(!deck.volume.active);
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
            pet_enabled: false,
            pet_variant: Default::default(),
            pet_energy: Default::default(),
        }
    }

    #[test]
    fn the_slider_covers_the_cluster_renders_its_level_and_restores_the_bar() {
        let mut deck = volume_deck();
        let config = outline_config();
        let (width, height) = (2008, 60);
        let mut surface = ImageSurface::create(Format::ARgb32, height, width).unwrap();
        deck.current_mut()
            .draw(&config, (width, height), &surface, (0.0, 0.0), true, None);
        let home_image = surface.data().unwrap().to_vec();

        // Pressing volume up summons the slider over the whole cluster.
        deck.touch_down(0, 2, 1260.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(0, &mut |_, _| {});
        let volume = deck.volume_render();
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            false,
            volume,
        );
        let low_level = surface.data().unwrap().to_vec();
        assert_ne!(&*low_level, home_image.as_slice());

        // A higher level fills more of the track.
        deck.volume.level = VOLUME_LEVELS;
        deck.mark_volume_changed();
        let volume = deck.volume_render();
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            false,
            volume,
        );
        assert_ne!(&*surface.data().unwrap(), low_level.as_slice());

        // The muted state renders differently at the same level.
        let full_level = surface.data().unwrap().to_vec();
        deck.volume.muted = true;
        deck.mark_volume_changed();
        let volume = deck.volume_render();
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            false,
            volume,
        );
        assert_ne!(&*surface.data().unwrap(), full_level.as_slice());

        // Collapsing restores exactly the original home bar.
        deck.volume_collapse();
        deck.current_mut()
            .draw(&config, (width, height), &surface, (0.0, 0.0), false, None);
        assert_eq!(&*surface.data().unwrap(), home_image.as_slice());
    }

    #[test]
    fn slider_clip_rects_stay_inside_the_framebuffer_when_the_cluster_ends_the_bar() {
        let mut deck = LayerDeck::new(
            vec![(
                "home".into(),
                FunctionLayer::with_config(vec![
                    keys("V-", &[Key::VolumeDown]),
                    keys("Mute", &[Key::Mute]),
                    keys("V+", &[Key::VolumeUp]),
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
        deck.current_mut()
            .draw(&config, (width, height), &surface, (0.0, 0.0), true, None);

        // The cluster spans the whole bar, so the slider's edges touch the
        // framebuffer borders; its dirty clips must still fit inside them.
        deck.touch_down(0, 2, 1800.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(0, &mut |_, _| {});
        let volume = deck.volume_render().unwrap();
        assert_eq!((volume.first, volume.last), (0, 2));
        let clips = deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            false,
            Some(volume),
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
        deck.volume_collapse();
        let clips =
            deck.current_mut()
                .draw(&config, (width, height), &surface, (0.0, 0.0), false, None);
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
                    keys("V-", &[Key::VolumeDown]),
                    keys("Mute", &[Key::Mute]),
                    keys("V+", &[Key::VolumeUp]),
                    keys("Esc", &[Key::Esc]),
                    keys("W-", &[Key::VolumeDown]),
                    keys("W+", &[Key::VolumeUp]),
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
        deck.current_mut()
            .draw(&config, (width, height), &surface, (0.0, 0.0), true, None);
        let home_image = surface.data().unwrap().to_vec();

        // Summon on the first cluster: its span is drawn over.
        deck.touch_down(0, 0, 100.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(0, &mut |_, _| {});
        assert_eq!(deck.volume.region, Some((0, 2)));
        let volume = deck.volume_render();
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            false,
            volume,
        );

        // Pressing the second, separated cluster moves the slider there.
        deck.touch_down(1, 4, 1500.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(1, &mut |_, _| {});
        assert_eq!(deck.volume.region, Some((4, 5)));
        let volume = deck.volume_render();
        deck.current_mut().draw(
            &config,
            (width, height),
            &surface,
            (0.0, 0.0),
            false,
            volume,
        );

        // Collapsing restores the whole bar, first cluster included.
        deck.volume_collapse();
        deck.current_mut()
            .draw(&config, (width, height), &surface, (0.0, 0.0), false, None);
        assert_eq!(&*surface.data().unwrap(), home_image.as_slice());
    }

    #[test]
    fn layer_changes_and_input_release_end_the_slider_and_only_home_summons_it() {
        let mut deck = volume_deck();
        deck.touch_down(0, 2, 1260.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(0, &mut |_, _| {});
        assert!(deck.volume.active);

        // Navigating away collapses it...
        assert!(deck.touch_down(1, 3, 1750.0, 30.0, &mut |_, _| {}, &mut |_| {}));
        assert_eq!(deck.active, 1);
        assert!(!deck.volume.active);

        // ...and volume buttons on other layers never summon it.
        deck.touch_down(2, 1, 1500.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(2, &mut |_, _| {});
        assert!(!deck.volume.active);
        assert!(!deck.volume_drag_start(3, 700.0, 30.0, 2008, 60, &mut |_, _| {}));

        // Back home it summons again; a reload replaces it with a fresh deck.
        assert!(deck.touch_down(4, 0, 100.0, 30.0, &mut |_, _| {}, &mut |_| {}));
        assert_eq!(deck.active, deck.home);
        deck.replace(volume_deck(), &mut |_, _| {});
        assert!(!deck.volume.active);

        // Summoning once more, input release (cancel, display sleep, device
        // removal) collapses it and drops any drag.
        deck.touch_down(5, 2, 1260.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_motion(5, 1280.0, 30.0, 2008, 60, &mut |_, _| {});
        assert!(deck.volume.drag.is_some());
        deck.release_all(&mut |_, _| {});
        assert!(!deck.volume.active);
        assert!(deck.volume.drag.is_none());
    }

    #[test]
    fn a_cancelled_drag_leaves_every_key_released() {
        let mut deck = volume_deck();
        let mut events = Vec::new();
        deck.touch_down(
            0,
            2,
            1260.0,
            30.0,
            &mut |k, a| events.push((k.to_vec(), a)),
            &mut |_| {},
        );
        deck.touch_motion(0, 1300.0, 30.0, 2008, 60, &mut |k, a| {
            events.push((k.to_vec(), a))
        }); // convert to drag
        deck.release_all(&mut |_, _| {}); // cancelled mid-drag
        assert!(deck.volume.drag.is_none());

        let mut held: Vec<Key> = Vec::new();
        for (keys, active) in &events {
            let key = keys[0];
            if *active {
                assert!(!held.contains(&key), "{key:?} pressed twice");
                held.push(key);
            } else {
                assert!(held.contains(&key), "{key:?} released unpressed");
                held.retain(|k| *k != key);
            }
        }
        assert!(held.is_empty());
    }

    #[test]
    fn legacy_double_fn_swap_collapses_the_slider_before_the_home_changes() {
        let mut deck = LayerDeck::new(
            vec![
                (
                    "media".into(),
                    FunctionLayer::with_config(vec![
                        keys("Mute", &[Key::Mute]),
                        keys("V-", &[Key::VolumeDown]),
                        keys("V+", &[Key::VolumeUp]),
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
        assert!(deck.volume.active);
        assert_eq!(deck.volume.region, Some((0, 2)));

        // The legacy double press swaps the home to the one-button primary;
        // the slider collapses while its region still matches the old home,
        // so releasing Fn and the linger deadline cannot touch stale indices.
        assert!(!deck.fn_event(true, true, &mut emit));
        assert!(deck.fn_event(false, false, &mut emit));
        assert_eq!(deck.home, 1);
        assert_eq!(deck.active, 1);
        assert!(!deck.volume.active);
        assert!(deck.layers[0]
            .buttons
            .iter()
            .all(|(_, button)| button.changed));
        assert_eq!(deck.volume.deadline, None);
        assert!(!deck.volume_refresh(Instant::now() + Duration::from_secs(60)));
    }

    #[test]
    fn decks_without_a_home_volume_cluster_never_summon_a_slider() {
        let mut deck = named_deck();
        deck.touch_down(0, 0, 330.0, 30.0, &mut |_, _| {}, &mut |_| {});
        deck.touch_up(0, &mut |_, _| {});
        assert!(!deck.volume.active);
        assert!(!deck.volume_drag_start(1, 500.0, 30.0, 2008, 60, &mut |_, _| {}));
    }
}
