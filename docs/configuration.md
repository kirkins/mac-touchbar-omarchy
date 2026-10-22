# Configuration

How the deck is configured: files, named layers, buttons and actions, Fn
behavior, reloads and the full knob reference. Back to the [README](../README.md).

Most people can use the **Touch Bar Settings** app instead
([settings-app.md](settings-app.md)). Everything it edits is the file described
here. Colors and the time-of-day background are in [themes.md](themes.md);
sliders, battery, now playing and the visualizer are in [widgets.md](widgets.md);
the pet is in [pet.md](pet.md).

## Files

The daemon merges the user configuration `/etc/tiny-dfr/config.toml` over the
base `/usr/share/tiny-dfr/config.toml` (shipped by the `tiny-dfr` package).
Global settings you leave out inherit from the base, and every knob has a
code-level default, so even a stripped base file loads.

The repository ships three files in `share/tiny-dfr/`; `make install` copies
them to `share/tiny-dfr-deck/` under its prefix (for example
`/usr/local/share/tiny-dfr-deck/multibar.toml`):

- `config.toml` - the base, which on its own keeps upstream's legacy
  function/media layout.
- `multibar.toml` - the full deck: Home, Function, Media and More layers.
  `make install` seeds `/etc/tiny-dfr/config.toml` with it when no
  configuration exists yet.
- `multibar-voice.toml` - the full deck plus a Mic button and a recording
  layer for the optional [voice dictation](../integrations/voice/README.md).
  Select it deliberately; the default deck has no voice command.

The shipped full deck's Home bar is Volume, Brightness, Play/Pause, F1-F12,
Media, More, Battery and Settings. The two sliders and the battery each have
`Stretch = 2`. The pet opener lives on More, and theme buttons are opt-in.

## Validating a configuration

`tiny-dfr-deck check [FILE]` runs the exact parse, merge and validation path
the daemon runs, including icon and font resolution, without opening the Touch
Bar. `make install` puts `tiny-dfr-deck` in the bin directory as a link to the
deck daemon; the package's `/usr/bin/tiny-dfr` is stock upstream and has no
`check`. It prints one diagnostic per problem (with the TOML line when the
parser knows it) and exits 0 when the file loads, 1 when it printed
diagnostics, and 2 on bad usage:

```sh
# Validate the installed user configuration against the installed base:
tiny-dfr-deck check

# Validate a file before installing it:
tiny-dfr-deck check /etc/tiny-dfr/config.toml

# Also exercise the wide-bar layout, which adds the Esc key:
tiny-dfr-deck check --width 2170 /etc/tiny-dfr/config.toml

# From a source checkout, validate the shipped example against the
# checkout's own files (what CI or a pre-commit hook runs):
target/release/tiny-dfr check --base share/tiny-dfr/config.toml \
    --icon-root share/tiny-dfr share/tiny-dfr/multibar.toml
```

Run it before installing a new file and before restarting the service after an
edit. An invalid file never bricks the bar - the daemon keeps its current
configuration - but `check` shows you the problem instead of leaving it in the
journal. Typos get a suggestion (`did you mean ShowButtonOutlines?`).

## Reloads and invalid files

The daemon watches the configuration and reloads on direct writes and atomic
saves.

- Unknown keys, unknown layer targets, duplicate or empty names, empty bars and
  out-of-range values reject the whole candidate file.
- An invalid file at startup logs an error and uses the base configuration.
- An invalid file on reload keeps the current configuration and the current bar.
- A valid reload returns to the new home bar. Deleting or renaming away the user
  configuration reloads the base.
- Layer changes and valid reloads release held virtual keys and discard existing
  touches; fingers already down must lift and press again on the new bar.

## Named layers

```toml
DefaultLayer = "home"
FnLayer = "function" # optional: the bar shown while Fn is held

[[Layers]]
Name = "home"
Keys = [{ Text = "Functions", Action = { Layer = "function" } }]

[[Layers]]
Name = "function"
Keys = [
    { Text = "Home", Action = { Layer = "home" } },
    { Text = "F1", Action = "F1" },
]
```

- `DefaultLayer` is the bar shown at startup and after a reload.
- Each `[[Layers]]` entry needs a unique, nonempty, case-sensitive `Name` and a
  nonempty `Keys` list.
- `Layers` replaces the legacy key arrays as a whole; global appearance and
  brightness settings still inherit from the base.
- A sub-bar gets a Home button automatically unless it already has a visible
  button targeting `DefaultLayer`.
- Escape is inserted automatically on Macs without a physical Escape key.
- `pet` and `themes` are reserved for the synthesized pet and theme-picker
  layers. A user layer with either name replaces the synthesized one.

### Navigation and Fn

Navigation buttons switch on touch-down. With `FnLayer` set, holding Fn shows
that bar and releasing Fn restores the previous one; tapping a navigation button
while Fn is held keeps the chosen bar open. Without `FnLayer`, named bars are
navigated only from the bar itself.

### Legacy syntax

Upstream's `PrimaryLayerKeys` / `MediaLayerKeys` configurations still work, with
their Fn and double-Fn behavior. `MediaLayerDefault` and
`DoublePressSwitchLayers` apply only to that legacy syntax. `Layers` and the
legacy arrays are mutually exclusive.

## Buttons

A button is a table in a layer's `Keys` list. It shows one kind of content -
`Text`, `Icon`, `Time`, `Battery`, `Waveform`, `Visualizer` or `NowPlaying` -
and optionally has an `Action`. `Stretch` sets how many button spaces it takes.
Icons default to 40 pixels (`IconWidth`, `IconHeight`). Clock, battery, icons,
spacers and `Stretch` work in named bars exactly as in upstream's layout.

### Actions

| Form | Example | Effect |
| --- | --- | --- |
| Key | `Action = "F1"` | Press and release a key. |
| Key chord | `Action = ["LeftCtrl", "F1"]` | Press a combination. |
| Slider | `Action = "VolumeSlider"` / `"BrightnessSlider"` | Open the shared slider ([widgets.md](widgets.md)). |
| Layer | `Action = { Layer = "media" }` | Switch bars. |
| Command | `Action = { Command = "my-sync-script" }` | Run a shell command. |
| Command + layer | `Action = { Command = "...", Layer = "home" }` | Run the command, then switch. |
| Cycle theme | `Action = "CycleTheme"` | Apply the next color theme ([themes.md](themes.md)). |
| Theme | `Action = { Theme = "Nord" }` | Apply one theme, optionally with `Layer`. |

### Command buttons

A `Command` action runs its string verbatim through `/bin/sh -c` on touch-down.
Nothing observed at runtime is interpolated into it, so the only input is what
you wrote in the file.

- Commands run detached: standard streams go to `/dev/null` and each child is
  reaped on its own thread, so a slow or hung command never blocks the bar.
- Spawn failures and nonzero exits are logged to stderr (the journal under
  systemd).
- At most one invocation of a given command string runs at a time. A second
  touch while the previous child is alive is dropped with a log line.
- Commands run as the daemon's unprivileged account inside the service sandbox.
  Anything that needs the desktop session (Wayland, the session bus) must go
  through a session receiver client, as the Screenshot and Settings keys do.
- An empty `Command` string, or a combined action naming an unknown layer, is
  rejected.

Adding `Layer` runs the command first, then opens the layer. Combined buttons
count as visible navigation, so a sub-bar whose Done button returns home through
a command gets no extra Home button.

### Quick keys (the More layer)

The shipped Home bar has a `More` button beside `F1-F12` and `Media`. It opens
the `more` layer, a place for one-press quick keys. Adding one is a config edit:

```toml
[[Layers]]
Name = "more"
Keys = [
    { Text = "Home", Action = { Layer = "home" } },
    { Text = "Screenshot", Action = { Command = "touchbar-capture-user screenshot", Layer = "home" } },
    { Text = "Lock", Accent = "amber", Action = ["LeftMeta", "LeftCtrl", "L"] },
]
```

- **Screenshot** opens the Omarchy screenshot picker and returns home. The
  daemon's sandbox cannot reach the compositor, so the key calls a thin client
  that hands the request to the
  [capture receiver](../integrations/capture/README.md) in the desktop session,
  which runs `omarchy capture screenshot` there. The command is fixed on the
  session side and never taken from the wire. Without the receiver the press
  still returns home and the failure is journalled.
- **Settings** on Home (`touchbar-capture-user settings`) uses the same receiver
  to open the Settings app.
- **Lock** emits Omarchy's lock bind, `SUPER+CTRL+L`, through the virtual
  keyboard - no shell or helper involved. If you rebind lock in Hyprland, change
  the chord here too.

An entry without `Layer` stays on the bar so you can run several keys in a row.

### Voice dictation buttons

The optional voice profile, `multibar-voice.toml`, uses combined actions for
dictation:

```toml
{ Text = "Mic", Accent = "cyan", Action = { Command = "voice-dictate-user start", Layer = "recording" } }
```

The Mic button starts recording and opens a `recording` layer. That layer has
**Stop** (`voice-dictate-user stop`: stop and type the transcript into the
focused window), **Cancel** (`voice-dictate-user cancel`: discard), and a live
input waveform between them ([widgets.md](widgets.md#recording-waveform)). Both
buttons return home. The [voice integration](../integrations/voice/README.md)
supplies the client and recorder; install it separately. Without it the
buttons still switch layers and the journal logs the failed command.

### Colors

Buttons get an accent by action: cyan for audio and commands, amber for
brightness, violet for navigation, neutral for other keys, battery and clock.
Override it per button with `Accent` - one of `neutral`, `cyan`, `amber`,
`blue`, `violet`, `orange`, `red`, `green`:

```toml
{ Text = "Home", Accent = "violet", Action = { Layer = "home" } }
```

Battery charging and low-charge colors take precedence over `Accent`. Labels
and icons are white on inset rounded fills; pressing a button changes its fill
and brightens its border. `ShowButtonOutlines = false` hides idle fills and
borders but keeps pressed feedback. The colors behind each accent name come from
the active [theme](themes.md).

## Knob reference

This table is generated from the knob registry; edit the registry, not the
table.

<!-- dfr-config knob reference: begin -->
The knob registry in `crates/dfr-config/src/registry.rs` is the single
source of truth for this table; a test regenerates it and fails when the
documented copy drifts. Every knob rejects unknown siblings: a typo in a
name rejects the candidate configuration naming the line, not a silent
ignore. Defaults are code-level: a stripped base file still loads.

### Global knobs

| Knob | Type | Default | Constraints | Purpose |
|---|---|---|---|---|
| **Appearance** | | | | |
| `ShowButtonOutlines` | bool | `true` | - | Draw the outline around each button; false leaves only labels and glyphs. |
| `EnablePixelShift` | bool | `false` | - | Slowly shift the strip contents sideways to spread pixel wear. |
| `FontTemplate` | string | `":bold"` | a fontconfig pattern that matches an installed font | Fontconfig pattern selecting the font for text labels (see the fontconfig font-names guide). |
| `ActiveBrightness` | u32 | `128` | 0-255 | Static brightness with adaptive off; the curve's maximum with it on. |
| `AdaptiveBrightness` | bool | `true` | - | Follow the primary display's backlight instead of a fixed brightness. |
| **Input** | | | | |
| `DoublePressSwitchLayers` | u32 | `0` | 0 disables; otherwise a millisecond window | Double-press Fn within this many milliseconds to swap the default layer. |
| **Battery** | | | | |
| `BatteryCycleEnabled` | bool | `true` | - | Tap a battery widget to cycle from the system battery through device batteries. |
| `BatteryCycleTimeoutSecs` | u32 | `15` | 1-3600 seconds | Return to the system battery this long after the last cycling tap. |
| `BatteryWarningThreshold` | u32 | `25` | 0-100, at least the critical threshold | Below this charge percent (and not charging) the widget paints the warning color. |
| `BatteryCriticalThreshold` | u32 | `10` | 0-100, at most the warning threshold | Below this charge percent the widget paints the critical color. |
| `BatteryWarningColor` | enum | `orange` | neutral, cyan, amber, blue, violet, orange, red, green | Accent painted below the warning threshold while not charging. |
| `BatteryCriticalColor` | enum | `red` | neutral, cyan, amber, blue, violet, orange, red, green | Accent painted below the critical threshold while not charging. |
| **Pet** | | | | |
| `PetEnabled` | bool | `true` | - | Keep the pet layer and its More-bar Pet button (Home fallback); false removes both. |
| `PetVariant` | enum | `cat` | cat, fox | The pet's sprite: an orange tabby or a red fox. |
| `PetEnergy` | enum | `calm` | calm, rowdy | Rowdy pets dash more and get hungry sooner; calm ones stroll and doze. |
| **Background** | | | | |
| `DayCycleEnabled` | bool | `true` | - | Tint the strip by time of day; false keeps the plain black background. |
| `DayCycleBands` | list of tables | `four shipped bands` | at least one band; unique names and 0-23 hours | Replace the default bands: each [[DayCycleBands]] entry has Name, StartHour, and a "#RRGGBB" Color. |
| **Themes** | | | | |
| `ThemesEnabled` | bool | `true` | - | Enable the color-theme system and picker layer; configure theme buttons explicitly on a bar. |
| `ActiveTheme` | string | `the remembered choice` | must name a defined theme | Pin the startup theme instead of the last choice made on the glass. |
| `Themes` | table map | `the thirteen built-ins` | at most 21 themes in total | Define or override themes: [Themes.Name] with an Accents map of accent names to "#RRGGBB" and optional [[...DayCycleBands]]. |
| **Layers** | | | | |
| `MediaLayerDefault` | bool | `false` | legacy syntax only | Legacy syntax: show the media keys without Fn pressed. |
| `PrimaryLayerKeys` | list of tables | `the shipped F1-F12 bar` | legacy syntax; mutually exclusive with Layers | Legacy syntax: the primary layer's buttons. |
| `MediaLayerKeys` | list of tables | `the shipped media bar` | legacy syntax; mutually exclusive with Layers | Legacy syntax: the media layer's buttons. |
| `Layers` | list of tables | `none (legacy syntax)` | named syntax; requires DefaultLayer; unique nonempty names | The named bars: each [[Layers]] entry has a Name and a nonempty Keys list of button tables. |
| `DefaultLayer` | string | `primary (legacy)` | must name a Layers entry when Layers is used | The layer the bar starts on and returns to. |
| `FnLayer` | string | `the other legacy layer` | optional; must name a Layers entry | The layer held while Fn is pressed. |

### Button knobs

| Knob | Type | Default | Constraints | Purpose |
|---|---|---|---|---|
| **Button content** | | | | |
| `Icon` | string | `none` | Svg is an accepted alias; a file must resolve | Icon name, without extension: the fixed icon directories, or the XDG theme when Theme is set. |
| `Text` | string | `none` | - | The button's text label. |
| `Theme` | string | `none` | an installed XDG icon theme | The XDG icon theme to resolve this button's Icon in (not a color theme). |
| `Time` | string | `none` | 24hr, 12hr, or a chrono strftime format string | Show a clock; the value is a shorthand or a strftime format. |
| `Locale` | string | `POSIX` | a locale chrono knows | Locale for the Time button's names; an unknown locale falls back to POSIX. |
| `Battery` | enum | `none` | icon, percentage, both | Show the battery as an icon ladder, a percentage, or both. |
| **Waveform and visualizer** | | | | |
| `Waveform` | bool | `false` | - | Make this button the live microphone waveform widget. |
| `WaveformStyle` | enum | `line` | line, bars | The waveform's graph style. |
| `WaveformSmoothing` | u32 | `0` | 0-5000 ms | Milliseconds of level decay for calmer waveform motion; the attack is a third of it. |
| `WaveformLag` | u32 | `0` | 0-500 ms | Milliseconds of visual delay behind the measured level. |
| `Visualizer` | bool | `false` | - | Make this button the opt-in music visualizer; a tap expands it to the full bar. |
| `VisualizerStyle` | enum | `line` | line, bars | The visualizer's graph style. |
| `VisualizerSmoothing` | u32 | `0` | 0-5000 ms | Visualizer motion decay, as WaveformSmoothing. |
| `VisualizerLag` | u32 | `0` | 0-500 ms | Visualizer visual delay, as WaveformLag. |
| **Button content** | | | | |
| `NowPlaying` | bool | `false` | - | Make this button the now-playing row: the current track's title and artist. |
| **Button action** | | | | |
| `Action` | string, list, or table | `none` | a key name, a key list, a slider name, CycleTheme, or a Layer/Command/Theme table | What a press does: emit keys, run a command, switch layers, open a slider, or apply a theme. |
| **Button size and color** | | | | |
| `Stretch` | usize | `1` | at least 1 | Button spaces this button occupies. |
| `IconWidth` | i32 | `40` | 8-256 px | Icon width; match PNG pixel size for best results. |
| `IconHeight` | i32 | `40` | 8-256 px | Icon height; match PNG pixel size for best results. |
| `Accent` | enum | `the action's family` | neutral, cyan, amber, blue, violet, orange, red, green | The semantic accent color for this button's fill and outline. |

Reserved layer names: `pet` and `themes` name the daemon's synthesized
layers; a user layer with the name replaces the synthesized one. The
button `Theme` selects an XDG *icon* theme; the action table's `Theme`
applies a *color* theme. No `Version` key exists yet; one would be
additive if a future migration needs a hook.
<!-- dfr-config knob reference: end -->

There is no `Version` key by design: new knobs are additive, and a version key
could be added later without breaking anything. The configuration became
stricter once, when unknown keys and out-of-range values started being
rejected; the [changelog](../CHANGELOG.md) explains how to fix a file that
stopped loading.
