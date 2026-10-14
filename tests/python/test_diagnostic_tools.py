"""Network-free regressions for the independent trade/dashboard diagnostics."""

import contextlib
import hashlib
import hmac
import io
import json
import math
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import reconcile_dashboard as dashboard
import verify_trade_truth as truth
import api_auth


class ApiCredentialTests(unittest.TestCase):
    def test_alias_precedence_and_empty_primary_match_the_server(self):
        with patch.dict(os.environ, {'MACBOT_API_KEY':' key ', 'MACBOT_API_HMAC_SECRET':' primary ',
                                    'MACBOT_API_SECRET':'secondary'}, clear=True):
            self.assertEqual(api_auth.resolve_api_key(None), 'key')
            self.assertEqual(api_auth.resolve_api_secret(None), ' primary ')
            self.assertEqual(api_auth.resolve_api_secret('explicit'), 'explicit')
            os.environ['MACBOT_API_HMAC_SECRET'] = ' '
            self.assertEqual(api_auth.resolve_api_secret(None), 'secondary')

    def test_each_signature_binds_a_fresh_nonce_and_exact_request(self):
        url = 'http://127.0.0.1/api/config?check=1'
        body = '{"execution_mode":"maker"}'
        with patch.object(api_auth.time, 'time', return_value=1234):
            first = api_auth.signed_headers('POST', url, body, 'key', 'secret')
            second = api_auth.signed_headers('POST', url, body, 'key', 'secret')
        self.assertRegex(first['x-api-nonce'], r'^[0-9a-f]{64}$')
        self.assertNotEqual(first['x-api-nonce'], second['x-api-nonce'])
        canonical = json.dumps(['1234',first['x-api-nonce'],'POST','/api/config?check=1','',body],ensure_ascii=False,separators=(',',':'))
        self.assertEqual(first['x-api-signature'], hmac.new(b'secret', canonical.encode(), hashlib.sha256).hexdigest())
        self.assertNotEqual(first['x-api-signature'], second['x-api-signature'])
        with patch.dict(os.environ, {'MACBOT_API_HMAC_SECRET':' secret '}, clear=True):
            secret = api_auth.resolve_api_secret(None)
            self.assertEqual(secret, ' secret ')
            signed = api_auth.signed_headers('POST',url,body,'key',secret)
            canonical = json.dumps([signed['x-api-ts'],signed['x-api-nonce'],'POST','/api/config?check=1','',body],ensure_ascii=False,separators=(',',':'))
            self.assertEqual(signed['x-api-signature'],hmac.new(b' secret ',canonical.encode(),hashlib.sha256).hexdigest())
        keyed = api_auth.signed_headers('POST',url,body,'key','secret','operation-1')
        self.assertEqual(keyed['x-idempotency-key'],'operation-1')
        canonical = json.dumps([keyed['x-api-ts'],keyed['x-api-nonce'],'POST','/api/config?check=1','operation-1',body],ensure_ascii=False,separators=(',',':'))
        self.assertEqual(keyed['x-api-signature'],hmac.new(b'secret',canonical.encode(),hashlib.sha256).hexdigest())
        self.assertNotEqual(keyed['x-api-signature'],hmac.new(b'secret',canonical.replace('operation-1','operation-2').encode(),hashlib.sha256).hexdigest())

    def test_unicode_canonical_vector_matches_rust_and_browser(self):
        with patch.object(api_auth.time, 'time', return_value=1700000000), \
             patch.object(api_auth.secrets, 'token_hex', return_value='0'*64):
            signed = api_auth.signed_headers('POST',
                'http://127.0.0.1/api/config?note=colon%3Aquote',
                '{"note":"é:🙂\\n"}\n','fixture-key','public-fixture-secret','key:quote"end')
        self.assertEqual(signed['x-api-signature'],
            '82d0e2bdb1555f0d3e91d2e4df2fb533ec7f823074b93de4b2bfc437f65218e2')


class TradeTruthTests(unittest.TestCase):
    def setUp(self):
        self.row = dict(
            id=1, direction="BUY_YES", exit_type="auto_resolved",
            timestamp="2026-09-16T12:00:00Z", exit_timestamp="2026-09-16T12:15:01Z",
            token_id="token", market_id="market", partial_closed=0,
            entry_price=0.5, exit_price=1.0, was_correct=1, pnl=10.0,
            shares=20.0, fee_paid=0.10, entry_cost=10.0, entry_shares=20.0,
            end_date="2026-09-16T12:15:00Z",
        )
        self.market = dict(umaResolutionStatus="resolved", winner="UP",
                           endDate="2026-09-16T12:15:00Z")

    def verify(self, row=None, entry_reference=0.5, exit_reference=None):
        checked_row = self.row if row is None else row
        with patch.object(truth, "price_history", side_effect=lambda token, at: [
            (int(at.timestamp()), exit_reference
             if exit_reference is not None and at == truth.parse_ts(checked_row["exit_timestamp"])
             else entry_reference)
        ]), patch.object(truth, "gamma_market", return_value=self.market):
            return truth.verify_trade(checked_row)

    def test_resolved_pnl_excludes_entry_fees_for_both_directions(self):
        for direction, exit_price, pnl, correct in (("BUY_YES", 1.0, 10.0, 1),
                                                   ("BUY_NO", 0.0, -10.0, 0)):
            with self.subTest(direction=direction):
                result = self.verify(dict(self.row, direction=direction, exit_price=exit_price,
                                          pnl=pnl, was_correct=correct))
                self.assertEqual(result["checks"]["pnl"]["status"], "OK")
                self.assertEqual(truth.verification_exit_code([result]), 0)

    def test_subtracting_entry_fee_again_is_a_mismatch_even_below_old_tolerance(self):
        for deducted_fee in (0.10, 0.01):
            with self.subTest(deducted_fee=deducted_fee):
                result = self.verify(dict(self.row, pnl=10.0 - deducted_fee))
                self.assertEqual(result["checks"]["pnl"]["status"], "MISMATCH")
                self.assertEqual(truth.verification_exit_code([result]), 1)

    def test_missing_pnl_is_incomplete_even_when_entry_has_acceptable_drift(self):
        result = self.verify(dict(self.row, pnl=None), entry_reference=0.49)
        self.assertEqual(result["checks"]["entry"]["status"], "DRIFT")
        self.assertEqual(result["checks"]["pnl"]["status"], "UNVERIFIABLE")
        self.assertEqual(result["verdict"], "UNVERIFIABLE")
        self.assertEqual(truth.verification_exit_code([result]), 2)

    def test_non_resolution_exit_does_not_guess_exit_fees_from_entry_fee(self):
        # 20 shares from .5 to .6 is $2 gross. The row doesn't record the
        # exit fee, so neither $2 nor $1.90 can be certified from this row.
        for pnl in (2.0, 1.9):
            with self.subTest(pnl=pnl):
                result = self.verify(dict(self.row, exit_type="manual", exit_price=0.6, pnl=pnl),
                                     exit_reference=0.6)
                self.assertEqual(result["checks"]["pnl"]["status"], "UNVERIFIABLE")
                self.assertEqual(truth.verification_exit_code([result]), 2)

    def test_receipt_managed_pnl_uses_actual_cash_fees_without_double_counting(self):
        row=dict(self.row,exit_type="venue_confirmed",accounting_version=1,
                 actual_cash_fees=0.15,exit_price=0.6,pnl=1.85)
        self.assertEqual(truth.pnl_check(row)["status"],"OK")
        self.assertEqual(truth.pnl_check(dict(row,pnl=1.95))["status"],"MISMATCH")
        self.assertEqual(truth.pnl_check(dict(row,actual_cash_fees=None))["status"],"UNVERIFIABLE")

    def test_partial_close_needs_fill_ledger_and_missing_entry_fields_are_explicit(self):
        result = self.verify(dict(self.row, partial_closed=1, entry_cost=None))
        self.assertEqual(result["checks"]["pnl"]["status"], "UNVERIFIABLE")
        self.assertEqual(result["checks"]["entry_cost"]["status"], "UNVERIFIABLE")

    def test_invalid_numbers_cannot_pass_arithmetic(self):
        for field, value in (("pnl", math.nan), ("shares", math.inf), ("shares", -1.0),
                             ("entry_price", None), ("entry_cost", math.nan)):
            with self.subTest(field=field):
                result = self.verify(dict(self.row, **{field: value}))
                self.assertNotEqual(result["verdict"], "OK")
                self.assertNotEqual(truth.verification_exit_code([result]), 0)

    def test_no_rows_is_incomplete(self):
        self.assertEqual(truth.verification_exit_code([]), 2)

    def cancelled_row(self):
        return dict(self.row, **dict(exit_type="cancelled", shares=0.0, entry_shares=0.0,
                                    entry_cost=0.0, fee_paid=0.0, pnl=0.0, exit_price=None,
                                    was_correct=None))

    def test_unfilled_cancellation_checks_accounting_without_inventing_execution(self):
        row = self.cancelled_row()
        with patch.object(truth, "price_history", side_effect=AssertionError("no fill to price")), \
                patch.object(truth, "gamma_market", side_effect=AssertionError("no outcome to settle")):
            result = truth.verify_trade(row)
        self.assertEqual(result["verdict"], "OK")
        self.assertEqual(truth.verification_exit_code([result]), 0)
        self.assertIn("no market execution", result["verification_scope"])

    def test_cancellation_with_missing_or_nonzero_accounting_cannot_pass(self):
        row = self.cancelled_row()
        for field in ("shares", "entry_shares", "entry_cost", "fee_paid", "pnl"):
            for value, status in ((None, "UNVERIFIABLE"), (0.01, "MISMATCH"), (math.nan, "MISMATCH")):
                with self.subTest(field=field, value=value):
                    result = truth.verify_trade(dict(row, **{field: value}))
                    self.assertEqual(result["checks"][field]["status"], status)
                    self.assertNotEqual(truth.verification_exit_code([result]), 0)
        for overrides in ({"partial_closed": 1}, {"exit_price": 0.6}):
            result = truth.verify_trade(dict(row, **overrides))
            self.assertEqual(result["verdict"], "MISMATCH")

    def test_cancellation_requires_valid_close_timing(self):
        row = self.cancelled_row()
        result = truth.verify_trade(dict(row, exit_timestamp=None))
        self.assertEqual(result["verdict"], "UNVERIFIABLE")
        result = truth.verify_trade(dict(row, exit_timestamp="2026-09-16T11:59:00Z"))
        self.assertEqual(result["verdict"], "MISMATCH")

    def test_cli_reads_isolated_database_and_reports_missing_pnl(self):
        with tempfile.TemporaryDirectory() as directory:
            db = Path(directory) / "trades.sqlite"
            with sqlite3.connect(db) as con:
                columns = ", ".join(f'"{name}"' for name in self.row)
                con.execute(f"CREATE TABLE trades ({columns}, status TEXT)")
                values = dict(self.row, pnl=None)
                con.execute(f"INSERT INTO trades VALUES ({','.join('?' for _ in values)}, ?)",
                            [*values.values(), "CLOSED"])
            output = io.StringIO()
            with patch.object(sys, "argv", ["verify_trade_truth.py", "--db", str(db), "--json"]), \
                    patch.object(truth, "price_history", return_value=[]), \
                    patch.object(truth, "gamma_market", return_value=self.market), \
                    contextlib.redirect_stdout(output):
                self.assertEqual(truth.main(), 2)
            result = json.loads(output.getvalue())
            self.assertEqual(result["trades"][0]["checks"]["pnl"]["status"], "UNVERIFIABLE")


class DashboardReconciliationTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.db = str(Path(self.directory.name) / "trades.sqlite")
        with sqlite3.connect(self.db) as con:
            con.execute("CREATE TABLE trades (status TEXT, pnl REAL, fee_paid REAL, amount REAL, mode TEXT, exit_timestamp TEXT)")
        self.initial_balance = 100.0

    def insert(self, status="CLOSED", pnl=1.0, fee=0.1, amount=10.0, mode="paper"):
        with sqlite3.connect(self.db) as con:
            con.execute("INSERT INTO trades VALUES (?,?,?,?,?,?)",
                        (status, pnl, fee, amount, mode, "2026-09-16T12:15:00Z"))

    def reconcile(self, *, mode="paper", balance=None, derived=None, authoritative=None,
                  remove_stream_stats=False):
        db = dashboard.db_truth(self.db)
        expected = (self.initial_balance + db["closed_non_live_delta"]
                    - db["open_non_live_locked"] + db["open_non_live_realized"])
        current = expected if balance is None else balance
        stats = dict(closed_trades=db["closed_trades"], open_trades=db["open_trades"],
                     wins=db["wins"], win_rate=db["win_rate_pct"] / 100,
                     total_pnl_net=db["total_pnl_net"], today_pnl=db["today_pnl"],
                     current_balance=current, total_entry_fees=db["closed_entry_fees"])
        stream = dict(stats={} if remove_stream_stats else stats,
                      balance=current, daily_pnl=db["today_pnl"], loop_running=False,
                      execution_mode="maker", mode=mode)
        endpoints = {
            "/api/stats": {"performance": stats, "balance": current, "daily_pnl": db["today_pnl"]},
            "/api/trades/analysis": dict(total_closed=db["closed_trades"], total_open=db["open_trades"],
                                        wins=db["wins"], win_rate=db["win_rate_pct"], total_pnl_net=db["total_pnl_net"]),
            "/api/accounting/truth": dict(current_balance=current,
                                          derived_restart_balance=expected if derived is None else derived,
                                          authoritative_live_balance=authoritative),
            "/api/trades": {"trades": [{"status": "CLOSED"}] * db["closed_trades"]
                            + [{"status": "OPEN"}] * db["open_trades"]},
            "/api/loop/status": {"running": False, "mode": mode},
            "/api/config": {"execution_mode": "maker", "mode": mode, "initial_balance": self.initial_balance},
        }
        with patch.object(dashboard, "one_stream_payload", return_value=stream), \
                patch.object(dashboard, "get_json", side_effect=lambda url, **kwargs: endpoints[url.removeprefix("mock://bot")]):
            return dashboard.take_and_compare(SimpleNamespace(base="mock://bot", db=self.db), "", {})

    def figure(self, name, **kwargs):
        return next(result for result in self.reconcile(**kwargs) if result["figure"] == name)

    def test_same_wrong_cached_balance_is_rejected_by_local_ledger(self):
        self.insert()
        result = self.figure("balance", balance=105.0, derived=105.0)
        self.assertEqual(result["verdict"], "DISAGREE")
        self.assertAlmostEqual(result["sources"]["db"], 100.9)

    def test_route_reconstruction_disagreement_is_detected(self):
        self.insert()
        self.assertEqual(self.figure("balance", derived=-999.0)["verdict"], "DISAGREE")

    def test_open_partial_position_and_entry_fee_are_in_balance_but_not_closed_fees(self):
        self.insert()
        self.insert(status="OPEN", pnl=0.3, fee=0.02, amount=2.0)
        self.insert(pnl=900.0, fee=2.0, mode="live")
        self.insert(status="OPEN", pnl=None, fee=0.0, amount=50.0, mode="live")
        db = dashboard.db_truth(self.db)
        self.assertAlmostEqual(db["closed_entry_fees"], 2.1)
        result = self.figure("balance")
        self.assertEqual(result["verdict"], "OK")
        self.assertAlmostEqual(result["sources"]["db"], 99.18)
        self.assertEqual(self.figure("closed entry fees")["verdict"], "OK")

    def test_legacy_live_and_receipt_managed_rows_keep_distinct_fee_semantics(self):
        self.insert(pnl=0.01,fee=0.02,mode="live")
        db=dashboard.db_truth(self.db)
        self.assertAlmostEqual(db["total_pnl_net"],-0.01)
        self.assertEqual(db["wins"],0)
        with sqlite3.connect(self.db) as con:
            con.execute("ALTER TABLE trades ADD COLUMN accounting_version INTEGER DEFAULT 0")
            con.execute("UPDATE trades SET accounting_version=1")
        db=dashboard.db_truth(self.db)
        self.assertAlmostEqual(db["total_pnl_net"],0.01)
        self.assertEqual(db["wins"],1)

    def test_percentages_under_one_percent_are_not_scaled_twice(self):
        self.insert(pnl=1.0, fee=0.0)
        for _ in range(199):
            self.insert(pnl=-1.0, fee=0.0)
        result = self.figure("win rate %")
        self.assertEqual(result["verdict"], "OK")
        self.assertTrue(all(value == 0.5 for value in result["sources"].values()))

    def test_live_balance_does_not_use_paper_initial_balance_formula(self):
        self.insert(pnl=1.0, mode="live")
        result = self.figure("balance", mode="live", balance=200.0, derived=-999.0, authoritative=200.0)
        self.assertEqual(result["verdict"], "OK")
        self.assertNotIn("db", result["sources"])
        result = self.figure("balance", mode="live", balance=200.0, authoritative=199.0)
        self.assertEqual(result["verdict"], "DISAGREE")

    def test_live_balance_without_authority_is_incomplete(self):
        results = self.reconcile(mode="live", balance=200.0)
        balance = next(result for result in results if result["figure"] == "balance")
        self.assertEqual(balance["verdict"], "UNVERIFIABLE")
        self.assertEqual(dashboard.reconciliation_exit_code(results), 2)

    def test_missing_required_source_and_nonfinite_values_cannot_pass(self):
        self.insert()
        results = self.reconcile(remove_stream_stats=True)
        self.assertEqual(dashboard.reconciliation_exit_code(results), 2)
        for value in (math.nan, math.inf, "not a number"):
            result = dashboard.compare_figure("money", 0.01, {"one": 1.0, "two": value}, ("one", "two"))
            self.assertEqual(result["verdict"], "INVALID")
            self.assertEqual(dashboard.reconciliation_exit_code([result]), 1)

    def test_missing_closed_pnl_is_not_summed_away(self):
        self.insert(pnl=None)
        results = self.reconcile()
        self.assertEqual(dashboard.reconciliation_exit_code(results), 1)
        self.assertEqual(results[-1]["figure"], "database evidence")

    def test_readonly_database_does_not_create_missing_path(self):
        missing = Path(self.directory.name) / "missing.sqlite"
        with self.assertRaises(sqlite3.OperationalError):
            dashboard.db_truth(str(missing))
        self.assertFalse(missing.exists())


if __name__ == "__main__":
    unittest.main()
