# Shadow Kill Criteria

Use this checker before any canary/live rollout.

## Script

`scripts/shadow_kill_check.py`

Exit codes:

- `0` = PASS (eligible for canary)
- `2` = FAIL (kill criteria triggered)
- `1` = runtime/API error

## Default thresholds

- `SHADOW_MIN_TRADES=200`
- `SHADOW_MIN_WIN_RATE=0.53`
- `SHADOW_MIN_EXPECTANCY_USD=0.003`
- `SHADOW_MIN_PROFIT_FACTOR=1.03`
- `SHADOW_MAX_DRAWDOWN_PCT=25.0`
- `SHADOW_MIN_RECENT_TRADES=50`
- `SHADOW_MIN_RECENT_WIN_RATE=0.50`

## Command

```bash
MACBOT_API_BASE="http://localhost:8000" \
scripts/shadow_kill_check.py
```

Override thresholds example:

```bash
MACBOT_API_BASE="http://localhost:8000" \
SHADOW_MIN_TRADES=300 \
SHADOW_MIN_WIN_RATE=0.54 \
SHADOW_MIN_EXPECTANCY_USD=0.005 \
SHADOW_MIN_PROFIT_FACTOR=1.05 \
SHADOW_MAX_DRAWDOWN_PCT=20 \
scripts/shadow_kill_check.py
```

## Notes

- The script evaluates only closed trades where `source == "shadow"` or `mode == "shadow"`.
- `max_drawdown_pct` is computed from realized closed-trade PnL against current `initial_balance` in `/api/config`.
- If the API is unreachable or malformed, the script exits with code `1`.
