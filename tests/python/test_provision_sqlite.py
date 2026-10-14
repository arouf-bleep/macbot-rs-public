"""Offline checks for the authenticated SQLite provisioning boundary."""
from __future__ import annotations

import hashlib
import contextlib
import io
from pathlib import Path
import stat
import tempfile
import unittest
from unittest import mock
import zipfile

import provision_sqlite as provision


class SQLiteProvisionTests(unittest.TestCase):
    def archive(self, source=b'verified C', *, duplicate=False, kind=stat.S_IFREG):
        stream = io.BytesIO()
        with zipfile.ZipFile(stream, 'w') as archive:
            member = zipfile.ZipInfo(provision.SOURCE_MEMBER)
            member.external_attr = (kind | 0o644) << 16
            archive.writestr(member, source)
            if duplicate:
                import warnings
                with warnings.catch_warnings():
                    warnings.simplefilter('ignore', UserWarning)
                    archive.writestr(member, source)
        return stream.getvalue()

    def test_two_published_checksums_are_required(self):
        data = self.archive()
        with mock.patch.object(provision, 'ARCHIVE_SHA256', hashlib.sha256(data).hexdigest()), \
             mock.patch.object(provision, 'SOURCE_SHA3_256', hashlib.sha3_256(b'verified C').hexdigest()):
            self.assertEqual(provision.verified_source(data), b'verified C')
            with self.assertRaisesRegex(ValueError, 'archive checksum'):
                provision.verified_source(data + b'tampered')
            with mock.patch.object(provision, 'SOURCE_SHA3_256', '0' * 64):
                with self.assertRaisesRegex(ValueError, 'published release checksum'):
                    provision.verified_source(data)

    def test_duplicate_and_link_sources_rejected(self):
        for data in (self.archive(duplicate=True), self.archive(kind=stat.S_IFLNK)):
            with self.subTest(), mock.patch.object(provision, 'ARCHIVE_SHA256', hashlib.sha256(data).hexdigest()):
                with self.assertRaises(ValueError):
                    provision.verified_source(data)

    def test_existing_prefix_never_downloaded_or_overwritten(self):
        with tempfile.TemporaryDirectory() as temporary, \
             mock.patch.object(provision.platform, 'system', return_value='Linux'), \
             mock.patch.object(provision, 'read_archive') as download:
            with self.assertRaisesRegex(ValueError, 'already exists'):
                provision.provision(Path(temporary))
            download.assert_not_called()

    def test_unsupported_platform_never_downloaded(self):
        with mock.patch.object(provision.platform, 'system', return_value='Darwin'), \
             mock.patch.object(provision, 'read_archive') as download:
            with self.assertRaisesRegex(ValueError, 'Linux only'):
                provision.provision(Path('/unused'))
            download.assert_not_called()

    def test_probe_uses_a_fresh_interpreter_and_selected_library(self):
        with mock.patch.object(provision.subprocess, 'run') as run, \
             mock.patch.dict(provision.os.environ, {'LD_LIBRARY_PATH': '/old'}):
            provision.probe_runtime(Path('/new/lib'))
        args, kwargs = run.call_args
        self.assertEqual(args[0][0], provision.sys.executable)
        self.assertIn(provision.VERSION, args[0][2])
        self.assertEqual(kwargs['env']['LD_LIBRARY_PATH'], '/new/lib:/old')
        self.assertTrue(kwargs['check'])

    def test_cli_unsafe_tls_refuses_download_with_concise_failure(self):
        import native_tls_runtime as native
        errors = io.StringIO()
        with tempfile.TemporaryDirectory() as temporary, \
             mock.patch.object(provision.platform, 'system', return_value='Linux'), \
             mock.patch.object(native.ssl, 'OPENSSL_VERSION', 'OpenSSL 3.6.1'), \
             mock.patch.dict(native.os.environ, {native.BACKPORT_ENV: ''}), \
             mock.patch.object(provision.sys, 'argv', ['provision_sqlite.py', '--prefix', str(Path(temporary) / 'new')]), \
             mock.patch.object(provision.urllib.request, 'build_opener') as transport, \
             contextlib.redirect_stderr(errors):
            self.assertEqual(provision.main(), 1)
            transport.assert_not_called()
        self.assertIn('SQLite provisioning failed:', errors.getvalue())
        self.assertIn('No HTTPS request', errors.getvalue())
        self.assertNotIn('Traceback', errors.getvalue())


if __name__ == '__main__':
    unittest.main()
