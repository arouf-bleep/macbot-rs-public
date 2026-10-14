"""Chronological, market-disjoint evaluation splits without ML dependencies."""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
import math


@dataclass(frozen=True)
class EvaluationSplit:
    development: tuple[int, ...]
    holdout: tuple[int, ...]
    purged: tuple[int, ...]


def chronological_holdout(
    metadata: list[dict],
    fraction: float = 0.2,
    embargo_seconds: int = 900,
) -> EvaluationSplit:
    """Reserve the newest observations before any training or model selection.

    Equal timestamps stay together. Earlier observations from any held-out
    market are removed, along with the embargo preceding the holdout boundary.
    Callers must set an embargo at least as long as their target horizon.
    """
    if not 0 < fraction < 1:
        raise ValueError("holdout fraction must be between 0 and 1")
    if embargo_seconds < 0:
        raise ValueError("embargo must be nonnegative")
    if len(metadata) < 2:
        raise ValueError("at least two observations are required")

    timestamps = []
    markets = []
    for row in metadata:
        timestamp = datetime.fromisoformat(str(row["timestamp"]).replace("Z", "+00:00"))
        if timestamp.tzinfo is None:
            timestamp = timestamp.replace(tzinfo=timezone.utc)
        timestamps.append(timestamp.astimezone(timezone.utc))
        market = str(row.get("market_id") or "").strip()
        if not market:
            raise ValueError("every observation needs a market_id to prevent outcome overlap")
        markets.append(market)

    ordered = sorted(range(len(metadata)), key=timestamps.__getitem__)
    count = max(1, math.ceil(len(ordered) * fraction))
    boundary = timestamps[ordered[-count]]
    holdout = tuple(i for i in ordered if timestamps[i] >= boundary)
    held_markets = {markets[i] for i in holdout}
    cutoff = boundary - timedelta(seconds=embargo_seconds)
    development = tuple(
        i for i in ordered
        if timestamps[i] < cutoff and markets[i] not in held_markets
    )
    if not development:
        raise ValueError("no development samples remain after market and time purging")
    retained = set(development) | set(holdout)
    return EvaluationSplit(development, holdout, tuple(i for i in ordered if i not in retained))
