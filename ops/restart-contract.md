# Restart Contract (Live Mode)

This contract defines what must happen on process restart before the bot is considered trading-safe.

## Invariants

1. Execution actor must be running before API serving.
2. SQLite `quick_check` must pass on startup before the bot is considered service-ready.
3. Open live trades must replay persisted `trade_fills` once on startup before hot memory is considered authoritative.
4. A startup full live resync must run once in `mode=live`.
5. Full resync must reconcile by stable `trade.id` (never by timestamp).
6. Any DB-vs-memory OPEN live drift above policy threshold trips fail-closed.
7. Runtime SLO guard must run continuously and remain visible via `/api/risk/status`.

## Startup sequence

1. Load config + DB.
2. Run SQLite `quick_check`.
3. Build bot, replay open-live fill ledger state, and start execution actor.
4. If `mode=live`, call `full_resync_live_state()` through the execution actor once.
5. Start the loop only if preflight passed and recovery left neither a kill switch nor degraded state.
6. Start API + background loops (L2, TP/SL, reconcile, full-resync cadence, SLO guard).

## Drift contract

- Source of truth for OPEN live trades is DB (`trades` table).
- Full resync repairs local memory to DB state.
- Unknown in-memory OPEN live trades are force-closed locally with:
  - `exit_type = reconcile_missing_db_live_open`
  - `pnl = 0.0`
- Missing in-memory OPEN live trades present in DB are re-hydrated into memory.
- After an initial repair, a second full resync should be idempotent and record zero drift.
- If drift count exceeds `MACBOT_RECONCILE_MAX_LIVE_DRIFT` (default `0`), kill switch activates.

## Runtime SLO guard contract

- Applies only in `mode=live` and `execution_mode in {maker,hybrid}`.
- L2 sustained breach:
  - threshold: `MACBOT_SLO_MAX_L2_AGE_MS` (default `3000`)
  - hold window: `MACBOT_SLO_L2_BREACH_HOLD_MS` (default `15000`)
  - action: mark L2 stale + publish stale reason
  - optional action: kill switch if `MACBOT_SLO_KILL_ON_L2_BREACH=true` (default true)
- Backpressure breach:
  - actor drop delta: `MACBOT_SLO_MAX_ACTOR_CMD_DROPS_DELTA` (default `120`)
  - outbox drop delta: `MACBOT_SLO_MAX_OUTBOX_DROPS_DELTA` (default `60`)
  - action: kill switch

## Operator visibility contract

- `/api/ready.checks.db_quick_check_ok` must remain `true` when DB quick-check readiness is enabled.
- `/api/ready.last_reconcile_error` must be `null` after a clean startup/restart.
- `/api/ready.last_reconcile_summary` must surface the most recent repair outcome after startup full resync.
- `/api/risk/status` must surface entry-block counters for kill switch, drawdown, alpha freeze, degraded mode, circuit breaker cooldown, and stale L2.
- `/api/metrics` must export the same counters for alerting.
- Restart-time fill-ledger repairs must leave an `order_outbox` audit trail with stage `restart_fill_replay`.

## Backup / Restore contract

Verified backups preserve schema version, SQLite integrity and row-count parity
for all fifteen operational tables: `meta`, `trades`, `order_outbox`, `trade_fills`,
`analysis_snapshots`, `idempotency_cache`, `live_orders`, `live_matches`,
`maker_orders`, `maker_matches`, `maker_match_allocations`, `maker_market_controls`,
`paper_maker_orders`, `paper_maker_fill_events` and `realized_pnl_events`.

The CLI validates counts in one read snapshot and atomically publishes a verified
copy. It refuses source/destination aliases, unknown schema versions and active
destination sidecars. The API requires a new filename and reserves the database
writer during its copy to keep verification counts consistent. It stages a private file and publishes it without replacing an existing destination only after validation. These copies
include all tables, not just the verification list.

Tests populate every listed table, preserve uncertain intents, partial fills,
remaining reservations, drawdown reset peaks and manual stops, and restore positions after a real
child-process termination. Mock-venue maker recovery also restores an actual
backup and proves that earlier receipts are not counted or submitted again.
Follow [recovery-runbook.md](recovery-runbook.md) for private evidence export,
terminal-order rechecks, and the limitations of restoring an older funded ledger.

## Verification commands

```bash
cargo test --all-targets --all-features full_resync_
cargo test --all-targets --all-features runtime_slo_
cargo test new_replays_fill_ledger_into_open_live_trade_on_restart -- --test-threads=1
```

Expected:
- full resync drift test passes and confirms fail-closed trip on contract breach.
- clean full resync test passes with kill switch remaining off.
- runtime SLO tests pass for both sustained L2 and backpressure-triggered trips.
- restart fill-ledger replay test passes and proves open live trades are repaired before serving.
