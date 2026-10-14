#!/usr/bin/env python3
"""Compare two maker-soak reports and emit a compact A/B verdict."""

from __future__ import annotations

import argparse
import json
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from run_maker_soak import bound_configuration, config_fingerprint, verified_treatment_mode


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Compare baseline and candidate maker-soak reports")
    parser.add_argument("--baseline", required=True, help="Baseline maker-soak JSON report path")
    parser.add_argument("--candidate", required=True, help="Candidate maker-soak JSON report path")
    parser.add_argument("--candidate-mode", choices=["widen", "guard", "roll_guard"], help="Require only this declared treatment to differ")
    parser.add_argument(
        "--outdir",
        default="artifacts/maker-soak-reports",
        help="Directory for JSON/Markdown comparison artifacts",
    )
    return parser.parse_args()


def load_report(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def dominant_bucket(items: dict[str, dict[str, Any]]) -> str | None:
    if not items:
        return None
    best_key, _best_value = max(items.items(), key=lambda item: item[1].get("count", 0))
    return best_key


def pct_change(base: float | int | None, cand: float | int | None) -> float | None:
    if base in (None, 0) or cand is None:
        return None
    return ((float(cand) / float(base)) - 1.0) * 100.0


def diff(base: float | int | None, cand: float | int | None) -> float | None:
    if base is None or cand is None:
        return None
    return float(cand) - float(base)


def compare_reports(
    baseline: dict[str, Any],
    candidate: dict[str, Any],
    baseline_path: Path,
    candidate_path: Path,
    candidate_mode: str | None = None,
) -> dict[str, Any]:
    baseline_summary = baseline.get("summary", {})
    candidate_summary = candidate.get("summary", {})

    baseline_session = dominant_bucket(baseline.get("session_buckets", {}))
    candidate_session = dominant_bucket(candidate.get("session_buckets", {}))
    baseline_hour = dominant_bucket(baseline.get("hour_buckets", {}))
    candidate_hour = dominant_bucket(candidate.get("hour_buckets", {}))
    baseline_places = baseline.get("place_event_count", 0) or 0
    candidate_places = candidate.get("place_event_count", 0) or 0
    candidate_guard_skips = candidate.get("guard_skip_event_count", 0) or 0
    baseline_config = bound_configuration(baseline)
    candidate_config = bound_configuration(candidate)
    treatment_mode = (verified_treatment_mode(baseline_config, candidate_config, candidate_mode)
                      if baseline_config is not None and candidate_config is not None else None)
    configuration_bound = treatment_mode is not None

    sample_adequate = (
        baseline.get("cancel_event_count", 0) >= 200 and candidate.get("cancel_event_count", 0) >= 200
    )
    comparable_window = (baseline_session is not None and baseline_hour is not None
                         and candidate_session is not None and candidate_hour is not None
                         and baseline_session == candidate_session and baseline_hour == candidate_hour)
    coverage_starved = candidate_places == 0 and candidate_guard_skips >= max(25, baseline_places // 2)

    metrics = {
        "places": {
            "baseline": baseline.get("place_event_count"),
            "candidate": candidate.get("place_event_count"),
            "delta_pct": pct_change(
                baseline.get("place_event_count"), candidate.get("place_event_count")
            ),
        },
        "cancels": {
            "baseline": baseline.get("cancel_event_count"),
            "candidate": candidate.get("cancel_event_count"),
            "delta_pct": pct_change(
                baseline.get("cancel_event_count"), candidate.get("cancel_event_count")
            ),
        },
        "guard_skips": {
            "baseline": baseline.get("guard_skip_event_count"),
            "candidate": candidate.get("guard_skip_event_count"),
            "delta": diff(
                baseline.get("guard_skip_event_count"), candidate.get("guard_skip_event_count")
            ),
        },
        "roll_guard_skip_count_at_decision": {
            "baseline": baseline_summary.get("roll_guard_skip_count_at_decision"),
            "candidate": candidate_summary.get("roll_guard_skip_count_at_decision"),
            "delta": diff(
                baseline_summary.get("roll_guard_skip_count_at_decision"),
                candidate_summary.get("roll_guard_skip_count_at_decision"),
            ),
        },
        "avg_quote_age_ms": {
            "baseline": baseline_summary.get("avg_quote_age_ms"),
            "candidate": candidate_summary.get("avg_quote_age_ms"),
            "delta": diff(
                baseline_summary.get("avg_quote_age_ms"),
                candidate_summary.get("avg_quote_age_ms"),
            ),
        },
        "median_quote_age_ms": {
            "baseline": baseline_summary.get("median_quote_age_ms"),
            "candidate": candidate_summary.get("median_quote_age_ms"),
            "delta": diff(
                baseline_summary.get("median_quote_age_ms"),
                candidate_summary.get("median_quote_age_ms"),
            ),
        },
        "avg_adverse_move_bps": {
            "baseline": baseline_summary.get("avg_adverse_move_bps"),
            "candidate": candidate_summary.get("avg_adverse_move_bps"),
            "delta": diff(
                baseline_summary.get("avg_adverse_move_bps"),
                candidate_summary.get("avg_adverse_move_bps"),
            ),
        },
        "p90_adverse_move_bps": {
            "baseline": baseline_summary.get("p90_adverse_move_bps"),
            "candidate": candidate_summary.get("p90_adverse_move_bps"),
            "delta": diff(
                baseline_summary.get("p90_adverse_move_bps"),
                candidate_summary.get("p90_adverse_move_bps"),
            ),
        },
        "avg_target_spread_bps_at_decision": {
            "baseline": baseline_summary.get(
                "avg_target_spread_bps_at_decision",
                baseline_summary.get("avg_target_spread_bps_at_place", baseline_summary.get("avg_spread_bps_at_place")),
            ),
            "candidate": candidate_summary.get(
                "avg_target_spread_bps_at_decision",
                candidate_summary.get("avg_target_spread_bps_at_place", candidate_summary.get("avg_spread_bps_at_place")),
            ),
            "delta": diff(
                baseline_summary.get(
                    "avg_target_spread_bps_at_decision",
                    baseline_summary.get("avg_target_spread_bps_at_place", baseline_summary.get("avg_spread_bps_at_place")),
                ),
                candidate_summary.get(
                    "avg_target_spread_bps_at_decision",
                    candidate_summary.get("avg_target_spread_bps_at_place", candidate_summary.get("avg_spread_bps_at_place")),
                ),
            ),
        },
        "avg_observed_market_spread_bps_at_place": {
            "baseline": baseline_summary.get(
                "avg_observed_market_spread_bps_at_place",
                baseline_summary.get("avg_spread_bps_at_place"),
            ),
            "candidate": candidate_summary.get(
                "avg_observed_market_spread_bps_at_place",
                candidate_summary.get("avg_spread_bps_at_place"),
            ),
            "delta": diff(
                baseline_summary.get(
                    "avg_observed_market_spread_bps_at_place",
                    baseline_summary.get("avg_spread_bps_at_place"),
                ),
                candidate_summary.get(
                    "avg_observed_market_spread_bps_at_place",
                    candidate_summary.get("avg_spread_bps_at_place"),
                ),
            ),
        },
        "avg_basis_pct_at_decision": {
            "baseline": baseline_summary.get("avg_basis_pct_at_decision"),
            "candidate": candidate_summary.get("avg_basis_pct_at_decision"),
            "delta": diff(
                baseline_summary.get("avg_basis_pct_at_decision"),
                candidate_summary.get("avg_basis_pct_at_decision"),
            ),
        },
        "basis_widen_count_at_decision": {
            "baseline": baseline_summary.get("basis_widen_count_at_decision"),
            "candidate": candidate_summary.get("basis_widen_count_at_decision"),
            "delta": diff(
                baseline_summary.get("basis_widen_count_at_decision"),
                candidate_summary.get("basis_widen_count_at_decision"),
            ),
        },
        "avg_basis_widen_bps_at_decision": {
            "baseline": baseline_summary.get("avg_basis_widen_bps_at_decision"),
            "candidate": candidate_summary.get("avg_basis_widen_bps_at_decision"),
            "delta": diff(
                baseline_summary.get("avg_basis_widen_bps_at_decision"),
                candidate_summary.get("avg_basis_widen_bps_at_decision"),
            ),
        },
        "avg_roll_signal_at_decision": {
            "baseline": baseline_summary.get("avg_roll_signal_at_decision"),
            "candidate": candidate_summary.get("avg_roll_signal_at_decision"),
            "delta": diff(
                baseline_summary.get("avg_roll_signal_at_decision"),
                candidate_summary.get("avg_roll_signal_at_decision"),
            ),
        },
        "avg_roll_confidence_at_decision": {
            "baseline": baseline_summary.get("avg_roll_confidence_at_decision"),
            "candidate": candidate_summary.get("avg_roll_confidence_at_decision"),
            "delta": diff(
                baseline_summary.get("avg_roll_confidence_at_decision"),
                candidate_summary.get("avg_roll_confidence_at_decision"),
            ),
        },
        "no_arb_max_violation_cents": {
            "baseline": (((baseline.get("run_context") or {}).get("no_arb") or {}).get("max_violation_cents")),
            "candidate": (((candidate.get("run_context") or {}).get("no_arb") or {}).get("max_violation_cents")),
            "delta": diff(
                (((baseline.get("run_context") or {}).get("no_arb") or {}).get("max_violation_cents")),
                (((candidate.get("run_context") or {}).get("no_arb") or {}).get("max_violation_cents")),
            ),
        },
        "maker_roll_strongest_confidence": {
            "baseline": (((baseline.get("run_context") or {}).get("maker_roll") or {}).get("strongest_confidence")),
            "candidate": (((candidate.get("run_context") or {}).get("maker_roll") or {}).get("strongest_confidence")),
            "delta": diff(
                (((baseline.get("run_context") or {}).get("maker_roll") or {}).get("strongest_confidence")),
                (((candidate.get("run_context") or {}).get("maker_roll") or {}).get("strongest_confidence")),
            ),
        },
    }

    observations: list[str] = []
    adverse_delta = metrics["avg_adverse_move_bps"]["delta"]
    p90_delta = metrics["p90_adverse_move_bps"]["delta"]
    places_delta = metrics["places"]["delta_pct"]
    target_spread_delta = metrics["avg_target_spread_bps_at_decision"]["delta"]
    observed_spread_delta = metrics["avg_observed_market_spread_bps_at_place"]["delta"]

    if adverse_delta is not None:
        if adverse_delta < 0:
            observations.append(
                f"Candidate reduced avg adverse move by {abs(adverse_delta):.2f}bps vs baseline."
            )
        elif adverse_delta > 0:
            observations.append(f"Candidate increased avg adverse move by {adverse_delta:.2f}bps.")
    if p90_delta is not None:
        if p90_delta < 0:
            observations.append(f"Candidate reduced p90 adverse move by {abs(p90_delta):.2f}bps.")
        elif p90_delta > 0:
            observations.append(f"Candidate increased p90 adverse move by {p90_delta:.2f}bps.")
    if places_delta is not None:
        if places_delta < 0:
            observations.append(f"Candidate placed {abs(places_delta):.2f}% fewer quotes.")
        elif places_delta > 0:
            observations.append(f"Candidate placed {places_delta:.2f}% more quotes.")
    if candidate.get("guard_skip_event_count", 0) > 0:
        observations.append(
            f"Candidate skipped {candidate['guard_skip_event_count']} quote decisions via live guard telemetry."
        )
    roll_guard_skip_count = candidate_summary.get("roll_guard_skip_count_at_decision")
    if roll_guard_skip_count:
        observations.append(
            f"Candidate skipped {int(roll_guard_skip_count)} quote decisions via the Roll toxicity guard."
        )
    widen_count = candidate_summary.get("basis_widen_count_at_decision")
    if widen_count:
        observations.append(
            f"Candidate applied backwardation widening on {int(widen_count)} quote decisions."
        )
    if coverage_starved:
        observations.append(
            "Candidate starved quote coverage completely in this window, so the guard is currently too blunt "
            "to compare on fill-quality metrics."
        )
    if target_spread_delta is not None:
        if target_spread_delta < 0:
            observations.append(
                f"Candidate targeted {abs(target_spread_delta):.2f}bps tighter quote width on average."
            )
        elif target_spread_delta > 0:
            observations.append(
                f"Candidate targeted {target_spread_delta:.2f}bps wider quote width on average."
            )
    if observed_spread_delta is not None:
        if observed_spread_delta < 0:
            observations.append(
                f"Candidate operated in a tighter observed market spread by {abs(observed_spread_delta):.2f}bps."
            )
        elif observed_spread_delta > 0:
            observations.append(
                f"Candidate operated in a wider observed market spread by {observed_spread_delta:.2f}bps."
            )
    roll_conf_delta = metrics["avg_roll_confidence_at_decision"]["delta"]
    if roll_conf_delta is not None:
        if roll_conf_delta > 0.05:
            observations.append(
                f"Candidate operated in stronger Roll-persistent conditions (+{roll_conf_delta:.3f} avg confidence)."
            )
        elif roll_conf_delta < -0.05:
            observations.append(
                f"Candidate operated in weaker Roll-persistent conditions ({roll_conf_delta:.3f} avg confidence)."
            )
    no_arb_delta = metrics["no_arb_max_violation_cents"]["delta"]
    if no_arb_delta is not None:
        if no_arb_delta > 0.5:
            observations.append(
                f"Candidate run ended in a more dislocated no-arb environment (+{no_arb_delta:.2f}c max violation)."
            )
        elif no_arb_delta < -0.5:
            observations.append(
                f"Candidate run ended in a cleaner no-arb environment ({no_arb_delta:.2f}c max violation)."
            )
    if not comparable_window:
        observations.append(
            f"Dominant run windows differ (baseline {baseline_session}/{baseline_hour}, "
            f"candidate {candidate_session}/{candidate_hour}), so this is directional evidence, not a clean promotion test."
        )

    verdict = "inconclusive"
    if coverage_starved:
        verdict = "guard_starved_candidate"
    elif sample_adequate and not comparable_window:
        if adverse_delta is not None and adverse_delta < -20 and p90_delta is not None and p90_delta < 0:
            verdict = "promising_but_noncomparable"
    elif sample_adequate and comparable_window:
        if adverse_delta is not None and adverse_delta < -20 and p90_delta is not None and p90_delta < 0:
            verdict = "promising_guard_candidate"
        elif adverse_delta is not None and adverse_delta > 10:
            verdict = "guard_regression"

    if not configuration_bound:
        verdict = "configuration_unverified"
        observations.append("Configuration evidence is missing or the two legs differ outside the tested treatment.")

    return {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "baseline_report": str(baseline_path),
        "candidate_report": str(candidate_path),
        "sample_adequate": sample_adequate,
        "comparable_window": comparable_window,
        "coverage_starved": coverage_starved,
        "configuration_bound": configuration_bound,
        "treatment_mode": treatment_mode,
        "baseline_config_fingerprint": config_fingerprint(baseline_config) if baseline_config is not None else None,
        "candidate_config_fingerprint": config_fingerprint(candidate_config) if candidate_config is not None else None,
        "baseline_window": {"session": baseline_session, "hour": baseline_hour},
        "candidate_window": {"session": candidate_session, "hour": candidate_hour},
        "verdict": verdict,
        "metrics": metrics,
        "observations": observations,
    }


def render_markdown(report: dict[str, Any]) -> str:
    lines = [
        "# Maker A/B Comparison",
        "",
        f"- Generated: `{report['generated_at']}`",
        f"- Baseline: `{report['baseline_report']}`",
        f"- Candidate: `{report['candidate_report']}`",
        f"- Verdict: `{report['verdict']}`",
        f"- Sample adequate: `{report['sample_adequate']}`",
        f"- Comparable window: `{report['comparable_window']}`",
        f"- Configuration bound: `{report['configuration_bound']}`",
        f"- Baseline window: `{report['baseline_window']['session']}` / `{report['baseline_window']['hour']}`",
        f"- Candidate window: `{report['candidate_window']['session']}` / `{report['candidate_window']['hour']}`",
        "",
        "## Metrics",
        "",
    ]
    for key, value in report["metrics"].items():
        lines.append(
            f"- `{key}`: baseline=`{value.get('baseline')}` candidate=`{value.get('candidate')}` "
            f"delta=`{value.get('delta', value.get('delta_pct'))}`"
        )
    lines.extend(["", "## Observations", ""])
    if report["observations"]:
        for item in report["observations"]:
            lines.append(f"- {item}")
    else:
        lines.append("- None")
    return "\n".join(lines) + "\n"


def main() -> int:
    args = parse_args()
    baseline_path = Path(args.baseline).expanduser().resolve()
    candidate_path = Path(args.candidate).expanduser().resolve()
    outdir = Path(args.outdir).expanduser().resolve()
    outdir.mkdir(parents=True, exist_ok=True)

    report = compare_reports(
        load_report(baseline_path),
        load_report(candidate_path),
        baseline_path,
        candidate_path,
        args.candidate_mode,
    )
    stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = outdir / f"{stamp}_maker_ab_compare.json"
    md_path = outdir / f"{stamp}_maker_ab_compare.md"
    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    md_path.write_text(render_markdown(report), encoding="utf-8")

    print(json.dumps({"json": str(json_path), "md": str(md_path), "verdict": report["verdict"]}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
