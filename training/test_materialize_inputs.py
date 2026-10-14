"""One malformed historical row must not abort otherwise usable reconstruction."""
import contextlib
from datetime import datetime, timedelta, timezone
import io
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch

try:
    import materialize_model_inputs as materialize
except ModuleNotFoundError as error:
    if error.name != "numpy":
        raise
    materialize = None


@unittest.skipIf(materialize is None, "install numpy to check reconstructed inputs")
class MaterializeInputTests(unittest.TestCase):
    def test_malformed_context_staleness_and_probabilities_are_skipped(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "history.sqlite"
            start = datetime(2026, 1, 1, tzinfo=timezone.utc)
            timestamp = (start + timedelta(minutes=120)).isoformat()
            with contextlib.closing(sqlite3.connect(path)) as conn:
                conn.execute("CREATE TABLE ml_features_timeseries(timestamp,run_id,market_id,features_json,fusion_score,p_up,signal,regime,phase,outcome,staleness_json)")
                conn.execute("CREATE TABLE ml_candles_1m(timestamp,open,high,low,close,volume)")
                conn.executemany("INSERT INTO ml_candles_1m VALUES(?,100,101,99,100,10)",
                                 [((start + timedelta(minutes=minute)).isoformat(),) for minute in range(120)])
                cases = [
                    ("bad-context", {"balance": None}, .5, .5, [0] * 34),
                    ("scalar-staleness", {}, .5, .5, 0),
                    ("nested-staleness", {}, .5, .5, [[0]] * 34),
                    ("invalid-staleness", {}, .5, .5, ["invalid"]),
                    ("nonfinite-score", {}, float("inf"), .5, [0] * 34),
                    ("invalid-probability", {}, .5, 2, [0] * 34),
                    ("valid", {}, .5, 0, [0] * 34),
                ]
                conn.executemany("INSERT INTO ml_features_timeseries VALUES(?,?,?,?,?,?,'BUY_NO','TREND_DOWN','MID',0,?)",
                                 [(timestamp, run, "market", json.dumps(features), score, probability, json.dumps(staleness))
                                  for run, features, score, probability, staleness in cases])
                conn.commit()
            with patch.object(sys, "argv", ["materialize_model_inputs.py", "--db", str(path)]), \
                 contextlib.redirect_stdout(io.StringIO()) as output:
                materialize.main()
            with contextlib.closing(sqlite3.connect(path)) as conn:
                result = conn.execute("SELECT run_id,p_up FROM ml_model_inputs").fetchall()
            self.assertEqual(result, [("reconstructed:valid", 0)])
            self.assertIn("Skipped 6", output.getvalue())


if __name__ == "__main__":
    unittest.main()
