#!/usr/bin/env python3
"""Offline regressions for the hash-pinned native model-engine installer."""
from __future__ import annotations

from contextlib import redirect_stderr, redirect_stdout
import hashlib
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest import mock

import setup_onnx_runtime as runtime


class RuntimeInstallerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='macbot-native-installer-test-')
        self.root = Path(self.temporary.name)
        self.archive = self.root / 'official.tgz'
        self.cache = self.root / 'cache'
        self.content = {
            'official/VERSION_NUMBER': b'1.31.0\n',
            'official/lib/runtime.dylib': b'pinned native engine',
            'official/lib/shared.so': b'pinned supplemental engine',
            'official/LICENSE': b'official licence',
            'official/ThirdPartyNotices.txt': b'official third-party notices',
        }
        self.write_archive()
        self.manifest = {
            'schema_version': 1, 'version': '1.31.0', 'git_commit': 'official-revision',
            'targets': {'test-cpu': {
                'url': 'https://example.invalid/official.tgz', 'archive_root': 'official',
                'archive_sha256': runtime.sha256(self.archive),
                'archive_bytes': self.archive.stat().st_size,
                'library_name': 'runtime.dylib', 'library_member': 'official/lib/runtime.dylib',
                'library_bytes': len(self.content['official/lib/runtime.dylib']),
                'library_sha256': hashlib.sha256(self.content['official/lib/runtime.dylib']).hexdigest(),
                'extra_libraries': [self.file_metadata('shared.so', 'official/lib/shared.so')],
                'notices': [self.file_metadata(name, 'official/' + name)
                            for name in ('LICENSE', 'ThirdPartyNotices.txt')],
            }}}

    def tearDown(self):
        self.temporary.cleanup()

    def file_metadata(self, name, member):
        data = self.content[member]
        return {'name': name, 'member': member, 'bytes': len(data),
                'sha256': hashlib.sha256(data).hexdigest()}

    def write_archive(self, extra=None, replace=None):
        with tarfile.open(self.archive, 'w:gz') as archive:
            for name, data in self.content.items():
                if name == replace:
                    member = tarfile.TarInfo(name)
                    member.type = tarfile.SYMTYPE
                    member.linkname = '/untrusted/native-library'
                    archive.addfile(member)
                else:
                    member = tarfile.TarInfo(name)
                    member.size = len(data)
                    archive.addfile(member, io.BytesIO(data))
            if extra is not None:
                member = tarfile.TarInfo(extra)
                member.size = 7
                archive.addfile(member, io.BytesIO(b'outside'))

    def repin_archive(self):
        entry = self.manifest['targets']['test-cpu']
        entry['archive_sha256'] = runtime.sha256(self.archive)
        entry['archive_bytes'] = self.archive.stat().st_size

    def prepare(self):
        return runtime.prepare_runtime(self.manifest, 'test-cpu', self.cache, self.archive)

    def test_verified_archive_installs_only_pinned_files_and_retains_notices(self):
        self.write_archive(extra='../outside')
        self.repin_archive()
        library = self.prepare()
        self.assertEqual(library.read_bytes(), self.content['official/lib/runtime.dylib'])
        self.assertFalse((self.root / 'outside').exists())
        self.assertEqual({p.name for p in library.parent.iterdir()},
                         {'runtime.dylib', 'shared.so', 'LICENSE', 'ThirdPartyNotices.txt', 'runtime-manifest.json'})
        self.assertEqual(json.loads((library.parent / 'runtime-manifest.json').read_text()), self.manifest)

    def test_cache_fast_path_rechecks_native_and_notice_bytes(self):
        library = self.prepare()
        with mock.patch.object(runtime, 'download', side_effect=AssertionError('cache should avoid download')):
            self.assertEqual(runtime.prepare_runtime(self.manifest, 'test-cpu', self.cache), library)
        for name in ('runtime.dylib', 'shared.so', 'LICENSE', 'ThirdPartyNotices.txt'):
            file = library.parent / name
            file.chmod(0o600)
            file.write_bytes(b'tampered')
            self.prepare()
            entry = self.manifest['targets']['test-cpu']
            metadata = next((f for f in entry['notices'] + entry['extra_libraries'] if f['name'] == name), None)
            expected = metadata['sha256'] if metadata else entry['library_sha256']
            self.assertEqual(runtime.sha256(file), expected)

    def test_missing_notice_does_not_count_as_a_complete_cached_runtime(self):
        library = self.prepare()
        (library.parent / 'LICENSE').unlink()
        self.prepare()
        self.assertTrue((library.parent / 'LICENSE').is_file())

    def test_archive_hash_failure_prevents_parsing_or_publication(self):
        self.archive.write_bytes(b'not a verified archive')
        with mock.patch.object(runtime.tarfile, 'open', side_effect=AssertionError('must verify before parsing')):
            with self.assertRaisesRegex(ValueError, 'size/SHA256'):
                self.prepare()
        self.assertFalse(any(self.cache.rglob('runtime.dylib')))

    def test_library_hash_failure_prevents_publication(self):
        self.manifest['targets']['test-cpu']['library_sha256'] = '0' * 64
        with self.assertRaisesRegex(ValueError, 'library fails'):
            self.prepare()
        self.assertFalse(any(self.cache.rglob('runtime.dylib')))

    def test_archive_library_link_is_refused(self):
        self.write_archive(replace='official/lib/runtime.dylib')
        self.repin_archive()
        with self.assertRaisesRegex(ValueError, 'not regular'):
            self.prepare()

    def test_duplicate_library_member_is_refused(self):
        self.write_archive(extra='official/lib/runtime.dylib')
        self.repin_archive()
        with self.assertRaisesRegex(ValueError, 'duplicated'):
            self.prepare()

    def test_runtime_cache_and_library_symlinks_are_refused(self):
        if not hasattr(Path, 'symlink_to'):
            self.skipTest('platform lacks symbolic links')
        self.cache.symlink_to(self.root, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, 'real directory'):
            self.prepare()
        self.cache.unlink()
        library = self.prepare()
        library.unlink()
        library.symlink_to(self.root / 'untrusted')
        with self.assertRaisesRegex(ValueError, 'symbolic-link'):
            self.prepare()

    def test_unpinned_target_is_refused(self):
        with self.assertRaisesRegex(ValueError, 'No pinned'):
            runtime.prepare_runtime(self.manifest, 'unsupported-cpu', self.cache, self.archive)

    def test_host_target_normalizes_supported_platform_architecture_names(self):
        cases = (
            ('Darwin', 'arm64', 'macos-aarch64'),
            ('Linux', 'arm64', 'linux-aarch64'),
            ('Linux', 'aarch64', 'linux-aarch64'),
            ('Linux', 'AMD64', 'linux-x86_64'),
            ('Linux', 'x86_64', 'linux-x86_64'),
        )
        manifest = json.loads(runtime.MANIFEST.read_text())
        for system, machine, expected in cases:
            with self.subTest(system=system, machine=machine):
                with mock.patch.object(runtime.platform, 'system', return_value=system):
                    with mock.patch.object(runtime.platform, 'machine', return_value=machine):
                        target = runtime.host_target()
                self.assertEqual(target, expected)
                self.assertIn(target, manifest['targets'])

    def test_unsupported_system_fails_before_installing_a_runtime(self):
        with mock.patch.object(runtime.platform, 'system', return_value='Windows'):
            with self.assertRaisesRegex(ValueError, 'supports'):
                runtime.host_target()

    def test_linux_arm_cli_selects_its_own_verified_cache_without_loading_code(self):
        # Exercise real host resolution and CLI installation using harmless bytes.
        # This must never load a native Linux library into the test host.
        self.manifest['targets']['linux-aarch64'] = self.manifest['targets'].pop('test-cpu')
        manifest_path = self.root / 'manifest.json'
        manifest_path.write_text(json.dumps(self.manifest))
        stdout, stderr = io.StringIO(), io.StringIO()
        with mock.patch.object(runtime, 'MANIFEST', manifest_path):
            with mock.patch.object(runtime.platform, 'system', return_value='Linux'):
                with mock.patch.object(runtime.platform, 'machine', return_value='arm64'):
                    with mock.patch.object(runtime.sys, 'argv', ['setup', '--cache-dir', str(self.cache), '--archive', str(self.archive)]):
                        with mock.patch.object(runtime, 'download', side_effect=AssertionError('offline installation must not download')):
                            with redirect_stdout(stdout), redirect_stderr(stderr):
                                self.assertEqual(runtime.main(), 0)
        library = Path(stdout.getvalue().strip())
        self.assertEqual(library.parent.name, 'linux-aarch64')
        self.assertEqual(library.read_bytes(), self.content['official/lib/runtime.dylib'])
        self.assertEqual((library.parent / 'shared.so').read_bytes(), self.content['official/lib/shared.so'])
        self.assertEqual(stderr.getvalue(), '')
        self.assertFalse((library.parent.parent / 'linux-x86_64').exists())

    def test_linux_unpinned_architecture_cli_refuses_without_network_or_cache(self):
        stdout, stderr = io.StringIO(), io.StringIO()
        with mock.patch.object(runtime.platform, 'system', return_value='Linux'):
            with mock.patch.object(runtime.platform, 'machine', return_value='riscv64'):
                with mock.patch.object(runtime.sys, 'argv', ['setup', '--cache-dir', str(self.cache)]):
                    with mock.patch.object(runtime, 'download', side_effect=AssertionError('unpinned targets must not download')):
                        with redirect_stdout(stdout), redirect_stderr(stderr):
                            self.assertEqual(runtime.main(), 1)
        self.assertEqual(stdout.getvalue(), '')
        self.assertIn('No pinned official CPU runtime for linux-riscv64', stderr.getvalue())
        self.assertFalse(self.cache.exists())

    def test_supplemental_library_hash_failure_prevents_any_publication(self):
        self.manifest['targets']['test-cpu']['extra_libraries'][0]['sha256'] = '0' * 64
        with self.assertRaisesRegex(ValueError, 'Supplemental runtime library fails'):
            self.prepare()
        self.assertFalse(any(self.cache.rglob('runtime.dylib')))
        self.assertFalse(any(self.cache.rglob('shared.so')))

    def test_notice_hash_failure_prevents_any_publication(self):
        self.manifest['targets']['test-cpu']['notices'][0]['sha256'] = '0' * 64
        with self.assertRaisesRegex(ValueError, 'licence notice fails'):
            self.prepare()
        self.assertFalse(any(self.cache.rglob('runtime.dylib')))
        self.assertFalse(any(self.cache.rglob('shared.so')))

    def test_unsafe_tls_runtime_is_refused_before_network_io(self):
        entry = self.manifest['targets']['test-cpu']
        with mock.patch.object(runtime, 'require_safe_tls', side_effect=RuntimeError('unsafe native TLS')):
            with mock.patch.object(runtime.urllib.request, 'build_opener', side_effect=AssertionError('network must not start')):
                with self.assertRaisesRegex(RuntimeError, 'unsafe native TLS'):
                    runtime.download(entry, self.root / 'download')
        self.assertFalse((self.root / 'download').exists())

    def test_cli_tls_refusal_returns_failure_without_printing_a_library_path(self):
        manifest_path = self.root / 'manifest.json'
        manifest_path.write_text(json.dumps(self.manifest))
        stdout, stderr = io.StringIO(), io.StringIO()
        with mock.patch.object(runtime, 'MANIFEST', manifest_path), mock.patch.object(runtime, 'host_target', return_value='test-cpu'):
            with mock.patch.object(runtime.sys, 'argv', ['setup', '--cache-dir', str(self.cache)]):
                with mock.patch.object(runtime, 'require_safe_tls', side_effect=RuntimeError('unsafe native TLS')):
                    with redirect_stdout(stdout), redirect_stderr(stderr):
                        self.assertEqual(runtime.main(), 1)
        self.assertEqual(stdout.getvalue(), '')
        self.assertIn('ONNX Runtime setup failed: unsafe native TLS', stderr.getvalue())

    def test_https_redirect_downgrade_is_refused(self):
        handler = runtime.HttpsRedirects()
        request = runtime.urllib.request.Request('https://example.invalid/release')
        with self.assertRaisesRegex(ValueError, 'non-HTTPS'):
            handler.redirect_request(request, None, 302, 'redirect', {}, 'http://example.invalid/native')


if __name__ == '__main__':
    unittest.main()
