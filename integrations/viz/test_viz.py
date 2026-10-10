"""Behavioral regression tests: no PipeWire, audio device, or input events."""

import importlib.machinery
import importlib.util
import itertools
import json
import os
from pathlib import Path
import socket
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


viz = load("dfr-viz")

ADAPTER = '''#!/usr/bin/python3
import json, os, pathlib, sys, time
name = pathlib.Path(sys.argv[0]).name
root = pathlib.Path(os.environ["FIXTURE_ROOT"])
args = sys.argv[1:]
with (root / (name + ".log")).open("a") as log:
    log.write(json.dumps(args) + "\\n")
if name == "omarchy-audio-output-sink":
    print(os.environ.get("FAKE_SINK", "audio_effect.test-sink"))
elif name == "pactl":
    print(os.environ.get("FAKE_SINK", "audio_effect.test-sink"))
elif name == "stdbuf":
    wrapped = list(args)
    while wrapped and wrapped[0].startswith("-"):
        wrapped.pop(0)
    os.execvp(wrapped[0], wrapped)
elif name == "pw-cat":
    # The capture writes two chunks of a fixed pattern and then idles
    # while the test talks to the feed.
    os.write(1, b"\\x00\\x10" * 1600)
    time.sleep(0.05)
    os.write(1, b"\\x00\\x40" * 1600)
    time.sleep(600)
'''


class MonitorFeedTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        binary = self.root / "bin"
        binary.mkdir()
        for name in ("omarchy-audio-output-sink", "pactl", "stdbuf", "pw-cat"):
            path = binary / name
            path.write_text(ADAPTER)
            path.chmod(0o755)
        self.environment = patch.dict(os.environ, {
            "PATH": str(binary), "FIXTURE_ROOT": str(self.root),
            "FAKE_SINK": "audio_effect.test-sink",
        })
        self.environment.start()
        self.feed = viz.MonitorFeed()

    def tearDown(self):
        self.feed.stop()
        self.environment.stop()
        self.temporary.cleanup()

    def calls(self, name):
        log = self.root / (name + ".log")
        return [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []

    def test_start_captures_the_sink_monitor_and_resolves_levels(self):
        # Without a capture, a level read is idle.
        self.assertIsNone(self.feed.level_now())
        self.feed.start()
        try:
            self.assertIsNotNone(self.feed.process)
            args = self.calls("pw-cat")[0]
            # The stream targets the default sink's monitor, never the sink
            # itself, unbuffered so the live feed stays fresh.
            self.assertEqual(args[args.index("--target") + 1],
                             "audio_effect.test-sink.monitor")
            self.assertEqual(self.calls("stdbuf"), [["-o0", "pw-cat"] + args])
            # The two written chunks fold into the newest peak: 0x4000 at
            # gain 96 is 48.
            deadline = time.monotonic() + 3
            while self.feed.level_now() != 48 and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertEqual(self.feed.level_now(), 48)
        finally:
            self.feed.stop()
        # Stopped: no process, and reads are idle again.
        self.assertIsNone(self.feed.process)
        self.assertIsNone(self.feed.level_now())

    def test_stop_is_idempotent_and_a_second_start_is_fresh(self):
        self.feed.start()
        first = self.feed.process
        self.feed.stop()
        self.feed.stop()
        self.assertIsNotNone(first.poll())
        self.assertEqual(len(self.calls("pw-cat")), 1)
        self.feed.start()
        self.assertIsNotNone(self.feed.process)
        self.assertEqual(len(self.calls("pw-cat")), 2)

    def test_a_failing_capture_reports_an_error_and_no_stream(self):
        binary = self.root / "bin"
        (binary / "pw-cat").write_text("#!/bin/sh\nexit 3\n")
        (binary / "pw-cat").chmod(0o755)
        with self.assertRaises(viz.MonitorError):
            self.feed.start()
        self.assertIsNone(self.feed.process)
        self.assertIsNone(self.feed.level_now())

    def test_the_watchdog_stops_the_capture_when_nothing_polls(self):
        with patch.dict(os.environ, {"DFR_VIZ_IDLE_TIMEOUT": "1"}):
            threading.Thread(target=self.feed.watchdog, daemon=True).start()
            self.feed.start()
            self.assertIsNotNone(self.feed.process)
            deadline = time.monotonic() + 5
            while self.feed.process is not None and time.monotonic() < deadline:
                time.sleep(0.05)
        self.assertIsNone(self.feed.process)
        # A level read keeps the feed alive: while polled, no watchdog stop.
        self.feed.start()
        for _ in range(4):
            time.sleep(0.4)
            self.assertIsNotNone(self.feed.process)
            self.feed.level_now()
        self.assertIsNotNone(self.feed.process)


class ReceiverTests(unittest.TestCase):
    counter = itertools.count()

    def setUp(self):
        self.feed = viz.MonitorFeed()
        self.address = "\0dfr-viz-test-%d-%d" % (os.getpid(), next(ReceiverTests.counter))
        self.allowed = {os.getuid(), 65534}
        threading.Thread(
            target=viz.serve, args=(self.feed, self.address, self.allowed),
            daemon=True).start()
        deadline = time.monotonic() + 3
        while not self._socketpath_ready() and time.monotonic() < deadline:
            time.sleep(0.02)

    def _socketpath_ready(self):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as probe:
            try:
                probe.connect(self.address)
                return True
            except OSError:
                return False

    def tearDown(self):
        self.feed.stop()

    def request(self, payload):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.connect(self.address)
            client.sendall(payload.encode("ascii"))
            client.shutdown(socket.SHUT_WR)
            return client.recv(32).decode("ascii")

    def test_the_protocol_is_finite_and_the_capture_follows_it(self):
        # Unknown or malformed requests never touch the feed.
        for bad in ("start", "ON", "get extra", ""):
            self.assertEqual(self.request(bad), "error")
        # The lifecycle: on and off are acknowledged, off is idempotent,
        # and get reports idle without a capture - then follows the feed.
        self.assertEqual(self.request("off"), "ok")
        self.assertEqual(self.request("get"), "idle")
        with patch.object(viz.MonitorFeed, "start", lambda self: None), \
             patch.object(viz.MonitorFeed, "level_now", lambda self: 42):
            self.assertEqual(self.request("on"), "ok")
            self.assertEqual(self.request("get"), "ok 42")
            self.assertEqual(self.request("off"), "ok")
        # Unpatched, the same feed reads idle again.
        self.assertEqual(self.request("get"), "idle")

    def test_a_caller_with_the_wrong_uid_is_refused(self):
        # The receiver checks SO_PEERCRED, which the kernel sets to this
        # process's own UID; an allowlist that excludes it must refuse the
        # request rather than touch the feed.
        refused = viz.MonitorFeed()
        address = self.address + "-refused"
        threading.Thread(
            target=viz.serve, args=(refused, address, set()), daemon=True).start()
        deadline = time.monotonic() + 3
        while True:
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as other:
                    other.settimeout(2)
                    other.connect(address)
                    other.sendall(b"get")
                    reply = other.recv(32).decode("ascii")
                break
            except OSError:
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.02)
        self.assertEqual(reply, "error")


if __name__ == "__main__":
    unittest.main()
