# Optional cloud Touch Bar dictation

Voice is separate from the daemon and native Settings. It requires Python
3.11+, PipeWire `pw-cat`, `pactl`, coreutils `stdbuf`, `curl`, `wtype`, and
`notify-send` in the desktop session, plus a deliberately supplied **Groq** API
credential. No other application's configuration is discovered, read or migrated.

**Privacy and interaction:** Start captures the selected microphone into temporary
raw audio in the desktop runtime directory. **Stop uploads a WAV to Groq's
transcription endpoint using `whisper-large-v3-turbo`**, then types the returned
transcript into whichever window is focused when insertion occurs. Focus can
change while transcription is running. **Cancel discards the capture without
uploading or inserting anything.** Recording/request/header files are removed
on Stop, Cancel or normal service shutdown; systemd clears the runtime directory
on service stop. Groq receives audio and returns transcript text; local temporary
cleanup does not control the provider's retention or account policy. Provider use
can incur account charges. Read its current terms/settings before enabling this.

Layer switching remains immediate. Notifications and the user-service journal
report recording, transcription and insertion failures. A successful `wtype`
exit does not establish that the intended window received the text.

## Independent credential setup (deliberate opt-in)

As the desktop user, create a new file for this integration only. Do not copy or
reuse a private application's config. Never put a key into Git, command arguments,
chat, logs or test fixtures. The following creates an empty template, refuses to
clobber an existing file and does not request a key:

```sh
install -d -m700 "$HOME/.config/dfr-deck"
(umask 077; set -C; printf '[groq]\napi_key = ""\n' > "$HOME/.config/dfr-deck/voice.toml")
```

Edit that file privately with your own Groq key. The expected schema is:

```toml
[groq]
api_key = "" # enter your own key privately; an empty template cannot record/upload
```

The file must be a regular, non-symlink file owned by the desktop user, readable
only by that user (normally mode 0600). Missing/malformed files, blank/invalid
keys or insecure file permissions fail with a generic error **before recording**
and before upload. A locally valid key can still be rejected by the provider;
that failure occurs at Stop and does not type text. No local key validation makes
a provider call. The helper takes **only an explicit `--credential-file PATH`**;
it has no credential default. The optional unit explicitly passes
`%h/.config/dfr-deck/voice.toml`; an XDG override for the Settings draft does not
change this separately chosen voice path. To choose another standalone file,
change that user unit's explicit argument. The credential stays out of command
arguments and travels to curl through a temporary mode-0600 header file.

## Install after opting in

First use the [complete daemon/Settings installation guide](../../INSTALL.md).
Preserve previous helper/unit/client/drop-in files before an upgrade. From the
repository root, as desktop UID **1000**:

```sh
install -Dm755 integrations/voice/voice-dictate "$HOME/.local/bin/voice-dictate"
install -Dm644 integrations/voice/voice-dictate.service "$HOME/.config/systemd/user/voice-dictate.service"
sudo install -Dm755 integrations/voice/voice-dictate-user /usr/local/bin/voice-dictate-user
sudo install -Dm644 integrations/voice/voice.conf /etc/systemd/system/tiny-dfr.service.d/voice.conf
systemctl --user daemon-reload
systemctl --user enable --now voice-dictate.service
sudo systemctl daemon-reload
```

The consumer `multibar.toml` has **no Mic/Stop/Cancel or recording layer**.
After this deliberate optional setup and privacy review, back up the installed
overlay, validate `/usr/share/tiny-dfr/multibar-voice.toml` with `tiny-dfr check`,
and have an administrator select that profile using the Apply procedure in
[INSTALL.md](../../INSTALL.md), preserving custom settings. Its Mic/Stop/Cancel
actions select this client; Settings remains in More. Merely installing the
profile asset does not select it or start recording. Restart only
`tiny-dfr.service` if needed after installing a drop-in. Voice is unnecessary
for the deck, Settings, Pet or themes. The user manager must already contain the
current `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR` and `DBUS_SESSION_BUS_ADDRESS`.

The client and daemon level reader pin desktop UID 1000; receivers derive their
own UID. A different desktop UID requires coordinated source adaptation and
qualification, not merely a service edit. The daemon remains nobody/65534 and
cannot access the user's home or runtime sockets. This integration keeps the
existing `ProtectHome`, `PrivateTmp`, `NoNewPrivileges` and AF_UNIX/AF_NETLINK
restrictions. It uses no privileged launcher or environment import into nobody.

The receiver's Linux abstract AF_UNIX socket `dfr-voice-<desktop UID>` accepts
only `start`, `stop`, `cancel` and authenticates `SO_PEERCRED` against its own
UID and 65534. The client verifies the receiver UID. Other processes with either
allowed UID can invoke those actions: this existing local trust boundary is
unchanged. Requests supply no shell commands. A second authenticated socket,
`dfr-voice-level-<desktop UID>`, serves only `get` with `ok <0-100>` or `idle`.
It passively tails existing capture bytes for the recording waveform, forgets the
path at capture completion and reports silence if growth stalls for two seconds.
It neither starts capture nor reads credentials.

Microphone selection resolves `pactl get-default-source` by name in
`pactl list sources short` and rejects speaker monitors/unavailable sources.
If necessary set `Environment=VOICE_DICTATE_SOURCE=<name>` in this user unit.
Start requires capture bytes within three seconds; this proves neither audible
speech nor transcription quality.

## Safe verification and separate live qualification

These executable adapters/socket tests use **dummy credentials, synthetic audio
and fake external tools**. They never access real keys, microphones, cloud services,
focused windows or production sockets:

```sh
python3 -B -m unittest discover -s integrations/voice -p 'test_*.py' -v
```

Missing/invalid credential tests assert that no capture/upload/insertion occurs;
Cancel tests assert no request or typing; request/HTTP/JSON/insertion failures
are exercised through mocks. A same-user test does not qualify the deployed
nobody namespace, hardware, compositor or provider.

Live voice acceptance is a separate deliberate operator action after credential
setup and disclosure review, never part of installation/offline tests. Focus a
disposable field, use Mic then Stop, and confirm where returned text lands.
Repeat Mic then Cancel: no upload or insertion should occur. If isolating provider
transcription using a synthetic WAV is separately wanted, the explicit CLI is:

```sh
"$HOME/.local/bin/voice-dictate" transcribe-file /path/to/synthetic.wav \
    --credential-file "$HOME/.config/dfr-deck/voice.toml"
```

That command **uploads to Groq and prints the transcript**, without insertion;
do not treat it as an offline test. Mic during transcription reports busy rather
than queueing another recording. Inspect only this service's relevant errors;
never print credential files.

## Rollback / uninstall

Stop/disable only `voice-dictate.service`. Restore the prior helper, client, user
unit and voice-specific drop-in, or remove only files newly added by this setup;
reload the respective managers and restart only the daemon if its drop-in changed.
Preserve other drop-ins and deck customizations. The standalone credential file
is user-owned and separate: retain it privately or deliberately delete/revoke it
through your provider account; do not migrate it elsewhere. Keep secrets, audio,
transcripts and personal backups outside source control.
