"""Behavioral regression tests: no microphone, network, or input events."""

import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import tempfile
import threading
import time
import unittest
from unittest.mock import patch


def load(name):
    path = Path(__file__).parent / name
    loader = importlib.machinery.SourceFileLoader(name.replace("-", "_"), str(path))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


voice = load("voice-dictate")
client = load("voice-dictate-user")

ADAPTER = '''#!/usr/bin/python3
import json, os, pathlib, sys, time
name = pathlib.Path(sys.argv[0]).name
root = pathlib.Path(os.environ["FIXTURE_ROOT"])
args = sys.argv[1:]
with (root / (name + ".log")).open("a") as log:
    log.write(json.dumps(args) + "\\n")
if name == "pactl":
    if args[0] == "get-default-source":
        print(os.environ.get("FAKE_DEFAULT", "effect_output.builtin-mic"))
    else:
        print("62\\talsa_input.headset\\tPipeWire\\ts32le 1ch 48000Hz\\tIDLE")
        print("68\\teffect_output.builtin-mic\\tPipeWire\\tfloat32le 1ch 48000Hz\\tSUSPENDED")
elif name == "stdbuf":
    # The helper unbuffers pw-cat through coreutils stdbuf; the prologue
    # above already logged this call, so just exec the command it wraps
    # (here the fake pw-cat adapter, found through the same PATH).
    wrapped = list(args)
    while wrapped and wrapped[0].startswith("-"):
        wrapped.pop(0)
    os.execvp(wrapped[0], wrapped)
elif name == "pw-cat":
    if os.environ.get("FAKE_CAPTURE") != "empty":
        os.write(1, b"\\x00\\x10" * 3200)
    time.sleep(600)
elif name == "curl":
    auth = pathlib.Path(args[args.index("--header") + 1][1:])
    (root / "auth-mode").write_text(oct(auth.stat().st_mode & 0o777))
    mode = os.environ.get("FAKE_HTTP", "ok")
    if mode == "network":
        print("synthetic network failure", file=sys.stderr)
        sys.exit(7)
    response = pathlib.Path(args[args.index("--output") + 1])
    body = {"text": "Synthetic fixture transcript."}
    if mode == "error":
        body = {"error": {"message": "Credential rejected"}}
    elif mode == "odd-error":
        body = {"error": "unexpected error schema"}
    elif mode == "empty":
        body = {"text": " "}
    response.write_text("not JSON" if mode == "invalid" else json.dumps(body))
    print("401" if mode in ("error", "odd-error") else "200", end="")
elif name == "wtype":
    sys.exit(int(os.environ.get("FAKE_INSERT_EXIT", "0")))
'''


class DictationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        binary = self.root / "bin"
        binary.mkdir()
        for name in ("pactl", "pw-cat", "curl", "wtype", "notify-send", "stdbuf"):
            path = binary / name
            path.write_text(ADAPTER)
            path.chmod(0o755)
        self.environment = patch.dict(os.environ, {
            "PATH": str(binary), "FIXTURE_ROOT": str(self.root),
            "VOICE_DICTATE_SOURCE": "", "FAKE_CAPTURE": "data", "FAKE_HTTP": "ok",
            "FAKE_DEFAULT": "effect_output.builtin-mic", "FAKE_INSERT_EXIT": "0",
        })
        self.environment.start()
        credential = self.root / "credential.toml"
        credential.write_text('[groq]\napi_key = "dummy-test-key"\n')
        credential.chmod(0o600)
        self.engine = voice.Dictation(self.root / "state", credential)

    def tearDown(self):
        self.engine.discard()
        self.environment.stop()
        self.temporary.cleanup()

    def calls(self, name):
        log = self.root / (name + ".log")
        return [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []

    def test_no_implicit_credential_is_read_or_recording_started(self):
        # Only this disposable home exists in the test. Never inspect a real key.
        with patch.object(voice.Path, "home", return_value=self.root):
            engine = voice.Dictation(self.root / "unconfigured")
        self.addCleanup(engine.discard)
        with self.assertRaises(voice.VoiceError):
            engine.start()
        self.assertEqual(self.calls("pactl"), [])
        self.assertEqual(self.calls("pw-cat"), [])
        self.assertEqual(self.calls("curl"), [])

    def test_missing_credential_blocks_start_before_capture(self):
        self.engine.credential_path.unlink()
        with self.assertRaises(voice.VoiceError):
            self.engine.start()
        self.assertEqual(self.calls("pw-cat"), [])
        self.assertEqual(self.calls("curl"), [])

    def test_invalid_credentials_never_record_upload_type_or_disclose_contents(self):
        path = self.engine.credential_path
        for contents in ('[groq]\napi_key = ""', '[groq]\napi_key = " "',
                         '[groq]\napi_key = 42', '[groq]\napi_key = "dummy\\nkey"',
                         '[groq]\napi_key = "dummy\\rkey"',
                         '[stt]\napi_key = "dummy-test-key"', 'not TOML'):
            with self.subTest(contents=contents):
                path.write_text(contents)
                with self.assertRaises(voice.VoiceError) as error:
                    self.engine.start()
                self.assertNotIn(contents, str(error.exception))
                with self.assertRaises(voice.VoiceError):
                    self.engine.transcribe(self.root / "synthetic.wav")
        self.assertEqual(self.calls("pactl"), [])
        self.assertEqual(self.calls("pw-cat"), [])
        self.assertEqual(self.calls("curl"), [])
        self.assertEqual(self.calls("wtype"), [])

    def test_insecure_or_symlinked_credential_is_rejected(self):
        path = self.engine.credential_path
        path.chmod(0o644)
        with self.assertRaises(voice.VoiceError):
            self.engine.start()
        path.chmod(0o600)
        link = self.root / "key-link"
        link.symlink_to(path)
        self.engine.credential_path = link
        with self.assertRaises(voice.VoiceError):
            self.engine.transcribe(self.root / "synthetic.wav")
        self.assertEqual(self.calls("pw-cat"), [])
        self.assertEqual(self.calls("curl"), [])

    def test_start_stop_records_default_by_name_then_inserts(self):
        self.engine.dispatch("start")
        self.engine.dispatch("start")
        self.assertEqual(len(self.calls("pw-cat")), 1)
        args = self.calls("pw-cat")[0]
        self.assertEqual(args[args.index("--target") + 1], "effect_output.builtin-mic")
        # The recorder runs unbuffered so the passive level feed tails the
        # capture as it is written instead of a quarter second behind.
        self.assertEqual(self.calls("stdbuf"), [["-o0", "pw-cat"] + args])
        # The capture file is also published to the passive level feed.
        self.assertIsNotNone(self.engine.levels.path)
        self.engine.dispatch("stop")
        self.assertIsNone(self.engine.levels.path)
        self.assertEqual(self.calls("wtype"), [["--", "Synthetic fixture transcript."]])
        self.assertEqual((self.root / "auth-mode").read_text(), "0o600")
        self.assertEqual(list(self.engine.state_dir.iterdir()), [])

    def test_cancel_never_requests_or_types_and_reaps_capture(self):
        self.engine.dispatch("start")
        recorder = self.engine.recorder
        self.engine.dispatch("cancel")
        self.assertIsNotNone(recorder.poll())
        self.assertIsNone(self.engine.levels.path)
        self.engine.dispatch("stop")
        self.assertEqual(self.calls("curl"), [])
        self.assertEqual(self.calls("wtype"), [])
        self.assertEqual(list(self.engine.state_dir.iterdir()), [])

    def test_dead_recorder_is_failure(self):
        self.engine.start()
        self.engine.recorder.kill()
        self.engine.recorder.wait()
        with self.assertRaisesRegex(voice.VoiceError, "exited before Stop"):
            self.engine.stop()
        self.assertEqual(self.calls("curl"), [])

    def test_no_audio_is_failure_and_cleaned(self):
        os.environ["FAKE_CAPTURE"] = "empty"
        with self.assertRaisesRegex(voice.VoiceError, "no capture data"):
            self.engine.start()
        self.assertIsNone(self.engine.recorder)
        self.assertEqual(list(self.engine.state_dir.iterdir()), [])

    def test_unavailable_or_monitor_default_does_not_capture(self):
        for source in ("missing", "speakers.monitor"):
            with self.subTest(source=source):
                os.environ["FAKE_DEFAULT"] = source
                with self.assertRaises(voice.VoiceError):
                    self.engine.start()
        self.assertEqual(self.calls("pw-cat"), [])

    def test_transcription_failures_are_errors_with_no_insertion(self):
        for mode, message in (("error", "HTTP 401.*Credential rejected"),
                              ("odd-error", "HTTP 401.*Request rejected"),
                              ("invalid", "invalid JSON"),
                              ("empty", "no text"), ("network", "transport failed.*synthetic network")):
            with self.subTest(mode=mode):
                os.environ["FAKE_HTTP"] = mode
                self.engine.start()
                with self.assertRaisesRegex(voice.VoiceError, message):
                    self.engine.stop()
                self.assertEqual(self.calls("wtype"), [])
                self.assertEqual(list(self.engine.state_dir.iterdir()), [])

    def test_failed_insertion_never_reports_typed(self):
        os.environ["FAKE_INSERT_EXIT"] = "1"
        self.engine.start()
        with self.assertRaisesRegex(voice.VoiceError, "insertion failed"):
            self.engine.stop()
        messages = [args[-1] for args in self.calls("notify-send")]
        self.assertFalse(any(message.startswith("Typed") for message in messages))


class ReceiverTests(unittest.TestCase):
    def setUp(self):
        self.address = "\0dfr-voice-test-" + str(os.getpid()) + "-" + str(time.monotonic_ns())
        self.server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.server.bind(self.address)
        self.server.listen(2)
        self.addCleanup(self.server.close)
        self.actions = []
        self.receiver = voice.Receiver(self.actions.append, self.address)
        self.notifications = patch.object(voice, "notify")
        self.notifications.start()
        self.addCleanup(self.notifications.stop)

    def accept_one(self):
        connection, _ = self.server.accept()
        self.receiver.handle(connection)

    def exchange(self, action):
        worker = threading.Thread(target=self.accept_one)
        worker.start()
        try:
            client.request(action, os.getuid(), self.address)
        finally:
            worker.join(timeout=3)
        self.assertFalse(worker.is_alive())

    def test_real_socket_routes_only_three_actions(self):
        for action in ("start", "stop", "cancel"):
            self.exchange(action)
        self.assertEqual(self.actions, ["start", "stop", "cancel"])

    @unittest.skipIf(os.getuid() == 0, "Run as an unprivileged desktop user")
    def test_non_root_child_without_session_environment_reaches_receiver(self):
        worker = threading.Thread(target=self.accept_one)
        worker.start()
        code = """import importlib.machinery, importlib.util, os, sys
assert os.getuid() != 0
assert 'XDG_RUNTIME_DIR' not in os.environ and 'WAYLAND_DISPLAY' not in os.environ
loader = importlib.machinery.SourceFileLoader('client', sys.argv[1])
spec = importlib.util.spec_from_loader(loader.name, loader)
client = importlib.util.module_from_spec(spec)
loader.exec_module(client)
client.request('start', os.getuid(), '\\0' + sys.argv[2])
"""
        try:
            result = subprocess.run(
                ["/usr/bin/python3", "-B", "-c", code,
                 str(Path(__file__).parent / "voice-dictate-user"), self.address[1:]],
                env={"PATH": "/usr/bin"}, capture_output=True, text=True, timeout=5)
        finally:
            worker.join(timeout=3)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.actions, ["start"])
        self.assertFalse(worker.is_alive())

    def test_peer_uid_is_enforced(self):
        self.receiver.allowed_uids = {os.getuid() + 1}
        # The receiver rejects on SO_PEERCRED before reading the request, so the
        # kernel may tear the socket down (EPIPE/ECONNRESET) before the client
        # reads the JSON error; either observable proves the rejection.
        with self.assertRaisesRegex(
                (ValueError, OSError), "not permitted|reset|Broken pipe"):
            self.exchange("start")
        self.assertEqual(self.actions, [])

    def test_shell_payload_is_rejected_on_wire(self):
        worker = threading.Thread(target=self.accept_one)
        worker.start()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.connect(self.address)
            connection.sendall(b"start; touch /tmp/unsafe")
            connection.shutdown(socket.SHUT_WR)
            result = json.loads(connection.recv(4096))
        worker.join(timeout=3)
        self.assertFalse(result["ok"])
        self.assertEqual(self.actions, [])

    def test_busy_returns_error_without_delayed_action(self):
        self.receiver.lock.acquire()
        try:
            with self.assertRaisesRegex(ValueError, "busy"):
                self.exchange("start")
        finally:
            self.receiver.lock.release()
        self.assertEqual(self.actions, [])

    def test_handler_failure_reaches_client(self):
        def failing(_):
            raise voice.VoiceError("Transcription failed (HTTP 401)")
        self.receiver.handler = failing
        with self.assertRaisesRegex(ValueError, "HTTP 401"):
            self.exchange("stop")

    def test_client_cli_rejects_arbitrary_commands_before_connect(self):
        result = subprocess.run(["/usr/bin/python3", str(Path(__file__).parent / "voice-dictate-user"),
                                 "start;echo unsafe"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("invalid choice", result.stderr)


class LevelFeedTests(unittest.TestCase):
    """The passive capture-level reader: synthetic PCM, no microphone."""

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.feed = voice.LevelFeed()

    def tearDown(self):
        self.feed.stop()
        self.temporary.cleanup()

    def capture(self, chunk):
        with (self.root / "audio.raw").open("ab") as raw:
            raw.write(chunk)
        return self.root / "audio.raw"

    def test_no_active_recording_reports_idle(self):
        self.assertIsNone(self.feed.level_now())
        self.feed.start(self.capture(b"\x00\x10" * 10))
        self.feed.stop()
        self.assertIsNone(self.feed.level_now())
        self.feed.start(self.root / "never-created.raw")
        self.assertIsNone(self.feed.level_now())

    def test_levels_follow_capture_peaks_of_new_samples_only(self):
        self.feed.start(self.capture(b""))
        full = struct.pack("<h", 32767) * 4 + struct.pack("<h", -32767) * 4
        self.capture(full)
        self.assertEqual(self.feed.level_now(), 100)
        # The loud chunk is consumed; appended silence reads as silence.
        self.capture(b"\x00\x00" * 8)
        self.assertEqual(self.feed.level_now(), 0)
        quarter = struct.pack("<h", 8192) + struct.pack("<h", -8192)
        self.capture(quarter)
        self.assertEqual(self.feed.level_now(), 40)
        # A lone trailing byte waits for its pair instead of shifting samples.
        self.capture(b"\x10")
        self.assertEqual(self.feed.level_now(), 40)
        self.capture(b"\x00")
        self.assertEqual(self.feed.level_now(), 0)

    def test_stalled_capture_reports_silence_not_a_frozen_bar(self):
        self.feed.start(self.capture(b""))
        self.capture(struct.pack("<h", 20000) * 4)
        self.assertEqual(self.feed.level_now(), 97)
        self.feed.grown = time.monotonic() - 3.0
        self.assertEqual(self.feed.level_now(), 0)


class LevelServerTests(unittest.TestCase):
    """The level endpoint beside the action receiver: real AF_UNIX only."""

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.address = "\0dfr-voice-level-test-" + str(os.getpid()) + "-" + str(time.monotonic_ns())
        self.feed = voice.LevelFeed()
        self.server = threading.Thread(
            target=voice.serve_levels,
            args=(self.feed, self.address, {os.getuid(), 65534}),
            daemon=True,
        )
        self.server.start()

    def tearDown(self):
        self.feed.stop()
        self.temporary.cleanup()

    def request_level(self, payload=b"get"):
        for _ in range(100):
            try:
                connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                connection.settimeout(5)
                connection.connect(self.address)
                break
            except OSError:
                connection.close()
                time.sleep(0.02)
        else:
            raise AssertionError("level receiver never came up")
        with connection:
            _, uid, _ = struct.unpack(
                "3i", connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            self.assertEqual(uid, os.getuid())
            connection.sendall(payload)
            connection.shutdown(socket.SHUT_WR)
            return connection.recv(64).decode("ascii")

    def test_serves_idle_levels_and_rejects_other_requests(self):
        self.assertEqual(self.request_level(), "idle")
        path = self.root / "audio.raw"
        path.write_bytes(struct.pack("<h", 32767) * 8)
        self.feed.start(path)
        self.assertEqual(self.request_level(), "ok 100")
        self.assertEqual(self.request_level(b"set 5"), "error")
        self.feed.stop()
        self.assertEqual(self.request_level(), "idle")

    def test_peer_uid_is_enforced(self):
        address = self.address + "-strict"
        feed = voice.LevelFeed()
        threading.Thread(
            target=voice.serve_levels, args=(feed, address, {os.getuid() + 1}), daemon=True
        ).start()
        for _ in range(100):
            try:
                connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                connection.settimeout(5)
                connection.connect(address)
                break
            except OSError:
                connection.close()
                time.sleep(0.02)
        else:
            raise AssertionError("level receiver never came up")
        with connection:
            connection.sendall(b"get")
            connection.shutdown(socket.SHUT_WR)
            self.assertEqual(connection.recv(64).decode("ascii"), "error")


if __name__ == "__main__":
    unittest.main()
