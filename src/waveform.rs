//! Live microphone input visualization for the recording layer.
//!
//! A button configured with `Waveform = true` renders the sound actually
//! coming in beside a recording duration timer, in place of the old static
//! `● Recording` label: by default a single drawn line - flat at rest,
//! rippling into a smooth wave while speaking - with the previous
//! scrolling bar graph still selectable per button via
//! `WaveformStyle = "bars"`. While its layer is displayed the daemon
//! polls the desktop voice receiver's level endpoint - the same abstract
//! AF_UNIX and kernel peer-UID boundary as the volume and brightness
//! receivers - and repaints only that button at the poll cadence. Everywhere
//! else the widget sleeps: the poller is disarmed, no sockets are dialed,
//! and the shared epoll keeps its existing low-cadence timeouts. The widget
//! also has a visually distinct idle state (dim dot, flat baseline) for a
//! missing helper, so a layer switch alone never fakes live input.
//!
//! Samples land at the poll cadence (one level every [`POLL_INTERVAL`]),
//! but motion does not step at that cadence: between two samples the
//! widget's frame clock ([`FRAME_INTERVAL_MS`], roughly 30 fps) glides
//! the curve fractionally leftward and eases the newest level from the
//! previous one toward its measured value, so the line flows instead of
//! snapping a full slot every 50 ms. Frames render only while the
//! recording layer is displayed and only while something is in motion;
//! an idle, settled widget still wakes at most once per second for the
//! timer. The level feed itself stays as fresh as the desktop helper's
//! capture tail - see `integrations/voice/README.md`.
//!
//! The same widget, in [`Mode::Music`], is the media layer's opt-in music
//! visualizer: it drops the recording dot and duration timer, draws the
//! graph across the whole button, and shows a centered `Viz` label while
//! its session is off - the visualizer never shows by default. Its feed
//! is [`MonitorLevels`], which dials a second desktop receiver whose
//! capture is the default sink's *output* monitor. That stream exists
//! only while the visualizer is enabled: arming the poller sends `on`, so
//! the helper starts capturing, and disarming - toggle-off, leaving the
//! media layer, a reload, or the daemon going away - sends `off` (or, for
//! a vanished daemon, trips the helper's own idle watchdog), so no
//! monitor stream outlives its consumer. See `integrations/viz/README.md`.

use crate::appearance::ButtonAccent;
use cairo::{Context, LineCap, LineJoin};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    net::Shutdown,
    os::{
        fd::AsRawFd,
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixStream},
    },
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

/// Input-level samples kept for the bar graph; at the sample period below
/// this is a bit over two seconds of recent audio.
pub const HISTORY: usize = 48;
/// How far apart two level samples are: exactly the poll cadence, since one
/// poll produces one sample. The interpolation math assumes this spacing.
pub const SAMPLE_PERIOD: Duration = Duration::from_millis(50);
/// How often the armed poller dials the level endpoint (one sample each).
pub const POLL_INTERVAL: Duration = SAMPLE_PERIOD;
/// Frame cadence while the widget animates, about 30 fps: smooth scrolling
/// without repainting more than the display can usefully show.
pub const FRAME_INTERVAL_MS: i32 = 33;
/// Level replies must arrive within this window or count as not live.
const LEVEL_TIMEOUT: Duration = Duration::from_millis(300);
/// Inner padding of the widget inside its button.
const PAD: f64 = 12.0;
/// Idle recording-dot radius and the extra radius a full level adds.
const DOT_R_BASE: f64 = 3.5;
const DOT_R_SPAN: f64 = 3.0;
/// Centered label the music visualizer shows while its session is off.
const VIZ_LABEL: &str = "Viz";
/// Gap between the pulsing dot and the duration label.
const TEXT_GAP: f64 = 12.0;
/// Width reserved on the left for dot plus duration label.
const TIMER_BLOCK: f64 = 86.0;
const TIMER_FONT: f64 = 22.0;
/// Bars and line keep this much clearance from the button's inset edges.
const BARS_MARGIN: f64 = 10.0;
/// Carrier cycles of the line's wave per history slot. The carrier is a
/// function of the continuous sample index - which stays continuous across
/// the phase reset each push performs - so the wave never pops when a new
/// level lands; at the real button's slot width this reads as roughly a
/// 60-pixel wavelength on the glass.
const WAVE_CYCLES_PER_SLOT: f64 = 0.4;
/// Stroke width of the rendered line.
const LINE_WIDTH: f64 = 2.25;
/// Horizontal distance between two points of the rendered polyline; at
/// this step the carrier's steepest slope stays well inside the display's
/// resolving power while the per-frame path cost stays trivial.
const LINE_STEP: f64 = 2.0;
/// Even silence draws a visible pixel pair on the midline.
const MIN_BAR: f64 = 1.0;
/// Gap between adjacent bars.
const BAR_GAP: f64 = 2.0;
/// Decay time constant of the dot's peak-following pulse, in milliseconds.
/// Chosen so one sample period multiplies the pulse by roughly 0.6, the
/// release rate of the original per-sample decay.
const PULSE_TAU_MS: f32 = 98.0;
/// Repaint for the pulse alone only when it moved this much.
const PULSE_EPSILON: f32 = 0.25;
/// Upper bound for the optional visual lag, in milliseconds: a lag past
/// half a second would read as broken rather than calm.
const LAG_MAX_MS: u64 = 500;
/// How long a lifecycle `on`/`off` control reply may take: starting the
/// capture spawns a recorder, so it is far laxer than a level poll.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);

/// Render-side motion feel for the waveform's level envelope: an
/// asymmetric attack/release smoothing time constant plus an optional
/// visual lag. The level feed itself is untouched - samples land at the
/// poll cadence with full freshness - and every value here is applied to
/// what the widget draws, never to what it measures. With both knobs
/// zero (the default when the config omits them) smoothing passes the
/// measured level straight through and the lag buffer empties
/// immediately, which is exactly the pre-existing behavior.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Motion {
    /// Rise time constant, milliseconds: kept a fraction of the release so
    /// speaking visibly climbs without delay.
    attack_ms: f64,
    /// Decay time constant, milliseconds: the glide back down.
    release_ms: f64,
    /// How long a measured sample waits before it moves the graph.
    lag: Duration,
}

impl Motion {
    /// A calm envelope: release of `smoothing_ms` (clamped to the
    /// 150-300 ms class when configured sanely), attack at a third of it,
    /// and a visual lag of `lag_ms` clamped to [`LAG_MAX_MS`].
    pub fn new(smoothing_ms: u32, lag_ms: u32) -> Self {
        let release_ms = f64::from(smoothing_ms);
        Self {
            // A real attack needs some slew; floor it at one period.
            attack_ms: (release_ms / 3.0).max(f64::from(SAMPLE_PERIOD.as_millis() as u32)),
            release_ms,
            lag: Duration::from_millis(u64::from(lag_ms).min(LAG_MAX_MS)),
        }
    }

    /// No smoothing, no lag: the behavior shipped before the knobs existed.
    pub fn raw() -> Self {
        Self {
            attack_ms: 0.0,
            release_ms: 0.0,
            lag: Duration::ZERO,
        }
    }

    /// One sampled step of the envelope: move `current` toward `target`
    /// by the per-period fraction of the relevant time constant - attack
    /// when rising, release when falling. A zero time constant jumps
    /// straight to the measurement.
    fn advance(&self, current: f64, target: f64) -> f64 {
        let tau = if target > current {
            self.attack_ms
        } else {
            self.release_ms
        };
        if tau <= 0.0 {
            return target;
        }
        let period = SAMPLE_PERIOD.as_secs_f64() * 1000.0;
        let alpha = 1.0 - (-period / tau).exp();
        current + (target - current) * alpha
    }
}

/// Visual style of the live graph: the line is the default; `bars` keeps
/// the previous scrolling bar graph for owners who prefer it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Style {
    #[default]
    Line,
    Bars,
}

/// What a [`WaveformWidget`] visualizes: the microphone on the recording
/// layer, or the media layer's music visualizer. The music mode drops
/// the recording dot and duration timer, spans the whole button with the
/// graph, and shows a centered `Viz` label while its session is off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Mic,
    Music,
}

impl Style {
    /// Parse a config value, `None` for an unknown one.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "line" => Some(Style::Line),
            "bars" => Some(Style::Bars),
            _ => None,
        }
    }
}

/// One live microphone observation from the desktop receiver: `level` is
/// 0-100 (latest capture peak) and `live` reports whether a recording was
/// actually in progress. A missing, rejecting, or garbling receiver yields
/// `level 0, live false` rather than an error.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    pub level: u8,
    pub live: bool,
}

/// Fixed-capacity ring of the most recent input levels, oldest dropped
/// first. Rendered left (oldest) to right (newest).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LevelHistory {
    entries: [u8; HISTORY],
    next: usize,
    len: usize,
}

impl LevelHistory {
    pub fn new() -> Self {
        Self {
            entries: [0; HISTORY],
            next: 0,
            len: 0,
        }
    }

    pub fn clear(&mut self) {
        self.next = 0;
        self.len = 0;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The most recently pushed level, if any.
    pub fn newest(&self) -> Option<u8> {
        (self.len > 0).then(|| self.entries[(self.next + HISTORY - 1) % HISTORY])
    }

    pub fn push(&mut self, level: u8) {
        self.entries[self.next] = level;
        self.next = (self.next + 1) % HISTORY;
        self.len = (self.len + 1).min(HISTORY);
    }

    /// Levels from oldest to newest.
    pub fn oldest_first(&self) -> impl Iterator<Item = u8> + '_ {
        let start = (self.next + HISTORY - self.len) % HISTORY;
        (0..self.len).map(move |i| self.entries[(start + i) % HISTORY])
    }

    /// The level at position `i` of the oldest-first sequence.
    pub fn at(&self, i: usize) -> Option<u8> {
        if i >= self.len {
            return None;
        }
        let start = (self.next + HISTORY - self.len) % HISTORY;
        Some(self.entries[(start + i) % HISTORY])
    }
}

impl Default for LevelHistory {
    fn default() -> Self {
        Self::new()
    }
}

/// Geometry of the widget inside its button, in button-local pixels with
/// `y` measured from the button's top edge (the layer passes its pixel
/// shift through). Kept as data so tests can check the drawing math
/// without a Cairo context.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    /// Recording-dot center.
    pub dot: (f64, f64),
    /// Left edge of the duration label.
    pub text_x: f64,
    /// Left edge and width of the bar-graph area.
    pub bars: (f64, f64),
    /// Vertical center the bars mirror around.
    pub mid_y: f64,
    /// Half-height of a full-level bar.
    pub bar_max_half: f64,
    /// Horizontal slot each history sample owns.
    pub slot_w: f64,
}

/// Widget layout for a button-sized area.
pub fn layout(width: f64, height: f64) -> Layout {
    let dot_x = PAD + DOT_R_BASE + DOT_R_SPAN;
    let bars_left = TIMER_BLOCK.min((width - PAD).max(0.0));
    Layout {
        dot: (dot_x, height / 2.0),
        text_x: dot_x + DOT_R_BASE + DOT_R_SPAN + TEXT_GAP,
        bars: (bars_left, (width - bars_left - PAD).max(0.0)),
        mid_y: height / 2.0,
        bar_max_half: (height / 2.0 - BARS_MARGIN).max(MIN_BAR),
        slot_w: (width - bars_left - PAD).max(0.0) / HISTORY as f64,
    }
}

/// Widget layout for the music visualizer: no timer block is reserved,
/// so the graph spans the button between its side padding.
pub fn layout_music(width: f64, height: f64) -> Layout {
    let bars_width = (width - 2.0 * PAD).max(0.0);
    Layout {
        dot: (0.0, 0.0),
        text_x: 0.0,
        bars: (PAD, bars_width),
        mid_y: height / 2.0,
        bar_max_half: (height / 2.0 - BARS_MARGIN).max(MIN_BAR),
        slot_w: bars_width / HISTORY as f64,
    }
}

/// The mirrored bar one history sample draws: `(x, y, width, height)`.
/// `slot` is the bar's logical position and may be fractional - the
/// interpolation shifts every bar leftward by fractions of a slot between
/// samples - and `level` is eased, so it may be fractional too. Bars whose
/// slot dips below zero are simply exiting the graph on the left.
pub fn bar_rect(layout: &Layout, slot: f64, level: f64) -> (f64, f64, f64, f64) {
    let x = layout.bars.0 + slot * layout.slot_w;
    let width = (layout.slot_w - BAR_GAP).max(1.0);
    let level = level.clamp(0.0, 100.0);
    let half = MIN_BAR + (layout.bar_max_half - MIN_BAR) * level / 100.0;
    (x, layout.mid_y - half, width, half * 2.0)
}

/// Smooth ease-in-out over `0..=1`, used for the newest bar's height.
pub fn smoothstep(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Catmull-Rom interpolation through `p1` and `p2` with `p0` and `p3` as
/// tangent guides: a curve that passes exactly through its samples with a
/// continuous slope, so the line flows where linear joins would kink.
fn catmull_rom(p0: f64, p1: f64, p2: f64, p3: f64, t: f64) -> f64 {
    let t2 = t * t;
    let t3 = t2 * t;
    0.5 * ((2.0 * p1)
        + (-p0 + p2) * t
        + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
        + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t3)
}

/// The line's envelope level (0-100) at continuous sample index `s`, where
/// 0 is the oldest retained sample and `len-1` the newest, eased toward its
/// measured level by `phase`. Indices left of the retained window read as
/// silence and fade in over the first slot, so the curve leaves and enters
/// the graph flat rather than stepping; right of the newest sample the eased
/// level holds, so the line's right end rides the measurement exactly where
/// the previous period's curve ended.
pub fn envelope_at(history: &LevelHistory, previous_level: f64, phase: f64, s: f64) -> f64 {
    let len = history.len();
    if len == 0 || s <= 0.0 {
        return 0.0;
    }
    let newest = f64::from(history.newest().unwrap_or(0));
    let value = |j: usize| {
        let j = j.min(len - 1);
        if j + 1 == len {
            display_level(previous_level, newest, phase)
        } else {
            f64::from(history.at(j).unwrap_or(0))
        }
    };
    let level = if s >= (len - 1) as f64 {
        value(len - 1)
    } else {
        let i = s.floor() as usize;
        let t = s - s.floor();
        catmull_rom(
            value(i.saturating_sub(1)),
            value(i),
            value(i + 1),
            value(i + 2),
            t,
        )
        .clamp(0.0, 100.0)
    };
    // Fade the curve flat across the first slot so the data boundary and
    // the graph's left edge never show a corner.
    level * smoothstep(s.clamp(0.0, 1.0))
}

/// The line's vertical offset from the baseline at continuous sample index
/// `s`: the envelope scaled to the graph's half-height, carried by a sine
/// of the sample coordinate, so the wave scrolls with the history and
/// stays continuous across the sample boundary instead of standing still
/// under a moving envelope.
pub fn wave_offset(layout: &Layout, s: f64, envelope: f64) -> f64 {
    let amplitude = layout.bar_max_half * envelope.clamp(0.0, 100.0) / 100.0;
    amplitude * (2.0 * std::f64::consts::PI * WAVE_CYCLES_PER_SLOT * s).sin()
}

/// The level the newest bar displays at `phase` into the current sample
/// period: it rises from the previous sample's level, which is what the
/// bar showed the instant the previous period ended, so heights stay
/// continuous across sample boundaries.
pub fn display_level(previous: f64, current: f64, phase: f64) -> f64 {
    let t = smoothstep(phase);
    previous + (current - previous) * t
}

/// `m:ss` with unbounded minutes, matching the clock's legibility.
pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// State of one recording-page widget. The timer counts from entering the
/// layer carrying the widget; the history and pulse follow the live feed,
/// and the frame clock interpolates motion between samples.
pub struct WaveformWidget {
    style: Style,
    /// Whether this widget meters the microphone or visualizes music.
    mode: Mode,
    /// Live color of the recording dot, from the button's accent.
    live_rgb: (f64, f64, f64),
    history: LevelHistory,
    started: Option<Instant>,
    live: bool,
    /// Peak-following pulse driver for the recording dot.
    smoothed: f32,
    /// The pulse as it was last painted, to gate decay-only repaints.
    shown_pulse: f32,
    /// Second currently shown on the timer.
    shown_second: u64,
    /// When the newest sample landed; drives scroll and height easing.
    last_sample: Option<Instant>,
    /// The level before the newest sample; the newest bar eases from it.
    previous_level: f64,
    /// The level the motion envelope last displayed.
    displayed_level: f64,
    /// Render-side motion feel: smoothing and lag from the button's knobs.
    motion: Motion,
    /// Samples waiting out the visual lag, oldest first.
    pending: VecDeque<(Instant, Sample)>,
    /// When the last frame advanced the animation clock.
    last_frame: Option<Instant>,
}

impl WaveformWidget {
    pub fn new(style: Style) -> Self {
        Self {
            style,
            mode: Mode::Mic,
            live_rgb: ButtonAccent::Blue.rgb(),
            history: LevelHistory::new(),
            started: None,
            live: false,
            smoothed: 0.0,
            shown_pulse: 0.0,
            shown_second: 0,
            last_sample: None,
            previous_level: 0.0,
            displayed_level: 0.0,
            motion: Motion::raw(),
            pending: VecDeque::new(),
            last_frame: None,
        }
    }

    /// A music-visualizer widget for the media layer: the same line
    /// renderer, history, and frame clock as the microphone meter,
    /// without the recording chrome and drawing across the full button.
    pub fn new_music(style: Style) -> Self {
        Self {
            mode: Mode::Music,
            ..Self::new(style)
        }
    }

    /// Whether this widget is the media layer's music visualizer rather
    /// than the recording layer's microphone meter.
    pub fn is_music(&self) -> bool {
        self.mode == Mode::Music
    }

    /// Take the render-side motion knobs from the button's config.
    pub fn with_motion(mut self, smoothing_ms: Option<u32>, lag_ms: Option<u32>) -> Self {
        self.motion = match (smoothing_ms, lag_ms) {
            (None, None) => Motion::raw(),
            (smoothing, lag) => Motion::new(smoothing.unwrap_or(0), lag.unwrap_or(0)),
        };
        self
    }

    /// Begin a fresh session when the widget's layer becomes the displayed
    /// one. Returns whether the bar must repaint the widget.
    pub fn activate(&mut self, now: Instant) -> bool {
        if self.started.is_some() {
            return false;
        }
        self.started = Some(now);
        self.history.clear();
        self.live = false;
        self.smoothed = 0.0;
        self.shown_pulse = 0.0;
        self.shown_second = 0;
        self.last_sample = None;
        self.previous_level = 0.0;
        self.displayed_level = 0.0;
        self.pending.clear();
        self.last_frame = None;
        true
    }

    /// End the session when another layer is displayed; re-entering starts
    /// the timer and graph over.
    pub fn deactivate(&mut self) {
        self.started = None;
        self.live = false;
    }

    #[cfg(test)]
    pub fn is_active(&self) -> bool {
        self.started.is_some()
    }

    /// Fold one poll result in. Returns whether the sample itself changed
    /// what is on glass - a new level or a live flip; the interpolated frames
    /// own motion and pulse repaints through [`Self::frame`], so a feed of
    /// identical levels or idle replies reports no change of its own.
    pub fn sample(&mut self, sample: Sample, now: Instant) -> bool {
        if self.started.is_none() {
            return false;
        }
        let before = (self.history.newest(), self.live);
        self.pending.push_back((now, sample));
        self.drain(now);
        self.history.newest() != before.0 || self.live != before.1
    }

    /// Release every lagged sample whose wait has elapsed. The lag is
    /// render-side only: the sample was measured when it arrived; the
    /// graph simply tells the story a moment later for steadiness.
    fn drain(&mut self, now: Instant) -> bool {
        let mut applied = false;
        while let Some(&(at, sample)) = self.pending.front() {
            if now.saturating_duration_since(at) < self.motion.lag {
                break;
            }
            self.pending.pop_front();
            self.apply(sample, at);
            applied = true;
        }
        applied
    }

    /// Fold one measured sample into the rendered history through the
    /// motion envelope: the level glides toward the measurement with the
    /// attack/release time constants before it joins the graph.
    fn apply(&mut self, sample: Sample, measured_at: Instant) {
        self.previous_level = self.displayed_level;
        self.live = sample.live;
        self.displayed_level = self
            .motion
            .advance(self.displayed_level, f64::from(sample.level));
        self.history.push(self.displayed_level.round() as u8);
        self.smoothed = f32::from(sample.level).max(self.smoothed);
        self.last_sample = Some(measured_at);
    }

    /// Advance the animation clock to `now`: decay the dot's pulse with a
    /// time constant and fold the timer's second boundary. Returns whether
    /// the widget is in motion - the graph is scrolling while live, the
    /// pulse is still settling, or the displayed second changed - and so
    /// must be repainted; a settled, idle widget stays quiet between
    /// second boundaries.
    pub fn frame(&mut self, now: Instant) -> bool {
        let Some(_) = self.started else {
            self.last_frame = Some(now);
            return false;
        };
        let elapsed = self
            .last_frame
            .map_or(Duration::ZERO, |last| now.saturating_duration_since(last));
        self.last_frame = Some(now);
        self.smoothed *= (-elapsed.as_secs_f32() * 1000.0 / PULSE_TAU_MS).exp();
        let scrolling = self.live && !self.history.is_empty();
        let lagged = self.drain(now);
        let pulse_settling = (self.smoothed - self.shown_pulse).abs() > PULSE_EPSILON;
        let ticked = self.tick(now);
        let moved = scrolling || lagged || pulse_settling || ticked;
        if moved {
            self.shown_pulse = self.smoothed;
        }
        moved
    }

    /// Advance the duration timer to `now`. Returns whether the displayed
    /// second changed and the widget must repaint. The music visualizer
    /// keeps no timer, so it never ticks.
    pub fn tick(&mut self, now: Instant) -> bool {
        if self.mode == Mode::Music {
            return false;
        }
        let Some(second) = self
            .started
            .map(|start| now.saturating_duration_since(start).as_secs())
        else {
            return false;
        };
        if second == self.shown_second {
            return false;
        }
        self.shown_second = second;
        true
    }

    /// How far into the current sample period the render is, `0..=1`: the
    /// fraction of [`SAMPLE_PERIOD`] elapsed since the newest sample. With
    /// no sample yet there is nothing to interpolate and the phase sits at
    /// rest (`1.0`), matching where the previous period's easing ended.
    pub fn phase(&self, now: Instant) -> f64 {
        self.last_sample
            .map_or(1.0, |last| {
                now.saturating_duration_since(last).as_secs_f64() / SAMPLE_PERIOD.as_secs_f64()
            })
            .min(1.0)
    }

    /// Milliseconds until the next frame for the shared epoll timeout.
    /// While the widget animates that is the frame cadence; a settled,
    /// idle microphone widget wakes only for the timer's second boundary;
    /// the music visualizer keeps no timer, so an on-but-idle one adds no
    /// wakes at all; an inactive one (`i32::MAX - 1`) never runs fast.
    pub fn next_timeout(&self, now: Instant) -> i32 {
        let Some(start) = self.started else {
            return i32::MAX - 1;
        };
        if (self.live && !self.history.is_empty()) || self.smoothed > PULSE_EPSILON {
            return FRAME_INTERVAL_MS;
        }
        if self.mode == Mode::Music {
            return i32::MAX - 1;
        }
        let elapsed = now.saturating_duration_since(start).as_millis() as i64;
        (1000 - elapsed % 1000).clamp(1, i32::MAX as i64 - 1) as i32
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        self.started
            .map_or(Duration::ZERO, |start| now.saturating_duration_since(start))
    }

    #[cfg(test)]
    pub fn history(&self) -> &LevelHistory {
        &self.history
    }

    /// Take the live dot color from the button's accent, defaulting to
    /// the shipped blue when the button carries no explicit accent.
    pub fn with_accent(mut self, accent: Option<ButtonAccent>) -> Self {
        self.live_rgb = accent.unwrap_or(ButtonAccent::Blue).rgb();
        self
    }

    /// Draw the widget. `c` carries the layer's rotation; the caller clips
    /// to the button's rect, so the layout never exceeds `width`/`height`.
    pub fn render(&self, c: &Context, height: i32, left: f64, width: f64, y_shift: f64) {
        self.render_at(c, height, left, width, y_shift, Instant::now());
    }

    /// [`Self::render`] with an explicit clock, so offline review frames can
    /// capture the animation mid-period.
    pub fn render_at(
        &self,
        c: &Context,
        height: i32,
        left: f64,
        width: f64,
        y_shift: f64,
        now: Instant,
    ) {
        if self.mode == Mode::Music {
            self.render_music(c, height, left, width, y_shift, now);
            return;
        }
        let layout = layout(width, height as f64);
        let cy = y_shift + layout.mid_y;
        // The recording dot pulses with live level; idle it stays small and dim.
        let radius = DOT_R_BASE + DOT_R_SPAN * f64::from(self.smoothed) / 100.0;
        let (r, g, b) = if self.live {
            self.live_rgb
        } else {
            (0.45, 0.45, 0.45)
        };
        c.set_source_rgb(r, g, b);
        c.arc(
            left + layout.dot.0,
            cy,
            radius,
            0.0,
            2.0 * std::f64::consts::PI,
        );
        c.fill().unwrap();
        // Duration label beside the dot.
        let label = format_duration(self.elapsed(now));
        c.set_source_rgb(1.0, 1.0, 1.0);
        c.set_font_size(TIMER_FONT);
        let extents = c.text_extents(&label).unwrap();
        c.move_to(
            left + layout.text_x,
            cy - extents.y_bearing() - extents.height() / 2.0,
        );
        c.show_text(&label).unwrap();
        // The live input visualization - the newest level eases in at the
        // right edge while older history glides left between samples, or a
        // flat dim baseline while idle. The fractional phase keeps positions
        // continuous across sample boundaries: a feature one slot right at
        // phase 0 is exactly where its predecessor ended at phase 1.
        if self.live {
            match self.style {
                Style::Line => self.render_line(c, &layout, left, cy, now),
                Style::Bars => self.render_bars(c, &layout, left, y_shift, now),
            }
        } else {
            c.set_source_rgb(0.42, 0.42, 0.42);
            c.rectangle(left + layout.bars.0, cy - 0.5, layout.bars.1, 1.0);
            c.fill().unwrap();
        }
    }

    /// The music visualizer's chrome-free rendering: the same line or bar
    /// graph across the whole button while a session runs and the feed is
    /// live, a dim flat baseline while on without a feed, and a centered
    /// `Viz` label while off - the visualizer is opt-in every time its
    /// layer is entered, and the label keeps the control discoverable.
    fn render_music(
        &self,
        c: &Context,
        height: i32,
        left: f64,
        width: f64,
        y_shift: f64,
        now: Instant,
    ) {
        let layout = layout_music(width, height as f64);
        let cy = y_shift + layout.mid_y;
        if self.started.is_none() {
            c.set_source_rgb(1.0, 1.0, 1.0);
            c.set_font_size(TIMER_FONT);
            let extents = c.text_extents(VIZ_LABEL).unwrap();
            c.move_to(
                left + (width - extents.width()) / 2.0 - extents.x_bearing(),
                cy - extents.y_bearing() - extents.height() / 2.0,
            );
            c.show_text(VIZ_LABEL).unwrap();
            return;
        }
        if self.live {
            match self.style {
                Style::Line => self.render_line(c, &layout, left, cy, now),
                Style::Bars => self.render_bars(c, &layout, left, y_shift, now),
            }
        } else {
            c.set_source_rgb(0.42, 0.42, 0.42);
            c.rectangle(left + layout.bars.0, cy - 0.5, layout.bars.1, 1.0);
            c.fill().unwrap();
        }
    }

    /// The line renderer: one continuous stroke across the graph area,
    /// flat where the envelope rests and rippling with live input. Both the
    /// envelope and the carrier are functions of the continuous sample
    /// index, so between samples the whole curve glides left by fractions
    /// of a slot - the same scroll the bars ride - and a new level landing
    /// never pops the wave's shape.
    fn render_line(&self, c: &Context, layout: &Layout, left: f64, cy: f64, now: Instant) {
        let phase = self.phase(now);
        let area_left = left + layout.bars.0;
        let right = area_left + layout.bars.1;
        c.set_source_rgb(1.0, 1.0, 1.0);
        c.set_line_width(LINE_WIDTH);
        c.set_line_cap(LineCap::Round);
        c.set_line_join(LineJoin::Round);
        let mut x = area_left;
        let mut started = false;
        while x < right {
            let y = cy - self.line_offset(layout, x, phase);
            if started {
                c.line_to(x, y);
            } else {
                c.move_to(x, y);
                started = true;
            }
            x += LINE_STEP;
        }
        // Close exactly on the right edge whatever the step remainder is.
        let y = cy - self.line_offset(layout, right, phase);
        if started {
            c.line_to(right, y);
        } else {
            c.move_to(right, y);
        }
        c.stroke().unwrap();
    }

    /// The line's vertical offset from the baseline at button-local pixel
    /// `x`, positive upward: the eased envelope at that pixel's continuous
    /// sample position, carried by the scrolling sine. Kept as data-side
    /// math so tests can check continuity and bounds without a context.
    fn line_offset(&self, layout: &Layout, x: f64, phase: f64) -> f64 {
        let s = (x + phase * layout.slot_w - layout.bars.0) / layout.slot_w
            - (HISTORY - self.history.len()) as f64;
        let envelope = envelope_at(&self.history, self.previous_level, phase, s);
        wave_offset(layout, s, envelope)
    }

    /// The bar renderer, the style this widget shipped with before the
    /// line: vertical bars mirroring around the baseline, gliding left.
    fn render_bars(&self, c: &Context, layout: &Layout, left: f64, y_shift: f64, now: Instant) {
        let len = self.history.len();
        let phase = self.phase(now);
        c.set_source_rgb(1.0, 1.0, 1.0);
        for (i, level) in self.history.oldest_first().enumerate() {
            let level = if i + 1 == len {
                display_level(self.previous_level, f64::from(level), phase)
            } else {
                f64::from(level)
            };
            let slot = (HISTORY - len + i) as f64 - phase;
            let (x, y, w, h) = bar_rect(layout, slot, level);
            c.rectangle(left + x, y_shift + y, w, h);
        }
        c.fill().unwrap();
    }
}

impl Default for WaveformWidget {
    fn default() -> Self {
        Self::new(Style::Line)
    }
}

/// Shared arm flag the render loop writes and the poller waits on.
struct Arm {
    armed: Mutex<bool>,
    signal: Condvar,
}

/// Client for the voice receiver's level endpoint. The poller thread dials
/// the abstract socket only while armed; each result wakes the shared epoll
/// through `wake`, exactly like the battery and level-control workers.
pub struct MicLevels {
    arm: Arc<Arm>,
    samples: Receiver<Sample>,
    pub wake: UnixStream,
}

impl MicLevels {
    /// Connect to the installed desktop receiver (UID 1000), mirroring the
    /// volume and brightness endpoints.
    pub fn levels() -> Self {
        let address = SocketAddr::from_abstract_name(b"dfr-voice-level-1000").unwrap();
        Self::connect_to(address, 1000)
    }

    fn connect_to(address: SocketAddr, uid: u32) -> Self {
        let arm = Arc::new(Arm {
            armed: Mutex::new(false),
            signal: Condvar::new(),
        });
        let (outgoing, samples) = mpsc::channel();
        let (wake, mut notify) = UnixStream::pair().unwrap();
        wake.set_nonblocking(true).unwrap();
        notify.set_nonblocking(true).unwrap();
        let poller_arm = Arc::clone(&arm);
        thread::spawn(move || poller(address, uid, &poller_arm, &outgoing, &mut notify));
        Self { arm, samples, wake }
    }

    /// Arm or disarm the poller; the value only signals on change so the
    /// render loop can call this every iteration.
    pub fn set_armed(&self, armed: bool) {
        let mut state = self.arm.armed.lock().unwrap();
        if *state != armed {
            *state = armed;
            self.arm.signal.notify_all();
        }
    }

    /// Drain pending results and return the freshest sample, if any.
    pub fn update(&mut self) -> Option<Sample> {
        while matches!(self.wake.read(&mut [0; 64]), Ok(n) if n > 0) {}
        self.samples.try_iter().last()
    }
}

/// Ask the desktop receiver for one live level observation. Any transport
/// or validation failure simply means "not live" - a missing helper is the
/// normal case, not an error worth logging at poll cadence.
fn fetch_level(address: &SocketAddr, uid: u32) -> Sample {
    fetch_level_result(address, uid).unwrap_or_default()
}

fn fetch_level_result(address: &SocketAddr, uid: u32) -> io::Result<Sample> {
    let mut connection = connect_verified(address, uid)?;
    connection.write_all(b"get")?;
    connection.shutdown(Shutdown::Write)?;
    let mut response = String::new();
    connection.take(32).read_to_string(&mut response)?;
    parse_level(&response)
}

/// Connect to a desktop receiver's abstract socket and verify the
/// kernel-supplied peer UID - the same boundary every receiver shares.
fn connect_verified(address: &SocketAddr, uid: u32) -> io::Result<UnixStream> {
    let connection = UnixStream::connect_addr(address)?;
    connection.set_read_timeout(Some(LEVEL_TIMEOUT))?;
    connection.set_write_timeout(Some(LEVEL_TIMEOUT))?;
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SO_PEERCRED is supplied by the kernel, not the peer's request payload.
    let rc = unsafe {
        libc::getsockopt(
            connection.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut credentials as *mut _ as *mut libc::c_void,
            &mut length,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if credentials.uid != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Unexpected level receiver UID",
        ));
    }
    Ok(connection)
}

/// Ask the visualizer receiver to start (`on`) or stop (`off`) its output
/// capture. A missing receiver is the normal not-installed case and a
/// rejected request is the helper's own error; the poller ignores both,
/// since the level feed reports not-live either way.
fn fetch_control(address: &SocketAddr, uid: u32, command: &str) -> io::Result<()> {
    let mut connection = connect_verified(address, uid)?;
    connection.set_read_timeout(Some(CONTROL_TIMEOUT))?;
    connection.write_all(command.as_bytes())?;
    connection.shutdown(Shutdown::Write)?;
    let mut response = String::new();
    connection.take(32).read_to_string(&mut response)?;
    if response.trim() == "ok" {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Visualizer receiver rejected request",
        ))
    }
}

fn parse_level(response: &str) -> io::Result<Sample> {
    let fields: Vec<_> = response.split_whitespace().collect();
    match fields.as_slice() {
        ["ok", level] => {
            let level = level
                .parse::<u8>()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid level payload"))?;
            if level <= 100 {
                Ok(Sample { level, live: true })
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Level receiver out of range",
                ))
            }
        }
        ["idle"] => Ok(Sample {
            level: 0,
            live: false,
        }),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Level receiver rejected request",
        )),
    }
}

/// Client for the visualizer receiver's monitor endpoint. Identical in
/// shape to [`MicLevels`], with the capture lifecycle riding the arm
/// flag: arming sends `on`, so the desktop helper starts its output-
/// monitor capture, and disarming - or losing the consumer - sends `off`,
/// so the stream exists only while the visualizer actually consumes it.
pub struct MonitorLevels {
    arm: Arc<Arm>,
    samples: Receiver<Sample>,
    pub wake: UnixStream,
}

impl MonitorLevels {
    /// Connect to the installed desktop receiver (UID 1000), mirroring
    /// the volume, brightness, and voice level endpoints.
    pub fn visualizer() -> Self {
        let address = SocketAddr::from_abstract_name(b"dfr-viz-1000").unwrap();
        Self::connect_to(address, 1000)
    }

    fn connect_to(address: SocketAddr, uid: u32) -> Self {
        let arm = Arc::new(Arm {
            armed: Mutex::new(false),
            signal: Condvar::new(),
        });
        let (outgoing, samples) = mpsc::channel();
        let (wake, mut notify) = UnixStream::pair().unwrap();
        wake.set_nonblocking(true).unwrap();
        notify.set_nonblocking(true).unwrap();
        let poller_arm = Arc::clone(&arm);
        thread::spawn(move || viz_poller(address, uid, &poller_arm, &outgoing, &mut notify));
        Self { arm, samples, wake }
    }

    /// Arm or disarm the poller; the value only signals on change so the
    /// render loop can call this every iteration. Flipping it also hands
    /// the capture lifecycle to the desktop receiver.
    pub fn set_armed(&self, armed: bool) {
        let mut state = self.arm.armed.lock().unwrap();
        if *state != armed {
            *state = armed;
            self.arm.signal.notify_all();
        }
    }

    /// Drain pending results and return the freshest sample, if any.
    pub fn update(&mut self) -> Option<Sample> {
        while matches!(self.wake.read(&mut [0; 64]), Ok(n) if n > 0) {}
        self.samples.try_iter().last()
    }
}

/// Dial the level endpoint only while armed, at a bounded cadence, pushing
/// every result to the render loop.
fn poller(
    address: SocketAddr,
    uid: u32,
    arm: &Arm,
    samples: &Sender<Sample>,
    notify: &mut UnixStream,
) {
    let mut armed = false;
    let mut deadline: Option<Instant> = None;
    loop {
        // Block until the arm state flips or the poll interval elapses.
        let mut state = arm.armed.lock().unwrap();
        loop {
            if *state != armed {
                armed = *state;
                deadline = None;
                break;
            }
            match deadline {
                None if armed => deadline = Some(Instant::now() + POLL_INTERVAL),
                Some(at) if Instant::now() >= at => {
                    deadline = None;
                    break;
                }
                _ => {}
            }
            state = match deadline {
                Some(at) => {
                    arm.signal
                        .wait_timeout(state, at.saturating_duration_since(Instant::now()))
                        .unwrap()
                        .0
                }
                None => arm.signal.wait(state).unwrap(),
            };
        }
        drop(state);
        if !armed {
            continue;
        }
        let sample = fetch_level(&address, uid);
        if samples.send(sample).is_err() {
            return;
        }
        let _ = notify.write(&[1]);
    }
}

/// The visualizer's poller: identical pacing to [`poller`], plus the
/// capture lifecycle handshake. The first armed transition sends `on` so
/// the desktop receiver starts its output-monitor capture, level polls
/// follow at the shared cadence, and disarming - or losing the consumer -
/// sends `off` so the monitor stream never outlives the visualizer.
fn viz_poller(
    address: SocketAddr,
    uid: u32,
    arm: &Arm,
    samples: &Sender<Sample>,
    notify: &mut UnixStream,
) {
    let mut armed = false;
    let mut deadline: Option<Instant> = None;
    loop {
        // Block until the arm state flips or the poll interval elapses.
        let mut state = arm.armed.lock().unwrap();
        let mut flipped = false;
        loop {
            if *state != armed {
                armed = *state;
                deadline = None;
                flipped = true;
                break;
            }
            match deadline {
                None if armed => deadline = Some(Instant::now() + POLL_INTERVAL),
                Some(at) if Instant::now() >= at => {
                    deadline = None;
                    break;
                }
                _ => {}
            }
            state = match deadline {
                Some(at) => {
                    arm.signal
                        .wait_timeout(state, at.saturating_duration_since(Instant::now()))
                        .unwrap()
                        .0
                }
                None => arm.signal.wait(state).unwrap(),
            };
        }
        drop(state);
        if flipped {
            // The capture stream's whole lifecycle rides the arm flag.
            let _ = fetch_control(&address, uid, if armed { "on" } else { "off" });
        }
        if !armed {
            continue;
        }
        let sample = fetch_level(&address, uid);
        if samples.send(sample).is_err() {
            // The daemon end is gone: stop the capture before exiting.
            let _ = fetch_control(&address, uid, "off");
            return;
        }
        let _ = notify.write(&[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairo::{Format, ImageSurface};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn the_motion_envelope_attacks_faster_than_it_releases_and_honors_the_knob() {
        // Zero knobs pass the measurement straight through, unchanged.
        let raw = Motion::raw();
        assert_eq!(raw.advance(20.0, 80.0), 80.0);
        assert_eq!(raw.advance(80.0, 20.0), 20.0);

        let calm = Motion::new(240, 0);
        // One 50 ms sample period: rising covers more ground than falling,
        // because the attack time constant is a third of the release.
        let rise = calm.advance(20.0, 80.0);
        let fall = calm.advance(80.0, 20.0);
        assert!(rise - 20.0 > 80.0 - fall, "attack must outrun release");
        // The knob is honored: a shorter release closes more of the gap.
        let brisk = Motion::new(60, 0);
        assert!(brisk.advance(20.0, 80.0) > rise);
        // The rise stays fractional for a long time constant, never jumps.
        assert!(rise < 80.0);
        // A larger smoothing still moves, just less.
        assert!(Motion::new(600, 0).advance(20.0, 80.0) < rise);
    }

    #[test]
    fn the_lag_buffer_holds_samples_back_and_bounding_arrives_late() {
        let now = Instant::now();
        // Absent knobs keep today's behavior: the sample lands immediately.
        let mut immediate = WaveformWidget::new(Style::Line).with_motion(None, None);
        immediate.activate(now);
        assert!(immediate.sample(
            Sample {
                level: 70,
                live: true
            },
            now
        ));
        assert_eq!(immediate.history().len(), 1);

        // A lag of one sample period holds the sample out of the history
        // until its wait elapses, then releases it exactly once.
        let mut wave = WaveformWidget::new(Style::Line).with_motion(Some(0), Some(50));
        wave.activate(now);
        assert!(!wave.sample(
            Sample {
                level: 70,
                live: true
            },
            now
        ));
        assert_eq!(wave.history().len(), 0);
        assert!(!wave.frame(now + Duration::from_millis(40)));
        assert_eq!(wave.history().len(), 0);
        assert!(wave.frame(now + Duration::from_millis(60)));
        assert_eq!(wave.history().len(), 1);

        // The lag is bounded: past LAG_MAX_MS it can never hold longer,
        // and a smoothing knob paired with it does not extend the wait.
        let mut wave = WaveformWidget::new(Style::Line).with_motion(Some(240), Some(u32::MAX));
        wave.activate(now);
        assert!(!wave.sample(
            Sample {
                level: 70,
                live: true
            },
            now
        ));
        // The clamp holds u32::MAX ms of lag to LAG_MAX_MS.
        let release_at = now + Duration::from_millis(LAG_MAX_MS) + Duration::from_millis(1);
        assert!(wave.frame(release_at));
        assert_eq!(wave.history().len(), 1);
    }

    fn address() -> SocketAddr {
        let name = format!(
            "dfr-waveform-level-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        );
        assert_ne!(name, "dfr-voice-level-1000");
        SocketAddr::from_abstract_name(name.as_bytes()).unwrap()
    }

    /// Serve one request per entry, replying the scripted bytes.
    fn script_server(
        address: SocketAddr,
        replies: Vec<&'static str>,
    ) -> (thread::JoinHandle<()>, mpsc::Receiver<()>) {
        let (ready, signal) = mpsc::channel();
        let handle = thread::spawn(move || {
            let listener = UnixListener::bind_addr(&address).unwrap();
            ready.send(()).unwrap();
            for reply in replies {
                let (mut connection, _) = listener.accept().unwrap();
                connection
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = String::new();
                (&mut connection)
                    .take(32)
                    .read_to_string(&mut request)
                    .unwrap();
                assert_eq!(request, "get");
                connection.write_all(reply.as_bytes()).unwrap();
            }
        });
        (handle, signal)
    }

    #[test]
    fn history_ring_drops_the_oldest_and_keeps_order() {
        let mut history = LevelHistory::new();
        assert!(history.is_empty());
        assert_eq!(history.newest(), None);
        for level in 1..=u8::try_from(HISTORY + 12).unwrap() {
            history.push(level);
        }
        assert_eq!(history.len(), HISTORY);
        assert_eq!(history.newest(), Some(HISTORY as u8 + 12));
        // Only the newest HISTORY levels survive, oldest first.
        assert_eq!(
            history.oldest_first().collect::<Vec<_>>(),
            (13..=HISTORY as u8 + 12).collect::<Vec<_>>()
        );
        history.push(200);
        assert_eq!(history.newest(), Some(200));
        assert_eq!(history.oldest_first().next(), Some(14));
        history.clear();
        assert!(history.is_empty());
        assert_eq!(history.newest(), None);
    }

    #[test]
    fn bar_geometry_grows_with_the_level_and_stays_inside_the_area() {
        let layout = layout(570.0, 60.0);
        assert!(layout.bars.0 + layout.bars.1 <= 570.0);
        assert!(layout.bar_max_half <= 30.0 - BARS_MARGIN);
        let mut previous = 0.0;
        for level in 0..=100u8 {
            for slot in [0.0, 0.25, 47.0] {
                let (x, y, w, h) = bar_rect(&layout, slot, f64::from(level));
                assert!(x >= layout.bars.0, "bar left of the area");
                assert!(
                    x + w <= layout.bars.0 + layout.bars.1 + 1e-6,
                    "bar past the area"
                );
                assert!(y >= layout.mid_y - layout.bar_max_half - 1e-6);
                assert!(y + h <= layout.mid_y + layout.bar_max_half + 1e-6);
                assert!(h >= MIN_BAR * 2.0);
            }
            let height = bar_rect(&layout, 0.0, f64::from(level)).3;
            assert!(height >= previous, "heights must not shrink with level");
            previous = height;
        }
        // Neighboring slots never overlap, eased levels stay in range, and
        // silence still draws a pixel pair.
        let (x0, _, w0, h0) = bar_rect(&layout, 0.0, 0.0);
        let (x1, _, _, _) = bar_rect(&layout, 1.0, 0.0);
        assert!(x1 >= x0 + w0);
        assert!(h0 >= MIN_BAR * 2.0);
        assert!(bar_rect(&layout, 0.0, 100.0).3 > bar_rect(&layout, 0.0, 0.0).3);
        let (_, _, _, clamped) = bar_rect(&layout, 0.0, 140.0);
        assert_eq!(clamped, bar_rect(&layout, 0.0, 100.0).3);
        let (_, _, _, eased) = bar_rect(&layout, 0.0, 33.5);
        assert!(bar_rect(&layout, 0.0, 33.0).3 < eased);
        assert!(eased < bar_rect(&layout, 0.0, 34.0).3);
    }

    #[test]
    fn interpolated_bars_scroll_and_ease_continuously_across_samples() {
        let layout = layout(570.0, 60.0);
        // The ease runs the full range, is monotone, and rests at both ends.
        assert_eq!(smoothstep(0.0), 0.0);
        assert_eq!(smoothstep(1.0), 1.0);
        assert!((smoothstep(0.5) - 0.5).abs() < 1e-9);
        let mut previous = display_level(20.0, 80.0, 0.0);
        for step in 1..=10 {
            let phase = f64::from(step) / 10.0;
            let level = display_level(20.0, 80.0, phase);
            assert!(level >= previous, "ease must not dip");
            assert!(level <= 80.0);
            previous = level;
        }
        assert_eq!(display_level(20.0, 80.0, 0.0), 20.0);
        assert_eq!(display_level(20.0, 80.0, 1.0), 80.0);
        assert_eq!(display_level(20.0, 80.0, 2.0), 80.0);

        // Positions are continuous across a sample boundary: a bar that
        // finished its period at phase 1 sits exactly where its successor
        // starts at phase 0 after the graph shifts one slot.
        let len = 12;
        for i in 0..len {
            let before = bar_rect(&layout, (HISTORY - len + i) as f64 - 1.0, 55.0);
            let after = bar_rect(&layout, (HISTORY - (len + 1) + i) as f64, 55.0);
            assert_eq!(before, after);
        }
        // Between samples every slot glides left by fractions of a slot.
        let start = bar_rect(&layout, 10.0, 55.0).0;
        let mid = bar_rect(&layout, 10.0 - 0.5, 55.0).0;
        let end = bar_rect(&layout, 9.0, 55.0).0;
        assert!(start > mid && mid > end);
        assert!((start - mid - (start - end) / 2.0).abs() < 1e-9);

        // The newest bar's height matches its eased level: it starts at the
        // previous sample's height and ends at its own.
        let widget_height = |phase| bar_rect(&layout, 0.0, display_level(40.0, 70.0, phase)).3;
        assert_eq!(widget_height(0.0), bar_rect(&layout, 0.0, 40.0).3);
        assert_eq!(widget_height(1.0), bar_rect(&layout, 0.0, 70.0).3);
        assert!(widget_height(0.5) > widget_height(0.0));
    }

    #[test]
    fn the_live_dot_defaults_to_blue_and_honors_the_button_accent() {
        use crate::appearance::ButtonAccent;

        assert_eq!(
            WaveformWidget::new(Style::Line).live_rgb,
            ButtonAccent::Blue.rgb()
        );
        assert_eq!(
            WaveformWidget::new(Style::Line)
                .with_accent(Some(ButtonAccent::Red))
                .live_rgb,
            ButtonAccent::Red.rgb()
        );
        assert_ne!(ButtonAccent::Blue.rgb(), ButtonAccent::Red.rgb());
    }

    #[test]
    fn the_live_dot_paints_the_accent_color_not_the_old_red() {
        // A live dot in default blue: some painted pixel carries a
        // distinctly stronger blue channel than red, which the old hardcoded
        // red could never do (ARGB32 bytes are B, G, R, A here).
        let mut surface = ImageSurface::create(Format::ARgb32, 570, 60).unwrap();
        let now = Instant::now();
        let mut wave = WaveformWidget::new(Style::Line);
        wave.activate(now);
        wave.sample(
            Sample {
                level: 50,
                live: true,
            },
            now,
        );
        {
            let c = Context::new(&surface).unwrap();
            wave.render_at(&c, 60, 0.0, 570.0, 0.0, now + Duration::from_millis(40));
        }
        // A live dot in default blue: some painted pixel carries a
        // distinctly stronger blue channel than red, which the old hardcoded
        // red could never do (ARGB32 bytes are B, G, R, A here).
        let data = surface.data().unwrap();
        let mut blue_dominant = false;
        for i in (0..data.len()).step_by(4) {
            if i32::from(data[i]) > i32::from(data[i + 2]) + 20 {
                blue_dominant = true;
                break;
            }
        }
        assert!(blue_dominant, "live dot is not painted blue");
    }

    #[test]
    fn styles_parse_from_config_values() {
        assert_eq!(Style::parse("line"), Some(Style::Line));
        assert_eq!(Style::parse("bars"), Some(Style::Bars));
        assert_eq!(Style::parse("squiggle"), None);
        assert_eq!(Style::parse(""), None);
        assert_ne!(Style::default(), Style::Bars);
    }

    #[test]
    fn the_envelope_interpolates_smoothly_and_fades_in_at_the_data_edge() {
        let mut history = LevelHistory::new();
        // Silence reads as a perfectly flat line whatever the carrier does.
        for _ in 0..HISTORY {
            history.push(0);
        }
        for s in [0.0, 0.5, 12.25, 47.0] {
            assert_eq!(envelope_at(&history, 0.0, 0.5, s), 0.0);
        }
        // The curve passes exactly through its samples and stays in range.
        let mut history = LevelHistory::new();
        for level in [10, 30, 55, 40, 70, 25, 60, 45] {
            history.push(level);
        }
        for (i, level) in [55, 40, 70, 25, 60].into_iter().enumerate() {
            let s = i as f64 + 2.0;
            assert!((envelope_at(&history, 70.0, 1.0, s) - f64::from(level)).abs() < 1e-9);
        }
        for step in 0..=14 {
            for phase in [0.0, 0.5, 1.0] {
                let value = envelope_at(&history, 70.0, phase, f64::from(step) / 2.0);
                assert!(
                    (0.0..=100.0).contains(&value),
                    "envelope out of range at {}",
                    step / 2
                );
            }
        }
        // The data edge fades in over the first slot: no corner where the
        // flat no-data region meets the oldest sample.
        assert_eq!(envelope_at(&history, 70.0, 1.0, 0.0), 0.0);
        assert!(envelope_at(&history, 70.0, 1.0, 0.5) < 30.0);
        // Right of the newest sample the eased level holds, so the line's
        // right end rides the measurement: it starts at the previous
        // sample's level and rests at the newest one.
        assert!((envelope_at(&history, 70.0, 1.0, 9.0) - 45.0).abs() < 1e-9);
        assert!((envelope_at(&history, 70.0, 0.0, 9.0) - 70.0).abs() < 1e-9);
        assert!((envelope_at(&history, 70.0, 0.5, 9.0) - 57.5).abs() < 1e-9);
        // An empty history is silence at every index.
        assert_eq!(envelope_at(&LevelHistory::new(), 80.0, 0.5, 3.0), 0.0);
    }

    #[test]
    fn the_line_rests_flat_stays_bounded_and_stays_continuous_across_samples() {
        let layout = layout(570.0, 60.0);
        let now = Instant::now();
        let sweep = (0..250)
            .map(|i| layout.bars.0 + layout.bars.1 * f64::from(i) / 249.0)
            .collect::<Vec<_>>();

        // A live but silent feed is exactly the flat resting line.
        let mut wave = WaveformWidget::new(Style::Line);
        wave.activate(now);
        for step in 0..HISTORY {
            wave.sample(
                Sample {
                    level: 0,
                    live: true,
                },
                now + Duration::from_millis(50 * step as u64),
            );
        }
        for &x in &sweep {
            assert_eq!(
                wave.line_offset(&layout, x, 0.37),
                0.0,
                "silence must be flat"
            );
        }

        // Loud input ripples within the graph's half-height at every phase.
        let mut wave = WaveformWidget::new(Style::Line);
        wave.activate(now);
        for (step, level) in [8, 22, 5, 41, 67, 88, 30, 12, 55, 73, 19, 96, 44, 27, 60]
            .into_iter()
            .enumerate()
        {
            wave.sample(
                Sample { level, live: true },
                now + Duration::from_millis(50 * step as u64),
            );
        }
        let loudest = sweep
            .iter()
            .flat_map(|&x| [0.0, 0.25, 0.5, 0.75, 1.0].map(|p| wave.line_offset(&layout, x, p)))
            .fold(0.0_f64, |acc, v| acc.max(v.abs()));
        assert!(
            loudest > layout.bar_max_half * 0.5,
            "loud input barely ripples"
        );
        assert!(
            loudest <= layout.bar_max_half + 1e-9,
            "ripple left the area"
        );

        // Quieter input ripples less: the excursion follows the level.
        let mut quiet = WaveformWidget::new(Style::Line);
        quiet.activate(now);
        for (step, level) in [3, 9, 6, 11, 8, 12, 5, 10, 7, 9, 4, 11, 6, 8, 9]
            .into_iter()
            .enumerate()
        {
            quiet.sample(
                Sample { level, live: true },
                now + Duration::from_millis(50 * step as u64),
            );
        }
        let quietest = sweep
            .iter()
            .flat_map(|&x| [0.0, 0.25, 0.5, 0.75, 1.0].map(|p| quiet.line_offset(&layout, x, p)))
            .fold(0.0_f64, |acc, v| acc.max(v.abs()));
        assert!(
            quietest < loudest * 0.5,
            "excursion does not follow the level"
        );

        // The wave stays continuous across a sample boundary: the curve at
        // phase 1 sits exactly where it restarts at phase 0 once the new
        // level lands, at every retained sample position.
        let len = 15;
        let positions = (0..len)
            .map(|i| layout.bars.0 + (HISTORY - len + i - 1) as f64 * layout.slot_w)
            .collect::<Vec<_>>();
        let before: Vec<_> = positions
            .iter()
            .map(|&x| wave.line_offset(&layout, x, 1.0))
            .collect();
        wave.sample(
            Sample {
                level: 33,
                live: true,
            },
            now + Duration::from_millis(50 * len as u64),
        );
        for (i, &x) in positions.iter().enumerate().skip(1) {
            let after = wave.line_offset(&layout, x, 0.0);
            assert!((before[i] - after).abs() < 1e-9, "pop at sample {i}");
        }
    }

    #[test]
    fn the_line_paints_a_continuous_stroke_and_the_bars_style_stays_available() {
        let graph = |surface: &mut ImageSurface| {
            let stride = surface.stride() as usize;
            let data = surface.data().unwrap();
            // Lit-pixel rows per graph-area column: the timer block and dot
            // live left of the graph, so scan only the wave's own area
            // (86..558 for this button width) with an inset on both ends.
            let left = 100;
            let right = 550;
            let mut rows = vec![Vec::new(); (right - left) as usize];
            for x in left..right {
                for y in 0..60 {
                    let i = y as usize * stride + x as usize * 4;
                    let luminance = data[i].max(data[i + 1]).max(data[i + 2]);
                    if luminance > 40 {
                        rows[(x - left) as usize].push(y);
                    }
                }
            }
            rows
        };
        let render = |style: Style, levels: &[u8]| -> ImageSurface {
            let surface = ImageSurface::create(Format::ARgb32, 570, 60).unwrap();
            let now = Instant::now();
            let mut wave = WaveformWidget::new(style);
            wave.activate(now);
            for (step, &level) in levels.iter().enumerate() {
                wave.sample(
                    Sample { level, live: true },
                    now + Duration::from_millis(50 * step as u64),
                );
            }
            let c = Context::new(&surface).unwrap();
            wave.render_at(&c, 60, 0.0, 570.0, 0.0, now + Duration::from_millis(40));
            surface
        };
        let loud = [8, 22, 5, 41, 67, 88, 30, 12, 55, 73, 19, 96, 44, 27, 60];

        // The line is one continuous stroke: every graph column carries
        // ink, and loud input carries it well above and below the baseline.
        let mut line_surface = render(Style::Line, &loud);
        let rows = graph(&mut line_surface);
        assert!(rows.iter().all(|r| !r.is_empty()), "the stroke has gaps");
        assert!(
            rows.iter().any(|r| r.iter().any(|&y| y < 15)),
            "no ink above the baseline"
        );
        assert!(
            rows.iter().any(|r| r.iter().any(|&y| y > 45)),
            "no ink below the baseline"
        );

        // Live silence is a bright flat line hugging the baseline.
        let mut flat_surface = render(Style::Line, &[0; 12]);
        let rows = graph(&mut flat_surface);
        assert!(rows
            .iter()
            .all(|r| r.iter().all(|&y| (25..=35).contains(&y))));
        assert!(
            rows.iter()
                .any(|r| r.iter().any(|&y| (29..=31).contains(&y))),
            "flat line lost its baseline"
        );

        // The bars style still draws its bar graph: same feed, visibly
        // different frame, and columns inside the bar gaps stay empty.
        let mut bars_surface = render(Style::Bars, &loud);
        assert_ne!(
            line_surface.data().unwrap().to_vec(),
            bars_surface.data().unwrap().to_vec()
        );
        let rows = graph(&mut bars_surface);
        assert!(rows.iter().any(|r| r.is_empty()), "bars lost their gaps");
    }

    #[test]
    fn the_phase_advances_with_the_clock_and_rests_at_one() {
        let now = Instant::now();
        let mut wave = WaveformWidget::new(Style::Line);
        assert_eq!(wave.phase(now), 1.0);
        assert!(wave.activate(now));
        assert_eq!(wave.phase(now + Duration::from_secs(3)), 1.0);
        wave.sample(
            Sample {
                level: 60,
                live: true,
            },
            now,
        );
        assert_eq!(wave.phase(now), 0.0);
        assert!((wave.phase(now + Duration::from_millis(25)) - 0.5).abs() < 1e-9);
        assert_eq!(wave.phase(now + SAMPLE_PERIOD), 1.0);
        assert_eq!(wave.phase(now + Duration::from_secs(90)), 1.0);
    }

    #[test]
    fn duration_formats_minutes_and_the_timer_ticks_the_monotonic_clock() {
        assert_eq!(format_duration(Duration::ZERO), "0:00");
        assert_eq!(format_duration(Duration::from_secs(59)), "0:59");
        assert_eq!(format_duration(Duration::from_secs(60)), "1:00");
        assert_eq!(format_duration(Duration::from_secs(667)), "11:07");
        assert_eq!(format_duration(Duration::from_secs(4505)), "75:05");
        let start = Instant::now();
        let mut wave = WaveformWidget::new(Style::Line);
        // Before its layer is shown the widget neither times nor ticks.
        assert!(!wave.is_active());
        assert!(!wave.tick(start + Duration::from_secs(5)));
        assert_eq!(wave.next_timeout(start), i32::MAX - 1);
        assert_eq!(wave.elapsed(start), Duration::ZERO);
        assert!(wave.activate(start));
        assert!(!wave.activate(start));
        assert!(wave.is_active());
        // Half a second in, the boundary is half a second away; with no
        // live feed there is nothing to animate between seconds.
        let now = start + Duration::from_millis(500);
        assert_eq!(wave.next_timeout(now), 500);
        assert!(!wave.frame(now));
        // The displayed second follows the elapsed monotonic time exactly.
        let now = now + Duration::from_millis(501);
        assert_eq!(wave.elapsed(now), Duration::from_millis(1001));
        assert!(wave.frame(now));
        assert!(!wave.frame(now + Duration::from_millis(400)));
        let now = now + Duration::from_millis(1200);
        assert!(wave.frame(now));
        assert_eq!(wave.next_timeout(now), 799);
        // Leaving the layer ends the session; a return starts it fresh.
        wave.deactivate();
        assert!(!wave.is_active());
        assert!(!wave.tick(now + Duration::from_secs(30)));
        assert!(wave.activate(now + Duration::from_secs(30)));
        assert_eq!(wave.elapsed(now + Duration::from_secs(30)), Duration::ZERO);
    }

    #[test]
    fn frames_repaint_while_the_graph_moves_and_settle_when_it_cannot() {
        let now = Instant::now();
        let mut wave = WaveformWidget::new(Style::Line);
        // Samples and frames before activation are ignored.
        assert!(!wave.sample(
            Sample {
                level: 80,
                live: true
            },
            now
        ));
        assert!(!wave.frame(now));
        assert!(wave.activate(now));
        assert!(wave.sample(
            Sample {
                level: 80,
                live: true
            },
            now
        ));
        assert_eq!(wave.history().len(), 1);
        // A live graph is always in motion: every frame repaints and the
        // epoll timeout drops to the frame cadence.
        let mut frame_now = now;
        for _ in 0..5 {
            frame_now += Duration::from_millis(FRAME_INTERVAL_MS as u64);
            assert!(wave.frame(frame_now));
            assert_eq!(wave.next_timeout(frame_now), FRAME_INTERVAL_MS);
        }
        // Even a repeated identical level keeps the graph scrolling; only
        // the sample gate reports no data change of its own.
        assert!(!wave.sample(
            Sample {
                level: 80,
                live: true
            },
            frame_now
        ));
        assert!(wave.frame(frame_now + Duration::from_millis(10)));
        assert_eq!(
            wave.next_timeout(frame_now + Duration::from_millis(10)),
            FRAME_INTERVAL_MS
        );
        assert_eq!(wave.history().len(), 2);
        // A lost feed freezes the scroll; the pulse decays out over a few
        // frames, then the widget rests until the timer's next second.
        assert!(wave.sample(Sample::default(), frame_now + Duration::from_secs(2)));
        let mut settling = 0;
        let mut idle_now = frame_now + Duration::from_secs(2);
        while wave.frame(idle_now) {
            idle_now += Duration::from_millis(FRAME_INTERVAL_MS as u64);
            settling += 1;
            assert!(settling < 100, "pulse never settled");
        }
        assert!(!wave.frame(idle_now));
        assert_eq!(wave.history().len(), 3);
        // Reaching a later second repaints the timer once, then repeated
        // idle samples and frames keep the settled widget quiet.
        let boundary = idle_now + Duration::from_millis(1100);
        assert!(wave.frame(boundary));
        let quiet = boundary + Duration::from_millis(1);
        assert!(!wave.sample(Sample::default(), quiet));
        assert!(!wave.frame(quiet));
    }

    #[test]
    fn level_replies_map_to_samples_and_failures_are_never_live() {
        for (reply, expected) in [
            (
                "ok 42",
                Sample {
                    level: 42,
                    live: true,
                },
            ),
            (
                "ok 100",
                Sample {
                    level: 100,
                    live: true,
                },
            ),
            (
                "ok 0",
                Sample {
                    level: 0,
                    live: true,
                },
            ),
            ("idle", Sample::default()),
            ("error", Sample::default()),
            ("ok 101", Sample::default()),
            ("ok -3", Sample::default()),
            ("ok", Sample::default()),
            ("loud", Sample::default()),
            ("", Sample::default()),
        ] {
            let parsed = parse_level(reply).unwrap_or_default();
            assert_eq!(parsed, expected, "reply {reply:?}");
        }
        let address = address();
        let (server, bound) = script_server(
            address.clone(),
            vec!["ok 42", "idle", "error", "ok 300", "ok", "ok 7 extra"],
        );
        bound.recv_timeout(Duration::from_secs(3)).unwrap();
        let uid = unsafe { libc::geteuid() };
        for expected in [
            Sample {
                level: 42,
                live: true,
            },
            Sample::default(),
            Sample::default(),
            Sample::default(),
            Sample::default(),
            Sample::default(),
        ] {
            assert_eq!(fetch_level(&address, uid), expected);
        }
        server.join().unwrap();
    }

    #[test]
    fn a_receiver_with_the_wrong_uid_is_never_live() {
        let address = address();
        let server_address = address.clone();
        let (ready, bound) = mpsc::channel();
        let server = thread::spawn(move || {
            let listener = UnixListener::bind_addr(&server_address).unwrap();
            ready.send(()).unwrap();
            let (mut connection, _) = listener.accept().unwrap();
            // A rejected caller may drop the connection before the reply.
            let mut request = String::new();
            let _ = (&mut connection).take(32).read_to_string(&mut request);
            let _ = connection.write_all(b"ok 90");
        });
        bound.recv_timeout(Duration::from_secs(3)).unwrap();
        let uid = unsafe { libc::geteuid() }.wrapping_add(1);
        assert_eq!(
            fetch_level_result(&address, uid).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        server.join().unwrap();
    }

    #[test]
    fn the_poller_samples_only_while_armed() {
        let address = address();
        let server_address = address.clone();
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&requests);
        let (ready, bound) = mpsc::channel();
        let server = thread::spawn(move || {
            let listener = UnixListener::bind_addr(&server_address).unwrap();
            ready.send(()).unwrap();
            loop {
                let Ok((mut connection, _)) = listener.accept() else {
                    return;
                };
                connection
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = String::new();
                (&mut connection)
                    .take(32)
                    .read_to_string(&mut request)
                    .unwrap();
                assert_eq!(request, "get");
                counter.fetch_add(1, Ordering::SeqCst);
                connection.write_all(b"idle").unwrap();
            }
        });
        let uid = unsafe { libc::geteuid() };
        let mut client = MicLevels::connect_to(address, uid);
        bound.recv_timeout(Duration::from_secs(3)).unwrap();
        // Disarmed the poller never dials, even well past one interval.
        thread::sleep(POLL_INTERVAL * 6);
        assert_eq!(client.update(), None);
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        // Arming starts the stream; the wake socket delivers each sample.
        client.set_armed(true);
        let deadline = Instant::now() + Duration::from_secs(5);
        while client.update().is_none() {
            assert!(Instant::now() < deadline, "armed poller never sampled");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(requests.load(Ordering::SeqCst) >= 1);
        // Disarming stops the dialing within an interval.
        client.set_armed(false);
        thread::sleep(POLL_INTERVAL * 4);
        let settled = requests.load(Ordering::SeqCst);
        thread::sleep(POLL_INTERVAL * 4);
        assert_eq!(requests.load(Ordering::SeqCst), settled);
        drop(client);
        drop(server);
    }

    #[test]
    fn the_widget_paints_levels_an_idle_baseline_and_interpolation_frames() {
        fn render(wave: &WaveformWidget, now: Instant) -> Vec<u8> {
            let mut surface = ImageSurface::create(Format::ARgb32, 570, 60).unwrap();
            {
                let c = Context::new(&surface).unwrap();
                c.set_source_rgb(1.0, 1.0, 1.0);
                wave.render_at(&c, 60, 0.0, 570.0, 0.0, now);
            }
            let data = surface.data().unwrap().to_vec();
            data
        }
        let now = Instant::now();
        let mut wave = WaveformWidget::new(Style::Line);
        wave.activate(now);
        wave.frame(now + Duration::from_secs(83));
        let idle = render(&wave, now + Duration::from_secs(83));
        assert!(idle.iter().any(|b| *b != 0), "idle widget drew nothing");
        // A live feed with varied levels draws a different frame.
        for (step, level) in [8, 22, 5, 41, 67, 88, 30, 12, 55, 73, 19, 96, 44, 27, 60]
            .into_iter()
            .enumerate()
        {
            wave.sample(
                Sample { level, live: true },
                now + Duration::from_millis(50 * step as u64),
            );
        }
        let last = now + Duration::from_millis(50 * 14);
        let loud = render(&wave, last);
        assert_ne!(idle, loud);
        // Between samples the interpolation repaints: the same history at
        // three phases in one period draws three visibly different frames.
        let early = render(&wave, last + Duration::from_millis(2));
        let mid = render(&wave, last + Duration::from_millis(25));
        let late = render(&wave, last + Duration::from_millis(48));
        assert_ne!(early, mid, "no easing between samples");
        assert_ne!(mid, late, "easing stops mid-period");
        assert_ne!(early, late);
        // Silence after loud bars is visibly calmer but still live.
        for step in 0..HISTORY {
            wave.sample(
                Sample {
                    level: 0,
                    live: true,
                },
                last + Duration::from_millis(50 + 50 * step as u64),
            );
        }
        let quiet = render(
            &wave,
            last + Duration::from_millis(50 + 50 * HISTORY as u64),
        );
        assert_ne!(loud, quiet);
        assert_ne!(idle, quiet);
    }

    #[test]
    fn incremental_repaint_of_one_button_stays_cheap() {
        // Guards the frugal posture: at the frame cadence one button's
        // repaint (erase, dot, timer, bars) must cost well under a frame
        // budget even on this machine's slowest core.
        let surface = ImageSurface::create(Format::ARgb32, 1300, 60).unwrap();
        let mut wave = WaveformWidget::new(Style::Line);
        let now = Instant::now();
        wave.activate(now);
        for step in 0..HISTORY {
            wave.sample(
                Sample {
                    level: (step * 7 % 101) as u8,
                    live: true,
                },
                now + Duration::from_millis(50 * step as u64),
            );
        }
        let mut frame = now + Duration::from_millis(50 * HISTORY as u64);
        let budget = Duration::from_millis(2);
        for _ in 0..100 {
            frame += Duration::from_millis(FRAME_INTERVAL_MS as u64);
            let c = Context::new(&surface).unwrap();
            c.set_source_rgb(0.1, 0.1, 0.1);
            c.rectangle(0.0, 0.0, 1300.0, 60.0);
            c.fill().unwrap();
            let started = Instant::now();
            wave.render_at(&c, 60, 0.0, 1300.0, 0.0, frame);
            let spent = started.elapsed();
            assert!(spent < budget, "repaint took {spent:?}, budget {budget:?}");
        }
    }

    #[test]
    fn the_music_visualizer_keeps_no_timer_and_adds_no_idle_wakes() {
        let now = Instant::now();
        let mut viz = WaveformWidget::new_music(Style::Line);
        assert!(viz.is_music());
        assert!(!WaveformWidget::new(Style::Line).is_music());
        // Off: nothing runs, no timeout is requested, no timer ticks.
        assert!(!viz.tick(now));
        assert_eq!(viz.next_timeout(now), i32::MAX - 1);
        assert!(!viz.frame(now));
        // On with a live feed: the frame clock runs at the shared cadence.
        assert!(viz.activate(now));
        assert!(viz.sample(
            Sample {
                level: 50,
                live: true
            },
            now
        ));
        assert_eq!(viz.next_timeout(now), FRAME_INTERVAL_MS);
        assert!(viz.frame(now + Duration::from_millis(33)));
        // On with a lost feed: no timer boundary keeps it waking; the
        // visualizer rests until the feed returns or it is toggled off.
        assert!(viz.sample(Sample::default(), now + Duration::from_secs(2)));
        let mut idle = now + Duration::from_secs(2);
        while viz.frame(idle) {
            idle += Duration::from_millis(FRAME_INTERVAL_MS as u64);
        }
        assert_eq!(viz.next_timeout(idle), i32::MAX - 1);
        assert!(!viz.tick(idle + Duration::from_secs(30)));
        // Off again ends the session; re-enabling starts one over.
        viz.deactivate();
        assert_eq!(viz.next_timeout(idle), i32::MAX - 1);
        assert!(viz.activate(idle));
        assert!(viz.history().is_empty());
    }

    #[test]
    fn the_music_visualizer_shows_its_off_label_and_spans_the_whole_button() {
        let render = |viz: &WaveformWidget, now: Instant| {
            let mut surface = ImageSurface::create(Format::ARgb32, 570, 60).unwrap();
            {
                let c = Context::new(&surface).unwrap();
                viz.render_at(&c, 60, 0.0, 570.0, 0.0, now);
            }
            let data = surface.data().unwrap().to_vec();
            data
        };
        let lit_columns = |data: &[u8]| {
            let stride = 570 * 4;
            let mut columns = 0;
            for x in 0..570usize {
                (0..60)
                    .any(|y| {
                        let i = y as usize * stride + x * 4;
                        data[i].max(data[i + 1]).max(data[i + 2]) > 40
                    })
                    .then(|| columns += 1);
            }
            columns
        };
        let now = Instant::now();

        // Off: a centered Viz label - ink clustered mid-button, nowhere
        // near the edges, and visibly fewer lit columns than any live line.
        let off = WaveformWidget::new_music(Style::Line);
        let off_frame = render(&off, now);
        assert!(off_frame.iter().any(|b| *b != 0), "off label drew nothing");
        let lit = lit_columns(&off_frame);
        assert!(lit < 120, "the off label must be a small centered label");

        // On with live music: the line spans the whole button, including
        // the left third where the microphone widget reserves its timer.
        let mut viz = WaveformWidget::new_music(Style::Line);
        viz.activate(now);
        for (step, level) in [12, 38, 8, 61, 84, 30, 55, 72, 21, 47, 66, 15]
            .into_iter()
            .enumerate()
        {
            viz.sample(
                Sample { level, live: true },
                now + Duration::from_millis(50 * step as u64),
            );
        }
        let last = now + Duration::from_millis(50 * 11);
        let live_frame = render(&viz, last + Duration::from_millis(40));
        let stride = 570 * 4;
        for x in (16..80usize).step_by(8) {
            assert!(
                (0..60usize).any(|y| {
                    let i = y * stride + x * 4;
                    live_frame[i].max(live_frame[i + 1]).max(live_frame[i + 2]) > 40
                }),
                "no ink in the timer block's columns at x={x}"
            );
        }
        assert!(
            lit_columns(&live_frame) > lit,
            "the live line is narrower than its label"
        );

        // On without a feed: a calm dim baseline across the button.
        let mut stalled = WaveformWidget::new_music(Style::Line);
        stalled.activate(now);
        stalled.sample(Sample::default(), now);
        let stalled_frame = render(&stalled, now + Duration::from_millis(25));
        assert_ne!(stalled_frame, off_frame);
        assert_ne!(stalled_frame, live_frame);

        // The bars style rides the same music layout: visibly different
        // frame, gaps between bars stay empty.
        let mut bars = WaveformWidget::new_music(Style::Bars);
        bars.activate(now);
        for (step, level) in [12, 38, 8, 61, 84, 30, 55, 72, 21, 47, 66, 15]
            .into_iter()
            .enumerate()
        {
            bars.sample(
                Sample { level, live: true },
                now + Duration::from_millis(50 * step as u64),
            );
        }
        let bars_frame = render(&bars, last + Duration::from_millis(40));
        assert_ne!(bars_frame, live_frame);
    }

    #[test]
    fn the_visualizer_poller_handshakes_the_capture_lifecycle() {
        let address = address();
        let server_address = address.clone();
        let commands = Arc::new(Mutex::new(Vec::<String>::new()));
        let observed = Arc::clone(&commands);
        let (ready, bound) = mpsc::channel();
        let server = thread::spawn(move || {
            let listener = UnixListener::bind_addr(&server_address).unwrap();
            ready.send(()).unwrap();
            loop {
                let Ok((mut connection, _)) = listener.accept() else {
                    return;
                };
                connection
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = String::new();
                (&mut connection)
                    .take(32)
                    .read_to_string(&mut request)
                    .unwrap();
                let reply = match request.as_str() {
                    "on" | "off" => "ok",
                    "get" => "ok 42",
                    _ => "error",
                };
                observed.lock().unwrap().push(request);
                connection.write_all(reply.as_bytes()).unwrap();
            }
        });
        let uid = unsafe { libc::geteuid() };
        let mut client = MonitorLevels::connect_to(address, uid);
        bound.recv_timeout(Duration::from_secs(3)).unwrap();
        // Disarmed, the poller neither starts nor polls anything.
        thread::sleep(POLL_INTERVAL * 6);
        assert!(commands.lock().unwrap().is_empty());
        assert_eq!(client.update(), None);
        // Arming first starts the capture, then streams levels.
        client.set_armed(true);
        let deadline = Instant::now() + Duration::from_secs(5);
        while client.update()
            != Some(Sample {
                level: 42,
                live: true,
            })
        {
            assert!(Instant::now() < deadline, "armed poller never sampled");
            thread::sleep(Duration::from_millis(5));
        }
        {
            let commands = commands.lock().unwrap();
            assert_eq!(commands.first().map(String::as_str), Some("on"));
            assert!(commands.iter().filter(|c| c.as_str() == "get").count() >= 1);
            assert!(!commands.contains(&"off".to_string()));
        }
        // Disarming stops the capture promptly; no further poll runs.
        client.set_armed(false);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !commands.lock().unwrap().contains(&"off".to_string()) {
            assert!(
                Instant::now() < deadline,
                "disarm never stopped the capture"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let settled = commands.lock().unwrap().len();
        thread::sleep(POLL_INTERVAL * 4);
        assert_eq!(commands.lock().unwrap().len(), settled);
        drop(client);
        drop(server);
    }

    #[test]
    fn a_lost_consumer_stops_the_capture_before_the_poller_exits() {
        let address = address();
        let server_address = address.clone();
        let commands = Arc::new(Mutex::new(Vec::<String>::new()));
        let observed = Arc::clone(&commands);
        let (ready, bound) = mpsc::channel();
        let server = thread::spawn(move || {
            let listener = UnixListener::bind_addr(&server_address).unwrap();
            ready.send(()).unwrap();
            for _ in 0..8 {
                let Ok((mut connection, _)) = listener.accept() else {
                    return;
                };
                connection
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = String::new();
                (&mut connection)
                    .take(32)
                    .read_to_string(&mut request)
                    .unwrap();
                observed.lock().unwrap().push(request.clone());
                let reply = match request.as_str() {
                    "on" | "off" => "ok",
                    _ => "ok 7",
                };
                connection.write_all(reply.as_bytes()).unwrap();
            }
        });
        let uid = unsafe { libc::geteuid() };
        let mut client = MonitorLevels::connect_to(address, uid);
        bound.recv_timeout(Duration::from_secs(3)).unwrap();
        client.set_armed(true);
        let deadline = Instant::now() + Duration::from_secs(5);
        while client.update().is_none() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        // Dropping the daemon end (a crash, or shutdown) must leave the
        // capture stopped, not streaming to a vanished consumer: after the
        // in-flight poll fails to deliver, the poller sends `off` and exits.
        drop(client);
        let deadline = Instant::now() + Duration::from_secs(5);
        while commands.lock().unwrap().last().map(String::as_str) != Some("off") {
            assert!(
                Instant::now() < deadline,
                "poller never stopped the capture"
            );
            thread::sleep(Duration::from_millis(5));
        }
        drop(server);
    }
}
