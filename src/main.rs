//! The tiny-dfr daemon: own the Touch Bar, drive input, render the
//! deck. Everything that draws lives in the `dfr-render` crate - the
//! deck, its buttons, and every widget - and this binary owns the
//! machine plumbing around it: the DRM display, libinput, uinput key
//! emission, the inotify config watcher, and the event loop that
//! schedules redraws and widget ticks.

use cairo::{Context, Format, ImageSurface};
use chrono::{Local, Timelike};
use drm::control::ClipRect;
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
    fs::{File, OpenOptions},
    os::{
        fd::{AsFd, AsRawFd, OwnedFd},
        unix::fs::OpenOptionsExt,
    },
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use udev::MonitorBuilder;

mod backlight;
mod command;
mod config;
mod display;
mod level_control;
mod pixel_shift;

use crate::config::ConfigManager;
use backlight::BacklightManager;
use command::CommandRunner;
use dfr_render::appearance::Palette;
use dfr_render::now_playing::NowPlaying;
use dfr_render::pet::{Bar as PetBar, PetManager};
use dfr_render::slider::Kind;
use dfr_render::themes::ThemeRequest;
use dfr_render::waveform::{MicLevels, MonitorLevels};
use dfr_render::BatteryMonitor;
use display::DrmBackend;
use level_control::LevelControl;
use pixel_shift::{PixelShiftManager, PIXEL_SHIFT_WIDTH_PX};

const TIMEOUT_MS: i32 = 10 * 1000;

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
    // `tiny-dfr check [file]` validates a configuration offline - the
    // exact parse/merge/validate path the daemon runs, plus icon and
    // font resolution - without opening the Touch Bar. Scripts, CI,
    // and editors run it before installing a candidate file; the
    // daemon never needs to see an invalid one.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("check") {
        std::process::exit(run_check(&args[2..]));
    }
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

/// The offline validator behind `tiny-dfr check`. Exit codes: 0 valid,
/// 1 diagnostics printed, 2 usage error.
fn run_check(args: &[String]) -> i32 {
    let usage = || {
        eprintln!("usage: tiny-dfr check [--base PATH] [--icon-root DIR] [--width N] [FILE]");
        eprintln!("  FILE defaults to /etc/tiny-dfr/config.toml, validated over the base config");
        eprintln!(
            "  --base overrides the base configuration (default /usr/share/tiny-dfr/config.toml)"
        );
        eprintln!("  --icon-root replaces the fixed /usr/share/tiny-dfr icon directory");
        eprintln!(
            "  --width synthesizes the layout for that bar width (default 2008; 2170 adds esc)"
        );
    };
    let mut base: Option<PathBuf> = None;
    let mut file: Option<PathBuf> = None;
    let mut icon_root: Option<PathBuf> = None;
    let mut width: u16 = 2008;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let missing = |flag: &str| -> ! {
            eprintln!("missing value for {flag}");
            usage();
            std::process::exit(2)
        };
        match arg.as_str() {
            "--base" => {
                base = Some(PathBuf::from(
                    iter.next().unwrap_or_else(|| missing("--base")),
                ))
            }
            "--icon-root" => {
                icon_root = Some(PathBuf::from(
                    iter.next().unwrap_or_else(|| missing("--icon-root")),
                ))
            }
            "--width" => match iter
                .next()
                .unwrap_or_else(|| missing("--width"))
                .parse::<u16>()
            {
                Ok(parsed) => width = parsed,
                Err(_) => {
                    eprintln!("--width must be a number");
                    usage();
                    std::process::exit(2);
                }
            },
            "-h" | "--help" => {
                usage();
                return 0;
            }
            other if other.starts_with('-') => {
                eprintln!("unknown option {other}");
                usage();
                return 2;
            }
            other => match file {
                None => file = Some(PathBuf::from(other)),
                Some(_) => {
                    eprintln!("only one configuration file may be given");
                    usage();
                    return 2;
                }
            },
        }
    }
    let base = base.unwrap_or_else(|| PathBuf::from(config::BASE_CFG_PATH));
    let file = file.unwrap_or_else(|| PathBuf::from(config::USER_CFG_PATH));
    let share = icon_root.unwrap_or_else(|| PathBuf::from("/usr/share/tiny-dfr"));
    let search = dfr_config::IconSearch::new(["/etc/tiny-dfr", share.to_str().unwrap_or("")]);
    let diagnostics = dfr_config::check::check_user_config(&base, &file, width, &search);
    if diagnostics.is_empty() {
        println!(
            "{}: OK (validated against {})",
            file.display(),
            base.display()
        );
        return 0;
    }
    for diagnostic in &diagnostics {
        eprintln!("{}: {diagnostic}", file.display());
    }
    1
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

        // The cycle button's applied-theme label reverts on its own
        // timer; like every other widget deadline it only ever narrows
        // the epoll timeout.
        let (_, theme_flash_timeout_ms) = layers.theme_flash_tick(Instant::now());
        next_timeout_ms = min(next_timeout_ms, theme_flash_timeout_ms);

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
                                // Theme buttons apply through here: the
                                // palette, day cycle, and persistence live
                                // outside the deck, and a successful apply
                                // returns the applied name and palette so
                                // the deck can retint its per-build colors.
                                // Any theme tap forces a complete redraw,
                                // so the whole strip repaints in the new
                                // colors on the very next frame.
                                let mut apply_theme =
                                    |request: ThemeRequest| -> Option<(String, Palette)> {
                                        let name = match request {
                                            ThemeRequest::Next => cfg.themes.cycle_next()?,
                                            ThemeRequest::Named(name) => name.to_owned(),
                                        };
                                        if !cfg.themes.apply(&name) {
                                            eprintln!("theme {name:?} is not defined");
                                            return None;
                                        }
                                        cfg.palette = cfg.themes.palette();
                                        cfg.day_cycle = cfg.themes.day_cycle(&cfg.global_day_cycle);
                                        if let Err(err) = cfg_mgr.save_theme(&name) {
                                            eprintln!(
                                                "could not persist the active theme {name:?}: {err}"
                                            );
                                        }
                                        Some((name, cfg.palette))
                                    };
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
                                    &mut apply_theme,
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
