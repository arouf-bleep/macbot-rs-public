#!/usr/bin/env python3
"""Install the pinned official CPU model engine outside the source tree.

Print only its verified absolute library path to stdout. Downloads and cache
publication never execute archive content or follow archive links.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import stat
import sys
import tarfile
import tempfile
import urllib.parse
import urllib.request

from native_tls_runtime import require_safe_tls

MANIFEST = Path(__file__).with_name('onnx_runtime_manifest.json')


def host_target() -> str:
    machine = platform.machine().lower()
    architecture = {'arm64': 'aarch64', 'amd64': 'x86_64'}.get(machine, machine)
    system = {'Darwin': 'macos', 'Linux': 'linux'}.get(platform.system())
    if system is None:
        raise ValueError('Pinned native runtime supports macOS ARM64 and Linux x64/ARM64 only')
    return f'{system}-{architecture}'


def default_cache() -> Path:
    override = os.environ.get('MACBOT_ONNX_RUNTIME_CACHE')
    if override:
        return Path(override).expanduser().absolute()
    if platform.system() == 'Darwin':
        return Path.home() / 'Library' / 'Caches' / 'macbot'
    return Path(os.environ.get('XDG_CACHE_HOME') or Path.home() / '.cache') / 'macbot'


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


def regular_file(path: Path) -> bool:
    try:
        return stat.S_ISREG(path.lstat().st_mode)
    except FileNotFoundError:
        return False


def secure_directory(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    if path.is_symlink() or not path.is_dir():
        raise ValueError(f'Runtime cache must be a real directory: {path}')
    if os.name == 'posix':
        if path.stat().st_uid != os.getuid():
            raise ValueError(f'Runtime cache must be owned by this user: {path}')
        path.chmod(0o700)


class HttpsRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        if urllib.parse.urlsplit(newurl).scheme != 'https':
            raise ValueError('Refusing a non-HTTPS runtime download redirect')
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def download(entry: dict, destination: Path) -> None:
    if urllib.parse.urlsplit(entry['url']).scheme != 'https':
        raise ValueError('Runtime download must use HTTPS')
    require_safe_tls(entry['url'])
    opener = urllib.request.build_opener(HttpsRedirects())
    request = urllib.request.Request(entry['url'], headers={'User-Agent': 'macbot-runtime-setup/1'})
    with opener.open(request, timeout=30) as source, destination.open('wb') as output:
        received = 0
        while chunk := source.read(1024 * 1024):
            received += len(chunk)
            if received > entry['archive_bytes']:
                raise ValueError('Runtime archive exceeds its pinned size')
            output.write(chunk)
        output.flush()
        os.fsync(output.fileno())
    if received != entry['archive_bytes']:
        raise ValueError('Runtime archive size does not match the pinned release')


def verified_member(archive: tarfile.TarFile, name: str, expected_size: int | None = None) -> bytes:
    members = [member for member in archive.getmembers() if member.name == name]
    if len(members) != 1 or not members[0].isfile():
        raise ValueError(f'Required archive member is missing, duplicated, or not regular: {name}')
    member = members[0]
    if member.size < 0 or member.size > 100 * 1024 * 1024:
        raise ValueError('Runtime archive member exceeds the supported size')
    if expected_size is not None and member.size != expected_size:
        raise ValueError('Runtime library size differs from the pinned artifact')
    stream = archive.extractfile(member)
    if stream is None:
        raise ValueError('Cannot read runtime archive member')
    data = stream.read(member.size + 1)
    if len(data) != member.size:
        raise ValueError('Incomplete runtime archive member')
    return data


def atomic_write(path: Path, data: bytes) -> None:
    fd, temporary = tempfile.mkstemp(prefix='.verified-', dir=path.parent)
    staging = Path(temporary)
    try:
        with os.fdopen(fd, 'wb') as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        staging.chmod(0o444)
        os.replace(staging, path)
        if os.name == 'posix':
            directory_fd = os.open(path.parent, os.O_RDONLY)
            try:
                os.fsync(directory_fd)
            finally:
                os.close(directory_fd)
    finally:
        staging.unlink(missing_ok=True)


def prepare_runtime(manifest: dict, target: str, cache: Path, archive_path: Path | None = None) -> Path:
    entry = manifest['targets'].get(target)
    if entry is None:
        raise ValueError(f'No pinned official CPU runtime for {target}')
    cache = cache.expanduser().absolute()
    secure_directory(cache)
    installation = cache / 'onnxruntime' / manifest['version'] / target
    for directory in (cache / 'onnxruntime', cache / 'onnxruntime' / manifest['version'], installation):
        secure_directory(directory)
    library = installation / entry['library_name']
    if library.is_symlink():
        raise ValueError('Refusing a symbolic-link runtime library')
    if regular_file(library) and library.stat().st_size == entry['library_bytes'] and sha256(library) == entry['library_sha256'] and all(regular_file(installation / extra['name']) and (installation / extra['name']).stat().st_size == extra['bytes'] and sha256(installation / extra['name']) == extra['sha256'] for extra in entry.get('extra_libraries', []) + entry['notices']) and regular_file(installation / 'runtime-manifest.json') and (installation / 'runtime-manifest.json').read_bytes() == (json.dumps(manifest, indent=2) + '\n').encode():
        if os.name == 'posix':
            library.chmod(0o444)
        return library
    with tempfile.TemporaryDirectory(prefix='runtime-stage-', dir=cache) as staging:
        archive_file = archive_path
        if archive_file is None:
            archive_file = Path(staging) / 'official-runtime.tgz'
            print(f"Downloading official ONNX Runtime {manifest['version']} for {target}", file=sys.stderr)
            download(entry, archive_file)
        if not regular_file(archive_file) or archive_file.stat().st_size != entry['archive_bytes'] or sha256(archive_file) != entry['archive_sha256']:
            raise ValueError('Runtime archive fails the pinned size/SHA256 check')
        with tarfile.open(archive_file, 'r:gz') as archive:
            version = verified_member(archive, entry['archive_root'] + '/VERSION_NUMBER').decode('ascii').strip()
            if version != manifest['version']:
                raise ValueError('Runtime archive reports an unexpected version')
            library_bytes = verified_member(archive, entry['library_member'], entry['library_bytes'])
            if hashlib.sha256(library_bytes).hexdigest() != entry['library_sha256']:
                raise ValueError('Runtime library fails its pinned SHA256 check')
            extra_libraries = {}
            for extra in entry.get('extra_libraries', []):
                content = verified_member(archive, extra['member'], extra['bytes'])
                if hashlib.sha256(content).hexdigest() != extra['sha256']:
                    raise ValueError('Supplemental runtime library fails its pinned SHA256 check')
                extra_libraries[extra['name']] = content
            # Keep upstream licence notices alongside the separately installed runtime.
            notices = {}
            for notice in entry['notices']:
                content = verified_member(archive, notice['member'], notice['bytes'])
                if hashlib.sha256(content).hexdigest() != notice['sha256']:
                    raise ValueError('Runtime licence notice fails its pinned SHA256 check')
                notices[notice['name']] = content
        for name, content in notices.items():
            atomic_write(installation / name, content)
        for name, content in extra_libraries.items():
            atomic_write(installation / name, content)
        atomic_write(installation / 'runtime-manifest.json', (json.dumps(manifest, indent=2) + '\n').encode())
        atomic_write(library, library_bytes)
    return library


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--cache-dir', type=Path, default=default_cache())
    parser.add_argument('--archive', type=Path, help='offline official archive; still requires pinned size/SHA256')
    args = parser.parse_args()
    try:
        manifest = json.loads(MANIFEST.read_text())
        library = prepare_runtime(manifest, host_target(), args.cache_dir, args.archive)
    except (OSError, ValueError, RuntimeError, KeyError, tarfile.TarError) as error:
        print(f'ONNX Runtime setup failed: {error}', file=sys.stderr)
        return 1
    print(library)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
