#!/usr/bin/env python3
"""Run a baseline/guarded maker A/B cycle and emit a comparison artifact."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
import tempfile
import uuid
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from run_maker_soak import config_fingerprint, baseline_from_candidate


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run a full maker A/B soak cycle")
    parser.add_argument("--config", default="config.json", help="Base config JSON path")
    parser.add_argument("--candidate-config", help="Exact frozen candidate config; baseline changes only the selected treatment")
    parser.add_argument("--expected-candidate-fingerprint", help="Refuse a candidate config that changed since selection")
    parser.add_argument(
        "--outdir",
        default="artifacts/maker-ab-cycles",
        help="Directory for cycle metadata",
    )
    parser.add_argument(
        "--dbdir",
        default="artifacts/maker-soak-dbs",
        help="Directory for isolated soak DBs",
    )
    parser.add_argument(
        "--duration-minutes",
        type=float,
        default=20.0,
        help="Minutes per leg (baseline and candidate)",
    )
    parser.add_argument(
        "--report-hours",
        type=float,
        default=1.0,
        help="Lookback window passed through to maker_soak_report",
    )
    parser.add_argument(
        "--enable-macro-guard",
        action="store_true",
        help="Enable the macro-event guard for both legs",
    )
    parser.add_argument(
        "--session-allowlist",
        default="ASIA,US",
        help="Comma-separated runtime session allowlist",
    )
    parser.add_argument(
        "--blocked-hours-utc",
        default="1,3,4,6,16,17,18,19,20",
        help="Comma-separated blocked UTC entry hours",
    )
    parser.add_argument(
        "--min-confidence-margin",
        type=float,
        default=0.03,
        help="Minimum runtime confidence margin above the mode floor",
    )
    parser.add_argument(
        "--maker-backwardation-guard-pct",
        type=float,
        default=-0.02,
        help="Backwardation threshold for the candidate leg",
    )
    parser.add_argument(
        "--maker-backwardation-widen-bps",
        type=float,
        default=40.0,
        help="Additional spread widening in bps for the candidate leg when running in widen mode",
    )
    parser.add_argument(
        "--maker-backwardation-min-target-spread-bps",
        type=float,
        default=0.0,
        help="Minimum target spread in bps enforced when backwardation widening is active",
    )
    parser.add_argument(
        "--candidate-mode",
        choices=["guard", "widen", "roll_guard"],
        default="guard",
        help="Candidate leg behavior: backwardation guard, spread widening, or side-aware Roll toxicity suppression",
    )
    parser.add_argument(
        "--maker-roll-toxic-min-confidence",
        type=float,
        default=0.15,
        help="Minimum Roll confidence for the candidate leg when running in roll_guard mode",
    )
    parser.add_argument(
        "--maker-roll-toxic-min-signal",
        type=float,
        default=0.05,
        help="Minimum absolute Roll signal for the candidate leg when running in roll_guard mode",
    )
    parser.add_argument(
        "--maker-roll-toxic-min-sample-count",
        type=int,
        default=6,
        help="Minimum Roll sample count for the candidate leg when running in roll_guard mode",
    )
    parser.add_argument(
        "--cargo-cmd",
        default="cargo run",
        help="Command used to start the bot",
    )
    parser.add_argument(
        "--base-url",
        default="http://127.0.0.1:8000",
        help="Local API base URL expected from the bot",
    )
    return parser.parse_args()


def run_json(cmd: list[str], cwd: Path) -> dict[str, Any]:
    proc = subprocess.run(cmd, cwd=cwd, check=True, capture_output=True, text=True)
    lines = [line.strip() for line in proc.stdout.splitlines() if line.strip()]
    if not lines:
        raise RuntimeError(f"command produced no JSON: {' '.join(cmd)}")
    return json.loads(lines[-1])


def run_exact_legs(args: argparse.Namespace, root: Path, baseline_db: Path, candidate_db: Path) -> tuple[dict[str, Any], dict[str, Any]]:
    candidate = json.loads((root / args.candidate_config).resolve().read_text())
    fingerprint = config_fingerprint(candidate)
    if args.expected_candidate_fingerprint and fingerprint != args.expected_candidate_fingerprint:
        raise ValueError("selected candidate configuration changed before the A/B cycle")
    baseline = baseline_from_candidate(candidate, args.candidate_mode)
    with tempfile.TemporaryDirectory(prefix="macbot_exact_ab_") as directory:
        config_paths = []
        for name, cfg in (("baseline", baseline), ("candidate", candidate)):
            path = Path(directory) / f"{name}.json"
            with path.open("x", encoding="utf-8") as stream:
                os.chmod(path, 0o600)
                stream.write(json.dumps(cfg, allow_nan=False))
            config_paths.append(path)
        common = [sys.executable, "scripts/run_maker_soak.py", "--exact-config",
                  "--duration-minutes", str(args.duration_minutes), "--report-hours", str(args.report_hours),
                  "--cargo-cmd", args.cargo_cmd, "--base-url", args.base_url]
        baseline_result = run_json(common + ["--config", str(config_paths[0]), "--db", str(baseline_db)], root)
        time.sleep(1.0)
        candidate_result = run_json(common + ["--config", str(config_paths[1]), "--db", str(candidate_db)], root)
        if (baseline_result.get("config_fingerprint") != config_fingerprint(baseline)
                or candidate_result.get("config_fingerprint") != fingerprint):
            raise ValueError("A/B runner did not execute the selected configurations")
        return baseline_result, candidate_result


def main() -> int:
    args = parse_args()
    root = Path(__file__).resolve().parent.parent
    outdir = (root / args.outdir).resolve()
    dbdir = (root / args.dbdir).resolve()
    outdir.mkdir(parents=True, exist_ok=True)
    dbdir.mkdir(parents=True, exist_ok=True)

    stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S") + "_" + uuid.uuid4().hex[:12]
    baseline_db = dbdir / f"{stamp}_baseline_cycle.db"
    guarded_db = dbdir / f"{stamp}_guarded_cycle.db"

    common = [
        sys.executable,
        "scripts/run_maker_soak.py",
        "--config",
        args.config,
        "--duration-minutes",
        str(args.duration_minutes),
        "--report-hours",
        str(args.report_hours),
        "--session-allowlist",
        args.session_allowlist,
        "--blocked-hours-utc",
        args.blocked_hours_utc,
        "--min-confidence-margin",
        str(args.min_confidence_margin),
        "--cargo-cmd",
        args.cargo_cmd,
        "--base-url",
        args.base_url,
    ]
    if args.enable_macro_guard:
        common.append("--enable-macro-guard")

    baseline_cmd = common + ["--db", str(baseline_db), "--maker-backwardation-guard-pct", str(args.maker_backwardation_guard_pct)]
    if args.candidate_mode == "roll_guard":
        baseline_cmd.extend(["--maker-roll-toxic-min-confidence", str(args.maker_roll_toxic_min_confidence),
                             "--maker-roll-toxic-min-signal", str(args.maker_roll_toxic_min_signal),
                             "--maker-roll-toxic-min-sample-count", str(args.maker_roll_toxic_min_sample_count)])
    candidate_cmd = common + ["--db", str(guarded_db), "--maker-backwardation-guard-pct", str(args.maker_backwardation_guard_pct)]
    if args.candidate_mode == "guard":
        candidate_cmd.extend(
            [
                "--maker-backwardation-guard-enabled",
                "--maker-backwardation-guard-live",
            ]
        )
    elif args.candidate_mode == "widen":
        candidate_cmd.extend(
            [
                "--maker-backwardation-widen-bps",
                str(args.maker_backwardation_widen_bps),
                "--maker-backwardation-min-target-spread-bps",
                str(args.maker_backwardation_min_target_spread_bps),
            ]
        )
    else:
        candidate_cmd.extend(
            [
                "--maker-roll-toxic-guard-enabled",
                "--maker-roll-toxic-guard-live",
                "--maker-roll-toxic-min-confidence",
                str(args.maker_roll_toxic_min_confidence),
                "--maker-roll-toxic-min-signal",
                str(args.maker_roll_toxic_min_signal),
                "--maker-roll-toxic-min-sample-count",
                str(args.maker_roll_toxic_min_sample_count),
            ]
        )

    if args.candidate_config:
        baseline, candidate = run_exact_legs(args, root, baseline_db, guarded_db)
    else:
        baseline = run_json(baseline_cmd, root)
        time.sleep(1.0)
        candidate = run_json(candidate_cmd, root)
    compare = run_json(
        [
            sys.executable,
            "scripts/maker_ab_compare.py",
            "--baseline",
            baseline["json"],
            "--candidate",
            candidate["json"],
            "--candidate-mode",
            args.candidate_mode,
        ],
        root,
    )

    cycle = {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "baseline": baseline,
        "candidate": candidate,
        "compare": compare,
        "baseline_db": str(baseline_db),
        "candidate_db": str(guarded_db),
        "args": {
            "duration_minutes": args.duration_minutes,
            "report_hours": args.report_hours,
            "enable_macro_guard": args.enable_macro_guard,
            "session_allowlist": args.session_allowlist,
            "blocked_hours_utc": args.blocked_hours_utc,
            "min_confidence_margin": args.min_confidence_margin,
            "candidate_mode": args.candidate_mode,
            "maker_backwardation_guard_pct": args.maker_backwardation_guard_pct,
            "maker_backwardation_widen_bps": args.maker_backwardation_widen_bps,
            "maker_backwardation_min_target_spread_bps": args.maker_backwardation_min_target_spread_bps,
            "maker_roll_toxic_min_confidence": args.maker_roll_toxic_min_confidence,
            "maker_roll_toxic_min_signal": args.maker_roll_toxic_min_signal,
            "maker_roll_toxic_min_sample_count": args.maker_roll_toxic_min_sample_count,
        },
    }

    cycle_path = outdir / f"{stamp}_maker_ab_cycle.json"
    cycle_path.write_text(json.dumps(cycle, indent=2), encoding="utf-8")
    print(json.dumps({"cycle": str(cycle_path), "compare": compare["json"], "verdict": compare["verdict"]}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
