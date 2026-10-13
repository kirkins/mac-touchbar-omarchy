# tiny-dfr
The most basic dynamic function row daemon possible

Runs on Apple Silicon Macs with a Touch Bar under Asahi Linux, on any
Asahi Linux distribution that provides the Touch Bar DRM and backlight
kernel devices the daemon expects. It uses DRM atomic modesetting, uinput,
and the included udev and systemd setup. It is developed and daily-driven
on Omarchy. The shipped quick keys include Omarchy-specific screenshot and
settings actions; adapt those entries for another desktop environment.


## Dependencies
cairo, libinput, freetype, fontconfig, librsvg 2.59 or later, uinput enabled in kernel config

## Build and check

Install Rust/Cargo, pkg-config, and the development files for the dependencies
above, GLib/GIO, libdrm, and libudev. Use a Rust toolchain compatible with
`Cargo.lock`. On machines with limited memory, keep one build job and run checks
sequentially:

```sh
cargo fmt --all -- --check
CARGO_BUILD_JOBS=1 cargo test --locked --workspace -- --test-threads=1
CARGO_BUILD_JOBS=1 cargo clippy --locked --workspace --all-targets
CARGO_BUILD_JOBS=1 cargo build --locked --workspace --release
```

The configuration model lives in the `dfr-config` crate
(`crates/dfr-config`); the daemon binary is the root `tiny-dfr` package,
so the check commands above build the whole workspace.

For an offline visual check of the shipped named bars, the voice-dispatch test
can also export actual Cairo frames without opening the Touch Bar or running
the voice commands:

```sh
DFR_DECK_REVIEW_OUTPUT=/tmp/dfr-deck-review CARGO_BUILD_JOBS=1 \
    cargo test --locked --bin tiny-dfr approved_multibar_preserves_voice_dispatch_and_renders_offline -- --test-threads=1
```

Inspect `implemented-home.png`, `implemented-home-muted.png` (the same bar
with the closed-slider mute mark on the volume button),
`implemented-volume-mid.png`,
`implemented-brightness.png`, `implemented-recording-live.png`, and
`implemented-recording-idle.png` in that directory, plus the quick-keys frame
`implemented-more.png` (the More bar), the visualizer
frames `implemented-media-viz.png` (the expanded full-bar view) and
`implemented-media-off.png` (the restored plain media bar), the
now-playing frame `implemented-media-playing.png` (the same media bar
with a playing track's title and artist caption; without the desktop
receiver the row ships its dim `Nothing playing` state),
the theme frames `implemented-themes.png` (the picker's full-bar takeover
listing every theme name), `implemented-home-nord.png` (the same home bar
with the Nord theme applied live), and `implemented-more-theme-flash.png`
(the cycle button briefly wearing the applied theme's name),
and the per-band frames
`implemented-daycycle-morning.png`, `implemented-daycycle-day.png`,
`implemented-daycycle-evening.png`, and `implemented-daycycle-night.png` of
the same home bar over each time-of-day background.
These frames use the configured font and icon search paths; their battery
display is illustrative and they do not prove physical touch or dictation.
Running the full test suite with the variable set also exports the pet's
feeding frames `pet-food-drop.png`, `pet-food-walk.png`, and
`pet-food-munch.png`, and its time-of-day scene frames
`pet-scene-morning.png`, `pet-scene-day.png`, `pet-scene-evening.png`, and
`pet-scene-night.png`, from its unit tests.

The unit tests run without a Touch Bar, DRM device, or virtual keyboard. They
cover configuration compatibility, navigation, Fn behavior, reloads, emitted key
presses/releases, battery widget cycling, the shared volume/brightness slider (either
selection without stepping, combined-region geometry and drag math, safe ranges,
auto-collapse timing, confirmed readback, failures, stale replies and selection
isolation, and the persistent closed-slider mute mark: a slow background
mute watch that only arms on bars carrying a volume slider, pauses while the
slider does its own reads, is claimed before the slider when a probe reply
lands mid-read, re-probes the moment the slider collapses, keeps the last
confirmed state on a failed read, and lays a slash over the volume button
that renders identically in both slider states without moving any button),
command buttons (parse and
combined layer dispatch, detached execution, the one-invocation-per-command
cap, and the More bar's quick-key entries and shipped screenshot flow), the
color themes (the seeded set's order, readability, and cycle wrap; user
themes merging by name with partial inheritance; invalid definitions
rejecting the candidate configuration; the picker layer and both synthesized
buttons with their suppression and disable rules; the live apply building
the palette and bands from the config pin; the remembered-choice state
file's precedence, healing, and atomic saves; the cycle button's
flash-and-revert clock; picker buttons applying and returning home;
unknown names consumed without effects; and the waveform retint without a
deck rebuild), the
recording waveform (level-history ring behavior, line and bar
geometry from synthetic levels, flatness, bounds, and sample-boundary
continuity of the line, incremental button repaints, recording-only fast cadence,
monotonic duration timer, live/idle rendering, and the level poller's arming,
reply parsing, and peer-UID boundary against fake receivers), and the virtual
pet (pet-layer wiring and button toggle, wander bounds,
pause and resume across layer switches, nap gating and placement, satiety decay
and feeding, food drops on open ground and walk-to-food bounds, startle, tap
consumption and hit-test precedence, reload transitions, and the time-of-day
mood and scene), and the
time-of-day background (band selection across the hours with boundaries and
the midnight wrap, unit-RGB conversion, configuration defaults, overrides, and
rejections, the disabled static background, the strip behind widgets and the
pet layer's open ground, and incremental redraws leaving no plain-black
residue). Physical
layout, touch accuracy, brightness, media-key delivery to the desktop, live
battery/clock refresh, the shared slider's on-glass drag feel, command buttons'
interaction with a real helper, the pet's on-glass charm, and the background's
one-shot repaint at a real band change still need a device
test; battery checks there must include repeated tap-to-cycle, device labels in
each display mode, hotplug, and timed return. The media visualizer's offline
tests cover the full-bar view's lifecycle (off by default, the Viz tap switches
in with no keys fired, Off/navigation/reload switch out, the capture feed arms
exactly while the view is displayed, a lost consumer still sends `off`),
the music widget's rendering and idle wakes, config parsing, and the helper's
capture, protocol, and idle-watchdog behavior against fakes; its on-glass look
with real playback is a device check.

## Validating a configuration offline

`tiny-dfr check [FILE]` runs the exact parse, merge, and validation path
the daemon runs - including icon and font resolution - without opening
the Touch Bar. It prints one diagnostic per problem (with the TOML line
where the parser knows it) and exits 0 when the file is as loadable as
the daemon's own path allows, 1 when diagnostics were printed, and 2 on
bad usage:

```sh
# Validate the installed user configuration against the installed base:
tiny-dfr check

# Validate the shipped showcase example over the checkout's own files
# (what CI or a pre-commit hook runs from a source tree):
tiny-dfr check --base share/tiny-dfr/config.toml \
    --icon-root share/tiny-dfr share/tiny-dfr/multibar.toml

# Also exercise the wide-bar layout, which synthesizes the esc key:
tiny-dfr check --width 2170 /etc/tiny-dfr/config.toml
```

Run it before installing a candidate file and before every service
restart after editing: an invalid candidate never bricks the bar (the
daemon keeps its current configuration), but `check` tells you about
the problem instead of leaving it in the journal.

## Graphical configurator

A native GTK4/libadwaita configurator lives in `crates/dfr-configurator`
(`cargo build -p dfr-configurator`). It is a build-on-demand tool for
people editing a deck, **not** part of the daemon install: nothing in
`make install` or the service depends on it, and it never runs as root
beyond whatever privileges you start it with — save installs the file
with an atomic rename, so point it at a copy if your config needs
elevated rights and install that copy yourself.

Building it needs the GTK 4 and libadwaita development packages at
build time only (the daemon itself stays dependency-free of them):
Arch `pacman -S gtk4 libadwaita`, Fedora `dnf install gtk4-devel
libadwaita-devel`, Debian/Ubuntu `apt install libgtk-4-dev
libadwaita-1-dev`. A checkout that lacks them still builds the daemon:
`cargo build -p tiny-dfr` never compiles the configurator.

Run it with:

```sh
cargo run -p dfr-configurator -- [--config PATH] [--base PATH]
```

The app edits the file supplied by `--config` (default
`/etc/tiny-dfr/config.toml`), over the `--base` file (default
`/usr/share/tiny-dfr/config.toml`). All pages share one staged document.
Opening or inspecting inherited layers does not create changes. Save or
Ctrl+S writes a valid changed draft, or deliberately creates a missing
user file. Nothing is installed by the configurator redesign task;
deployment remains with the documented [reviewed installation
flow](#reversible-device-installation-after-review).

**Layers** opens with a bounded collection sidebar, a compact contextual
preview, and ordered, keyboard-activatable button rows. Selecting a row
opens Button Properties inside the layer workspace; native Back returns
to the list. Add Button stays beside the list heading. Button Options
provides Move Earlier/Later and deletion, while Layer Options provides
rename, reordering, and deletion. Rename commits on the entry's Apply
button and updates Startup/Fn assignments and every incoming action
together. Deleting a linked or assigned layer requires an explicit
replacement; incoming links can be opened for repair. The final layer
and final button cannot be deleted.

**Appearance** puts theme switching first: choose any available theme
to audition it in the live configurator preview, then use **Switch to
Theme** to save the valid draft and reload the Touch Bar. Color Theme,
Background, and Button Style remain below, with fixed versus remembered
startup themes, built-in palette overrides and custom themes, native
color pickers with validated hex entries, named hourly background
bands, and typography. **Behavior**
groups Layer Switching, Display, Battery, and Pet. Ordinary controls use
familiar labels and units; Advanced disclosures keep fontconfig, XDG
icon themes, icon dimensions, waveform/visualizer smoothing and lag,
and pixel shift available. Every existing content kind and all ten
supported action forms remain editable. Shortcut capture has a distinct
listening state plus explicit Finish and Cancel.

Settings show their effective base-plus-user value, with **Using Base
Setting** or **Customized** and a reset action that removes the user
override. Explicit values equal to a code default are still saved when
they override a different base value. Since an absent overlay value
inherits the base, it cannot erase a base-pinned startup theme or Fn
assignment: the UI explains the former and offers Fn **None** only
when the base permits it. Theme tables follow the same whole-map
replacement contract; an explicit empty map is preserved by the writer.

The preview calls the unchanged `dfr-render` Cairo path. Enlarge opens a
native adaptive dialog with Fit/1:1 scale, horizontal scrolling, both
2008/2170 geometries, layer and theme choices, background-hour audition,
and accessible rendered button identities. Generated Pet, theme, Home,
and Esc controls are identified separately from authored buttons.
Authored slots can open their properties; preview interactions never
execute actions. Theme/hour audition affects only a cloned in-memory
candidate, never the document or persisted theme state. A remembered
on-device theme is not read: the caption labels that fallback. Feed data,
hardware brightness, and action execution are not simulated. Preview
refresh cancels superseded debounce work; invalid drafts show accessible
diagnostics rather than an old valid frame. Temporary render files are
removed after use.

The shell uses GTK4/libadwaita only, with system styling and a single
primary Save action. It follows native [NavigationSplitView sizing and
collapse](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/1-latest/class.NavigationSplitView.html)
and [NavigationView Back
navigation](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/1-latest/class.NavigationView.html).
At narrow widths, the collection becomes a navigation page and the
switcher moves to the bottom. Native preference pages supply their own
clamp and scrolling. Where the wireframes suggest a bespoke arrangement,
native conventions take precedence: HeaderBar supplies Back, the
properties page keeps stable identity, and Adw.Dialog chooses its
presentation from the actual host window and available space. The
baseline remains GTK 4.10/libadwaita 1.5.

Validation still uses the shared `dfr-config` parse path at **both** bar
geometries, including icon resolution. Invalid drafts disable Save;
field diagnostics and Review Issue direct repair. A valid Save backs up
an existing file once per session (`.bak-<epoch>`), stages canonical TOML,
and renames it atomically. The daemon's watcher reloads that rename
without a restart. A success toast says **Saved**, without claiming daemon
acknowledgement. Write failures stay visible with Retry Save until a
successful save or dismissal. Closing a dirty draft offers
Cancel/Discard and, when valid, Save and Close.

Headless regression tests cover merge/reset and round-trip semantics,
all action forms, rename/deletion dependencies, theme limits, failed
save retention, atomic saving, and the close guard. A display-dependent
native acceptance test exercises selection, repeated structure changes,
save feedback, Enlarge, inherited controls, and wide/short/narrow/large
text allocations against disposable configuration files:

```sh
cargo test --locked -p dfr-configurator -- --test-threads=1
G_DEBUG=fatal-criticals cargo test --locked -p dfr-configurator \
  native_selection_structure_save_feedback_and_layouts -- --ignored --test-threads=1
```

Set `DFR_CONFIGURATOR_PROOF_DIR` to a scratch directory to save native
layout captures. The native test uses the application's resizable
window and waits for mapped widgets and GTK paint; fixed sleeps or
forced allocation immediately before a cached snapshot can observe
stale UI. No test writes the live configuration or drives a service.

The redesign's validation host exceeded the renderer's unchanged 2 ms
debug waveform repaint budget on both the recorded base and the branch,
using separate builds under identical limits. This remains a timing
acceptance gap; the renderer and threshold were retained. The functional
configuration, renderer and native UI checks are reported separately.

## Configuration reference

<!-- dfr-config knob reference: begin -->
The knob registry in `crates/dfr-config/src/registry.rs` is the single
source of truth for this table; a test regenerates it and fails when the
README copy drifts. Every knob rejects unknown siblings: a typo in a
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
| `PetEnabled` | bool | `true` | - | Keep the pet layer and its home-bar Pet button; false removes both. |
| `PetVariant` | enum | `cat` | cat, fox | The pet's sprite: an orange tabby or a red fox. |
| `PetEnergy` | enum | `calm` | calm, rowdy | Rowdy pets dash more and get hungry sooner; calm ones stroll and doze. |
| **Background** | | | | |
| `DayCycleEnabled` | bool | `true` | - | Tint the strip by time of day; false keeps the plain black background. |
| `DayCycleBands` | list of tables | `four shipped bands` | at least one band; unique names and 0-23 hours | Replace the default bands: each [[DayCycleBands]] entry has Name, StartHour, and a "#RRGGBB" Color. |
| **Themes** | | | | |
| `ThemesEnabled` | bool | `true` | - | Enable the color-theme system: the picker layer and the more bar's two theme buttons. |
| `ActiveTheme` | string | `the remembered choice` | must name a defined theme | Pin the startup theme instead of the last choice made on the glass. |
| `Themes` | table map | `the eight built-ins` | at most 21 themes in total | Define or override themes: [Themes.Name] with an Accents map of accent names to "#RRGGBB" and optional [[...DayCycleBands]]. |
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

## Named bars

The daemon merges `/etc/tiny-dfr/config.toml` over
`/usr/share/tiny-dfr/config.toml`. The optional
[multibar example](share/tiny-dfr/multibar.toml) defines a home bar with volume,
brightness, play/pause, and battery controls, buttons opening F1-F12, media,
and quick-key bars, and a mic button that starts voice dictation on a recording
layer.

```toml
DefaultLayer = "home"
FnLayer = "function" # optional temporary bar while Fn is held

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

`DefaultLayer` names the startup and reload home bar. Each `[[Layers]]` has a
unique, nonempty `Name` and nonempty `Keys`. Names are case-sensitive. `Layers`
replaces the legacy key arrays as a whole; global appearance and brightness
settings still inherit from the base configuration. Add further named bars and
navigation buttons using `Action = { Layer = "name" }`, or run a shell command
on touch-down with `Action = { Command = "..." }` (see below). Each sub-bar gets
a Home button automatically if it lacks a visible button targeting
`DefaultLayer`.
Escape is still inserted on Macs without a physical Escape key.

Navigation buttons switch immediately on touch-down. With `FnLayer`, holding Fn
temporarily shows that bar and releasing Fn restores the previous bar. Tapping a
navigation button while Fn is held keeps the chosen bar open. Omitting `FnLayer`
leaves named bars entirely under on-bar navigation. `MediaLayerDefault` and
`DoublePressSwitchLayers` apply to legacy configurations only. Existing
`PrimaryLayerKeys`/`MediaLayerKeys` configurations retain their Fn and double-Fn
behavior, and `Action = "F1"` or `Action = ["LeftCtrl", "F1"]` still sends keys.
Clock, battery, icons, spacers, and `Stretch` work in named bars as before.

The deck uses cyan for audio controls and commands, amber for brightness,
violet for navigation, and neutral surfaces for other keys, battery, and clock.
White icons and centered bold labels stay legible on the inset rounded fills;
pressing a control changes its fill and brightens its border. The existing
16-pixel gaps and touch targets are unchanged. Icons default to 40 pixels,
with `IconWidth` and `IconHeight` still available; icons and labels fit within
their control. `ShowButtonOutlines = false` hides the idle fills and borders
while preserving pressed feedback.

An optional per-button `Accent` overrides the automatic color: `"neutral"`,
`"cyan"`, `"amber"`, `"violet"`, `"orange"`, `"red"`, or `"green"`. For example,
`{ Text = "Home", Accent = "violet", Action = { Layer = "home" } }`.
Battery charging and low-charge colors take precedence over this override.
The shared slider uses cyan for volume (with a subdued muted fill) and amber
for brightness, with a speaker or sun identifying the selected control.

Tap a battery widget to cycle from the system battery through connected device
batteries (including `scope=Device` power supplies such as hidpp mice), then back
to the system. A small `SYS` or abbreviated device name with a device number
identifies the reading in all three battery display modes. Discovery and battery
reads run in the background, refreshing on power-supply events and every ten
seconds. A disconnected or unreadable selected device returns to the system.

`BatteryCycleEnabled = false` disables cycling and retains the configured button
action and original system-battery display. With no device battery, the widget
also retains that behavior. `BatteryCycleTimeoutSecs = 15` sets the automatic
return delay after the last cycling tap; it must be greater than zero. Both
settings have these defaults when absent from older configurations. Cycling,
automatic return, and removal of a selected source release held virtual keys and
discard existing touches; fingers already down must lift before pressing again.

The battery widget warns at a glance as its displayed charge runs low. While
the battery is not charging, below `BatteryWarningThreshold` percent (default
25) it paints the `BatteryWarningColor` accent, and below the lower
`BatteryCriticalThreshold` (default 10) it paints `BatteryCriticalColor`.
Both colors take the accent names, defaulting to the shipped clear orange and
red; thresholds are percentages 0-100 with the critical one at or below the
warning one. A battery exactly at a threshold still shows the milder tier, so
the escalation mirrors the historical hardcoded red below 10. Above the
warning point the widget renders exactly as before, with its configured
accent, and a charging battery stays green at every level. The tier is
resolved against the currently displayed battery, so system and cycled device
batteries escalate independently while each is the one shown. All four
settings have these defaults when absent from older configurations.

### Shared volume and brightness slider

The named-bar home has one cyan `VolumeSlider` button and one amber
`BrightnessSlider` button. They are one pair: size them with equal `Stretch`
values - the shipped example gives each 2 - so the two buttons render
identically. Pressing either reads its current hardware level and
replaces their entire combined region, including the gap, with one slider for
that control. Other controls stay accessible, with Mic at the far right.
Adjacent slider actions share their span on any bar; a standalone action uses
its own span. The media bar retains its single volume control.

Opening never changes the level or jumps to the activation position. The first
finger can drag after moving past the tap threshold; a later touch grabs the
drawn track directly. The speaker or sun zone selects the minimum. The slider
collapses about two and a half seconds after the last interaction; navigation,
Fn, reload and input release also restore the bar. To select the other control,
let the slider close and press its button. A second finger inside an open slider
cannot activate the hidden buttons.

Volume remains absolute: the far left sets all output channels to true zero,
the far right reaches 130%, and intermediate percentages remain reachable.
Absolute writes clear mute. The [desktop audio receiver](integrations/audio/README.md)
resolves the current output using the existing Omarchy sink resolver and returns
mixer readback.

While the slider is closed, a muted mixer is still visible: the volume button
wears the same slash the drawn slider lays over its speaker glyph. A slow
background mute watch feeds the mark - it probes the audio receiver every few
seconds, pauses while the slider performs its own reads, re-probes the moment
the slider collapses, and shows nothing until its first confirmed read, so a
missing receiver never invents a state. The mark overdraws the button's icon
or label only; button sizes, the brightness twin, and every hit target stay
exactly as configured.

Brightness uses the [display receiver](integrations/brightness/README.md) and
the working desktop `brightnessctl` interface for `apple-panel-bl`, with the
keyboard helper's linear 1–100% range. The minimum leaves the display on;
the Touch Bar and keyboard backlights are never selected. A separate read after
every write supplies confirmed brightness, rather than echoing the target.
Opening either slider reflects external changes without writing the other
control.

Both adapters run outside the render loop. Fast drag updates coalesce to the
latest target, and the fill follows confirmed readback while writes are pending.
Failures are logged and collapse the overlay; stale replies from an earlier
selection or opening cannot repaint it or overwrite a newer confirmed level.
The daemon keeps its existing `nobody`, home/tmp and AF_UNIX restrictions; the
narrow user receivers own session access and verify kernel peer UIDs. No host
permission changes or voice integration replacements are needed. Legacy volume
and brightness key actions continue to emit their configured keys.

### Command buttons and voice dictation

Any button can run a shell command on touch-down instead of - or combined
with - switching layers:

```toml
{ Text = "Sync", Action = { Command = "my-sync-script" } }
{ Text = "Mic", Accent = "cyan", Action = { Command = "voice-dictate-user start", Layer = "recording" } }
```

A `Command` action runs the configured string verbatim through `/bin/sh -c`.
Nothing observed at runtime is ever interpolated into it, so there is no
injection surface beyond the string written in the configuration file.
Commands run detached from the renderer - standard streams go to `/dev/null`
and each child is reaped on its own thread - so a slow or hung command never
blocks the bar. Spawn failures and nonzero exits are logged on stderr, which
under systemd lands in the journal. At most one invocation of a given command
string runs at a time; since a button's action is fixed, this caps concurrent
invocations per button, and a second touch while the previous child is still
alive is dropped with a log line instead of double-firing. Commands run as the
daemon's unprivileged user inside the service sandbox, so a helper must work
under those restrictions. An empty `Command` string is rejected, as is a
combined action naming an unknown layer.

Adding `Layer` to the table combines both effects in one press: the command
runs first, then the named layer opens. Combined buttons count as visible
navigation, so a sub-bar whose Stop or Done button returns home through a
command gets no extra Home button. The [multibar example](share/tiny-dfr/multibar.toml)
uses this for voice dictation: the home-bar mic button runs
`voice-dictate-user start` and opens a `recording` layer, which is the visual
state for the ongoing recording. That layer
holds a `Stop` button (runs `voice-dictate-user stop`, which stops recording and
types the transcript into whatever window has focus) and a `Cancel` button
(runs `voice-dictate-user cancel`, discarding a mis-start); both return home. The
mic button has a bright cyan `Mic` label, and the recording layer's middle
button is the live input waveform described below. The optional private
[desktop-session integration](integrations/voice/README.md) supplies a client
and recorder service. It crosses the daemon's unprivileged AF_UNIX boundary
without changing its privilege drop or network restrictions. Install that
integration separately; without a receiver these presses still switch layers
and log the failed command. Check the desktop notification or receiver journal
to confirm recording started.

### Custom quick keys

The multibar home bar carries a `More` button in its navigation cluster
(beside `F1-F12` and `Media`) opening the `more` layer: a parking lot for
one-press quick keys with room for as many entries as you want. Each quick
key is an ordinary button entry - a `Text` label or an `Icon` glyph - whose
action runs a shell command:

```toml
[[Layers]]
Name = "more"
Keys = [
    { Text = "Home", Action = { Layer = "home" } },
    { Text = "Screenshot", Action = { Command = "touchbar-capture-user screenshot", Layer = "home" } },
]
```

The shipped example opens the Omarchy screenshot picker and returns home in
one press. It cannot run the capture flow directly: the daemon's unprivileged
sandbox has no session environment and cannot reach the compositor's Wayland
socket, so the flow would silently no-op there (the flow's own failure path
exits 0, so not even the journal would show it). Instead the key calls a thin
client that hands the request to a small receiver running in the desktop
session, which runs the real `omarchy capture screenshot` there - the same
pattern as the voice integration below, with the command fixed on the
session side and never taken from the wire. The optional private
[desktop-session capture integration](integrations/capture/README.md)
supplies that client and receiver; without it the press still returns home
and the failed command is journalled. An entry without `Layer` stays on the
bar so several keys can run in a row, and the leading `Home` button always
leaves without running anything. Adding a quick key is a config edit, not a
code change; labels, glyphs, accents, and `Stretch` work as on any bar, and
command execution follows the command-button rules above (detached, at most
one invocation per command string, failures journalled). A quick key that
needs the desktop session must go through a session receiver client like
this one.

The shipped bar also carries a `Lock` key:

```toml
{ Text = "Lock", Accent = "amber", Action = ["LeftMeta", "LeftCtrl", "L"] }
```

It emits the compositor's lock bind - Omarchy's default `SUPER+CTRL+L`
(`omarchy-system-lock`) - as a pure key chord through the daemon's virtual
keyboard, the same mechanism every other key button uses: no shell, no
helper, nothing for the unprivileged sandbox to reach. Locking is the
compositor's own bind, so rebinding it in Hyprland moves the bar's lock key
too. If your compositor's lock bind differs, change the chord in this one
entry like any key combo button.

### Color themes

The bar's colors come from named themes: presets of the knobs the config
already expresses - the colors behind the accent names every button uses,
and the time-of-day background bands. Eight tasteful built-ins ship seeded
- `Default` (the live palette exactly as the deck always rendered it),
`Midnight`, `Nord`, `Ember`, `Rosewater`, `Forest`, `Solarized`, and `Mono` -
and owners add or override themes with `[Themes.Name]` tables:

```toml
ThemesEnabled = true     # set false to remove the theme buttons and picker
ActiveTheme = "Midnight" # optional: pin the startup theme from the config

[Themes.Mine.Accents]
neutral = "#9AA0A6"
cyan = "#40E0D0"
# ... any of the eight accent names, values "#RRGGBB"
[[Themes.Mine.DayCycleBands]]
Name = "day"
StartHour = 12
Color = "#1A1B1E"
```

A theme is partial by design: accents and bands it does not name are
inherited - from the shipped defaults for a new theme, or from the built-in
it shadows when overriding one by name. Invalid accent names, colors, or
bands reject the candidate configuration on reload, keeping the current one,
like every other knob. The alarm colors that must never follow a theme (the
waveform's dead-feed red) keep their shipped values.

Two activation surfaces reach the themes from the glass, both synthesized
onto the quick-keys `more` bar (default on, exactly like the pet button):

- A compact `Theme` cycle button: each tap applies the next theme in the
  seeded order, wrapping, and the button briefly wears the applied theme's
  name before returning to its own label. The tap is pure - no keys fire,
  no command runs, the bar stays put.
- A `Themes` button opening the picker: the theme list takes over the whole
  bar, one button per theme name plus the usual Home. Tapping a name
  applies that theme and returns home in the same press.

Applying is live and invisible to the service: the palette and the
background bands swap on the next frame - buttons, slider fills, accents,
the waveform tint, the strip - with no restart and no reload, the same
inotify reload path staying available for config edits. The choice is
remembered in `/var/lib/tiny-dfr/active-theme` (prepared by the install
notes below): it survives restarts, a config `ActiveTheme` pin wins over
it, and a remembered name that no longer exists falls back to the first
theme and heals the file, so a stale file can never wedge the bar. Up to 21
themes fit the picker bar; more reject the candidate configuration.

Authors reach the picker from other bars with their own buttons:
`Action = { Layer = "themes" }` opens it (a user-defined `[[Layers]]` entry
named `themes` replaces the synthesized picker, like the pet's layer),
`Action = "CycleTheme"` cycles in place, and
`Action = { Theme = "Nord" }` applies one theme by name, optionally with
`Layer = "home"` to switch in the same press. With `ThemesEnabled = false`
the synthesized layer and buttons disappear and buttons still targeting
`themes` reject the candidate configuration like any unknown layer.

### Live recording waveform

A button configured with `{ Waveform = true, Stretch = 3 }` - the middle of
the multibar example's `recording` layer - replaces the old static
`● Recording` label with the sound actually coming in: a single drawn line
that rests flat and silent until you speak, then ripples into a smooth
traveling wave whose height follows the last two seconds of microphone
input, beside a recording dot whose pulse follows the live level and a
duration timer counting `m:ss` from entering the layer. The button's
`Accent` key colors both the dot and the button's background tint; the
shipped default is `Accent = "blue"` (RGB 0.36, 0.60, 1.0 - a readable
blue on the Touch Bar's dark glass, matching the deck's accent palette).
Owners who prefer the previous red can set `Accent = "red"` on the button,
or any of the deck's other accents. Re-entering the
layer starts the timer and graph fresh. This indicates live capture as
reported by the receiver; it does not confirm transcription success. Owners
who prefer the previous look can set `WaveformStyle = "bars"` on the button
to keep the scrolling bar graph instead of the line.

By default the drawn level follows the measurement directly - unsmoothed
motion is the default feel. Owners who prefer a calmer line can opt in per
button with two optional keys: `WaveformSmoothing` switches on a render-side
envelope that rises fast (a third of the value) so speech visibly climbs
without delay and glides back down over the configured milliseconds - the
calm 150-300 ms class (e.g. 240) - and `WaveformLag` (e.g. 80) holds each
sample out of the graph for that many milliseconds (capped at 500) in
exchange for steadiness. Both keys leave the level feed untouched -
freshness is unchanged; the smoothing and lag shape only what is drawn, e.g.
`{ Waveform = true, Stretch = 3, WaveformSmoothing = 240, WaveformLag = 80 }`.

While that layer is displayed, the daemon polls a second abstract socket,
`dfr-voice-level-1000`, exposed by the same optional voice receiver: the
receiver passively tails the raw capture file its recorder is already
writing and answers each `get` with the latest 0-100 input peak (`ok <n>`) or
`idle` when no recording is active. The recorder runs unbuffered, so the tailed
file trails the live microphone by only a few milliseconds. Dictation
mechanics, audio routing, and
the receiver's existing `start`/`stop`/`cancel` behavior are unchanged. The
daemon verifies the receiver's kernel peer UID like the volume and brightness
receivers. Feed health is visible on the glass: a quiet but healthy feed
shows the widget's idle look - a dim dot, flat baseline, and the timer -
while a missing, rejecting, or garbling receiver shows an unmissable dead
state, a red slashed circle around the dot and a red `No feed` label, so a
dead source never reads as mere silence and never fakes a signal.

The repaint cadence stays bounded: the poller is armed only while the
recording layer is displayed, polling at 20 Hz, and the widget repaints just
that button's column at roughly 30 fps while its curve is in motion: between
two samples the whole line glides fractionally left and its newest end eases
to the measured level, so the wave flows instead of stepping one slot per
sample. Every other layer keeps its existing redraws, and a settled, silent
feed repaints nothing but the once-a-second timer tick.

### Media visualizer

The media bar ends with an opt-in music visualizer: a button that shows a
small waveform glyph beside a centered `Viz` label until tapped. A tap
expands it into a dedicated full-bar
view - the same drawn line the recording layer uses, flat at rest, rippling
into a smooth wave with the music actually playing on the default output -
spanning the whole Touch Bar except one `Off` button at the end that toggles
back to the plain media bar, which returns exactly as it was. The toggle and
its exit are pure layer switches: no key ever fires, and media playback is
never disturbed. It is off by default every time: leaving the view by any
navigation or reloading the configuration returns to the normal bars, and
nothing runs while it is off - no sockets, no capture. The monitor stream
exists only while the view is displayed, at the same 20 Hz poll cadence as the
recording waveform's feed.

```toml
{ Visualizer = true, Stretch = 2 }  # on the media bar
```

The feed is the optional [desktop-session receiver](integrations/viz/README.md):
`dfr-viz` captures the default sink's *output monitor* - the sound heading to
the speakers, not the microphone, regardless of which player owns it - and
serves one-shot level requests over the same abstract AF_UNIX and kernel
peer-UID boundary as the volume, brightness, and voice receivers. **The
receiver must be installed and its service enabled before the visualizer can
draw anything**: without it an enabled view shows an unmissable red `No feed`
marker instead of the calm dim baseline a quiet healthy feed rests on, and
the daemon logs one line per enable (`visualizer receiver unavailable; install
and enable integrations/viz/dfr-viz`) so a missing helper is diagnosable from
the journal instead of failing silently, exactly like the recording waveform
without its helper. Measured on the target machine, the enabled capture costs
about 0.5% of one CPU core, keeps the sinks suspended, and writes nothing to
disk; a five-second watchdog in the helper also stops the capture if the daemon
ever vanishes without turning it off. Changing the default output while
enabled requires toggling the visualizer off and on to follow the new sink.

The line reuses the recording waveform's motion knobs, per button:
`VisualizerStyle = "bars"` keeps the scrolling bar graph instead of the line.
The line itself follows the level directly, unsmoothed; `VisualizerSmoothing`
(ms decay; attack a third of it) with `VisualizerLag` (ms of visual delay,
capped at 500) are opt-in for a calmer motion, e.g.
`{ Visualizer = true, Stretch = 2, VisualizerSmoothing = 240, VisualizerLag = 80 }`.
v1 deliberately ships a level-driven wave rather than a full spectrum: the
level feed reuses the landed waveform line renderer and frame clock unchanged,
needs no FFT dependency in the narrow desktop helper, and keeps the wire
protocol a single `ok <level>` - a spectrum would have needed a new renderer,
a larger protocol, and a math dependency for marginal gain at Touch Bar size.

### Now playing text

The media bar also shows what is actually sounding: a `NowPlaying = true`
button between the transport controls and the volume slider renders the
playing track's title, and the artist as a small caption beneath it when
the player reports one. Long text is truncated to the row's budgets (32
characters for the title, 24 for the artist, each with an ellipsis) and
then fitted to the button like every other label, so an epic title never
overflows the row or shrinks past reading. The button is display-only:
taps are consumed without firing any key.

```toml
{ NowPlaying = true, Stretch = 2 }  # on the media bar
```

Every poll replaces the whole row, so it can never hold a stale track: a
track change lands within about a second, and a player that pauses or
stops clears the row instead of leaving the last song behind. When
nothing is playing, or metadata is unavailable, the row rests on a dim
`Nothing playing` label - never a broken row. The feed is the optional
[desktop-session receiver](integrations/now-playing/README.md):
`dfr-now-playing` reads the session bus's MPRIS players (preferring one
that reports `Playing`), strips control and format characters from the
metadata, and answers one-shot requests over the same abstract AF_UNIX
and kernel peer-UID boundary as the volume, brightness, voice, and
visualizer receivers. Without it the row stays in its empty state and
the daemon logs one line per arm
(`now-playing receiver unavailable; install and enable
integrations/now-playing/dfr-now-playing`), so a missing install is
diagnosable from the journal like every other helper. The poll runs at
one request per second and only while a now-playing bar is displayed -
metadata changes on the scale of songs, not frames - so the cost is a
handful of session-bus property reads, no stream, capture, or
subprocess.

## Time-of-day background

The strip behind every bar follows the local time of day: a warm dawn gray in
the morning, a brighter neutral slate at midday, warm amber toward evening,
and a dark cool blue overnight. The band color shows through the 16-pixel gaps
and behind every widget on every named layer, including the pet layer's open
ground beneath its painted sky and grass scene - a scene that itself follows
the same hours, with the sun riding low at dawn and dusk and a moon and stars
overnight. Key fills, accents, the shared
slider, and the pet sprites keep their existing design on top of the band
color, and nothing about touch behavior changes. The daemon repaints once
when the band changes, on the same wake cadence it already keeps for the
clock and battery widgets; there is no per-frame animation and no extra
wakeups.

```toml
DayCycleEnabled = true  # set false for the plain static black background

# Optional: replace the default bands entirely. Each band runs from its
# StartHour (local time) until the next begins; the last wraps past midnight.
[[DayCycleBands]]
Name = "day"
StartHour = 9
Color = "#1A1B1E"
[[DayCycleBands]]
Name = "night"
StartHour = 21
Color = "#04060D"
```

Bands need a unique nonempty `Name`, a `StartHour` between 0 and 23 unique
across bands, and a six-hex-digit `Color` like `"#1A1B1E"` (with or without
the leading `#`). The array replaces the shipped morning/day/evening/night
defaults as a whole, so a single band paints one color all day. Invalid bands
reject the candidate configuration like the other knobs, keeping the current
configuration on reload, and `DayCycleEnabled = false` preserves the previous
static background at every hour.

## Touch Bar pet

A small virtual pet, in the spirit of the 2016 Touchbar Pet, lives on its own
pet layer (a behavioral homage written from scratch - no original assets or
code). With `PetEnabled = true` the home bar gains a Pet button beside the
other navigation buttons - the same styling as Functions, Media, and More -
that opens the pet layer, a bar carrying a Home button like every other
sub-bar
and otherwise open ground. The pet exists and animates only inside its
layer; it never renders on or interferes with the home bar or any other
layer, and the layer follows the same rules as any named bar, including Fn
behavior, invalid-config safety, and input release on layer switches.
Closing the layer pauses the pet in place, and its state survives layer
switches and config reloads.

Inside the layer the pet wanders left and right with brief pauses and curls
up for naps. The layer is painted as a little scene: a blue sky with a sun
and clouds above the green ground strip the pet walks on, while buttons
keep their normal deck look. The cat is a chunky orange tabby - big head,
tabby stripes, white muzzle and chest, pink nose and inner ears, green
eyes - drawn as per-state pixel frames through the existing cairo surface
(no images are loaded and no font glyphs are required). It is noticeably
larger than the original pet, about four fifths of the bar height.
Tapping the pet directly startles it (it jumps and scurries away); when it
gets hungry it sits and begs, and tapping the hungry pet itself drops a
kibble right beside it. Tapping open ground anywhere else in the layer -
away from the pet and from the layer's buttons - drops a kibble right at
the tap, and the pet walks over and munches it, hungry or not. It holds
one kibble at a time, so a new drop replaces a waiting one; a napping or
startled pet finishes its act first and comes for the kibble after, and a
pet already munching ignores further drops until it is done. A satiety
value decays over about an hour and gates the behavior: hungry pets beg
and never nap, well-fed pets nap and wander. A direct tap on the pet is
always consumed and never fires the button underneath; food drops land
only on open ground and never steal taps aimed at the layer's buttons,
whether the pet is hungry or not.

The pet also lives in the time of day, mirroring the same local-hour bands
as the time-of-day background tint. Mornings bring zoomies - short pauses,
long roams, few naps - under a pale sky with the sun climbing low on the
left; midday keeps the plain tempo; evenings wind down with longer pauses
and likelier, longer naps under a warm dusk with the sun sinking low on the
right; and overnight a well-fed pet mostly sleeps beneath a moon and stars
where the sun stood, waking for the odd stroll or a midnight snack, since
hunger still overrides sleepiness exactly as by day. The stride and the
metabolism never change with the clock - only the tempo of life does - and
the scene repaints on the wake cadence the daemon already keeps.

```toml
PetEnabled = true   # set false to remove the Pet button and pet layer
PetVariant = "cat"  # or "fox"
PetEnergy = "calm"  # or "rowdy" for a faster, hungrier pet
```

Invalid values are rejected like the other knobs, and configurations without
pet keys keep the defaults above. A `[[Layers]]` entry named `pet` replaces
the synthesized playground, and an existing visible home-bar button targeting
`pet` replaces the synthesized Pet button. With the pet disabled the name
`pet` no longer exists, so buttons still targeting it are rejected like any
unknown layer. There is no scripting, network access, or persistence beyond
the running daemon.

Unknown targets, duplicate or empty names, and empty bars reject the candidate
configuration. Invalid TOML or layer definitions at startup log an error and use
the base configuration. Those errors on reload keep the current configuration
and selection. Valid reloads return to the new home bar; deleting
or renaming away the user configuration reloads the base. Direct writes and
atomic saves are watched. Layer changes and valid reloads release held virtual
keys and discard existing touches; fingers already down must lift and press
again on the new bar.

## Migration notes: the configuration is a contract now

The configuration file is validated as a contract: unknown keys are
rejected everywhere (previously a typo like `ShowButtonOutline` or a
button's `Tex` was silently ignored), documented ranges are enforced at
load time, and content errors that used to crash the daemon now reject
the candidate configuration instead. Every shipped config and example
passes unchanged; the tightening only bites files that were already
broken in ways the old daemon ignored, clamped, or crashed on.

If a configuration suddenly fails to load after upgrading:

1. Run `tiny-dfr check` on it; every diagnostic names the file, the TOML
   line where the parser knows it, and - for typos - the closest real
   knob name (`did you mean `ShowButtonOutlines`?`).
2. Unknown fields: fix the spelling, or remove keys the daemon never
   read. The `Svg` alias for `Icon` remains accepted forever.
3. Newly enforced ranges, each previously documented but not checked:
   `ActiveBrightness` 0-255; `Stretch` at least 1 (a zero used to be
   clamped to 1 with a console note); `WaveformLag`/`VisualizerLag` at
   most 500 ms (the renderer silently clamped larger values);
   `WaveformSmoothing`/`VisualizerSmoothing` at most 5000 ms;
   `IconWidth`/`IconHeight` 8-256 px; `BatteryCycleTimeoutSecs` 1-3600
   seconds; `Battery` one of icon, percentage, both; `WaveformStyle`
   and `VisualizerStyle` one of line, bars; a `Time` value that chrono
   can parse.
4. Icon and font names must resolve: a button `Icon` (or an XDG theme
   icon, or the battery icon set) that finds no file, or a
   `FontTemplate` no installed font matches, now rejects the candidate
   with the name and the searched paths - previously either one
   panicked the daemon and left the bar a dead bitmap until an admin
   edited the file by hand.

No `Version` key exists (recorded decision): every change so far is
additive, and adding one later would itself be additive. A future
migration that needs a hook can introduce it then.

## Reversible device installation after review

Branch development and offline checks do not require installation. After review
and approved landing, build on the target machine (or for its architecture) and
use the following commands for an existing `/usr/bin/tiny-dfr` systemd install.
They keep the existing system binary and select the new binary through a separate
service drop-in. The installed `/usr/share/tiny-dfr/config.toml` and icons must
already be available. Keep any desired global settings in the example before
installing it.

The schema pass adds one operator step to this dance: the rebuild below is a
workspace build (it compiles `crates/dfr-config` first), so on memory-limited
machines keep the capped `CARGO_BUILD_JOBS=1` form from "Build and check" -
that capped rebuild is the operator step at landing - and validate the live
user configuration with `tiny-dfr check /etc/tiny-dfr/config.toml` before the
restart so the stricter validation surprises no one at boot.

Run this block in one shell and retain the printed backup path. It refuses to
overwrite an earlier multibar binary or drop-in.

```sh
set -e
sudo test ! -e /usr/local/libexec/tiny-dfr-multibar
sudo test ! -e /etc/systemd/system/tiny-dfr.service.d/multibar.conf
dfr_backup=$(sudo mktemp -d /var/lib/tiny-dfr-multibar.XXXXXXXX)
if sudo test -e /etc/tiny-dfr/config.toml; then
    sudo cp -a /etc/tiny-dfr/config.toml "$dfr_backup/config.toml"
else
    sudo touch "$dfr_backup/no-user-config"
fi
printf 'Backup path: %s\n' "$dfr_backup"
sudo install -D -m 0755 target/release/tiny-dfr /usr/local/libexec/tiny-dfr-multibar
sudo install -d /etc/tiny-dfr /etc/systemd/system/tiny-dfr.service.d
sudo install -m 0644 share/tiny-dfr/multibar.toml /etc/tiny-dfr/config.toml
printf '[Service]\nExecStart=\nExecStart=/usr/local/libexec/tiny-dfr-multibar\n# The daemon (running as nobody after its privilege drop) remembers the\n# theme chosen on the glass here; ExecStartPre prepares the one writable\n# directory ProtectSystem=strict otherwise denies.\nExecStartPre=/usr/bin/install -d -o nobody -g nobody -m 0755 /var/lib/tiny-dfr\nReadWritePaths=/var/lib/tiny-dfr\n' |
    sudo tee /etc/systemd/system/tiny-dfr.service.d/multibar.conf >/dev/null
sudo systemctl daemon-reload
sudo systemctl restart tiny-dfr.service
sudo systemctl status tiny-dfr.service --no-pager
```

On the device, check Home → Functions → Home and Home → Media → Home, all
F1-F12/media controls, the battery, Home → More → Screenshot → Home (the
screenshot flow runs and the bar returns home), Home → More → Lock (the
compositor locks; unlock and check the bar again), Home → More → Theme
(each tap applies the next theme with the name flashing on the button; the
palette and strip repaint at once) and Home → More → Themes → a theme name
→ Home (the picker lists every theme, the tap applies it and returns), then
restart the service and confirm the last theme is still the one worn, then
Home → Pet → pet
wandering, feeding, and startling → Home, plus Fn press/release. With the
desktop-session voice integration installed, check the mic button opens
the recording layer while dictation starts, and that Stop types the
transcript into the focused window while Cancel discards; without the receiver
the layers still switch and the journal logs the failed command. Hold a key
while switching with another finger and while saving a valid configuration;
verify its release and that the old finger cannot activate a new button. Test an
invalid target reload, an atomic save, and returning to the previous configuration.

To roll back, set `dfr_backup` to the retained backup path in the shell below:

```sh
set -e
dfr_backup=/var/lib/tiny-dfr-multibar.REPLACE_WITH_BACKUP_SUFFIX
if sudo test -e "$dfr_backup/config.toml"; then
    sudo cp -a "$dfr_backup/config.toml" /etc/tiny-dfr/config.toml
else
    sudo test -e "$dfr_backup/no-user-config"
    sudo rm -f /etc/tiny-dfr/config.toml
fi
sudo rm /etc/systemd/system/tiny-dfr.service.d/multibar.conf
sudo rm -rf /var/lib/tiny-dfr
sudo systemctl daemon-reload
sudo systemctl restart tiny-dfr.service
sudo rm /usr/local/libexec/tiny-dfr-multibar
sudo systemctl status tiny-dfr.service --no-pager
```

## License

tiny-dfr is licensed under the MIT license, as included in the [LICENSE](LICENSE) file.

* Copyright The Asahi Linux Contributors

Please see the Git history for authorship information.

tiny-dfr embeds Google's [material-design-icons](https://github.com/google/material-design-icons)
which are licensed under [Apache License Version 2.0](LICENSE.material)
Some icons are derivatives of material-icons, with edits made by kekrby.
