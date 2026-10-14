# L2 Replay Check

Use this when debugging sequence-gap incidents or suspected phantom liquidity after WS reconnects.

## Input format

`scripts/l2_replay_check.py` accepts JSONL where each line is either:

1. Raw WS payload object
2. Wrapper with `payload` key containing the raw object

## Command

```bash
python3 scripts/l2_replay_check.py /path/to/l2-events.jsonl
```

## Output

- Per-token counts:
  - `seen`
  - `gaps` (`seq > prev + 1`)
  - `ooo` (out-of-order, `seq < prev`)
  - `dup` (duplicate sequence)
- Global summary line.

Exit codes:

- `0`: no gaps/out-of-order events detected
- `2`: at least one gap or out-of-order event detected
- non-zero runtime code: parse/file errors
