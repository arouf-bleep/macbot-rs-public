#!/usr/bin/env python3
"""Assemble only reviewed public runtime resources inside an image build.

The official ONNX archive is downloaded by curl, then this helper verifies its
size/hash and every selected member through the existing offline installer.
No source config, private models, credentials, database or training data enter
the runtime. This helper does not start the engine.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import shutil
import subprocess

from setup_onnx_runtime import MANIFEST, host_target, prepare_runtime, sha256


def copy_file(source: Path, destination: Path) -> None:
    if source.is_symlink() or not source.is_file():
        raise ValueError("runtime resources must be regular files")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)


def assemble(root: Path, output: Path) -> None:
    if output.exists():
        raise ValueError("container output already exists")
    output.mkdir(mode=0o755)
    manifest = json.loads(MANIFEST.read_text())
    target = host_target()
    if target not in ("linux-aarch64", "linux-x86_64"):
        raise ValueError("container requires native Linux ARM64 or x64")
    entry = manifest["targets"][target]
    archive = output / "official-runtime.tgz"
    subprocess.run([
        "curl", "-q", "--proto", "=https", "--proto-redir", "=https",
        "--tlsv1.2", "--fail", "--location", "--silent", "--show-error",
        "--max-time", "180", "--max-filesize", str(entry["archive_bytes"]),
        "--output", str(archive), entry["url"],
    ], check=True, timeout=190)
    runtime = prepare_runtime(manifest, target, output / "verified-cache", archive)
    for name in [entry["library_name"], "runtime-manifest.json"] + [item["name"] for item in entry.get("extra_libraries", []) + entry["notices"]]:
        copy_file(runtime.parent / name, output / "runtime" / name)
    shutil.rmtree(output / "verified-cache")
    archive.unlink()
    inventory = json.loads((root / "static/vendor/asset-manifest.json").read_text())
    resources = ["static/index.html", "static/dashboard.js", "static/api-auth.js",
                 "static/dashboard.css", "static/redesign.css", "static/vendor/asset-manifest.json"]
    for item in inventory["files"]:
        relative = Path(item["path"])
        if relative.is_absolute() or ".." in relative.parts:
            raise ValueError("unsafe vendor inventory path")
        source = root / "static/vendor" / relative
        if source.is_symlink() or source.stat().st_size != item["bytes"] or sha256(source) != item["sha256"]:
            raise ValueError("vendor resource differs from reviewed inventory")
        resources.append("static/vendor/" + relative.as_posix())
    for relative in resources + ["models/latest.onnx", "models/provenance.json", "LICENSE.md", "THIRD_PARTY_NOTICES.md"]:
        copy_file(root / relative, output / relative)
    provenance = json.loads((output / "models/provenance.json").read_text())
    if provenance.get("origin") != "synthetic_untrained_test_model" or provenance.get("sha256") != sha256(output / "models/latest.onnx"):
        raise ValueError("untrained demonstration model differs from provenance")
    for source, destination in [("ops/container-research.json", "research-template.json"),
                                ("ops/container-model-fixture.json", "model-fixture.json"),
                                ("scripts/container_entrypoint.py", "container_entrypoint.py"),
                                ("target/release/macbot", "bin/macbot"),
                                ("target/release/examples/check_model_runtime", "bin/check_model_runtime")]:
        copy_file(root / source, output / destination)
    for binary in (output / "bin").iterdir():
        binary.chmod(0o555)
    (output / "empty.env").touch(mode=0o600)
    files = [{"path": path.relative_to(output).as_posix(), "bytes": path.stat().st_size,
              "sha256": sha256(path)} for path in sorted(output.rglob("*")) if path.is_file()]
    source_inputs = [{"path": path.relative_to(root).as_posix(), "bytes": path.stat().st_size,
                      "sha256": sha256(path)} for path in sorted(root.rglob("*"))
                     if path.is_file() and "target" not in path.relative_to(root).parts]
    if any(item["path"] == "config.json" or item["path"].startswith(("notes/", ".env"))
           or "private-" in item["path"] for item in source_inputs):
        raise ValueError("private/unexpected source input reached the build stage")
    (output / "container-resources.json").write_text(json.dumps({"schema_version": 1,
        "host_target": target, "onnx_runtime": manifest["version"], "files": files,
        "build_source_inputs": source_inputs,
        "scope": "Resource assembly; no runtime, platform or funded acceptance claim."}, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    assemble(Path(__file__).resolve().parents[1], args.output)
