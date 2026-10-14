# Shutdown Proof Runbook

Use this runbook for the truth-first phase multi-open shutdown proof. The goal is not alpha; the goal is to prove shutdown cleanup with real open maker inventory.

## When To Run

Attempt the proof only in a live UTC window with evidence of real market activity.

- Preferred window: `23:00-23:30 UTC`
- Fallback window: `10:15-10:45 UTC`
- Avoid: `02:30-06:00 UTC`

Use the current shutdown validation profile as-is:

```bash
python3 scripts/run_frozen_maker_soak.py \
  --config ops/profiles/maker_paper_validation_shutdown.json \
  --db artifacts/soak-runs/shutdown_validation_multiopen_proof.sqlite \
  --duration-minutes 20 \
  --shutdown-validation \
  --stop-on-open-trades 2 \
  --min-run-seconds 45
```

## Success Criteria

Treat the proof as successful only if all of these are true:

1. The run reaches at least `2` open maker trades before exit.
2. The final DB has `0 OPEN` trades.
3. The final DB has `0 CLOSING` trades.
4. Each shutdown-closed trade has `exit_type='shutdown_cleanup'`.
5. Each shutdown cleanup trade has matching `mark_exit` fill evidence.
6. The integrity report returns `status=ok` and `mismatch_count=0`.

## Early-Stop Starvation Rule

Abort and classify as `starvation/dead_window` if the first `3-5` minutes show:

- `0` events
- `0` maker quote activity
- `0` open or closing trades
- repeated `all_markets is EMPTY after filtering` messages

If there are suppressions but no trades, do not call it starvation yet. That is a plumbing signal and should be classified separately by the run bundle.

## How To Read The Bundle

Read the four outputs together:

- Maker soak report
- Sim price verification report
- Validation integrity report
- Runner terminal classification

The practical order is:

1. Check `terminal_class` first.
2. Check `run_classification` and `primary_blocker_summary`.
3. Check `status`, `verdict`, `reason`, and `mismatch_count` in the integrity report.
4. Check whether the sim verification marked execution fills plausible and cleanup marks clean.

Bundle meanings:

- `success`: the proof reached the open-trade target and shut down cleanly.
- `lifecycle_bug`: shutdown or persistence truth failed.
- `policy_suppression`: gates blocked quoting/trading.
- `market_quality_failure`: quotes were present, but books were not good enough.
- `starvation/dead_window`: the run never meaningfully exercised the quote path.

## If The Proof Passes

Update these surfaces:

- [docs/history/docs/history/CHANGELOG.md](../docs/history/docs/history/CHANGELOG.md)
- [truth-first-program.md](./truth-first-program.md)
- [profiles/maker_rollout_state.json](./profiles/maker_rollout_state.json)

Record:

- the proof DB path
- the soak report path
- the verification report path
- the integrity report path
- whether the proof was multi-open or still only single-open

If the proof passes multi-open cleanup, keep alpha shelved unless the project owner explicitly reopens that phase.
