"""Offline regressions for absent synthetic trade evidence in reports."""
from __future__ import annotations

import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import backtest_trade_report_common as common
import lane_comparison_report as comparison
import lane_forensics_report as forensics


class BacktestReportEvidenceTests(unittest.TestCase):
    def test_no_sample_has_zero_count_and_total_but_no_rates_or_averages(self):
        for result in (common.summary_from_trades([]), common.BucketStats().as_json("empty")):
            self.assertEqual(result["trades"], 0)
            self.assertEqual(result["total_pnl"], 0.0)
            for field in ("win_rate", "break_even_win_rate_pct", "edge_gap_pp", "avg_pnl", "avg_cost_pct"):
                self.assertIsNone(result[field], field)

    def test_missing_path_is_not_a_zero_excursion(self):
        for key in ("path_upside_pct", "path_downside_pct"):
            with self.assertRaises(ValueError):
                common.numeric_value({"entry_price": .5}, key)
            self.assertIsNone(common.optional_numeric_value({"entry_price": .5}, key))
        self.assertEqual(common.numeric_value({"entry_price": .5, "path_low_price": 0.0}, "path_downside_pct"), 100.0)
        for invalid in (True, float("nan"), float("inf"), "bad"):
            self.assertIsNone(common.optional_numeric_value({"score": invalid}, "score"))

    def test_incomplete_costs_do_not_manufacture_complete_break_even_evidence(self):
        trades = [{"won": True, "stake": 1.0, "total_cost": .02, "pnl": 1.0},
                  {"won": False, "stake": 1.0, "pnl": -1.0}]
        result = common.summary_from_trades(trades)
        self.assertEqual(result["win_rate"], 50.0)
        for field in ("avg_cost_pct", "break_even_win_rate_pct", "edge_gap_pp"):
            self.assertIsNone(result[field], field)
        rows = common.summarize_buckets(trades, lambda _: "both")
        self.assertIsNone(rows[0]["edge_gap_pp"])

    def test_empty_lane_writers_render_unavailable(self):
        summary = common.summary_from_trades([])
        with TemporaryDirectory() as tmp:
            report = {"generated_at_utc": "fixture", "presets": ["empty"], "days": 1,
                      "shift_days": 0, "lanes": [{"preset": "empty", "summary": summary}],
                      "comparisons": []}
            _, markdown = comparison.write_outputs(report, Path(tmp))
            self.assertIn("unavailable", markdown.read_text())
            report = {"generated_at_utc": "fixture", "api_base": "http://localhost", "lane_label": "empty",
                      "summary": summary, "by_session": [], "by_hour_utc": [],
                      "by_volatility_bucket": [], "by_confidence_margin_bucket": [],
                      "by_edge_margin_bucket": [], "worst_trades": [], "best_trades": []}
            _, markdown = forensics.write_outputs(report, Path(tmp), "fixture")
            self.assertIn("unavailable", markdown.read_text())

    def test_summary_delta_does_not_replace_absence_with_zero(self):
        self.assertIsNone(common.summary_metric_delta({"win_rate": None}, {"win_rate": 50.0}, "win_rate"))
        self.assertEqual(common.summary_metric_delta({"total_pnl": 0.0}, {"total_pnl": 0.0}, "total_pnl"), 0.0)

    def test_unknown_inputs_are_separate_from_zero_groups_and_rankings(self):
        trades = [{"id": 1, "won": True, "pnl": 0.0, "entry_hour_utc": 0,
                   "volatility": 0.0, "confidence_margin": 0.0, "edge_margin_pp": 0.0},
                  {"id": 2}, {"id": 3, "won": "false", "entry_hour_utc": True}]
        result = common.analyze_trades(trades, high_vol_threshold=.01)
        self.assertEqual(result["ranking_unavailable_trade_count"], 2)
        self.assertEqual([trade["id"] for trade in result["worst_trades"]], [1])
        for key in ("by_session", "by_hour_utc", "by_volatility_bucket", "by_confidence_margin_bucket", "by_edge_margin_bucket"):
            absent = next(row for row in result[key] if row["bucket"] == "unavailable")
            self.assertEqual(absent["trades"], 2)
            self.assertIsNone(absent["total_pnl"])
        profiles = result["winner_loser_profile"]
        self.assertEqual((profiles["winner_count"], profiles["loser_count"], profiles["unknown_outcome_count"]), (1, 0, 2))
        self.assertEqual(len(common.top_bucket_rows(result["by_hour_utc"])), 1)
        import signal_validity_report as signal
        self.assertIsNone(signal.monotonic_win_rate([{"win_rate": None}, {"win_rate": 50.0}]))

    def test_real_zero_and_complete_cost_samples_keep_their_arithmetic(self):
        trades = [{"won": won, "pnl": 0.0, "stake": 1.0, "total_cost": .02} for won in (True, False)]
        result = common.summary_from_trades(trades)
        self.assertEqual(result["total_pnl"], 0.0)
        self.assertEqual(result["avg_pnl"], 0.0)
        self.assertEqual(result["win_rate"], 50.0)
        self.assertEqual(result["avg_cost_pct"], 2.0)
        self.assertEqual(result["break_even_win_rate_pct"], 51.0)
        self.assertEqual(result["edge_gap_pp"], -1.0)
        self.assertEqual(result["metric_observations"]["cost_pct"], 2)

    def test_unavailable_quartiles_do_not_claim_nonmonotonic_or_valid_ranking(self):
        import signal_validity_report as signal
        fields = {"edge_margin_pp": {"correlation_with_win": .3},
                  "next_return_pct": {"correlation_with_win": .6}}
        result = signal.classify_surface(fields, [], {"edge_margin_pp": None})
        self.assertIsNone(result["edge_margin_monotonic"])
        self.assertEqual(result["label"], "insufficient_ranking_evidence")
        self.assertIn("edge_margin_quartiles_unavailable", result["warnings"])
        self.assertNotIn("edge_margin_quartiles_non_monotonic", result["warnings"])
        observed = signal.classify_surface(fields, [], {"edge_margin_pp": False})
        self.assertIn("edge_margin_quartiles_non_monotonic", observed["warnings"])


if __name__ == "__main__":
    unittest.main()
