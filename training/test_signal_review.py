"""Regression checks for outcome truth, reconstruction and purged diagnostics."""

import contextlib
import datetime
import io
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch

import numpy as np
import backfill_historical as historical
import backfill_outcomes as outcomes
import materialize_model_inputs as materialize
import sanity_check
import train


class SignalReviewTests(unittest.TestCase):
    def test_outcomes_require_a_single_final_named_winner(self):
        self.assertEqual(historical.determine_outcome({"closed": True, "outcomes": '["Down","Up"]', "outcomePrices": '["0","1"]'}), 1.0)
        self.assertIsNone(historical.determine_outcome({"closed": True, "outcomes": ["Up", "Down"], "outcomePrices": [0.95, 0.05]}))
        self.assertIsNone(historical.determine_outcome({"closed": False, "outcomes": ["Up", "Down"], "outcomePrices": [1, 0]}))
        self.assertIsNone(outcomes.determine_outcome({"closed": True, "tokens": [{"winner": True, "outcome": None}]}))
        self.assertIsNone(outcomes.determine_outcome({"closed": True, "tokens": [{"winner": True, "outcome": "Up"}, {"winner": True, "outcome": "Down"}]}))
        self.assertEqual(outcomes.determine_outcome({"closed": True, "tokens": [{"winner": True, "outcome": "Down"}]}), 0.0)

    def test_cold_padding_does_not_create_zero_price_low_or_change_tensor_shape(self):
        candles = np.zeros((120, 5), dtype=np.float32)
        candles[-7:] = [[100, 102, 99, 101, 3]] * 7
        for aggregate in [materialize._aggregate_candles, train._aggregate_candles]:
            bars = aggregate(candles, 5)
            self.assertEqual(bars.shape, (24, 5))
            np.testing.assert_array_equal(bars[-2], [100, 102, 99, 101, 6])
            np.testing.assert_array_equal(bars[-1], [100, 102, 99, 101, 15])

    def test_failed_candle_pagination_is_bounded(self):
        class Response:
            status_code = 200
            def json(self):
                return [[0, '100', '102', '99', '101', '3', 0]]
        with patch.object(historical.requests, "get", return_value=Response()) as request, patch.object(historical.time, "sleep"), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaisesRegex(RuntimeError, "failed repeatedly"):
                historical.prefetch_all_candles(1000, 100000)
            self.assertEqual(request.call_count, 3)

    def test_diagnostic_folds_purge_shared_market_outcomes_and_horizon(self):
        origin = datetime.datetime(2026, 1, 1, tzinfo=datetime.timezone.utc)
        metadata = [{"timestamp": (origin + datetime.timedelta(minutes=30 * i)).isoformat(), "market_id": f"market-{i // 3}"} for i in range(120)]
        folds = list(sanity_check.evaluation_folds(metadata))
        self.assertEqual(len(folds), 5)
        for development, validation in folds:
            self.assertTrue({metadata[i]["market_id"] for i in development}.isdisjoint({metadata[i]["market_id"] for i in validation}))
            last = datetime.datetime.fromisoformat(metadata[development[-1]]["timestamp"])
            first = datetime.datetime.fromisoformat(metadata[validation[0]]["timestamp"])
            self.assertGreater((first - last).total_seconds(), 900)

    def test_reconstruction_is_idempotent_and_preserves_runtime_rows(self):
        with tempfile.TemporaryDirectory() as directory:
            db_path = str(Path(directory) / "history.db")
            connection = historical.init_db(db_path)
            materialize.ensure_model_input_schema(connection)
            origin = datetime.datetime(2026, 1, 1, tzinfo=datetime.timezone.utc)
            for i in range(100):
                timestamp = (origin + datetime.timedelta(minutes=i)).isoformat()
                connection.execute("INSERT INTO ml_candles_1m(timestamp,open,high,low,close,volume,market_id) VALUES(?,?,?,?,?,?,?)", (timestamp, 100, 102, 99, 101, 3, ""))
            feature_ts = (origin + datetime.timedelta(minutes=90)).isoformat()
            connection.execute("INSERT INTO ml_features_timeseries(timestamp,run_id,market_id,features_json,fusion_score,p_up,signal,regime,phase,outcome) VALUES(?,?,?,?,?,?,?,?,?,?)", (feature_ts, "backfill_test", "market-1", json.dumps({"orderbook_imbalance": 0.2}), 50, 0.6, "BUY_YES", "TREND_UP", "MID", 1))
            connection.commit()
            connection.close()
            arguments = ["materialize_model_inputs.py", "--db", db_path]
            with patch.object(sys, "argv", arguments), contextlib.redirect_stdout(io.StringIO()):
                materialize.main()
                materialize.main()
            with sqlite3.connect(db_path) as connection:
                self.assertEqual(connection.execute("SELECT COUNT(*) FROM ml_model_inputs").fetchone()[0], 1)
                dataset = train.BTCDataset(db_path, source="exact_inputs")
                self.assertEqual(dataset.source_kind, "reconstructed_inputs")
                connection.execute("UPDATE ml_model_inputs SET run_id='runtime-archive'")
                connection.commit()
            with patch.object(sys, "argv", arguments + ["--clear-existing"]), contextlib.redirect_stdout(io.StringIO()):
                materialize.main()
            with sqlite3.connect(db_path) as connection:
                self.assertEqual(connection.execute("SELECT COUNT(*) FROM ml_model_inputs WHERE run_id='runtime-archive'").fetchone()[0], 1)
                self.assertEqual(connection.execute("SELECT COUNT(*) FROM ml_model_inputs WHERE run_id='reconstructed:backfill_test'").fetchone()[0], 1)


if __name__ == "__main__":
    unittest.main()
