#!/usr/bin/env python3
"""
Deterministic L2 replay sanity checker.

Reads a JSONL stream of CLOB WS payloads and reports per-token sequence issues:
- out-of-order updates
- sequence gaps (> prev + 1)
- duplicate sequence values

Accepted JSONL line formats:
1) Raw payload object (as received on WS)
2) Wrapper object with `payload` key containing the raw payload
"""

from __future__ import annotations

import argparse
import json
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Any


SEQ_KEYS = ("sequence", "seq", "offset", "version")
TOKEN_KEYS = ("asset_id", "token_id", "market", "condition_id")
EVENT_KEYS = ("event_type", "event", "type")


@dataclass
class SeqStats:
    seen: int = 0
    duplicates: int = 0
    out_of_order: int = 0
    gaps: int = 0
    last_seq: int | None = None


def extract_sequence(obj: Any) -> int | None:
    if isinstance(obj, dict):
        for k in SEQ_KEYS:
            v = obj.get(k)
            if isinstance(v, int) and not isinstance(v, bool) and v >= 0:
                return v
            if isinstance(v, str) and v.isdigit():
                return int(v)
        for v in obj.values():
            seq = extract_sequence(v)
            if seq is not None:
                return seq
    elif isinstance(obj, list):
        for item in obj:
            seq = extract_sequence(item)
            if seq is not None:
                return seq
    return None


def extract_token_id(obj: Any) -> str | None:
    if isinstance(obj, dict):
        for k in TOKEN_KEYS:
            v = obj.get(k)
            if isinstance(v, str) and v:
                return v
        for v in obj.values():
            token = extract_token_id(v)
            if token:
                return token
    elif isinstance(obj, list):
        for item in obj:
            token = extract_token_id(item)
            if token:
                return token
    return None


def extract_event_type(obj: Any) -> str:
    if isinstance(obj, dict):
        for k in EVENT_KEYS:
            v = obj.get(k)
            if isinstance(v, str) and v:
                return v
    return "unknown"


def parse_line(line: str) -> dict[str, Any] | list[Any] | None:
    line = line.strip()
    if not line:
        return None
    obj = json.loads(line)
    if isinstance(obj, dict) and isinstance(obj.get("payload"), (dict, list)):
        return obj["payload"]
    if isinstance(obj, (dict, list)):
        return obj
    return None


def frame_observations(payload):
    """One sequence observation per token per wire frame, including batches."""
    frames = payload if isinstance(payload, list) else [payload]
    observations = []
    for frame in frames:
        if not isinstance(frame, dict):
            continue
        event = extract_event_type(frame)
        changes = frame.get("price_changes")
        rows = changes if isinstance(changes, list) else [frame]
        frame_sequence = extract_sequence({key: frame[key] for key in SEQ_KEYS if key in frame})
        seen = set()
        for row in rows:
            if not isinstance(row, dict):
                continue
            token = next((row[key] for key in ("asset_id", "token_id") if isinstance(row.get(key), str) and row[key]), None)
            sequence = extract_sequence(row)
            if sequence is None:
                sequence = frame_sequence
            if sequence is None:
                continue
            identity = (token or "UNKNOWN", sequence)
            if identity not in seen:
                observations.append((identity[0], sequence, event))
                seen.add(identity)
    return observations


def run(path: Path) -> int:
    stats = defaultdict(SeqStats)
    unknown_token = SeqStats()
    line_count = 0
    event_counts = defaultdict(int)

    with path.open("r", encoding="utf-8") as fh:
        for raw in fh:
            line_count += 1
            payload = parse_line(raw)
            if not payload:
                continue
            for token, seq, event in frame_observations(payload):
                event_counts[event] += 1
                bucket = stats[token] if token != "UNKNOWN" else unknown_token
                bucket.seen += 1
                if bucket.last_seq is None or event == "book":
                    bucket.last_seq = seq
                    continue
                if seq == bucket.last_seq:
                    bucket.duplicates += 1
                elif seq < bucket.last_seq:
                    bucket.out_of_order += 1
                elif seq > bucket.last_seq + 1:
                    bucket.gaps += 1
                if seq > bucket.last_seq:
                    bucket.last_seq = seq

    print(f"lines={line_count}")
    print("event_counts=", json.dumps(dict(sorted(event_counts.items())), sort_keys=True))

    total_gaps = 0
    total_ooo = 0
    total_dup = 0
    total_seen = 0
    for token in sorted(stats.keys()):
        s = stats[token]
        total_gaps += s.gaps
        total_ooo += s.out_of_order
        total_dup += s.duplicates
        total_seen += s.seen
        print(
            f"token={token} seen={s.seen} gaps={s.gaps} "
            f"ooo={s.out_of_order} dup={s.duplicates} last_seq={s.last_seq}"
        )

    if unknown_token.seen:
        total_gaps += unknown_token.gaps
        total_ooo += unknown_token.out_of_order
        total_dup += unknown_token.duplicates
        total_seen += unknown_token.seen
        print(
            "token=UNKNOWN "
            f"seen={unknown_token.seen} gaps={unknown_token.gaps} "
            f"ooo={unknown_token.out_of_order} dup={unknown_token.duplicates} "
            f"last_seq={unknown_token.last_seq}"
        )

    print(
        f"summary seen={total_seen} gaps={total_gaps} "
        f"ooo={total_ooo} dup={total_dup}"
    )
    # Non-zero exit if quality issues were found.
    return 2 if (total_gaps or total_ooo or total_dup or not total_seen) else 0


def main() -> int:
    ap = argparse.ArgumentParser(description="Replay-check L2 WS JSONL for sequence integrity")
    ap.add_argument("jsonl", type=Path, help="Path to JSONL file")
    args = ap.parse_args()
    return run(args.jsonl)


if __name__ == "__main__":
    raise SystemExit(main())
