# Changelog

Notable changes to mac-touchbar-omarchy, newest first. Back to the
[README](README.md).

The project has its own version line, starting at 0.1.0 (set in `Cargo.toml`
on 2026-10-16). No fork release has been tagged yet, so everything below is
unreleased; the sections group it by when it landed.

## Unreleased

### Installation and packaging

- **Installs beside the distribution's `tiny-dfr` package.** `make install` puts
  the deck daemon in `libexec` (`/usr/local/libexec/tiny-dfr-multibar`) and adds
  a systemd drop-in (`tiny-dfr.service.d/multibar.conf`) that points the
  packaged `tiny-dfr.service` at it. It no longer replaces `/usr/bin/tiny-dfr`,
  the package's unit, its udev rules or `/usr/share/tiny-dfr`.
- **Dedicated `tiny-dfr` account.** The daemon drops privileges to a `tiny-dfr`
  system account (created through `sysusers.d`) instead of the shared `nobody`,
  and the desktop helpers accept requests only from that account's UID. Installs
  that predate the account fall back to `nobody`.
- **Desktop helpers follow the seat's active user** instead of assuming UID
  1000: `TINY_DFR_DESKTOP_UID` if set in the service environment, else seat0's
  active user from logind, else 1000. The Settings launcher finds
  `dfr-configurator` on `PATH`.
- **`tiny-dfr-deck check FILE`** validates a configuration. `make install`
  links `tiny-dfr-deck` in the bin directory to the deck daemon;
  `/usr/bin/tiny-dfr` is the stock upstream binary and has no `check`.
- **New install locations.** Example decks install to `share/tiny-dfr-deck/`
  (for example `/usr/local/share/tiny-dfr-deck/multibar.toml`), documentation to
  `share/doc/tiny-dfr-deck`. A fresh install seeds `/etc/tiny-dfr/config.toml`
  with the full deck only when no configuration exists.
- **`make install-user-session`** installs every desktop helper except voice
  (capture, volume, brightness, now playing, visualizer). Voice stays a separate
  opt-in.
- **`make uninstall` and `make uninstall-user-session`** added. Uninstalling
  hands the bar back to stock tiny-dfr and keeps `/etc/tiny-dfr/config.toml`.
- **Arch PKGBUILD** in `packaging/arch` (`makepkg -si`). Package users set up
  the desktop helpers with `tiny-dfr-deck-user-setup install`.

### Themes

- **Aurora Noir** built-in theme (fourteen in total): mint, cyan and violet
  accents over an inky strip that deepens at night.

### Upgrading

Desktop helpers installed by earlier versions accept requests only from
`nobody`. Once the daemon runs as `tiny-dfr`, reinstall them, then restart
them, or the sliders, now playing, the visualizer and the Screenshot and
Settings keys will not respond:

```sh
make uninstall-user-session USER_HOME="$HOME" && make install-user-session USER_HOME="$HOME"
systemctl --user restart touchbar-capture dfr-volume dfr-brightness dfr-now-playing dfr-viz
```

The upgrade section of [INSTALL.md](INSTALL.md) has the details, including how
to move from the earlier manual `tiny-dfr-multibar` recipe or from an older
`make install` that replaced `/usr/bin/tiny-dfr`.

### Documentation

- The README is now a short overview. Reference material moved to
  [docs/](docs/configuration.md), and history and migration notes to this file.

## 2026-10-16: Public repository

- Package metadata points at <https://github.com/kirkins/mac-touchbar-omarchy>.
- The root package gets its own version, 0.1.0, instead of upstream's 0.3.7.
  No behavior change.
- CI builds, tests and lints the workspace and runs the install staging tests on
  every push and pull request.
- The README gained real rendered images of the shipped deck.
- The supported surface is stated: the MacBook Pro 13" with Touch Bar on Apple
  Silicon (`MacBookPro17,1`, `Mac14,7`) under Omarchy. The Intel/T2 backlight
  identifiers kept in the udev rules are upstream heritage, not Intel support.

## 2026-10-12 to 2026-10-15: Settings app and the configuration contract

- **Touch Bar Settings** (`dfr-configurator`), a native GTK4/libadwaita app,
  redesigned around Layers, Appearance and Behavior. Save writes a user draft;
  Apply installs it separately. Home gets a Settings key, routed through the
  capture helper.
- The draw path moved into the `dfr-render` crate; the daemon uses it unchanged.
- Five more built-in themes (thirteen in total). Theme buttons became opt-in.
- The pet opener moved from Home to the More layer.
- The waveform widget keeps its own timer font, fixing a cold-start repaint
  spike.
- Standalone voice profile (`multibar-voice.toml`); the default deck no longer
  carries a Mic button.

### The configuration is a contract

The configuration model moved into the `dfr-config` crate, and `check` was
added. The file is now validated strictly:

- Unknown keys are rejected everywhere. Before, a typo such as
  `ShowButtonOutline` or a button's `Tex` was silently ignored.
- Documented ranges are enforced at load time.
- Content errors that used to crash the daemon now reject the candidate file
  instead.

Every shipped config and example passes unchanged; only files that were already
broken in ways the old daemon ignored, clamped or crashed on are affected. The
daemon is now a workspace build, so on low-memory machines keep one build job
(`make build`, or `CARGO_BUILD_JOBS=1`), and validate the live configuration
before restarting the service so the stricter rules do not surprise you at
boot.

If a configuration stops loading after upgrading:

1. Run `tiny-dfr-deck check` on it. Each diagnostic names the file, the TOML
   line when the parser knows it, and for typos the closest real knob
   (``did you mean `ShowButtonOutlines`?``).
2. **Unknown fields:** fix the spelling or remove keys the daemon never read.
   `Svg` stays accepted as an alias for `Icon`.
3. **Newly enforced ranges** (all previously documented, not checked):
   `ActiveBrightness` 0-255; `Stretch` at least 1 (a zero used to be clamped to
   1 with a console note); `WaveformLag` / `VisualizerLag` at most 500 ms (the
   renderer used to clamp silently); `WaveformSmoothing` / `VisualizerSmoothing`
   at most 5000 ms; `IconWidth` / `IconHeight` 8-256 px;
   `BatteryCycleTimeoutSecs` 1-3600 s; `Battery` one of `icon`, `percentage`,
   `both`; `WaveformStyle` and `VisualizerStyle` one of `line`, `bars`; a `Time`
   value chrono can parse.
4. **Icons and fonts must resolve.** A button `Icon` (or XDG theme icon, or
   battery icon) with no file, or a `FontTemplate` no installed font matches,
   now rejects the file and names the searched paths. Before, either one
   panicked the daemon and left the bar a dead bitmap until someone edited the
   file by hand.

## 2026-10-08 to 2026-10-11: The deck

- Named color themes and a Lock key.
- The pet gained a time-of-day mood and scene.
- Two-threshold battery warning colors (#15).
- Now-playing title and artist on the Media bar (#14).
- Waveform glyph on the idle Viz button (#13).
- Mute mark on the volume button while the slider is closed (#12).
- Volume and brightness slider buttons sized as an identical pair (#11).
- A dead feed is visible on the waveform widgets (#10).
- The Screenshot key goes through a desktop-session helper (#9).
- The Home-bar clock was replaced by a More bar of custom quick keys (#8).
- Opt-in music visualizer on the Media bar, expanding to a full-bar view.
- Live microphone level waveform on the recording layer: a line by default,
  blue accent, interpolated frames, optional smoothing and lag (unsmoothed by
  default).
- Time-of-day background tint.
- Pet food drops on open ground.
- One shared region for the volume and brightness slider; absolute volume
  endpoints (true zero to 130%).
- Semantic accent colors across the deck.
- Voice dictation repaired with desktop-session IPC.
- Command buttons: run a shell command on touch-down, optionally with a layer
  switch (#6).

## 2026-10-06 to 2026-10-07: The fork begins

- Named Touch Bar layers with Home navigation (#1).
- Tap the battery to cycle through connected device batteries (#2).
- A virtual pet on its own layer (#3, #4).
- A transient, draggable volume slider on the Home bar (#5).

## Upstream base

Forked from [AsahiLinux/tiny-dfr](https://github.com/AsahiLinux/tiny-dfr) at
v0.3.7 (2026-03-01). The fork also carries these later upstream commits: a
battery state fallback for all architectures, the input 0.9 update, every
possible button on the virtual keyboard, the cairo-rs / librsvg-rebind /
freetype-rs bumps, configurable icon sizes, `DoublePressSwitchLayers`, an
optional display backlight path, and clearing tracked touches on config reload.
See upstream's history for earlier changes.
