# Install mac-touchbar-omarchy

The deck installs **beside** your distribution's `tiny-dfr` package. Its daemon
goes to `libexec`, and a systemd drop-in points the packaged
`tiny-dfr.service` at it. Nothing the package owns is replaced (`/usr/bin/tiny-dfr`,
its unit, udev rules and `/usr/share/tiny-dfr` icons), so uninstalling, or just
removing the drop-in, gives you stock tiny-dfr back. Package upgrades of
`tiny-dfr` don't undo the deck.

## Requirements

- **Hardware:** the Touch Bar MacBook Pro 13-inch on Apple Silicon:
  `MacBookPro17,1` (M1, tested) or `Mac14,7` (M2, expected to work, untested).
  Intel/T2 Macs aren't supported.
- **System:** Asahi Linux kernel support for the Touch Bar, the distribution's
  `tiny-dfr` package installed and showing the stock bar, and an Omarchy
  (Hyprland) Wayland session with a systemd user manager and PipeWire.
  Screenshot uses Omarchy's capture flow; the Lock key sends Omarchy's
  `SUPER+CTRL+L` binding.
- **Build:** Rust/Cargo, a C toolchain, `make`, `pkg-config`, and development
  files for Cairo, libinput, FreeType, fontconfig, librsvg **2.59+**, GLib,
  Pango, GDK-Pixbuf, libdrm, libudev, **GTK 4.10+** and **libadwaita 1.5+**.
  On Arch: `base-devel rust cairo libinput freetype2 fontconfig librsvg glib2
  pango gdk-pixbuf2 libdrm systemd-libs gtk4 libadwaita`.

## Install from source

```sh
git clone https://github.com/kirkins/mac-touchbar-omarchy
cd mac-touchbar-omarchy
make build CARGO_JOBS=4      # leave CARGO_JOBS at its default of 1 on low-memory machines
sudo make install
sudo systemctl daemon-reload && sudo systemctl restart tiny-dfr.service
```

`make install` copies files and, on a real install, creates the daemon's
account with `systemd-sysusers`. It never restarts services or overwrites an
existing `/etc/tiny-dfr/config.toml`; when there is none, it seeds it with the
full deck. It installs (default `PREFIX=/usr/local`):

| Path | What |
| --- | --- |
| `/usr/local/libexec/tiny-dfr-multibar` | the deck daemon |
| `/usr/local/bin/tiny-dfr-deck` | the same binary, for `tiny-dfr-deck check FILE` |
| `/usr/local/bin/dfr-configurator` | Touch Bar Settings (GTK app) and its desktop entry |
| `/usr/local/bin/touchbar-capture-user` | client the Screenshot/Settings keys use |
| `/etc/systemd/system/tiny-dfr.service.d/multibar.conf` | drop-in that runs the deck daemon |
| `/etc/sysusers.d/tiny-dfr-deck.conf` | the `tiny-dfr` account the daemon drops to |
| `/usr/local/share/tiny-dfr-deck/` | example decks (`multibar.toml`, `multibar-voice.toml`) and themes |
| `/usr/local/share/doc/tiny-dfr-deck/` | this guide, the README, docs and licenses |

To stage without touching the machine (for review or packaging), add
`DESTDIR=/some/dir`. Packagers use `PREFIX=/usr UNITDIR=/usr/lib/systemd/system
SYSUSERSDIR=/usr/lib/sysusers.d SEED_CONFIG=0`.

### Arch package

`packaging/arch/PKGBUILD` builds `tiny-dfr-deck` from a release tag. It
depends on `tiny-dfr` rather than replacing it, and pacman's hooks create the
account and reload systemd:

```sh
cd packaging/arch && makepkg -si
sudo systemctl restart tiny-dfr.service
```

## Desktop helpers

The daemon runs sandboxed, without access to your session, so anything that
touches the desktop goes through small helpers running as you. Copy and enable
them as your desktop user (not with sudo):

```sh
make install-user-session USER_HOME="$HOME"        # from a checkout
# or, with the Arch package: tiny-dfr-deck-user-setup install
systemctl --user daemon-reload
systemctl --user enable --now touchbar-capture dfr-volume dfr-brightness dfr-now-playing dfr-viz
```

This copies the helpers to `~/.local/bin` and their units to
`~/.config/systemd/user`. It refuses to overwrite existing files and never
starts anything by itself.

| Helper | Powers | Needs |
| --- | --- | --- |
| `touchbar-capture` | Screenshot and Settings keys | Python 3.11+, `systemd-run`; Omarchy for screenshots; `notify-send` recommended |
| `dfr-volume` | volume slider | `pactl` (PipeWire PulseAudio compatibility), `omarchy-audio-output-sink` |
| `dfr-brightness` | brightness slider | `brightnessctl` |
| `dfr-now-playing` | now-playing text | `python-dbus`, an MPRIS player |
| `dfr-viz` | audio visualizer | `pw-cat`, `pactl` |

Without a helper, the deck still works: that control fails quietly and logs
to the daemon's journal. Each helper only accepts requests from the daemon's
`tiny-dfr` account and runs a fixed set of actions; see its README under
`integrations/` for the protocol.

**Voice dictation** is optional and separate: it uploads microphone audio to
Groq for transcription. Read [its guide](integrations/voice/README.md) before
setting it up, then switch to the `multibar-voice.toml` deck.

The daemon talks to the helpers of whoever is logged in on the seat. On a
machine with several users, pin one with `sudo systemctl edit tiny-dfr.service`:

```ini
[Service]
Environment=TINY_DFR_DESKTOP_UID=1000
```

## Settings: Save and Apply

Open **Touch Bar Settings** from the app menu, or with the More → Settings key.
Don't run it with sudo. **Save** writes your draft to
`~/.config/dfr-deck/config.toml`; it doesn't change the running bar. To
**apply** a saved draft:

```sh
draft="${XDG_CONFIG_HOME:-$HOME/.config}/dfr-deck/config.toml"
tiny-dfr-deck check "$draft" && \
  sudo cp -a /etc/tiny-dfr/config.toml /etc/tiny-dfr/config.toml.before-settings && \
  sudo install -m644 "$draft" /etc/tiny-dfr/config.toml
```

The daemon picks up valid changes on its own; an invalid file is rejected and
the last good configuration stays. To undo, copy `config.toml.before-settings`
back. You can also edit `/etc/tiny-dfr/config.toml` directly and check it with
`tiny-dfr-deck check /etc/tiny-dfr/config.toml`.

Whoever can write `/etc/tiny-dfr/config.toml` decides which commands the bar's
keys run (as the `tiny-dfr` account, which can read input devices), so keep
that file root-owned.

## Check that it works

```sh
systemctl status tiny-dfr.service
journalctl -u tiny-dfr.service -b
systemctl --user status touchbar-capture dfr-volume
```

The journal shows one line for each missing helper and for any config
problem. If the Settings key does nothing, check
`journalctl --user -u touchbar-capture -b` and make sure your session
environment (`WAYLAND_DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`) reaches the user
manager; Omarchy does this by default. If the daemon logs that it's running as
`nobody`, the account is missing: run `sudo systemd-sysusers` and restart it.

## Upgrade

Pull, rebuild and reinstall; your configuration, theme choice and drafts are
kept:

```sh
git pull && make build CARGO_JOBS=4 && sudo make install
sudo systemctl restart tiny-dfr.service
```

Helpers are never overwritten, so to update them run
`make uninstall-user-session USER_HOME="$HOME"` first, then install them again
and restart them (`systemctl --user restart touchbar-capture dfr-volume
dfr-brightness dfr-now-playing dfr-viz`).

**Upgrading to the release with the `tiny-dfr` account:** update your helpers
as above. Older helpers only accept requests from `nobody`, so until they're
updated the sliders, now playing, visualizer and Screenshot/Settings keys
don't respond.

**From an older install:**

- *Manual `/usr/local/libexec/tiny-dfr-multibar` + `multibar.conf` setup:*
  `sudo make install` replaces both in place and moves theme state to the new
  account. Nothing else to do.
- *Older `sudo make install` that replaced `/usr/bin/tiny-dfr`:* restore the
  package's files with `sudo pacman -S tiny-dfr`, remove the old copies
  (`sudo rm /etc/systemd/system/tiny-dfr.service
  /etc/udev/rules.d/99-touchbar-seat.rules /etc/udev/rules.d/99-touchbar-tiny-dfr.rules
  /usr/local/bin/dfr-configurator`), then install as above.

## Uninstall

```sh
sudo make uninstall && sudo systemctl daemon-reload && sudo systemctl restart tiny-dfr.service
make uninstall-user-session USER_HOME="$HOME"
systemctl --user daemon-reload
```

Stock tiny-dfr takes over again. `/etc/tiny-dfr/config.toml`, the theme state
in `/var/lib/tiny-dfr` and the `tiny-dfr` account are left in place; remove
them by hand if you're done with the deck. With the Arch package, run
`tiny-dfr-deck-user-setup uninstall` first, then `sudo pacman -R tiny-dfr-deck`.
