"""Runtime setup failure must stop the actual operator/demo entrypoints."""
from __future__ import annotations

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class RuntimeWrapperTests(unittest.TestCase):
    def test_failed_installer_stops_each_real_wrapper_before_cargo(self):
        cases = (
            ("verify.sh", "MACBOT_VERIFY_PYTHON"),
            ("run_demo.sh", "MACBOT_DEMO_PYTHON"),
            ("verify_decision_parity.sh", "MACBOT_PARITY_PYTHON"),
        )
        for script, python_variable in cases:
            with self.subTest(script=script), tempfile.TemporaryDirectory(
                prefix="macbot-runtime-wrapper-test-"
            ) as temporary:
                directory = Path(temporary)
                commands = directory / "commands"
                commands.mkdir()
                installer = commands / "failed-installer"
                installer.write_text(
                    "#!/bin/sh\n"
                    "echo 'deliberate runtime installer failure' >&2\n"
                    "exit 7\n"
                )
                cargo = commands / "cargo"
                cargo.write_text(
                    "#!/bin/sh\n"
                    "echo ran > \"$MACBOT_WRAPPER_TEST_MARKER\"\n"
                    "exit 86\n"
                )
                installer.chmod(0o700)
                cargo.chmod(0o700)
                marker = directory / "downstream-ran"
                temporary_state = directory / "temporary-state"
                temporary_state.mkdir()
                environment = os.environ.copy()
                environment[python_variable] = str(installer)
                environment["MACBOT_WRAPPER_TEST_MARKER"] = str(marker)
                environment["TMPDIR"] = str(temporary_state)
                environment["PATH"] = str(commands) + os.pathsep + environment["PATH"]
                command = ["bash", str(ROOT / "scripts" / script)]
                if script == "verify_decision_parity.sh":
                    command.append(str(directory / "parity-output"))
                result = subprocess.run(
                    command,
                    cwd=ROOT,
                    env=environment,
                    text=True,
                    capture_output=True,
                    timeout=10,
                )
                self.assertEqual(result.returncode, 7, result.stderr)
                self.assertIn("deliberate runtime installer failure", result.stderr)
                self.assertFalse(marker.exists(), "cargo ran after runtime setup failed")
                self.assertEqual(list(temporary_state.iterdir()), [])


if __name__ == "__main__":
    unittest.main()
