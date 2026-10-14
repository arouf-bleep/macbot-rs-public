import unittest
import contextlib
import io
from unittest import mock

import check_verification_environment as environment


class NativeSQLiteEnvironmentTests(unittest.TestCase):
    def test_outdated_installer_is_refused_before_pip_check(self):
        real_version = environment.metadata.version
        errors = io.StringIO()
        with mock.patch.object(environment, 'python_requirement_error', return_value=None), \
             mock.patch.object(environment, 'sqlite_requirement_error', return_value=None), \
             mock.patch.object(environment, 'tls_requirement_error', return_value=None), \
             mock.patch.object(environment.metadata, 'version', side_effect=lambda name: '26.0.1' if name == 'pip' else real_version(name)), \
             mock.patch.object(environment.subprocess, 'run') as run, \
             contextlib.redirect_stderr(errors):
            self.assertEqual(environment.main(), 1)
            run.assert_not_called()
        self.assertIn('pip: expected 26.2.1, found 26.0.1', errors.getvalue())

    def test_outdated_python_security_patch_is_refused(self):
        for version in ((3, 11, 15), (3, 11, 16), (3, 10, 20), (3, 12, 1)):
            with self.subTest(version=version), mock.patch.object(environment.sys, 'version_info', version):
                self.assertIn('3.11.17', environment.python_requirement_error())
        with mock.patch.object(environment.sys, 'version_info', (3, 11, 17)):
            self.assertIsNone(environment.python_requirement_error())

    def test_affected_native_sqlite_is_refused(self):
        with mock.patch.object(environment.sqlite3, 'sqlite_version_info', (3, 51, 2)), \
             mock.patch.object(environment.sqlite3, 'sqlite_version', '3.51.2'):
            message = environment.sqlite_requirement_error()
        self.assertIn('3.51.3 or newer', message)
        self.assertIn('3.51.2', message)

    def test_fixed_native_sqlite_is_accepted(self):
        for version in ((3, 51, 3), (3, 54, 0)):
            with self.subTest(version=version), \
                 mock.patch.object(environment.sqlite3, 'sqlite_version_info', version):
                self.assertIsNone(environment.sqlite_requirement_error())
