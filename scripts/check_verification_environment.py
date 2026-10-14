#!/usr/bin/env python3
"""Validate the pinned Python 3.11 environment used by the verification gate."""
from __future__ import annotations
import importlib.metadata as metadata
from pathlib import Path
import subprocess
import sys
import sqlite3
from native_tls_runtime import tls_requirement_error


def sqlite_requirement_error() -> str | None:
    if sqlite3.sqlite_version_info < (3, 51, 3):
        return (
            'full project verification requires native SQLite 3.51.3 or newer; '
            f'this Python uses {sqlite3.sqlite_version}. On Linux use '
            'scripts/provision_sqlite.py and LD_LIBRARY_PATH before verification.'
        )
    return None


def python_requirement_error() -> str | None:
    if sys.version_info[:2] != (3, 11) or sys.version_info[:3] < (3, 11, 17):
        return 'full project verification requires Python 3.11.17 or newer within the 3.11 branch'
    return None


def main() -> int:
    errors = []
    python_error = python_requirement_error()
    if python_error:
        errors.append(python_error)
    sqlite_error = sqlite_requirement_error()
    if sqlite_error:
        errors.append(sqlite_error)
    tls_error = tls_requirement_error()
    if tls_error:
        errors.append(tls_error)
    lock = Path(__file__).resolve().parents[1] / 'training/requirements-verify.lock'
    for line in lock.read_text().splitlines():
        if not line.strip() or line.startswith('#'): continue
        name, expected = line.strip().split('==', 1)
        try:
            actual = metadata.version(name)
        except metadata.PackageNotFoundError:
            errors.append(f'{name} is missing')
            continue
        # The Linux CPU Torch wheel uses a +cpu local-version suffix.
        if actual.split('+', 1)[0] != expected:
            errors.append(f'{name}: expected {expected}, found {actual}')
    if errors:
        for error in errors: print(error, file=sys.stderr)
        return 1
    subprocess.run([sys.executable, '-m', 'pip', 'check'], check=True)
    print('Pinned Python verification environment matches.')
    return 0


if __name__ == '__main__': raise SystemExit(main())
