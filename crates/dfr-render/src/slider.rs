//! One transient level control: ranges/adapters vary; drawing and interaction do not.
use crate::{
    appearance::{AccentPaint, ButtonAccent, Palette},
    config::ButtonAction,
    level::{Request, State, MAX_VOLUME_PERCENT},
    rounded_rect, BUTTON_COLOR_ACTIVE, BUTTON_COLOR_INACTIVE, BUTTON_SPACING_PX,
};
use cairo::Context;
use std::{
    io,
    time::{Duration, Instant},
};

pub const LINGER: Duration = Duration::from_millis(2500);
pub const DRAG_THRESHOLD_PX: f64 = 8.0;
const GLYPH_ZONE_PX: f64 = 28.0;
/// How often the closed slider's mute watch re-reads the mixer. The desktop
/// receiver answers one request per connection with real mixer work behind
/// it, so the watch polls slowly and refreshes promptly only on the events
/// that change what the closed bar shows: startup and the slider's collapse.
pub const MUTE_PROBE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Volume,
    Brightness,
}

impl Kind {
    pub fn for_action(action: &ButtonAction) -> Option<Self> {
        match action {
            ButtonAction::VolumeSlider => Some(Self::Volume),
            ButtonAction::BrightnessSlider => Some(Self::Brightness),
            _ => None,
        }
    }

    pub fn range(self) -> (u8, u8) {
        match self {
            Self::Volume => (0, MAX_VOLUME_PERCENT),
            Self::Brightness => (1, 100),
        }
    }

    fn accent(self) -> ButtonAccent {
        match self {
            Self::Volume => ButtonAccent::Cyan,
            Self::Brightness => ButtonAccent::Amber,
        }
    }

    pub fn level_at(self, x: f64, left: f64, right: f64) -> u8 {
        let fraction = ((x - left) / (right - left).max(1.0)).clamp(0.0, 1.0);
        let (min, max) = self.range();
        min + (fraction * (max - min) as f64).round() as u8
    }

    fn fraction(self, level: u8) -> f64 {
        let (min, max) = self.range();
        (level.saturating_sub(min) as f64 / (max - min) as f64).clamp(0.0, 1.0)
    }
}

pub struct Drag {
    pub slot: u32,
}

pub struct Overlay {
    pub active: bool,
    pub kind: Kind,
    pub level: u8,
    pub muted: bool,
    pub region: Option<(usize, usize)>,
    pub deadline: Option<Instant>,
    pub drag: Option<Drag>,
    pub layer: usize,
    pub confirmed: bool,
    pub revision: u64,
    pub opened_revision: u64,
    pub applied_revision: Option<u64>,
    pub pixel_shift_width: u64,
    pub pixel_shift_x: f64,
    pub requests: Vec<(u64, Kind, Request)>,
}

impl Default for Overlay {
    fn default() -> Self {
        Self {
            active: false,
            kind: Kind::Volume,
            level: 0,
            muted: false,
            region: None,
            deadline: None,
            drag: None,
            layer: 0,
            confirmed: false,
            revision: rand::random(),
            opened_revision: 0,
            applied_revision: None,
            pixel_shift_width: 0,
            pixel_shift_x: 0.0,
            requests: Vec::new(),
        }
    }
}

impl Overlay {
    pub fn open(&mut self, kind: Kind, layer: usize, region: (usize, usize)) {
        if !self.active || self.kind != kind || self.layer != layer || self.region != Some(region) {
            self.confirmed = false;
            self.opened_revision = self.revision.wrapping_add(1);
            self.applied_revision = None;
            self.drag = None;
        }
        self.kind = kind;
        self.active = true;
        self.layer = layer;
        self.region = Some(region);
        self.queue(Request::Get);
        self.deadline = Some(Instant::now() + LINGER);
    }

    fn queue(&mut self, request: Request) {
        self.revision = self.revision.wrapping_add(1);
        self.requests.push((self.revision, self.kind, request));
    }

    pub fn set_level(&mut self, target: u8) {
        let (min, max) = self.kind.range();
        self.queue(Request::Set(target.clamp(min, max)));
        // The thumb continues to show confirmed hardware state until readback.
    }

    pub fn begin_drag(&mut self, slot: u32, x: f64, geometry: &Geometry) -> bool {
        if !self.active || self.drag.is_some() {
            return false;
        }
        self.drag = Some(Drag { slot });
        self.drag_motion(slot, x, geometry)
    }

    pub fn drag_motion(&mut self, slot: u32, x: f64, geometry: &Geometry) -> bool {
        if !self.active || !self.drag.as_ref().is_some_and(|drag| drag.slot == slot) {
            return false;
        }
        self.set_level(
            self.kind
                .level_at(x, geometry.track_left(), geometry.track_right()),
        );
        self.deadline = Some(Instant::now() + LINGER);
        true
    }

    pub fn release_drag(&mut self, slot: u32) -> bool {
        if !self.drag.as_ref().is_some_and(|drag| drag.slot == slot) {
            return false;
        }
        self.drag = None;
        self.deadline = Some(Instant::now() + LINGER);
        true
    }

    pub fn collapse(&mut self) -> bool {
        let was_active = self.active;
        self.active = false;
        self.drag = None;
        self.deadline = None;
        was_active
    }

    pub fn reply(&mut self, revision: u64, kind: Kind, result: io::Result<State>) -> bool {
        if !self.active
            || kind != self.kind
            || revision < self.opened_revision
            || revision > self.revision
            || self.applied_revision.is_some_and(|last| revision <= last)
        {
            return false;
        }
        match result {
            Ok(state) => {
                self.applied_revision = Some(revision);
                self.confirmed = true;
                self.level = state.percent;
                self.muted = kind == Kind::Volume && state.muted;
                true
            }
            Err(error) if revision == self.revision => {
                eprintln!("{kind:?} handoff failed: {error}");
                self.collapse();
                true
            }
            Err(_) => false,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Render {
    pub first: usize,
    pub last: usize,
    pub level: u8,
    pub muted: bool,
    pub kind: Kind,
}

pub struct Geometry {
    pub left: f64,
    pub right: f64,
}

impl Geometry {
    pub fn new(
        width: i32,
        count: usize,
        start: usize,
        end: usize,
        shift_width: u64,
        shift_x: f64,
    ) -> Self {
        let button_width = ((width - shift_width as i32) - BUTTON_SPACING_PX * (count - 1) as i32)
            as f64
            / count as f64;
        let stride = button_width + BUTTON_SPACING_PX as f64;
        let left = (start as f64 * stride).floor() + shift_x + (shift_width / 2) as f64;
        let region_width = button_width + ((end - start - 1) as f64 * stride).floor();
        Self {
            left,
            right: left + region_width,
        }
    }

    pub fn contains(&self, x: f64, y: f64, height: f64) -> bool {
        x >= self.left && x <= self.right && y >= height * 0.1 && y <= height * 0.9
    }

    pub fn track_left(&self) -> f64 {
        self.left + GLYPH_ZONE_PX + 2.0
    }
    pub fn track_right(&self) -> f64 {
        self.right - 2.0
    }

    pub fn draw(
        &self,
        c: &Context,
        height: f64,
        pixel_shift_y: f64,
        render: Render,
        palette: &Palette,
    ) {
        // The track and fill replace the cluster, macOS-style: a slim
        // rounded bar across the region with a fill up to the level,
        // anchored left by the selected control's glyph.
        let track_left = self.track_left();
        let track_right = self.track_right();
        let track_height = 14.0;
        let track_top = (height - track_height) / 2.0 + pixel_shift_y;
        c.set_source_rgb(
            BUTTON_COLOR_INACTIVE,
            BUTTON_COLOR_INACTIVE,
            BUTTON_COLOR_INACTIVE,
        );
        rounded_rect(
            c,
            track_left,
            track_top,
            track_right - track_left,
            track_height,
            track_height / 2.0,
        );
        c.fill().unwrap();

        let frac = render.kind.fraction(render.level);
        let fill_width = (track_right - track_left - 2.0) * frac;
        if fill_width > 0.0 {
            let fill_height = track_height - 2.0;
            if render.muted {
                c.set_source_rgb(
                    BUTTON_COLOR_ACTIVE,
                    BUTTON_COLOR_ACTIVE,
                    BUTTON_COLOR_ACTIVE,
                );
            } else {
                render.kind.accent().foreground(c, palette);
            }
            rounded_rect(
                c,
                track_left + 1.0,
                track_top + 1.0,
                fill_width,
                fill_height,
                fill_height / 2.0,
            );
            c.fill().unwrap();
        }

        // The glyph identifies the selected control; muted volume adds a slash.
        let mid = track_top + track_height / 2.0;
        let glyph_x = self.left + 8.0;
        c.set_source_rgb(1.0, 1.0, 1.0);
        if render.kind == Kind::Brightness {
            let center = glyph_x + 6.0;
            c.arc(center, mid, 4.0, 0.0, std::f64::consts::TAU);
            c.fill().unwrap();
            c.set_line_width(1.5);
            for ray in 0..8 {
                let angle = ray as f64 * std::f64::consts::TAU / 8.0;
                c.move_to(center + 7.0 * angle.cos(), mid + 7.0 * angle.sin());
                c.line_to(center + 10.0 * angle.cos(), mid + 10.0 * angle.sin());
            }
            c.stroke().unwrap();
        } else {
            c.rectangle(glyph_x, mid - 3.0, 4.0, 6.0);
            c.fill().unwrap();
            c.new_sub_path();
            c.move_to(glyph_x + 4.0, mid - 3.0);
            c.line_to(glyph_x + 12.0, mid - 7.0);
            c.line_to(glyph_x + 12.0, mid + 7.0);
            c.line_to(glyph_x + 4.0, mid + 3.0);
            c.close_path();
            c.fill().unwrap();
        }
        if render.muted {
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
    }
}

/// Persistent mute state for the closed slider. The shared overlay tracks
/// mute only while it is open, so without the watch nothing on the bar
/// would show a mute toggle that happens with the slider closed. The watch
/// probes the volume receiver on a slow cadence, pauses while the slider
/// performs its own reads, and re-probes right after the collapse. Its
/// request revisions come from their own counter: the main loop routes
/// every volume reply by exact revision, to the watch first and into the
/// overlay only when the watch does not own it.
pub struct MuteWatch {
    muted: Option<bool>,
    revision: u64,
    outstanding: Option<u64>,
    next_at: Instant,
}

impl Default for MuteWatch {
    fn default() -> Self {
        Self {
            muted: None,
            revision: rand::random(),
            outstanding: None,
            next_at: Instant::now(),
        }
    }
}

impl MuteWatch {
    /// The last confirmed mute state; `None` before the first read.
    pub fn muted(&self) -> Option<bool> {
        self.muted
    }

    /// Whether this revision belongs to the outstanding probe.
    pub fn owns(&self, revision: u64) -> bool {
        self.outstanding == Some(revision)
    }

    /// The due probe request, when the slider is closed, no probe is
    /// outstanding, and the cadence allows one.
    pub fn tick(&mut self, now: Instant, slider_open: bool) -> Option<(u64, Request)> {
        if slider_open || self.outstanding.is_some() || now < self.next_at {
            return None;
        }
        self.revision = self.revision.wrapping_add(1);
        self.outstanding = Some(self.revision);
        Some((self.revision, Request::Get))
    }

    /// Ask for a fresh read as soon as possible; used when the slider
    /// collapses and the closed bar becomes the mute mark's canvas again.
    pub fn settle(&mut self, now: Instant) {
        self.next_at = now;
    }

    /// Consume the reply to the outstanding probe; other revisions pass
    /// through untouched. Returns whether the confirmed mute state
    /// changed. A failed read keeps the last confirmed state and only
    /// schedules the next probe, so a dead receiver never invents a state.
    pub fn reply(&mut self, revision: u64, result: io::Result<State>, now: Instant) -> bool {
        if !self.owns(revision) {
            return false;
        }
        self.outstanding = None;
        self.next_at = now + MUTE_PROBE_INTERVAL;
        match result {
            Ok(state) => {
                let changed = self.muted != Some(state.muted);
                self.muted = Some(state.muted);
                changed
            }
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn state(percent: u8, muted: bool) -> io::Result<State> {
        Ok(State { percent, muted })
    }

    #[test]
    fn watch_probes_on_cadence_only_while_the_slider_is_closed() {
        let mut watch = MuteWatch::default();
        let start = Instant::now();
        let (revision, request) = watch.tick(start, false).unwrap();
        assert_eq!(request, Request::Get);
        assert!(watch.owns(revision));
        // One outstanding probe: nothing further until it is answered.
        assert!(watch.tick(start, false).is_none());
        assert!(watch.reply(revision, state(40, false), start));
        // An open slider pauses the watch entirely.
        assert!(watch.tick(start, true).is_none());
        assert!(watch.tick(start + MUTE_PROBE_INTERVAL, true).is_none());
        // The next probe waits out the interval from the reply.
        assert!(watch
            .tick(
                start + MUTE_PROBE_INTERVAL - Duration::from_millis(1),
                false
            )
            .is_none());
        let (next, _) = watch
            .tick(start + MUTE_PROBE_INTERVAL, false)
            .expect("interval elapsed without a due probe");
        assert_ne!(next, revision);
        assert!(watch.owns(next));
        assert!(watch.tick(start + MUTE_PROBE_INTERVAL, false).is_none());
    }

    #[test]
    fn watch_reports_only_real_mute_changes_and_keeps_state_on_errors() {
        let mut watch = MuteWatch::default();
        let start = Instant::now();
        let (revision, _) = watch.tick(start, false).unwrap();
        // The first confirmed read changes the watch's knowledge from
        // unknown to unmuted; the deck dedupes what the buttons show.
        assert!(watch.reply(revision, state(40, false), start));
        assert_eq!(watch.muted(), Some(false));
        // An unknown or already-answered revision is never consumed.
        assert!(!watch.reply(revision, state(40, true), start));
        assert_eq!(watch.muted(), Some(false));
        let (revision, _) = watch.tick(start + MUTE_PROBE_INTERVAL, false).unwrap();
        assert!(!watch.reply(revision, state(40, false), start));
        assert_eq!(watch.muted(), Some(false));
        let (revision, _) = watch.tick(start + MUTE_PROBE_INTERVAL, false).unwrap();
        assert!(watch.reply(revision, state(40, true), start));
        assert_eq!(watch.muted(), Some(true));
        // A failed read keeps the last confirmed state and still schedules
        // the next probe.
        let (revision, _) = watch.tick(start + MUTE_PROBE_INTERVAL, false).unwrap();
        assert!(!watch.reply(
            revision,
            Err(io::Error::other("receiver unavailable")),
            start
        ));
        assert_eq!(watch.muted(), Some(true));
        assert!(watch.tick(start + MUTE_PROBE_INTERVAL, false).is_some());
    }

    #[test]
    fn collapse_settles_the_watch_into_an_immediate_probe() {
        let mut watch = MuteWatch::default();
        let start = Instant::now();
        let (revision, _) = watch.tick(start, false).unwrap();
        watch.reply(revision, state(40, false), start);
        assert!(watch.tick(start, false).is_none());
        watch.settle(start);
        let (revision, request) = watch.tick(start, false).unwrap();
        assert_eq!(request, Request::Get);
        watch.reply(revision, state(40, true), start);
        assert_eq!(watch.muted(), Some(true));
    }
}
