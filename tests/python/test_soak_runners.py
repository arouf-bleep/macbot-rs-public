"""Paper runners must isolate data, authenticate reads, and refuse false proof."""
import json
import io
import os
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from argparse import Namespace
from pathlib import Path
from unittest.mock import Mock, patch

from run_maker_soak import paper_run_environment, redacted_config, fetch_json
from run_frozen_maker_soak import classify_terminal_outcome, load_active_trade_count
from run_maker_ab_batch import summarize_batch


class SoakRunners(unittest.TestCase):
    def test_health_probes_use_bounded_shared_transport(self):
        from collections import deque
        from http_limits import MAX_RESPONSE_BYTES
        import run_maker_soak as maker
        import run_frozen_maker_soak as frozen
        process = Mock()
        process.poll.return_value = None
        calls = [
            (maker, lambda: maker.wait_ready('http://127.0.0.1:18099', proc=process)),
            (frozen, lambda: frozen.wait_ready(process, 'http://127.0.0.1:18099', deque())),
        ]
        for module, run in calls:
            body = io.BytesIO(b'x' * (MAX_RESPONSE_BYTES + 1))
            with self.subTest(probe=run), patch.object(module, 'open_api_request', return_value=body) as transport:
                with self.assertRaisesRegex(RuntimeError, 'response limit'):
                    run()
                self.assertEqual(transport.call_args.args[0].full_url, 'http://127.0.0.1:18099/api/health')
            self.assertTrue(body.closed)
            # An unavailable booting service still retries instead of failing
            # at the first ordinary connection-refused response.
            import urllib.error
            healthy = io.BytesIO(b'{"status":"ok"}')
            with patch.object(module, 'open_api_request', side_effect=[urllib.error.URLError('booting'), healthy]) as transport, \
                 patch.object(module.time, 'sleep'):
                run()
                self.assertEqual(transport.call_count, 2)

    def test_live_trade_inspection_opens_read_only_and_closes(self):
        with tempfile.TemporaryDirectory() as tmp:
            db = Path(tmp) / "live.sqlite"
            with sqlite3.connect(db) as conn:
                conn.execute("CREATE TABLE trades(status)")
                conn.executemany("INSERT INTO trades VALUES(?)", [("OPEN",), ("CLOSING",), ("CLOSED",)])
            real_connect = sqlite3.connect
            captured = []

            def inspect_connection(path, **kwargs):
                self.assertEqual(path, db.resolve().as_uri() + "?mode=ro")
                self.assertEqual(kwargs, {"uri": True})
                opened = real_connect(path, **kwargs)
                with self.assertRaisesRegex(sqlite3.OperationalError, "readonly"):
                    opened.execute("DELETE FROM trades")
                captured.append(opened)
                return opened

            with patch("run_frozen_maker_soak.sqlite3.connect", side_effect=inspect_connection):
                self.assertEqual(load_active_trade_count(db), 2)
            with self.assertRaisesRegex(sqlite3.ProgrammingError, "closed"):
                captured[0].execute("SELECT 1")

    def test_paper_environment_requires_local_port_and_fresh_database(self):
        with tempfile.TemporaryDirectory() as tmp:
            cfg = {"mode": "simulation", "execution_mode": "maker"}
            db = Path(tmp) / "fresh.sqlite"
            env = paper_run_environment(cfg, Path(tmp) / "cfg.json", db, "http://127.0.0.1:18099")
            self.assertEqual(env["MACBOT_PORT"], "18099")
            self.assertEqual(env["MACBOT_BIND_ALL"], "0")
            for bad in ["https://127.0.0.1:18099", "http://remote.example", "http://127.0.0.1/api", "http://secret@127.0.0.1"]:
                with self.assertRaises(ValueError):
                    paper_run_environment(cfg, Path(tmp) / "cfg.json", db, bad)
            with self.assertRaises(ValueError):
                paper_run_environment({**cfg, "mode": "live"}, Path(tmp) / "cfg.json", db, "http://127.0.0.1:18099")
            db.write_text("preserved history")
            with self.assertRaises(ValueError):
                paper_run_environment(cfg, Path(tmp) / "cfg.json", db, "http://127.0.0.1:18099")
            self.assertEqual(db.read_text(), "preserved history")

    def test_config_preview_hides_credentials_and_snapshot_uses_read_auth(self):
        self.assertEqual(redacted_config({"api_key": "private", "api_hmac_secret": "secret", "private_rpc_url": "private-url", "stake": 2}),
                         {"api_key": "[redacted]", "api_hmac_secret": "[redacted]", "private_rpc_url": "[redacted]", "stake": 2})
        with patch("run_maker_soak.get_json", return_value={}) as read:
            fetch_json("http://127.0.0.1:18099/api/maker/state", api_key="test-read-key")
            self.assertTrue(read.call_args.kwargs["read_auth"])
            self.assertEqual(read.call_args.kwargs["api_key"], "test-read-key")

    def test_trade_activity_needs_complete_fill_and_flat_inventory_evidence(self):
        report = {"summary": {"trade_count": 1}}
        sim = {"summary": {"execution_fill_rows": 2, "plausible_execution_rows": 2,
                            "implausible_execution_rows": 0, "unverifiable_rows": 0}}
        integrity = {"status": "ok", "trade_counts": {"open_count": 0, "closing_count": 0}}
        classify = lambda v, i: classify_terminal_outcome(report, v, None, i, None)["class"]
        self.assertEqual(classify(sim, integrity), "success")
        self.assertEqual(classify(None, integrity), "evidence_incomplete")
        self.assertEqual(classify(sim, {**integrity, "trade_counts": {"open_count": 0, "closing_count": 1}}), "lifecycle_bug")
        bad = {"summary": {**sim["summary"], "unverifiable_rows": 1}}
        self.assertEqual(classify(bad, integrity), "evidence_incomplete")

    def test_repeated_batch_evidence_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp) / "comparison.json"
            p.write_text('{}')
            cycle = {"compare": {"json": str(p)}}
            with self.assertRaises(ValueError):
                summarize_batch([cycle, cycle], Namespace())

    def test_ineligible_batch_renders_without_starting_any_bot(self):
        with tempfile.TemporaryDirectory() as tmp:
            script = (Path(__file__).resolve().parents[2] / "scripts" / "run_maker_ab_batch.py")
            result = subprocess.run([sys.executable, str(script), "--outdir", tmp,
                                     "--blocked-hours-utc", ",".join(map(str, range(24))),
                                     "--wait-until-eligible-seconds", "0"],
                                    capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads(Path(json.loads(result.stdout)["json"]).read_text())
            self.assertEqual(report["verdict"], "awaiting_eligible_start")
            self.assertEqual(report["cycles_run"], 0)
            self.assertEqual(report["counts"]["adequate_comparable_windows"], 0)


if __name__ == "__main__":
    unittest.main()
