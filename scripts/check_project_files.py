#!/usr/bin/env python3
"""Validate portable, safe-to-track project configuration using the stdlib only."""

from __future__ import annotations

import json
import hashlib
import re
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
TEXT_SUFFIXES = {
    ".html",
    ".js",
    ".json",
    ".md",
    ".mjs",
    ".py",
    ".rs",
    ".sh",
    ".toml",
    ".yml",
    ".yaml",
}
SCAN_ROOTS = ("src", "scripts", "training", "ops", "static", "docs", ".github")
SKIP_DIRS = {".git", ".playwright-cli", ".pycache", ".venv-training", "__pycache__", "artifacts", "data", "target", "target.nosync"}
ROOT_TEXT_FILES = (
    ".cargo/config.toml",
    ".env.example",
    ".gitignore",
    "docs/history/CHANGELOG.md",
    "CONTRIBUTING.md",
    "Cargo.toml",
    "LICENSE.md",
    "README.md",
    "SECURITY.md",
    "config.json",
    "rust-toolchain.toml",
    "models/provenance.json",
)
PERSONAL_PATH = re.compile(r"(?:/(?:Users|home)/[^/\s]+|[A-Za-z]:[\\/]+Users[\\/]+[^\\/\s]+)")
SAFE_CREDENTIAL_VALUES = {"", "local", "localsecret"}


def load_json(path: Path, errors: list[str]):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        errors.append(f"invalid JSON: {path.relative_to(ROOT)}: {exc}")
        return None


def iter_text_files():
    for relative in ROOT_TEXT_FILES:
        path = ROOT / relative
        if path.exists():
            yield path
    for dirname in SCAN_ROOTS:
        base = ROOT / dirname
        if not base.exists():
            continue
        for path in base.rglob("*"):
            relative_parts = path.relative_to(ROOT).parts
            if any(part in SKIP_DIRS for part in relative_parts):
                continue
            if path.name.endswith("_temp.json"):
                continue
            if path.is_file() and path.suffix.lower() in TEXT_SUFFIXES:
                yield path


def is_ignored(path: Path) -> bool:
    # A plain source export must not inherit ignore rules from a parent repo.
    if not (ROOT / '.git').exists():
        return False
    result = subprocess.run(
        ["git", "check-ignore", "--quiet", str(path)],
        cwd=ROOT,
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return result.returncode == 0


def check_vendor_assets(vendor: Path, errors: list[str]) -> None:
    """Ensure every shipped vendor file still matches the reviewed inventory."""
    try:
        manifest_path = vendor / "asset-manifest.json"
        if vendor.is_symlink() or manifest_path.is_symlink():
            raise ValueError("vendor root and inventory must not be symlinks")
        descendants = list(vendor.rglob("*"))
        if any(path.is_symlink() for path in descendants):
            raise ValueError("vendor assets and directories must not be symlinks")
        manifest = json.loads(manifest_path.read_text())
        entries = manifest["files"]
        if not isinstance(entries, list):
            raise ValueError("files must be a list")
        expected = set()
        for entry in entries:
            relative = entry["path"]
            path = Path(relative)
            if (not isinstance(relative, str) or path.is_absolute()
                    or ".." in path.parts or relative in expected
                    or relative == "asset-manifest.json"):
                raise ValueError("invalid or duplicate asset path")
            expected.add(relative)
            target = vendor / path
            if target.is_symlink() or not target.is_file():
                errors.append(f"vendor asset missing or a symlink: {relative}")
                continue
            data = target.read_bytes()
            if len(data) != entry["bytes"] or hashlib.sha256(data).hexdigest() != entry["sha256"]:
                errors.append(f"vendor asset differs from reviewed bytes: {relative}")
        actual = {p.relative_to(vendor).as_posix() for p in descendants
                  if p.is_file() and p != manifest_path}
        if actual != expected:
            errors.append("vendor asset inventory does not cover the distributed files")
    except (OSError, ValueError, TypeError, KeyError) as exc:
        errors.append(f"invalid vendor asset inventory: {exc}")


def main() -> int:
    errors: list[str] = []
    checked: set[Path] = set()
    check_vendor_assets(ROOT / "static/vendor", errors)

    json_paths = [ROOT / "config.json", ROOT / "ops/macro-events.json"]
    json_paths.extend(sorted((ROOT / "ops/profiles").glob("*.json")))
    for path in json_paths:
        if path.exists() and not is_ignored(path):
            load_json(path, errors)

    config = load_json(ROOT / "config.json", errors)
    if isinstance(config, dict):
        if config.get("mode") == "live":
            errors.append("config.json must not default to live mode")
        for key in ("api_key", "api_hmac_secret", "finnhub_api_key", "lunarcrush_api_key"):
            value = config.get(key, "")
            if value not in SAFE_CREDENTIAL_VALUES:
                errors.append(f"config.json contains a non-placeholder credential in {key}")

    for path in iter_text_files():
        if path in checked:
            continue
        checked.add(path)
        try:
            content = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        if PERSONAL_PATH.search(content):
            errors.append(f"personal absolute path found: {path.relative_to(ROOT)}")

    for relative in (".env", "trades.db", "ml_weights.json", "ensemble_state.json"):
        path = ROOT / relative
        if path.exists() and not is_ignored(path):
            errors.append(f"local runtime file is present in the source boundary: {relative}")

    model = ROOT / "models/latest.onnx"
    if not model.is_file() or model.stat().st_size == 0:
        errors.append("models/latest.onnx is missing or empty")
    provenance = ROOT / "models/provenance.json"
    if provenance.exists():
        record = load_json(provenance, errors)
        if isinstance(record, dict) and model.is_file():
            if record.get('sha256') != hashlib.sha256(model.read_bytes()).hexdigest():
                errors.append('bundled model differs from models/provenance.json')
            if record.get('origin') != 'synthetic_untrained_test_model' or record.get('training_data') != 'none':
                errors.append('model provenance must identify the untrained fixture without training data')

    # The TLS provider must be installed before anything opens a socket.
    #
    # rustls 0.23 will not pick one when both aws-lc-rs and ring are compiled
    # in, and both are. Without the install, l2_ingestion, price_bus and
    # liquidation_feed each panic on their first handshake roughly fourteen
    # seconds after startup; l2_ingestion is monitored as critical, so the task
    # monitor engages the kill switch and the bot halts itself on every run.
    # The unit tests in src/tls.rs prove the installer works; this proves main
    # still calls it, which no test inside the library can.
    main_rs = (ROOT / "src/main.rs").read_text(encoding="utf-8")
    if "tls::install_provider()" not in main_rs:
        errors.append(
            "src/main.rs does not install a TLS crypto provider; every TLS task "
            "will panic on its first handshake and the kill switch will engage"
        )
    else:
        install_at = main_rs.index("tls::install_provider()")
        for task in ("spawn_l2_ingestion", "spawn_price_bus", "spawn_liquidation_feed"):
            at = main_rs.find(task)
            if at != -1 and at < install_at:
                errors.append(
                    f"src/main.rs spawns {task} before installing the TLS provider"
                )

    # Every path that halts trading must be able to say why.
    #
    # A halted bot with no stated cause sends an operator to logs that may have
    # rotated. Paths either call halt_trading directly, or call
    # record_reconcile_error within a few lines, which adopts the error as the
    # cause when none is set. Anything else halts silently.
    halt_gaps: list[str] = []
    for rs in sorted((ROOT / "src").rglob("*.rs")):
        lines = rs.read_text(encoding="utf-8").split("\n")
        joined = "\n".join(lines)
        for index, line in enumerate(lines):
            if "set_kill_switch(true)" not in line:
                continue
            before = "\n".join(lines[:index])
            if "#[cfg(test)]" in before and before.rfind("#[cfg(test)]") > before.rfind("\n}\n"):
                continue  # inside a test module
            window = "\n".join(lines[index:index + 9])
            if "record_reconcile_error" in window or "freeze_entries" in window:
                continue
            halt_gaps.append(
                f"{rs.relative_to(ROOT)}:{index + 1} halts trading without recording a cause; "
                "use halt_trading(source, detail)"
            )
    errors.extend(halt_gaps)

    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1

    print(f"Project file checks passed: {len(checked)} portable text files checked")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
