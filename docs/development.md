# Development

Building, testing and checking the workspace, what CI runs, and how the pinned
render tests work. Back to the [README](../README.md).

## Dependencies

- A Rust toolchain compatible with `Cargo.lock`, a C compiler and linker, make
  and pkg-config.
- Development files for cairo, libinput, freetype, fontconfig, librsvg **2.59+**,
  GLib/GIO, Pango, GDK-Pixbuf, libdrm and libudev. The kernel needs uinput.
- GTK **4.10+** and libadwaita **1.5+** for the Settings app only.
- A font that fontconfig resolves for `:bold` (the default `FontTemplate`).
- Python 3.11+ for the desktop helpers and their tests.

On Arch: `base-devel rust pkgconf cairo libinput freetype2 fontconfig librsvg
glib2 pango gdk-pixbuf2 libdrm systemd-libs gtk4 libadwaita`. Fedora's GUI
packages are `gtk4-devel libadwaita-devel`; Debian/Ubuntu's are `libgtk-4-dev
libadwaita-1-dev`. [INSTALL.md](../INSTALL.md) has the full list.

The Nix flake is inherited from upstream and unsupported: its version, asset
paths and GUI dependencies have not been updated for the workspace or the
Settings app. Use `make`.

## Layout

| Path | What it is |
| --- | --- |
| `src/` | The `tiny-dfr` daemon binary (root package): DRM, input, uinput, backlight, commands, `check`. |
| `crates/dfr-config` | The configuration model: parsing, merging, validation, the knob registry, the TOML writer. |
| `crates/dfr-render` | The draw path and widgets: layout, themes, day cycle, slider, waveform, now playing, pet. |
| `crates/dfr-configurator` | The GTK4/libadwaita Settings app. |
| `share/tiny-dfr/` | Base config, example decks, themes and icons. |
| `integrations/*/` | Desktop-session helpers (Python) with their units, READMEs and tests. |
| `etc/` | The systemd drop-in template and sysusers.d entry used by `make install`. |
| `tests/` | Staging tests for `make install` and friends. |

## Build and check

```sh
make build CARGO_JOBS=4    # release build of the whole workspace
```

`CARGO_JOBS` defaults to 1 so low-memory machines do not run out of RAM. The
same applies to the check commands; on a small machine keep one job and run
them one after another:

```sh
cargo fmt --all -- --check
CARGO_BUILD_JOBS=1 cargo test --locked --workspace -- --test-threads=1
CARGO_BUILD_JOBS=1 cargo clippy --locked --workspace --all-targets
CARGO_BUILD_JOBS=1 cargo build --locked --workspace --release
```

`cargo build -p tiny-dfr` builds only the daemon and does not need GTK.

Python tests:

```sh
python3 -B -m unittest discover -s tests -p test_install.py   # install staging tests
python3 -B -m unittest discover -s integrations/capture -p 'test_*.py' -v
```

Each integration directory has its own `test_*.py`; run them the same way. The
staging tests install into a temporary `DESTDIR` with stub binaries, never touch
the host, and refuse to run as root. They also check that every relative `.md`
and `.png` link in the README, changelog, `docs/` and integration READMEs still
resolves after installation.

## CI

`.github/workflows/ci.yml` runs on every push and pull request in an
`archlinux:latest` container:

1. Install the build and test dependencies (plus `ttf-liberation` and
   `desktop-file-utils`).
2. Pin `sans-serif` to Liberation Sans (see below).
3. Stage `share/tiny-dfr/*.toml` and icons into `/usr/share/tiny-dfr`, the fixed
   icon path the Settings app resolves from.
4. As an ordinary `builder` user: `cargo fmt --check`, build, test, clippy, then
   the install staging tests.

## Pinned frame digests and fonts

`shipped_multibar_frames_match_the_pinned_pixel_digests` in `dfr-render` renders
the shipped deck and compares exact pixel digests, pinned in
`crates/dfr-render/src/config.rs`. That makes it depend on font resolution:
Omarchy maps the generic `sans-serif` family to Liberation Sans, so `:bold`
resolves to Liberation Sans Bold. On a bare container or another distribution
`:bold` resolves to a different face and the digests fail even though nothing
regressed. Provide that mapping (CI writes it to `/etc/fonts/local.conf`) or
install Liberation Sans before treating such a failure as real. Never weaken or
re-pin the digests to make it pass.

The images in [assets/](../assets/README.md) are rendered from these same pinned
frames; that README explains how to regenerate them.

## Exporting frames for review

Set `DFR_DECK_REVIEW_OUTPUT` to write real Cairo frames to a directory, with no
Touch Bar and no commands executed:

```sh
DFR_DECK_REVIEW_OUTPUT=/tmp/dfr-deck-review CARGO_BUILD_JOBS=1 \
    cargo test --locked -p dfr-render consumer_multibar_settings_dispatch_and_renders_offline -- --test-threads=1
```

| Frame | Shows |
| --- | --- |
| `implemented-home.png` | The Home bar. |
| `implemented-home-muted.png` | Home with the mute mark on the volume button. |
| `implemented-volume-mid.png`, `implemented-brightness.png` | The open slider. |
| `implemented-more.png` | The More bar's quick keys. |
| `implemented-media-viz.png`, `implemented-media-off.png` | The visualizer view, and the plain Media bar after it. |
| `implemented-media-playing.png` | Media with a track's title and artist. |
| `implemented-themes.png` | The theme picker. |
| `implemented-home-nord.png` | Home with Nord applied. |
| `implemented-more-theme-flash.png` | The cycle button showing the applied theme's name. |
| `implemented-daycycle-{morning,day,evening,night}.png` | Home over each background band. |

The theme frames add `CycleTheme` and picker buttons to the example; the shipped
More bar does not have them.

`approved_multibar_preserves_voice_dispatch_and_renders_offline` does the same
for `multibar-voice.toml`, capturing command strings without running them and
writing `voice-`-prefixed frames, including
`voice-implemented-recording-live.png` and `voice-implemented-recording-dead.png`.

Running the whole suite with the variable set also writes the pet's frames:
`pet-food-drop.png`, `pet-food-walk.png`, `pet-food-munch.png` and
`pet-scene-{morning,day,evening,night}.png`.

Frames use the configured font and icon paths. Live values such as the battery
are illustrative.

## The knob reference

The knob tables in [configuration.md](configuration.md) are generated from the
registry in `crates/dfr-config/src/registry.rs` (`reference_markdown()`). A unit
test compares the block between the `dfr-config knob reference` marker comments
with the generated output and fails on any drift. When you add or change a knob,
update the registry and paste the regenerated block.

## Settings app tests

Headless tests cover merge and reset, round trips, all action forms, rename and
delete dependencies, theme limits, failed-save retention, atomic saves and the
close guard. A native acceptance test needs a display; it exercises selection,
repeated structure changes, save feedback, Enlarge, inherited controls, and
wide, short, narrow and large-text layouts against throwaway files:

```sh
cargo test --locked -p dfr-configurator -- --test-threads=1
G_DEBUG=fatal-criticals cargo test --locked -p dfr-configurator \
  native_selection_structure_save_feedback_and_layouts -- --ignored --test-threads=1
```

Set `DFR_CONFIGURATOR_PROOF_DIR` to a scratch directory to save layout captures.
The native test uses the app's resizable window and waits for mapped widgets and
a GTK paint; fixed sleeps or forced allocation right before a snapshot can
capture stale UI. No test writes the live configuration or touches a service.

## Waveform repaint budget

A waveform repaint test enforces a 2 ms budget. Early runs exceeded it on a cold
start; the cause was Cairo resolving its default font implicitly on first draw.
The waveform widget now resolves and keeps its own timer font at construction
and reuses it in each short-lived drawing context. Configured layer fonts and
font options still take priority, and nothing is measured or drawn during
construction. The repaint test does not cover construction time or on-device
latency; font and transform pixel-equivalence checks cover the draw path
separately.

## What the unit tests cover

The suite runs without a Touch Bar, DRM device or virtual keyboard. It covers:

- **Configuration:** compatibility, navigation, Fn behavior, reloads, emitted
  key presses and releases.
- **Battery:** widget cycling.
- **Slider:** selection without stepping, shared-region geometry and drag math,
  safe ranges, auto-collapse timing, confirmed readback, failures, stale replies,
  selection isolation, and the mute mark (arming only on bars with a volume
  slider, pausing during slider reads, re-probing on collapse, keeping the last
  state on failure, identical rendering in both slider states).
- **Commands:** parsing, combined layer dispatch, detached execution, the
  one-invocation-per-command cap, and the More bar's quick keys and screenshot
  flow.
- **Themes:** built-in order, readability and cycle wrap; user themes merging by
  name with partial inheritance; invalid definitions rejected; the picker layer
  and synthesized buttons; live apply; the remembered-choice file's precedence,
  healing and atomic saves; the cycle button's name flash; unknown names
  ignored; waveform retint without a deck rebuild.
- **Recording waveform:** level history, line and bar geometry, flatness,
  bounds and continuity, incremental repaints, recording-only fast cadence, the
  timer, live and idle rendering, and the level poller's arming, reply parsing
  and peer-UID check against fake receivers.
- **Visualizer:** off by default, the Viz tap switching in without keys,
  Off/navigation/reload switching out, capture armed exactly while shown, a lost
  consumer still sending `off`, rendering, idle wakes, config parsing, and the
  helper's capture, protocol and watchdog against fakes.
- **Pet:** layer wiring and toggle, wander bounds, pause and resume, nap gating,
  satiety and feeding, food drops and walk-to-food, startle, tap precedence,
  reloads, and time-of-day mood and scene.
- **Background:** band selection across hours and the midnight wrap, color
  conversion, defaults, overrides and rejections, the disabled background, the
  strip behind widgets and the pet layer, and incremental redraws with no
  black residue.

## Device checks

Some things only a real Touch Bar can show: physical layout and touch accuracy,
brightness, media-key delivery to the desktop, live battery and clock refresh,
the slider's drag feel, command buttons with real helpers, the pet, the
visualizer with real playback, and the repaint at a real band change. A useful
pass after installing a build:

- Home → F1-F12 → Home and Home → Media → Home; every function and media key.
- Fn press and release.
- Both sliders: open, drag, auto-close.
- Battery: repeated tap-to-cycle, device labels in each display mode, hotplug,
  and the timed return.
- Home → More → Screenshot (the picker runs and the bar returns home) and
  More → Lock (unlock and check the bar again).
- Home → Settings opens the app.
- Home → More → Pet: wandering, feeding, startling, then Home.
- With theme buttons added: Theme cycles with the name flash; Themes → a name
  applies and returns home; after a service restart the remembered theme comes
  back when `ActiveTheme` is not set, and a fixed `ActiveTheme` wins.
- With the voice profile and helper: Mic opens the recording layer and starts
  dictation, Stop types into the focused window, Cancel discards.
- Hold a key while switching layers with another finger and while saving a
  valid configuration: the key releases and the old finger cannot press a new
  button.
- Reload an invalid target (the bar keeps its configuration), save atomically,
  and return to the previous configuration.
