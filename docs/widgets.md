# Widgets

The deck's live widgets: the volume and brightness slider, the battery, now
playing, the music visualizer and the recording waveform. Back to the
[README](../README.md).

Most widgets get their data from a small helper running in your desktop
session. The daemon runs as an unprivileged system account and cannot reach the
session itself, so each helper listens on an abstract AF_UNIX socket named
after the desktop user's UID, and both sides check the kernel peer UID. The
helpers are installed by `make install-user-session`; each has its own README,
listed in the [documentation index](../README.md#documentation). The daemon talks to the
seat's active user, or to `TINY_DFR_DESKTOP_UID` if that is set in the service
environment.

## Volume and brightness slider

The Home bar has a cyan `VolumeSlider` button and an amber `BrightnessSlider`
button. They are a pair: give them equal `Stretch` (the shipped deck uses 2
each) so they render identically.

```toml
{ Icon = "volume_up", Stretch = 2, Action = "VolumeSlider" },
{ Icon = "brightness_high", Stretch = 2, Action = "BrightnessSlider" },
```

Pressing either one reads the current hardware level and turns both buttons'
combined space, gap included, into one slider for that control. Other buttons
stay usable. Adjacent slider buttons share their span on any bar; a lone slider
button uses its own span. The Media bar keeps its single volume control.

- Opening never changes the level or jumps to where you touched. Drag past the
  tap threshold to move it; a later touch grabs the track directly. The speaker
  or sun zone selects the minimum.
- The slider closes about 2.5 seconds after the last touch. Navigation, Fn, a
  reload or input release also close it. To switch to the other control, let
  it close and press the other button.
- A second finger inside an open slider cannot press the buttons hidden under it.

**Volume** is absolute: the far left sets every output channel to zero, the far
right reaches 130%, and every percentage in between is reachable. Setting a
level clears mute. The [audio helper](../integrations/audio/README.md) finds the
current output with Omarchy's sink resolver and reports the mixer's actual level.
A muted output shows a subdued fill.

**Brightness** uses the [brightness helper](../integrations/brightness/README.md)
and `brightnessctl` on the `apple-panel-bl` display backlight, with the same
linear 1-100% range as Omarchy's brightness keys. The minimum leaves the display on. The Touch Bar and keyboard
backlights are never touched.

Both helpers run outside the render loop. Fast drags coalesce to the latest
target, and the fill follows the level read back after each write, not the
requested one. Failures are logged and close the slider. Replies from an older
opening cannot repaint the slider or overwrite a newer reading. Opening either
slider picks up changes made elsewhere. Legacy volume and brightness key
actions still send their keys.

### Mute mark

While the slider is closed, a muted output still shows: the volume button gets
the same slash the open slider draws over its speaker. A slow background check
asks the audio helper every few seconds, only on bars that have a volume slider.
It pauses while the slider does its own reads, checks again the moment the
slider closes, keeps the last known state if a read fails, and shows nothing
until its first successful read. The slash draws over the icon or label only;
button sizes and touch targets do not change.

## Battery

```toml
{ Battery = "both", Stretch = 2 }   # "icon", "percentage" or "both"
```

**Cycling.** Tap the battery to cycle from the system battery through connected
device batteries (including `scope=Device` power supplies such as hidpp
mice), then back. A small `SYS` or abbreviated device name and number labels the
reading in all three display modes. Discovery and reads run in the background,
refreshing on power-supply events and every ten seconds. A device that
disconnects or cannot be read returns the widget to the system battery.

- `BatteryCycleEnabled = false` turns cycling off and keeps the button's
  configured action. With no device battery present, the widget also behaves
  that way.
- `BatteryCycleTimeoutSecs` (default 15, 1-3600) returns to the system battery
  after the last tap.
- Cycling, the automatic return and removal of the shown device release held
  virtual keys and discard current touches.

**Low-charge colors.** While not charging, the widget paints
`BatteryWarningColor` (default orange) below `BatteryWarningThreshold` (default
25%) and `BatteryCriticalColor` (default red) below `BatteryCriticalThreshold`
(default 10%). The critical threshold must be at or below the warning one. A
battery exactly at a threshold shows the milder color. Above the warning
threshold the widget uses its normal accent, and a charging battery is green at
every level. The color follows whichever battery is currently shown.

## Now playing

The Media bar shows the playing track's title, with the artist as a small
caption when the player reports one.

```toml
{ NowPlaying = true, Stretch = 2 }
```

Titles are cut to 32 characters and artists to 24 (with an ellipsis), then
fitted to the button like any label. The button is display-only; taps fire
nothing. Each poll replaces the whole row, so a track change appears within
about a second and a paused or stopped player clears it. With nothing playing
or no metadata, the row shows a dim `Nothing playing`.

The data comes from the [now-playing helper](../integrations/now-playing/README.md),
which reads MPRIS players on the session bus (preferring one that is playing)
and strips control and formatting characters. The daemon asks once a second,
only while a bar with a now-playing button is shown. Without the helper the row
stays empty and the journal logs `now-playing receiver unavailable; install and
enable integrations/now-playing/dfr-now-playing`.

## Music visualizer

The Media bar ends with a `Viz` button. Tapping it turns the whole bar into a
live waveform of the music playing on the default output, with one `Off` button
at the end that restores the Media bar exactly as it was.

```toml
{ Visualizer = true, Stretch = 2 }
```

- Switching in and out never sends a key or disturbs playback.
- It is off by default. Leaving the view by any navigation, or reloading the
  configuration, turns it off.
- Nothing runs while it is off: no socket, no capture. Audio is captured only
  while the view is shown, polled at 20 Hz.

The [viz helper](../integrations/viz/README.md) captures the default sink's
output monitor - what goes to the speakers, from any player, never the
microphone. **The helper must be installed and running for the visualizer to
draw anything.** Without it the view shows a red `No feed` marker and the
journal logs `visualizer receiver unavailable; install and enable
integrations/viz/dfr-viz` once per opening.

On the development machine the capture costs about 0.5% of one CPU core, keeps
the sinks suspended and writes nothing to disk. A five-second watchdog in the
helper stops capture if the daemon disappears. If you change the default output
while the view is open, toggle it off and on to follow the new sink.

The visualizer is level-driven rather than a full spectrum. That keeps the
helper free of an FFT dependency, reuses the waveform renderer and frame clock,
and keeps the protocol to a single `ok <level>` reply - a spectrum would add a
renderer, a bigger protocol and a math dependency for little gain at Touch Bar
size.

## Recording waveform

On the `recording` layer of the voice profile (`multibar-voice.toml`), the
middle button is the live microphone level:

```toml
{ Waveform = true, Stretch = 3 }
```

It draws a line that rests flat in silence and ripples into a traveling wave
following the last two seconds of input, beside a dot that pulses with the
level and an `m:ss` timer counting from when the layer opened. Re-entering the
layer starts the timer and graph fresh. The waveform shows live capture as
reported by the helper; it does not mean transcription succeeded.

The level comes from a second socket on the
[voice helper](../integrations/voice/README.md), `dfr-voice-level-<uid>`. The
helper tails the raw capture file its recorder is already writing (unbuffered,
so it trails the microphone by a few milliseconds) and answers each `get` with
the latest 0-100 peak (`ok <n>`) or `idle`. Dictation itself is unchanged.

**Feed health is visible.** A quiet, healthy feed shows a dim dot, a flat line
and the timer. A missing, rejecting or garbled feed shows a red slashed circle
and a red `No feed` label, so a dead source never looks like silence.

**Cost stays bounded.** The daemon polls only while the recording layer is shown,
at 20 Hz, and repaints just that button at about 30 fps while the line is
moving: between samples the line glides left and its newest point eases to the
new level, so it flows instead of stepping. A settled, silent feed repaints only
the once-a-second timer.

## Styles and smoothing

The waveform and visualizer share the same per-button knobs:

| Waveform | Visualizer | Effect |
| --- | --- | --- |
| `WaveformStyle` | `VisualizerStyle` | `"line"` (default) or `"bars"` for a scrolling bar graph. |
| `WaveformSmoothing` | `VisualizerSmoothing` | 0-5000 ms decay; the attack is a third of it. Default 0. |
| `WaveformLag` | `VisualizerLag` | 0-500 ms of visual delay for steadier motion. Default 0. |

By default the line follows the measured level directly. For calmer motion try
`WaveformSmoothing = 240, WaveformLag = 80` (the 150-300 ms range feels calm).
Smoothing and lag only shape what is drawn; the feed itself stays just as fresh.

The waveform's `Accent` colors the dot and the button tint; the shipped default
is `blue`. Set `Accent = "red"` for the earlier look.
