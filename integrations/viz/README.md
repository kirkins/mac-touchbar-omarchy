# Touch Bar music visualizer feed

`dfr-viz` runs in the desktop user's session and feeds the Touch Bar's
opt-in media visualizer: live peak levels of the sound actually playing on
the default output. It follows the installed voice integration's shape -
same session, same abstract AF_UNIX boundary, different source - without
connecting to its socket or modifying its files.

## What it captures

The receiver resolves the session's default output sink through the
installed Omarchy sink resolver (`omarchy-audio-output-sink`, falling back
to `pactl get-default-sink`), then records that sink's *monitor* source
with `pw-cat` at 16 kHz mono s16 into a pipe the receiver drains. Each
drained chunk folds into a 0-100 peak (gain 96, tuned for mastered music:
typical peaks read 50-100 and a quiet passage 10-30, keeping the wave's
dynamics visible). `DFR_VIZ_SINK` overrides the sink choice for testing.

## Protocol

The daemon dials the abstract socket `dfr-viz-<uid>` with one-shot
requests, verifying the receiver's kernel peer UID (the receiver accepts
only its own UID and `nobody`, 65534):

- `on` - start the monitor capture (reply `ok`, or `error` when no sink
  can be resolved or the capture cannot run)
- `off` - stop the capture, idempotent (reply `ok`)
- `get` - latest level (reply `ok <0-100>` while capturing, `idle` when
  not, `error` for malformed requests)

Requests are a finite set; callers cannot supply sink names, commands, or
paths.

## Lifecycle and resource care

The capture stream exists only while something consumes it:

- The daemon arms its poller only while the media layer is displayed AND
  its visualizer button was tapped on. Arming sends `on`; toggle-off,
  leaving the media layer, a configuration reload, or daemon shutdown
  disarms it, which sends `off`.
- If the daemon vanishes without sending `off`, an idle watchdog stops
  the capture after five seconds without a request
  (`DFR_VIZ_IDLE_TIMEOUT` seconds, minimum one, for tests).
- A capture that stalls or dies reports silence (`0`) or `idle` rather
  than a stale level; the pipe is drained and closed on every stop.

Measured on the target machine while enabled: the `pw-cat` monitor stream
costs about 0.5% of one CPU core and moves ~32 kB/s through an in-memory
pipe (no disk writes), and the sinks remain `SUSPENDED` with the monitor
stream active. The daemon side polls `get` at 20 Hz only while enabled,
matching the recording waveform's feed. While the visualizer is off -
the default - nothing runs: no sockets are dialed, no stream exists.

Known limitation: changing the default output sink while the visualizer is
enabled does not move the capture; toggle it off and on to follow the new
output.

## Deployment

Deploy the checked `dfr-viz` to the desktop user's `~/.local/bin/dfr-viz`
as 0755 and `dfr-viz.service` to their user systemd directory as 0644
(`After=`/`PartOf=` `graphical-session.target` and `pipewire.service`,
`NoNewPrivileges=yes`, PATH including the Omarchy bin directory). Enable
the user service with the scoped deployment, then add the media bar's
`Visualizer = true` button (see `share/tiny-dfr/multibar.toml`). No voice
client, receiver, helper, or unit replacement is required, and the Touch
Bar system unit, `nobody` execution, and AF_UNIX policy are unchanged.
These are deployment instructions, not authorization to install during
review.

## Tests

Run `python3 -B -m unittest discover -s integrations/viz -p test_viz.py`
for synthetic capture, protocol, and watchdog checks. The tests fake
`pw-cat`, `pactl`, and the sink resolver through a fixture PATH; no test
connects to the production socket, starts a real capture, or changes live
audio state. Rust-side lifecycle tests live in `src/waveform.rs`.
