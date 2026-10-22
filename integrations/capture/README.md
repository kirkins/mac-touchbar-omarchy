# Touch Bar Settings and screenshot session receiver

This receiver is **required for the consumer full deck's Home → Settings button**.
It is not required to open Settings from the application menu or a graphical
terminal. Use the [complete installation guide](../../INSTALL.md): system
installation and ordinary-user session setup are distinct, copy-only steps;
only the operator subsequently enables the user unit.

## Requirements and setup

The supplied route needs the seat's graphical desktop user, the daemon's
`tiny-dfr` account, Python 3.11+, a systemd user manager / `systemd-run`, and the installed
native GTK4/libadwaita app and runtime libraries. `notify-send` is recommended
for bounded failure notifications. Settings does not require Omarchy or audio.
Screenshot separately requires `omarchy capture screenshot` and its `grim`,
`slurp`, `hyprpicker`, `wl-copy`, `hyprctl` session flow.

After reviewing/staging/building and system installation per INSTALL.md, run
from the reviewed checkout **as the ordinary desktop user, never sudo**:

```sh
make install-user-session USER_HOME="$HOME"
systemctl --user daemon-reload
systemctl --user enable --now touchbar-capture.service
systemctl --user status touchbar-capture.service
```

For copy-only staging, add `DESTDIR="$dfr_stage"` to the make command. `USER_HOME`
is explicit, absolute and normalized; no home is guessed from sudo. The target
copies just `%h/.local/bin/touchbar-capture` and
`%h/.config/systemd/user/touchbar-capture.service`, refuses preexisting files or
dangling symlinks before copying, and never enables/starts a service or touches
a draft. An XDG override for the Settings draft does not relocate these unit
paths. On upgrade, stop only this receiver, preserve/review the two old files,
move them aside deliberately before replacement, reload the user manager and
start the receiver. Preserve backups for rollback.

The graphical user-manager environment must include `WAYLAND_DISPLAY` (or
`DISPLAY`), `XDG_RUNTIME_DIR` and `DBUS_SESSION_BUS_ADDRESS`; Screenshot also needs
the compositor's session environment. Use the desktop's normal import/re-login
mechanism and restart this receiver after environment changes. Other desktop
UIDs require coordinated source adaptation and qualification, not just a unit edit.

## Fixed actions and security boundary

The [consumer deck](../../share/tiny-dfr/multibar.toml) uses:

```toml
# Home
{ Text = "Settings", Action = { Command = "touchbar-capture-user settings" } }
# More
{ Text = "Screenshot", Action = { Command = "touchbar-capture-user screenshot", Layer = "home" } }
```

The machine client is installed by `make install`. Commands run inside the
daemon's sandbox: its privilege drop, `ProtectHome`, `PrivateTmp`,
`NoNewPrivileges`, and restricted address families prevent direct compositor /
user-manager access. Do not loosen them. A Linux abstract AF_UNIX socket named
`dfr-capture-<desktop UID>` crosses that boundary without making the desktop's
0700 runtime directory public.

The receiver authenticates `SO_PEERCRED` against its own UID and the daemon's `tiny-dfr` account (`nobody` on installs without it), and
the client verifies the receiver UID. Other processes with either allowed UID
can invoke these two fixed actions; this is the explicit local trust boundary.
Only `screenshot` and `settings` are accepted; wire bytes are never executed as
commands or arguments. Settings selects exactly:

```text
systemd-run --user --collect --quiet --property=Type=exec <dfr-configurator on PATH>
```

`Type=exec` catches executable-launch failures, **not GTK readiness or a mapped
window**. Repeated launches ask the same native application to present its
existing window. Settings saves a user draft; administrator Apply is a separate
terminal operation described in INSTALL.md. No GUI elevation/policy is supplied.

Screenshot launches the fixed `omarchy capture screenshot` detached; a second
capture while one is active is refused. An immediate nonzero launcher exit is
reported. Neither its submission nor an exit code is proof of a completed image.

## Errors and fallback

- Missing receiver: the client exits nonzero; the daemon logs fixed setup and
  app-menu fallback guidance in `journalctl -u tiny-dfr.service -b`. The sandbox
  cannot notify the desktop by itself. No on-glass launch confirmation is claimed.
- Missing app, launch failure or missing graphical environment: the receiver
  returns an error and attempts a bounded notification. Read
  `journalctl --user -u touchbar-capture.service -b` and `journalctl --user -b`
  for transient app/runtime/display failures. If no window appears, open **Touch
  Bar Settings** from the application menu or run
  `dfr-configurator` in the ordinary user's graphical terminal.
- Unreadable/invalid base or draft: the native app shows a safe, bounded error
  window when GTK can initialize, otherwise session-journal guidance. It changes
  no files and does not quote configuration contents. Back up/repair the file;
  run `tiny-dfr-deck check --base BASE DRAFT` in a private terminal for detailed
  diagnostics. Never run the GUI with sudo.

There is no success notification pretending a submitted launch mapped. The
operator must confirm the window. Without a receiver, navigation remains usable
and Screenshot still returns Home, but its command has failed.

## Safe regression checks

```sh
python3 -B -m unittest discover -s integrations/capture -p 'test_*.py' -v
```

Tests use fake launchers, notifications/preflight and unique nonproduction
AF_UNIX sockets. They cover fixed argv, repeated submissions, missing app/session/
receiver, launcher exit/OS-error/timeout feedback, bounded failure text, action
allowlisting, busy rejection and kernel peer-UID checks. They never capture a
screen, open a native app, touch a production socket or send a real notification.
These are not physical Touch Bar/namespace/fresh-install/display qualification.
A device owner can separately verify Home → Settings and More → Screenshot only
after setup and explicit approval for that live desktop test.

## Rollback

Stop/disable only the user unit you added. Restore the two retained user files
(or remove only newly added files), reload the user manager, and restore the
previous deck overlay if it was changed. The shared machine capture client and
native app may still be needed by other configuration; do not blindly remove
them. Retain user drafts, backups and theme state. The application menu remains
the Settings fallback when the Touch Bar receiver is disabled.
