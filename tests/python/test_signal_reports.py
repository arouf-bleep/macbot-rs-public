"""Report regressions: absent evidence must not become sale or rollout proof."""
from __future__ import annotations

import contextlib
import io
import json
import math
import os
import sqlite3
import sys
import tempfile
import unittest
from datetime import datetime, timezone, timedelta
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import l2_replay_check as replay
import live_shadow_report as live
import maker_ab_compare as compare
import maker_rollout_readiness_report as readiness
import maker_soak_report as maker
import model_input_archive_report as archive
import model_input_validity_report as validity
import recent_specialist_validation_report as recent
import recent_specialist_soak_report as soak
import shadow_kill_check as shadow
import shutdown_validation_attempts_report as shutdown
import soak_watch_report as watch
import taker_soak_report as taker
import run_maker_soak as runner
import run_maker_ab_cycle as cycle
import run_maker_ab_batch as batch
import run_maker_rollout_gate as gate
import run_frozen_maker_soak as frozen


class SignalReportTests(unittest.TestCase):
    def test_batched_sequence_belongs_to_each_token_once_per_frame(self):
        frame = {"event_type":"price_change", "market":"condition", "sequence":11,
                 "price_changes":[{"asset_id":"yes","price":.4}, {"asset_id":"yes","price":.5}, {"asset_id":"no","price":.6}]}
        self.assertEqual(replay.frame_observations(frame), [("yes",11,"price_change"),("no",11,"price_change")])
        self.assertIsNone(replay.extract_sequence({"sequence": True}))
        self.assertEqual(replay.frame_observations(dict(frame, seq=True)), [("yes",11,"price_change"),("no",11,"price_change")])
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/"stream.jsonl"
            path.write_text(json.dumps([{"event_type":"book","asset_id":"yes","sequence":10},{"event_type":"book","asset_id":"no","sequence":10}])+"\n"+json.dumps(frame)+"\n")
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(replay.run(path),0)
            path.write_text(json.dumps(frame)+"\n"+json.dumps(frame)+"\n")
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(replay.run(path),2)
            path.write_text(json.dumps({"asset_id":"yes","price":.4})+"\n")
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(replay.run(path),2)

    def test_absent_windows_cannot_be_comparable_or_promotable(self):
        baseline={"cancel_event_count":250,"summary":{"avg_adverse_move_bps":100,"p90_adverse_move_bps":120}}
        candidate={"cancel_event_count":250,"summary":{"avg_adverse_move_bps":10,"p90_adverse_move_bps":20}}
        report=compare.compare_reports(baseline,candidate,Path("b"),Path("c"))
        self.assertFalse(report["comparable_window"])
        self.assertNotEqual(report["verdict"],"promising_guard_candidate")

    def test_selected_candidate_cannot_borrow_another_profile_evidence(self):
        rows=[{"label":"other","verdict":"promising_guard_candidate","avg_adverse_move_delta_bps":-100}]
        self.assertIsNone(readiness.select_front_runner(rows,"selected"))
        self.assertEqual(readiness.front_runner_cost({"avg_adverse_move_delta_bps":0.0,"avg_places_delta_pct":0.0,"avg_p90_adverse_delta_bps":0.0}),0.0)

    def test_shutdown_proof_requires_complete_execution_and_no_unfinished_orders(self):
        maker_report={"summary":{"trade_count":2,"open_trade_count":0}}
        integrity={"status":"ok","verdict":"ok","counts":{"trades":{"cleanup":2}},"trade_counts":{"open_count":0,"closing_count":0}}
        simulation={"summary":{"execution_fill_rows":2,"plausible_execution_rows":2,"implausible_execution_rows":0,"unverifiable_rows":0}}
        self.assertEqual(shutdown.classify_attempt(maker_report,simulation,integrity,{})["class"],"proof")
        self.assertNotEqual(shutdown.classify_attempt(maker_report,None,integrity,{})["class"],"proof")
        integrity["trade_counts"]["closing_count"]=1
        self.assertNotEqual(shutdown.classify_attempt(maker_report,simulation,integrity,{})["class"],"proof")

    def test_read_reports_do_not_create_missing_databases(self):
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/"missing.sqlite"
            with self.assertRaises(sqlite3.OperationalError):
                live.load_archive_stats(path)
            self.assertFalse(path.exists())

    def test_archive_nonfinite_tensors_do_not_appear_aligned(self):
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/"archive.sqlite"
            with sqlite3.connect(path) as conn:
                conn.execute("CREATE TABLE ml_model_inputs(timestamp,market_id,run_id,signal,regime_label,regime_value,features_json,scalars_json,staleness_json,candles_1m_json,candles_5m_json,candles_15m_json,orderbook_seq_json,outcome)")
                tensors=[json.dumps({"a":0,"b":0}),json.dumps([math.nan]+[0]*7),json.dumps([0]*2),json.dumps([0]*300),json.dumps([0]*120),json.dumps([0]*80),json.dumps([0]*540)]
                conn.execute("INSERT INTO ml_model_inputs VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)",["2026-01-01T00:00:00Z","m","reconstructed:r","BUY_YES","TREND_UP",1]+tensors+[1])
            report=archive.analyze_archive(path,["a","b"])
            self.assertFalse(report["contract_valid"])
            self.assertFalse(report["runtime_provenance_only"])
            self.assertEqual(report["contract_rows"]["scalar_prefix_aligned_rows"],0)
            self.assertEqual(report["contract_rows"]["parse_error_rows"],1)

    def test_live_archive_excludes_reconstructed_or_legacy_backfill_rows(self):
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/"archive.sqlite"
            with sqlite3.connect(path) as conn:
                conn.execute("CREATE TABLE ml_model_inputs(timestamp,market_id,run_id,outcome)")
                for run in ["runtime","reconstructed:r","backfill_2026"]:
                    conn.execute("INSERT INTO ml_model_inputs VALUES('2026-01-01T00:00:00Z',?,?,1)",(run,run))
            stats=live.load_archive_stats(path)
            self.assertEqual(stats["total_rows"],1)
            self.assertEqual(stats["labeled_rows"],1)

    def test_report_child_commands_redact_credentials(self):
        command=["python","--api-key","secret-key","--api-secret=secret-signature"]
        process=SimpleNamespace(stdout="{}",stderr="",returncode=0)
        with patch.object(recent.subprocess,"run",return_value=process):
            report=recent.run_child("test",command)
        self.assertNotIn("secret-key",str(report.command))
        self.assertNotIn("secret-signature",str(report.command))
        self.assertEqual(command[2],"secret-key")

    def test_historical_anchor_does_not_stand_in_for_current_day(self):
        run=soak.ShiftValidation(1,0,{},[])
        with self.assertRaisesRegex(ValueError,"current anchor"):
            soak.summarize_runs("test",[run])

    def test_child_process_arguments_do_not_contain_credentials(self):
        def inspect_child(command, **kwargs):
            self.assertNotIn("key-private", command)
            self.assertNotIn("secret-private", command)
            self.assertEqual(os.environ["MACBOT_API_KEY"], "key-private")
            self.assertEqual(os.environ["MACBOT_API_HMAC_SECRET"], "secret-private")
            raise RuntimeError("checked child authentication transport")
        for module in (recent,soak,taker):
            with self.subTest(module=module.__name__),patch.dict(os.environ,{},clear=True),patch.object(sys,"argv",[module.__name__,"--api-key","key-private","--api-secret","secret-private"]),patch.object(module.subprocess,"run",side_effect=inspect_child):
                with self.assertRaisesRegex(RuntimeError,"checked child"):
                    module.main()

    def test_quoteability_and_finite_summary_admit_missing_values(self):
        self.assertIsNone(maker.mean_or_none([math.nan,math.inf,"bad",None]))
        self.assertEqual(maker.mean_or_none([0,math.nan,2]),1)
        self.assertEqual(maker.basis_bucket(math.nan), "unknown")
        self.assertEqual(maker.quote_age_bucket(-1), "unknown")
        self.assertEqual(maker.roll_bucket(math.nan,1), "unknown")
        summary=watch.summarize_events([{"event":"cancel","adverse_move_bps":math.nan,"quote_age_ms":"bad"}])
        self.assertIsNone(summary["avg_recent_adverse_move_bps"])
        parsed=maker.event_timestamp({"_timestamp":"2026-01-01T02:00:00+02:00"})
        self.assertEqual(parsed.hour,0)
        state=maker.summarize_quoteability_state({"snapshot_available":True,"loaded_count":1,"two_sided_count":1,"quoteable_count":1,"l2_stale":True})
        self.assertEqual(state["label"],"stale_books")

    def test_missing_drawdown_denominator_cannot_pass_shadow_gate(self):
        trades={"trades":[{"status":"CLOSED","source":"shadow","pnl":1,"timestamp":"2026-01-01T00:00:00Z"}]}
        thresholds=shadow.Thresholds(min_trades=1,min_recent_trades=1)
        with patch.object(shadow.Thresholds,"from_env",return_value=thresholds),patch.object(shadow,"fetch_json",side_effect=[trades,{"initial_balance":0}]),contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(shadow.main(),2)
        self.assertIsNone(json.loads(output.getvalue())["metrics"]["max_drawdown_pct"])
        with self.assertRaises(ValueError):
            shadow.Thresholds(min_win_rate=math.nan)

    def test_shadow_drawdown_orders_actual_instants_and_rejects_invalid_time(self):
        rows=[{"timestamp":"2026-01-01T02:00:00+02:00"},{"timestamp":"2026-01-01T00:30:00Z"}]
        self.assertLess(shadow.parse_ts_key(rows[0]),shadow.parse_ts_key(rows[1]))
        trades={"trades":[{"status":"CLOSED","source":"shadow","pnl":1,"timestamp":"bad"}]}
        with patch.object(shadow,"fetch_json",side_effect=[trades,{"initial_balance":100}]),contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(shadow.main(),1)

    def test_configuration_fingerprint_covers_settings_and_excludes_credentials(self):
        cfg={"mode":"simulation","execution_mode":"maker","stake":7,"api_key":"private","nested":{"secret":"x","value":2}}
        other={"nested":{"value":2,"secret":"different"},"api_key":"changed", "stake":7,"execution_mode":"maker","mode":"simulation"}
        self.assertEqual(runner.config_fingerprint(cfg),runner.config_fingerprint(other))
        self.assertNotEqual(runner.config_fingerprint(cfg),runner.config_fingerprint(dict(cfg,stake=8)))
        self.assertNotIn("private",json.dumps(runner.configuration_evidence(cfg)))
        with self.assertRaises(ValueError):
            runner.config_fingerprint(dict(cfg,stake=math.nan))
        with tempfile.TemporaryDirectory() as tmp:
            path=Path(tmp)/"cfg.json";path.write_text(json.dumps(other))
            runner.assert_configuration_unchanged(path,cfg)
            path.write_text(json.dumps(dict(other,stake=9)))
            with self.assertRaises(ValueError):
                runner.assert_configuration_unchanged(path,cfg)

    def test_compare_and_readiness_require_the_exact_selected_configuration(self):
        base_cfg={"mode":"simulation","execution_mode":"maker","stake":7,"maker_backwardation_widen_bps":0.0,"maker_backwardation_min_target_spread_bps":0.0,"maker_roll_toxic_guard_enabled":False}
        candidate_cfg=dict(base_cfg,maker_backwardation_widen_bps=20)
        baseline={"cancel_event_count":250,"place_event_count":500,"session_buckets":{"asia":{"count":250}},"hour_buckets":{"0":{"count":250}},"summary":{"avg_adverse_move_bps":100,"p90_adverse_move_bps":120},**runner.configuration_evidence(base_cfg)}
        candidate={**baseline,"summary":{"avg_adverse_move_bps":10,"p90_adverse_move_bps":20},**runner.configuration_evidence(candidate_cfg)}
        report=compare.compare_reports(baseline,candidate,Path("b"),Path("c"))
        self.assertTrue(report["configuration_bound"])
        self.assertEqual(report["verdict"],"promising_guard_candidate")
        wrong={**candidate,**runner.configuration_evidence(dict(candidate_cfg,stake=9))}
        self.assertEqual(compare.compare_reports(baseline,wrong,Path("b"),Path("c"))["verdict"],"configuration_unverified")
        wrong_baseline={**baseline,**runner.configuration_evidence(dict(base_cfg,maker_roll_toxic_guard_enabled=True))}
        self.assertEqual(compare.compare_reports(wrong_baseline,candidate,Path("b"),Path("c"),"widen")["verdict"],"configuration_unverified")
        malformed={**candidate,**runner.configuration_evidence(dict(candidate_cfg,maker_backwardation_widen_bps="unavailable"))}
        self.assertEqual(compare.compare_reports(baseline,malformed,Path("b"),Path("c"),"widen")["verdict"],"configuration_unverified")
        selected=runner.config_fingerprint(candidate_cfg)
        rows=[{"label":"widen_20bps","verdict":"promising_repeatable_candidate","avg_adverse_move_delta_bps":-90,"configuration_bound":True,"candidate_config_fingerprint":runner.config_fingerprint(dict(candidate_cfg,stake=9))}]
        self.assertIsNone(readiness.select_front_runner(rows,"widen_20bps",selected))
        rows[0].pop("candidate_config_fingerprint")
        self.assertIsNone(readiness.select_front_runner(rows,"widen_20bps",selected))
        rows[0]["candidate_config_fingerprint"]=selected
        rows[0]["candidate_mode"]="widen"
        rows[0]["treatment_mode"]="widen"
        self.assertIsNotNone(readiness.select_front_runner(rows,"widen_20bps",selected))

    def test_batch_cannot_promote_legacy_or_mixed_configuration_evidence(self):
        with patch.object(sys,"argv",["batch"]):
            args=batch.parse_args()
        with tempfile.TemporaryDirectory() as tmp:
            reports=[]
            for i in range(2):
                comparison={"unique_fixture":i,"configuration_bound":True,"treatment_mode":"widen","baseline_config_fingerprint":"baseline","candidate_config_fingerprint":"selected","sample_adequate":True,"comparable_window":True,"coverage_starved":False,"verdict":"promising_guard_candidate","metrics":{"avg_adverse_move_bps":{"delta":-90},"p90_adverse_move_bps":{"delta":-100},"places":{"delta_pct":0},"basis_widen_count_at_decision":{"candidate":5}}}
                path=Path(tmp)/f"{i}.json";path.write_text(json.dumps(comparison))
                reports.append({"baseline":{"events":250},"candidate":{"events":250},"compare":{"json":str(path)},"cycle_path":str(i)})
            self.assertEqual(batch.summarize_batch(reports,args)["verdict"],"promising_repeatable_candidate")
            args.expected_candidate_fingerprint="different"
            self.assertEqual(batch.summarize_batch(reports,args)["verdict"],"configuration_unverified")
            args.expected_candidate_fingerprint=None
            first=Path(reports[0]["compare"]["json"])
            unbound=json.loads(first.read_text());unbound.pop("candidate_config_fingerprint");first.write_text(json.dumps(unbound))
            self.assertEqual(batch.summarize_batch(reports,args)["verdict"],"configuration_unverified")

    def test_latest_readiness_groups_by_configuration_and_keeps_negative_update(self):
        with tempfile.TemporaryDirectory() as tmp:
            for index,(fingerprint,verdict,time) in enumerate([("old","promising_repeatable_candidate","1"),("old","repeatable_regression_when_engaged","3"),("different","promising_repeatable_candidate","4"),(None,"promising_repeatable_candidate","5")]):
                payload={"generated_at":time,"verdict":verdict,"configuration_bound":fingerprint is not None,"treatment_mode":"widen","candidate_config_fingerprint":fingerprint,"args":{"candidate_mode":"widen","maker_backwardation_widen_bps":20}}
                (Path(tmp)/f"{index}.json").write_text(json.dumps(payload))
            rows=readiness.latest_batches_by_configuration(Path(tmp))
            self.assertEqual(len(rows),3)
            self.assertIsNone(readiness.select_front_runner(rows,"widen_20bps","old"))
            self.assertIsNone(readiness.select_front_runner(rows,"widen_20bps","selected"))
            self.assertEqual(readiness.select_front_runner(rows,"widen_20bps","different")["candidate_config_fingerprint"],"different")

    def test_exact_ab_preserves_custom_settings_and_refuses_profile_change(self):
        with tempfile.TemporaryDirectory() as tmp:
            candidate={"mode":"simulation","execution_mode":"maker","maker_quote_notional_usd":7,"take_profit":False,"api_key":"private","maker_backwardation_widen_bps":17,"maker_backwardation_min_target_spread_bps":0}
            path=Path(tmp)/"selected.json";original=json.dumps(candidate);path.write_text(original)
            args=SimpleNamespace(candidate_config=str(path),expected_candidate_fingerprint=runner.config_fingerprint(candidate),candidate_mode="widen",duration_minutes=0,report_hours=1,cargo_cmd="unused",base_url="http://127.0.0.1:18099")
            seen=[]
            def inspect_command(command,root):
                self.assertIn("--exact-config",command)
                config=Path(command[command.index("--config")+1]);cfg=json.loads(config.read_text())
                self.assertEqual(config.stat().st_mode & 0o777,0o600)
                self.assertEqual(cfg["maker_quote_notional_usd"],7)
                self.assertFalse(cfg["take_profit"])
                seen.append(cfg)
                return runner.configuration_evidence(cfg)
            with patch.object(cycle,"run_json",side_effect=inspect_command),patch.object(cycle.time,"sleep"):
                cycle.run_exact_legs(args,Path(tmp),Path(tmp)/"b.sqlite",Path(tmp)/"c.sqlite")
            self.assertEqual(seen[0]["maker_backwardation_widen_bps"],0)
            self.assertEqual(seen[1],candidate)
            self.assertEqual(path.read_text(),original)
            args.expected_candidate_fingerprint="wrong"
            with patch.object(cycle,"run_json") as child,self.assertRaises(ValueError):
                cycle.run_exact_legs(args,Path(tmp),Path(tmp)/"b.sqlite",Path(tmp)/"c.sqlite")
            child.assert_not_called()

    def test_rollout_gate_keeps_selected_profile_and_checks_bound_readiness(self):
        with tempfile.TemporaryDirectory() as tmp:
            cfg={"mode":"simulation","execution_mode":"maker","maker_quote_notional_usd":7,"maker_backwardation_widen_bps":17}
            selected=Path(tmp)/"selected.json";original=json.dumps(cfg);selected.write_text(original)
            fp=runner.config_fingerprint(cfg)
            state=Path(tmp)/"state.json";state.write_text(json.dumps({"active_candidate_profile":str(selected)}))
            ready=Path(tmp)/"ready.json";ready.write_text(json.dumps({"verdict":"paper_rollout_candidate_ready","active_candidate_profile":{"config_fingerprint":fp},"front_runner":{"label":"widen_17bps","configuration_bound":True,"treatment_mode":"widen","candidate_config_fingerprint":fp},"blockers":[]}))
            def inspect_child(command,root):
                self.assertIn("scripts/maker_rollout_readiness_report.py",command)
                return {"json":str(ready)}
            with patch.object(sys,"argv",["gate","--rollout-state",str(state),"--outdir",tmp,"--rounds","1"]),patch.object(gate,"run_json",side_effect=inspect_child),patch.object(gate,"run_ready_soak",return_value={"config_fingerprint":fp}),contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(gate.main(),0)
            self.assertEqual(selected.read_text(),original)
            with self.assertRaises(ValueError):
                gate.validate_selected_readiness(SimpleNamespace(_candidate_fingerprint=fp),{"active_candidate_profile":{"config_fingerprint":"different"}})

    def test_frozen_runner_classifies_the_saved_report_not_its_pointer(self):
        with tempfile.TemporaryDirectory() as tmp:
            cfg=Path(tmp)/"cfg.json";cfg.write_text(json.dumps({"mode":"simulation","execution_mode":"maker"}))
            saved=Path(tmp)/"report.json";saved.write_text(json.dumps({"summary":{"trade_count":1,"fill_count":2},"event_count":100}))
            sim={"summary":{"execution_fill_rows":2,"plausible_execution_rows":2,"implausible_execution_rows":0,"unverifiable_rows":0}}
            integrity={"status":"ok","trade_counts":{"open_count":0,"closing_count":0}}
            window={"elapsed_seconds":0,"stop_on_open_target_reached":False,"peak_active_trade_count":1,"exit_cause":"duration"}
            pump=SimpleNamespace(join=lambda **kwargs:None)
            with patch.object(sys,"argv",["frozen","--config",str(cfg),"--db",str(Path(tmp)/"fresh.sqlite"),"--duration-minutes","0","--base-url","http://127.0.0.1:18099"]),patch.object(frozen,"preflight_bind_conflict"),patch.object(frozen,"_spawn_logged_process",return_value=(object(),pump,[])),patch.object(frozen,"wait_ready"),patch.object(frozen,"fetch_json",return_value={}),patch.object(frozen,"wait_for_soak_window",return_value=window),patch.object(frozen,"terminate_process"),patch.object(frozen,"run_report",return_value={"json":str(saved),"md":None,"events":100}),patch.object(frozen,"run_sim_verification_report",return_value=sim),patch.object(frozen,"run_validation_integrity_report",return_value=integrity),patch.object(frozen,"write_validation_bundle",return_value={}),contextlib.redirect_stdout(io.StringIO()) as output:
                self.assertEqual(frozen.main(),0)
            payload=json.loads(output.getvalue())
            self.assertEqual(payload["terminal_class"],"success")
            self.assertEqual(payload["report"]["summary"]["trade_count"],1)
            self.assertEqual(payload["config_fingerprint"],runner.config_fingerprint(json.loads(cfg.read_text())))

    def test_nullable_correlations_render_as_unavailable(self):
        child={"status":"pass","artifacts":{}}
        report={"generated_at_utc":"now","preset":"test","anchor_shift_days":0,"overall":{"verdict":"test","recent_window_pass":False,"long_stress_pass":False,"lane_positive_now":False,"summary_line":"test"},"daily_report":child,"recent_recency":child,"long_recency":child,"lane_forensics":{"summary":{"trades":0,"total_pnl":0,"win_rate":0,"edge_gap_pp":0}},"signal_audit":{"shifts":[{"surface_assessment":{"edge_margin_corr_win":None,"next_return_corr_win":None}}]}}
        with tempfile.TemporaryDirectory() as tmp:
            _,markdown=recent.write_outputs(report,Path(tmp))
            self.assertIn("edge_margin_corr_win: unavailable",markdown.read_text())
            self.assertIn("next_return_corr_win: unavailable",markdown.read_text())


if __name__ == "__main__":
    unittest.main()
