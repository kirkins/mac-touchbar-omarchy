# Install mac-touchbar-omarchy (tiny-dfr)

This is the supported source installation procedure. It installs the native
GTK4/libadwaita Settings app as well as the daemon. It does not install drivers,
change kernel/boot configuration, or configure cloud voice. Commands below are
for the operator of the target machine; development tests use staging and mocks.

## Scope and prerequisites

**Supported:** Touch Bar MacBook Pro 13-inch on Apple Silicon - `MacBookPro17,1`
(M1) and `Mac14,7` (M2) - in an Omarchy Wayland session with a systemd user
manager and PipeWire, **desktop UID 1000** and daemon user `nobody` (65534).
Everything else is unsupported: Intel/T2 Macs, other Apple Silicon models,
other distributions and desktops, other user IDs, and the experimental Nix
flake. Only `MacBookPro17,1` has been exercised on hardware; `Mac14,7` is named
by the shipped rules but is not yet device-qualified.

The daemon needs Linux Touch Bar DRM, touch-input, backlight and uinput support
already provided by the distribution. The inherited udev rules name
`MacBookPro17,1` and `Mac14,7`, the `adp`/`appletbdrm` DRM drivers, the
`228200000.display-pipe.0`/`228600000.dsi.0`/`appletb_backlight` Touch Bar
backlight and the `apple-panel-bl` display backlight. They also retain Intel/T2
identifiers (`gmux_backlight`, `intel_backlight`, `acpi_video0`); their presence
is not a qualification of Intel/T2 hardware. There is no fresh device or
all-distribution Asahi acceptance claim here.

The supplied desktop integrations pin **desktop UID 1000** and the daemon's
`nobody` UID 65534. Receivers derive their own socket UID but the daemon and
clients pin 1000. A different user UID requires coordinated source changes and
qualification; editing only a service is insufficient. Screenshot needs Omarchy;
the lock key emits its `SUPER+CTRL+L` binding. The Settings application itself
is a generic native app; its Touch Bar launch needs the capture receiver and the
graphical user manager. Other desktops and hardware are adaptation work, outside
the supplied full-deck qualification.

The system daemon opens DRM, Touch Bar input/backlight and `/dev/uinput`, then
drops to `nobody` with supplementary **input** and **video** groups. Those groups
and the nobody account and **nobody group** must already exist (the stock unit
uses both names to prepare theme state); the distribution's device/logind
permissions must allow this existing model. Do not chmod devices, home/runtime
directories or configuration world-writable to fix a permission error. The
udev `SYSTEMD_ALIAS` entries create systemd device aliases, not necessarily
filesystem symlinks named `/dev/tiny_dfr_*`.

Build requirements: a Rust/Cargo toolchain compatible with `Cargo.lock`, a C
compiler/linker, make, pkg-config, and development libraries for Cairo,
libinput, FreeType, fontconfig, librsvg **2.59+**, GLib/GIO, Pango, GDK-Pixbuf,
libdrm and libudev. A fontconfig-resolvable font (the example uses `:bold`)
is needed for configuration validation/rendering. Native Settings additionally
requires **GTK 4.10+ and libadwaita 1.5+**, at build and run time. Package names
vary; on Arch the libraries are `cairo libinput freetype2 fontconfig librsvg
glib2 pango gdk-pixbuf2 libdrm systemd-libs gtk4 libadwaita`, with `base-devel`
and Rust/Cargo. On Fedora the GUI development packages are `gtk4-devel
libadwaita-devel`; on Debian/Ubuntu `libgtk-4-dev libadwaita-1-dev`. Verify the
minimum versions; an older distribution package is not sufficient.

Nix/`flake.nix` is **experimental and unsupported** for this workspace/native
Settings install: its legacy version, asset substitutions and GUI dependencies
have not been qualified. Use the procedure here rather than `nix build`.

## Build and stage without changing the machine

From the repository root, as an ordinary user:

```sh
pkg-config --modversion gtk4 libadwaita-1 librsvg-2.0
cargo build --locked --workspace --release -j1
dfr_stage=$(mktemp -d)
make install DESTDIR="$dfr_stage"
target/release/tiny-dfr check --base "$dfr_stage/usr/share/tiny-dfr/config.toml" \
    --icon-root "$dfr_stage/usr/share/tiny-dfr" "$dfr_stage/usr/share/tiny-dfr/multibar.toml"
desktop-file-validate "$dfr_stage/usr/local/share/applications/org.dfrdeck.Configurator.desktop"
```

`make install` copies files only: no build, service start, audio, cloud request,
credential setup or personal config overwrite. `DESTDIR` changes the staging
root, not compiled runtime paths. Keep the stage for an installation manifest
and uninstall reference. The installed files are `/usr/bin/tiny-dfr`,
`/usr/local/bin/dfr-configurator`, the capture client, base/example TOML and all
inherited SVGs under `/usr/share/tiny-dfr`, **both** seat and trigger udev rules,
the stock system unit, desktop entry, both licenses and this guide/README plus
relative integration/privacy guides and example configurations. Optional voice
is an example only; no voice client, receiver or credentials are installed. The base file is required;
an `/etc` overlay cannot replace it. A daemon-only `cargo build -p tiny-dfr`
is possible but does not provide the complete Settings route described here.

To refresh only the installed documentation after a docs change, without
reinstalling binaries or touching `/etc`, run `sudo make install-docs`. It copies
the same guide, README, licenses, assets, integration guides and example
configurations below `/usr/share/doc/tiny-dfr`; `DESTDIR` stages it like `install`.

The ordinary-user Settings receiver has a **separate, explicit copy-only** target.
Stage it as the ordinary user (not sudo), choosing an absolute, normalized home
path; `DESTDIR` prefixes that path without changing the unit's `%h` runtime paths:

```sh
make install-user-session DESTDIR="$dfr_stage" USER_HOME="$HOME"
```

It copies only `.local/bin/touchbar-capture` (0755) and
`.config/systemd/user/touchbar-capture.service` (0644), owned by the invoking
ordinary user. It refuses existing helpers/units, including dangling symlinks,
before copying either file. It neither creates a draft nor enables/starts any
unit. Never run this target with sudo or infer another user's home from sudo.

## First installation

If any destination in the stage is already installed or package-managed, use
the upgrade section instead. Review the stage and the supplied udev/unit first.
Check accounts/groups with `getent passwd nobody` and `getent group nobody input video`.
As the machine administrator, from the same reviewed checkout:

```sh
sudo make install
# Deliberately select the full named deck; the base alone is the legacy layout.
sudo install -Dm644 /usr/share/tiny-dfr/multibar.toml /etc/tiny-dfr/config.toml
/usr/bin/tiny-dfr check /etc/tiny-dfr/config.toml
sudo systemctl daemon-reload
sudo udevadm control --reload
```

The stock unit prepares `/var/lib/tiny-dfr` owned by `nobody` (0755) and allows
writes only there through `ReadWritePaths`; `ProtectSystem=strict`, `ProtectHome`,
`PrivateTmp`, `NoNewPrivileges` and its other restrictions remain enabled.
This directory persists on-glass theme choice. No writable `/etc` is required.

**Complete the required graphical-user receiver setup below** for the full deck's
Home → Settings button. System installation alone does not supply an active
session receiver; the application-menu launcher remains the fallback.

The seat rule assigns Touch Bar devices to `seat-touchbar` and removes DRM
master-of-seat tagging so the daemon, not the desktop compositor, can own the
Touch Bar. On upgrades, check for a distribution-owned rule/collision before
replacement. The two tracked empty `systemd-backlight@backlight:*.service` files
are historical mask placeholders with no verified need in this install; they
are deliberately **not installed**. Backlight restore/mask behavior on a clean
boot remains a target-device qualification gate, not a reason to install global
masks or change device permissions blindly.

New rules need a device add event. Reboot at a suitable time after installation
so the already connected devices are discovered with the new rules. The udev
rules request `tiny-dfr.service`; the unit has no `[Install]` section and is
not enabled with `systemctl enable`. Once the systemd device aliases are present,
`sudo systemctl start tiny-dfr.service` can start it explicitly. Inspect
`systemctl status tiny-dfr.service dev-tiny_dfr_display.device
dev-tiny_dfr_backlight.device` and `journalctl -u tiny-dfr.service -b`.
Missing driver/device aliases or groups are prerequisites to resolve with the
distribution; this guide does not install a kernel or relax the sandbox.

## Native Settings, Save and Apply

Launch **Touch Bar Settings** from the desktop application menu, or run
`/usr/local/bin/dfr-configurator` as the desktop user. Never run the GUI with
sudo. The default draft is `$XDG_CONFIG_HOME/dfr-deck/config.toml` when that
variable is absolute, otherwise `$HOME/.config/dfr-deck/config.toml`. Existing
drafts are preserved. With no draft, Settings loads the current `/etc` overlay,
or the installed `multibar.toml` example if no overlay exists, into memory.
Opening does not write; Save/Ctrl+S creates the user-owned directory/file and
later saves atomically with a first-save backup. Invalid drafts cannot be saved.

**Save writes the draft. Apply installs that draft for the daemon.** Save alone
does not change the running Touch Bar. The persistent Save/Apply banner,
explicit Apply Instructions action, Configuration File menu and saved-draft toast
explain this contract without interrupting each save with a dialog. The action
points to the terminal steps. Explicit `--config PATH` edits that file directly;
use it only with a writable file. `/etc` remains administrator-owned.

After saving, run this separate handoff in a terminal as the desktop user:

```sh
dfr_config_home=${XDG_CONFIG_HOME:-"$HOME/.config"}
# If XDG_CONFIG_HOME is relative, use $HOME/.config as Settings does.
case "$dfr_config_home" in /*) ;; *) dfr_config_home="$HOME/.config" ;; esac
dfr_draft="$dfr_config_home/dfr-deck/config.toml"
/usr/bin/tiny-dfr check "$dfr_draft" && \
    sudo cp -a /etc/tiny-dfr/config.toml /etc/tiny-dfr/config.toml.before-settings && \
    sudo install -m644 "$dfr_draft" /etc/tiny-dfr/config.toml
```

Retain an earlier `config.toml.before-settings` under a different backup name
before applying again. The daemon watches its configuration and reloads valid
changes; confirm the result in its journal. If it needs an explicit reload,
`sudo systemctl restart tiny-dfr.service`. To undo, reinstall the retained
overlay with `sudo install -m644 /etc/tiny-dfr/config.toml.before-settings
/etc/tiny-dfr/config.toml`. For a manual first setup with no installed overlay,
skip the backup copy and use `sudo install -Dm644` after validation. There is
no privileged apply helper, sudo policy or home/runtime permission change.

## Required full-deck Settings receiver (ordinary graphical UID 1000)

This step is **part of the full-deck installation**, not optional for its Settings
button. Python 3.11+, `systemd-run`, a systemd user manager in the graphical
UID1000 session, and the installed native app/runtime libraries are required.
`notify-send` is recommended for session-side failure notifications (journal
feedback remains if it is absent). From the same reviewed checkout, in that
user's graphical terminal, without sudo:

```sh
id -u # must be 1000 for the supplied daemon/client route
make install-user-session USER_HOME="$HOME"
systemctl --user daemon-reload
systemctl --user enable --now touchbar-capture.service
```

The receiver launches the same native app/default draft as the desktop entry.
Its user-manager environment must contain `WAYLAND_DISPLAY` (or `DISPLAY`),
`XDG_RUNTIME_DIR`, `DBUS_SESSION_BUS_ADDRESS` and, for Omarchy screenshot, the
compositor's session environment. Use the desktop's normal session-environment
import/re-login mechanism; do not grant the daemon access to the user's home,
runtime directory or bus. Inspect `systemctl --user status touchbar-capture.service`
and `journalctl --user -u touchbar-capture.service -b`. After a deliberate setup,
`/usr/local/bin/touchbar-capture-user settings` submits the same fixed app launch
as the Home button. **Submission/exec success is not proof a window mapped.**
Confirm the window yourself; GTK/display/runtime failures appear in
`journalctl --user -b`. Repeated launches present the existing native window. Screenshot additionally needs Omarchy's `grim`, `slurp`, `hyprpicker`,
`wl-copy`, `hyprctl` flow. Settings needs neither a screenshot nor Omarchy itself.

### If Settings does not open

- Missing receiver: the sandboxed daemon cannot display a desktop notification.
  Check `journalctl -u tiny-dfr.service -b` for handoff/setup guidance, complete
  the user setup above, or open **Touch Bar Settings** from the application menu.
  A button press does not claim on-glass launch success.
- Missing app/launcher or missing session environment: the receiver returns an
  error and attempts a bounded desktop notification. Check its journal and the
  installed `/usr/local/bin/dfr-configurator`, runtime libraries and graphical
  user-manager environment; re-login/restart only that user receiver as needed.
- Invalid/unreadable base or draft: Settings shows a bounded native error window
  when GTK can initialize; otherwise it writes safe launch guidance to stderr /
  the session journal. It does not dump configuration contents or change files.
  Use `tiny-dfr check --base BASE DRAFT` in a **private terminal** for detailed
  parser diagnostics (these may quote the file); back up/repair the draft rather
  than deleting it. Verify installed base/example files using the retained stage.

The user install target refuses replacements. For an upgrade, back up both user
files, stop only this receiver, deliberately move the prior files aside, copy
the reviewed replacements as that same user, reload the user manager and start
it again; preserve the backups for rollback. Do not overwrite a personal unit
or a distribution file without reviewing it.

## Other optional desktop receivers

Each other integration is separately optional:

| Directory / receiver | Prerequisites and effect |
| --- | --- |
| `audio` / `dfr-volume` | Python 3, `pactl`, PipeWire PulseAudio compatibility, `omarchy-audio-output-sink`; reads/writes output volume |
| `brightness` / `dfr-brightness` | Python 3, `brightnessctl`, existing logind permissions and **apple-panel-bl** display; reads/writes display brightness |
| `viz` / `dfr-viz` | Python 3, `pw-cat`, `pactl`, PipeWire; captures output-monitor peaks only while the visualizer is enabled |
| `now-playing` / `dfr-now-playing` | Python 3 with `dbus-python` (Arch `python-dbus`), session bus and MPRIS player; reads title/artist |
| `voice` / `voice-dictate` | Python 3.11+, `pw-cat`, `pactl`, `stdbuf`, `curl`, `wtype`, `notify-send`, deliberate standalone Groq credential setup; see its [privacy/setup guide](integrations/voice/README.md) before enabling |

For a chosen non-voice receiver, substitute the directory/script from the table:

```sh
dfr_integration=audio
dfr_receiver=dfr-volume
install -Dm755 "integrations/$dfr_integration/$dfr_receiver" "$HOME/.local/bin/$dfr_receiver"
install -Dm644 "integrations/$dfr_integration/$dfr_receiver.service" "$HOME/.config/systemd/user/$dfr_receiver.service"
systemctl --user daemon-reload
systemctl --user enable --now "$dfr_receiver.service"
```

User units deliberately use `%h/.config/systemd/user`/`%h/.local/bin` conventions;
an XDG override for the Settings draft does not relocate these helper binaries.
Read each integration README for its protocol/rollback. No receiver is started
by `make install`. Without receivers, deck navigation still works, slider/quick
key requests fail visibly/in the journal, now-playing is idle and an enabled
visualizer reports `No feed`. Voice is unnecessary for Settings or the deck. The consumer `multibar.toml`
contains no voice commands or recording layer. Only after reading the installed
[voice privacy/setup guide](integrations/voice/README.md), deliberately configuring
its separate receiver and backing up the existing overlay, select
`/usr/share/tiny-dfr/multibar-voice.toml` instead if desired. Validate it before
administrator Apply. This optional profile restores Mic/recording controls and
keeps Settings in More. Never enable it just to test or open Settings.

## Safe checks and optional device smoke

Offline: use the staged `tiny-dfr check` above, then validate the installed
overlay and any draft before applying. Executable regression tests use dummy
credentials, synthetic audio, fake external tools and unique sockets:

```sh
python3 -B -m unittest discover -s tests -v
python3 -B -m unittest discover -s integrations/voice -v
python3 -B -m unittest discover -s integrations/capture -v
cargo test --locked --workspace -j1 -- --test-threads=1
```

These checks do not qualify live hardware, microphone/cloud behavior or desktop
text insertion. A device owner can separately check Home → Functions → Home,
Home → Media → Home, Fn press/release and key release across layer/config changes.
With default `PetEnabled=true`, use **Home → More → Pet → Home**; a custom deck
without More gets the Pet opener on Home unless it supplies its own opener.
Open native Settings, Save a draft, apply it separately, and check the journal.
Screenshot/Lock/volume/brightness tests need their stated prerequisites.

Theme buttons are **opt-in**: with `ThemesEnabled=true`, add a More button
with `Action = "CycleTheme"` and/or a button with `Action = { Layer = "themes" }`
in Settings, then Save and Apply. Name those buttons Theme and Themes respectively.
Only then test More → Theme and/or More → Themes → a name → Home. With **Remember Last Touch Bar Choice**
(`ActiveTheme` absent), the on-glass selection persists through restart in
`/var/lib/tiny-dfr/active-theme`. **Use a Fixed Theme**/Switch to Theme saves
`ActiveTheme` in the draft, and the fixed name wins on startup/reload after Apply;
on-glass changes can still occur in the session. `ThemesEnabled=false` disables
theme application; selecting a startup theme then has no effect until enabled.
Voice device/cloud tests require separate deliberate opt-in: Stop uploads audio
and inserts returned text into the focused window; Cancel uploads/inserts nothing.
Use a disposable focused field and read the voice disclosure first.

## Upgrade, rollback and uninstall

An existing/package-managed installation is an **upgrade**, not first install.
Keep the previous source/package and one administrator-owned backup of every
destination you will replace, the `/etc/tiny-dfr` overlay/custom icons, and any
`tiny-dfr.service.d` drop-ins. Do not overwrite distro-owned files behind its
package manager: use its packaging/rollback mechanism or resolve the collision
first. Stage and validate the new binaries/assets as above. Stop only
`tiny-dfr.service` during replacement, install the reviewed files, preserve the
existing `/etc` overlay (the install target never overwrites it), check it with
the new daemon, reload units/rules, then start that service. A legacy
`multibar.conf` ExecStart override still selects `/usr/local/libexec/tiny-dfr-multibar`:
back it up and remove that specific override if adopting `/usr/bin/tiny-dfr`.
Preserve unrelated drop-ins. Sync a stale Settings draft deliberately using
`--config` or by backing it up before reopening; upgrades do not overwrite drafts.

To roll an upgrade back, stop the daemon, restore the previous destination files,
overlay and drop-ins (remove only newly added files using the retained stage),
reload the system manager/udev rules, then start the previous daemon. Retain theme
state and user drafts for rollback. Stop/disable only optional user units you
added and restore their prior files; reload the user manager.

To uninstall a first install, first stop the daemon and any added user receivers.
Using the retained stage as the exact file list, remove only the files installed
by that stage from their corresponding absolute locations. Remove the selected
`/etc/tiny-dfr/config.toml` only if it belongs to this install and has been backed
up. Remove only your added user units/helpers and reload the relevant managers
and udev rules. Empty installation directories can be removed with `rmdir`;
keep custom icons, drafts, backups and `/var/lib/tiny-dfr/active-theme` unless
you deliberately choose to discard them. No kernel/boot uninstall is involved.
