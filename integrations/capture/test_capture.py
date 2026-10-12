"""Behavioral regression tests: no screen capture, picker, or session."""

import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import socket
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


capture = load("touchbar-capture")
client = load("touchbar-capture-user")

# Every test resolves `omarchy` and `notify-send` to these adapters, so a
# regression can never open the real picker, take a real screenshot, or
# send a real notification.
ADAPTER = '''#!/usr/bin/python3
import json, os, pathlib, sys, time
name = pathlib.Path(sys.argv[0]).name
root = pathlib.Path(os.environ["FIXTURE_ROOT"])
args = sys.argv[1:]
with (root / (name + ".log")).open("a") as log:
    log.write(json.dumps(args) + "\\n")
if name == "omarchy":
    if os.environ.get("FAKE_CAPTURE_EXIT"):
        sys.exit(int(os.environ["FAKE_CAPTURE_EXIT"]))
    time.sleep(float(os.environ.get("FAKE_CAPTURE_SLEEP", "600")))
if name == "systemd-run" and os.environ.get("FAKE_SYSTEMD_RUN_EXIT"):
    sys.exit(int(os.environ["FAKE_SYSTEMD_RUN_EXIT"]))
'''


class Fixture:
    """Fake session binaries on PATH plus a shared capture engine."""

    def start_fixture(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        binary = self.root / "bin"
        binary.mkdir()
        for name in ("omarchy", "notify-send", "systemd-run"):
            path = binary / name
            path.write_text(ADAPTER)
            path.chmod(0o755)
        self.environment = patch.dict(os.environ, {
            "PATH": str(binary), "FIXTURE_ROOT": str(self.root),
            "FAKE_CAPTURE_SLEEP": "600", "FAKE_CAPTURE_EXIT": "",
            "FAKE_SYSTEMD_RUN_EXIT": "",
        })
        self.environment.start()
        self.engine = capture.Capture()
        self.notifications = patch.object(capture, "notify")
        self.notifications.start()

    def stop_fixture(self):
        self.stop_capture()
        self.notifications.stop()
        self.environment.stop()
        self.temporary.cleanup()

    def stop_capture(self):
        child = self.engine.capture
        if child is not None and child.poll() is None:
            child.kill()
            child.wait()
        self.engine.capture = None

    def calls(self, name):
        log = self.root / (name + ".log")
        return [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []


class CaptureTests(Fixture, unittest.TestCase):
    def setUp(self):
        self.start_fixture()

    def tearDown(self):
        self.stop_fixture()

    def test_dispatch_runs_the_fixed_session_flow(self):
        os.environ["FAKE_CAPTURE_SLEEP"] = "0.1"
        self.engine.dispatch("screenshot")
        self.assertEqual(self.calls("omarchy"), [["capture", "screenshot"]])
        self.assertEqual(self.calls("notify-send"), [])
        self.engine.capture.wait(timeout=5)

    def test_second_dispatch_while_capture_runs_is_busy_then_recoverable(self):
        self.engine.dispatch("screenshot")
        child = self.engine.capture
        # The picker is detached into its own session, so it outlives a
        # receiver restart instead of dying with it.
        self.assertNotEqual(os.getpgid(child.pid), os.getpgid(0))
        with self.assertRaisesRegex(capture.CaptureError, "already running"):
            self.engine.dispatch("screenshot")
        self.assertEqual(len(self.calls("omarchy")), 1)
        child.kill()
        child.wait()
        self.engine.dispatch("screenshot")
        self.assertEqual(len(self.calls("omarchy")), 2)

    def test_immediate_nonzero_exit_is_an_error(self):
        os.environ["FAKE_CAPTURE_EXIT"] = "3"
        with self.assertRaisesRegex(capture.CaptureError, r"exited immediately \(3\)"):
            self.engine.dispatch("screenshot")

    def test_invalid_action_is_rejected_before_any_launch(self):
        with self.assertRaisesRegex(capture.CaptureError, "Invalid action"):
            self.engine.dispatch("stop")
        self.assertEqual(self.calls("omarchy"), [])

    def test_missing_launcher_is_an_error(self):
        engine = capture.Capture(launchers={
            "screenshot": ["/nonexistent-dir/omarchy", "capture", "screenshot"],
            "settings": ["systemd-run", "--user", "--collect", "--quiet",
                         "/usr/local/bin/dfr-configurator"],
        })
        with self.assertRaisesRegex(capture.CaptureError, "Cannot launch"):
            engine.dispatch("screenshot")

    def test_settings_runs_only_the_fixed_session_launcher(self):
        self.engine.dispatch("settings")
        self.assertEqual(self.calls("systemd-run"), [[
            "--user", "--collect", "--quiet", "/usr/local/bin/dfr-configurator"]])

    def test_settings_launcher_failure_is_reported(self):
        os.environ["FAKE_SYSTEMD_RUN_EXIT"] = "1"
        with self.assertRaisesRegex(capture.CaptureError, "exit status: 1"):
            self.engine.dispatch("settings")


class ReceiverTests(Fixture, unittest.TestCase):
    def setUp(self):
        self.start_fixture()
        self.address = "\0dfr-capture-test-" + str(os.getpid()) + "-" + str(time.monotonic_ns())
        self.receiver = capture.Receiver(self.engine.dispatch, self.address)
        self.server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.server.bind(self.address)
        self.server.listen(2)
        self.addCleanup(self.server.close)

    def tearDown(self):
        self.stop_fixture()

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

    def raw_exchange(self, payload):
        worker = threading.Thread(target=self.accept_one)
        worker.start()
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
                connection.settimeout(5)
                connection.connect(self.address)
                connection.sendall(payload)
                connection.shutdown(socket.SHUT_WR)
                result = json.loads(connection.recv(4096))
        finally:
            worker.join(timeout=3)
        self.assertFalse(worker.is_alive())
        return result

    def test_real_socket_routes_only_screenshot(self):
        self.exchange("screenshot")
        self.assertIsNotNone(self.engine.capture)
        before = len(self.calls("omarchy"))
        # A clean but unknown action is rejected on the wire, not by the
        # client's pre-connect validation.
        result = self.raw_exchange(b"record")
        self.assertFalse(result["ok"])
        self.assertIn("Invalid action", result["error"])
        self.assertEqual(len(self.calls("omarchy")), before)

    def test_real_socket_routes_settings_and_rejects_unknown_actions(self):
        self.exchange("settings")
        self.assertEqual(self.calls("systemd-run"), [[
            "--user", "--collect", "--quiet", "/usr/local/bin/dfr-configurator"]])
        result = self.raw_exchange(b"arbitrary")
        self.assertFalse(result["ok"])
        self.assertIn("Invalid action", result["error"])
        self.assertEqual(len(self.calls("systemd-run")), 1)

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
client.request('screenshot', os.getuid(), '\\0' + sys.argv[2])
"""
        try:
            result = subprocess.run(
                ["/usr/bin/python3", "-B", "-c", code,
                 str(Path(__file__).parent / "touchbar-capture-user"), self.address[1:]],
                env={"PATH": "/usr/bin"}, capture_output=True, text=True, timeout=5)
        finally:
            worker.join(timeout=3)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIsNotNone(self.engine.capture)

    def test_peer_uid_is_enforced(self):
        self.receiver.allowed_uids = {os.getuid() + 1}
        # The receiver rejects on SO_PEERCRED before reading the request, so the
        # kernel may tear the socket down (EPIPE/ECONNRESET) before the client
        # reads the JSON error; either observable proves the rejection.
        with self.assertRaisesRegex(
                (ValueError, OSError), "not permitted|reset|Broken pipe"):
            self.exchange("screenshot")
        self.assertIsNone(self.engine.capture)

    def test_shell_payload_is_rejected_on_wire(self):
        result = self.raw_exchange(b"screenshot; touch /tmp/unsafe")
        self.assertFalse(result["ok"])
        self.assertIsNone(self.engine.capture)

    def test_busy_returns_error_without_delayed_action(self):
        self.receiver.lock.acquire()
        try:
            with self.assertRaisesRegex(ValueError, "busy"):
                self.exchange("screenshot")
        finally:
            self.receiver.lock.release()
        self.assertIsNone(self.engine.capture)

    def test_handler_failure_reaches_client(self):
        def failing(_):
            raise capture.CaptureError("Capture exited immediately (3)")
        self.receiver.handler = failing
        with self.assertRaisesRegex(ValueError, "exited immediately"):
            self.exchange("screenshot")

    def test_client_cli_rejects_arbitrary_commands_before_connect(self):
        result = subprocess.run(["/usr/bin/python3", str(Path(__file__).parent / "touchbar-capture-user"),
                                 "screenshot;echo unsafe"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("invalid choice", result.stderr)

    def test_client_action_validation_accepts_settings_and_rejects_unknown(self):
        with self.assertRaisesRegex(ValueError, "Invalid action"):
            client.request("arbitrary", os.getuid(), "\0missing")
        self.exchange("settings")


if __name__ == "__main__":
    unittest.main()
