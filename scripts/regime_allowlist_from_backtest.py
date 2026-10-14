#!/usr/bin/env python3
"""Suggest a regime_allowlist from /api/backtest edge_audit.by_regime.

Usage:
  python3 scripts/regime_allowlist_from_backtest.py --base-url http://localhost:8000 --min-trades 20
"""
from __future__ import annotations

import argparse
import json
import math
import sys

from api_auth import add_api_auth_args, post_json


def suggest_allowlist(out: dict, min_trades: int, min_gap: float) -> list[str]:
    if "error" in out:
        raise RuntimeError(f"backtest API error: {out['error']}")
    edge = out.get("edge_audit")
    if not isinstance(edge, dict) or not isinstance(edge.get("by_regime"), dict):
        raise RuntimeError("backtest response is missing per-regime evidence")
    allow = []
    for regime, row in edge["by_regime"].items():
        if not isinstance(row, dict):
            raise RuntimeError(f"invalid backtest bucket for {regime}")
        try:
            trades = int(row.get("trades", 0))
        except (TypeError, ValueError, OverflowError) as exc:
            raise RuntimeError(f"invalid backtest trade count for {regime}") from exc
        gap = row.get("edge_gap_pp")
        if gap is None or isinstance(gap, bool):
            continue
        try:
            gap = float(gap)
        except (TypeError, ValueError, OverflowError):
            continue
        if math.isfinite(gap) and trades >= min_trades and gap >= min_gap:
            allow.append(regime)
    return sorted(allow)


def main() -> int:
    p = argparse.ArgumentParser(description="Regime allowlist suggestion")
    p.add_argument("--base-url", default="http://localhost:8000")
    p.add_argument("--days", type=int, default=14)
    p.add_argument("--entry-threshold", type=float, default=0.30)
    p.add_argument("--min-trades", type=int, default=20)
    p.add_argument("--min-edge-gap-pp", type=float, default=0.0)
    p.add_argument("--request-timeout-secs", type=int, default=120)
    add_api_auth_args(p)
    args = p.parse_args()

    payload = {
        "days": args.days,
        "entry_threshold": args.entry_threshold,
    }
    url = args.base_url.rstrip("/") + "/api/backtest"
    try:
        out = post_json(
            url,
            payload,
            timeout=args.request_timeout_secs,
            api_key=args.api_key,
            api_secret=args.api_secret,
            require_auth=True,
        )
    except RuntimeError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    try:
        allow = suggest_allowlist(out, args.min_trades, args.min_edge_gap_pp)
    except RuntimeError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    if not allow:
        print(
            "error: no qualifying evidence; an empty regime_allowlist means "
            "unrestricted trading, so no recommendation was generated",
            file=sys.stderr,
        )
        return 2
    print(json.dumps({"regime_allowlist": allow}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
