"""operator doctor checks local facts without installing or contacting services."""
import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import check_environment as doctor


class BuyerEnvironmentTests(unittest.TestCase):
    def test_local_probe_does_not_inherit_api_credentials_or_pip_indexes(self):
        completed=mock.Mock(returncode=0,stdout="version",stderr="")
        with mock.patch.dict(doctor.os.environ,{"MACBOT_API_KEY":"fixture-private-key",
             "POLYMARKET_SECRET":"fixture-private-secret","PIP_INDEX_URL":"fixture-private-index"}), \
             mock.patch.object(doctor.subprocess,"run",return_value=completed) as run:
            self.assertEqual(doctor.command_output(["node","--version"]),"version")
        environment=run.call_args.kwargs["env"]
        self.assertNotIn("MACBOT_API_KEY",environment)
        self.assertNotIn("POLYMARKET_SECRET",environment)
        self.assertNotIn("PIP_INDEX_URL",environment)
        self.assertEqual(environment["PIP_CONFIG_FILE"],doctor.os.devnull)

    def test_missing_rust_is_reported_without_invoking_a_rustup_shim(self):
        with mock.patch.object(doctor.shutil, "which", side_effect=lambda name: "/tools/rustup" if name == "rustup" else None), \
             mock.patch.object(doctor, "installed_rust_binary", return_value=None), \
             mock.patch.object(doctor, "command_output") as probe:
            errors = doctor.toolchain_errors()
        self.assertEqual(len(errors), 5)
        probe.assert_not_called()

    def test_wrong_node_major_is_actionable(self):
        with tempfile.TemporaryDirectory() as temporary:
            # Version responses differ for compiler and cargo; retain each
            # binary name so the test exercises actual version classification.
            binaries = {name:Path(temporary)/name for name in ("rustc","cargo","rustfmt","clippy-driver")}
            for item in binaries.values():
                item.write_text("fixture")
                item.chmod(0o755)
            def probe(command):
                name=Path(command[0]).name
                return {"node":"v20.19.0", "rustc":"rustc 1.94.0 (fixture)",
                        "cargo":"cargo 1.94.0 (fixture)", "rustfmt":"rustfmt 1.8.0-stable (fixture)",
                        "clippy-driver":"clippy 0.1.94 (fixture)"}[name]
            with mock.patch.object(doctor.shutil,"which",side_effect=lambda name: "/tools/"+name), \
                 mock.patch.object(doctor,"installed_rust_binary",side_effect=lambda name: str(binaries[name])), \
                 mock.patch.object(doctor,"command_output",side_effect=probe):
                errors=doctor.toolchain_errors()
            self.assertEqual(len(errors),1)
            self.assertIn("Node.js 22",errors[0])

    def check_component_fixture(self, component, content, *, executable):
        """Invoke the actual local probes, not a mocked successful response."""
        responses={"rustc":"rustc 1.94.0 (fixture)", "cargo":"cargo 1.94.0 (fixture)",
                   "rustfmt":"rustfmt 1.8.0-stable (fixture)", "clippy-driver":"clippy 0.1.94 (fixture)",
                   "node":"v22.23.3"}
        with tempfile.TemporaryDirectory() as temporary:
            binaries={name:Path(temporary)/name for name in responses}
            for name,item in binaries.items():
                item.write_text("#!/bin/sh\nprintf '%s\\n' '"+responses[name]+"'\n")
                item.chmod(0o755)
            binaries[component].write_text(content)
            binaries[component].chmod(0o755 if executable else 0o644)
            with mock.patch.object(doctor.shutil,"which",side_effect=lambda name: str(binaries[name]) if name in binaries else None), \
                 mock.patch.object(doctor,"installed_rust_binary",side_effect=lambda name: str(binaries[name])):
                return doctor.toolchain_errors()

    def test_non_executable_rust_components_are_not_accepted(self):
        for component in ("rustfmt","clippy-driver"):
            with self.subTest(component=component):
                errors=self.check_component_fixture(component,"#!/bin/sh\nexit 0\n",executable=False)
                self.assertEqual(len(errors),1)
                self.assertIn(component,errors[0])
                self.assertIn("not executable",errors[0])

    def test_failing_rust_components_are_not_accepted(self):
        for component in ("rustfmt","clippy-driver"):
            with self.subTest(component=component):
                errors=self.check_component_fixture(component,"#!/bin/sh\nprintf '%s\\n' 'broken component' >&2\nexit 2\n",executable=True)
                self.assertEqual(len(errors),1)
                self.assertIn(component,errors[0])
                self.assertIn("broken component",errors[0])

    def test_wrong_successful_component_version_is_not_accepted(self):
        for component,response in (("rustfmt","rustfmt 1.7.0-stable"),("clippy-driver","clippy 0.1.93")):
            with self.subTest(component=component):
                errors=self.check_component_fixture(component,"#!/bin/sh\nprintf '%s\\n' '"+response+"'\n",executable=True)
                self.assertEqual(len(errors),1)
                self.assertIn("unexpected version response",errors[0])

    def test_arm64_rust_toolchain_resolution_uses_files_only(self):
        with mock.patch.object(doctor.shutil,"which",return_value="/tools/rustup"), \
             mock.patch.object(doctor,"host_target",return_value="linux-aarch64"), \
             mock.patch.dict(doctor.os.environ,{"RUSTUP_HOME":"/external/toolchains"},clear=True):
            self.assertEqual(doctor.installed_rust_binary("rustc"),
                             "/external/toolchains/toolchains/1.94.0-aarch64-unknown-linux-gnu/bin/rustc")

    def test_runtime_target_and_tampered_cache_are_not_accepted(self):
        import hashlib
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary)
            entry={"library_name":"lib.so","library_bytes":4,
                   "library_sha256":hashlib.sha256(b"good").hexdigest(),"notices":[],"extra_libraries":[]}
            manifest={"version":"1.0","targets":{"linux-aarch64":entry}}
            with mock.patch.object(doctor,"default_cache",return_value=root), \
                 mock.patch.dict(doctor.os.environ,{},clear=True):
                self.assertIn("supported targets",doctor.runtime_errors(manifest,"linux-x86_64",required=False)[0])
                self.assertEqual(doctor.runtime_errors(manifest,"linux-aarch64",required=False),[])
                self.assertTrue(doctor.runtime_errors(manifest,"linux-aarch64",required=True))
                library=root/"onnxruntime"/"1.0"/"linux-aarch64"/"lib.so"
                library.parent.mkdir(parents=True);library.write_bytes(b"good")
                self.assertEqual(doctor.runtime_errors(manifest,"linux-aarch64",required=True),[])
                library.write_bytes(b"evil")
                self.assertTrue(doctor.runtime_errors(manifest,"linux-aarch64",required=True))

    def test_package_mismatch_does_not_run_pip_check(self):
        with mock.patch.object(doctor.metadata,"version",return_value="0"), \
             mock.patch.object(doctor,"command_output") as probe:
            self.assertTrue(doctor.validation_errors())
            probe.assert_not_called()

    def test_loaded_native_error_and_unsupported_architecture_fail_without_full_package_probe(self):
        with mock.patch.object(doctor,"python_requirement_error",return_value=None), \
             mock.patch.object(doctor,"sqlite_requirement_error",return_value="loaded SQLite too old"), \
             mock.patch.object(doctor,"tls_requirement_error",return_value="loaded TLS too old"), \
             mock.patch.object(doctor,"host_target",return_value="windows-x86_64"), \
             mock.patch.object(doctor,"toolchain_errors",return_value=[]), \
             mock.patch.object(doctor,"validation_errors") as packages:
            result=doctor.inspect_environment(full=False)
        self.assertEqual(result["status"],"fail")
        self.assertIn("loaded SQLite too old",result["errors"])
        self.assertIn("loaded TLS too old",result["errors"])
        self.assertTrue(any("windows-x86_64" in value for value in result["errors"]))
        packages.assert_not_called()

    def test_actual_version_validators_refuse_older_python_sqlite_and_tls(self):
        with mock.patch.object(doctor.sys,"version_info",(3,11,16)), \
             mock.patch.object(doctor.sys,"version","3.11.16 fixture"), \
             mock.patch.object(doctor.sqlite3,"sqlite_version_info",(3,51,2)), \
             mock.patch.object(doctor.sqlite3,"sqlite_version","3.51.2"), \
             mock.patch.object(doctor.ssl,"OPENSSL_VERSION","OpenSSL 3.5.8 fixture"), \
             mock.patch.dict(doctor.os.environ,{},clear=True), \
             mock.patch.object(doctor,"runtime_errors",return_value=[]), \
             mock.patch.object(doctor,"toolchain_errors",return_value=[]):
            result=doctor.inspect_environment(full=False)
        self.assertEqual(result["status"],"fail")
        self.assertTrue(any("3.11.17" in error for error in result["errors"]))
        self.assertTrue(any("3.51.2" in error for error in result["errors"]))
        self.assertTrue(any("OpenSSL 3.5.8" in error for error in result["errors"]))

    def test_success_reports_only_environment_scope_and_full_checks_are_explicit(self):
        with mock.patch.object(doctor,"python_requirement_error",return_value=None), \
             mock.patch.object(doctor,"sqlite_requirement_error",return_value=None), \
             mock.patch.object(doctor,"tls_requirement_error",return_value=None), \
             mock.patch.object(doctor,"runtime_errors",return_value=[]) as runtime, \
             mock.patch.object(doctor,"toolchain_errors",return_value=[]), \
             mock.patch.object(doctor,"validation_errors",return_value=[]) as packages:
            result=doctor.inspect_environment(full=True)
        self.assertEqual(result["status"],"pass")
        self.assertIn("no network",result["scope"])
        self.assertIn("no network, deployment or platform acceptance",result["scope"])
        self.assertTrue(runtime.call_args.kwargs["required"])
        packages.assert_called_once()

    def test_json_failure_exit_is_machine_readable(self):
        report={"status":"fail","checks":[],"errors":["missing"],"scope":"local"}
        stream=io.StringIO()
        with mock.patch.object(doctor.sys,"argv",["doctor","--json"]), \
             mock.patch.object(doctor,"inspect_environment",return_value=report), \
             contextlib.redirect_stdout(stream):
            self.assertEqual(doctor.main(),1)
        self.assertEqual(json.loads(stream.getvalue()),report)


if __name__ == "__main__":
    unittest.main()
