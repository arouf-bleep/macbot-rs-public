"""Local setup isolates data, defaults to research and never replaces a operator install."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from contextlib import contextmanager

import install_local as setup


class LocalSetupTests(unittest.TestCase):
    @contextmanager
    def fixture(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            for directory in ("static", "models", "ops", "scripts"):
                shutil.copytree(setup.ROOT / directory, source / directory)
            for name in ("config.json", "LICENSE.md", "THIRD_PARTY_NOTICES.md"):
                shutil.copy2(setup.ROOT / name, source / name)
            binary = root / "compiled-macbot"
            binary.write_bytes(b"#!/bin/sh\nprintf '%s\\n' ENGINE_STARTED\n")
            binary.chmod(0o700)
            runtime = root / "libruntime.so"
            runtime.write_bytes(b"fixture library")
            (root / "runtime-manifest.json").write_text("fixture manifest")
            manifest = root / "manifest.json"
            manifest.write_text(json.dumps({"targets":{"fixture-target":{
                "library_name":runtime.name,"library_bytes":runtime.stat().st_size,
                "library_sha256":hashlib.sha256(runtime.read_bytes()).hexdigest(),"notices":[]}}}))
            stage = root / "installation"
            stage.mkdir(mode=0o700)
            with patch.object(setup, "MANIFEST", manifest), patch.object(setup, "host_target", return_value="fixture-target"):
                yield source, stage, binary, runtime

    def test_fresh_install_does_not_copy_local_config_or_unknown_private_fields(self):
        with self.fixture() as (source, stage, binary, runtime):
            local = json.loads((source / "config.json").read_text())
            local.update(auth_required=False, mode="live", shadow_trading=True,
                         signal_weights_path="/private/local/research/weights.json",
                         local_unknown_secret="ARTIFICIAL_PRIVATE_CONFIG_SENTINEL")
            (source / "config.json").write_text(json.dumps(local))
            setup.assemble(source, stage, binary, runtime)
            actual = json.loads((stage / "state/config.json").read_text())
            self.assertIs(actual["auth_required"], True)
            self.assertEqual(actual["mode"], "simulation")
            self.assertIs(actual["auto_trading"], False)
            self.assertIs(actual["shadow_trading"], False)
            self.assertEqual(actual["signal_weights_path"], "")
            self.assertNotIn("local_unknown_secret", actual)
            self.assertNotIn("ARTIFICIAL_PRIVATE_CONFIG_SENTINEL", (stage / "state/config.json").read_text())

    def test_install_uses_reviewed_template_without_local_config(self):
        with self.fixture() as (source, stage, binary, runtime):
            (source / "config.json").unlink()
            setup.assemble(source, stage, binary, runtime)
            self.assertIs(json.loads((stage / "state/config.json").read_text())["auth_required"], True)

    def test_unsafe_research_template_is_refused(self):
        with self.fixture() as (source, stage, binary, runtime):
            template = source / "ops/container-research.json"
            config = json.loads(template.read_text())
            config["auth_required"] = False
            template.write_text(json.dumps(config))
            with self.assertRaisesRegex(ValueError, "safe installation defaults"):
                setup.assemble(source, stage, binary, runtime)
            self.assertFalse((stage / "installation.json").exists())

    def test_each_persistent_install_gets_distinct_private_credentials(self):
        credentials = []
        for _ in range(2):
            with self.fixture() as (source, stage, binary, runtime):
                setup.assemble(source, stage, binary, runtime)
                actual = json.loads((stage / "state/config.json").read_text())
                pair = (actual["api_key"], actual["api_hmac_secret"])
                for value in pair:
                    self.assertRegex(value, r"^[0-9a-f]{64}$")
                self.assertNotEqual(*pair)
                credentials.append(pair)
        self.assertNotEqual(credentials[0], credentials[1])

    def test_vendor_change_after_inventory_check_is_refused_before_publication(self):
        with self.fixture() as (source, stage, binary, runtime):
            inventory = json.loads((source / "static/vendor/asset-manifest.json").read_text())
            vendor = source / "static/vendor" / inventory["files"][0]["path"]
            original_copy = setup.copy_regular
            def mutate_before_copy(resource, destination):
                if resource == vendor:
                    resource.write_bytes(b"ARTIFICIAL_CHANGED_VENDOR_BYTES")
                original_copy(resource, destination)
            with patch.object(setup, "copy_regular", side_effect=mutate_before_copy):
                with self.assertRaisesRegex(ValueError, "installed dashboard vendor bytes"):
                    setup.assemble(source, stage, binary, runtime)
            self.assertFalse((stage / "installation.json").exists())

    def test_credential_command_reads_saved_keys_without_starting_or_writing_state(self):
        with self.fixture() as (source, stage, binary, runtime):
            setup.assemble(source, stage, binary, runtime)
            config_path = stage / "state/config.json"
            saved = config_path.read_bytes()
            config = json.loads(saved)
            database = stage / "state/trades.sqlite"
            database.write_bytes(b"ARTIFICIAL_EXISTING_DATABASE_SENTINEL")
            before = {path.relative_to(stage).as_posix(): path.read_bytes()
                      for path in stage.rglob("*") if path.is_file()}
            environment = os.environ.copy()
            environment["PATH"] = str(Path(sys.executable).parent) + os.pathsep + environment.get("PATH", "")
            environment["MACBOT_BACKTEST_ONLY"] = "0"
            result = subprocess.run([str(stage / "run.sh"), "credentials"], capture_output=True,
                                    text=True, env=environment, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, f"API key: {config['api_key']}\nAPI secret: {config['api_hmac_secret']}\n")
            self.assertNotIn("ENGINE_STARTED", result.stdout)
            self.assertEqual(before, {path.relative_to(stage).as_posix(): path.read_bytes()
                                     for path in stage.rglob("*") if path.is_file()})
            self.assertEqual(config_path.read_bytes(), saved)

    def test_launcher_default_and_invalid_arguments_never_log_keys(self):
        with self.fixture() as (source, stage, binary, runtime):
            setup.assemble(source, stage, binary, runtime)
            config = json.loads((stage / "state/config.json").read_text())
            environment = os.environ.copy()
            environment.pop("MACBOT_BACKTEST_ONLY", None)
            for arguments, expected in (([], 0), (["serve"], 0), (["unknown"], 2),
                                        (["credentials", "extra"], 2), (["serve", "extra"], 2)):
                result = subprocess.run([str(stage / "run.sh"), *arguments], capture_output=True,
                                        text=True, env=environment, timeout=10)
                self.assertEqual(result.returncode, expected, result.stderr)
                output = result.stdout + result.stderr
                self.assertNotIn(config["api_key"], output)
                self.assertNotIn(config["api_hmac_secret"], output)
                if expected == 0:
                    self.assertIn("research mode", output)
                    self.assertIn("ENGINE_STARTED", output)
                else:
                    self.assertNotIn("ENGINE_STARTED", output)

    def test_existing_destination_and_failed_prerequisites_never_build_or_download(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            destination = root / "install"
            destination.mkdir()
            (destination / "cash.sqlite").write_bytes(b"existing evidence")
            with patch.object(setup, "build_engine") as build, patch.object(setup, "prepare_runtime") as runtime:
                with self.assertRaisesRegex(ValueError, "already exists"):
                    setup.install(destination, profile="debug", offline=False, archive=None)
                self.assertEqual((destination / "cash.sqlite").read_bytes(), b"existing evidence")
                with patch.object(setup, "inspect_environment", return_value={"status":"fail", "errors":["patched native runtime required"]}):
                    with self.assertRaisesRegex(ValueError, "prerequisites failed"):
                        setup.install(root / "new", profile="debug", offline=False, archive=None)
                build.assert_not_called()
                runtime.assert_not_called()
                self.assertFalse((root / "new").exists())

    def test_offline_missing_cache_never_reaches_downloader(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with patch.object(setup, "inspect_environment", return_value={"status":"pass"}), patch.object(setup, "default_cache", return_value=root / "empty-cache"), patch.object(setup, "prepare_runtime") as runtime:
                with self.assertRaisesRegex(ValueError, "offline runtime cache missing"):
                    setup.install(root / "install", profile="debug", offline=True, archive=None)
                runtime.assert_not_called()
                self.assertFalse((root / "install").exists())

    def test_resources_launcher_model_and_inventory_have_safe_boundaries(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            for directory in ("static", "models", "ops", "scripts"):
                shutil.copytree(setup.ROOT / directory, source / directory)
            for name in ("config.json", "LICENSE.md", "THIRD_PARTY_NOTICES.md"):
                shutil.copy2(setup.ROOT / name, source / name)
            (source / ".env").write_text("secret-token")
            (source / "static/unlisted-private.json").write_text("secret-token")
            (source / "models/private.onnx").write_bytes(b"private model")
            config = json.loads((source / "config.json").read_text())
            config.update(mode="live", api_key="secret-token", private_rpc_url="https://private-rpc", maker_live_quoting=True)
            (source / "config.json").write_text(json.dumps(config))
            binary = root / "compiled-macbot"
            binary.write_bytes(b"fixture executable")
            binary.chmod(0o700)
            runtime = root / "libruntime.so"
            runtime.write_bytes(b"fixture library")
            (root / "runtime-manifest.json").write_text("fixture manifest")
            manifest = root / "manifest.json"
            manifest.write_text(json.dumps({"targets":{"fixture-target":{"library_name":runtime.name,"library_bytes":runtime.stat().st_size,"library_sha256":hashlib.sha256(runtime.read_bytes()).hexdigest(),"notices":[]}}}))
            stage = root / "installation"
            stage.mkdir()
            with patch.object(setup, "MANIFEST", manifest), patch.object(setup, "host_target", return_value="fixture-target"):
                setup.assemble(source, stage, binary, runtime)
            installed = json.loads((stage / "state/config.json").read_text())
            self.assertEqual(installed["mode"], "simulation")
            self.assertEqual(installed["private_rpc_url"], "")
            self.assertFalse(installed["maker_live_quoting"])
            self.assertFalse(installed["live_partial_take_profit"])
            self.assertNotIn("secret-token", (stage / "state/config.json").read_text())
            self.assertFalse((stage / ".env").exists())
            self.assertFalse((stage / "static/unlisted-private.json").exists())
            self.assertFalse((stage / "models/private.onnx").exists())
            launcher = (stage / "run.sh").read_text()
            self.assertIn('MACBOT_BACKTEST_ONLY:-1', launcher)
            self.assertIn('MACBOT_BIND_ALL=0', launcher)
            self.assertIn('/state/trades.sqlite', launcher)
            inventory = json.loads((stage / "installation.json").read_text())
            for entry in inventory["files"]:
                self.assertEqual(hashlib.sha256((stage / entry["path"]).read_bytes()).hexdigest(), entry["sha256"])
            self.assertEqual(os.stat(stage / "state/config.json").st_mode & 0o777, 0o600)


if __name__ == "__main__":
    unittest.main()
