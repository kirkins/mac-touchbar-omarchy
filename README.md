# mac-touchbar-omarchy

A dynamic Touch Bar deck for the Apple Silicon MacBook Pro 13" (M1 and M2) on
[Omarchy](https://omarchy.org). It is a fork of Asahi Linux's
[tiny-dfr](https://github.com/AsahiLinux/tiny-dfr) that turns the plain function
row into named layers with sliders, now playing, a music visualizer, a battery
widget, quick keys, color themes, an on-glass pet and a native Settings app. It
installs beside your distribution's `tiny-dfr` package through a systemd drop-in,
and `make uninstall` hands the bar straight back to stock tiny-dfr.

<!-- TODO: device photo or GIF of the bar on the MacBook -->

![The Home bar: volume, brightness, play/pause, F1-F12, Media, More, battery and Settings](assets/deck-home.png)

![The Media bar: Home, previous, play/pause, next, now playing, volume and Viz](assets/deck-media.png)

![The Function bar: Home and F1-F12](assets/deck-function.png)

*The Home, Media and Function layers of the shipped deck, rendered at the bar's
real 2008x60 resolution by the same code that draws the Touch Bar
([about these images](assets/README.md)).*

## Features

- **Layers:** Home, Function (F1-F12), Media and More, switched from the bar or
  by holding Fn. Define your own in TOML.
- **Volume and brightness sliders** that read the real hardware level, plus a
  mute mark on the volume button.
- **Now playing:** the current track's title and artist on the Media bar.
- **Music visualizer:** a full-bar waveform of whatever is playing.
- **Battery widget** that cycles through connected device batteries and changes
  color as charge runs low.
- **Quick keys:** Screenshot, Lock and Settings, plus any shell command you add.
- **Color themes:** fourteen built in, custom themes, and a background that
  follows the time of day.
- **A pet** - a cat or a fox - that wanders, naps and begs for food on its own
  layer.
- **Touch Bar Settings,** a native GTK4/libadwaita app with a live preview.
- **Voice dictation** (optional): tap Mic, speak, and the transcript is typed
  into the focused window.

## Requirements

- A 13" MacBook Pro with Touch Bar on Apple Silicon: M1 (`MacBookPro17,1`) or
  M2 (`Mac14,7`). Development and testing happen on the M1.
- The Asahi Linux kernel, with the Touch Bar's display, touch and backlight
  drivers working.
- Omarchy (Hyprland), with PipeWire and a systemd user session.
- The distribution's `tiny-dfr` package, installed and working.
- To build: Rust and the usual cairo/librsvg/libinput development files, plus
  GTK 4.10+ and libadwaita 1.5+ for the Settings app. See
  [INSTALL.md](INSTALL.md).

Intel Macs with a T1 or T2 Touch Bar are not supported. Some Intel identifiers
remain in the code from upstream, but nothing here is built or tested for them.

## Quickstart

Build and install the daemon, then install the desktop helpers as your normal
user:

```sh
git clone https://github.com/kirkins/mac-touchbar-omarchy
cd mac-touchbar-omarchy
make build CARGO_JOBS=4        # Rust build; the default CARGO_JOBS=1 suits low-memory machines
sudo make install              # installs beside the tiny-dfr package and creates the tiny-dfr account
sudo systemctl daemon-reload && sudo systemctl restart tiny-dfr.service
make install-user-session USER_HOME="$HOME"
systemctl --user daemon-reload
systemctl --user enable --now touchbar-capture dfr-volume dfr-brightness dfr-now-playing dfr-viz
```

On Arch you can instead build the package in `packaging/arch` with
`makepkg -si`. [INSTALL.md](INSTALL.md) covers the details, upgrades, optional
voice dictation and troubleshooting.

## Uninstall

```sh
sudo make uninstall && sudo systemctl daemon-reload && sudo systemctl restart tiny-dfr.service
make uninstall-user-session USER_HOME="$HOME"
```

Stock tiny-dfr takes over again. Your `/etc/tiny-dfr/config.toml` is kept.

## Configure

Open **Touch Bar Settings** from the app menu or the Settings key on the bar,
or edit `/etc/tiny-dfr/config.toml` directly. The daemon reloads valid changes
on its own and keeps the current configuration if a file is invalid. Check a
file before installing it with `tiny-dfr-deck check /etc/tiny-dfr/config.toml`.
The [configuration reference](docs/configuration.md) covers layers, buttons,
actions and every knob; [themes.md](docs/themes.md) covers colors and the day
cycle.

## Documentation

- [INSTALL.md](INSTALL.md) - install, upgrade, uninstall, troubleshooting.
- [docs/configuration.md](docs/configuration.md) - the config file, layers,
  buttons, actions, validation and the full knob reference.
- [docs/themes.md](docs/themes.md) - color themes, theme buttons and the
  time-of-day background.
- [docs/widgets.md](docs/widgets.md) - sliders, battery, now playing,
  visualizer and recording waveform.
- [docs/pet.md](docs/pet.md) - the Touch Bar pet.
- [docs/settings-app.md](docs/settings-app.md) - the Settings app, Save and
  Apply, drafts.
- [docs/development.md](docs/development.md) - building, testing, CI and the
  pinned render tests.
- [CHANGELOG.md](CHANGELOG.md) - changes, upgrade and migration notes.

Desktop helpers, each with its own setup and privacy notes:

- [audio](integrations/audio/README.md) - the volume slider.
- [brightness](integrations/brightness/README.md) - the brightness slider.
- [capture](integrations/capture/README.md) - the Screenshot and Settings keys.
- [now-playing](integrations/now-playing/README.md) - the now-playing row.
- [viz](integrations/viz/README.md) - the music visualizer.
- [voice](integrations/voice/README.md) - optional cloud dictation.

## Contributing

Issues and pull requests are welcome. [docs/development.md](docs/development.md)
explains how to build, run the Rust and Python test suites, and what CI checks
on every push and pull request.

## License

This project is a fork of [AsahiLinux/tiny-dfr](https://github.com/AsahiLinux/tiny-dfr)
(MIT), forked at v0.3.7. Upstream code is © the Asahi Linux contributors; the
fork's additions are © 2026 Philip Kirkbride. Both are MIT - see
[LICENSE](LICENSE). Icons are from Google's
[material-design-icons](https://github.com/google/material-design-icons)
(Apache-2.0, see [LICENSE.material](LICENSE.material)); some are derivatives
edited by kekrby. Not affiliated with Asahi Linux or Omarchy.
