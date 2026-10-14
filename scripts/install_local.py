#!/usr/bin/env python3
"""Build a private local engine installation from this source package.

Requires the reviewed native prerequisites; installs no system tools or Python
packages. It starts no service, handles no wallet, and never replaces data.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import sys
import tempfile

from check_environment import installed_rust_binary, inspect_environment
from setup_onnx_runtime import MANIFEST, default_cache, host_target, prepare_runtime, regular_file, sha256

ROOT = Path(__file__).resolve().parents[1]


def build_engine(root: Path, profile: str, offline: bool) -> Path:
    cargo = installed_rust_binary("cargo")
    if not cargo:
        raise ValueError("reviewed Cargo executable missing")
    command = [cargo, "build", "--locked", "--bin", "macbot"]
    if profile == "release":
        command.append("--release")
    if offline:
        command.append("--offline")
    subprocess.run(command, cwd=root, check=True, timeout=1800)
    metadata_command = [cargo, "metadata", "--locked", "--no-deps", "--format-version", "1"]
    if offline:
        metadata_command.append("--offline")
    result = subprocess.run(metadata_command,
                            cwd=root, check=True, capture_output=True, text=True, timeout=30)
    if len(result.stdout) > 1024 * 1024:
        raise ValueError("Cargo metadata exceeds 1MiB")
    binary = Path(json.loads(result.stdout)["target_directory"]) / profile / "macbot"
    if binary.is_symlink() or not binary.is_file() or not os.access(binary, os.X_OK):
        raise ValueError("compiled engine is absent or unsafe")
    return binary


def copy_regular(source: Path, destination: Path) -> None:
    if source.is_symlink() or not source.is_file():
        raise ValueError(f"installation resource must be a regular file: {source.name}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination)


def assemble(root: Path, stage: Path, binary: Path, runtime: Path) -> None:
    # Copy the named demonstration model only. Private models, databases,
    # credentials, local caches and ignored experiments never enter the install.
    inventory = json.loads((root / "static/vendor/asset-manifest.json").read_text())
    resources = ["static/index.html", "static/dashboard.js", "static/api-auth.js", "static/dashboard.css", "static/redesign.css", "static/vendor/asset-manifest.json"]
    for item in inventory["files"]:
        relative = Path(item["path"])
        if relative.is_absolute() or ".." in relative.parts:
            raise ValueError("unsafe dashboard inventory path")
        source = root / "static/vendor" / relative
        if source.stat().st_size != item["bytes"] or sha256(source) != item["sha256"]:
            raise ValueError("dashboard vendor bytes differ from reviewed inventory")
        resources.append("static/vendor/" + relative.as_posix())
    for relative in resources:
        copy_regular(root / relative, stage / relative)
    # Validate the installed bytes, not just the source observed before copying.
    for item in inventory["files"]:
        copied = stage / "static/vendor" / item["path"]
        if copied.stat().st_size != item["bytes"] or sha256(copied) != item["sha256"]:
            raise ValueError("installed dashboard vendor bytes differ from reviewed inventory")
    for relative in ("models/latest.onnx", "models/provenance.json", "ops/macro-events.json",
                     "scripts/update_macro_calendar.py", "scripts/native_tls_runtime.py", "scripts/installation_credentials.py",
                     "LICENSE.md", "THIRD_PARTY_NOTICES.md"):
        copy_regular(root / relative, stage / relative)
    provenance = json.loads((stage / "models/provenance.json").read_text())
    if (provenance.get("origin") != "synthetic_untrained_test_model"
            or provenance.get("sha256") != sha256(stage / "models/latest.onnx")):
        raise ValueError("bundled demonstration model differs from its documented provenance")
    manifest = json.loads(MANIFEST.read_text())
    entry = manifest["targets"][host_target()]
    runtime_names = [entry["library_name"], "runtime-manifest.json"] + [item["name"] for item in entry.get("extra_libraries", []) + entry["notices"]]
    for name in runtime_names:
        copy_regular(runtime.parent / name, stage / "runtime" / name)
    for item in [{"name": entry["library_name"], "bytes": entry["library_bytes"], "sha256": entry["library_sha256"]}] + entry.get("extra_libraries", []) + entry["notices"]:
        copied = stage / "runtime" / item["name"]
        if copied.stat().st_size != item["bytes"] or sha256(copied) != item["sha256"]:
            raise ValueError("installed runtime differs from pinned official bytes")
    copy_regular(binary, stage / "bin/macbot")
    # Start from the reviewed research template, never process-local configuration.
    config = json.loads((root / "ops/container-research.json").read_text())
    if (config.get("mode") != "simulation" or config.get("auth_required") is not True
            or any(config.get(name) is not False for name in
                   ("auto_trading", "shadow_trading", "maker_live_quoting", "live_partial_take_profit",
                    "sim_live_execution", "macro_event_guard_enabled", "macro_event_auto_refresh"))
            or any(config.get(name) != "" for name in
                   ("private_rpc_url", "finnhub_api_key", "lunarcrush_api_key", "signal_weights_path"))):
        raise ValueError("reviewed research template does not contain safe installation defaults")
    config.update(api_key=secrets.token_hex(32), api_hmac_secret=secrets.token_hex(32),
                  macro_event_calendar_path="state/macro-events.json")
    (stage / "state").mkdir(mode=0o700)
    config_path = stage / "state/config.json"
    config_path.write_text(json.dumps(config, indent=2) + "\n")
    config_path.chmod(0o600)
    copy_regular(stage / "ops/macro-events.json", stage / "state/macro-events.json")
    launcher = r'''#!/bin/sh
set -eu
MACBOT_INSTALL_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$MACBOT_INSTALL_DIR"
case "${1:-serve}" in
  credentials)
    [ "$#" -eq 1 ] || { echo 'Usage: run.sh [serve|credentials]' >&2; exit 2; }
    exec python3 "$MACBOT_INSTALL_DIR/scripts/installation_credentials.py" --installation "$MACBOT_INSTALL_DIR"
    ;;
  serve)
    [ "$#" -le 1 ] || { echo 'Usage: run.sh [serve|credentials]' >&2; exit 2; }
    ;;
  *) echo 'Usage: run.sh [serve|credentials]' >&2; exit 2 ;;
esac
export MACBOT_CONFIG="$MACBOT_INSTALL_DIR/state/config.json"
export MACBOT_DOTENV_PATH="$MACBOT_INSTALL_DIR/state/.env"
export MACBOT_DB="$MACBOT_INSTALL_DIR/state/trades.sqlite"
export MACBOT_CONFIG_SNAPSHOT_DIR="$MACBOT_INSTALL_DIR/state/config-snapshots"
export MACBOT_ML_WEIGHTS_PATH="$MACBOT_INSTALL_DIR/state/ml_weights.json"
export MACBOT_ENSEMBLE_PATH="$MACBOT_INSTALL_DIR/state/ensemble_state.json"
export MACBOT_ALERT_FALLBACK_PATH="$MACBOT_INSTALL_DIR/state/alerts.jsonl"
export MACBOT_MODELS_DIR="$MACBOT_INSTALL_DIR/models"
export MACBOT_ONNX_RUNTIME_LIB="$MACBOT_INSTALL_DIR/runtime/__LIBRARY__"
export MACBOT_BIND_ALL=0
export MACBOT_BACKTEST_ONLY="${MACBOT_BACKTEST_ONLY:-1}"
case "$MACBOT_BACKTEST_ONLY" in
  1) echo 'Starting Macbot on loopback in research mode; no trading loop or feeds start.' ;;
  *) echo 'Starting Macbot on loopback with runtime operation explicitly enabled.' ;;
esac
echo 'Run ./run.sh credentials to read the private dashboard keys.'
exec "$MACBOT_INSTALL_DIR/bin/macbot"
'''.replace("__LIBRARY__", entry["library_name"])
    (stage / "run.sh").write_text(launcher)
    (stage / "run.sh").chmod(0o700)
    files = [{"path": path.relative_to(stage).as_posix(), "sha256": sha256(path), "bytes": path.stat().st_size}
             for path in sorted(stage.rglob("*")) if path.is_file()]
    (stage / "installation.json").write_text(json.dumps({"schema_version": 1, "host_target": host_target(),
        "default_mode": "simulation, research-only, loopback", "model": "untrained demonstration only",
        "files": files, "acceptance": "local build/resource assembly; platform acceptance must be run separately"}, indent=2) + "\n")


def install(destination: Path, *, profile: str, offline: bool, archive: Path | None) -> Path:
    destination = destination.expanduser().absolute()
    if destination.exists() or destination.is_symlink():
        raise ValueError("destination already exists; existing installations/data are never replaced")
    if not destination.parent.is_dir() or destination.parent.is_symlink():
        raise ValueError("destination parent must be an existing real directory")
    report = inspect_environment(full=False)
    if report["status"] != "pass":
        raise ValueError("prerequisites failed: " + "; ".join(report["errors"]))
    manifest = json.loads(MANIFEST.read_text())
    cache = default_cache()
    if offline and archive is None:
        # An offline request must never fall through to a network downloader.
        entry = manifest["targets"][host_target()]
        expected = cache / "onnxruntime" / manifest["version"] / host_target() / entry["library_name"]
        artifacts = [{"name":entry["library_name"], "bytes":entry["library_bytes"], "sha256":entry["library_sha256"]}] + entry.get("extra_libraries", []) + entry["notices"]
        if (any(not regular_file(expected.parent / item["name"])
                or (expected.parent / item["name"]).stat().st_size != item["bytes"]
                or sha256(expected.parent / item["name"]) != item["sha256"] for item in artifacts)
                or not regular_file(expected.parent / "runtime-manifest.json")
                or (expected.parent / "runtime-manifest.json").read_bytes() != (json.dumps(manifest, indent=2) + "\n").encode()):
            raise ValueError("offline runtime cache missing; supply --runtime-archive with the pinned official archive")
        runtime = expected
    else:
        runtime = prepare_runtime(manifest, host_target(), cache, archive)
    binary = build_engine(ROOT, profile, offline)
    with tempfile.TemporaryDirectory(dir=destination.parent, prefix=".macbot-install-") as temporary:
        stage = Path(temporary) / "install"
        stage.mkdir(mode=0o700)
        assemble(ROOT, stage, binary, runtime)
        # Reserve the final pathname before publication. A concurrent setup or
        # an existing user installation cannot be overwritten by rename.
        destination.mkdir(mode=0o700)
        try:
            os.rename(stage, destination)
        except BaseException:
            destination.rmdir()  # Only our empty reservation; never recursive.
            raise
    return destination


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination", type=Path, required=True)
    parser.add_argument("--profile", choices=("debug", "release"), default="debug")
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--runtime-archive", type=Path)
    args = parser.parse_args()
    try:
        destination = install(args.destination, profile=args.profile, offline=args.offline, archive=args.runtime_archive)
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError, KeyError) as error:
        parser.exit(1, f"Local setup failed; no service started: {error}\n")
    print(f"Prepared private installation: {destination}\nStart research demo: {destination / 'run.sh'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
