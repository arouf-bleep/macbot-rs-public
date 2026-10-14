"""Regressions for the operator source boundary and archive-only validation."""

import io
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import check_project_files
import package_source


class SourceBoundaryTests(unittest.TestCase):
    def test_each_verification_stage_receives_the_same_external_build_cache(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for settings in ({}, {'CARGO_TARGET_DIR': str(root / 'existing')},
                             {'MACBOT_VERIFY_TARGET_DIR': str(root / 'requested')}):
                with self.subTest(settings=settings), patch.dict(os.environ, settings, clear=True):
                    env = package_source.verification_environment(root)
                    self.assertEqual(env['CARGO_TARGET_DIR'], env['MACBOT_VERIFY_TARGET_DIR'])
                    self.assertTrue(Path(env['CARGO_TARGET_DIR']).is_absolute())
                    self.assertNotEqual(Path(env['CARGO_TARGET_DIR']), root / 'source')

    def test_verification_imports_leave_no_bytecode_in_the_delivery_tree(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'helper.py').write_text('value = 42\n')
            for setting in ('', '0', '1'):
                with self.subTest(setting=setting), patch.dict(
                    os.environ, {'PYTHONDONTWRITEBYTECODE': setting}
                ):
                    subprocess.run(
                        [sys.executable, '-c', 'import helper; assert helper.value == 42'],
                        cwd=root, env=package_source.verification_environment(root),
                        check=True,
                    )
                    self.assertEqual(
                        sorted(p.name for p in root.iterdir()), ['helper.py']
                    )

    def test_source_files_and_licence_texts_are_allowed(self):
        for path in (".env.example", "src/main.rs", "models/latest.onnx",
                     "static/vendor/licenses/bootstrap.txt"):
            self.assertFalse(package_source.excluded(path), path)

    def test_ignored_private_state_cannot_be_packaged(self):
        for path in (".env", "ops/.env.secret", "trades.db-wal", "ensemble_state.json",
                     "ml_weights.json", "notes/owner.md", "data/trades.csv",
                     "artifacts/report.json", "target.nosync/debug/macbot", "wallet.pem",
                     "training/__pycache__/model.pyc", ".venv-training/bin/python"):
            self.assertTrue(package_source.excluded(path), path)

    def test_archive_without_git_cannot_inherit_parent_ignore_rules(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / '.gitignore').write_text('ml_weights.json\n')
            (root / 'ml_weights.json').write_text('{}')
            with patch.object(check_project_files, 'ROOT', root), \
                 patch.object(check_project_files.subprocess, 'run') as run:
                self.assertFalse(check_project_files.is_ignored(root / 'ml_weights.json'))
                run.assert_not_called()

    def test_symlinks_and_sensitive_members_are_rejected(self):
        for name, kind in ((".env", tarfile.REGTYPE), ("src/link", tarfile.SYMTYPE),
                           ("../outside", tarfile.REGTYPE)):
            with self.subTest(name=name):
                buffer = io.BytesIO()
                with tarfile.open(fileobj=buffer, mode='w') as archive:
                    entry = tarfile.TarInfo(name)
                    entry.type = kind
                    entry.size = 0
                    archive.addfile(entry)
                with self.assertRaises(ValueError):
                    package_source.inspect_archive(buffer.getvalue())

    def test_manifest_hashes_actual_source_bytes(self):
        content = b'fn main() {}\n'
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode='w') as archive:
            entry = tarfile.TarInfo('src/main.rs')
            entry.size = len(content)
            entry.mode = 0o644
            archive.addfile(entry, io.BytesIO(content))
        entries = package_source.inspect_archive(buffer.getvalue())
        self.assertEqual(entries[0]['bytes'], len(content))
        import hashlib
        self.assertEqual(entries[0]['sha256'], hashlib.sha256(content).hexdigest())


if __name__ == '__main__':
    unittest.main()
