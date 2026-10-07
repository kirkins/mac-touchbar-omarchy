# tiny-dfr
The most basic dynamic function row daemon possible


## Dependencies
cairo, libinput, freetype, fontconfig, librsvg 2.59 or later, uinput enabled in kernel config

## Build and check

Install Rust/Cargo, pkg-config, and the development files for the dependencies
above, GLib/GIO, libdrm, and libudev. Use a Rust toolchain compatible with
`Cargo.lock`. On machines with limited memory, keep one build job and run checks
sequentially:

```sh
cargo fmt --all -- --check
CARGO_BUILD_JOBS=1 cargo test --locked -- --test-threads=1
CARGO_BUILD_JOBS=1 cargo clippy --locked --all-targets
CARGO_BUILD_JOBS=1 cargo build --locked --release
```

The unit tests run without a Touch Bar, DRM device, or virtual keyboard. They
cover configuration compatibility, navigation, Fn behavior, reloads, emitted key
presses/releases, battery widget cycling, and the virtual pet (pet-layer wiring
and button toggle, wander bounds, pause and resume across layer switches, nap
gating and placement, satiety decay and feeding, startle, tap consumption, and
reload transitions). Physical layout, touch accuracy, brightness, media-key
delivery to the desktop, live battery/clock refresh, and the pet's on-glass
charm still need a device test; battery checks there must include repeated
tap-to-cycle, device labels in each display mode, hotplug, and timed return.

## Named bars

The daemon merges `/etc/tiny-dfr/config.toml` over
`/usr/share/tiny-dfr/config.toml`. The optional
[multibar example](share/tiny-dfr/multibar.toml) defines a home bar with volume,
brightness, play/pause, battery, and clock controls, plus buttons opening F1-F12
and media bars.

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
navigation buttons using `Action = { Layer = "name" }`. Each sub-bar gets a Home
button automatically if it lacks a visible button targeting `DefaultLayer`.
Escape is still inserted on Macs without a physical Escape key.

Navigation buttons switch immediately on touch-down. With `FnLayer`, holding Fn
temporarily shows that bar and releasing Fn restores the previous bar. Tapping a
navigation button while Fn is held keeps the chosen bar open. Omitting `FnLayer`
leaves named bars entirely under on-bar navigation. `MediaLayerDefault` and
`DoublePressSwitchLayers` apply to legacy configurations only. Existing
`PrimaryLayerKeys`/`MediaLayerKeys` configurations retain their Fn and double-Fn
behavior, and `Action = "F1"` or `Action = ["LeftCtrl", "F1"]` still sends keys.
Clock, battery, icons, spacers, and `Stretch` work in named bars as before.

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

## Touch Bar pet

A small virtual pet, in the spirit of the 2016 Touchbar Pet, lives on its own
pet layer (a behavioral homage written from scratch - no original assets or
code). With `PetEnabled = true` the home bar gains a Pet button beside the
other navigation buttons - the same styling as Functions and Media - that
opens the pet layer, a bar carrying a Home button like every other sub-bar
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
gets hungry it sits and begs, and tapping the pet itself or the bar next to
it drops a kibble it walks over and munches. A satiety value decays over
about an hour and gates the behavior: hungry pets beg and never nap,
well-fed pets nap and wander. A direct tap on the pet is always consumed
and never fires the button underneath; nearby taps are consumed only while
the pet is hungry, whether it is idling, walking, or begging.

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

## Reversible device installation after review

Branch development and offline checks do not require installation. After review
and approved landing, build on the target machine (or for its architecture) and
use the following commands for an existing `/usr/bin/tiny-dfr` systemd install.
They keep the existing system binary and select the new binary through a separate
service drop-in. The installed `/usr/share/tiny-dfr/config.toml` and icons must
already be available. Keep any desired global settings in the example before
installing it.

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
printf '[Service]\nExecStart=\nExecStart=/usr/local/libexec/tiny-dfr-multibar\n' |
    sudo tee /etc/systemd/system/tiny-dfr.service.d/multibar.conf >/dev/null
sudo systemctl daemon-reload
sudo systemctl restart tiny-dfr.service
sudo systemctl status tiny-dfr.service --no-pager
```

On the device, check Home → Functions → Home and Home → Media → Home, all
F1-F12/media controls, the battery and clock, and Home → Pet → pet
wandering, feeding, and startling → Home, plus Fn press/release. Hold a key
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
