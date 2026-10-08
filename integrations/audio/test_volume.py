"""Synthetic mixer/protocol tests: never connect to a production audio socket."""
import importlib.machinery
import importlib.util
import os
from pathlib import Path
import socket
import subprocess
import threading
import unittest
from unittest.mock import patch

loader = importlib.machinery.SourceFileLoader("dfr_volume", str(Path(__file__).with_name("dfr-volume")))
spec = importlib.util.spec_from_loader(loader.name, loader)
volume = importlib.util.module_from_spec(spec)
loader.exec_module(volume)


class FakeCommands:
    def __init__(self):
        self.outputs = {"speakers": 100, "headphones": 25}
        self.selected = "speakers"
        self.muted = True
        self.calls = []

    def __call__(self, *args):
        self.calls.append(args)
        if args == ("omarchy-audio-output-sink",):
            return self.selected
        self.sink = args[2]
        if args[1] == "get-sink-volume":
            return f"Volume: left: 123 / {self.outputs[self.sink]}% / 0 dB, right: 123 / {self.outputs[self.sink]}% / 0 dB"
        if args[1] == "get-sink-mute":
            return "Mute: " + ("yes" if self.muted else "no")
        if args[1] == "set-sink-volume":
            self.outputs[self.sink] = int(args[3][:-1])
        elif args[1] == "set-sink-mute":
            self.muted = not self.muted if args[3] == "toggle" else args[3] == "1"
        else:
            raise AssertionError(args)
        return ""


class VolumeTests(unittest.TestCase):
    def test_zero_is_absolute_and_maximum_and_intermediates_reach_selected_output(self):
        fake = FakeCommands()
        with patch.object(volume, "command", fake):
            mixer = volume.Mixer()
            self.assertEqual(mixer.dispatch("get"), "ok 100 1")
            for percent in (0, 1, 25, 65, 100, 129, 130, 0):
                self.assertEqual(mixer.dispatch(f"set {percent}"), f"ok {percent} 0")
                self.assertEqual(fake.outputs["speakers"], percent)
            fake.selected = "headphones"
            self.assertEqual(mixer.dispatch("set 0"), "ok 0 0")
            self.assertEqual(fake.outputs["headphones"], 0)
            self.assertEqual(fake.outputs["speakers"], 0)
        self.assertIn(("pactl", "set-sink-volume", "speakers", "0%"), fake.calls)

    def test_invalid_protocol_never_touches_mixer(self):
        fake = FakeCommands()
        with patch.object(volume, "command", fake):
            for request in ("set -1", "set 131", "set 0; echo hi", "set 000", "set 1\n", "adjust 130", "adjust 5", "mute", "start", ""):
                with self.assertRaises(ValueError):
                    volume.Mixer().dispatch(request)
        self.assertEqual(fake.calls, [])

    def test_failed_absolute_write_does_not_return_success_or_unmute(self):
        fake = FakeCommands()
        def fail_write(*args):
            if args[:2] == ("pactl", "set-sink-volume"):
                raise subprocess.CalledProcessError(1, args)
            return fake(*args)
        with patch.object(volume, "command", fail_write):
            with self.assertRaises(subprocess.CalledProcessError):
                volume.Mixer().dispatch("set 0")
        self.assertEqual(fake.outputs["speakers"], 100)
        self.assertTrue(fake.muted)
        self.assertFalse(any(call[:2] == ("pactl", "set-sink-mute") for call in fake.calls))

    def test_socket_protocol_is_isolated_and_unauthorized_peer_cannot_set_volume(self):
        # A private socketpair cannot name or connect to either deployed
        # abstract receiver; all mixer commands below use a synthetic adapter.
        fake = FakeCommands()
        for allowed in ({os.getuid()}, set()):
            client, server = socket.socketpair()
            errors = []
            def respond():
                try:
                    with server:
                        volume.handle(server, volume.Mixer(), allowed)
                except Exception as error:
                    errors.append(error)
            with patch.object(volume, "command", fake):
                thread = threading.Thread(target=respond)
                with client:
                    client.sendall(b"set 0")
                    client.shutdown(socket.SHUT_WR)
                    thread.start()
                    try:
                        reply = client.recv(64)
                    except ConnectionResetError:
                        reply = b""
                thread.join(3)
                self.assertFalse(thread.is_alive())
                self.assertEqual(errors, [])
                self.assertEqual(reply, b"ok 0 0" if allowed else b"")
            if allowed:
                fake.outputs["speakers"] = 100
                fake.calls.clear()
            else:
                self.assertEqual(fake.outputs["speakers"], 100)
                self.assertEqual(fake.calls, [])


if __name__ == "__main__":
    unittest.main()
