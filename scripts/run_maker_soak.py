#!/usr/bin/env python3
"""Run a repeatable maker paper-soak with a temporary config override."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import uuid
import os
import shlex
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

from api_auth import get_json, open_api_request
from http_limits import read_response


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Boot a maker paper-soak and emit a maker-soak report")
    parser.add_argument("--config", default="config.json", help="Base config JSON path")
    parser.add_argument("--exact-config", action="store_true", help="Use the supplied frozen paper config without strategy overrides")
    parser.add_argument("--db", default=None, help="Fresh SQLite DB path; defaults to an isolated artifact")
    parser.add_argument(
        "--duration-minutes",
        type=float,
        default=30.0,
        help="How long to leave the bot running before generating a soak report",
    )
    parser.add_argument(
        "--base-url",
        default="http://127.0.0.1:8000",
        help="Local API base URL expected from the bot",
    )
    parser.add_argument(
        "--cargo-cmd",
        default="cargo run",
        help="Command used to start the bot",
    )
    parser.add_argument(
        "--enable-macro-guard",
        action="store_true",
        help="Enable the macro-event guard in the temporary config",
    )
    parser.add_argument(
        "--regime-allowlist",
        default="TREND_UP",
        help="Comma-separated runtime regime allowlist for the temporary config",
    )
    parser.add_argument(
        "--session-allowlist",
        default="ASIA,US",
        help="Comma-separated runtime session allowlist (asia,europe,us)",
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
        "--report-hours",
        type=float,
        default=None,
        help="Telemetry lookback window for the final maker soak report",
    )
    parser.add_argument(
        "--maker-backwardation-guard-enabled",
        action="store_true",
        help="Enable the maker backwardation guard in the temporary config",
    )
    parser.add_argument(
        "--maker-backwardation-guard-live",
        action="store_true",
        help="Run the maker backwardation guard as a live suppression rule instead of shadow-only",
    )
    parser.add_argument(
        "--maker-backwardation-guard-pct",
        type=float,
        default=-0.02,
        help="Backwardation threshold (pct) that triggers the maker guard",
    )
    parser.add_argument(
        "--maker-backwardation-widen-bps",
        type=float,
        default=0.0,
        help="Additional spread widening in bps applied when backwardation breaches the guard threshold",
    )
    parser.add_argument(
        "--maker-backwardation-min-target-spread-bps",
        type=float,
        default=0.0,
        help="Minimum target spread in bps enforced when backwardation widening is active",
    )
    parser.add_argument(
        "--maker-roll-toxic-guard-enabled",
        action="store_true",
        help="Enable the side-aware Roll toxicity guard in the temporary config",
    )
    parser.add_argument(
        "--maker-roll-toxic-guard-live",
        action="store_true",
        help="Run the Roll toxicity guard as a live suppression rule instead of shadow-only",
    )
    parser.add_argument(
        "--maker-roll-toxic-min-confidence",
        type=float,
        default=0.15,
        help="Minimum Roll confidence required before the toxicity guard engages",
    )
    parser.add_argument(
        "--maker-roll-toxic-min-signal",
        type=float,
        default=0.05,
        help="Minimum absolute Roll signal required before the toxicity guard engages",
    )
    parser.add_argument(
        "--maker-roll-toxic-min-sample-count",
        type=int,
        default=6,
        help="Minimum Roll sample count required before the toxicity guard engages",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the generated config patch and command without starting the bot",
    )
    return parser.parse_args()


def load_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def patch_config(cfg: dict[str, Any], args: argparse.Namespace) -> dict[str, Any]:
    patched = dict(cfg)
    regimes = [item.strip().upper() for item in args.regime_allowlist.split(",") if item.strip()]
    sessions = [item.strip().lower() for item in args.session_allowlist.split(",") if item.strip()]
    blocked_hours = []
    for item in args.blocked_hours_utc.split(","):
        item = item.strip()
        if not item:
            continue
        try:
            hour = int(item)
        except ValueError as exc:
            raise ValueError("blocked hours must be integers from 0 to 23") from exc
        if not 0 <= hour <= 23:
            raise ValueError("blocked hours must be integers from 0 to 23")
        blocked_hours.append(hour)
    patched.update(
        {
            "mode": "simulation",
            "execution_mode": "maker",
            "auto_trading": True,
            "auto_refresh": True,
            "sim_live_execution": True,
            "shadow_trading": False,
            "time_of_day_weight": False,
            "maker_live_quoting": True,
            "maker_backwardation_guard_enabled": bool(args.maker_backwardation_guard_enabled),
            "maker_backwardation_guard_shadow_only": not bool(args.maker_backwardation_guard_live),
            "maker_backwardation_guard_pct": float(args.maker_backwardation_guard_pct),
            "maker_backwardation_widen_bps": float(args.maker_backwardation_widen_bps),
            "maker_backwardation_min_target_spread_bps": float(
                args.maker_backwardation_min_target_spread_bps
            ),
            "maker_roll_toxic_guard_enabled": bool(args.maker_roll_toxic_guard_enabled),
            "maker_roll_toxic_guard_shadow_only": not bool(args.maker_roll_toxic_guard_live),
            "maker_roll_toxic_min_confidence": float(args.maker_roll_toxic_min_confidence),
            "maker_roll_toxic_min_signal": float(args.maker_roll_toxic_min_signal),
            "maker_roll_toxic_min_sample_count": int(args.maker_roll_toxic_min_sample_count),
            "macro_event_guard_enabled": bool(args.enable_macro_guard),
            "regime_allowlist": regimes,
            "session_allowlist": sessions,
            "blocked_hours_utc": blocked_hours,
            "min_confidence_margin": float(args.min_confidence_margin),
        }
    )
    return patched


def paper_run_environment(cfg: dict[str, Any], config_path: Path, db_path: Path, base_url: str) -> dict[str, str]:
    parsed = urlparse(base_url)
    if (parsed.scheme != "http" or parsed.hostname not in {"localhost", "127.0.0.1"}
            or parsed.username or parsed.password or parsed.path not in {"", "/"}
            or parsed.query or parsed.fragment):
        raise ValueError("paper soak base URL must be a local HTTP origin")
    if str(cfg.get("mode", "")).lower() not in {"sim", "simulation", "shadow"} or cfg.get("execution_mode") != "maker":
        raise ValueError("paper soak requires simulation/shadow maker configuration")
    if db_path.exists():
        raise ValueError("paper soak requires a fresh database; never overwrite or reuse trading history")
    db_path.parent.mkdir(parents=True, exist_ok=True)
    if not 1 <= (parsed.port or 80) <= 65535 or parsed.port == 0:
        raise ValueError("paper soak port must be between 1 and 65535")
    env = os.environ.copy()
    env.update(MACBOT_CONFIG=str(config_path), MACBOT_DB=str(db_path),
               MACBOT_PORT=str(parsed.port or 80), MACBOT_BIND_ALL="0")
    env.pop("MACBOT_BACKTEST_ONLY", None)
    # The frozen file owns this noncredential config value too.
    env.pop("MACBOT_MACRO_EVENT_CALENDAR_PATH", None)
    env.pop("MACBOT_DEPLOYMENT_PROFILE", None)
    env.pop("MACBOT_ROLLOUT_STAGE", None)
    return env


def redacted_config(cfg: dict[str, Any]) -> dict[str, Any]:
    return {key: ("[redacted]" if isinstance(value, str) and value and
                  any(marker in key.lower() for marker in ("key", "secret", "passphrase", "private_rpc", "webhook"))
                  else value) for key, value in cfg.items()}


def noncredential_config(value: Any) -> Any:
    if isinstance(value, dict):
        return {key: noncredential_config(item) for key, item in value.items()
                if not any(marker in key.lower() for marker in ("key", "secret", "passphrase", "private_rpc", "webhook"))}
    if isinstance(value, list):
        return [noncredential_config(item) for item in value]
    return value


def config_fingerprint(cfg: dict[str, Any]) -> str:
    if not isinstance(cfg, dict):
        raise ValueError("paper configuration must be an object")
    canonical = json.dumps(noncredential_config(cfg), sort_keys=True, separators=(",", ":"), allow_nan=False)
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()


def configuration_evidence(cfg: dict[str, Any]) -> dict[str, Any]:
    return {"configuration": noncredential_config(cfg), "config_fingerprint": config_fingerprint(cfg)}


def bound_configuration(report: dict[str, Any]) -> dict[str, Any] | None:
    cfg = report.get("configuration")
    try:
        return cfg if isinstance(cfg, dict) and report.get("config_fingerprint") == config_fingerprint(cfg) else None
    except (ValueError, TypeError):
        return None


def baseline_from_candidate(candidate: dict[str, Any], mode: str) -> dict[str, Any]:
    baseline = dict(candidate)
    if mode == "widen":
        baseline.update(maker_backwardation_widen_bps=0.0, maker_backwardation_min_target_spread_bps=0.0)
    elif mode == "guard":
        baseline.update(maker_backwardation_guard_enabled=False, maker_backwardation_guard_shadow_only=True)
    elif mode == "roll_guard":
        baseline.update(maker_roll_toxic_guard_enabled=False, maker_roll_toxic_guard_shadow_only=True)
    else:
        raise ValueError("unsupported candidate treatment")
    return baseline


def verified_treatment_mode(baseline: dict[str, Any], candidate: dict[str, Any], declared: str | None = None) -> str | None:
    modes = [declared] if declared is not None else ["widen", "guard", "roll_guard"]
    for mode in modes:
        if mode not in {"widen", "guard", "roll_guard"}:
            continue
        if mode == "widen":
            try:
                widen = float(candidate.get("maker_backwardation_widen_bps") or 0)
                floor = float(candidate.get("maker_backwardation_min_target_spread_bps") or 0)
            except (ValueError, TypeError):
                continue
            if not math.isfinite(widen) or not math.isfinite(floor) or max(widen, floor) <= 0:
                continue
        if mode == "guard" and (candidate.get("maker_backwardation_guard_enabled") is not True or candidate.get("maker_backwardation_guard_shadow_only") is not False):
            continue
        if mode == "roll_guard" and (candidate.get("maker_roll_toxic_guard_enabled") is not True or candidate.get("maker_roll_toxic_guard_shadow_only") is not False):
            continue
        if noncredential_config(baseline) == noncredential_config(candidate):
            continue
        if config_fingerprint(baseline) == config_fingerprint(baseline_from_candidate(candidate, mode)):
            return mode
    return None


def bind_report_configuration(result: dict[str, Any], cfg: dict[str, Any]) -> dict[str, Any]:
    evidence = configuration_evidence(cfg)
    path = Path(result["json"])
    report = load_json(path)
    report.update(evidence)
    path.write_text(json.dumps(report, indent=2, allow_nan=False), encoding="utf-8")
    result.update(evidence)
    return result


def assert_configuration_unchanged(path: Path, selected: dict[str, Any]) -> None:
    if config_fingerprint(load_json(path)) != config_fingerprint(selected):
        raise ValueError("paper configuration changed during the run; its evidence is not comparable")


def read_key(cfg: dict[str, Any]) -> str:
    return os.getenv("MACBOT_API_KEY", "").strip() or str(cfg.get("api_key") or "").strip()


def wait_ready(base_url: str, timeout_secs: float = 60.0, proc: subprocess.Popen[str] | None = None) -> None:
    deadline = time.monotonic() + timeout_secs
    last_error: str | None = None
    while time.monotonic() < deadline:
        if proc is not None and proc.poll() is not None:
            raise RuntimeError(f"maker soak process exited before health: {proc.returncode}")
        try:
            req = urllib.request.Request(f"{base_url}/api/health")
            with open_api_request(req, timeout=3) as resp:
                payload = json.loads(read_response(resp, label="Health response").decode("utf-8"))
            if payload.get("status") == "ok":
                return
            last_error = f"unexpected health payload: {payload}"
        except (urllib.error.URLError, TimeoutError, ValueError) as exc:
            last_error = str(exc)
        time.sleep(1.0)
    raise RuntimeError(f"maker soak bot did not become healthy: {last_error or 'timeout'}")


def fetch_json(url: str, timeout_secs: float = 5.0, api_key: str | None = None) -> dict[str, Any]:
    return get_json(url, timeout=timeout_secs, api_key=api_key, read_auth=True)


def terminate_process(proc: subprocess.Popen[str]) -> None:
    if proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


def run_report(
    db_path: Path,
    report_hours: float,
    maker_state_snapshot_path: Path | None = None,
    no_arb_snapshot_path: Path | None = None,
) -> dict[str, Any]:
    cmd = [
        sys.executable,
        "scripts/maker_soak_report.py",
        "--db",
        str(db_path),
        "--hours",
        f"{report_hours:.6f}",
    ]
    if maker_state_snapshot_path is not None:
        cmd.extend(["--maker-state-snapshot", str(maker_state_snapshot_path)])
    if no_arb_snapshot_path is not None:
        cmd.extend(["--no-arb-snapshot", str(no_arb_snapshot_path)])
    proc = subprocess.run(
        cmd,
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(proc.stdout.strip())


def main() -> int:
    args = parse_args()
    args.base_url = args.base_url.rstrip("/")
    config_path = Path(args.config).expanduser().resolve()
    root = Path(__file__).resolve().parent.parent
    db_path = Path(args.db).expanduser().resolve() if args.db else root / "artifacts/maker-soak-dbs" / f"paper-{uuid.uuid4().hex}.sqlite"
    if not math.isfinite(args.duration_minutes) or args.duration_minutes < 0.0:
        raise ValueError("duration must be finite and nonnegative")
    base_cfg = load_json(config_path)
    patched_cfg = dict(base_cfg) if args.exact_config else patch_config(base_cfg, args)
    configuration_evidence(patched_cfg)  # Reject invalid numeric values before launching.

    cargo_cmd = shlex.split(args.cargo_cmd)
    if not cargo_cmd:
        raise SystemExit("empty --cargo-cmd")
    report_hours = (
        args.report_hours
        if args.report_hours is not None
        else max(0.25, (max(0.0, args.duration_minutes) / 60.0) + 0.10)
    )

    if not math.isfinite(report_hours) or report_hours <= 0:
        raise ValueError("report hours must be finite and positive")

    if args.dry_run:
        print(
            json.dumps(
                {
                    "config_path": str(config_path),
                    "db_path": str(db_path),
                    "base_url": args.base_url,
                    "cargo_cmd": cargo_cmd,
                    "report_hours": report_hours,
                    "patched_config": redacted_config(patched_cfg),
                    "config_fingerprint": config_fingerprint(patched_cfg),
                },
                indent=2,
            )
        )
        return 0

    with tempfile.TemporaryDirectory(prefix="macbot_maker_soak_") as tmpdir:
        temp_cfg_path = Path(tmpdir) / "maker_soak_config.json"
        maker_state_snapshot_path = Path(tmpdir) / "maker_state_snapshot.json"
        no_arb_snapshot_path = Path(tmpdir) / "no_arb_snapshot.json"
        with temp_cfg_path.open("x", encoding="utf-8") as config_file:
            os.chmod(temp_cfg_path, 0o600)
            config_file.write(json.dumps(patched_cfg, indent=2))

        env = paper_run_environment(patched_cfg, temp_cfg_path, db_path, args.base_url)
        from run_frozen_maker_soak import preflight_bind_conflict
        preflight_bind_conflict(args.base_url)

        proc = subprocess.Popen(cargo_cmd, cwd=root, env=env)
        try:
            wait_ready(args.base_url, proc=proc)
            if not math.isfinite(args.duration_minutes) or args.duration_minutes < 0.0:
                raise ValueError("duration must be finite and nonnegative")
            deadline = time.monotonic() + args.duration_minutes * 60.0
            while time.monotonic() < deadline:
                if proc.poll() is not None:
                    raise RuntimeError(f"maker soak process exited early: {proc.returncode}")
                time.sleep(min(1.0, max(0.0, deadline - time.monotonic())))
            try:
                maker_state_snapshot_path.write_text(
                    json.dumps(fetch_json(f"{args.base_url}/api/maker/state", api_key=read_key(patched_cfg)), indent=2),
                    encoding="utf-8",
                )
            except Exception:
                maker_state_snapshot_path = None
            try:
                no_arb_snapshot_path.write_text(
                    json.dumps(fetch_json(f"{args.base_url}/api/markets/no-arb", api_key=read_key(patched_cfg)), indent=2),
                    encoding="utf-8",
                )
            except Exception:
                no_arb_snapshot_path = None
        finally:
            terminate_process(proc)

        assert_configuration_unchanged(temp_cfg_path, patched_cfg)
        report = run_report(db_path, report_hours, maker_state_snapshot_path, no_arb_snapshot_path)
        report = bind_report_configuration(report, patched_cfg)
    print(json.dumps(report))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
