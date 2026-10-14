"""Regressions for research reports using isolated SQLite evidence."""
from __future__ import annotations

import json
import sqlite3
import unittest
import tempfile
import os
import shutil
import subprocess
from pathlib import Path
from datetime import datetime, timezone, timedelta
from unittest.mock import patch
from types import SimpleNamespace

import calibrate_signal_weights as calibration
from time_of_day_report import parse_ts
from verify_trade_truth import resolution
import verify_trade_truth as truth
from reconcile_dashboard import db_truth, daily_realized_truth
from validation_db_integrity_report import load_fill_integrity
from backtest_trade_report_common import edge_gap_from_result, require_resolution_edge_mode, redacted_command
from backtest_trade_report_common import numeric_profile, winner_loser_profile
from lane_comparison_report import numeric_delta_table
import recency_alpha_report as recency
import daily_alpha_report as daily
import meta_fusion_shadow_report as meta
import high_confidence_wrong_trade_report as wrong_report
import buy_no_shadow_guard_report as no_guard
import validation_control_bundle as control
import validation_artifact_hygiene_report as hygiene
import sim_price_verification_report as sim
import reconstruct_trade_history as reconstruction


class ResearchReportTests(unittest.TestCase):
    @staticmethod
    def redemption_row(payout_units=20_000_000):
        timestamp = "2026-10-11T00:00:00Z"
        transaction = "0x" + "a" * 64
        proof = {"transaction_hash": transaction, "account": "0x" + "b" * 40,
                 "condition_id": "0x" + "c" * 64, "block_number": 100,
                 "block_hash": "0x" + "d" * 64, "timestamp": timestamp,
                 "token_ids": ["token", "other"], "burned_units": [20_000_000, 0],
                 "payout_units": [payout_units, 0], "total_payout_units": payout_units,
                 "redemption_log_index": 1, "evidence": {"receipt": {"status": "0x1"}}}
        payout = payout_units / 1_000_000
        row = dict(id=1, direction="BUY_YES", exit_type="redemption_confirmed", status="CLOSED",
                   timestamp="2026-10-09T00:00:00Z", exit_timestamp=timestamp,
                   token_id="token", market_id=proof["condition_id"], partial_closed=0,
                   entry_price=.5, exit_price=payout / 20, was_correct=1, pnl=payout - 10,
                   shares=20.0, fee_paid=0, entry_cost=10.0, entry_shares=20.0,
                   end_date="2026-10-09T00:15:00Z", accounting_version=1, actual_cash_fees=0)
        receipt = {key: proof[key] for key in (
            "transaction_hash", "account", "condition_id", "block_number", "block_hash", "timestamp")}
        receipt.update(proof_json=json.dumps(proof), trade_ids_json="[1]",
                       result_json=json.dumps({"trades": [{"id": 1, "shares_redeemed": 20,
                           "collateral_received": payout, "total_pnl": payout - 10, "status": "CLOSED"}]}))
        fill = {"timestamp": timestamp, "avg_price": payout / 20, "delta_size": 20,
                "source": "finalized_redemption", "details": json.dumps({"transaction_hash": transaction,
                    "token_id": "token", "collateral_received": payout})}
        row["redemption_evidence"] = [{"fill": fill, "receipt": receipt}]
        return row

    def test_trade_truth_does_not_treat_late_redemption_as_book_sale_or_independent_chain_proof(self):
        for payout in (0, 10_000_000, 20_000_000):
            row = self.redemption_row(payout)
            with patch.object(truth, "price_history", return_value=[
                    (int(truth.parse_ts(row["timestamp"]).timestamp()), .5)]) as prices, \
                 patch.object(truth, "gamma_market", side_effect=AssertionError("Gamma is not redemption payout proof")):
                report = truth.verify_trade(row)
            self.assertEqual(prices.call_count, 1, "only entry is compared with market history")
            self.assertEqual(report["checks"]["exit"]["local_consistency"], "OK")
            self.assertEqual(report["checks"]["timing"]["status"], "OK")
            self.assertEqual(report["verdict"], "UNVERIFIABLE")
            self.assertEqual(truth.verification_exit_code([report]), 2)
            self.assertIn("gas", report["pnl_excludes"])

    def test_trade_truth_refuses_contradictory_redemption_allocation_and_missing_receipt(self):
        row = self.redemption_row()
        row["redemption_evidence"][0]["fill"]["details"] = json.dumps({
            "transaction_hash": "0x" + "a" * 64, "token_id": "token", "collateral_received": 10})
        self.assertEqual(truth.redemption_exit_check(row, truth.parse_ts(row["exit_timestamp"]))["status"], "MISMATCH")
        row["redemption_evidence"] = []
        self.assertEqual(truth.redemption_exit_check(row, truth.parse_ts(row["exit_timestamp"]))["status"], "UNVERIFIABLE")

    def test_trade_truth_equal_second_redemption_and_corrupt_dates_do_not_abort_other_rows(self):
        row = self.redemption_row()
        row["timestamp"] = row["exit_timestamp"]
        row["end_date"] = "2026-10-11T01:00:00Z"
        with patch.object(truth, "price_history", return_value=[
                (int(truth.parse_ts(row["timestamp"]).timestamp()), .5)]), \
             patch.object(truth, "gamma_market", side_effect=AssertionError("not redemption proof")):
            good = truth.verify_trade(row)
        self.assertEqual(good["checks"]["timing"]["status"], "OK")
        for field in ("timestamp", "exit_timestamp"):
            corrupt = {**row, field: "not-a-time"}
            bad = truth.verify_trade(corrupt)
            self.assertEqual(bad["verdict"], "MISMATCH")
            self.assertEqual(truth.verification_exit_code([good, bad]), 1)
        missing = truth.verify_trade({**row, "timestamp": None})
        self.assertEqual(missing["verdict"], "UNVERIFIABLE")

    def test_trade_truth_loads_only_transaction_bound_redemption_evidence(self):
        row = self.redemption_row()
        original = row["redemption_evidence"][0]
        with sqlite3.connect(":memory:") as conn:
            conn.row_factory = sqlite3.Row
            conn.execute("CREATE TABLE trade_fills(trade_id,phase,timestamp,avg_price,delta_size,source,details)")
            conn.execute("CREATE TABLE redemption_receipts(transaction_hash,account,condition_id,block_number,block_hash,timestamp,proof_json,trade_ids_json,result_json)")
            fill, receipt = original["fill"], original["receipt"]
            conn.execute("INSERT INTO trade_fills VALUES(1,'redemption',?,?,?,?,?)",
                         tuple(fill[key] for key in ("timestamp", "avg_price", "delta_size", "source", "details")))
            conn.execute("INSERT INTO redemption_receipts VALUES(?,?,?,?,?,?,?,?,?)", tuple(receipt.values()))
            row["redemption_evidence"] = truth.load_redemption_evidence(conn, row, {"trade_fills", "redemption_receipts"})
            self.assertEqual(truth.redemption_exit_check(row, truth.parse_ts(row["exit_timestamp"]))["local_consistency"], "OK")
            conn.execute("UPDATE redemption_receipts SET trade_ids_json='[2]'")
            row["redemption_evidence"] = truth.load_redemption_evidence(conn, row, {"trade_fills", "redemption_receipts"})
            self.assertEqual(truth.redemption_exit_check(row, truth.parse_ts(row["exit_timestamp"]))["status"], "MISMATCH")

    def test_daily_reconciliation_tracks_partial_event_day_and_unknown_cross_day_history(self):
        conn=sqlite3.connect(":memory:");conn.row_factory=sqlite3.Row
        conn.execute("CREATE TABLE trades(id,timestamp,status,shares,entry_shares,pnl,fee_paid,accounting_version,partial_closed,exit_timestamp)")
        conn.execute("CREATE TABLE realized_pnl_events(trade_id,event_key,timestamp,net_pnl,realized_shares)")
        conn.execute("INSERT INTO trades VALUES(1,'2026-10-09T00:00:00Z','OPEN',2,10,-2,0,1,1,NULL)")
        conn.executemany("INSERT INTO realized_pnl_events VALUES(1,?,?,?,4)",[
            ('gain','2026-10-10T23:59:00Z',4),('loss','2026-10-11T01:00:00+01:00',-6)])
        self.assertEqual(daily_realized_truth(conn,'2026-10-11',set())["exact_today_pnl"],-6)
        self.assertEqual(daily_realized_truth(conn,'2026-10-10',set())["exact_today_pnl"],4)
        conn.execute("INSERT INTO trades VALUES(2,'2026-10-10T00:00:00Z','OPEN',5,10,0,0,1,1,NULL)")
        evidence=daily_realized_truth(conn,'2026-10-11',set())
        self.assertIsNone(evidence["exact_today_pnl"])
        self.assertFalse(evidence["evidence_complete"])
        self.assertEqual(evidence["dated_today_pnl"],-6)
        self.assertEqual(evidence["unallocated_trade_ids"],[2])
        conn.execute("UPDATE trades SET pnl=-3 WHERE id=1")
        with self.assertRaisesRegex(ValueError,"disagree with lot"):
            daily_realized_truth(conn,'2026-10-11',set())
        conn.close()

    def test_dashboard_cash_reconstruction_retains_closing_inventory(self):
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/"closing.sqlite"
            with sqlite3.connect(path) as conn:
                conn.execute("CREATE TABLE trades(status,pnl,fee_paid,accounting_version,mode,amount,exit_timestamp)")
                conn.execute("INSERT INTO trades VALUES('CLOSING',0,.1,0,'simulation',2.5,NULL)")
            result=db_truth(str(path))
            self.assertEqual(result["open_trades"],1)
            self.assertEqual(result["open_non_live_locked"],2.6)

    def test_daily_reconciliation_separates_real_and_simulated_capital(self):
        with sqlite3.connect(":memory:") as conn:
            conn.row_factory = sqlite3.Row
            conn.execute("CREATE TABLE trades(id,timestamp,status,shares,entry_shares,pnl,fee_paid,accounting_version,partial_closed,exit_timestamp,mode)")
            conn.executemany("INSERT INTO trades VALUES(?,'2026-10-11T00:00:00Z','CLOSED',5,5,?,0,1,0,'2026-10-11T01:00:00Z',?)", [(1, 100, 'simulation'), (2, -60, 'live')])
            live = daily_realized_truth(conn, '2026-10-11', {'mode'}, 'live')
            self.assertEqual(live['capital_origin'], 'live')
            self.assertEqual(live['exact_today_pnl'], -60)
            self.assertEqual(daily_realized_truth(conn, '2026-10-11', {'mode'}, 'non_live')['exact_today_pnl'], 100)
            self.assertEqual(daily_realized_truth(conn, '2026-10-11', {'mode'})['exact_today_pnl'], 40)

    def test_empty_missing_or_single_outcome_signal_samples_do_not_claim_ranking(self):
        import signal_validity_report as validity
        empty = {name: validity.field_report([], fn) for name, fn in validity.CORE_FIELDS}
        self.assertIsNone(empty["edge_margin_pp"]["mean"])
        self.assertIsNone(empty["edge_margin_pp"]["correlation_with_win"])
        self.assertEqual(validity.classify_surface(empty, [], {})["label"], "insufficient_ranking_evidence")
        rows = [{"edge_margin_pp": value, "next_return_pct": value, "won": True, "pnl": 1.0}
                for value in (.1, .2, .3)]
        reports = {name: validity.field_report(rows, fn) for name, fn in validity.CORE_FIELDS}
        self.assertEqual(reports["rsi"]["count"], 0)
        self.assertIsNone(reports["rsi"]["mean"])
        self.assertIsNone(reports["edge_margin_pp"]["correlation_with_win"])
        self.assertEqual(validity.classify_surface(reports, [], {})["label"], "insufficient_ranking_evidence")
        self.assertEqual(validity.metric_text(None), "unavailable")
        self.assertEqual(validity.metric_text(0.0), "0.0000")

    def test_report_child_credentials_are_environment_only(self):
        import taker_validation_report as wrapper
        environment = wrapper.credential_environment("private-key", "private-secret")
        command = ["python", "scripts/daily_alpha_report.py", "--preset", "test"]
        with patch.object(wrapper.subprocess, "run", return_value=SimpleNamespace(
            returncode=0, stdout='{"status":"pass"}', stderr="")) as run:
            child = wrapper.run_child("test", command, environment=environment)
        self.assertEqual(run.call_args.args[0], command)
        self.assertEqual(run.call_args.kwargs["env"]["MACBOT_API_KEY"], "private-key")
        self.assertEqual(run.call_args.kwargs["env"]["MACBOT_API_HMAC_SECRET"], "private-secret")
        self.assertNotIn("private-key", child.command)
        self.assertNotIn("private-secret", child.command)

    def test_generated_profiles_exclude_private_credentials_and_publish_privately(self):
        from build_maker_rollout_profiles import build_profile, write_profile
        secrets = {field: "private-credential" for field in (
            "api_key", "api_hmac_secret", "private_rpc_url", "finnhub_api_key", "lunarcrush_api_key")}
        profile = build_profile({**secrets, "trade_amount": 2}, api_key="another-private-key")
        self.assertEqual(profile["trade_amount"], 2)
        self.assertTrue(all(profile[field] == "" for field in secrets))
        self.assertEqual(secrets["api_key"], "private-credential")
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp) / "profile.json"
            target.write_text("old profile")
            target.chmod(0o644)
            write_profile(target, profile)
            self.assertNotIn("private-credential", target.read_text())
            self.assertEqual(target.stat().st_mode & 0o777, 0o600)
            self.assertFalse(list(Path(tmp).glob(".profile-*")))

    def test_absent_profile_and_single_outcome_group_are_unavailable(self):
        empty=numeric_profile([])
        self.assertEqual(empty["rsi"]["count"],0)
        self.assertIsNone(empty["rsi"]["mean"])
        profile=numeric_profile([{"score":0.0,"entry_price":.5,"path_low_price":0.0}])
        self.assertEqual(profile["score"]["mean"],0.0)
        self.assertEqual(profile["path_downside_pct"]["mean"],100.0)
        self.assertIsNone(profile["rsi"]["mean"])
        groups=winner_loser_profile([{"won":True,"score":.5}])
        self.assertIsNone(groups["winner_minus_loser_mean"]["score"])
        row=next(row for row in numeric_delta_table(empty,profile) if row["field"]=="score")
        self.assertIsNone(row["delta"])

    def test_shadow_report_requires_exact_input_provenance_and_directional_regime(self):
        import deep_model_shadow_filter_report as shadow
        import numpy as np
        self.assertFalse(shadow.regime_matches("TRENDING",{"TREND_UP"}))
        self.assertTrue(shadow.regime_matches("TREND_UP",{"TREND_UP"}))
        dataset=SimpleNamespace(meta=[{"source_kind":"reconstructed_inputs"}],samples=[{}])
        with self.assertRaisesRegex(ValueError,"reconstructed"):
            shadow.build_report(dataset=dataset,preds=np.array([.5]),days_list=[7],shift_days_list=[0],
                                threshold=.5,lane_regimes=set(),lane_signals=set(),entry_threshold=.3,
                                min_confidence_margin=0,session_allowlist=set(),blocked_hours=set())

    def test_model_sync_publishes_only_complete_checked_transfer(self):
        import onnx
        from onnx import helper, TensorProto
        source_script=(Path(__file__).resolve().parents[2] / "scripts" / "sync_model_from_pc.sh")
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);(root/"scripts").mkdir();(root/"models").mkdir();(root/"bin").mkdir()
            shutil.copyfile(source_script,root/"scripts"/source_script.name)
            (root/"training").mkdir()
            shutil.copyfile(source_script.parents[1]/"training"/"model_artifact.py", root/"training"/"model_artifact.py")
            incoming=root/"incoming.onnx"
            graph=helper.make_graph([helper.make_node("Identity",["x"],["y"])],"test",
                                    [helper.make_tensor_value_info("x",TensorProto.FLOAT,[1])],
                                    [helper.make_tensor_value_info("y",TensorProto.FLOAT,[1])])
            model=helper.make_model(graph,opset_imports=[helper.make_opsetid("",13)],ir_version=8)
            onnx.save(model,incoming)
            scp=root/"bin"/"scp"
            scp.write_text('#!/bin/bash\ncp "$MACBOT_TEST_MODEL_SRC" "$2/latest.onnx"\nexit "$MACBOT_TEST_SCP_EXIT"\n')
            scp.chmod(0o700)
            env=dict(os.environ,PATH=str(root/"bin")+os.pathsep+os.environ["PATH"],
                     MACBOT_TEST_MODEL_SRC=str(incoming),MACBOT_TEST_SCP_EXIT="1",MACBOT_SYNC_PYTHON=os.sys.executable)
            active=root/"models"/"latest.onnx";active.write_bytes(b"previous model")
            command=["bash",str(root/"scripts"/source_script.name),"mock-pc"]
            failed=subprocess.run(command,env=env,capture_output=True,text=True)
            self.assertNotEqual(failed.returncode,0);self.assertEqual(active.read_bytes(),b"previous model")
            env["MACBOT_TEST_SCP_EXIT"]="0";incoming.write_bytes(b"incomplete ONNX")
            invalid=subprocess.run(command,env=env,capture_output=True,text=True)
            self.assertNotEqual(invalid.returncode,0);self.assertEqual(active.read_bytes(),b"previous model")
            onnx.save(model,incoming)
            passed=subprocess.run(command,env=env,capture_output=True,text=True)
            self.assertEqual(passed.returncode,0,passed.stderr);self.assertEqual(active.read_bytes(),incoming.read_bytes())
            previous = active.read_bytes()
            # A structurally valid Constant can hide a sidecar tensor outside
            # graph.initializer. Publication must refuse it before native
            # checker file access, preserving the last accepted model.
            tensor=helper.make_tensor("external",TensorProto.FLOAT,[1],[0.0])
            tensor.ClearField("float_data");tensor.data_location=TensorProto.EXTERNAL
            tensor.external_data.add(key="location",value="latest.onnx")
            external=helper.make_model(helper.make_graph(
                [helper.make_node("Constant",[],["y"],value=tensor)],"sidecar",[],
                [helper.make_tensor_value_info("y",TensorProto.FLOAT,[1])]),
                opset_imports=[helper.make_opsetid("",13)],ir_version=8)
            incoming.write_bytes(external.SerializeToString())
            rejected=subprocess.run(command,env=env,capture_output=True,text=True)
            self.assertNotEqual(rejected.returncode,0)
            self.assertIn("self-contained", rejected.stderr)
            self.assertEqual(active.read_bytes(),previous)
            self.assertFalse(list((root/"models").glob('.model-sync.*')))

    def test_asymmetric_payoffs_and_missing_edge_are_not_fabricated(self):
        require_resolution_edge_mode("next_candle")
        with self.assertRaisesRegex(ValueError, "asymmetric"):
            require_resolution_edge_mode("taker_tp_sl")
        for result in ({}, {"edge_audit":{"edge_gap_pp":None}}, {"edge_audit":{"edge_gap_pp":float("inf")}}):
            with self.assertRaises(ValueError):
                edge_gap_from_result(result)
        self.assertEqual(edge_gap_from_result({"edge_audit":{"edge_gap_pp":0.0}}), 0.0)

    def test_research_gate_cannot_pass_without_trades(self):
        score = recency.CandidateScore(recency.Candidate(.3,"both",False,8,0,8),
                                      [recency.WindowScore(7,[recency.FoldResult(0,0,0,0,0)])],None)
        gates = recency.Gates(0,0,0,0,0,0,10,10)
        status, checks = recency.evaluate_gates(score,gates)
        self.assertEqual(status,"fail")
        self.assertFalse(checks["trade_evidence_present"])
        dscore = daily.CandidateScore(daily.Candidate(.3,"both",False),[daily.FoldResult(0,0,0,0,0)])
        self.assertEqual(daily.evaluate_gates(dscore,daily.Gates(0,0,0,0,0,0))[0],"fail")

    def test_meta_holdout_purges_shared_market_outcomes(self):
        rows=[{"market_id":m} for m in ["old","shared","shared","new"]]
        train,test=meta.chronological_market_split(rows,.5)
        self.assertEqual(train,[0])
        self.assertEqual(test,[2,3])
        self.assertFalse({rows[i]["market_id"] for i in train}&{rows[i]["market_id"] for i in test})
        with self.assertRaisesRegex(ValueError,"independent markets"):
            meta.chronological_market_split([{"market_id":"one"}]*4,.5)

    def test_artifact_commands_redact_both_secret_argument_forms(self):
        command=["python","--api-key","private-key","--api-secret=private-secret","--days","7"]
        redacted=redacted_command(command)
        self.assertNotIn("private-key",str(redacted)); self.assertNotIn("private-secret",str(redacted))
        self.assertEqual(redacted[-2:],["--days","7"])
        self.assertEqual(command[2],"private-key", "actual child command must not be mutated")

    def test_control_bundle_rejects_implausible_or_unclosed_execution(self):
        maker={"summary":{"trade_count":10}}
        integrity={"status":"ok","trade_counts":{"open_count":0,"closing_count":0}}
        evidence={"summary":{"execution_fill_rows":1,"plausible_execution_rows":0,"implausible_execution_rows":1,"unverifiable_rows":0}}
        self.assertEqual(control.derive_terminal_class(maker_report=maker,sim_report=evidence,integrity_report=integrity),"market_quality_failure")
        evidence["summary"].update(plausible_execution_rows=1,implausible_execution_rows=0)
        integrity["trade_counts"]["closing_count"]=1
        self.assertEqual(control.derive_terminal_class(maker_report=maker,sim_report=evidence,integrity_report=integrity),"lifecycle_bug")

    def test_zero_prediction_and_empty_guard_report_are_preserved(self):
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/"data.sqlite"
            with sqlite3.connect(path) as conn:
                conn.execute("CREATE TABLE ml_model_inputs(timestamp,market_id,run_id,signal,regime_label,phase,fusion_score,p_up,features_json,outcome)")
                conn.execute("INSERT INTO ml_model_inputs VALUES('2026-01-01T00:00:00Z','m','r','BUY_NO','TREND_UP','MID',-.5,0,'[]',1)")
            result=wrong_report.analyze_rows(path,[],.7,5)
            self.assertEqual(result["confident_wrong_rows"],1)
            self.assertEqual(result["top_wrong_rows"][0]["p_up"],0)
            rows=no_guard.load_rows(path)
            self.assertFalse(no_guard.build_candidates()[1].block(rows[0]), "zero p_up is a strong NO probability")
        text=no_guard.markdown_report({"db":"empty","buy_no_rows":0,"baseline_wrong_rate_pct":None,
                                      "candidates":[no_guard.summarize_candidate(no_guard.build_candidates()[0],[])]})
        self.assertIn("unavailable",text)

    def test_sim_manual_sell_evidence_and_invalid_probability(self):
        details={"path":"manual_orderbook","side":"sell","intended_price":.5,"observed_price":.49,
                 "snapshot":{"token_id":"t","best_bid":.5,"best_ask":.51}}
        row={"id":1,"trade_id":1,"token_id":"t","phase":"exit","source":"sim","details":json.dumps(details)}
        self.assertEqual(sim.classify_fill(row).status,"plausible")
        details["observed_price"]=float("inf");row["details"]=json.dumps(details)
        self.assertEqual(sim.classify_fill(row).status,"unverifiable")
        details["observed_price"]=.49;details["snapshot"]["token_id"]="other";row["details"]=json.dumps(details)
        self.assertEqual(sim.classify_fill(row).status,"implausible")
        details["path"]="maker_passive";row["details"]=json.dumps(details)
        self.assertEqual(sim.classify_fill(row).status,"implausible", "maker paths require the same token too")
        details["path"]="resolution";details["observed_price"]=0.0
        row["phase"]="mark_exit";row["details"]=json.dumps(details)
        payout=sim.classify_fill(row)
        self.assertEqual(payout.classification,"resolution_payout")
        self.assertEqual(payout.observed_price,0.0)
        self.assertEqual(payout.status,"n/a")

    def test_reconstruction_groups_partial_entry_receipts_and_requires_full_close(self):
        conn=sqlite3.connect(":memory:");conn.row_factory=sqlite3.Row
        conn.execute("CREATE TABLE trade_fills(id,trade_id,phase,order_id,timestamp)")
        conn.executemany("INSERT INTO trade_fills VALUES(?,1,?,'entry-order','2026-01-01T00:00:00Z')",[(1,"entry"),(2,"entry"),(3,"exit")])
        segments=reconstruction.build_segments(conn)
        self.assertEqual(len(segments),1); self.assertEqual(len(segments[0].fills),3)
        self.assertEqual(reconstruction.classify_segment(100,96),"partial_or_truncated")
        self.assertEqual(reconstruction.classify_segment(100,101),"inconsistent_exit_quantity")
        conn.close()

    def test_hygiene_zero_limit_really_selects_no_artifacts(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);(root/"one_validation_control_bundle.json").write_text('{}')
            self.assertEqual(hygiene.summarize_bundles(root,root,0)["recent_bundle_count"],0)

    def test_training_labels_exclude_early_profit_and_bad_features(self):
        conn = sqlite3.connect(":memory:")
        conn.execute("CREATE TABLE trades(direction,was_correct,confidence,raw_confidence,signals_json,status,exit_type)")
        for exit_type, signals in [("manual", {"flow": 0.8}), ("resolution", []),
                                   ("resolution", {"flow": "NaN"}), ("resolution", {"flow": 0.8})]:
            conn.execute("INSERT INTO trades VALUES('BUY_NO',0,.8,NULL,?,'CLOSED',?)", (json.dumps(signals), exit_type))
        rows = calibration.load_rows(conn)
        self.assertEqual(len(rows), 1)
        x, y, names = calibration.build_dataset(rows)
        self.assertEqual(names, ["flow"])
        self.assertEqual(float(x[0, 0]), 0.8)
        self.assertEqual(float(y[0]), 1.0, "losing NO at resolution means market resolved YES")
        conn.close()

    def test_hour_groups_use_utc_including_day_rollover(self):
        dt = parse_ts("2026-10-11T01:00:00+02:00")
        self.assertEqual(dt.hour, 23)
        self.assertEqual(dt.day, 10)
        self.assertEqual(parse_ts("2026-10-11T01:00:00").hour, 1)

    def test_contradictory_resolution_cannot_pass_external_truth_check(self):
        for prices in ([1, 1], [0, 0], [float("nan"), 1], "not JSON"):
            self.assertIsNone(resolution({"umaResolutionStatus":"resolved", "outcomes":["Yes","No"], "outcomePrices":prices}))
        self.assertEqual(resolution({"umaResolutionStatus":"resolved", "outcomes":"[\"No\",\"Yes\"]", "outcomePrices":"[\"1\",\"0\"]"}), "DOWN")
        self.assertIsNone(resolution({"umaResolutionStatus":"resolved", "winner":"Yes", "outcomes":["Yes","No"], "outcomePrices":[0,1]}))

    def test_dashboard_reconciliation_uses_net_pnl_and_utc_exit_day(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "history.sqlite"
            conn = sqlite3.connect(path)
            conn.execute("CREATE TABLE trades(status,pnl,fee_paid,accounting_version,mode,amount,exit_timestamp)")
            stamp = datetime.now(timezone.utc).replace(hour=0,minute=10).astimezone(timezone(timedelta(hours=-5))).isoformat()
            conn.executemany("INSERT INTO trades VALUES('CLOSED',?,?,?,'simulation',0,?)", [(1.0,0.1,0,stamp),(0.4,0.2,1,stamp)])
            conn.commit(); conn.close()
            self.assertAlmostEqual(db_truth(str(path))["today_pnl"], 1.3)

    def test_multiple_quote_audits_do_not_multiply_fill_rows(self):
        conn = sqlite3.connect(":memory:")
        conn.executescript("CREATE TABLE trades(id); CREATE TABLE trade_fills(id,trade_id,order_id,phase,source); CREATE TABLE order_outbox(id,order_id,stage,action,details);")
        conn.execute("INSERT INTO trades VALUES(1)")
        conn.execute("INSERT INTO trade_fills VALUES(1,1,'quote','entry','maker_sim')")
        conn.execute("INSERT INTO trade_fills VALUES(2,1,'taker','entry','simulation')")
        conn.executemany("INSERT INTO order_outbox VALUES(?,'quote','maker_quote','fill',?)", [(1,'{"opened":1}'),(2,'opened=0')])
        result = load_fill_integrity(conn)
        self.assertEqual(result["total_count"], 2)
        self.assertEqual(result["entry_fill_missing_outbox_ids"], [])
        self.assertEqual(result["maker_quote_fill_opened_missing_trade_fill_ids"], [])
        conn.close()


if __name__ == "__main__":
    unittest.main()
