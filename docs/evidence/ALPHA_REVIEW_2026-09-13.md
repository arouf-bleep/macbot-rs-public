# Alpha review — 2026-09-13

This review asks a narrow question: does the current repository contain a strategy configuration that is defensible for live use? The answer from the fresh tests below is **no**. The result is useful because it identifies what should be retired, what may be worth observing, and what the project must prove next.

## Safety and method

- The bot ran in isolated `MACBOT_BACKTEST_ONLY=1` mode on loopback port `18080`.
- No exchange feeds, trading loop, credentials, orders, or live capital were used.
- Each candidate was tested over 7-, 14-, and 21-day windows with 0-, 7-, and 14-day shifts.
- A separate 30-day stress window used the same three shifts.
- Promotion gates required positive aggregate P&L and edge, adequate positive-fold ratios, and acceptable decay.
- Filters were tested sequentially after a lane autopsy; no broad parameter search was used.

## Results

P&L values below are backtest outputs under the report's configured $10 initial balance. They are comparison evidence, not expected future returns.

| Candidate | Weighted 7/14/21d P&L | 30d stress mean P&L | Positive stress folds | Verdict |
| --- | ---: | ---: | ---: | --- |
| Directional baseline | -$0.0969 | -$3.0039 | 0/3 | Fail |
| Directional, Europe session only | +$0.8676 | -$0.9837 | 1/3 | Fail; recent-window effect |
| TREND_UP / BUY_YES only | +$0.3379 | -$0.9328 | 1/3 | Fail; closest lane, not stable |
| TREND_DOWN / BUY_NO only | -$1.2993 | -$3.4265 | 0/3 | Fail; retire from consideration |
| Existing `trend_up_yes_recent_specialist` preset | -$0.9806 | -$2.0311 | 0/3 | Fail |

The 30-day baseline autopsy contained 47 trades and lost $1.8857 in its current fold. Europe was the only profitable session in that sample (+$1.4862 across 22 trades); Asia and US lost $2.2031 and $1.1688 respectively. Re-running Europe as an actual filter changed the adaptive stand-down path and still failed the stress gate, which is why the subset result is not treated as edge.

## Decision

- Keep rollout status at `baseline_only`; there is no active alpha candidate.
- Do not promote the named recent specialist or any session-filtered variant.
- Treat the TREND_DOWN / BUY_NO branch as a negative finding, not a tuning target.
- TREND_UP / BUY_YES is the least-bad research lane, but it is suitable only for collecting new shadow observations. It is not approved for live execution.
- Do not continue slicing the same historical sample. That would increase overfitting risk without adding independent evidence.

## Reproduction

The isolated server was started with:

```sh
CARGO_TARGET_DIR=/tmp/macbot_sale_target MACBOT_PORT=18080 ./scripts/run_demo.sh
```

The main reports were generated with:

```sh
python3 -u scripts/recency_alpha_report.py --preset trend_directional_specialist --base-url http://127.0.0.1:18080 --days-list 7,14,21 --stress-days 30 --fold-shifts 0,7,14 --stress-fold-shifts 0,7,14 --top 1 --request-interval-secs 0.35 --api-key local --api-secret localsecret
python3 -u scripts/lane_forensics_report.py --preset trend_directional_specialist --base-url http://127.0.0.1:18080 --lane-label directional_30d_shift0 --days 30 --shift-days 0 --api-key local --api-secret localsecret
python3 -u scripts/recency_alpha_report.py --preset trend_up_yes_recent_specialist --base-url http://127.0.0.1:18080 --days-list 7,14,21 --stress-days 30 --fold-shifts 0,7,14 --stress-fold-shifts 0,7,14 --top 1 --request-interval-secs 0.35 --api-key local --api-secret localsecret
```

Additional invocations of the first recency command applied `--session-allowlist EUROPE`, then split `TREND_UP:yes_only` and `TREND_DOWN:no_only` to test the findings from the autopsy.

Local generated artifacts:

- `artifacts/alpha-reports/20260913_213800_recency_alpha_report.{json,md}`
- `artifacts/alpha-reports/20260913_213824_directional_30d_shift0_lane_forensics.{json,md}`
- `artifacts/alpha-reports/20260913_213858_recency_alpha_report.{json,md}`
- `artifacts/alpha-reports/20260913_213953_recency_alpha_report.{json,md}`
- `artifacts/alpha-reports/20260913_214025_recency_alpha_report.{json,md}`
- `artifacts/alpha-reports/20260913_214217_recency_alpha_report.{json,md}`

## Next highest-signal move

Collect a genuinely new, untouched shadow dataset for TREND_UP / BUY_YES while leaving execution disabled. Re-evaluate only after the sample is large enough to test without reusing these folds. The project's near-term value comes from trustworthy execution, observability, and honest evidence—not from claiming profitability that the current data does not support.
