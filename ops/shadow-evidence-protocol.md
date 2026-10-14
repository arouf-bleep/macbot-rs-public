# Historical shadow evidence protocol — TREND_UP / BUY_YES

> Superseded research plan. The recorded model/profile identities and proposed fixed-strategy behavior below do not describe the current source candidate. The original undocumented model is excluded and the current model is an untrained fixture. This document is retained as a historical protocol; it does not authorize a current collection window or establish strategy performance. See the current limitations and roadmap before designing a new, independently registered experiment.

**Status: pre-registered. Acceptance criteria below were fixed on 2026-09-14, before any
observation in this window was collected. They must not be edited after data collection starts.
If they change, the window is void and a new one begins.**

This exists because the project's own alpha review concluded: "Do not continue slicing the same
historical sample. That would increase overfitting risk without adding independent evidence." The
only honest next step is a fresh, untouched sample against a frozen strategy.

## Frozen artefacts

| Artefact | Value |
| --- | --- |
| Profile | `ops/profiles/shadow_trend_up_yes_frozen.json` |
| Profile SHA-256 | `30a76baca8f13050c350e20dacfbc8a03067a85fdee9523af8710120c227c602` |
| Model | `models/latest.onnx` |
| Model SHA-256 | `85ecf8da7b4f0191a9694ef2b27ed2aabd3885d92bf4f2491a3ce6e5fa906f8c` |
| Lane | TREND_UP / BUY_YES only; BUY_NO gated off, not merely deprioritised |
| Execution | Disabled. Shadow mode, no funded orders, no credentials required |

Nothing adaptive may move during the window. Kelly sizing, adaptive stand-down, time-of-day
weighting, regime and session allowlists are all disabled, and the stake is fixed at $1.00 so
position size cannot drift with equity. A strategy that retunes itself mid-window is not one
strategy, and its results cannot be interpreted.

## What is recorded

Per decision, whether or not it becomes a trade:

- Configuration in effect, redacted, and its hash
- Model version hash
- Every signal value and the fused output
- Rejection reason when no trade is taken, from `trade_rejections`
- Simulated entry and exit, fees, and the resulting P&L, from `trades` and `trade_fills`
- Decision captures for offline replay, via `MACBOT_DECISION_CAPTURE_PATH`

Rejections matter as much as fills. A strategy that declines 95% of markets is a different object
from one that trades them, even if the traded subset looks identical.

## Sample size required before any conclusion

**Minimum 200 closed shadow trades across at least 60 distinct markets and at least 21 calendar
days.** The retained historical snapshot was 66 trades across 32 markets over 13 days, which is too
small to separate skill from noise at any plausible edge. At 200 trades a true 55% win rate is
still only about 1.4 standard errors from a coin flip, so this is a floor, not a comfort.

The window may not be stopped early because results look good. It may be stopped early only if a
kill criterion below is hit.

## Acceptance criteria, fixed in advance

Evaluate only after the sample size above is met. All four must hold.

1. **Net expectancy positive after costs.** Mean P&L per closed trade greater than zero with fees
   and simulated slippage included. Gross figures do not count.
2. **Lower confidence bound above break-even.** The 95% lower bound on win rate must exceed the
   break-even rate implied by the average entry price. A point estimate above break-even is not
   sufficient.
3. **Stability across thirds.** Split the window chronologically into three equal parts. At least
   two must show positive expectancy, and no third may lose more than twice the mean gain of the
   others. This is the test the previous candidates failed.
4. **Beats the null.** Expectancy must exceed a random-entry baseline over the same markets and the
   same fee model, by more than the baseline's own standard error.

If all four hold, the outcome is a candidate worth a further out-of-sample window. It is **not**
approval for funded execution. Canary follows `ops/live-rollout-runbook.md` and remains a separate
decision.

## Kill criteria

Stop the window immediately and record why:

- Cumulative simulated drawdown exceeds 25% of the starting balance
- Feed staleness or degraded no-trade exceeds 10% of wall-clock time, meaning the sample reflects
  infrastructure rather than strategy
- Any configuration or model hash changes mid-window
- Rejection rate exceeds 98%, meaning there is no strategy to evaluate

## Interpretation limits

Shadow results are simulated fills. `docs/REPLAY_AND_FILL_FIDELITY.md` documents where the fill
model diverges from real execution, and the remaining differences are not a measured bound on real performance.
Passing the criteria above establishes that the strategy is worth testing with money. It does not
establish that it makes money.

## Running the window

```bash
./scripts/run_shadow_evidence.sh ARTIFACT_DIR
```

The script refuses to start if the profile or model hash does not match the values recorded above,
and stamps both into the run manifest.
