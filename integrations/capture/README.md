# Touch Bar desktop-session actions

Install this optional receiver separately from the Touch Bar daemon.
The [complete install guide](../../INSTALL.md) installs the native Settings
binary/desktop launcher and the machine capture client. Python 3.11+ and the Omarchy capture
flow (`omarchy capture screenshot`, with `grim`, `slurp`, `hyprpicker`,
`wl-copy`, and `hyprctl`) must be available in the desktop session.

## Why the quick keys need it

The More bar's Screenshot and Settings keys are ordinary command buttons.
Command buttons run inside the daemon's sandbox: the daemon keeps its internal
privilege drop to `nobody` (`src/main.rs`), and the child inherits `ProtectHome`,
`PrivateTmp`, `NoNewPrivileges`, and the restricted address families. Running
`omarchy capture screenshot` there does not fail loudly - it silently no-ops.
Reproduced on this machine (`sudo -u nobody env -i PATH=... /bin/sh -c
'omarchy capture screenshot'`): the flow cannot resolve a screenshot
directory without a home, cannot reach `hyprctl`, and the region picker
returns no selection, so the script reaches its empty-selection
`exit 0`. The daemon logs only nonzero exits and redirects the child's
streams to `/dev/null`, so the press leaves no trace: no screenshot, no
journal line.

Importing the user's session environment does not help: with
`XDG_RUNTIME_DIR`, `WAYLAND_DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`, and
`HYPRLAND_INSTANCE_SIGNATURE` all set, the flow still fails, because
`/run/user/1000` is mode 0700 and `nobody` cannot open the Wayland or
compositor sockets inside it. The wall is kernel permissions, not
environment. `systemd-run --user --machine=<user>@.host` and `machinectl
shell` fail too: the daemon user may not connect to the desktop user's
manager bus (`Permission denied` on the machine transport) and no polkit
authentication agent exists in that context. This integration therefore uses
the same design as the voice integration: the real work runs in a tiny
receiver inside the graphical session, and the button runs a thin
unprivileged client.

## Boundary

The quick keys call `/usr/local/bin/touchbar-capture-user screenshot` or
`/usr/local/bin/touchbar-capture-user settings`. The client uses a Linux
abstract AF_UNIX socket named `dfr-capture-<desktop UID>` to reach
`touchbar-capture.service` in the desktop user's graphical session.
The abstract socket crosses the daemon's private mount and IPC namespaces
without making the desktop runtime directory public; only AF_UNIX is needed,
which the daemon already allows.

The receiver accepts exactly the named actions `screenshot` and `settings`,
authenticates `SO_PEERCRED` against the desktop UID and UID 65534 (`nobody`),
and never executes bytes from the wire. Each action selects a fixed argv on
the session side: `omarchy capture screenshot`, or
`systemd-run --user --collect --quiet /usr/local/bin/dfr-configurator`.
The native app defaults to the desktop user's XDG draft. Save writes that draft;
the explicit administrator Apply steps in the install guide update the daemon.
Settings itself does not require Omarchy or elevated GUI privileges.
There is no arbitrary command or argument channel. Other processes with
either allowed UID can invoke these two actions; this is the explicit local
trust boundary. The client checks the receiver's UID. The daemon keeps its
privilege drop, `NoNewPrivileges`, and AF_UNIX / AF_NETLINK restriction.
Neither `runuser`, a privileged launcher, nor a grant of home or runtime
directory access is involved.

The receiver launches the picker detached in its own session, replies as
soon as the flow is up (the picker can stay open for minutes), refuses a
second capture while one is running, and reports a launcher that exits
nonzero immediately (a broken session) as an error instead of
acknowledging it.

## Install

Before replacing an existing helper or adding the button, keep one rollback
copy in a private task directory outside the repository. Never commit
screenshots or copies of personal configuration.

Run these commands as the desktop user from the repository root:

First build/install the native GTK4/libadwaita app using [INSTALL.md](../../INSTALL.md).

```sh
install -Dm755 integrations/capture/touchbar-capture "$HOME/.local/bin/touchbar-capture"
install -Dm644 integrations/capture/touchbar-capture.service "$HOME/.config/systemd/user/touchbar-capture.service"
sudo install -Dm755 integrations/capture/touchbar-capture-user /usr/local/bin/touchbar-capture-user
systemctl --user daemon-reload
systemctl --user enable --now touchbar-capture.service
```

Then point the Screenshot and Settings quick keys at the client. The shipped
[`multibar.toml`](../../share/tiny-dfr/multibar.toml) already reads:

```toml
{ Text = "Screenshot", Action = { Command = "touchbar-capture-user screenshot", Layer = "home" } },
{ Text = "Settings", Action = { Command = "touchbar-capture-user settings", Layer = "home" } },
```

so after `sudo install -m644 share/tiny-dfr/multibar.toml
/etc/tiny-dfr/config.toml` (or the same edit by hand, preserving other
customizations) run `sudo systemctl restart tiny-dfr.service`. The example's
Settings key uses the `settings` verb and returns to Home after handing off.
The client targets desktop UID 1000. The receiver derives its socket name from its own
UID. Both the shipped client and daemon target UID 1000; a different desktop UID
requires coordinated source adaptation and qualification, not just editing the
receiver. The daemon remains nobody/65534.

The service starts with `graphical-session.target`; its manager environment
must already contain the current `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR`.
Re-login or restart only this service if the desktop environment changes.

## Verification

These regressions run small executable adapters, real AF_UNIX requests, and
detached fake launchers. They never open a picker, take a screenshot, or
send a notification:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s integrations/capture -p 'test_*.py' -v
systemd-analyze --user verify integrations/capture/touchbar-capture.service
```

The suite covers both fixed launcher lines, action allowlisting, busy refusal
during an ongoing capture, immediate nonzero exit reporting, the wire reject of unknown and
shell-like payloads, the `SO_PEERCRED` boundary, a session-free client
process reaching the receiver, and the client CLI's rejection of arbitrary
arguments before it connects.

First verify the receiver with the client under the actual daemon UID,
supplementary groups, session-free environment, mount namespace, and
no-new-privileges restrictions:

```sh
sudo -u nobody env -i /usr/local/bin/touchbar-capture-user screenshot
```

with the receiver's real unit temporarily stopped (it would otherwise take a
real screenshot) and a marker handler in its place if you need to avoid the
real flow. A same-user socket test alone does not prove the daemon's
namespace boundary. Then press the Touch Bar's More bar Screenshot key:
the Omarchy picker must appear, and a completed selection must land in
`~/Pictures` and the clipboard. Only the physical press establishes the
end-to-end path.

## Rollback

Stop and disable only `touchbar-capture.service`. Restore the one saved
helper and client, or delete them (`rm ~/.local/bin/touchbar-capture
/usr/local/bin/touchbar-capture-user`), remove the user unit, and reload the
user manager. Point the Screenshot quick key back at its previous command if
it was changed. Without the receiver, presses still switch layers home and
journal the failed command; the previous deployment silently no-oped.
