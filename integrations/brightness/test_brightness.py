"""All dependencies are fake; sockets have nonce names and never use production."""
import importlib.machinery
import importlib.util
import os
from pathlib import Path
import socket
import subprocess
import threading
import unittest
import uuid

loader = importlib.machinery.SourceFileLoader("dfr_brightness", str(Path(__file__).with_name("dfr-brightness")))
spec = importlib.util.spec_from_loader(loader.name, loader)
brightness = importlib.util.module_from_spec(spec)
loader.exec_module(brightness)


class FakeCommands:
    def __init__(self):
        self.raw = 473
        self.maximum = 509
        self.calls = []
        self.fail_write = False
        self.readback = None

    def __call__(self, *args):
        self.calls.append(args)
        assert args[:5] == ("brightnessctl", "-c", "backlight", "-d", "apple-panel-bl"), args
        if args[5:] == ("-m", "info"):
            return self.readback or f"apple-panel-bl,backlight,{self.raw},unused,{self.maximum}"
        if args[5] == "set":
            if self.fail_write:
                raise subprocess.CalledProcessError(1, args)
            percent = int(args[6][:-1])
            self.raw = (percent * self.maximum + 50) // 100
            return "This is the target, never confirmed readback"
        raise AssertionError(args)


class BrightnessTests(unittest.TestCase):
    def test_read_only_open_and_endpoints_match_existing_linear_range(self):
        fake = FakeCommands()
        backlight = brightness.Backlight(fake)
        self.assertEqual(backlight.dispatch("get"), "ok 93 0")
        self.assertEqual(fake.raw, 473)
        self.assertEqual(len(fake.calls), 1)
        for percent in (1, 2, 25, 50, 99, 100, 1):
            self.assertEqual(backlight.dispatch(f"set {percent}"), f"ok {percent} 0")
            self.assertGreaterEqual(fake.raw, 5)
            self.assertLessEqual(fake.raw, 509)
            self.assertEqual(fake.calls[-2][-2:], ("set", f"{percent}%"))
            self.assertEqual(fake.calls[-1][-2:], ("-m", "info"))

    def test_thumb_gets_separate_hardware_readback_and_failures_propagate(self):
        fake = FakeCommands()
        fake.readback = "apple-panel-bl,backlight,239,47%,509"
        self.assertEqual(brightness.Backlight(fake).dispatch("set 50"), "ok 47 0")
        fake.fail_write = True
        fake.calls.clear()
        with self.assertRaises(subprocess.CalledProcessError):
            brightness.Backlight(fake).dispatch("set 1")
        self.assertEqual(len(fake.calls), 1)

    def test_invalid_protocol_never_touches_hardware(self):
        fake = FakeCommands()
        for request in ("set 0", "set 101", "set -1", "set 01", "set 1\n", "set 1; id", "get apple-panel-bl", "set 228600000.dsi.0 50", ""):
            with self.assertRaises(ValueError):
                brightness.Backlight(fake).dispatch(request)
        self.assertEqual(fake.calls, [])

    def test_bad_readback_is_failure_and_external_zero_is_reported_honestly(self):
        fake = FakeCommands()
        for reply in ("228600000.dsi.0,backlight,123,48%,255", "apple-panel-bl,leds,1,1%,100", "apple-panel-bl,backlight,510,100%,509", "apple-panel-bl,backlight,1,1%,0", "bad"):
            fake.readback = reply
            with self.assertRaises(ValueError):
                brightness.Backlight(fake).dispatch("get")
        fake.readback = "apple-panel-bl,backlight,0,0%,509"
        self.assertEqual(brightness.Backlight(fake).dispatch("get"), "ok 0 0")

    def test_nonce_socket_authentication_and_readback(self):
        for allowed in ({os.getuid()}, set()):
            address = "\0dfr-shared-sliders-brightness-test-" + uuid.uuid4().hex
            self.assertNotIn(address, ("\0dfr-brightness-1000", "\0dfr-volume-1000", "\0voice-dictate-1000"))
            fake = FakeCommands()
            errors = []
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
                listener.bind(address)
                listener.listen(1)
                self.assertEqual(listener.getsockname(), address.encode())
                def respond():
                    try:
                        connection, _ = listener.accept()
                        with connection:
                            brightness.handle(connection, brightness.Backlight(fake), allowed)
                    except Exception as error:
                        errors.append(error)
                thread = threading.Thread(target=respond)
                thread.start()
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                    client.settimeout(3)
                    client.connect(address)
                    client.sendall(b"set 1")
                    client.shutdown(socket.SHUT_WR)
                    try:
                        reply = client.recv(64)
                    except ConnectionResetError:
                        reply = b""
                thread.join(3)
                self.assertFalse(thread.is_alive())
                self.assertEqual(errors, [])
                self.assertEqual(reply, b"ok 1 0" if allowed else b"")
            if not allowed:
                self.assertEqual(fake.calls, [])
                self.assertEqual(fake.raw, 473)


if __name__ == "__main__":
    unittest.main()
