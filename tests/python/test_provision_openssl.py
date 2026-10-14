"""No unverified or linked upstream archive can reach the native build."""
import hashlib
import io
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import provision_openssl as provision


class OpenSSLProvisionTests(unittest.TestCase):
    def archive(self, path, *, link=False, traversal=False):
        with tarfile.open(path, 'w:gz') as archive:
            member = tarfile.TarInfo('../outside' if traversal else 'openssl-3.5.9/Configure')
            if link:
                member.type = tarfile.SYMTYPE
                member.linkname = '../outside'
                archive.addfile(member)
            else:
                data = b'authenticated build source'
                member.size = len(data)
                archive.addfile(member, io.BytesIO(data))

    def test_published_checksum_required_before_extraction(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / 'source.tar.gz'
            self.archive(archive)
            destination = root / 'extracted'
            with self.assertRaisesRegex(ValueError, 'checksum'):
                provision.verified_extract(archive, destination)
            self.assertFalse(destination.exists())
            with patch.object(provision, 'SHA256', hashlib.sha256(archive.read_bytes()).hexdigest()):
                source = provision.verified_extract(archive, destination)
            self.assertEqual((source / 'Configure').read_bytes(), b'authenticated build source')

    def test_links_and_traversal_rejected_before_any_extract(self):
        for kwargs in ({'link': True}, {'traversal': True}):
            with self.subTest(kwargs=kwargs), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                archive = root / 'source.tar.gz'
                self.archive(archive, **kwargs)
                destination = root / 'extracted'
                with patch.object(provision, 'SHA256', hashlib.sha256(archive.read_bytes()).hexdigest()):
                    with self.assertRaisesRegex(ValueError, 'unsafe'):
                        provision.verified_extract(archive, destination)
                self.assertFalse(destination.exists())

    def test_existing_prefix_refused_before_build(self):
        with tempfile.TemporaryDirectory() as temporary, \
             patch.object(provision.platform, 'system', return_value='Linux'), \
             patch.object(provision.subprocess, 'run') as run:
            with self.assertRaisesRegex(ValueError, 'already exists'):
                provision.provision(Path(temporary))
            run.assert_not_called()

    def test_probe_uses_fresh_python_with_selected_library(self):
        with patch.object(provision.subprocess, 'run') as run, \
             patch.object(provision.platform, 'system', return_value='Linux'), \
             patch.dict(provision.os.environ, {'LD_LIBRARY_PATH': '/old'}):
            provision.probe_runtime(Path('/new/lib'))
        args, kwargs = run.call_args
        self.assertEqual(args[0][0], provision.sys.executable)
        self.assertIn(provision.VERSION, args[0][2])
        self.assertEqual(kwargs['env']['LD_LIBRARY_PATH'], '/new/lib:/old')
        self.assertTrue(kwargs['check'])

    def test_unsupported_platform_refused_before_build(self):
        with patch.object(provision.platform, 'system', return_value='Windows'), \
             patch.object(provision.subprocess, 'run') as run:
            with self.assertRaisesRegex(ValueError, 'Linux and macOS only'):
                provision.provision(Path('/unused'))
            run.assert_not_called()

    def test_native_install_identity_is_durable_and_final_probe_follows_cleanup(self):
        with tempfile.TemporaryDirectory() as temporary:
            prefix = Path(temporary) / 'durable'
            staged = []
            probes = []

            def extract(archive, staging):
                source = staging / 'openssl-3.5.9'
                source.mkdir()
                (source / 'LICENSE.txt').write_text('authenticated upstream license')
                return source

            def command(arguments, **kwargs):
                if arguments[:2] == ['make', 'install_sw']:
                    install_root = Path(arguments[2].split('=', 1)[1])
                    library = install_root / prefix.relative_to(prefix.anchor) / 'lib'
                    library.mkdir(parents=True)
                    (library / 'libssl.so.3').write_bytes(b'authenticated compiled output')
                    staged.append(install_root)

            def probe(directory):
                probes.append(directory)
                if directory == prefix / 'lib':
                    self.assertFalse(staged[0].exists(), 'final probe must not resolve a deleted temporary install')
                self.assertTrue((directory / 'libssl.so.3').is_file())

            with patch.object(provision.platform, 'system', return_value='Linux'), \
                 patch.object(provision.shutil, 'which', return_value='/usr/bin/tool'), \
                 patch.object(provision, 'verified_extract', side_effect=extract), \
                 patch.object(provision.subprocess, 'run', side_effect=command) as run, \
                 patch.object(provision, 'probe_runtime', side_effect=probe):
                self.assertEqual(provision.provision(prefix, Path('/verified-archive')), prefix / 'lib')
            configure = run.call_args_list[0].args[0]
            self.assertIn(f'--prefix={prefix}', configure)
            self.assertEqual(len(probes), 2)
            self.assertEqual(probes[-1], prefix / 'lib')
            self.assertEqual((prefix / 'LICENSE.txt').read_text(), 'authenticated upstream license')
