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
    os::{
        fd::{AsFd, AsRawFd},
        unix::{fs::OpenOptionsExt, io::OwnedFd},
    },
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use udev::MonitorBuilder;

mod backlight;
mod config;
mod display;
mod fonts;
mod pixel_shift;

use crate::config::ConfigManager;
use backlight::BacklightManager;
use config::{ButtonAction, ButtonConfig, Config};
use display::DrmBackend;
use pixel_shift::{PixelShiftManager, PIXEL_SHIFT_WIDTH_PX};

const BUTTON_SPACING_PX: i32 = 16;
const BUTTON_COLOR_INACTIVE: f64 = 0.200;
const BUTTON_COLOR_ACTIVE: f64 = 0.400;
const DEFAULT_ICON_SIZE: i32 = 48;
const TIMEOUT_MS: i32 = 10 * 1000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum BatteryState {
    NotCharging,
    Charging,
    Low,
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
    Battery(String, BatteryIconMode, BatteryImages),
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

fn find_battery_device() -> Option<String> {
    let power_supply_path = "/sys/class/power_supply";
    if let Ok(entries) = fs::read_dir(power_supply_path) {
        for entry in entries.flatten() {
            let dev_path = entry.path();
            let type_path = dev_path.join("type");
            if let Ok(typ) = fs::read_to_string(&type_path) {
                if typ.trim() == "Battery" {
                    if let Some(name) = dev_path.file_name().and_then(|n| n.to_str()) {
                        return Some(name.to_string());
                    }
                }
            }
        }
    }
    None
}

fn get_battery_state(battery: &str) -> (u32, BatteryState) {
    let status_path = format!("/sys/class/power_supply/{}/status", battery);
    let status = fs::read_to_string(&status_path).unwrap_or_else(|_| "Unknown".to_string());

    let capacity = {
        #[cfg(target_arch = "x86_64")]
        {
            let charge_now_path = format!("/sys/class/power_supply/{}/charge_now", battery);
            let charge_full_path = format!("/sys/class/power_supply/{}/charge_full", battery);
            let charge_now = fs::read_to_string(&charge_now_path)
                .ok()
                .and_then(|s| s.trim().parse::<f64>().ok());
            let charge_full = fs::read_to_string(&charge_full_path)
                .ok()
                .and_then(|s| s.trim().parse::<f64>().ok());
            match (charge_now, charge_full) {
                (Some(now), Some(full)) if full > 0.0 => ((now / full) * 100.0).round() as u32,
                _ => 100,
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let capacity_path = format!("/sys/class/power_supply/{}/capacity", battery);
            fs::read_to_string(&capacity_path)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(100)
        }
    };

    let status = match status.trim() {
        "Charging" | "Full" => BatteryState::Charging,
        "Discharging" if capacity < 10 => BatteryState::Low,
        _ => BatteryState::NotCharging,
    };
    (capacity, status)
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
            if let Some(battery) = find_battery_device() {
                Button::new_battery(cfg.action, battery, battery_mode, cfg.theme)
            } else {
                Button::new_text("Battery N/A".to_string(), cfg.action)
            }
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
        battery: String,
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
                battery,
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
                match item {
                    Item::Numeric(Numeric::Second, _)
                    | Item::Numeric(Numeric::Nanosecond, _)
                    | Item::Numeric(Numeric::Timestamp, _) => true,
                    _ => false,
                }
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
                let (capacity, state) = get_battery_state(battery);
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
                let percent_str = format!("{:.0}%", capacity);
                let extents = c.text_extents(&percent_str).unwrap();
                let mut width = extents.width();
                let mut text_offset = 0;
                if let Some(svg) = icon {
                    if !battery_mode.should_draw_text() {
                        width = DEFAULT_ICON_SIZE as f64;
                    } else {
                        width += DEFAULT_ICON_SIZE as f64;
                    }
                    text_offset = DEFAULT_ICON_SIZE;
                    let x = button_left_edge + (button_width as f64 / 2.0 - width / 2.0).round();
                    let y = y_shift + ((height as f64 - DEFAULT_ICON_SIZE as f64) / 2.0).round();

                    svg.render_document(
                        c,
                        &Rectangle::new(x, y, DEFAULT_ICON_SIZE as f64, DEFAULT_ICON_SIZE as f64),
                    )
                    .unwrap();
                }
                if battery_mode.should_draw_text() {
                    c.move_to(
                        button_left_edge
                            + (button_width as f64 / 2.0 - width / 2.0 + text_offset as f64)
                                .round(),
                        y_shift + (height as f64 / 2.0 + extents.height() / 2.0).round(),
                    );
                    c.show_text(&percent_str).unwrap();
                }
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
            let (_, state) = get_battery_state(battery);
            match state {
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
    displays_battery: bool,
    buttons: Vec<(usize, Button)>,
    virtual_button_count: usize,
    faster_refresh: bool,
}

impl FunctionLayer {
    fn with_config(cfg: Vec<ButtonConfig>) -> FunctionLayer {
        if cfg.is_empty() {
            panic!("Invalid configuration, layer has 0 buttons");
        }

        let mut virtual_button_count = 0;
        let displays_time = cfg.iter().any(|cfg| cfg.time.is_some());
        let displays_battery = cfg.iter().any(|cfg| cfg.battery.is_some());
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
            displays_battery,
            buttons,
            virtual_button_count,
            faster_refresh,
        }
    }
    fn draw(
        &mut self,
        config: &Config,
        width: i32,
        height: i32,
        surface: &Surface,
        pixel_shift: (f64, f64),
        complete_redraw: bool,
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

        for i in 0..self.buttons.len() {
            let end = if i + 1 < self.buttons.len() {
                self.buttons[i + 1].0
            } else {
                self.virtual_button_count
            };
            let (start, button) = &mut self.buttons[i];
            let start = *start;

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

        modified_regions
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
}

// Input state belongs to the deck, so transitions always release the old layer
// before changing the displayed layer or discarding its buttons on reload.
pub struct LayerDeck {
    layers: Vec<FunctionLayer>,
    names: HashMap<String, usize>,
    home: usize,
    fn_layer: Option<usize>,
    legacy: bool,
    active: usize,
    fn_pressed: bool,
    fn_return: Option<usize>,
    touches: HashMap<u32, TouchState>,
}

impl LayerDeck {
    fn new(
        layers: Vec<(String, FunctionLayer)>,
        home: usize,
        fn_layer: Option<usize>,
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
            legacy,
            active: home,
            fn_pressed: false,
            fn_return: None,
            touches: HashMap::new(),
        }
    }

    fn current(&self) -> &FunctionLayer {
        &self.layers[self.active]
    }

    fn current_mut(&mut self) -> &mut FunctionLayer {
        &mut self.layers[self.active]
    }

    fn release_all(&mut self, emit_keys: &mut impl FnMut(&[Key], bool)) {
        for layer in &mut self.layers {
            for (_, button) in &mut layer.buttons {
                button.set_active(false, emit_keys);
            }
        }
        self.touches.clear();
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
            if self.legacy && double_press {
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
        emit_keys: &mut impl FnMut(&[Key], bool),
    ) -> bool {
        self.touch_up(slot, emit_keys);
        if matches!(self.current().buttons[button].1.image, ButtonImage::Spacer) {
            return false;
        }
        if let ButtonAction::SwitchLayer(name) = &self.current().buttons[button].1.action {
            let name = name.clone();
            return self.switch_named(&name, emit_keys);
        }
        self.touches.insert(
            slot,
            TouchState {
                button,
                inside: true,
            },
        );
        self.update_button(button, emit_keys);
        false
    }

    fn touch_motion(&mut self, slot: u32, inside: bool, emit_keys: &mut impl FnMut(&[Key], bool)) {
        if let Some(touch) = self.touches.get_mut(&slot) {
            touch.inside = inside;
            let button = touch.button;
            self.update_button(button, emit_keys);
        }
    }

    fn touch_up(&mut self, slot: u32, emit_keys: &mut impl FnMut(&[Key], bool)) {
        if let Some(touch) = self.touches.remove(&slot) {
            self.update_button(touch.button, emit_keys);
        }
    }

    fn replace(&mut self, new: Self, emit_keys: &mut impl FnMut(&[Key], bool)) {
        self.release_all(emit_keys);
        *self = new;
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
    let mut last_fn_press: Option<Instant> = None;

    // drop privileges to input and video group
    let groups = ["input", "video"];

    PrivDrop::default()
        .user("nobody")
        .group_list(&groups)
        .apply()
        .unwrap_or_else(|e| panic!("Failed to drop privileges: {}", e));

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
            last_fn_press = None;
            needs_complete_redraw = true;
        }

        let now = Local::now();
        let ms_left = ((60 - now.second()) * 1000) as i32;
        let mut next_timeout_ms = min(ms_left, TIMEOUT_MS);

        if cfg.enable_pixel_shift {
            let (pixel_shift_needs_redraw, pixel_shift_next_timeout_ms) = pixel_shift.update();
            if pixel_shift_needs_redraw {
                needs_complete_redraw = true;
            }
            next_timeout_ms = min(next_timeout_ms, pixel_shift_next_timeout_ms);
        }

        let current_ts = if layers.current().faster_refresh {
            Local::now().second()
        } else {
            Local::now().minute()
        };
        if layers.current().displays_time && (current_ts != last_redraw_ts) {
            needs_complete_redraw = true;
            last_redraw_ts = current_ts;
        }
        if layers.current().displays_battery {
            for button in &mut layers.current_mut().buttons {
                if let ButtonImage::Battery(_, _, _) = button.1.image {
                    button.1.changed = true;
                }
            }
        }

        if needs_complete_redraw || layers.current().buttons.iter().any(|b| b.1.changed) {
            let shift = if cfg.enable_pixel_shift {
                pixel_shift.get()
            } else {
                (0.0, 0.0)
            };
            let clips = layers.current_mut().draw(
                &cfg,
                width as i32,
                height as i32,
                &surface,
                shift,
                needs_complete_redraw,
            );
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

        _ = udev_monitor.iter().last();

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
                            if let Some(btn) = layers.current().hit(width, height, x, y, None) {
                                needs_complete_redraw |=
                                    layers.touch_down(dn.seat_slot(), btn, &mut |keys, active| {
                                        toggle_keys(&mut uinput, keys, active as i32)
                                    });
                            }
                        }
                        TouchEvent::Motion(mtn) => {
                            let Some(touch) = layers.touches.get(&mtn.seat_slot()) else {
                                continue;
                            };

                            let x = mtn.x_transformed(width as u32);
                            let y = mtn.y_transformed(height as u32);
                            let hit = layers
                                .current()
                                .hit(width, height, x, y, Some(touch.button))
                                .is_some();
                            layers.touch_motion(mtn.seat_slot(), hit, &mut |keys, active| {
                                toggle_keys(&mut uinput, keys, active as i32)
                            });
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
            false,
        )
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
        };
        let (width, height) = (2008, 60);
        let mut surface = ImageSurface::create(Format::ARgb32, height, width).unwrap();
        deck.current_mut()
            .draw(&config, width, height, &surface, (0.0, 0.0), true);
        let home_image = surface.data().unwrap().to_vec();

        // The center navigation button on home opens the function bar.
        let button = deck
            .current()
            .hit(width as u16, height as u16, (width / 2) as f64, 30.0, None)
            .unwrap();
        assert!(deck.touch_down(0, button, &mut |_, _| {}));
        deck.current_mut()
            .draw(&config, width, height, &surface, (0.0, 0.0), true);
        assert_ne!(&*surface.data().unwrap(), home_image.as_slice());

        // The first button on the function bar returns to the same home image.
        let button = deck
            .current()
            .hit(width as u16, height as u16, 100.0, 30.0, None)
            .unwrap();
        assert!(deck.touch_down(1, button, &mut |_, _| {}));
        deck.current_mut()
            .draw(&config, width, height, &surface, (0.0, 0.0), true);
        assert_eq!(&*surface.data().unwrap(), home_image.as_slice());
    }

    #[test]
    fn on_bar_navigation_releases_keys_ignores_old_touches_and_returns_home() {
        let mut deck = named_deck();
        let mut events = Vec::new();
        let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
        assert!(!deck.touch_down(0, 0, &mut emit));
        assert!(deck.touch_down(1, 1, &mut emit));
        assert_eq!(deck.active, 1);
        assert!(deck.touches.is_empty());
        deck.touch_motion(0, true, &mut emit);
        deck.touch_up(0, &mut emit);
        deck.touch_motion(1, true, &mut emit);
        deck.touch_up(1, &mut emit);
        assert!(deck.touch_down(2, 0, &mut emit));
        assert_eq!(deck.active, deck.home);
        assert!(deck.touch_down(3, 2, &mut emit));
        assert_eq!(deck.active, 2);
        assert!(deck.touch_down(4, 0, &mut emit));
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
        deck.touch_down(0, 1, &mut emit);
        assert!(deck.fn_event(true, false, &mut emit));
        assert_eq!(deck.active, 1);
        deck.touch_down(1, 1, &mut emit);
        assert!(deck.fn_event(false, false, &mut emit));
        assert_eq!(deck.active, 2);
        assert!(deck.fn_event(true, false, &mut emit));
        assert!(deck.touch_down(2, 0, &mut emit)); // Home while Fn is held
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
                true,
            );
            let mut events = Vec::new();
            let mut emit = |keys: &[Key], active| events.push((keys.to_vec(), active));
            deck.touch_down(0, 0, &mut emit);
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
        deck.touch_down(0, 1, &mut emit);
        let replacement = LayerDeck::new(
            vec![(
                "new-home".into(),
                FunctionLayer::with_config(vec![keys("Volume", &[Key::VolumeUp])]),
            )],
            0,
            None,
            false,
        );
        deck.replace(replacement, &mut emit);
        assert_eq!(deck.active, deck.home);
        assert_eq!(deck.layers.len(), 1);
        assert!(deck.touches.is_empty());
        deck.touch_motion(0, true, &mut emit);
        deck.touch_up(0, &mut emit);
        assert!(!deck.fn_event(false, false, &mut emit));
        deck.touch_down(1, 0, &mut emit);
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
        deck.touch_down(0, 0, &mut emit);
        deck.touch_motion(0, false, &mut emit);
        deck.touch_motion(0, true, &mut emit);
        deck.touch_up(0, &mut emit);
        deck.touch_motion(0, true, &mut emit);
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
        deck.touch_down(0, 0, &mut emit);
        deck.touch_down(1, 0, &mut emit);
        deck.touch_motion(0, false, &mut emit);
        deck.touch_up(1, &mut emit);
        assert!(!deck.current().buttons[0].1.active);
        deck.touch_motion(0, true, &mut emit);
        deck.release_all(&mut emit); // cancel, device removal, or display sleeping
        deck.touch_motion(0, true, &mut emit);
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
        deck.touch_down(0, 0, &mut emit);
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
}
