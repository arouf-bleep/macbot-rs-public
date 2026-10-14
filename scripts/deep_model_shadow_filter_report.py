#!/usr/bin/env python3
"""Evaluate a deep-model artifact as a shadow filter on an exact-input lane.

This report works directly off the canonical `ml_model_inputs` archive instead of
the Rust candle-only backtest path. It is meant for shadow validation of a
specialist lane such as TREND_UP + BUY_YES, with optional hour/session and
baseline-confidence filters that mirror the specialist rail.
"""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

import numpy as np
from sklearn.metrics import brier_score_loss, roc_auc_score


ROOT = Path(__file__).resolve().parents[1]
TRAINING_DIR = ROOT / "training"
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))
if str(TRAINING_DIR) not in sys.path:
    sys.path.insert(0, str(TRAINING_DIR))

import onnxruntime as ort  # noqa: E402
from alpha_presets import PRESETS  # noqa: E402
from training.train import BTCDataset  # noqa: E402


DAY_MS = 86_400_000


def csv_strings(raw: str) -> list[str]:
    return [part.strip() for part in raw.split(",") if part.strip()]


def csv_upper_strings(raw: str) -> list[str]:
    return [part.strip().upper() for part in raw.split(",") if part.strip()]


def csv_hours(raw: str) -> list[int]:
    hours: list[int] = []
    for part in raw.split(","):
        s = part.strip()
        if not s:
            continue
        hour = int(s)
        if hour < 0 or hour > 23:
            raise ValueError(f"invalid hour '{s}', expected 0-23")
        hours.append(hour)
    return hours


def first_threshold(raw: Any, fallback: float) -> float:
    if raw is None:
        return fallback
    if isinstance(raw, (int, float)):
        return float(raw)
    if isinstance(raw, str):
        for part in raw.split(","):
            s = part.strip()
            if s:
                return float(s)
    return fallback


def signal_from_side(side: str) -> str:
    side = side.strip().lower()
    if side == "yes_only":
        return "BUY_YES"
    if side == "no_only":
        return "BUY_NO"
    raise ValueError(f"unsupported side '{side}' for shadow lane report")


def implied_signal_from_score(score: float, entry_threshold: float) -> str:
    if score >= entry_threshold:
        return "BUY_YES"
    if score <= -entry_threshold:
        return "BUY_NO"
    return "NEUTRAL"


def infer_lane_signal(preset_values: dict[str, Any]) -> str:
    regime_side_modes = str(preset_values.get("regime_side_modes", "") or "").strip()
    if regime_side_modes:
        modes = [part.strip() for part in regime_side_modes.split(",") if part.strip()]
        unique = {signal_from_side(item.split(":", 1)[1]) for item in modes if ":" in item}
        if len(unique) == 1:
            return next(iter(unique))
    sides = str(preset_values.get("sides", "") or "").strip().lower()
    if sides in {"yes_only", "no_only"}:
        return signal_from_side(sides)
    raise ValueError(
        "could not infer a single lane signal from preset; pass --lane-signals explicitly"
    )


def apply_shadow_preset(args: argparse.Namespace) -> None:
    if not args.preset:
        return
    values = PRESETS.get(args.preset.strip().lower())
    if values is None:
        valid = ", ".join(sorted(PRESETS))
        raise ValueError(f"unknown preset '{args.preset}', expected one of: {valid}")

    if args.entry_threshold == 0.38:
        args.entry_threshold = first_threshold(values.get("entry_thresholds"), args.entry_threshold)
    if args.min_confidence_margin == 0.0 and "min_confidence_margin" in values:
        args.min_confidence_margin = float(values["min_confidence_margin"])
    if not args.session_allowlist and values.get("session_allowlist"):
        args.session_allowlist = str(values["session_allowlist"])
    if not args.blocked_hours_utc and values.get("blocked_hours_utc"):
        args.blocked_hours_utc = str(values["blocked_hours_utc"])
    if not args.lane_regimes and values.get("regime_allowlist"):
        args.lane_regimes = str(values["regime_allowlist"])
    if not args.lane_signals:
        args.lane_signals = infer_lane_signal(values)


def session_bucket(hour: int) -> str:
    if 0 <= hour <= 7:
        return "asia"
    if 8 <= hour <= 15:
        return "europe"
    return "us"


def regime_matches(meta_regime: str, requested_regimes: set[str]) -> bool:
    if not requested_regimes:
        return True
    meta_regime = meta_regime.upper()
    if meta_regime in requested_regimes:
        return True
    return False


def compute_metrics(labels: np.ndarray, preds: np.ndarray) -> dict[str, float | int | None]:
    if len(labels) == 0:
        return {
            "samples": 0,
            "base_rate": None,
            "avg_pred": None,
            "brier": None,
            "auc": None,
            "accuracy": None,
        }
    labels = labels.astype(np.float32)
    preds = preds.astype(np.float32)
    out: dict[str, float | int | None] = {
        "samples": int(len(labels)),
        "base_rate": float(labels.mean()),
        "avg_pred": float(preds.mean()),
        "brier": float(brier_score_loss(labels, preds)),
        "accuracy": float(np.mean((preds >= 0.5) == (labels >= 0.5))),
    }
    try:
        out["auc"] = float(roc_auc_score(labels, preds)) if len(np.unique(labels)) > 1 else None
    except ValueError:
        out["auc"] = None
    return out


def summarize_directional_rows(rows: list[dict[str, Any]]) -> dict[str, Any]:
    if not rows:
        return {
            "trades": 0,
            "wins": 0,
            "win_rate": 0.0,
            "avg_model_confidence": 0.0,
            "avg_baseline_confidence_margin": 0.0,
            "avg_fusion_score": 0.0,
            "metrics": compute_metrics(np.empty((0,), dtype=np.float32), np.empty((0,), dtype=np.float32)),
        }

    labels = np.array([float(row["directional_label"]) for row in rows], dtype=np.float32)
    preds = np.array([float(row["directional_confidence"]) for row in rows], dtype=np.float32)
    wins = int(labels.sum())
    return {
        "trades": len(rows),
        "wins": wins,
        "win_rate": float(labels.mean() * 100.0),
        "avg_model_confidence": float(preds.mean()),
        "avg_baseline_confidence_margin": float(
            np.mean([float(row["baseline_confidence_margin"]) for row in rows])
        ),
        "avg_fusion_score": float(np.mean([float(row["fusion_score"]) for row in rows])),
        "metrics": compute_metrics(labels, preds),
    }


def bucket_rows(rows: list[dict[str, Any]], key: str) -> list[dict[str, Any]]:
    buckets: dict[str, list[dict[str, Any]]] = {}
    for row in rows:
        label = str(row.get(key, "unknown"))
        buckets.setdefault(label, []).append(row)
    out: list[dict[str, Any]] = []
    for label, members in buckets.items():
        summary = summarize_directional_rows(members)
        out.append(
            {
                "bucket": label,
                "trades": summary["trades"],
                "win_rate": summary["win_rate"],
                "avg_model_confidence": summary["avg_model_confidence"],
                "avg_baseline_confidence_margin": summary["avg_baseline_confidence_margin"],
            }
        )
    out.sort(key=lambda row: (row["win_rate"], row["trades"]), reverse=True)
    return out


def load_predictions(dataset: BTCDataset, model_path: str, head: str, batch_size: int) -> np.ndarray:
    if batch_size <= 0:
        raise ValueError("batch size must be positive")
    ort.disable_telemetry_events()
    session = ort.InferenceSession(model_path, providers=["CPUExecutionProvider"])
    input_names = {inp.name for inp in session.get_inputs()}
    output_names = [out.name for out in session.get_outputs()]
    if head not in output_names:
        raise SystemExit(f"requested head '{head}' not in ONNX outputs {output_names}")

    preds: list[np.ndarray] = []
    for start in range(0, len(dataset.samples), batch_size):
        batch = dataset.samples[start : start + batch_size]

        def stack(key: str) -> np.ndarray:
            return np.stack([sample[key] for sample in batch]).astype(np.float32)

        feed: dict[str, np.ndarray] = {}
        if "candles_1m" in input_names:
            feed["candles_1m"] = stack("candles_1m")
        if "candles_5m" in input_names:
            feed["candles_5m"] = stack("candles_5m")
        if "candles_15m" in input_names:
            feed["candles_15m"] = stack("candles_15m")
        if "orderbook_seq" in input_names:
            feed["orderbook_seq"] = stack("orderbook_seq")
        if "scalars" in input_names:
            feed["scalars"] = stack("scalars")
        if "staleness" in input_names:
            feed["staleness"] = stack("staleness")
        if "regime" in input_names:
            feed["regime"] = stack("regime")

        outputs = session.run(None, feed)
        out_map = dict(zip(output_names, outputs))
        preds.append(np.asarray(out_map[head], dtype=np.float32).reshape(-1))

    result = np.concatenate(preds) if preds else np.empty((0,), dtype=np.float32)
    if len(result) != len(dataset.samples) or not np.all(np.isfinite(result)) or not np.all((result >= 0) & (result <= 1)):
        raise ValueError("model did not return one finite probability per exact input")
    return result


@dataclass
class WindowConfig:
    days: int
    shift_days: int


def window_rows(
    dataset: BTCDataset,
    preds: np.ndarray,
    *,
    window: WindowConfig,
    lane_regimes: set[str],
    lane_signals: set[str],
    entry_threshold: float,
    min_confidence_margin: float,
    session_allowlist: set[str],
    blocked_hours: set[int],
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    timestamps = [
        datetime.fromisoformat(str(meta["timestamp"]).replace("Z", "+00:00")).astimezone(timezone.utc)
        for meta in dataset.meta
    ]
    anchor_end = max(timestamps) - timedelta(days=window.shift_days)
    anchor_start = anchor_end - timedelta(days=window.days)

    rows: list[dict[str, Any]] = []
    skipped: dict[str, int] = {}

    def inc(reason: str) -> None:
        skipped[reason] = skipped.get(reason, 0) + 1

    for idx, (sample, meta, pred, ts) in enumerate(zip(dataset.samples, dataset.meta, preds, timestamps)):
        if ts <= anchor_start or ts > anchor_end:
            continue
        regime = str(meta.get("regime_label", "")).upper()
        stored_signal = str(meta.get("signal", "")).upper()
        if not regime_matches(regime, lane_regimes):
            inc("regime_filter")
            continue
        hour = ts.hour
        session = session_bucket(hour)
        if session_allowlist and session not in session_allowlist:
            inc("session_allowlist")
            continue
        if hour in blocked_hours:
            inc("blocked_hour_utc")
            continue

        fusion_score = float(meta.get("fusion_score") or 0.0)
        baseline_margin = max(abs(fusion_score) - entry_threshold, 0.0)
        signal = (
            stored_signal
            if stored_signal not in {"", "NEUTRAL", "HOLD"}
            else implied_signal_from_score(fusion_score, entry_threshold)
        )
        if lane_signals and signal not in lane_signals:
            inc("signal_filter")
            continue
        if baseline_margin < min_confidence_margin:
            inc("baseline_confidence_margin")
            continue

        if signal == "BUY_YES":
            directional_label = float(sample["label_p_up"])
            directional_confidence = float(pred)
        elif signal == "BUY_NO":
            directional_label = 1.0 - float(sample["label_p_up"])
            directional_confidence = 1.0 - float(pred)
        else:
            inc("unsupported_signal")
            continue

        rows.append(
            {
                "index": idx,
                "timestamp": ts.isoformat(),
                "market_id": meta.get("market_id"),
                "regime": regime,
                "signal": signal,
                "stored_signal": stored_signal,
                "entry_hour_utc": hour,
                "entry_session_utc": session,
                "fusion_score": fusion_score,
                "baseline_confidence_margin": baseline_margin,
                "directional_label": directional_label,
                "directional_confidence": directional_confidence,
                "archived_p_up": meta.get("archived_p_up"),
            }
        )

    meta = {
        "anchor_start_utc": anchor_start.isoformat(),
        "anchor_end_utc": anchor_end.isoformat(),
        "skipped": skipped,
    }
    return rows, meta


def build_report(
    *,
    dataset: BTCDataset,
    preds: np.ndarray,
    days_list: list[int],
    shift_days_list: list[int],
    threshold: float,
    lane_regimes: set[str],
    lane_signals: set[str],
    entry_threshold: float,
    min_confidence_margin: float,
    session_allowlist: set[str],
    blocked_hours: set[int],
) -> dict[str, Any]:
    if not dataset.meta or len(dataset.samples) != len(preds):
        raise ValueError("nonempty aligned exact archive predictions are required")
    if any(meta.get("source_kind") != "exact_inputs" for meta in dataset.meta):
        raise ValueError("exact shadow evidence cannot include reconstructed or mixed-provenance inputs")
    if not days_list or any(day <= 0 for day in days_list) or not shift_days_list or any(shift < 0 for shift in shift_days_list):
        raise ValueError("days must be positive; shifts must be nonempty and nonnegative")
    if not 0 <= threshold <= 1:
        raise ValueError("threshold must be a finite probability")
    window_reports: list[dict[str, Any]] = []
    improved = 0
    evaluated = 0
    for days in days_list:
        for shift_days in shift_days_list:
            rows, window_meta = window_rows(
                dataset,
                preds,
                window=WindowConfig(days=days, shift_days=shift_days),
                lane_regimes=lane_regimes,
                lane_signals=lane_signals,
                entry_threshold=entry_threshold,
                min_confidence_margin=min_confidence_margin,
                session_allowlist=session_allowlist,
                blocked_hours=blocked_hours,
            )
            baseline = summarize_directional_rows(rows)
            filtered_rows = [
                row for row in rows if float(row["directional_confidence"]) >= threshold
            ]
            filtered = summarize_directional_rows(filtered_rows)

            win_rate_lift_pp = filtered["win_rate"] - baseline["win_rate"] if baseline["trades"] else 0.0
            coverage = filtered["trades"] / baseline["trades"] if baseline["trades"] else 0.0
            improved_now = filtered["trades"] > 0 and win_rate_lift_pp > 0.0
            if baseline["trades"] > 0:
                evaluated += 1
                if improved_now:
                    improved += 1

            window_reports.append(
                {
                    "days": days,
                    "shift_days": shift_days,
                    "window": window_meta,
                    "baseline": baseline,
                    "filtered": filtered,
                    "coverage": coverage,
                    "win_rate_lift_pp": win_rate_lift_pp,
                    "improved": improved_now,
                    "by_hour_baseline": bucket_rows(rows, "entry_hour_utc"),
                    "by_hour_filtered": bucket_rows(filtered_rows, "entry_hour_utc"),
                    "by_session_baseline": bucket_rows(rows, "entry_session_utc"),
                    "by_session_filtered": bucket_rows(filtered_rows, "entry_session_utc"),
                    "best_filtered_rows": sorted(
                        filtered_rows,
                        key=lambda row: (row["directional_confidence"], row["baseline_confidence_margin"]),
                        reverse=True,
                    )[:5],
                    "worst_baseline_rows": sorted(
                        rows,
                        key=lambda row: (row["directional_label"], row["directional_confidence"]),
                    )[:5],
                }
            )

    current = next(
        (
            row
            for row in window_reports
            if row["days"] == min(days_list) and row["shift_days"] == min(shift_days_list)
        ),
        None,
    )
    return {
        "window_reports": window_reports,
        "overall": {
            "evaluated_windows": evaluated,
            "improved_windows": improved,
            "improved_fraction": (improved / evaluated) if evaluated else 0.0,
            "current_window_improved": bool(current and current["improved"]),
            "current_window_lift_pp": float(current["win_rate_lift_pp"]) if current else 0.0,
            "current_window_coverage": float(current["coverage"]) if current else 0.0,
        },
    }


def write_outputs(report: dict[str, Any], out_dir: Path, stem: str) -> tuple[Path, Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = out_dir / f"{ts}_{stem}.json"
    md_path = out_dir / f"{ts}_{stem}.md"
    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")

    overall = report["overall"]
    lines = [
        "# Deep Model Shadow Filter Report",
        "",
        f"- generated_at_utc: {report['generated_at_utc']}",
        f"- db: {report['db']}",
        f"- model: {report['model']}",
        f"- head: {report['head']}",
        f"- threshold: {report['threshold']:.3f}",
        f"- preset: {report['preset'] or 'none'}",
        f"- lane_regimes: {', '.join(report['lane_regimes']) or 'all'}",
        f"- lane_signals: {', '.join(report['lane_signals']) or 'all'}",
        "",
        "## Overall",
        "",
        f"- evaluated_windows: {overall['evaluated_windows']}",
        f"- improved_windows: {overall['improved_windows']}",
        f"- improved_fraction: {overall['improved_fraction']:.2%}",
        f"- current_window_improved: {overall['current_window_improved']}",
        f"- current_window_lift_pp: {overall['current_window_lift_pp']:.2f}",
        f"- current_window_coverage: {overall['current_window_coverage']:.2%}",
        "",
        "## Window Summary",
        "",
    ]
    for row in report["window_reports"]:
        lines.append(
            f"- {row['days']}d shift={row['shift_days']}d: "
            f"baseline={row['baseline']['trades']} filtered={row['filtered']['trades']} "
            f"lift={row['win_rate_lift_pp']:.2f}pp coverage={row['coverage']:.2%} improved={row['improved']}"
        )
    md_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return json_path, md_path


def main() -> int:
    parser = argparse.ArgumentParser(description="Shadow-filter report on the exact-input archive")
    parser.add_argument("--db", required=True, help="SQLite DB containing labeled ml_model_inputs")
    parser.add_argument("--model", required=True, help="ONNX model artifact")
    parser.add_argument("--head", choices=("p_up", "calibration"), default="p_up")
    parser.add_argument("--preset", default="")
    parser.add_argument("--threshold", type=float, default=0.56)
    parser.add_argument("--days-list", default="7,14,21,30")
    parser.add_argument("--shift-days-list", default="0,7,14,21")
    parser.add_argument("--entry-threshold", type=float, default=0.38)
    parser.add_argument("--min-confidence-margin", type=float, default=0.0)
    parser.add_argument("--session-allowlist", default="")
    parser.add_argument("--blocked-hours-utc", default="")
    parser.add_argument("--lane-regimes", default="")
    parser.add_argument("--lane-signals", default="")
    parser.add_argument("--batch-size", type=int, default=256)
    parser.add_argument("--output-dir", default="artifacts/alpha-reports")
    parser.add_argument("--report-label", default="deep_model_shadow_filter_report")
    args = parser.parse_args()

    apply_shadow_preset(args)
    days_list = [int(part) for part in csv_strings(args.days_list)]
    shift_days_list = [int(part) for part in csv_strings(args.shift_days_list)]
    lane_regimes = set(csv_upper_strings(args.lane_regimes))
    lane_signals = set(csv_upper_strings(args.lane_signals))
    session_allowlist = {part.lower() for part in csv_strings(args.session_allowlist)}
    blocked_hours = set(csv_hours(args.blocked_hours_utc))

    dataset = BTCDataset(args.db, source="exact_inputs")
    preds = load_predictions(dataset, args.model, args.head, args.batch_size)
    report = {
        "methodology": "archived prediction rows, not executed trades; windows may overlap and share outcomes; model training isolation is not verified",
        "generated_at_utc": datetime.now(timezone.utc).isoformat(),
        "db": str(Path(args.db)),
        "model": str(Path(args.model)),
        "head": args.head,
        "preset": args.preset,
        "threshold": float(args.threshold),
        "entry_threshold": float(args.entry_threshold),
        "min_confidence_margin": float(args.min_confidence_margin),
        "session_allowlist": sorted(session_allowlist),
        "blocked_hours_utc": sorted(blocked_hours),
        "lane_regimes": sorted(lane_regimes),
        "lane_signals": sorted(lane_signals),
    }
    report.update(
        build_report(
            dataset=dataset,
            preds=preds,
            days_list=days_list,
            shift_days_list=shift_days_list,
            threshold=args.threshold,
            lane_regimes=lane_regimes,
            lane_signals=lane_signals,
            entry_threshold=args.entry_threshold,
            min_confidence_margin=args.min_confidence_margin,
            session_allowlist=session_allowlist,
            blocked_hours=blocked_hours,
        )
    )

    out_dir = Path(args.output_dir)
    if not out_dir.is_absolute():
        out_dir = (ROOT / out_dir).resolve()
    json_path, md_path = write_outputs(report, out_dir, args.report_label)
    report["artifacts"] = {
        "json": str(json_path),
        "markdown": str(md_path),
    }
    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps(report, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
