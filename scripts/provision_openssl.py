#!/usr/bin/env python3
"""Build checksum-pinned OpenSSL 3.5.9 LTS for an isolated Python runtime.

The bootstrap download uses the host's curl and its TLS library, so OS security
updates are still required. The verified upstream build scripts run only inside
a temporary source directory. Nothing replaces the system/Homebrew runtime.
"""
from __future__ import annotations

import argparse
import hashlib
import os
from pathlib import Path, PurePosixPath
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile

VERSION = '3.5.9'
URL = 'https://github.com/openssl/openssl/releases/download/openssl-3.5.9/openssl-3.5.9.tar.gz'
# Official release checksum linked from https://openssl-library.org/source/.
SHA256 = '603f5602e2eef00d77fbd429d34dcd5822bb301757a1bc9cdb24c670f1eb859a'
ARCHIVE_LIMIT = 80 * 1024 * 1024
SOURCE_LIMIT = 1024 * 1024 * 1024


def verified_extract(archive_path: Path, destination: Path) -> Path:
    if (archive_path.is_symlink() or not archive_path.is_file()
            or archive_path.stat().st_size > ARCHIVE_LIMIT):
        raise ValueError('OpenSSL archive must be a bounded regular file')
    digest = hashlib.sha256()
    with archive_path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    if digest.hexdigest() != SHA256:
        raise ValueError('OpenSSL archive checksum differs from the official pinned release')
    with tarfile.open(archive_path, 'r:gz') as archive:
        members = archive.getmembers()
        total = 0
        seen = set()
        for member in members:
            relative = PurePosixPath(member.name)
            if (relative.is_absolute() or '..' in relative.parts or not relative.parts
                    or relative.parts[0] != f'openssl-{VERSION}'
                    or member.name in seen or not (member.isdir() or member.isfile())):
                raise ValueError('OpenSSL source contains an unsafe, duplicate or linked archive member')
            seen.add(member.name)
            if member.size < 0 or member.size > 64 * 1024 * 1024:
                raise ValueError('OpenSSL source member exceeds the supported size')
            total += member.size
            if total > SOURCE_LIMIT:
                raise ValueError('OpenSSL source exceeds the supported total size')
        # Validation finishes before any member is written. Do not use tar's
        # link/path extraction behavior, even for this authenticated archive.
        for member in members:
            target = destination.joinpath(*PurePosixPath(member.name).parts)
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            source = archive.extractfile(member)
            if source is None:
                raise ValueError('Cannot read an authenticated source member')
            with source, target.open('xb') as output:
                shutil.copyfileobj(source, output)
            target.chmod(0o755 if member.mode & 0o111 else 0o644)
    return destination / f'openssl-{VERSION}'


def probe_runtime(directory: Path) -> None:
    environment = os.environ.copy()
    variable = 'DYLD_LIBRARY_PATH' if platform.system() == 'Darwin' else 'LD_LIBRARY_PATH'
    previous = environment.get(variable)
    environment[variable] = str(directory) + (':' + previous if previous else '')
    # Context construction also exercises provider/algorithm initialization.
    code = (
        'import ssl,hashlib; '
        f'assert ssl.OPENSSL_VERSION.split()[1] == {VERSION!r}, ssl.OPENSSL_VERSION; '
        'ssl.create_default_context(); '
        'assert hashlib.sha256(b"probe").hexdigest() == '
        '"ba9c736f19e7f60b7f6764adb0b7908c0a2b394e09b6c09863528c7f2bc86095"'
    )
    subprocess.run([sys.executable, '-c', code], env=environment, check=True)


def provision(prefix: Path, archive_path: Path | None = None, jobs: int = 2) -> Path:
    if platform.system() not in {'Linux', 'Darwin'}:
        raise ValueError('This optional OpenSSL provisioning helper supports Linux and macOS only')
    prefix = prefix.expanduser().absolute()
    if prefix.exists() or prefix.is_symlink():
        raise ValueError('OpenSSL installation prefix already exists; choose a new directory')
    if jobs < 1 or jobs > 16:
        raise ValueError('Build jobs must be between1 and16')
    for tool in ('cc', 'make', 'perl'):
        if shutil.which(tool) is None:
            raise ValueError(f'{tool} is required to provision OpenSSL')
    with tempfile.TemporaryDirectory(prefix='macbot-openssl-build-') as temporary:
        staging = Path(temporary)
        if archive_path is None:
            curl = shutil.which('curl')
            if curl is None:
                raise ValueError('curl is required for the native TLS bootstrap download')
            archive_path = staging / 'official-openssl.tar.gz'
            subprocess.run([
                curl, '-q', '--proto', '=https', '--proto-redir', '=https',
                '--tlsv1.2', '--fail', '--location', '--silent', '--show-error',
                '--max-time', '180', '--max-filesize', str(ARCHIVE_LIMIT),
                '--output', str(archive_path), URL,
            ], check=True)
        source = verified_extract(archive_path, staging)
        install_root = staging / 'installed'
        # Compile durable library/provider/install names, then stage under
        # DESTDIR. A temporary --prefix survives inside macOS dylib identities
        # and native module paths even after copying the files elsewhere.
        installed = install_root / prefix.relative_to(prefix.anchor)
        subprocess.run([
            'perl', str(source / 'Configure'), f'--prefix={prefix}',
            '--libdir=lib', '--openssldir=/etc/ssl', 'shared', 'no-tests',
        ], cwd=source, check=True, stdout=sys.stderr)
        subprocess.run(['make', f'-j{jobs}'], cwd=source, check=True, stdout=sys.stderr)
        subprocess.run(['make', 'install_sw', f'DESTDIR={install_root}'], cwd=source, check=True, stdout=sys.stderr)
        probe_runtime(installed / 'lib')
        prefix.mkdir(parents=True, exist_ok=False, mode=0o700)
        try:
            shutil.copytree(installed / 'lib', prefix / 'lib', symlinks=False)
            shutil.copyfile(source / 'LICENSE.txt', prefix / 'LICENSE.txt')
            # Keep the Linux system trust-store location at /etc/ssl. Do not
            # point SSL_CERT_FILE or OPENSSL_CONF into the temporary build.
        except BaseException:
            shutil.rmtree(prefix)
            raise
    try:
        # Probe only after the temporary installation has disappeared: this
        # must not pass by loading a dangling staged dylib/provider dependency.
        probe_runtime(prefix / 'lib')
    except BaseException:
        shutil.rmtree(prefix)
        raise
    return prefix / 'lib'


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--prefix', required=True, type=Path)
    parser.add_argument('--archive', type=Path, help='optional cached official archive')
    parser.add_argument('--jobs', type=int, default=2)
    args = parser.parse_args()
    try:
        directory = provision(args.prefix, args.archive, args.jobs)
    except (OSError, ValueError, subprocess.CalledProcessError, tarfile.TarError) as error:
        print(f'OpenSSL provisioning failed: {error}', file=sys.stderr)
        return 1
    print(f'Authenticated OpenSSL {VERSION} built and verified in fresh Python.', file=sys.stderr)
    print(directory)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
