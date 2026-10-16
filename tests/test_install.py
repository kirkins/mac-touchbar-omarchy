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
        for name in ("systemctl", "udevadm", "sudo", "cargo"):
            path = guards / name
            path.write_text("#!/bin/sh\nexit 91\n")
            path.chmod(0o755)
        self.environment = dict(os.environ, PATH=str(guards) + os.pathsep + os.environ["PATH"],
                                DFR_TEST_LOG=str(self.log))

    def install(self):
        return subprocess.run(["make", "install", f"DESTDIR={self.stage}",
                               f"BINARY_DIR={self.binary}"], cwd=REPO,
                              env=self.environment, capture_output=True, text=True)

    def test_complete_staging_keeps_existing_overlay_and_launches_native_entry(self):
        overlay = self.stage / "etc/tiny-dfr/config.toml"
        overlay.parent.mkdir(parents=True)
        overlay.write_text("ActiveBrightness = 201\n")
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(overlay.read_text(), "ActiveBrightness = 201\n")
        assets = self.stage / "usr/share/tiny-dfr"
        for source in (REPO / "share/tiny-dfr").iterdir():
            if source.suffix in (".toml", ".svg"):
                self.assertEqual((assets / source.name).read_bytes(), source.read_bytes())
        for name in ("LICENSE", "LICENSE.material"):
            self.assertEqual((self.stage / "usr/share/licenses/tiny-dfr" / name).read_bytes(),
                             (REPO / name).read_bytes())
        docs = self.stage / "usr/share/doc/tiny-dfr"
        for source in [REPO / "README.md", REPO / "INSTALL.md", *REPO.glob("integrations/*/README.md")]:
            installed = docs / source.relative_to(REPO)
            self.assertEqual(installed.read_bytes(), source.read_bytes())
            for target in re.findall(r"\]\(([^)#]+)(?:#[^)]*)?\)", installed.read_text()):
                if "://" not in target:
                    self.assertTrue((installed.parent / target).is_file(), f"{installed}: {target}")
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
        self.assertTrue((self.stage / "usr/bin/tiny-dfr").is_file())
        self.assertTrue((self.stage / "usr/local/bin/touchbar-capture-user").is_file())
        for name in ("99-touchbar-tiny-dfr.rules", "99-touchbar-seat.rules"):
            self.assertEqual((self.stage / "etc/udev/rules.d" / name).read_bytes(),
                             (REPO / "etc/udev/rules.d" / name).read_bytes())
        for source in (REPO / "etc/systemd/system").glob("systemd-backlight@*"):
            self.assertFalse((self.stage / "etc/systemd/system" / source.name).exists(),
                             "unqualified empty backlight masks must not be installed")
        self.assertFalse((self.stage / "var").exists())
        self.assertFalse((self.stage / "home").exists())

    def user_install(self, home="/home/fixture"):
        return subprocess.run(["make", "install-user-session", f"DESTDIR={self.stage}",
                               f"USER_HOME={home}"], cwd=REPO, env=self.environment,
                              capture_output=True, text=True)

    @unittest.skipIf(os.getuid() == 0, "receiver setup must run as an ordinary user")
    def test_user_session_staging_is_explicit_owned_copy_only_and_non_overwriting(self):
        for bad in ("", "relative", "/", "/home/../fixture", "/home/./fixture"):
            result = self.user_install(bad)
            self.assertNotEqual(result.returncode, 0, bad)
            self.assertFalse(self.stage.exists())
        home = self.stage / "home/fixture"
        draft = home / ".config/dfr-deck/config.toml"
        draft.parent.mkdir(parents=True)
        draft.write_text("private preexisting draft")
        result = self.user_install()
        self.assertEqual(result.returncode, 0, result.stderr)
        for relative, source, mode in (
                (".local/bin/touchbar-capture", "touchbar-capture", 0o755),
                (".config/systemd/user/touchbar-capture.service", "touchbar-capture.service", 0o644)):
            path = home / relative
            self.assertEqual(path.read_bytes(), (REPO / "integrations/capture" / source).read_bytes())
            self.assertEqual(path.stat().st_uid, os.getuid())
            self.assertEqual(path.stat().st_mode & 0o777, mode)
        self.assertEqual(draft.read_text(), "private preexisting draft")
        self.assertFalse((home / ".config/systemd/user/graphical-session.target.wants").exists())
        self.assertFalse((self.stage / "usr").exists())
        result = self.user_install()
        self.assertNotEqual(result.returncode, 0, "never overwrite an existing personal helper/unit")

    @unittest.skipIf(os.getuid() == 0, "receiver setup must run as an ordinary user")
    def test_existing_unit_or_dangling_symlink_refuses_before_any_copy(self):
        unit = self.stage / "home/fixture/.config/systemd/user/touchbar-capture.service"
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

    def test_missing_native_binary_refuses_before_installing_any_files(self):
        (self.binary / "dfr-configurator").unlink()
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.stage.exists())

    def test_staged_unit_prepares_only_theme_state_and_preserves_sandbox(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        unit = configparser.ConfigParser(interpolation=None, strict=False)
        unit.read(self.stage / "etc/systemd/system/tiny-dfr.service")
        service = unit["Service"]
        self.assertEqual(service["ReadWritePaths"], "/var/lib/tiny-dfr")
        for key, value in {
            "NoNewPrivileges": "true", "ProtectSystem": "strict", "ProtectHome": "true",
            "PrivateTmp": "true", "PrivateIPC": "true", "ProtectKernelTunables": "true",
            "ProtectKernelModules": "true", "ProtectKernelLogs": "true",
            "ProtectControlGroups": "strict", "RestrictAddressFamilies": "AF_UNIX AF_NETLINK",
            "RestrictNamespaces": "true", "RestrictSUIDSGID": "true",
        }.items():
            self.assertEqual(service[key], value)
        argv = shlex.split(service["ExecStartPre"])
        self.assertEqual(argv, ["/usr/bin/install", "-d", "-o", "nobody", "-g", "nobody",
                                "-m", "0755", "/var/lib/tiny-dfr"])
        # Execute prestart through an adapter; owner/group are simulated only.
        adapter = self.root / "install-adapter"
        adapter.write_text("#!/usr/bin/python3\nimport json, os, pathlib, sys\n"
                           "pathlib.Path(os.environ['DFR_TEST_LOG']).write_text(json.dumps(sys.argv[1:]))\n"
                           "pathlib.Path(os.environ['DFR_STATE_STAGE']).mkdir(parents=True)\n")
        adapter.chmod(0o755)
        state = self.stage / "var/lib/tiny-dfr"
        subprocess.run([str(adapter)] + argv[1:], check=True,
                       env=dict(self.environment, DFR_STATE_STAGE=str(state)))
        self.assertEqual(json.loads(self.log.read_text()), argv[1:])
        self.assertTrue(state.is_dir())
