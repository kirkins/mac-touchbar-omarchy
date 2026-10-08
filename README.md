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

For an offline visual check of the shipped named bars, the voice-dispatch test
can also export actual Cairo frames without opening the Touch Bar or running
the voice commands:

```sh
DFR_DECK_REVIEW_OUTPUT=/tmp/dfr-deck-review CARGO_BUILD_JOBS=1 \
    cargo test --locked approved_multibar_preserves_voice_dispatch_and_renders_offline -- --test-threads=1
```

Inspect `implemented-home.png` and `implemented-recording.png` in that directory.
These frames use the configured font and icon search paths; their battery and
clock display is illustrative and they do not prove physical touch or dictation.

The unit tests run without a Touch Bar, DRM device, or virtual keyboard. They
cover configuration compatibility, navigation, Fn behavior, reloads, emitted key
presses/releases, battery widget cycling, the transient volume slider (summon
on volume press, drag position-to-level math, auto-collapse timing, live level
rendering, mute interplay, and layer isolation), command buttons (parse and
combined layer dispatch, detached execution, and the one-invocation-per-command
cap), and the virtual pet (pet-layer wiring and button toggle, wander bounds,
pause and resume across layer switches, nap gating and placement, satiety decay
and feeding, startle, tap consumption, and reload transitions). Physical
layout, touch accuracy, brightness, media-key delivery to the desktop, live
battery/clock refresh, the volume slider's on-glass drag feel, command buttons'
interaction with a real helper, and the pet's on-glass charm still need a device
test; battery checks there must include repeated tap-to-cycle, device labels in
each display mode, hotplug, and timed return.

## Named bars

The daemon merges `/etc/tiny-dfr/config.toml` over
`/usr/share/tiny-dfr/config.toml`. The optional
[multibar example](share/tiny-dfr/multibar.toml) defines a home bar with volume,
brightness, play/pause, battery, and clock controls, buttons opening F1-F12 and
media bars, and a mic button that starts voice dictation on a recording layer.

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
`"cyan"`, `"amber"`, `"violet"`, `"red"`, or `"green"`. For example,
`{ Text = "Home", Accent = "violet", Action = { Layer = "home" } }`.
Battery charging and low-charge colors take precedence over this override.
The transient volume slider uses the same cyan, with a subdued muted fill.

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

### Transient volume slider

On the home bar, tapping a volume button (`VolumeUp`, `VolumeDown`, or `Mute`)
summons a macOS-style slider that expands over the contiguous volume cluster:
a slim track showing the live level, which collapses back to the normal layout
about two and a half seconds after the last interaction. The summoning press
steps the level; once the slider is drawn it owns the whole segment, so the
next touch lands on it and jumps to the touched position instead of pressing
the covered buttons. A dimmed fill shows the muted state, and any volume
change or drag clears it, matching how the desktop unmutes.

The slider is draggable from the first touch: a speaker glyph at its left end
is the mute affordance - tapping it toggles mute, and dragging from it still
drags - and touching anywhere on the slider sets the volume to the touched
position. A finger that began as the summoning press converts to the same
drag once it moves. Dragging sets
volume through the same uinput key events the volume buttons use, one tap per
step. The daemon runs unprivileged and starts no external services, so it
cannot read the desktop mixer directly; it tracks the level driven through its
own key events from a midpoint estimate, and volume changes made outside the
Touch Bar (keyboard keys, desktop mixer) are not reflected until the deck
re-drives them. Layer navigation, Fn (including the legacy home-swap double
press), reloads, and input release collapse the slider and restore the bar.
Only the home bar summons it; volume buttons on other layers behave exactly as
before.

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
state for the ongoing recording - no dynamic widget is needed. That layer
holds a `Stop` button (runs `voice-dictate-user stop`, which stops recording and
types the transcript into whatever window has focus) and a `Cancel` button
(runs `voice-dictate-user cancel`, discarding a mis-start); both return home. The
mic button has a bright cyan `Mic` label. A persistent red dot and `Recording`
label distinguish the recording layer, with a prominent red Stop and quieter
Cancel. This indicates the selected layer; it does not confirm microphone
signal, helper startup, or transcription success. The optional private
[desktop-session integration](integrations/voice/README.md) supplies a client
and recorder service. It crosses the daemon's unprivileged AF_UNIX boundary
without changing its privilege drop or network restrictions. Install that
integration separately; without a receiver these presses still switch layers
and log the failed command. Check the desktop notification or receiver journal
to confirm recording started.

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
