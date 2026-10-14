#!/usr/bin/env python3
"""Build the authenticated SQLite 3.51.3 amalgamation for Linux Python.

Only the checksum-verified C file is compiled. No archive scripts are executed.
The existing interpreter/system installation is unchanged; print the new library
directory for LD_LIBRARY_PATH and verify it in a fresh Python process first.
"""
from __future__ import annotations

import argparse
import hashlib
import io
import os
from pathlib import Path
import platform
import shutil
import stat
import subprocess
import sys
import tempfile
import urllib.parse
import urllib.request
import zipfile
from native_tls_runtime import require_safe_tls

VERSION = '3.51.3'
URL = 'https://www.sqlite.org/2026/sqlite-amalgamation-3510300.zip'
ARCHIVE_SHA256 = 'acb1e6f5d832484bf6d32b681e858c38add8b2acdfd42ac5df24b8afb46552b4'
# Published by https://www.sqlite.org/releaselog/3_51_3.html.
SOURCE_SHA3_256 = '32d5424f97e0a7fc5ed2f6335afbb58be4e0298bd7117a34e39d345ff13d859e'
SOURCE_MEMBER = 'sqlite-amalgamation-3510300/sqlite3.c'
MAX_BYTES = 16 * 1024 * 1024


class HttpsRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        if urllib.parse.urlsplit(newurl).scheme != 'https':
            raise ValueError('Refusing a non-HTTPS SQLite download redirect')
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def read_archive(path: Path | None) -> bytes:
    if path is not None:
        if not stat.S_ISREG(path.lstat().st_mode) or path.stat().st_size > MAX_BYTES:
            raise ValueError('SQLite archive must be a bounded regular file')
        data = path.read_bytes()
    else:
        require_safe_tls(URL)
        request = urllib.request.Request(URL, headers={'User-Agent': 'macbot-sqlite-setup/1'})
        opener = urllib.request.build_opener(HttpsRedirects())
        with opener.open(request, timeout=30) as response:
            data = response.read(MAX_BYTES + 1)
    if len(data) > MAX_BYTES:
        raise ValueError('SQLite archive exceeds its supported size')
    return data


def verified_source(data: bytes) -> bytes:
    if hashlib.sha256(data).hexdigest() != ARCHIVE_SHA256:
        raise ValueError('SQLite archive checksum differs from the pinned release')
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        candidates = [item for item in archive.infolist() if item.filename == SOURCE_MEMBER]
        if len(candidates) != 1:
            raise ValueError('SQLite source is missing or duplicated')
        member = candidates[0]
        kind = stat.S_IFMT(member.external_attr >> 16)
        if member.is_dir() or kind not in (0, stat.S_IFREG) or member.file_size > MAX_BYTES:
            raise ValueError('SQLite source must be a bounded regular archive member')
        # Read one exact member; no archive paths or links are extracted.
        source = archive.read(member)
    if hashlib.sha3_256(source).hexdigest() != SOURCE_SHA3_256:
        raise ValueError('SQLite C source differs from the published release checksum')
    return source


def probe_runtime(library_directory: Path) -> None:
    environment = os.environ.copy()
    old_path = environment.get('LD_LIBRARY_PATH')
    environment['LD_LIBRARY_PATH'] = str(library_directory) + (':' + old_path if old_path else '')
    code = (
        'import sqlite3; '
        f'assert sqlite3.sqlite_version == {VERSION!r}, sqlite3.sqlite_version; '
        'connection = sqlite3.connect(":memory:"); '
        'connection.execute("CREATE TABLE probe (value INTEGER)"); '
        'connection.execute("INSERT INTO probe VALUES (7)"); '
        'assert connection.execute("SELECT value FROM probe").fetchone() == (7,); '
        'connection.close()'
    )
    subprocess.run([sys.executable, '-c', code], env=environment, check=True)


def provision(prefix: Path, archive_path: Path | None = None) -> Path:
    if platform.system() != 'Linux':
        raise ValueError('This optional SQLite provisioning helper supports Linux only')
    prefix = prefix.expanduser().absolute()
    if prefix.exists() or prefix.is_symlink():
        raise ValueError('SQLite installation prefix already exists; choose a new directory')
    source = verified_source(read_archive(archive_path))
    compiler = shutil.which('cc')
    if compiler is None:
        raise ValueError('A C compiler (cc) is required to provision SQLite')
    with tempfile.TemporaryDirectory(prefix='macbot-sqlite-build-') as temporary:
        staging = Path(temporary)
        c_file = staging / 'sqlite3.c'
        library = staging / 'libsqlite3.so.0'
        c_file.write_bytes(source)
        subprocess.run([
            compiler, '-shared', '-fPIC', '-O2', '-DSQLITE_THREADSAFE=1',
            '-DSQLITE_ENABLE_COLUMN_METADATA', '-Wl,-soname,libsqlite3.so.0',
            str(c_file), '-o', str(library), '-ldl', '-lpthread', '-lm',
        ], check=True)
        probe_runtime(staging)
        # mkdir refuses an existing prefix even if it appeared during the build.
        prefix.mkdir(parents=True, exist_ok=False, mode=0o700)
        directory = prefix / 'lib'
        try:
            directory.mkdir(mode=0o700)
            shutil.copyfile(library, directory / library.name)
            (directory / library.name).chmod(0o500)
            probe_runtime(directory)
        except BaseException:
            # Only remove the new, exclusively created directory owned by us.
            shutil.rmtree(prefix)
            raise
    return directory


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--prefix', required=True, type=Path)
    parser.add_argument('--archive', type=Path, help='optional cached official archive')
    args = parser.parse_args()
    try:
        directory = provision(args.prefix, args.archive)
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError, zipfile.BadZipFile) as error:
        print(f'SQLite provisioning failed: {error}', file=sys.stderr)
        return 1
    print(f'Authenticated SQLite {VERSION} built and verified in fresh Python.', file=sys.stderr)
    print(directory)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
