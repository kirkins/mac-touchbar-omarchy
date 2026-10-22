# Touch Bar output volume

`dfr-volume` runs in the desktop user's session. The Touch Bar daemon sends
bounded requests over the abstract AF_UNIX socket `dfr-volume-<uid>` of the
seat's active desktop user, verifies the receiver's kernel peer UID matches, and
reads back the output percentage and mute state. This follows the voice
integration's desktop-user boundary without connecting to its socket or
modifying its files.

The receiver binds its own UID's abstract socket and accepts only that UID or
the daemon's `tiny-dfr` account (`nobody` on installs without it). It serializes a finite `get`, `set 0..130` protocol; callers cannot supply commands, sink names
or paths. It resolves each
operation's active output with `omarchy-audio-output-sink`, writes an absolute
percentage with `pactl`, and returns mixer readback. Both channels become zero
at the slider's left endpoint. Absolute writes clear mute, matching the existing
keyboard helper. The existing 130% helper remains unchanged.

The optional user service requires Python3, PipeWire's PulseAudio tools and the
installed Omarchy sink resolver. It starts with the graphical session, runs as
the desktop user and retains `NoNewPrivileges=yes`. The Touch Bar system unit,
root-owned binary, `tiny-dfr` execution, private home/tmp and AF_UNIX/NETLINK policy
do not need changes. The new service includes the packaged Omarchy bin directory
in its own PATH, so it does not depend on an interactive shell profile.

Deploy the checked `dfr-volume` to the desktop user's `~/.local/bin/dfr-volume`
as 0755 and `dfr-volume.service` to their user systemd directory as 0644. Enable
the new user service with the approved scoped deployment, then install the
checked Touch Bar binary and update only the affected home/media controls in
the existing configuration. Retain any other installed configuration options
and controls. No voice client, receiver, helper or unit replacement is required.
These are deployment instructions, not authorization to install during review.

Run `python3 -B -m unittest discover -s integrations/audio -p test_volume.py`
for synthetic mixer and socketpair checks. Rust tests use task-unique abstract
socket names with fake receivers; no test connects to the production audio or
voice socket or changes live mixer state. Physical touch feel and installed
receiver operation remain scoped deployment checks after approval.
