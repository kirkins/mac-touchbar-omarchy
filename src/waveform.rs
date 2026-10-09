//! Live microphone input visualization for the recording layer.
//!
//! A button configured with `Waveform = true` renders a scrolling bar graph
//! of recent input levels beside a recording duration timer, in place of the
//! old static `● Recording` label. While its layer is displayed the daemon
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
//! widget's frame clock ([`FRAME_INTERVAL_MS`], roughly 30 fps) scrolls
//! the graph fractionally leftward and eases the newest bar from the
//! previous level toward its measured one, so bars flow instead of
//! snapping a full slot every 50 ms. Frames render only while the
//! recording layer is displayed and only while something is in motion;
//! an idle, settled widget still wakes at most once per second for the
//! timer. The level feed itself stays as fresh as the desktop helper's
//! capture tail - see `integrations/voice/README.md`.

use cairo::Context;
use std::{
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
/// Gap between the pulsing dot and the duration label.
const TEXT_GAP: f64 = 12.0;
/// Width reserved on the left for dot plus duration label.
const TIMER_BLOCK: f64 = 86.0;
const TIMER_FONT: f64 = 22.0;
/// Bars keep this much clearance from the button's inset fill edges.
const BARS_MARGIN: f64 = 10.0;
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

/// The level the newest bar displays at `phase` into the current sample
/// period: it rises from the previous sample's level, which is what the
/// bar showed the instant the previous period ended, so heights stay
/// continuous across sample boundaries.
pub fn display_level(previous: u8, current: u8, phase: f64) -> f64 {
    let t = smoothstep(phase);
    f64::from(previous) + (f64::from(current) - f64::from(previous)) * t
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
    previous_level: u8,
    /// When the last frame advanced the animation clock.
    last_frame: Option<Instant>,
}

impl WaveformWidget {
    pub fn new() -> Self {
        Self {
            history: LevelHistory::new(),
            started: None,
            live: false,
            smoothed: 0.0,
            shown_pulse: 0.0,
            shown_second: 0,
            last_sample: None,
            previous_level: 0,
            last_frame: None,
        }
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
        self.previous_level = 0;
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
        self.previous_level = self.history.newest().unwrap_or(0);
        self.live = sample.live;
        self.history.push(sample.level);
        self.last_sample = Some(now);
        self.smoothed = f32::from(sample.level).max(self.smoothed);
        Some(sample.level) != before.0 || sample.live != before.1
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
        let pulse_settling = (self.smoothed - self.shown_pulse).abs() > PULSE_EPSILON;
        let ticked = self.tick(now);
        let moved = scrolling || pulse_settling || ticked;
        if moved {
            self.shown_pulse = self.smoothed;
        }
        moved
    }

    /// Advance the duration timer to `now`. Returns whether the displayed
    /// second changed and the widget must repaint.
    pub fn tick(&mut self, now: Instant) -> bool {
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
    /// idle widget wakes only for the timer's second boundary; an inactive
    /// one (`i32::MAX - 1`) never runs fast.
    pub fn next_timeout(&self, now: Instant) -> i32 {
        let Some(start) = self.started else {
            return i32::MAX - 1;
        };
        if (self.live && !self.history.is_empty()) || self.smoothed > PULSE_EPSILON {
            return FRAME_INTERVAL_MS;
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
        let layout = layout(width, height as f64);
        let cy = y_shift + layout.mid_y;
        // The recording dot pulses with live level; idle it stays small and dim.
        let radius = DOT_R_BASE + DOT_R_SPAN * f64::from(self.smoothed) / 100.0;
        let (r, g, b) = if self.live {
            (1.0, 0.32, 0.42)
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
        // The scrolling level graph - the newest bar eases in at the right
        // edge while older history glides left between samples, or a flat
        // dim baseline while idle. The fractional phase keeps positions
        // continuous across sample boundaries: a bar one slot right at
        // phase 0 is exactly where its predecessor ended at phase 1.
        if self.live {
            let len = self.history.len();
            let phase = self.phase(now);
            for (i, level) in self.history.oldest_first().enumerate() {
                let level = if i + 1 == len {
                    display_level(self.previous_level, level, phase)
                } else {
                    f64::from(level)
                };
                let slot = (HISTORY - len + i) as f64 - phase;
                let (x, y, w, h) = bar_rect(&layout, slot, level);
                c.rectangle(left + x, y_shift + y, w, h);
            }
            c.fill().unwrap();
        } else {
            c.set_source_rgb(0.42, 0.42, 0.42);
            c.rectangle(left + layout.bars.0, cy - 0.5, layout.bars.1, 1.0);
            c.fill().unwrap();
        }
    }
}

impl Default for WaveformWidget {
    fn default() -> Self {
        Self::new()
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
    let mut connection = UnixStream::connect_addr(address)?;
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
            "Unexpected voice level receiver UID",
        ));
    }
    connection.write_all(b"get")?;
    connection.shutdown(Shutdown::Write)?;
    let mut response = String::new();
    connection.take(32).read_to_string(&mut response)?;
    parse_level(&response)
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

#[cfg(test)]
mod tests {
    use super::*;
    use cairo::{Format, ImageSurface};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        let mut previous = display_level(20, 80, 0.0);
        for step in 1..=10 {
            let phase = f64::from(step) / 10.0;
            let level = display_level(20, 80, phase);
            assert!(level >= previous, "ease must not dip");
            assert!(level <= 80.0);
            previous = level;
        }
        assert_eq!(display_level(20, 80, 0.0), 20.0);
        assert_eq!(display_level(20, 80, 1.0), 80.0);
        assert_eq!(display_level(20, 80, 2.0), 80.0);

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
        let widget_height = |phase| bar_rect(&layout, 0.0, display_level(40, 70, phase)).3;
        assert_eq!(widget_height(0.0), bar_rect(&layout, 0.0, 40.0).3);
        assert_eq!(widget_height(1.0), bar_rect(&layout, 0.0, 70.0).3);
        assert!(widget_height(0.5) > widget_height(0.0));
    }

    #[test]
    fn the_phase_advances_with_the_clock_and_rests_at_one() {
        let now = Instant::now();
        let mut wave = WaveformWidget::new();
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
        let mut wave = WaveformWidget::new();
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
        let mut wave = WaveformWidget::new();
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
        let mut wave = WaveformWidget::new();
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
        let mut wave = WaveformWidget::new();
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
}
