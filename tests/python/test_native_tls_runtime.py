"""Native TLS policy and refusal-before-transport regressions."""
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

import native_tls_runtime as native


class NativeTLSRuntimeTests(unittest.TestCase):
    def test_known_patched_branches_are_accepted(self):
        for version in ((3, 4, 8), (3, 5, 9), (3, 6, 5), (4, 0, 3)):
            with self.subTest(version=version), \
                 patch.object(native.ssl, 'OPENSSL_VERSION_INFO', (version[0], version[1], 0, version[2], 0)), \
                 patch.object(native.ssl, 'OPENSSL_VERSION_NUMBER', (version[0] << 28) | (version[1] << 20) | (version[2] << 4)), \
                 patch.object(native.ssl, 'OPENSSL_VERSION', 'OpenSSL ' + '.'.join(map(str, version))):
                self.assertIsNone(native.tls_requirement_error())

    def test_old_unsupported_or_unknown_native_versions_refused(self):
        for version in ((3, 4, 7), (3, 5, 8), (3, 6, 1), (4, 0, 2), (3, 0, 23), (4, 1, 0)):
            with self.subTest(version=version), \
                 patch.object(native.ssl, 'OPENSSL_VERSION_INFO', (*version, 0, 0)), \
                 patch.object(native.ssl, 'OPENSSL_VERSION', 'OpenSSL ' + '.'.join(map(str, version))), \
                 patch.dict(os.environ, {native.BACKPORT_ENV: ''}):
                self.assertIsNotNone(native.tls_requirement_error())
                with self.assertRaisesRegex(RuntimeError, 'No HTTPS request'):
                    native.require_safe_tls('https://example.invalid')
                native.require_safe_tls('http://127.0.0.1:8000')

    def test_backport_is_exact_explicit_operator_evidence(self):
        version = 'OpenSSL 3.0.13 distributor build'
        with tempfile.TemporaryDirectory() as directory, \
             patch.object(native.ssl, 'OPENSSL_VERSION_INFO', (3, 0, 13, 0, 0)), \
             patch.object(native.ssl, 'OPENSSL_VERSION', version):
            path = Path(directory) / 'backport.json'
            evidence = {'openssl_version': version, 'cve': 'CVE-2026-35189',
                        'source': 'https://distributor.invalid/advisory',
                        'reason': 'Operator checked the installed package revision against the distributor advisory'}
            with patch.dict(os.environ, {native.BACKPORT_ENV: str(path)}):
                self.assertIn('Invalid', native.tls_requirement_error())
                path.write_text(json.dumps(evidence))
                self.assertIsNone(native.tls_requirement_error())
                for key, wrong in [('openssl_version', 'different'), ('cve', 'different'),
                                   ('source', 'http://unverified.invalid'), ('reason', '')]:
                    with self.subTest(field=key):
                        path.write_text(json.dumps({**evidence, key: wrong}))
                        self.assertIn('Invalid', native.tls_requirement_error())

    def test_network_entry_points_refuse_before_https_transport(self):
        import api_auth
        import provision_sqlite
        import verify_trade_truth
        sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'training'))
        import backfill_outcomes
        with patch.object(native.ssl, 'OPENSSL_VERSION_INFO', (3, 6, 1, 0, 0)), \
             patch.object(native.ssl, 'OPENSSL_VERSION', 'OpenSSL 3.6.1'), \
             patch.dict(os.environ, {native.BACKPORT_ENV: ''}), \
             patch('urllib.request.urlopen') as urlopen, \
             patch('urllib.request.build_opener') as opener:
            calls = [lambda: api_auth.get_json('https://example.invalid'),
                     lambda: verify_trade_truth.http_json('https://example.invalid'),
                     lambda: backfill_outcomes.fetch_market('test'),
                     lambda: provision_sqlite.read_archive(None)]
            for run in calls:
                with self.subTest(endpoint=run), self.assertRaisesRegex(RuntimeError, 'No HTTPS request'):
                    run()
            urlopen.assert_not_called()
            opener.assert_not_called()


if __name__ == '__main__':
    unittest.main()
