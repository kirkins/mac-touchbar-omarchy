//! Now-playing text for the media bar.
//!
//! A button configured with `NowPlaying = true` shows what is actually
//! sounding: the playing track's title - and, when the player reports one
//! and the row has the room, the artist as a caption beneath it - in place
//! of a static label. While its layer is displayed the daemon polls the
//! desktop now-playing receiver's endpoint - the same abstract AF_UNIX and
//! kernel peer-UID boundary as the volume, brightness, voice, and
//! visualizer receivers - at a slow cadence, because track metadata
//! changes on the scale of songs, not frames; a changed title repaints
//! only that button. Everywhere else the poller is disarmed and no socket
//! is dialed.
//!
//! The feed is deliberately coarse: every poll replaces the whole display,
//! so the row can never hold a stale track. A player that pauses or
//! stops, a missing or garbling receiver, a bus failure, malformed or
//! unusable metadata - all land in one graceful empty state, a dim
//! `Nothing playing` label, never a broken row and never the previous
//! song pretending to play. A receiver that is not installed at all is
//! the normal case: the empty state shows as everywhere else and the
//! daemon logs one line per arm so the missing helper stays diagnosable
//! from the journal, like the visualizer's.
//!
//! The desktop helper is `integrations/now-playing/dfr-now-playing`: it
//! asks the session bus for the MPRIS player that is playing (falling
//! back to the first player with usable metadata, so its paused/stopped
//! state is what the daemon sees) and answers one-shot requests with
//!
//! - `ok <state>\u{1F}<title>\u{1F}<artist>` - state `playing`, `paused`,
//!   or `stopped`; either text may be empty,
//! - `idle` - no MPRIS player or no usable title,
//! - `error` - malformed request or bus failure.
//!
//! Metadata is display text, not bytes to trust: the helper and the
//! parser both strip control and format characters (a soft hyphen in a
//! real artist name arrived on the target machine), collapse whitespace,
//! and cap lengths. Only `playing` ever draws a track.

use crate::appearance::LABEL_FONT;
use crate::desktop::Endpoint;
use cairo::Context;
use std::{
    io::{self, Read, Write},
    net::Shutdown,
    os::{
        fd::AsRawFd,
        unix::net::{SocketAddr, UnixStream},
    },
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
/// How often the armed poller asks the receiver for the current track.
/// Metadata changes on the scale of songs, so one second keeps the row
/// live on track changes without waking anything per frame.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// A reply must arrive within this window or the poll counts as empty:
/// the session bus answers property reads in well under a millisecond.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(500);
/// Upper bound for one reply read: the helper caps each field, so this
/// only bounds a misbehaving peer, never clips a real track.
const MAX_REPLY: u64 = 512;
/// The helper's per-field cap, enforced again parser-side: long enough
/// for any real title, short enough that the row's fit logic never sees
/// absurd text.
const MAX_FIELD_CHARS: usize = 120;
/// The field separator of the wire protocol: ASCII unit separator, which
/// sanitized metadata can never contain.
const SEPARATOR: char = '\u{1F}';
/// Longest track title the row draws; beyond this the label carries an
/// ellipsis and the render still fits what remains to the button.
pub const MAX_TITLE_CHARS: usize = 32;
/// Longest artist caption; the artist is the second tier and yields room
/// before the title ever competes with the row's width.
pub const MAX_ARTIST_CHARS: usize = 24;
/// The graceful empty state's label, drawn dim: nothing playing, nothing
/// stale, nothing broken.
const EMPTY_LABEL: &str = "Nothing playing";
/// The artist caption's size, the deliberate second tier below the bar's
/// primary label size - the same hierarchy as the battery's caption.
const CAPTION_FONT: f64 = 12.0;
/// Horizontal padding the fit logic keeps clear on either side.
const PAD: f64 = 10.0;
/// The dim gray of informational quiet: the empty state and the idle
/// baseline share it, and neither reads as an alert.
const DIM: f64 = 0.42;

/// One track's display metadata, already sanitized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    pub title: String,
    pub artist: Option<String>,
}

/// What one poll said. Only [`Feed::Playing`] ever draws a track; every
/// other outcome - paused, stopped, no player, no metadata, an
/// unreachable or garbling receiver - folds into [`Feed::Idle`] so the
/// row shows the empty state instead of a stale track.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Feed {
    Playing(Track),
    Idle,
}

/// The label a track renders as: a title line plus an optional artist
/// caption, each already truncated to its tier's budget.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Label {
    pub title: String,
    pub artist: Option<String>,
}

/// Truncate to a character budget with an ellipsis, never splitting a
/// character and never leaving trailing whitespace before it.
pub fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut kept: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    while kept.ends_with(char::is_whitespace) {
        kept.pop();
    }
    kept.push('…');
    kept
}

/// Strip characters that have no place in display text: control
/// characters, format characters a real library sometimes ships (the
/// soft hyphen arrived on the target machine inside an artist name),
/// and the protocol's own separator. Whitespace runs collapse to one
/// space and the result is capped at [`MAX_FIELD_CHARS`].
pub fn sanitize(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .filter(|c| {
            !c.is_control()
                && *c != SEPARATOR
                && !matches!(
                    c,
                    '\u{00AD}' | '\u{200B}'..='\u{200F}' | '\u{2028}' | '\u{2029}' | '\u{FEFF}'
                )
        })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&collapsed, MAX_FIELD_CHARS)
}

/// Parse one receiver reply. Any surprise - an unknown state word, a
/// field count that does not match the protocol, garbage, an empty
/// read - is [`Feed::Idle`], the same graceful empty state every other
/// failure produces. A `playing` reply needs a nonempty title to count;
/// the artist is optional.
pub fn parse_feed(response: &str) -> Feed {
    let fields: Vec<&str> = response.split(SEPARATOR).collect();
    let [head, title, artist] = fields.as_slice() else {
        return Feed::Idle;
    };
    let mut words = head.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        // Only `playing` ever draws a track, and it needs a nonempty
        // title; the artist is optional.
        (Some("ok"), Some("playing"), None) => {
            let title = sanitize(title);
            if title.is_empty() {
                return Feed::Idle;
            }
            let artist = sanitize(artist);
            let artist = (!artist.is_empty()).then_some(artist);
            Feed::Playing(Track { title, artist })
        }
        // Paused and stopped are nothing playing: the empty state, never
        // the track pretending to play.
        (Some("ok"), Some("paused" | "stopped"), None) => Feed::Idle,
        _ => Feed::Idle,
    }
}

/// Compose the two display tiers: the title, and the artist when the
/// player reports one. Both tiers are truncated to their own budget, so
/// a long title never crowds the artist line off the bar and vice versa.
pub fn label_for(track: &Track) -> Label {
    let title = truncate(track.title.trim(), MAX_TITLE_CHARS);
    let artist = track
        .artist
        .as_deref()
        .map(str::trim)
        .filter(|artist| !artist.is_empty())
        .map(|artist| truncate(artist, MAX_ARTIST_CHARS));
    Label { title, artist }
}

/// Shared arm flag the render loop writes and the poller waits on.
struct Arm {
    armed: Mutex<bool>,
    signal: Condvar,
}

/// Client for the now-playing receiver's endpoint. The poller thread
/// dials the abstract socket only while armed, at the slow metadata
/// cadence; each result wakes the shared epoll through `wake`, exactly
/// like the level, visualizer, and battery workers.
pub struct NowPlaying {
    arm: Arc<Arm>,
    feeds: Receiver<Feed>,
    pub wake: UnixStream,
}

impl NowPlaying {
    /// Connect to the desktop user's receiver, mirroring the volume,
    /// brightness, voice, and visualizer endpoints.
    pub fn client() -> Self {
        Self::connect_to(Endpoint::Desktop("dfr-now-playing"))
    }

    fn connect_to(endpoint: Endpoint) -> Self {
        let arm = Arc::new(Arm {
            armed: Mutex::new(false),
            signal: Condvar::new(),
        });
        let (outgoing, feeds) = mpsc::channel();
        let (wake, mut notify) = UnixStream::pair().unwrap();
        wake.set_nonblocking(true).unwrap();
        notify.set_nonblocking(true).unwrap();
        let poller_arm = Arc::clone(&arm);
        thread::spawn(move || now_playing_poller(endpoint, &poller_arm, &outgoing, &mut notify));
        Self { arm, feeds, wake }
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

    /// Drain pending results and return the freshest feed, if any.
    pub fn update(&mut self) -> Option<Feed> {
        while matches!(self.wake.read(&mut [0; 64]), Ok(n) if n > 0) {}
        self.feeds.try_iter().last()
    }
}

/// Dial the endpoint only while armed, at the metadata cadence, pushing
/// every result - including the empty state a failed poll produces - to
/// the render loop, so the row never holds a track the receiver no
/// longer reports.
fn now_playing_poller(
    endpoint: Endpoint,
    arm: &Arm,
    feeds: &Sender<Feed>,
    notify: &mut UnixStream,
) {
    let mut armed = false;
    let mut deadline: Option<Instant> = None;
    // A receiver that is not installed logs exactly once per arm: the
    // missing-helper case stays diagnosable without per-second spam.
    let mut said_unreachable = false;
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
            said_unreachable = false;
        }
        if !armed {
            continue;
        }
        let (address, uid) = endpoint.resolve();
        let feed = match fetch_feed(&address, uid) {
            Ok(feed) => feed,
            Err(error) => {
                if error.kind() == io::ErrorKind::ConnectionRefused && !said_unreachable {
                    said_unreachable = true;
                    eprintln!(
                        "tiny-dfr: now-playing receiver unavailable; \
                         install and enable integrations/now-playing/dfr-now-playing"
                    );
                }
                Feed::Idle
            }
        };
        if feeds.send(feed).is_err() {
            return;
        }
        let _ = notify.write(&[1]);
    }
}

/// Ask the receiver for one now-playing observation. A transport or
/// validation failure is an error only to let the poller distinguish the
/// unreachable receiver for its one-line log; every caller maps a
/// failure to [`Feed::Idle`], never to a stale track.
fn fetch_feed(address: &SocketAddr, uid: u32) -> io::Result<Feed> {
    let mut connection = connect_verified(address, uid)?;
    connection.write_all(b"get")?;
    connection.shutdown(Shutdown::Write)?;
    let mut response = String::new();
    connection.take(MAX_REPLY).read_to_string(&mut response)?;
    Ok(parse_feed(&response))
}

/// Connect to the receiver's abstract socket and verify the
/// kernel-supplied peer UID - the same boundary every receiver shares.
fn connect_verified(address: &SocketAddr, uid: u32) -> io::Result<UnixStream> {
    let connection = UnixStream::connect_addr(address)?;
    connection.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    connection.set_write_timeout(Some(REQUEST_TIMEOUT))?;
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
            "Unexpected now-playing receiver UID",
        ));
    }
    Ok(connection)
}

/// The media bar's now-playing row: the current track's label, or none
/// while nothing is playing. Every poll replaces the whole label, so a
/// track change lands live and a stopped player clears the row instead
/// of leaving the last song behind.
#[derive(Default)]
pub struct NowPlayingWidget {
    label: Option<Label>,
}

impl NowPlayingWidget {
    /// Apply one poll result; returns whether the button needs a repaint.
    pub fn apply(&mut self, feed: &Feed) -> bool {
        let label = match feed {
            Feed::Playing(track) => Some(label_for(track)),
            Feed::Idle => None,
        };
        if self.label == label {
            return false;
        }
        self.label = label;
        true
    }

    /// Drop the label entirely - used when the layer is left, so a later
    /// return starts from the next poll, never from an old track.
    pub fn reset(&mut self) -> bool {
        if self.label.is_none() {
            return false;
        }
        self.label = None;
        true
    }

    pub fn render(&self, c: &Context, height: i32, left: f64, width: f64, y_shift: f64) {
        match &self.label {
            Some(label) => match label.artist.as_deref().filter(|a| !a.is_empty()) {
                Some(artist) => {
                    // Two tiers, the battery's caption hierarchy: the
                    // title at the primary label size above, the artist
                    // as a caption beneath it.
                    draw_fitted(
                        c,
                        &label.title,
                        left,
                        width,
                        y_shift + height as f64 * 0.34,
                        LABEL_FONT,
                        1.0,
                    );
                    draw_fitted(
                        c,
                        artist,
                        left,
                        width,
                        y_shift + height as f64 * 0.68,
                        CAPTION_FONT,
                        1.0,
                    );
                }
                None => {
                    // One line, vertically centered like every other
                    // single-line button label.
                    draw_fitted(
                        c,
                        &label.title,
                        left,
                        width,
                        y_shift + height as f64 / 2.0,
                        LABEL_FONT,
                        1.0,
                    );
                }
            },
            None => {
                // The graceful empty state: a dim label at the primary
                // size, clearly intentional, never a broken row.
                draw_fitted(
                    c,
                    EMPTY_LABEL,
                    left,
                    width,
                    y_shift + height as f64 / 2.0,
                    LABEL_FONT,
                    DIM,
                );
            }
        }
    }
}

/// Draw one centered label at `font`, shrunk only as far as the
/// established fit logic shrinks - until it fits the padded width - and
/// never grown. `tone` is the gray level: 1.0 for content, [`DIM`] for
/// the quiet empty state.
fn draw_fitted(c: &Context, text: &str, left: f64, width: f64, cy: f64, font: f64, tone: f64) {
    c.save().unwrap();
    c.set_source_rgb(tone, tone, tone);
    c.set_font_size(font);
    let mut extents = c.text_extents(text).unwrap();
    if extents.width() > 0.0 {
        let scale = ((width - 2.0 * PAD).max(1.0) / extents.width()).min(1.0);
        c.set_font_size(font * scale);
        extents = c.text_extents(text).unwrap();
    }
    c.move_to(
        left + (width - extents.width()) / 2.0 - extents.x_bearing(),
        cy - extents.y_bearing() - extents.height() / 2.0,
    );
    c.show_text(text).unwrap();
    c.restore().unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::UnixListener;

    /// A unique abstract test address, never the production one.
    fn test_address(tag: &str) -> SocketAddr {
        let name = format!(
            "dfr-now-playing-test-{}-{}-{}",
            tag,
            std::process::id(),
            rand::random::<u64>()
        );
        assert_ne!(name, "dfr-now-playing-1000");
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
    fn the_protocol_parses_playing_paused_and_empty_replies() {
        // A full playing reply carries both tiers.
        assert_eq!(
            parse_feed("ok playing\u{1F}Lucky Star\u{1F}Weaban"),
            Feed::Playing(Track {
                title: "Lucky Star".into(),
                artist: Some("Weaban".into()),
            })
        );
        // An empty artist field is no artist, not an empty caption.
        assert_eq!(
            parse_feed("ok playing\u{1F}Song only\u{1F}"),
            Feed::Playing(Track {
                title: "Song only".into(),
                artist: None,
            })
        );
        // Whitespace around fields is transport noise, not content.
        assert_eq!(
            parse_feed("ok playing \u{1F} Spaced \u{1F} Out "),
            Feed::Playing(Track {
                title: "Spaced".into(),
                artist: Some("Out".into()),
            })
        );
        // Paused and stopped are nothing playing: the empty state, never
        // the track pretending to play.
        for state in ["paused", "stopped"] {
            assert_eq!(
                parse_feed(&format!("ok {state}\u{1F}Old track\u{1F}Old artist")),
                Feed::Idle
            );
        }
        assert_eq!(parse_feed("idle"), Feed::Idle);
    }

    #[test]
    fn unusable_playing_replies_degrade_to_the_empty_state() {
        // A playing reply without a title has nothing to draw.
        assert_eq!(parse_feed("ok playing\u{1F}\u{1F}Artist"), Feed::Idle);
        assert_eq!(parse_feed("ok playing\u{1F}  \u{1F}Artist"), Feed::Idle);
        // Unknown state words, wrong head, wrong field counts, garbage,
        // and empty reads all fold into the same empty state: the row is
        // never broken and never stale.
        for reply in [
            "ok resumed\u{1F}Song\u{1F}Artist",
            "fine playing\u{1F}Song\u{1F}Artist",
            "ok playing\u{1F}Song",
            "ok playing\u{1F}Song\u{1F}Artist\u{1F}extra",
            "playing\u{1F}Song\u{1F}Artist",
            "error",
            "",
            "\n",
            "totally unexpected text",
        ] {
            assert_eq!(parse_feed(reply), Feed::Idle, "reply: {reply:?}");
        }
    }

    #[test]
    fn sanitized_metadata_strips_controls_formats_and_separator() {
        // The real-world case: a soft hyphen inside an artist name.
        assert_eq!(sanitize("Weaban \u{AD}"), "Weaban");
        // Control characters, the protocol separator, zero-width format
        // characters, and whitespace runs never reach the label.
        assert_eq!(sanitize("A\u{7}B\u{1F}C\u{200B}D"), "ABCD");
        assert_eq!(sanitize("  many   spaces \t here "), "many spaces here");
        // Real text survives: punctuation, accents, emoji, CJK.
        assert_eq!(
            sanitize("Sigur Rós - ( ) — 🎵 / アーティスト"),
            "Sigur Rós - ( ) — 🎵 / アーティスト"
        );
        // The field cap bounds a misbehaving peer.
        let long = "x".repeat(500);
        assert_eq!(sanitize(&long).chars().count(), MAX_FIELD_CHARS);
    }

    #[test]
    fn the_label_truncates_each_tier_on_character_boundaries() {
        // Title and artist join only through their own tiers; an empty
        // or whitespace artist is no artist.
        let label = label_for(&Track {
            title: "Song".into(),
            artist: Some("  ".into()),
        });
        assert_eq!(label.title, "Song");
        assert_eq!(label.artist, None);
        // Exact-budget text stays whole; one over it gains an ellipsis.
        let exactly = "x".repeat(MAX_TITLE_CHARS);
        let label = label_for(&Track {
            title: exactly.clone(),
            artist: None,
        });
        assert_eq!(label.title, exactly);
        let over = "x".repeat(MAX_TITLE_CHARS + 5);
        let label = label_for(&Track {
            title: over,
            artist: None,
        });
        assert_eq!(label.title.chars().count(), MAX_TITLE_CHARS);
        assert!(label.title.ends_with('…'));
        // Multibyte text truncates on characters, never mid-character:
        // an emoji-heavy title survives as valid text.
        let emoji = "🎵".repeat(MAX_TITLE_CHARS + 3);
        let label = label_for(&Track {
            title: emoji,
            artist: None,
        });
        assert_eq!(label.title.chars().count(), MAX_TITLE_CHARS);
        assert!(label.title.ends_with('…'));
        // The artist has its own, smaller budget.
        let artist = "y".repeat(MAX_ARTIST_CHARS + 9);
        let label = label_for(&Track {
            title: "Song".into(),
            artist: Some(artist),
        });
        assert_eq!(
            label.artist.map(|a| a.chars().count()),
            Some(MAX_ARTIST_CHARS)
        );
        // Leading and trailing whitespace around real metadata is trim.
        let label = label_for(&Track {
            title: "  Song  ".into(),
            artist: Some(" Artist ".into()),
        });
        assert_eq!(label.title, "Song");
        assert_eq!(label.artist.as_deref(), Some("Artist"));
    }

    #[test]
    fn the_widget_replaces_its_whole_label_on_every_poll() {
        let mut widget = NowPlayingWidget::default();
        // First track lands and marks the button for repaint.
        let first = Feed::Playing(Track {
            title: "First".into(),
            artist: Some("Artist".into()),
        });
        assert!(widget.apply(&first));
        // The same track again is not a repaint.
        assert!(!widget.apply(&first));
        // A track change lands live and repaints.
        let second = Feed::Playing(Track {
            title: "Second".into(),
            artist: None,
        });
        assert!(widget.apply(&second));
        // Pause stops the row: the empty state, and never the last
        // track; repeated idle polls stay quiet.
        assert!(widget.apply(&Feed::Idle));
        assert!(!widget.apply(&Feed::Idle));
        // The clear was real: the first track, reapplied, repaints
        // instead of finding itself already on glass.
        assert!(widget.apply(&first));
        // Leaving the layer resets, and a reset on an empty widget is
        // not a repaint.
        assert!(widget.apply(&second));
        assert!(widget.reset());
        assert!(!widget.reset());
        // And a reset is a real clear: the same track lands again.
        assert!(widget.apply(&second));
    }

    #[test]
    fn fetch_speaks_the_wire_and_fails_closed() {
        let address = test_address("fetch");
        let (handle, ready) = script_server(
            address.clone(),
            vec![
                "ok playing\u{1F}Wire Song\u{1F}Wire Artist",
                "idle",
                "ok paused\u{1F}Old\u{1F}Old",
            ],
        );
        ready.recv().unwrap();
        assert_eq!(
            fetch_feed(&address, 1000).unwrap(),
            Feed::Playing(Track {
                title: "Wire Song".into(),
                artist: Some("Wire Artist".into()),
            })
        );
        assert_eq!(fetch_feed(&address, 1000).unwrap(), Feed::Idle);
        assert_eq!(fetch_feed(&address, 1000).unwrap(), Feed::Idle);
        handle.join().unwrap();
    }

    #[test]
    fn an_absent_receiver_refuses_instead_of_hanging() {
        let address = test_address("absent");
        let error = fetch_feed(&address, 1000).unwrap_err();
        // The poller's one-line missing-helper log keys off exactly this
        // error kind.
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
    }
}
