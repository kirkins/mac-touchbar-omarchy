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

/// Input-level samples kept for the bar graph; at the poll cadence below
/// this is a bit over two seconds of recent audio.
pub const HISTORY: usize = 48;
/// How often the armed poller dials the level endpoint.
pub const POLL_INTERVAL: Duration = Duration::from_millis(50);
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
/// Per-sample decay of the dot's pulse driver.
const PULSE_DECAY: f32 = 0.6;
/// Repaint only when the smoothed pulse moved this much.
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

    #[cfg(test)]
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

/// The mirrored bar one history sample draws: `(x, y, width, height)`,
/// fully inside the layout's bar area.
pub fn bar_rect(layout: &Layout, index: usize, level: u8) -> (f64, f64, f64, f64) {
    let x = layout.bars.0 + index as f64 * layout.slot_w;
    let width = (layout.slot_w - BAR_GAP).max(1.0);
    let half = MIN_BAR + (layout.bar_max_half - MIN_BAR) * f64::from(level) / 100.0;
    (x, layout.mid_y - half, width, half * 2.0)
}

/// `m:ss` with unbounded minutes, matching the clock's legibility.
pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// State of one recording-page widget. The timer counts from entering the
/// layer carrying the widget; the history and pulse follow the live feed.
pub struct WaveformWidget {
    history: LevelHistory,
    started: Option<Instant>,
    live: bool,
    /// Peak-following pulse driver for the recording dot.
    smoothed: f32,
    /// Second currently shown on the timer.
    shown_second: u64,
}

impl WaveformWidget {
    pub fn new() -> Self {
        Self {
            history: LevelHistory::new(),
            started: None,
            live: false,
            smoothed: 0.0,
            shown_second: 0,
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
        self.shown_second = 0;
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

    /// Fold one poll result in. Returns whether the visible widget changed.
    pub fn sample(&mut self, sample: Sample) -> bool {
        if self.started.is_none() {
            return false;
        }
        let before = (self.history.newest(), self.live, self.smoothed);
        self.live = sample.live;
        self.history.push(sample.level);
        self.smoothed = f32::from(sample.level).max(self.smoothed * PULSE_DECAY);
        Some(sample.level) != before.0
            || sample.live != before.1
            || (self.smoothed - before.2).abs() > PULSE_EPSILON
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

    /// Milliseconds until the displayed second changes, for the shared
    /// epoll timeout; `i32::MAX - 1` while inactive.
    pub fn next_timeout(&self, now: Instant) -> i32 {
        let Some(start) = self.started else {
            return i32::MAX - 1;
        };
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
        let label = format_duration(self.elapsed(Instant::now()));
        c.set_source_rgb(1.0, 1.0, 1.0);
        c.set_font_size(TIMER_FONT);
        let extents = c.text_extents(&label).unwrap();
        c.move_to(
            left + layout.text_x,
            cy - extents.y_bearing() - extents.height() / 2.0,
        );
        c.show_text(&label).unwrap();
        // The scrolling level graph - the newest bar sits at the right edge
        // and older history scrolls left - or a flat dim baseline while idle.
        if self.live {
            let len = self.history.len();
            for (i, level) in self.history.oldest_first().enumerate() {
                let (x, y, w, h) = bar_rect(&layout, HISTORY - len + i, level);
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
            for index in 0..HISTORY {
                let (x, y, w, h) = bar_rect(&layout, index, level);
                assert!(x >= layout.bars.0, "bar left of the area");
                assert!(
                    x + w <= layout.bars.0 + layout.bars.1 + 1e-6,
                    "bar past the area"
                );
                assert!(y >= layout.mid_y - layout.bar_max_half - 1e-6);
                assert!(y + h <= layout.mid_y + layout.bar_max_half + 1e-6);
                assert!(h >= MIN_BAR * 2.0);
            }
            let height = bar_rect(&layout, 0, level).3;
            assert!(height >= previous, "heights must not shrink with level");
            previous = height;
        }
        // Neighboring slots never overlap and silence still draws a pixel pair.
        let (x0, _, w0, h0) = bar_rect(&layout, 0, 0);
        let (x1, _, _, _) = bar_rect(&layout, 1, 0);
        assert!(x1 >= x0 + w0);
        assert!(h0 >= MIN_BAR * 2.0);
        assert!(bar_rect(&layout, 0, 100).3 > bar_rect(&layout, 0, 0).3);
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
        // Half a second in, the boundary is half a second away.
        let now = start + Duration::from_millis(500);
        assert_eq!(wave.next_timeout(now), 500);
        assert!(!wave.tick(now));
        // The displayed second follows the elapsed monotonic time exactly.
        let now = now + Duration::from_millis(501);
        assert_eq!(wave.elapsed(now), Duration::from_millis(1001));
        assert!(wave.tick(now));
        assert!(!wave.tick(now + Duration::from_millis(400)));
        let now = now + Duration::from_millis(1200);
        assert!(wave.tick(now));
        assert_eq!(wave.next_timeout(now), 799);
        // Leaving the layer ends the session; a return starts it fresh.
        wave.deactivate();
        assert!(!wave.is_active());
        assert!(!wave.tick(now + Duration::from_secs(30)));
        assert!(wave.activate(now + Duration::from_secs(30)));
        assert_eq!(wave.elapsed(now + Duration::from_secs(30)), Duration::ZERO);
    }

    #[test]
    fn widget_repaints_only_for_visible_changes() {
        let now = Instant::now();
        let mut wave = WaveformWidget::new();
        // Samples before activation are ignored.
        assert!(!wave.sample(Sample {
            level: 80,
            live: true
        }));
        wave.activate(now);
        assert!(wave.sample(Sample {
            level: 80,
            live: true
        }));
        assert_eq!(wave.history().len(), 1);
        // Identical level, live, and settled pulse: nothing to repaint.
        assert!(!wave.sample(Sample {
            level: 80,
            live: true
        }));
        // A lower level still repaints while the pulse decays toward it.
        assert!(wave.sample(Sample {
            level: 20,
            live: true
        }));
        while wave.sample(Sample {
            level: 20,
            live: true,
        }) {}
        assert!(!wave.sample(Sample {
            level: 20,
            live: true
        }));
        assert_eq!(wave.history().len(), 7);
        // A lost feed repaints the idle look immediately, then stays quiet
        // once the pulse has faded.
        assert!(wave.sample(Sample::default()));
        while wave.sample(Sample::default()) {}
        assert!(!wave.sample(Sample::default()));
        assert!(wave.history().len() >= 9);
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
    fn the_widget_paints_levels_and_an_idle_baseline_inside_its_button() {
        fn render(wave: &WaveformWidget) -> Vec<u8> {
            let mut surface = ImageSurface::create(Format::ARgb32, 570, 60).unwrap();
            {
                let c = Context::new(&surface).unwrap();
                c.set_source_rgb(1.0, 1.0, 1.0);
                wave.render(&c, 60, 0.0, 570.0, 0.0);
            }
            let data = surface.data().unwrap().to_vec();
            data
        }
        let now = Instant::now();
        let mut wave = WaveformWidget::new();
        wave.activate(now);
        wave.tick(now + Duration::from_secs(83));
        let idle = render(&wave);
        assert!(idle.iter().any(|b| *b != 0), "idle widget drew nothing");
        // A live feed with varied levels draws a different frame.
        for level in [8, 22, 5, 41, 67, 88, 30, 12, 55, 73, 19, 96, 44, 27, 60] {
            wave.sample(Sample { level, live: true });
        }
        let loud = render(&wave);
        assert_ne!(idle, loud);
        // Silence after loud bars is visibly calmer but still live.
        for _ in 0..HISTORY {
            wave.sample(Sample {
                level: 0,
                live: true,
            });
        }
        let quiet = render(&wave);
        assert_ne!(loud, quiet);
        assert_ne!(idle, quiet);
    }
}
