#!/usr/bin/env python3
"""Create an immutable source archive and a manifest from a clean Git commit.

Verification runs in a temporary extracted copy. No test output is repacked.
The manifest identifies the exact delivered bytes, not a mutable checkout.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile


ROOT = Path(__file__).resolve().parents[1]


def excluded(path: str) -> bool:
    parts = Path(path).parts
    name = parts[-1]
    return (
        path.startswith(("notes/", "data/", "artifacts/", "output/", "target"))
        or any(p in {".git", ".claude", ".playwright-cli", "__pycache__", "node_modules"}
               or p.startswith(".venv") for p in parts)
        or (name.startswith(".env") and path != ".env.example")
        or name in {"ml_weights.json", "ensemble_state.json"}
        or name.endswith((".db", ".db-wal", ".db-shm", ".sqlite", ".sqlite-wal",
                          ".sqlite-shm", ".key", ".pem", ".p12", ".pfx"))
    )


def inspect_archive(raw: bytes) -> list[dict]:
    files = []
    with tarfile.open(fileobj=io.BytesIO(raw)) as archive:
        for member in archive:
            path = Path(member.name)
            if path.is_absolute() or ".." in path.parts:
                raise ValueError("unsafe archive path")
            if member.isdir():
                continue
            if not member.isfile() or excluded(member.name):
                raise ValueError(f"excluded or unsupported archive member: {member.name}")
            content = archive.extractfile(member).read()
            files.append({"path": member.name, "bytes": len(content),
                          "mode": oct(member.mode),
                          "sha256": hashlib.sha256(content).hexdigest()})
    if not files:
        raise ValueError("empty source archive")
    return sorted(files, key=lambda f: f["path"])


def verification_environment(temporary: Path) -> dict[str, str]:
    environment = os.environ.copy()
    target = environment.get('MACBOT_VERIFY_TARGET_DIR') or environment.get('CARGO_TARGET_DIR')
    target = str(Path(target).resolve()) if target else str(temporary / 'target')
    environment['MACBOT_VERIFY_TARGET_DIR'] = target
    environment['CARGO_TARGET_DIR'] = target
    # Imported verification helpers must not add bytecode to the delivery tree.
    # Keep the later exact-file-set check strict rather than cleaning its output.
    environment['PYTHONDONTWRITEBYTECODE'] = '1'
    return environment


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ref", default="HEAD", help="commit or tag")
    parser.add_argument("--output", type=Path, required=True, help="new .tar.gz path")
    args = parser.parse_args()
    output = args.output.resolve()
    manifest_path = output.with_suffix("").with_suffix(".manifest.json")
    log_path = output.with_suffix("").with_suffix(".verification.log")
    if not str(output).endswith(".tar.gz"):
        parser.error("output must end in .tar.gz")
    if any(p.exists() for p in (output, manifest_path, log_path)):
        parser.error("refusing to replace an existing package or its evidence")
    status = subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT)
    if status.strip():
        parser.error("commit the source changes before packaging")
    commit = subprocess.check_output(
        ["git", "rev-parse", "--verify", f"{args.ref}^{{commit}}"], cwd=ROOT,
        text=True).strip()
    raw = subprocess.check_output(["git", "archive", "--format=tar", commit], cwd=ROOT)
    files = inspect_archive(raw)
    packed = gzip.compress(raw, mtime=0)
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="macbot-package-") as temporary:
        stage = Path(temporary) / "source"
        stage.mkdir()
        with tarfile.open(fileobj=io.BytesIO(raw)) as archive:
            archive.extractall(stage)
        environment = verification_environment(Path(temporary))
        with log_path.open("w") as log:
            result = subprocess.run(["bash", "scripts/verify_source_package.sh"], cwd=stage,
                                    env=environment, stdout=log, stderr=subprocess.STDOUT)
        if result.returncode:
            print(f"Verification failed; no delivery archive created. Evidence: {log_path}")
            return result.returncode
        for entry in files:
            actual = (stage / entry["path"]).read_bytes()
            if hashlib.sha256(actual).hexdigest() != entry["sha256"]:
                raise ValueError(f"verification changed a delivery file: {entry['path']}")
        expected = {f["path"] for f in files}
        extra = [str(p.relative_to(stage)) for p in stage.rglob("*")
                 if p.is_file() and str(p.relative_to(stage)) not in expected]
        if extra:
            raise ValueError(f"verification left {len(extra)} files in the delivery source; first twenty: {extra[:20]}")
    manifest = {
        "schema_version": 1, "git_commit": commit, "source_ref": args.ref,
        "archive": output.name, "archive_sha256": hashlib.sha256(packed).hexdigest(),
        "file_count": len(files), "source_bytes": sum(f["bytes"] for f in files),
        "verification": "full technical gate, training tests without skips, synthetic decision/model parity",
        "verification_log_sha256": hashlib.sha256(log_path.read_bytes()).hexdigest(),
        "files": files,
    }
    output.write_bytes(packed)
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"Verified archive: {output}\nManifest: {manifest_path}\nEvidence: {log_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
