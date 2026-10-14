#!/usr/bin/env python3
"""Offline regression checks for private initialization and safe runtime arguments."""
import importlib.util
import fcntl
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch, Mock

ROOT = Path(__file__).resolve().parents[2]


def module(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / "scripts" / (name + ".py"))
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


entry = module("container_entrypoint")
commands = module("container")


class ResearchContainerTests(unittest.TestCase):
    def setUp(self):
        self.original_umask = os.umask(0o077)
        self.temporary = tempfile.TemporaryDirectory()
        self.directory = Path(self.temporary.name)
        self.state = self.directory / "state"
        self.state.mkdir(mode=0o700)
        self.template = ROOT / "ops/container-research.json"

    def tearDown(self):
        self.temporary.cleanup()
        os.umask(self.original_umask)

    def test_fresh_credentials_persist_and_existing_values_are_never_replaced(self):
        first = entry.initialize_state(self.state, self.template)
        raw = (self.state / "config.json").read_bytes()
        second = entry.initialize_state(self.state, self.template)
        self.assertEqual(first, second)
        self.assertEqual(raw, (self.state / "config.json").read_bytes())
        self.assertEqual(len(first["api_key"]), 64)
        self.assertNotEqual(first["api_key"], first["api_hmac_secret"])
        self.assertEqual((self.state / "config.json").stat().st_mode & 0o777, 0o600)

    def test_unrelated_volume_and_config_symlink_are_refused_without_overwrite(self):
        private = self.directory / "private.json"
        private.write_text('"unchanged"')
        (self.state / "config.json").symlink_to(private)
        with self.assertRaises(ValueError):
            entry.initialize_state(self.state, self.template)
        self.assertEqual(private.read_text(), '"unchanged"')
        (self.state / "config.json").unlink()
        (self.state / "trades.sqlite").write_bytes(b"unrelated prior state")
        with self.assertRaises(ValueError):
            entry.initialize_state(self.state, self.template)
        self.assertFalse((self.state / "config.json").exists())

    def test_reused_live_config_or_enabled_auth_bypass_is_refused_unchanged(self):
        config = entry.initialize_state(self.state, self.template)
        for changes in ({"mode": "live"}, {"auth_required": False}, {"maker_live_quoting": True}, {"private_rpc_url": "https://example.invalid"}):
            candidate = config | changes
            path = self.state / "config.json"
            path.write_text(json.dumps(candidate))
            raw = path.read_bytes()
            with self.assertRaises(ValueError):
                entry.initialize_state(self.state, self.template)
            self.assertEqual(path.read_bytes(), raw)

    def test_runtime_environment_ignores_wallet_dotenv_and_operation_injection(self):
        with patch.dict(os.environ, {"PRIVATE_KEY": "private", "MACBOT_BACKTEST_ONLY": "0", "MACBOT_ONNX_RUNTIME_LIB": "/bad/lib.so", "MACBOT_CONFIG": "/bad/config"}):
            environment = entry.runtime_environment(Path("/opt/macbot"), Path("/var/lib/macbot"))
        self.assertNotIn("PRIVATE_KEY", environment)
        self.assertEqual(environment["MACBOT_BACKTEST_ONLY"], "1")
        self.assertEqual(environment["MACBOT_DOTENV_PATH"], "/opt/macbot/empty.env")
        self.assertEqual(environment["MACBOT_CONFIG"], "/var/lib/macbot/config.json")
        self.assertEqual(environment["MACBOT_ONNX_RUNTIME_LIB"], "/opt/macbot/runtime/libonnxruntime.so")

    def test_competing_initializer_has_a_bounded_deadline_without_config_writes(self):
        descriptor = os.open(self.state / ".initialize.lock", os.O_CREAT | os.O_RDWR, 0o600)
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX)
            with self.assertRaisesRegex(ValueError, "another initializer"):
                entry.initialize_state(self.state, self.template, lock_timeout=0.01)
            self.assertFalse((self.state / "config.json").exists())
        finally:
            os.close(descriptor)

    def test_unmarked_or_wrong_layout_existing_config_is_preserved_and_refused(self):
        entry.initialize_state(self.state, self.template)
        layout = self.state / ".state-layout.json"
        raw = (self.state / "config.json").read_bytes()
        layout.unlink()
        with self.assertRaisesRegex(ValueError, "state layout marker"):
            entry.initialize_state(self.state, self.template)
        layout.write_text('{"application":"unrelated","layout_version":1}')
        with self.assertRaisesRegex(ValueError, "state layout"):
            entry.initialize_state(self.state, self.template)
        self.assertEqual((self.state / "config.json").read_bytes(), raw)

    def test_root_and_unowned_state_are_refused(self):
        with patch.object(os, "geteuid", return_value=0):
            with self.assertRaises(ValueError):
                entry.initialize_state(self.state, self.template)
        with patch.object(os, "geteuid", return_value=self.state.stat().st_uid + 1):
            with self.assertRaises(ValueError):
                entry.initialize_state(self.state, self.template)

    def test_context_directory_allowances_remain_bounded_to_reviewed_files(self):
        rules = (ROOT / ".dockerignore").read_text().splitlines()
        for index, rule in enumerate(rules):
            if rule.startswith("!") and rule.endswith("/"):
                self.assertNotIn("*", rule, "wildcard directory allowances include unreviewed children")
                self.assertEqual(rules[index + 1], rule[1:] + "**",
                    "directory allowances must immediately deny unreviewed descendants")
        required = [path.relative_to(ROOT).as_posix() for path in (ROOT / "src").rglob("*.rs")]
        inventory = json.loads((ROOT / "static/vendor/asset-manifest.json").read_text())
        required += ["static/vendor/" + item["path"] for item in inventory["files"]]
        for relative in required:
            parent = Path(relative).parent
            while str(parent) != ".":
                self.assertIn("!" + parent.as_posix() + "/", rules,
                    "reviewed nested source must remain traversable")
                parent = parent.parent
        self.assertNotIn("!static/vendor/**", rules)
        for item in inventory["files"]:
            self.assertIn("!static/vendor/" + item["path"], rules)

    def test_podman_build_keeps_declared_healthcheck_in_docker_format(self):
        absent = Mock(returncode=1)
        with patch("sys.argv", ["container.py", "build"]), patch.object(commands.shutil, "which", return_value="podman"), patch.object(commands.subprocess, "run", side_effect=[absent, Mock(returncode=0)]) as run:
            self.assertEqual(commands.main(), 0)
        invocation = run.call_args_list[1].args[0]
        self.assertIn("--format=docker", invocation)
        self.assertIn("--network=slirp4netns", invocation)
        self.assertEqual(invocation[-1], ".")

    def test_runtime_arguments_publish_only_loopback_without_host_secrets(self):
        for engine in ("podman", "docker"):
            command = commands.run_arguments(engine, "image", "container", "private-state", 18084)
            self.assertIn("127.0.0.1:18084:8000", command)
            self.assertIn("--read-only", command)
            self.assertIn("--cap-drop=ALL", command)
            self.assertIn("--security-opt=no-new-privileges", command)
            self.assertNotIn("--env-file", command)
            self.assertFalse(any("privileged" in value for value in command))
            self.assertIn("type=volume,source=private-state,destination=/var/lib/macbot", command)


if __name__ == "__main__":
    unittest.main()
