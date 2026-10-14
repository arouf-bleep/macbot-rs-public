#!/usr/bin/env python3
"""Read-only prerequisite check; never installs, downloads or opens credentials.

Use the exact Python selected for the demo or verification gate. --full also checks
the pinned validation packages, their dependency consistency and cached ONNX
bytes. Success is an environment check, not Linux or funded acceptance.
"""
from __future__ import annotations

import argparse
import importlib.metadata as metadata
import json
import os
from pathlib import Path
import re
import shutil
import ssl
import sqlite3
import subprocess
import sys

from check_verification_environment import python_requirement_error, sqlite_requirement_error
from native_tls_runtime import tls_requirement_error
from setup_onnx_runtime import MANIFEST, default_cache, host_target, regular_file, sha256

ROOT = Path(__file__).resolve().parents[1]


def command_output(command: list[str]) -> str:
    """Only local version/metadata probes, with no shell or installer fallback."""
    environment = {name: os.environ[name] for name in (
        "PATH", "HOME", "LANG", "LC_ALL", "LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH",
        "SSL_CERT_FILE", "SSL_CERT_DIR", "SYSTEMROOT",
    ) if name in os.environ}
    environment.update(PYTHONDONTWRITEBYTECODE="1", PIP_DISABLE_PIP_VERSION_CHECK="1",
                       PIP_CONFIG_FILE=os.devnull)
    result = subprocess.run(command, cwd=ROOT, env=environment, capture_output=True,
                            text=True, timeout=30, check=False)
    if result.returncode:
        raise ValueError((result.stderr or result.stdout).strip() or "command failed")
    return result.stdout.strip()


def toolchain_errors() -> list[str]:
    errors = []
    versions = {"rustc": r"rustc 1\.94\.0", "cargo": r"cargo 1\.94\.0",
                "rustfmt": r"rustfmt 1\.8\.0(?:-stable)?", "clippy-driver": r"clippy 0\.1\.94"}
    for tool in ("rustc", "cargo", "rustfmt", "clippy-driver"):
        try:
            executable = installed_rust_binary(tool)
            if not executable or not Path(executable).is_file() or not os.access(executable, os.X_OK):
                raise ValueError("not installed or not executable")
            version = command_output([executable, "--version"])
            if not re.match(rf"^{versions[tool]}(?:\s|$)", version):
                raise ValueError(f"unexpected version response: {version}")
        except (OSError, ValueError, subprocess.TimeoutExpired) as error:
            errors.append(f"{tool} from Rust 1.94.0 is unavailable ({error}); run rustup toolchain install "
                          "1.94.0 --profile minimal --component rustfmt --component clippy")
    node = shutil.which("node")
    try:
        version = command_output([node, "--version"]) if node else ""
        match = re.fullmatch(r"v(\d+)\.\d+\.\d+", version)
        if not match or int(match.group(1)) < 22:
            raise ValueError("Node 22 or newer required")
    except (OSError, ValueError, subprocess.TimeoutExpired):
        errors.append("Node.js 22 or newer is unavailable; install Node 22 LTS from nodejs.org")
    return errors


def installed_rust_binary(tool: str) -> str | None:
    """Resolve files directly; even rustup itself is never launched by the doctor."""
    if not shutil.which("rustup"):
        return shutil.which(tool)
    triples = {"macos-aarch64":"aarch64-apple-darwin", "linux-x86_64":"x86_64-unknown-linux-gnu",
               "linux-aarch64":"aarch64-unknown-linux-gnu"}
    triple = triples.get(host_target())
    if triple is None:
        return None
    installation = Path(os.environ.get("RUSTUP_HOME") or Path.home()/".rustup")
    return str(installation/"toolchains"/f"1.94.0-{triple}"/"bin"/tool)


def runtime_errors(manifest: dict, target: str, *, required: bool) -> list[str]:
    entry = manifest["targets"].get(target)
    if entry is None:
        return [f"No checksum-pinned ONNX runtime for {target}; supported targets: "
                + ", ".join(sorted(manifest["targets"]))]
    directory = default_cache() / "onnxruntime" / manifest["version"] / target
    selected = os.environ.get("MACBOT_ONNX_RUNTIME_LIB")
    library = Path(selected).expanduser() if selected else directory / entry["library_name"]
    if not library.exists() and not required and not selected:
        return []  # Demo installer verifies/downloads it explicitly on first use.
    expected = [{"name": library.name, "bytes": entry["library_bytes"],
                 "sha256": entry["library_sha256"]}] + entry.get("extra_libraries", []) + entry["notices"]
    for index, artifact in enumerate(expected):
        candidate = library if index == 0 else library.parent / artifact["name"]
        if (not regular_file(candidate) or candidate.stat().st_size != artifact["bytes"]
                or sha256(candidate) != artifact["sha256"]):
            return ["ONNX runtime cache is absent or differs from the official pinned artifact; run "
                    "this interpreter with scripts/setup_onnx_runtime.py before the full check"]
    return []


def validation_errors() -> list[str]:
    errors = []
    for line in (ROOT / "training/requirements-verify.lock").read_text().splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        name, expected = line.strip().split("==", 1)
        try:
            actual = metadata.version(name)
        except metadata.PackageNotFoundError:
            actual = "missing"
        if actual.split("+", 1)[0] != expected:
            errors.append(f"{name}: expected {expected}, found {actual}; install the validation "
                          "lock in this interpreter's external virtual environment")
    if not errors:
        try:
            command_output([sys.executable, "-B", "-m", "pip", "check"])
        except (OSError, ValueError, subprocess.TimeoutExpired) as error:
            errors.append(f"Validation dependencies are inconsistent: {error}")
    return errors


def inspect_environment(*, full: bool) -> dict:
    errors = [error for error in (python_requirement_error(), sqlite_requirement_error(),
                                  tls_requirement_error()) if error]
    information = [f"Python {sys.version.split()[0]}", f"Loaded {ssl.OPENSSL_VERSION}",
                   f"Loaded SQLite {sqlite3.sqlite_version}"]
    if sys.maxsize <= 2**32:
        errors.append("The pinned native ONNX runtimes require a 64-bit Python interpreter")
    try:
        context = ssl.create_default_context()
        if context.cert_store_stats()["x509_ca"] == 0:
            information.append("No CA certificates preloaded; a hashed CA directory may load lazily. "
                               "Review the system trust store or SSL_CERT_FILE before HTTPS use.")
    except (OSError, ssl.SSLError) as error:
        errors.append(f"Cannot initialize Python's certificate store: {error}; review SSL_CERT_FILE")
    try:
        manifest = json.loads(MANIFEST.read_text())
        target = host_target()
        information.append(f"Host target {target}; native CPU ONNX runtime {manifest['version']}")
        errors.extend(runtime_errors(manifest, target, required=full))
        if not full:
            information.append("The demo checks/downloads its runtime on first use; this check does not download it.")
    except (OSError, ValueError, KeyError, TypeError) as error:
        errors.append(f"Cannot select or verify the native runtime: {error}")
    errors.extend(toolchain_errors())
    if full:
        errors.extend(validation_errors())
    return {"status": "pass" if not errors else "fail", "checks": information, "errors": errors,
            "scope": "local prerequisites only; no network, deployment or platform acceptance performed"}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--full", action="store_true", help="also require cached ONNX and pinned validation dependencies")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    report = inspect_environment(full=args.full)
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        for check in report["checks"]:
            print(f"check  {check}")
        for error in report["errors"]:
            print(f"FAIL   {error}")
        print(report["scope"])
        print("operator prerequisites " + ("passed." if report["status"] == "pass" else "need attention."))
    return 0 if report["status"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main())
