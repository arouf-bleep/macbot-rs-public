# Plumbing Backlog

`macbot-rs` is currently in a truth-first phase. This backlog tracks the remaining
plumbing work needed before alpha work resumes.

## Status Snapshot (2026-09-13)

| Batch | Status |
| --- | --- |
| 1. Shutdown Truth | Code-complete; multi-open non-starved proof still pending |
| 2. Validation Harness Truth | Implemented |
| 3. Lifecycle Integrity | Implemented with automated reports/tests |
| 4. Feed and Session Truth | Implemented |
| 5. Reporting and Operator Readout | Implemented |
| 6. Window Intelligence | Implemented; continued observation required |
| 7. Storage and Artifact Hygiene | Implemented operationally; ongoing maintenance |
| 8. Truth-State Documentation | Implemented; ongoing maintenance |

## Batch 1: Shutdown Truth

Goal: prove shutdown cleanup under real open inventory, not just empty or single-trade exits.

Work:
- run a multi-open shutdown-validation soak in a known active market window
- prove `0` stranded `OPEN` trades after exit
- prove `0` stranded `CLOSING` trades after exit
- prove every shutdown-closed trade records `exit_type='shutdown_cleanup'`
- prove every shutdown cleanup has matching `mark_exit` evidence
- rerun the proof until it passes in a non-starved window

Done when:
- a `--stop-on-open-trades 2` shutdown-validation run passes with clean lifecycle state
- the integrity report returns `status: ok`

## Batch 2: Validation Harness Truth

Goal: make validation runs classify themselves cleanly and fail closed.

Work:
- keep startup failure classification explicit
- keep soak, fill-verification, and integrity outputs bundled together
- add clean classification for starvation vs policy suppression vs market-quality failure
- make post-run validation fail closed on lifecycle mismatches
- keep self-tests for shutdown validation and startup-failure classification current

Done when:
- every validation run ends in a clear terminal class instead of manual interpretation

## Batch 3: Lifecycle Integrity

Goal: make local trading state internally consistent without manual DB forensics.

Work:
- extend integrity checks across `trades`, `trade_fills`, `order_outbox`, and `trade_rejections`
- catch orphan fills, missing entry evidence, bad cleanup rows, and outbox/fill mismatches
- add compact machine-readable and human-readable integrity summaries
- run the report automatically for validation runs

Done when:
- recent validation DBs pass the integrity sweep cleanly
- integrity failures point directly to the broken invariant

## Batch 4: Feed and Session Truth

Goal: make no-trade runs explain whether the blocker was the market, the feed, or our own policy.

Work:
- surface warmup timeout counts and stale durations clearly
- report when there are no active filtered markets
- report two-sided-book coverage and quoteable market availability
- distinguish dead windows from policy suppression and from market-quality collapse
- keep target drift / reconnect diagnostics honest

Done when:
- zero-trade runs can be classified without ad hoc log digging

## Batch 5: Reporting and Operator Readout

Goal: make the truth surfaces good enough that the operator does not need SQLite spelunking.

Work:
- keep soak reports focused on blockers, suppress mix, rejection mix, and shutdown outcome
- include lifecycle integrity summaries in validation bundles
- keep fill plausibility summaries easy to compare across runs
- make starvation and shutdown-proof outcomes visible in reports and readiness surfaces

Done when:
- a normal validation/control run can be judged from generated artifacts alone

## Batch 6: Window Intelligence

Goal: stop wasting proof attempts in dead hours.

Work:
- keep a current map of active and dead UTC windows for validation soaks
- define an early-stop rule for obvious starvation runs
- attach the resulting classification to validation artifacts

Done when:
- proof attempts are concentrated in windows that can actually exercise the plumbing

## Batch 7: Storage and Artifact Hygiene

Goal: keep the truth program sustainable and inspectable.

Work:
- keep validation artifacts organized and prunable
- ensure backup and snapshot flows remain usable
- prevent stale proof artifacts from obscuring the current state
- keep the repo lean while preserving important evidence

Done when:
- the latest proof artifacts are easy to find and old ones are easy to archive or prune

## Batch 8: Truth-State Documentation

Goal: keep project state synchronized with reality.

Work:
- update `docs/history/docs/history/CHANGELOG.md` as each plumbing batch lands
- update `ops/truth-first-program.md` when exit criteria meaningfully advance
- update `ops/profiles/maker_rollout_state.json` when a proof moves from pending to done
- keep readiness artifacts aligned with the actual truth-first phase

Done when:
- docs and readiness surfaces stop lagging behind the code and evidence

## Current Priority

1. Complete Batch 1 with a non-starved multi-open shutdown run.
2. Preserve the implemented truth surfaces while collecting candidate-quality alpha evidence.
3. Keep storage hygiene and documentation synchronized with new validation runs.
