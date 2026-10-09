# Local Touch Bar dictation

This optional integration is maintained in this private repository. Install it
separately from the Touch Bar daemon. Python 3.11+, PipeWire `pw-cat`, `pactl`,
`curl`, `wtype`, and `notify-send` must be available in the desktop session.

The Mic command calls `/usr/local/bin/voice-dictate-user start`. This client
uses a Linux abstract AF_UNIX socket named `dfr-voice-<desktop UID>` to reach
`voice-dictate.service` in the desktop user's graphical session. Stop terminates
capture, sends a WAV to Groq Whisper, and types the returned text into the
focused window. Cancel terminates capture and discards it without a request
or typing. Layer switching remains immediate, so notifications and the user
service journal report recording, transcription, and insertion failures.

The receiver accepts exactly `start`, `stop`, and `cancel`, authenticates
`SO_PEERCRED` against the desktop UID and UID 65534 (`nobody`), and runs no shell
commands from requests. The client also checks the receiver's UID. Other
processes with either allowed UID can invoke these same three actions; this
is an explicit local trust boundary. The abstract socket crosses the daemon's
private mount namespace without making the desktop runtime directory public.
The daemon keeps its internal privilege drop, `NoNewPrivileges`, and AF_UNIX /
AF_NETLINK restriction. Neither `runuser` nor a privileged launcher is involved.

The same service also binds a second abstract socket, `dfr-voice-level-<desktop
UID>`, for the Touch Bar's live recording waveform. Each one-shot `get` request
returns the latest 0-100 input peak of the ongoing capture (`ok <n>`) or `idle`
when no recording is active; anything else is rejected with `error`. The level
reader passively tails the raw capture file the recorder is already writing,
under the same `SO_PEERCRED` boundary, and never touches the recorder, its file
descriptors, or the dictation flow; capture finishing forgets the path at once.
A capture that stops growing for two seconds reports silence so a stalled bar
cannot freeze loud. The recorder itself runs unbuffered - `stdbuf -o0`
wraps `pw-cat` - so the tailed bytes trail the microphone by roughly one
audio quantum instead of the quarter second an 8 KB stdio buffer would add;
the captured file's content is unchanged. The daemon polls this endpoint only while its recording
layer is displayed. Updating the installed helper and user service is enough
to enable it; no new unit or permission is involved.

## Install

Before replacing an existing helper, launcher, or voice-specific drop-in, keep
one rollback copy in a private task directory outside the repository. Never
commit credentials, audio, transcripts, or copies of personal configuration.

Run these commands as the desktop user from the repository root:

```sh
install -Dm755 integrations/voice/voice-dictate "$HOME/.local/bin/voice-dictate"
install -Dm644 integrations/voice/voice-dictate.service "$HOME/.config/systemd/user/voice-dictate.service"
sudo install -Dm755 integrations/voice/voice-dictate-user /usr/local/bin/voice-dictate-user
sudo install -Dm644 integrations/voice/voice.conf /etc/systemd/system/tiny-dfr.service.d/voice.conf
systemctl --user daemon-reload
systemctl --user enable --now voice-dictate.service
sudo systemctl daemon-reload
sudo systemctl restart tiny-dfr.service
```

The client targets desktop UID 1000. The receiver derives its socket name
from its own UID. If the daemon uses a different UID, adapt the receiver's
allowed UID set after reviewing that boundary. Do not grant the daemon access
to the user's home or runtime directory. The new drop-in restores the stock
`ProtectHome` and `PrivateTmp` settings which the previous `runuser`
deployment had relaxed.

Use the existing mic / Stop / Cancel actions from
[`share/tiny-dfr/multibar.toml`](../../share/tiny-dfr/multibar.toml) or change only
those actions in the deployed configuration. Preserve other custom buttons.
The service starts with `graphical-session.target`; its manager environment must
already contain the current `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`, and
`DBUS_SESSION_BUS_ADDRESS`. Inspect only those fields if troubleshooting.
Re-login or restart only this voice service if the desktop environment changes.

The helper reads `[stt].api_key` from the existing private
`~/.config/herdr-telegram-bridge/config.toml`. It uses Groq's transcription
endpoint and `whisper-large-v3-turbo`. The key stays out of command arguments
and travels in a temporary mode-0600 header file. Recording and request files
live in the desktop runtime directory and are removed after Stop, Cancel, or
service shutdown. An interrupted session is cleared by systemd on service stop.

Audio selection resolves `pactl get-default-source` by the name column in
`pactl list sources short`. It rejects unavailable sources and speaker monitors.
It does not select a headset simply because it appears first. If the intended
mic differs from the session default, add an `Environment=VOICE_DICTATE_SOURCE=<name>`
override to this user unit and restart only the voice service. Verify actual
signal before declaring a source working. Start requires capture bytes within
three seconds; it does not establish audible speech or transcription quality.
The level feed reports capture peaks, not audibility or transcription quality.

## Verification

These regressions run small executable adapters, actual subprocesses, and real
AF_UNIX requests. They capture no microphone, send no HTTP request, and type
nothing:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s integrations/voice -p 'test_*.py' -v
systemd-analyze --user verify integrations/voice/voice-dictate.service
```

First verify the receiver with a harmless marker handler and the client under
the actual daemon UID, supplementary groups, session-free environment, mount
namespace, and no-new-privileges restrictions. A same-user socket test alone
does not reproduce the original root-only `runuser` failure. Never use a live
Mic press for this first check if the receiver is configured to record.

Then isolate the provider with a small synthetic spoken WAV and the existing
credential path:

```sh
"$HOME/.local/bin/voice-dictate" transcribe-file /private/task/synthetic.wav
journalctl --user -u voice-dictate.service --since today --no-pager
```

`transcribe-file` prints the transcript and never inserts it. Test actual
insertion only after deliberately creating and focusing a disposable text field.
Network errors, HTTP failures (including provider error messages), invalid JSON,
empty transcripts, and `wtype` failures return errors. “Typed” is announced
only after `wtype` exits successfully; that alone still cannot prove the intended
window received the text.

Finally coordinate a physical check: focus the disposable field, press Mic,
speak a short phrase, observe the recording notification, press Stop, and
observe the transcript in that field. Repeat with Mic then Cancel; the field
must stay unchanged, and no transcription should run. Only this physical path
can establish end-to-end dictation. Pressing Mic again during transcription
reports busy; retry when transcription finishes. New actions are not queued
to record unexpectedly after a slow request.

## Rollback

Stop and disable only `voice-dictate.service`. Restore the one saved helper,
launcher, and voice drop-in, then reload both relevant managers and restart only
`tiny-dfr.service`. Restore the voice button commands only if they were changed.
Remove the newly added voice unit and reload the user manager. The original
launcher is known to fail as `nobody`; restoring it restores the previous
deployment, not working dictation. Remove superseded rollback copies once the
repair is validated and durably recorded.
