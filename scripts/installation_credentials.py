#!/usr/bin/env python3
"""Explicitly read a private native installation's saved dashboard credentials.

This command starts no engine, reads no wallet, and writes no files. Environment
overrides used by a running engine are deliberately outside its persisted scope.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import stat

MAX_JSON_BYTES = 64 * 1024


def private_directory(path: Path | str, *, parent: int | None = None) -> int:
    descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)
    info = os.fstat(descriptor)
    if (not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid()
            or stat.S_IMODE(info.st_mode) & 0o077):
        os.close(descriptor)
        raise ValueError("installation and state directories must be owned by you and private")
    return descriptor


def read_json(parent: int, name: str, *, private: bool) -> dict:
    descriptor = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=parent)
    try:
        info = os.fstat(descriptor)
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or (private and stat.S_IMODE(info.st_mode) & 0o077)):
            raise ValueError("saved installation files must be regular, owned by you, and credentials private")
        if info.st_size > MAX_JSON_BYTES:
            raise ValueError("saved installation JSON exceeds the read limit")
        with os.fdopen(descriptor, "rb", closefd=False) as stream:
            payload = stream.read(MAX_JSON_BYTES + 1)
        if len(payload) > MAX_JSON_BYTES:
            raise ValueError("saved installation JSON exceeds the read limit")
        try:
            value = json.loads(payload)
        except (ValueError, UnicodeError) as error:
            raise ValueError("saved installation JSON is invalid") from error
        if not isinstance(value, dict):
            raise ValueError("saved installation JSON must contain an object")
        return value
    finally:
        os.close(descriptor)


def read_credentials(installation: Path) -> tuple[str, str]:
    if os.name != "posix" or not hasattr(os, "O_NOFOLLOW"):
        raise ValueError("private credential retrieval requires a supported POSIX installation")
    root = private_directory(installation)
    try:
        marker = read_json(root, "installation.json", private=False)
        if (type(marker.get("schema_version")) is not int or marker["schema_version"] != 1
                or not isinstance(marker.get("host_target"), str) or not marker["host_target"]):
            raise ValueError("installation marker is missing or unsupported")
        state = private_directory("state", parent=root)
        try:
            config = read_json(state, "config.json", private=True)
        finally:
            os.close(state)
    finally:
        os.close(root)
    credentials = (config.get("api_key"), config.get("api_hmac_secret"))
    if any(not isinstance(value, str) or not 1 <= len(value) <= 512
           or any(ord(character) < 33 or ord(character) > 126 for character in value)
           for value in credentials):
        raise ValueError("saved dashboard credentials are missing or invalid")
    return credentials


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, required=True)
    args = parser.parse_args()
    try:
        key, secret = read_credentials(args.installation)
    except (OSError, ValueError) as error:
        # Never include JSON contents or secret values in an error.
        parser.exit(1, f"Cannot read saved dashboard credentials: {error}\n")
    print(f"API key: {key}\nAPI secret: {secret}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
