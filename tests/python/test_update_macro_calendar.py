"""Calendar parsing and failed-refresh behavior use offline official-shaped input."""
from datetime import datetime, timezone
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import update_macro_calendar as calendar


BLS = b"""BEGIN:VCALENDAR
BEGIN:VEVENT
SUMMARY:Consumer Price Index
DTSTART;TZID=US/Eastern:20261014T083000
END:VEVENT
BEGIN:VEVENT
SUMMARY:Employment Situation
DTSTART;TZID=America/New_York:20261106T083000
END:VEVENT
END:VCALENDAR
"""
FED = b"""<h4>2026 FOMC Meetings</h4>
<div class="fomc-meeting"><div class="fomc-meeting__month"><strong>October</strong></div>
<div class="fomc-meeting__date">27-28</div></div>
<div class="fomc-meeting"><div class="fomc-meeting__month">December</div>
<div class="fomc-meeting__date">8-9*</div></div>"""
NOW = datetime(2026, 10, 11, tzinfo=timezone.utc)


class CalendarTests(unittest.TestCase):
    def test_dst_folding_month_boundary_and_scope(self):
        events = calendar.parse_bls(BLS.replace(b"Consumer Price Index", b"Consumer Price \n Index"))
        self.assertEqual(events[0]["timestamp"], "2026-10-14T12:30:00Z")
        self.assertEqual(events[1]["timestamp"], "2026-11-06T13:30:00Z")
        fed = calendar.parse_fed(FED.replace(b"October", b"Oct/Nov").replace(b"27-28", b"31-1"))
        self.assertEqual(fed[0]["timestamp"], "2026-11-01T19:00:00Z")
        self.assertEqual(fed[1]["timestamp"], "2026-12-09T19:00:00Z")
        self.assertEqual(len(calendar.parse_fed(FED.replace(b"27-28", b"22 (notation vote)"))), 1)

    def test_missing_timezone_recurrence_invalid_dates_and_duplicates_refused(self):
        invalid = [BLS.replace(b";TZID=US/Eastern", b""),
                   BLS.replace(b"20261014T083000", b"20261314T083000"),
                   BLS.replace(b"SUMMARY:Consumer", b"RRULE:FREQ=YEARLY\nSUMMARY:Consumer"),
                   BLS.replace(b"SUMMARY:Consumer", b"DTSTART:20261014T123000Z\nSUMMARY:Consumer"),
                   BLS[:-20]]
        for data in invalid:
            with self.subTest(data=data), self.assertRaises(ValueError):
                calendar.parse_bls(data)
        with self.assertRaises(ValueError):
            calendar.parse_fed(FED.replace(b"27-28", b"unknown"))

    def test_generated_document_identifies_sources_and_expires(self):
        document = calendar.make_document(BLS, FED, NOW)
        self.assertEqual(document["refreshed_at"], "2026-10-11T00:00:00Z")
        self.assertEqual(document["valid_until"], "2026-10-12T12:00:00Z")
        self.assertEqual(len(document["events"]), 4)
        self.assertEqual(len(document["sources"][0]["sha256"]), 64)
        changed = calendar.make_document(BLS.replace(b"20261014", b"20261015"), FED, NOW)
        self.assertNotEqual(document["sources"], changed["sources"])

    def test_missing_future_kind_and_oversized_source_refused(self):
        with self.assertRaises(ValueError):
            calendar.make_document(BLS, FED, datetime(2027, 1, 1, tzinfo=timezone.utc))
        with self.assertRaises(ValueError):
            calendar.parse_bls(b"x" * (calendar.MAX_BYTES + 1))
        with self.assertRaises(ValueError):
            calendar.parse_fed(b"x" * (calendar.MAX_BYTES + 1))

    def test_refresh_failure_preserves_calendar_and_success_is_atomic(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "events.json"
            output.write_text("old bytes")
            with patch("sys.argv", ["updater", "--output", str(output)]), patch.object(calendar, "fetch", side_effect=OSError("source unavailable")):
                with self.assertRaises(SystemExit) as failed:
                    calendar.main()
                self.assertEqual(failed.exception.code, 1)
            self.assertEqual(output.read_text(), "old bytes")
            calendar.write_atomic(output, calendar.make_document(BLS, FED, NOW))
            self.assertEqual(json.loads(output.read_text())["schema_version"], 1)
            self.assertEqual(list(output.parent.glob(".macro-calendar-*")), [])
            link = output.parent / "alias.json"
            link.symlink_to(output)
            with self.assertRaises(ValueError):
                calendar.write_atomic(link, {})

    def test_network_policy_has_fixed_sources_no_redirects_and_tls_preflight(self):
        with self.assertRaises(ValueError):
            calendar.fetch("https://example.com/calendar")
        with self.assertRaises(ValueError):
            calendar.NoRedirect().redirect_request(None, None, 302, "", {}, calendar.FED_URL)
        with patch.object(calendar, "require_safe_tls", side_effect=RuntimeError("outdated native TLS")), patch.object(calendar, "build_opener") as opener:
            with self.assertRaises(RuntimeError):
                calendar.fetch(calendar.BLS_URL)
            opener.assert_not_called()


if __name__ == "__main__":
    unittest.main()
