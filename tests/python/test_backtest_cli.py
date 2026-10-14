"""Offline regressions for nullable backtest figures and explicit API failures."""
from __future__ import annotations

import contextlib
import io
import json
import unittest
from unittest.mock import patch

import backtest_regime_report as report
import regime_allowlist_from_backtest as allowlist


def empty_result():
    return {
        "execution_mode": "taker", "days": 1, "total_trades": 0,
        "final_balance": 100.0, "total_pnl": 0.0, "win_rate": None,
        "edge_audit": {"break_even_win_rate_pct": None, "edge_gap_pp": None,
                       "by_regime": {}},
    }


def run_main(module, result=None, error=None):
    stdout, stderr = io.StringIO(), io.StringIO()
    with patch("sys.argv", [module.__name__]), \
         patch.object(module, "post_json", return_value=result, side_effect=error), \
         contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
        status = module.main()
    return status, stdout.getvalue(), stderr.getvalue()


class BacktestCliTests(unittest.TestCase):
    def test_no_trades_report_renders_unavailable_without_fabricating_zero_edge(self):
        status, stdout, stderr = run_main(report, empty_result())
        self.assertEqual(status, 0)
        self.assertEqual(stderr, "")
        self.assertIn("trades=0", stdout)
        self.assertIn("win_rate=unavailable", stdout)
        self.assertIn("be_wr=unavailable", stdout)
        self.assertIn("edge_gap=unavailable", stdout)
        self.assertNotIn("edge_gap=0.00", stdout)

    def test_unavailable_regime_metrics_render_alongside_real_observations(self):
        result = empty_result()
        result["edge_audit"].update(break_even_win_rate_pct=50.0, edge_gap_pp=0.0)
        result["edge_audit"]["by_regime"] = {
            "BULL": {"trades": 2, "win_rate": 50.0, "break_even_win_rate_pct": None,
                     "edge_gap_pp": None, "expectancy_per_trade": -0.1,
                     "total_pnl": -0.2, "avg_cost_pct": 2.0}}
        status, stdout, stderr = run_main(report, result)
        self.assertEqual((status, stderr), (0, ""))
        self.assertIn("BULL,2,50.00,unavailable,unavailable,-0.1000,-0.2000,2.00", stdout)

    def test_api_failure_is_not_a_successful_empty_allowlist(self):
        status, stdout, stderr = run_main(allowlist, {"error": "provider unavailable"})
        self.assertEqual(status, 2)
        self.assertEqual(stdout, "")
        self.assertIn("provider unavailable", stderr)

    def test_transport_failure_has_a_clear_nonzero_diagnostic(self):
        status, stdout, stderr = run_main(allowlist, error=RuntimeError("401 fixture"))
        self.assertEqual(status, 1)
        self.assertEqual(stdout, "")
        self.assertIn("401 fixture", stderr)

    def test_unavailable_bucket_is_not_selected_using_an_invented_zero(self):
        result = empty_result()
        result["edge_audit"]["by_regime"] = {
            "UNKNOWN": {"trades": 30, "edge_gap_pp": None},
            "OBSERVED": {"trades": 30, "edge_gap_pp": 0.0}}
        status, stdout, stderr = run_main(allowlist, result)
        self.assertEqual((status, stderr), (0, ""))
        self.assertEqual(json.loads(stdout), {"regime_allowlist": ["OBSERVED"]})

    def test_no_qualifying_evidence_does_not_recommend_unrestricted_trading(self):
        for buckets in ({}, {"OBSERVED": {"trades": 30, "edge_gap_pp": -1.0}}):
            with self.subTest(buckets=buckets):
                result = empty_result()
                result["edge_audit"]["by_regime"] = buckets
                status, stdout, stderr = run_main(allowlist, result)
                self.assertEqual(status, 2)
                self.assertEqual(stdout, "")
                self.assertIn("no qualifying evidence", stderr)
                self.assertIn("unrestricted", stderr)


if __name__ == "__main__":
    unittest.main()
