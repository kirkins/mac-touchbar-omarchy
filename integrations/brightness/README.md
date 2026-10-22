# Touch Bar display brightness

`dfr-brightness` is a narrow desktop-user receiver for the built-in display.
It accepts only `get` and `set 1..100` over its own abstract AF_UNIX socket
`dfr-brightness-<uid>`. Requests cannot supply device names, paths or commands.
The daemon verifies the receiver runs as the seat's desktop user; the receiver
permits that user and the daemon's `tiny-dfr` account (`nobody` on installs without it), matching the audio boundary. It expects the
built-in `apple-panel-bl` display; adapt it before using different hardware.

The existing keyboard path uses `omarchy-hw-display` to select `apple-panel-bl`
and `brightnessctl` with linear percentages. Its usable minimum is 1% and its
maximum is 100%. On the observed 509-level panel, 1% selects raw level 5.
This receiver pins that display and preserves the range, leaving the separate
Touch Bar (`228600000.dsi.0`) and keyboard backlight untouched. Each write is
followed by a fresh `brightnessctl -m info` call: set's output echoes its target
and is not confirmed hardware state. The read uses sysfs `brightness`, like the
working keyboard helper; `actual_brightness` can differ. External zero is
reported honestly on reads, but the slider never requests display-off.

The installed `brightnessctl` has systemd support and uses the existing
active desktop session's logind `SetBrightness` interface when sysfs is not
writable. The receiver runs as the desktop user with `NoNewPrivileges=yes`.
No root service, group, udev, polkit or sandbox changes are required by this
implementation. Its ability to write from the installed user service remains
an approved deployment check; a failure must be reported with its exact error
before considering any policy change. Do not replace or reconfigure the audio,
dictation or keyboard helpers.

After local landing approval, install the checked script as
`~/.local/bin/dfr-brightness` (0755) and the unit as
`~/.config/systemd/user/dfr-brightness.service` (0644), then enable it for the
graphical session. Install the checked Touch Bar binary and replace only the
two home brightness key actions with one `BrightnessSlider` action next to
`VolumeSlider`, retaining their combined stretch and unrelated settings.
Preserve backups of the installed binary and configuration, and remove/disable
only this added user receiver when rolling back. These instructions do not
authorize live installation during development or validation.

Run `python3 -B -m unittest discover -s integrations/brightness -p test_brightness.py`
for fake hardware/protocol checks. Tests inject every command dependency,
bind nonce task-specific sockets, verify the bound address before connecting,
and close them after each test. Rust transport tests use the same isolation.
They never start the production receiver or write real backlight/mixer state.
