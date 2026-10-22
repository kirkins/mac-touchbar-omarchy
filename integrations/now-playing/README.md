# Touch Bar now-playing feed

`dfr-now-playing` runs in the desktop user's session and feeds the Touch
Bar media bar's now-playing row: the track title and artist of the audio
actually playing, read from the session bus's MPRIS players. It follows
the installed voice and visualizer integrations' shape - same session,
same abstract AF_UNIX boundary, different source - without connecting to
their sockets or modifying their files.

**Install and enable this receiver before expecting the now-playing row
to show a track.** A missing receiver is a normal case, not a failure:
the row rests in its dim `Nothing playing` empty state, and the daemon
logs one line per arm (`now-playing receiver unavailable; install and
enable integrations/now-playing/dfr-now-playing`) - check
`journalctl -u tiny-dfr` if the row never wakes up.

## What it reads

Each request opens a fresh read of the session bus: the receiver lists
the `org.mpris.MediaPlayer2.*` names, reads `PlaybackStatus` and
`Metadata` (`xesam:title`, `xesam:artist`) from each, prefers a player
that reports `Playing`, and otherwise stands on the first player with a
usable title - so its paused or stopped state is exactly what the bar
sees. Metadata is display text, not bytes to trust: control and format
characters are stripped (a soft hyphen inside a real artist name arrived
on the target machine), whitespace runs collapse, and each field is
capped at 120 characters. Players that die mid-read are skipped. Nothing
is cached: a player crash, a bus restart, or a receiver restart can
never leave a stale track behind.

## Protocol

The daemon dials the abstract socket `dfr-now-playing-<uid>` with
one-shot requests, verifying the receiver's kernel peer UID (the
receiver accepts only its own UID and the daemon's `tiny-dfr` account (`nobody` on installs without it)):

- `get` - current track. Replies `ok <state>\x1f<title>\x1f<artist>`
  with state `playing`, `paused`, or `stopped` (either text may be
  empty), `idle` when no MPRIS player offers a usable title, or `error`
  for malformed requests or a bus failure.

Only `playing` ever draws a track on the bar: paused and stopped fold
into the row's empty state. Requests are a finite set; callers cannot
supply player names, commands, or paths.

## Lifecycle and resource care

The daemon arms its poller only while a layer carrying the
`NowPlaying = true` button is displayed, and polls at one request per
second - track metadata changes on the scale of songs, not frames. Each
request is a handful of session-bus property reads (microseconds each);
no stream, capture, or subprocess exists, and while no now-playing bar
is displayed no socket is dialed at all. The receiver itself holds no
state and keeps no timers: a vanished daemon leaves nothing behind.

## Deployment

Deploy the checked `dfr-now-playing` to the desktop user's
`~/.local/bin/dfr-now-playing` as 0755 and `dfr-now-playing.service` to
their user systemd directory as 0644 (`PartOf=`/`After=`
`graphical-session.target`, `NoNewPrivileges=yes`, PATH including the
Omarchy bin directory). Enable the user service, then add the media
bar's `NowPlaying = true` button (see `share/tiny-dfr/multibar.toml`).
The receiver needs `python-dbus` in the desktop session. No other
client, receiver, helper, or unit replacement is required, and the
Touch Bar system unit, `tiny-dfr` execution, and AF_UNIX policy are
unchanged. These are deployment instructions, not authorization to
install during review.

## Tests

Run `python3 -B -m unittest discover -s integrations/now-playing -p
test_now_playing.py` for metadata sanitizing, player selection, wire
protocol, and UID-boundary checks. The tests fake the session bus
through a fixture factory; no test connects to the production socket,
the real session bus, or any player. Rust-side parsing, truncation, and
empty-state tests live in `src/now_playing.rs`.
