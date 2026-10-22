"""Execute installation in disposable staging; never install/start on host."""
import configparser
import json
import os
import re
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[1]
HELPERS = {"touchbar-capture": "capture", "dfr-volume": "audio", "dfr-brightness": "brightness",
           "dfr-now-playing": "now-playing", "dfr-viz": "viz"}


def unit_file(path):
    unit = configparser.ConfigParser(interpolation=None, strict=False, delimiters=("=",))
    unit.optionxform = str
    # systemd allows repeated keys (ExecStart= resets, then sets); keep the last.
    unit.read_string("\n".join(line for line in Path(path).read_text().splitlines()
                               if not line.startswith("#")))
    return unit


class InstallTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="dfr-install-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.stage = self.root / "stage"
        self.binary = self.root / "binaries"
        self.binary.mkdir()
        self.log = self.root / "launch.json"
        for name in ("tiny-dfr", "dfr-configurator"):
            path = self.binary / name
            path.write_text("#!/usr/bin/python3\nimport json, os, pathlib, sys\n"
                            "pathlib.Path(os.environ['DFR_TEST_LOG']).write_text(json.dumps(sys.argv))\n")
            path.chmod(0o755)
        guards = self.root / "guards"
        guards.mkdir()
        for name in ("systemctl", "udevadm", "sudo", "cargo", "systemd-sysusers"):
            path = guards / name
            path.write_text("#!/bin/sh\nexit 91\n")
            path.chmod(0o755)
        self.environment = dict(os.environ, PATH=str(guards) + os.pathsep + os.environ["PATH"],
                                DFR_TEST_LOG=str(self.log))

    def make(self, *targets_and_vars):
        return subprocess.run(["make", *targets_and_vars, f"DESTDIR={self.stage}",
                               f"BINARY_DIR={self.binary}"], cwd=REPO,
                              env=self.environment, capture_output=True, text=True)

    def install(self, *extra):
        return self.make("install", *extra)

    def staged_files(self):
        return sorted(str(p.relative_to(self.stage)) for p in self.stage.rglob("*") if not p.is_dir())

    def test_staging_installs_beside_the_package_and_keeps_existing_overlay(self):
        overlay = self.stage / "etc/tiny-dfr/config.toml"
        overlay.parent.mkdir(parents=True)
        overlay.write_text("ActiveBrightness = 201\n")
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(overlay.read_text(), "ActiveBrightness = 201\n")
        # Nothing the distribution's tiny-dfr package owns is touched.
        for owned in ("usr/bin/tiny-dfr", "usr/share/tiny-dfr", "usr/lib/udev",
                      "etc/udev", "usr/lib/systemd/system/tiny-dfr.service",
                      "etc/systemd/system/tiny-dfr.service"):
            self.assertFalse((self.stage / owned).exists(), owned)
        daemon = self.stage / "usr/local/libexec/tiny-dfr-multibar"
        self.assertEqual(daemon.read_bytes(), (self.binary / "tiny-dfr").read_bytes())
        self.assertEqual(daemon.stat().st_mode & 0o777, 0o755)
        self.assertTrue((self.stage / "usr/local/bin/touchbar-capture-user").is_file())
        cli = self.stage / "usr/local/bin/tiny-dfr-deck"
        self.assertEqual(os.readlink(cli), "/usr/local/libexec/tiny-dfr-multibar")
        data = self.stage / "usr/local/share/tiny-dfr-deck"
        for name in ("multibar.toml", "multibar-voice.toml", "themes.toml", "config.toml"):
            self.assertEqual((data / name).read_bytes(), (REPO / "share/tiny-dfr" / name).read_bytes())
        for name in ("LICENSE", "LICENSE.material"):
            self.assertEqual((self.stage / "usr/local/share/licenses/tiny-dfr-deck" / name).read_bytes(),
                             (REPO / name).read_bytes())
        self.assertEqual((self.stage / "etc/sysusers.d/tiny-dfr-deck.conf").read_bytes(),
                         (REPO / "etc/sysusers.d/tiny-dfr-deck.conf").read_bytes())
        self.assertFalse((self.stage / "var").exists())
        self.assertFalse((self.stage / "home").exists())

    def test_installed_docs_keep_every_relative_link_working(self):
        self.assertEqual(self.install().returncode, 0)
        docs = self.stage / "usr/local/share/doc/tiny-dfr-deck"
        sources = [REPO / "README.md", REPO / "INSTALL.md", REPO / "CHANGELOG.md",
                   *REPO.glob("docs/*.md"), *REPO.glob("integrations/*/README.md")]
        for source in sources:
            installed = docs / source.relative_to(REPO)
            self.assertEqual(installed.read_bytes(), source.read_bytes())
            for target in re.findall(r"\]\(([^)#\s]+)(?:#[^)]*)?\)", installed.read_text()):
                if "://" not in target and target.endswith((".md", ".png")):
                    self.assertTrue((installed.parent / target).is_file(), f"{installed}: {target}")

    def test_desktop_entry_launches_the_installed_settings_app(self):
        self.assertEqual(self.install().returncode, 0)
        entry = self.stage / "usr/local/share/applications/org.dfrdeck.Configurator.desktop"
        if shutil.which("desktop-file-validate"):
            validation = subprocess.run(["desktop-file-validate", str(entry)], capture_output=True, text=True)
            self.assertEqual(validation.returncode, 0, validation.stderr)
        desktop = configparser.ConfigParser(interpolation=None)
        desktop.read(entry)
        argv = shlex.split(desktop["Desktop Entry"]["Exec"])
        self.assertEqual(argv, ["/usr/local/bin/dfr-configurator"])
        argv[0] = str(self.stage / argv[0].lstrip("/"))
        subprocess.run(argv, env=self.environment, check=True)
        self.assertEqual(json.loads(self.log.read_text()), argv)
        self.assertEqual(Path(argv[0]).stat().st_mode & 0o777, 0o755)

    def test_fresh_install_seeds_the_full_deck_configuration(self):
        self.assertEqual(self.install().returncode, 0)
        self.assertEqual((self.stage / "etc/tiny-dfr/config.toml").read_bytes(),
                         (REPO / "share/tiny-dfr/multibar.toml").read_bytes())

    def test_drop_in_runs_the_deck_daemon_and_prepares_only_theme_state(self):
        self.assertEqual(self.install().returncode, 0)
        service = unit_file(self.stage / "etc/systemd/system/tiny-dfr.service.d/multibar.conf")["Service"]
        self.assertEqual(service["ExecStart"], "/usr/local/libexec/tiny-dfr-multibar")
        self.assertEqual(service["ReadWritePaths"], "/var/lib/tiny-dfr")
        # The packaged unit's sandbox is kept: the drop-in relaxes nothing else.
        self.assertEqual(sorted(service), ["ExecStart", "ExecStartPre", "ReadWritePaths"])
        prestart = service["ExecStartPre"]
        self.assertTrue(prestart.startswith("-"), "a missing account must not block startup")
        argv = shlex.split(prestart[1:])
        self.assertEqual(argv, ["/usr/bin/install", "-d", "-o", "tiny-dfr", "-g", "tiny-dfr",
                                "-m", "0755", "/var/lib/tiny-dfr"])

    def test_packager_prefix_uses_distribution_paths(self):
        result = self.install("PREFIX=/usr", "UNITDIR=/usr/lib/systemd/system",
                              "SYSUSERSDIR=/usr/lib/sysusers.d")
        self.assertEqual(result.returncode, 0, result.stderr)
        dropin = self.stage / "usr/lib/systemd/system/tiny-dfr.service.d/multibar.conf"
        self.assertEqual(unit_file(dropin)["Service"]["ExecStart"], "/usr/libexec/tiny-dfr-multibar")
        self.assertTrue((self.stage / "usr/lib/sysusers.d/tiny-dfr-deck.conf").is_file())
        desktop = configparser.ConfigParser(interpolation=None)
        desktop.read(self.stage / "usr/share/applications/org.dfrdeck.Configurator.desktop")
        self.assertEqual(desktop["Desktop Entry"]["Exec"], "/usr/bin/dfr-configurator")
        self.assertFalse((self.stage / "usr/local").exists())
        self.assertFalse((self.stage / "usr/bin/tiny-dfr").exists())

    def test_uninstall_removes_everything_but_the_user_configuration(self):
        self.assertEqual(self.install().returncode, 0)
        result = self.make("uninstall")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.staged_files(), ["etc/tiny-dfr/config.toml"])

    def test_missing_native_binary_refuses_before_installing_any_files(self):
        (self.binary / "dfr-configurator").unlink()
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.stage.exists())

    def user_install(self, target="install-user-session", home="/home/fixture"):
        return subprocess.run(["make", target, f"DESTDIR={self.stage}", f"USER_HOME={home}"],
                              cwd=REPO, env=self.environment, capture_output=True, text=True)

    @unittest.skipIf(os.getuid() == 0, "helper setup must run as an ordinary user")
    def test_user_session_copies_every_helper_but_voice_and_never_overwrites(self):
        for bad in ("", "relative", "/", "/home/../fixture", "/home/./fixture"):
            result = self.user_install(home=bad)
            self.assertNotEqual(result.returncode, 0, bad)
            self.assertFalse(self.stage.exists())
        home = self.stage / "home/fixture"
        draft = home / ".config/dfr-deck/config.toml"
        draft.parent.mkdir(parents=True)
        draft.write_text("private preexisting draft")
        result = self.user_install()
        self.assertEqual(result.returncode, 0, result.stderr)
        for name, directory in HELPERS.items():
            for relative, source, mode in (
                    (f".local/bin/{name}", name, 0o755),
                    (f".config/systemd/user/{name}.service", f"{name}.service", 0o644)):
                path = home / relative
                self.assertEqual(path.read_bytes(), (REPO / "integrations" / directory / source).read_bytes())
                self.assertEqual(path.stat().st_uid, os.getuid())
                self.assertEqual(path.stat().st_mode & 0o777, mode)
        self.assertFalse((home / ".local/bin/voice-dictate").exists())
        self.assertEqual(draft.read_text(), "private preexisting draft")
        self.assertFalse((home / ".config/systemd/user/graphical-session.target.wants").exists())
        self.assertFalse((self.stage / "usr").exists())
        result = self.user_install()
        self.assertNotEqual(result.returncode, 0, "never overwrite an existing personal helper/unit")
        self.assertEqual(self.user_install("uninstall-user-session").returncode, 0)
        self.assertFalse(any((home / ".local/bin").iterdir()))
        self.assertEqual(draft.read_text(), "private preexisting draft")

    @unittest.skipIf(os.getuid() == 0, "helper setup must run as an ordinary user")
    def test_existing_unit_or_dangling_symlink_refuses_before_any_copy(self):
        unit = self.stage / "home/fixture/.config/systemd/user/dfr-viz.service"
        unit.parent.mkdir(parents=True)
        unit.symlink_to("absent-personal-unit")
        result = self.user_install()
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(unit.is_symlink())
        self.assertFalse((self.stage / "home/fixture/.local").exists())

    def test_consumer_deck_is_voice_free_and_optional_profile_is_deliberate(self):
        import tomllib
        consumer = tomllib.loads((REPO / "share/tiny-dfr/multibar.toml").read_text())
        home = next(layer for layer in consumer["Layers"] if layer["Name"] == "home")
        self.assertEqual([key.get("Text") or key.get("Icon") or key.get("Battery") for key in home["Keys"]],
                         ["volume_up", "brightness_high", "play_pause", "F1-F12", "Media", "More", "both", "Settings"])
        self.assertEqual([home["Keys"][index]["Stretch"] for index in (0, 1, 6)], [2, 2, 2])
        for layer in consumer["Layers"]:
            self.assertNotEqual(layer["Name"], "recording")
            for key in layer["Keys"]:
                self.assertNotIn("voice", str(key.get("Action", "")))
                self.assertFalse(key.get("Waveform"))
        voice = tomllib.loads((REPO / "share/tiny-dfr/multibar-voice.toml").read_text())
        self.assertIn("recording", [layer["Name"] for layer in voice["Layers"]])


if __name__ == "__main__":
    unittest.main()
