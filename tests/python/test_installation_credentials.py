"""Private persisted credential retrieval rejects unsafe files without mutation."""
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import installation_credentials as credentials


class BuyerCredentialTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "install"
        self.root.mkdir(mode=0o700)
        self.state = self.root / "state"
        self.state.mkdir(mode=0o700)
        self.marker = self.root / "installation.json"
        self.marker.write_text(json.dumps({"schema_version": 1, "host_target": "fixture-target"}))
        self.config = self.state / "config.json"
        self.write_config({"api_key": "a" * 64, "api_hmac_secret": "b" * 64})

    def write_config(self, payload):
        self.config.write_text(json.dumps(payload))
        self.config.chmod(0o600)

    def test_reads_saved_private_values_and_never_writes(self):
        before = [(path, path.read_bytes(), path.stat().st_mtime_ns)
                  for path in (self.marker, self.config)]
        with patch.dict(os.environ, {"MACBOT_API_KEY": "unrelated-environment-override"}):
            self.assertEqual(credentials.read_credentials(self.root), ("a" * 64, "b" * 64))
        for path, contents, modified in before:
            self.assertEqual(path.read_bytes(), contents)
            self.assertEqual(path.stat().st_mtime_ns, modified)

    def test_nonprivate_installation_state_or_config_refused(self):
        for path, public_mode, private_mode in ((self.root, 0o755, 0o700),
                                                (self.state, 0o750, 0o700),
                                                (self.config, 0o640, 0o600)):
            with self.subTest(path=path.name):
                path.chmod(public_mode)
                with self.assertRaisesRegex(ValueError, "private"):
                    credentials.read_credentials(self.root)
                path.chmod(private_mode)

    def test_symlink_installation_state_config_and_marker_refused(self):
        for path in (self.root, self.state, self.config, self.marker):
            with self.subTest(path=path.name):
                moved = path.with_name(path.name + "-real")
                path.rename(moved)
                path.symlink_to(moved, target_is_directory=moved.is_dir())
                with self.assertRaises(OSError):
                    credentials.read_credentials(self.root)
                path.unlink()
                moved.rename(path)

    def test_nonregular_fifo_config_refused_without_blocking(self):
        self.config.unlink()
        os.mkfifo(self.config, mode=0o600)
        with self.assertRaisesRegex(ValueError, "regular"):
            credentials.read_credentials(self.root)

    def test_foreign_owner_refused(self):
        with patch.object(credentials.os, "getuid", return_value=os.getuid() + 1):
            with self.assertRaisesRegex(ValueError, "owned by you"):
                credentials.read_credentials(self.root)

    def test_oversized_invalid_and_nonobject_json_refused(self):
        for payload in (b" " * (credentials.MAX_JSON_BYTES + 1), b"{invalid", b"[]"):
            with self.subTest(length=len(payload)):
                self.config.write_bytes(payload)
                with self.assertRaises(ValueError):
                    credentials.read_credentials(self.root)

    def test_unsupported_markers_and_missing_credentials_refused(self):
        for marker in ({}, {"schema_version": True, "host_target": "fixture-target"},
                       {"schema_version": 2, "host_target": "fixture-target"},
                       {"schema_version": 1, "host_target": ""}):
            self.marker.write_text(json.dumps(marker))
            with self.assertRaisesRegex(ValueError, "unsupported"):
                credentials.read_credentials(self.root)
        self.marker.write_text(json.dumps({"schema_version": 1, "host_target": "fixture-target"}))
        for value in (None, "", "secret\nterminal-command", "x" * 513, "white space", 42):
            self.write_config({"api_key": "safe-key", "api_hmac_secret": value})
            with self.assertRaisesRegex(ValueError, "missing or invalid"):
                credentials.read_credentials(self.root)


if __name__ == "__main__":
    unittest.main()
