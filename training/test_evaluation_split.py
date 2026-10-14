import unittest
from datetime import datetime, timedelta, timezone

from evaluation_split import chronological_holdout


def observations(count=20):
    start = datetime(2026, 1, 1, tzinfo=timezone.utc)
    return [
        {"timestamp": (start + timedelta(minutes=15 * i)).isoformat(), "market_id": f"m{i}"}
        for i in range(count)
    ]


class EvaluationSplitTests(unittest.TestCase):
    def test_holdout_contains_newest_samples_and_embargo_is_excluded(self):
        result = chronological_holdout(observations())
        self.assertEqual(result.holdout, (16, 17, 18, 19))
        self.assertEqual(result.development, tuple(range(15)))
        self.assertEqual(result.purged, (15,))

    def test_repeated_market_outcome_cannot_cross_the_boundary(self):
        rows = observations()
        rows[2]["market_id"] = rows[17]["market_id"]
        result = chronological_holdout(rows)
        self.assertNotIn(2, result.development)
        train_markets = {rows[i]["market_id"] for i in result.development}
        test_markets = {rows[i]["market_id"] for i in result.holdout}
        self.assertFalse(train_markets & test_markets)

    def test_equal_timestamps_stay_in_the_same_partition(self):
        rows = observations()
        rows[15]["timestamp"] = rows[16]["timestamp"]
        result = chronological_holdout(rows, embargo_seconds=0)
        self.assertIn(15, result.holdout)
        self.assertNotIn(15, result.development)

    def test_sorting_uses_actual_time_including_offsets(self):
        rows = observations()
        rows[19]["timestamp"] = "2026-01-01T05:45:00+01:00"
        rows = list(reversed(rows))
        result = chronological_holdout(rows)
        self.assertEqual([rows[i]["market_id"] for i in result.holdout], ["m16", "m17", "m18", "m19"])

    def test_missing_markets_and_exhausted_splits_fail_closed(self):
        rows = observations()
        rows[0]["market_id"] = None
        with self.assertRaisesRegex(ValueError, "market_id"):
            chronological_holdout(rows)
        rows = observations()
        for row in rows:
            row["market_id"] = "same-market"
        with self.assertRaisesRegex(ValueError, "no development"):
            chronological_holdout(rows)

    def test_invalid_split_parameters_are_rejected(self):
        for fraction in [0, 1, -0.2, 1.1, float("nan")]:
            with self.assertRaises(ValueError):
                chronological_holdout(observations(), fraction=fraction)
        with self.assertRaises(ValueError):
            chronological_holdout(observations(), embargo_seconds=-1)


if __name__ == "__main__":
    unittest.main()
