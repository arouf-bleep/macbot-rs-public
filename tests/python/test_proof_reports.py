"""Evidence classification must not turn missing or contradictory data into proof."""
import tempfile
import unittest
from pathlib import Path

from anchor_diff_report import metric_text, summary_delta, numeric_delta_table
from proof_bundle_index import derive_terminal_class, read_json


class ProofReports(unittest.TestCase):
    def classify(self, *, open_count=0, closing_count=0, plausible=2, implausible=0, unknown=0):
        return derive_terminal_class(
            soak_report={"summary": {"trade_count": 2}},
            sim_report={"summary": {"execution_fill_rows": 2,
                         "plausible_execution_rows": plausible,
                         "implausible_execution_rows": implausible,
                         "unverifiable_rows": unknown}},
            integrity_report={"status": "ok", "verdict": "ok", "mismatches": [],
                              "trade_counts": {"open_count": open_count,
                                               "closing_count": closing_count}},
        )

    def test_activity_is_not_success_with_unresolved_inventory(self):
        self.assertEqual(self.classify(open_count=1), "lifecycle_bug")
        self.assertEqual(self.classify(closing_count=1), "lifecycle_bug")
        self.assertEqual(self.classify(), "success")

    def test_execution_must_be_plausible_and_verifiable(self):
        self.assertEqual(self.classify(plausible=1, implausible=1), "market_quality_failure")
        self.assertEqual(self.classify(plausible=1, unknown=1), "evidence_incomplete")
        self.assertEqual(self.classify(plausible=0), "evidence_incomplete")

    def test_missing_counts_cannot_mean_zero_exposure(self):
        self.assertEqual(derive_terminal_class(
            soak_report={"summary": {"trade_count": 2}}, sim_report={},
            integrity_report={"status": "ok"}), "evidence_incomplete")

    def test_report_json_must_be_an_object(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp) / "report.json"
            p.write_text("[]")
            self.assertIsNone(read_json(p))

    def test_unavailable_backtest_edge_is_not_zero(self):
        self.assertEqual(metric_text(None), "unavailable")
        self.assertEqual(metric_text(float("nan")), "unavailable")
        self.assertEqual(metric_text(0), "0.00")
        self.assertIsNone(summary_delta({"edge": None}, {"edge": 1.0}, "edge"))
        self.assertEqual(summary_delta({"edge": 0.0}, {"edge": 1.0}, "edge"), -1.0)

    def test_missing_numeric_samples_cannot_create_a_drift(self):
        rows = numeric_delta_table({"Confidence": {"mean": None, "count": 0}}, {})
        self.assertTrue(all(row["delta"] is None for row in rows))


if __name__ == "__main__":
    unittest.main()
