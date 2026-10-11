"""Behavioral regression tests: no session bus, player, or socket state."""

import importlib.machinery
import importlib.util
import itertools
import os
from pathlib import Path
import socket
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


np = load("dfr-now-playing")


class FakeProps:
    """One MPRIS player's properties, canned - shaped the way
    dbus.Interface consumes a proxy object."""

    def __init__(self, status="Playing", title="Song", artist=("Artist",)):
        self.status = status
        self.meta = {}
        if title is not None:
            self.meta["xesam:title"] = title
        if artist is not None:
            self.meta["xesam:artist"] = artist

    def get_dbus_method(self, method, dbus_interface=None):
        # dbus.Interface may probe with its default interface first;
        # the real interface check happens inside Get.
        assert method == "Get"
        return self.Get

    def Get(self, interface, property_):
        assert interface == np.PLAYER_IFACE
        if property_ == "PlaybackStatus":
            return self.status
        if property_ == "Metadata":
            return self.meta
        raise ValueError(property_)


class FakeManager:
    """The DBus name listing, canned."""

    def __init__(self, names):
        self.names = names

    def ListNames(self):
        return list(self.names)


class FakeBus:
    """A session bus whose objects come from a table; unknown players
    raise the DBus error a dying player would."""

    def __init__(self, names, players):
        self.names = FakeManager(names)
        self.players = players

    def get_object(self, name, path):
        if name == "org.freedesktop.DBus":
            assert path == "/org/freedesktop/DBus"
            return self.names
        assert path == np.MPRIS_PATH, path
        try:
            return self.players[name]
        except KeyError:
            import dbus

            raise dbus.DBusException("no such player")


def bus_with(*players):
    """A bus carrying the given players under generated MPRIS names."""
    names = [f"org.mpris.MediaPlayer2.fake{i}" for i in range(len(players))]
    return FakeBus(names + ["org.freedesktop.DBus"],
                   dict(zip(names, players)))


class SanitizeTests(unittest.TestCase):
    def test_metadata_is_reduced_to_single_line_display_text(self):
        # The real-world case: a soft hyphen inside an artist name.
        self.assertEqual(np.sanitize("Weaban \u00ad"), "Weaban")
        # Control characters, the field separator, and format characters
        # never reach the wire's payload fields.
        self.assertEqual(np.sanitize("A\x07B\x1fC\u200bD"), "ABCD")
        # Whitespace runs collapse; real text survives untouched.
        self.assertEqual(np.sanitize("  many   spaces \t here "), "many spaces here")
        self.assertEqual(np.sanitize("Sigur Rós — 🎵 / アーティスト"),
                         "Sigur Rós — 🎵 / アーティスト")
        # The field cap bounds absurd metadata.
        self.assertEqual(len(np.sanitize("x" * 500)), np.MAX_FIELD_CHARS)

    def test_metadata_fields_join_lists_and_tolerate_missing_values(self):
        meta = {"xesam:title": "Song", "xesam:artist": ("A", "B")}
        self.assertEqual(np.metadata_field(meta, "xesam:title"), "Song")
        self.assertEqual(np.metadata_field(meta, "xesam:artist"), "A, B")
        self.assertEqual(np.metadata_field({}, "xesam:title"), "")
        # Non-string scalars (some players ship integers) stringify.
        self.assertEqual(np.metadata_field({"xesam:title": 42}, "xesam:title"), "42")


class SelectionTests(unittest.TestCase):
    def test_a_playing_player_wins_over_paused_ones(self):
        paused = FakeProps(status="Paused", title="Old")
        playing = FakeProps(status="Playing", title="Now")
        track = np.current_track(bus_with(paused, playing))
        self.assertEqual(track, ("playing", "Now", "Artist"))

    def test_without_a_playing_player_the_first_usable_one_stands(self):
        # Its paused state is what the daemon sees: the row's empty
        # state, never the track pretending to play.
        track = np.current_track(bus_with(FakeProps(status="Paused", title="Old")))
        self.assertEqual(track, ("paused", "Old", "Artist"))
        track = np.current_track(bus_with(FakeProps(status="Stopped", title="Held")))
        self.assertEqual(track, ("stopped", "Held", "Artist"))

    def test_players_without_usable_titles_are_skipped(self):
        blank = FakeProps(status="Playing", title="   ")
        titled = FakeProps(status="Paused", title="Real")
        self.assertEqual(np.current_track(bus_with(blank, titled)),
                         ("paused", "Real", "Artist"))
        # Nobody usable: nothing to show.
        self.assertIsNone(np.current_track(bus_with(blank)))
        self.assertIsNone(np.current_track(bus_with()))

    def test_a_dying_player_is_skipped_not_fatal(self):
        import dbus

        # A listed name whose player cannot be read: skipped, never
        # fatal - the remaining usable player still answers.
        bus = bus_with(FakeProps(status="Paused", title="Fine"))
        bus.names.names = ["org.mpris.MediaPlayer2.ghost",
                           "org.mpris.MediaPlayer2.fake0"]
        self.assertEqual(np.current_track(bus), ("paused", "Fine", "Artist"))
        # A bus failure raises out of the read, and the wire answer is
        # error - never a stale or half reply.
        broken = bus_with(FakeProps())
        with patch.object(broken, "get_object",
                          side_effect=dbus.DBusException("bus gone")):
            self.assertRaises(dbus.DBusException, np.current_track, broken)
            self.assertEqual(np.reply("get", broken), "error")


class ReplyTests(unittest.TestCase):
    def test_the_wire_answers_are_exact_and_finite(self):
        self.assertEqual(np.reply("get", bus_with(FakeProps())),
                         "ok playing\x1fSong\x1fArtist")
        # An empty artist field is no artist, not an empty caption.
        self.assertEqual(np.reply("get", bus_with(FakeProps(artist=()))),
                         "ok playing\x1fSong\x1f")
        self.assertEqual(np.reply("get", bus_with(FakeProps(status="Paused"))),
                         "ok paused\x1fSong\x1fArtist")
        self.assertEqual(np.reply("get", bus_with()), "idle")
        # Unknown requests never touch the bus.
        for bad in ("start", "GET", "get extra", "", "set playing"):
            self.assertEqual(np.reply(bad, bus_with()), "error")

    def test_a_bus_failure_answers_error(self):
        import dbus

        with patch.object(np, "session_bus", side_effect=dbus.DBusException("down")):
            # The failure surfaces at the read, as error - never a stale
            # or half reply.
            self.assertEqual(np.reply("get", None), "error")


class ReceiverTests(unittest.TestCase):
    counter = itertools.count()

    def setUp(self):
        self.bus = bus_with(FakeProps(status="Playing", title="Wire", artist=("Band",)))
        self.address = "\0dfr-now-playing-test-%d-%d" % (
            os.getpid(), next(ReceiverTests.counter))
        self.allowed = {os.getuid(), 65534}
        # The patch must cover request handling, not just server setup:
        # each request opens its own session bus through the same seam.
        patcher = patch.object(np, "session_bus", return_value=self.bus)
        patcher.start()
        self.addCleanup(patcher.stop)
        threading.Thread(
            target=np.serve, args=(self.address, self.allowed),
            daemon=True).start()
        deadline = time.monotonic() + 3
        while not self._socket_ready() and time.monotonic() < deadline:
            time.sleep(0.02)

    def _socket_ready(self):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as probe:
            try:
                probe.connect(self.address)
                return True
            except OSError:
                return False

    def request(self, payload):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.connect(self.address)
            client.sendall(payload.encode("ascii"))
            client.shutdown(socket.SHUT_WR)
            return client.recv(512).decode("ascii")

    def test_get_reports_the_playing_track_over_the_socket(self):
        self.assertEqual(self.request("get"), "ok playing\x1fWire\x1fBand")
        # Track changes land on the next read: the receiver holds no
        # state that could go stale.
        self.bus.players["org.mpris.MediaPlayer2.fake0"] = FakeProps(
            status="Playing", title="Next", artist=())
        self.assertEqual(self.request("get"), "ok playing\x1fNext\x1f")
        self.bus.players["org.mpris.MediaPlayer2.fake0"] = FakeProps(
            status="Paused", title="Next", artist=())
        self.assertEqual(self.request("get"), "ok paused\x1fNext\x1f")

    def test_malformed_requests_answer_error(self):
        for bad in ("start", "ON", "get extra", ""):
            self.assertEqual(self.request(bad), "error")

    def test_a_caller_with_the_wrong_uid_is_refused(self):
        # The receiver checks SO_PEERCRED, which the kernel sets to this
        # process's own UID; an allowlist that excludes it must refuse
        # the request rather than touch the bus.
        address = self.address + "-refused"
        threading.Thread(
            target=np.serve, args=(address, set()), daemon=True).start()
        deadline = time.monotonic() + 3
        while True:
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as other:
                    other.settimeout(2)
                    other.connect(address)
                    other.sendall(b"get")
                    reply = other.recv(512).decode("ascii")
                break
            except OSError:
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.02)
        self.assertEqual(reply, "error")


if __name__ == "__main__":
    unittest.main()
