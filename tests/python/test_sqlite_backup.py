"""Safety and consistent snapshot tests; no real runtime database is used."""
from contextlib import closing
from pathlib import Path
import os
import re
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

import sqlite_backup as backup


class BackupTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.source = self.root / 'source.sqlite'
        self.dest = self.root / 'backup.sqlite'
        with closing(sqlite3.connect(self.source)) as con:
            con.execute('PRAGMA journal_mode=WAL')
            con.execute(f'PRAGMA user_version={backup.SCHEMA_VERSION}')
            for table in backup.CRITICAL_TABLES:
                con.execute(f'CREATE TABLE {table}(id INTEGER PRIMARY KEY, state TEXT)')
                con.execute(f"INSERT INTO {table} VALUES(1,'preserve-me')")
            con.commit()

    def test_same_file_and_aliases_never_remove_source(self):
        original = self.source.read_bytes()
        for alias in (self.source, self.root / 'symlink.sqlite', self.root / 'hardlink.sqlite'):
            if alias.name == 'symlink.sqlite':
                alias.symlink_to(self.source)
            elif alias.name == 'hardlink.sqlite':
                os.link(self.source, alias)
            with self.subTest(alias=alias.name), self.assertRaisesRegex(ValueError, 'same database'):
                backup.create_backup(self.source, alias, overwrite=True)
            self.assertEqual(self.source.read_bytes(), original)

    def test_populated_ledgers_and_version_are_preserved(self):
        result = backup.create_backup(self.source, self.dest)
        self.assertEqual(result['schema_version'], backup.SCHEMA_VERSION)
        self.assertEqual(set(result['critical_counts']), set(backup.CRITICAL_TABLES))
        self.assertIn('meta', result['critical_counts'])
        self.assertIn('redemption_receipts', result['critical_counts'])
        with closing(sqlite3.connect(self.dest)) as con:
            for table in backup.CRITICAL_TABLES:
                self.assertEqual(con.execute(f'SELECT state FROM {table}').fetchall(), [('preserve-me',)])

    def test_failed_validation_preserves_previous_backup(self):
        self.dest.write_bytes(b'previous backup')
        for problem in ('schema', 'missing_table', 'corrupt'):
            with self.subTest(problem=problem):
                if problem == 'corrupt':
                    self.source.write_bytes(b'not sqlite')
                else:
                    with closing(sqlite3.connect(self.source)) as con:
                        if problem == 'schema': con.execute('PRAGMA user_version=0')
                        else:
                            con.execute(f'PRAGMA user_version={backup.SCHEMA_VERSION}')
                            con.execute('DROP TABLE maker_orders')
                        con.commit()
                with self.assertRaises((ValueError, sqlite3.Error)):
                    backup.create_backup(self.source, self.dest, overwrite=True)
                self.assertEqual(self.dest.read_bytes(), b'previous backup')
                self.assertFalse(list(self.root.glob('.macbot-backup-*')))

    def test_missing_source_is_not_created(self):
        missing = self.root / 'missing.sqlite'
        with self.assertRaises(FileNotFoundError): backup.create_backup(missing, self.dest)
        self.assertFalse(missing.exists())
        self.assertFalse(self.dest.exists())

    def test_overwrite_replaces_only_after_success(self):
        self.dest.write_bytes(b'old backup')
        with self.assertRaises(ValueError): backup.create_backup(self.source, self.dest)
        backup.create_backup(self.source, self.dest, overwrite=True)
        with closing(sqlite3.connect(self.dest)) as con:
            self.assertEqual(con.execute('SELECT state FROM live_orders').fetchone()[0], 'preserve-me')

    def test_active_destination_sidecars_are_refused(self):
        self.dest.write_bytes(b'previous backup')
        Path(str(self.dest) + '-wal').write_bytes(b'active')
        with self.assertRaisesRegex(ValueError, 'sidecars'):
            backup.create_backup(self.source, self.dest, overwrite=True)
        self.assertEqual(self.dest.read_bytes(), b'previous backup')

    def test_writes_during_backup_do_not_change_verified_snapshot(self):
        count = backup.critical_table_counts
        def counts_then_write(con):
            result = count(con)
            if not self.dest.exists():
                with closing(sqlite3.connect(self.source)) as writer:
                    writer.execute("INSERT OR IGNORE INTO maker_orders VALUES(2,'newer')")
                    writer.commit()
            return result
        with patch.object(backup, 'critical_table_counts', counts_then_write):
            result = backup.create_backup(self.source, self.dest)
        self.assertEqual(result['critical_counts']['maker_orders'], 1)
        with closing(sqlite3.connect(self.source)) as con:
            self.assertEqual(con.execute('SELECT COUNT(*) FROM maker_orders').fetchone()[0], 2)

    def test_schema_and_table_list_match_rust_contract(self):
        source = (Path(__file__).resolve().parents[2] / 'src/db/mod.rs').read_text()
        self.assertEqual(int(re.search(r'const DB_SCHEMA_VERSION: i64 = (\d+)', source)[1]), backup.SCHEMA_VERSION)
        section = source.split('pub async fn critical_table_counts', 1)[1].split('pub async fn backup_database', 1)[0]
        self.assertEqual(set(re.findall(r'count\(pool, "([a-z_]+)"\)', section)), set(backup.CRITICAL_TABLES))


if __name__ == '__main__': unittest.main()
