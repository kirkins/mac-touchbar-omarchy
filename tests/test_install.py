"""Execute installation in disposable staging; never install/start on host."""
import configparser
import json
import os
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
        for name in ("README.md", "INSTALL.md"):
            self.assertTrue((self.stage / "usr/share/doc/tiny-dfr" / name).is_file())
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
        self.assertTrue((self.stage / "etc/udev/rules.d/99-touchbar-tiny-dfr.rules").is_file())
        self.assertFalse((self.stage / "var").exists())
        self.assertFalse((self.stage / "home").exists())

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
