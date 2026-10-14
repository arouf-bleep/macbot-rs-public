"""Unsafe Python SQLite runtimes must fail before shared database access."""
from pathlib import Path
import sqlite3
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import sqlite_runtime


class SQLiteRuntimeTests(unittest.TestCase):
    def test_unsafe_writer_runtime_does_not_open_database(self):
        for version in ((3, 7, 0), (3, 45, 1), (3, 51, 2)):
            with self.subTest(version=version), \
                 patch.object(sqlite3, "sqlite_version_info", version), \
                 patch.object(sqlite3, "connect") as connect:
                with self.assertRaisesRegex(RuntimeError, "requires SQLite 3.51.3"):
                    sqlite_runtime.connect_for_write("must-not-exist.sqlite", operation="Test")
                connect.assert_not_called()

    def test_fixed_writer_runtime_opens_database(self):
        for version in ((3, 51, 3), (3, 52, 0), (4, 0, 0)):
            with self.subTest(version=version), \
                 patch.object(sqlite3, "sqlite_version_info", version), \
                 patch.object(sqlite3, "connect") as connect:
                self.assertIs(sqlite_runtime.connect_for_write("target.sqlite", operation="Test"), connect.return_value)
                connect.assert_called_once_with("target.sqlite")

    def test_writer_entry_points_refuse_before_database_or_network_access(self):
        try:
            import backfill_historical
            import backfill_outcomes
            import materialize_model_inputs
        except ModuleNotFoundError as error:
            if error.name not in {"requests", "numpy"}:
                raise
            self.skipTest("install optional training dependencies to check writer entry points")

        invocations = [
            (lambda: backfill_historical.init_db("must-not-exist.sqlite"), []),
            (backfill_outcomes.main, ["backfill_outcomes.py", "--db", "must-not-exist.sqlite"]),
            (materialize_model_inputs.main, ["materialize_model_inputs.py", "--db", "must-not-exist.sqlite"]),
        ]
        for run, arguments in invocations:
            with self.subTest(command=arguments or "historical backfill"), \
                 patch.object(sys, "argv", arguments), \
                 patch.object(sqlite3, "sqlite_version_info", (3, 51, 2)), \
                 patch.object(backfill_historical, "require_safe_tls"), \
                 patch.object(backfill_outcomes, "require_safe_tls"), \
                 patch.object(sqlite3, "connect") as connect, \
                 patch.object(backfill_outcomes, "fetch_market") as fetch:
                with self.assertRaisesRegex(RuntimeError, "requires SQLite 3.51.3"):
                    run()
                connect.assert_not_called()
                fetch.assert_not_called()

    def test_dry_run_uses_read_only_connection_on_older_runtime(self):
        import backfill_outcomes
        with patch.object(sys, "argv", ["backfill_outcomes.py", "--db", "inspect.sqlite", "--dry-run"]), \
             patch.object(sqlite3, "sqlite_version_info", (3, 51, 2)), \
             patch.object(backfill_outcomes, "require_safe_tls"), \
             patch.object(sqlite3, "connect") as connect:
            cursor = connect.return_value.execute.return_value
            cursor.fetchall.return_value = []
            cursor.fetchone.return_value = [0]
            backfill_outcomes.main()
            connect.assert_called_once_with(Path("inspect.sqlite").resolve().as_uri() + "?mode=ro", uri=True)
            self.assertTrue(connect.return_value.close.called)


if __name__ == "__main__":
    unittest.main()
