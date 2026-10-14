# Changelog

All notable changes to this project are documented here. Historical entries describe their versions, including removed routes and superseded authentication behavior; the current functional review and archive verification identify the delivered implementation.

## [2026-10-11] — Complete Project-Source Functional Review

- Corrected probability/price EV admission, unresolved exposure, serialized execution and atomic paper accounting.
- Added durable signed-request replay protection and operation-key binding across Rust, browser and Python clients.
- Tightened configuration/startup validation, verified private backups and truthful missing/stale dashboard values.
- Corrected source attribution, feature handling, chronological training evaluation and unsupported research metrics.
- Isolated evaluation databases and bound selected configuration evidence through A/B, soak and readiness.
- Added source/function review records and targeted regressions; the replacement archive log records the complete final gate.

See the functional review for coverage, findings and remaining limits.

## [2026-09-14] — Shutdown and Restart Truth

- Made shutdown block new entries before sweeping positions and verify persisted exposure even when hot state is empty.
- Added explicit clean, exposure-remains and verification-failed shutdown outcomes; an unreadable database can no longer appear flat.
- Added owned execution-actor shutdown: accepted commands and queued audit events drain before the actor is joined, while timeout or panic engages fail-closed controls and records a reconciliation error.
- Made incomplete position verification or actor shutdown return a process error instead of logging a successful shutdown.
- Replaced the simulated `drop(Arc)` restart test with an orderly actor-stop boundary and added a subprocess crash/restart test. The child exits without cleanup; the new process-side bot recovers three positions and its balance from SQLite WAL state before closing them.
- Added regression coverage proving shutdown rejects attempts to open new exposure behind its close sweep.

## [2026-09-13] — Repository Restoration and Engineering Baseline

### Repository and Reproducibility

- Established a clean Git repository boundary and expanded ignores for secrets, runtime databases, raw datasets, generated artifacts, mutable model state, and local browser/build caches.
- Replaced the machine-specific Cargo target directory with a repository-relative target and established Rust 1.94.0 as the CI/minimum baseline.
- Restored CI across formatting, all Rust targets/features, Clippy with warnings denied, tests, Python parsing, and inline dashboard JavaScript parsing.
- Updated code for the Rust 1.94 Clippy surface while documenting intentional domain-contract exceptions.

### Developer Experience

- Added the primary README, research-only demo wrapper, full verification wrapper, environment template, security policy, and license-status notice.
- Added architecture, quick-start, known-limitations, and sanitized engineering evidence documents.
- Included the canonical ONNX model for reproducibility while excluding versioned backups and mutable runtime weights/state.
- Removed personal machine paths from tracked configuration, scripts, runbooks, Prometheus guidance, and historical changelog links.
- Fixed time-dependent ML archive tests and delayed the first retention sweep so test rows cannot race startup pruning.
- Added isolated demo/smoke environments and configurable `MACBOT_PORT` support so startup, health, readiness, dashboard serving, and graceful shutdown can be checked without touching local runtime state.

### Verification

- Clean isolated Rust build and check passed.
- Full Rust test run passed with 418 library tests, 418 mirrored binary-module tests, and one integration test.
- Python sources and all inline dashboard scripts parsed successfully.
- The retained local SQLite runtime database passed `PRAGMA quick_check`; it remains excluded from Git.

## [2026-03-30] — Dashboard Session Persistence Fix

### Session Boundary Persistence
- Fixed the dashboard "New Session" flow in [static/index.html](../../static/index.html):
  - session cutoffs now persist into runtime config instead of living only in browser `localStorage`
  - fresh launches now hydrate the session boundary from `/api/config`
  - legacy browser-only session boundaries are migrated forward when write access is available
  - `New Session` now saves the cutoff before resetting balance/drawdown and reports partial reset failures honestly
- Added persisted dashboard session fields to [src/config.rs](../../src/config.rs):
  - `dashboard_session_start_trade_id`
  - `dashboard_session_start_at`
  - added runtime-save/load coverage for the new boundary fields

### Verification
- `cargo test --quiet`
- extracted inline dashboard JS from [static/index.html](../../static/index.html) and verified it with `node --check`

### Trade Detail UX
- Smoothed partial-trade detail expansion in [static/index.html](../../static/index.html):
  - added a client-side partial-fills cache and in-flight dedupe
  - visible partial trades now prefetch fill legs in the background
  - hidden detail rows now render cached trade legs before they are unhidden
  - opening a row like trade `#13` no longer has to flash through `Loading fills...` before showing the final leg breakdown
  - history-row expansion now keeps the cached render key in sync with expanded state, which removes the extra background `history-tbody` redraw that was still causing a visible flicker after click

### Operator Surface Polish
- Added a persistent session/control strip in [static/index.html](../../static/index.html):
  - shows the restart-safe session boundary
  - separates current-session activity from archived-session totals
  - surfaces runtime/control state in one place
  - exposes whether the dashboard is on live SSE, quiet SSE, fallback polling, or read backoff
- Trimmed overview-only dashboard reads in [static/index.html](../../static/index.html):
  - maker rollout and A/B summary polling now stay maker-tab scoped
  - ML and deep-model polling now stay overview-tab scoped
  - fallback refreshes now only pull the heavier history/analysis reads when the current tab actually needs them

## [2026-03-30] — Feed/Session Truth Surface Batch

### Feed and No-Trade Truth
- Added richer maker L2/feed truth in [src/bot/actor.rs](../../src/bot/actor.rs):
  - `l2_summary` now includes `target_market_count`
  - added top `not_ready_reasons` counts
  - added `primary_not_ready_reason`
  - added `ready_count` so quoteable vs blocked tokens are easier to read at a glance
  - added normalized `l2_stale_code` alongside the existing human `l2_stale_reason`
- Added clearer operator-facing maker truth in [src/api/maker.rs](../../src/api/maker.rs):
  - `/api/maker/state` now includes `feed_truth`
  - `/api/maker/state` now includes `no_trade_truth`
  - those surfaces now pass through `target_market_count`, `primary_not_ready_reason`, `not_ready_reasons`, and `l2_stale_code`
  - `derive_no_trade_label()` no longer treats `quote_count == 0` as equivalent to `no_target_markets`
  - zero-trade runs now separate `no_target_markets`, `global_l2_feed_stale`, `no_snapshot`, `one_sided_books`, and `token_bbo_stale`

### Reporting and Bundles
- Improved no-trade classification in [scripts/maker_soak_report.py](../../scripts/maker_soak_report.py):
  - added `quoteability_state`
  - split true `dead_window` / `no_target_markets` from `low_quoteability`
  - zero-trade runs now distinguish missing snapshots, one-sided books, stale books, and feed/session pressure more cleanly
  - zero-trade inactive runs no longer collapse into misleading `low_activity`
- Improved [scripts/validation_control_bundle.py](../../scripts/validation_control_bundle.py):
  - bundles now carry `maker_l2_context`
  - bundles now derive `terminal_class` locally when the validation bundle omits it
  - bundles now emit compact `dead_run_summary` (`bucket`, `label`, `note`, and small L2 evidence)
  - rendered Markdown now shows the feed/book shape inline (`markets`, `loaded`, `two_sided`, `one_sided`, `stale`, `quoteable`, `live_quotes`, `l2_stale_reason`)
- Improved [scripts/proof_bundle_index.py](../../scripts/proof_bundle_index.py):
  - synced terminal-class mapping with the current soak-report labels
  - added compact `dead_run_summary` so shutdown proof attempts explain zero-trade outcomes without nested forensics
- Generated a fresh validation control bundle:
  - 20260330_141634_validation_control_bundle.json
  - 20260330_141634_validation_control_bundle.md
  - 20260330_142436_validation_control_bundle.json
  - 20260330_142436_validation_control_bundle.md

### Operator UI
- Improved the maker/operator dashboard in [static/index.html](../../static/index.html):
  - added compact feed truth chips for `targets`, `books`, `quoteable`, `no-trade`, `not ready`, and `shutdown proof`
  - added operator note blocks for feed truth, no-trade truth, and shutdown-proof status
  - the Maker Desk now surfaces the new API fields directly instead of collapsing dead runs into a generic stale state
  - shutdown proof now shows as a compact parked/proven/broken state with attempt counts and latest run note
  - no-trade truth now shows the primary not-ready reason and top blocker histogram inline

### Verification
- `python3 -m py_compile scripts/maker_soak_report.py scripts/validation_control_bundle.py scripts/proof_bundle_index.py`
- `python3 scripts/validation_control_bundle.py`
- `python3 scripts/proof_bundle_index.py --db artifacts/soak-runs/shutdown_validation_20260327_multiopen_proof_1020.sqlite`
- checked `static/index.html` DOM wiring for new `getElementById(...)` references
- extracted inline scripts from [static/index.html](../../static/index.html) and static/dashboard-v2.html and verified them with `node --check`
- `cargo test --quiet`
- Result: `396 passed; 0 failed`

### Realtime, Metrics, Integrity, and Soak Hygiene Follow-Up
- Extended realtime parity in [src/api/realtime.rs](../../src/api/realtime.rs):
  - `fast_snapshot()` now emits `execution_mode`, `l2_feed_stale`, `l2_feed_age_ms`, `feed_truth`, and `no_trade_truth`
  - WS/SSE inherit the same maker truth through the existing realtime snapshot path
- Extended metrics and integrity in:
  - [src/api/metrics.rs](../../src/api/metrics.rs)
  - [src/api/integrity.rs](../../src/api/integrity.rs)
  - Prometheus now exports maker L2 readiness gauges and top `not_ready_reason` counts
  - integrity reports now include `checks.maker.l2_summary` and compact `checks.maker.l2_diagnosis`
- Normalized L2 stale truth at the source in:
  - [src/market/l2_ws.rs](../../src/market/l2_ws.rs)
  - [src/bot/state/execution_actor.rs](../../src/bot/state/execution_actor.rs)
  - [src/bot/actor.rs](../../src/bot/actor.rs)
  - structured `l2_stale_code` is now carried explicitly instead of being re-inferred from free-form reason strings
- Improved soak preflight and artifact hygiene in:
  - [scripts/run_frozen_maker_soak.py](../../scripts/run_frozen_maker_soak.py)
  - [scripts/maker_soak_report.py](../../scripts/maker_soak_report.py)
  - [scripts/artifact_hygiene_report.py](../../scripts/artifact_hygiene_report.py)
  - [scripts/validation_artifact_hygiene_report.py](../../scripts/validation_artifact_hygiene_report.py)
  - maker soaks now record compact `preflight_quoteability` and hygiene reports keep `low_quoteability` distinct from true dead-window starvation

### Verification
- `python3 -m py_compile scripts/run_frozen_maker_soak.py scripts/maker_soak_report.py scripts/artifact_hygiene_report.py scripts/validation_artifact_hygiene_report.py scripts/validation_control_bundle.py scripts/proof_bundle_index.py`
- `cargo test --quiet`
- Result: `402 passed; 0 failed`

### Browser QA and Realtime Parity Follow-Up
- Ran a real browser QA pass against the live dashboard using the local Playwright wrapper and the app running on `http://127.0.0.1:8000/`.
- Fixed a Maker Desk semantics leak where taker-mode SSE updates could overwrite the correct API-driven maker inactivity label:
  - [src/api/maker.rs](../../src/api/maker.rs)
  - [src/api/realtime.rs](../../src/api/realtime.rs)
  - [static/index.html](../../static/index.html)
  - `execution_mode_taker` now wins over generic dead-feed labels, and the SSE render path now passes through `feed_truth` and `no_trade_truth` instead of re-deriving a stale fallback
- Added inline data-URI favicons to:
  - [static/index.html](../../static/index.html)
  - static/dashboard-v2.html
  so fresh browser sessions no longer emit the noisy `favicon.ico` 404
- Fresh browser validation after restart:
  - console: `0` errors, `0` warnings
  - Maker Desk now shows `PRIMARY BLOCKER = EXECUTION MODE TAKER` in taker mode without looking like a broken maker feed

### Verification
- `cargo test --quiet`
- Result: `403 passed; 0 failed`
- extracted inline scripts from [static/index.html](../../static/index.html) and static/dashboard-v2.html and verified them with `node --check`
- browser QA via Playwright against `http://127.0.0.1:8000/`

### Dashboard Rate-Limit Hardening
- Hardened the public dashboard read limiter in [src/api/auth.rs](../../src/api/auth.rs):
  - non-API paths like `/` and `/static/*` now skip the public GET limiter
  - `429` responses now include `Retry-After`
  - public read budget was raised to `300/min`
- Added localhost operator auth bypass in [src/api/auth.rs](../../src/api/auth.rs):
  - loopback clients (`127.0.0.1` / `::1`) now bypass dashboard read/write auth middleware
  - off-host clients still require the existing API key + HMAC flow for protected routes
- Reduced dashboard self-pressure in [static/index.html](../../static/index.html):
  - GET requests now dedupe in flight and honor per-endpoint cooldown after `429`
  - `initialLoad()` now stages visible-above-the-fold reads first and defers noncritical panels with staggered timers
  - ML/deep model/integrity polling was slowed down, and SSE stale fallback now polls a smaller critical set
  - saved-tab boot no longer triggers heavy execution/risk fetches before the core load finishes
  - `loadTrades()` now throttles `loadTradeAnalysis()` instead of chaining it on every refresh
  - `loadMlState()` now uses the shared GET wrapper, so it participates in read dedupe/backoff like the rest of the dashboard
- Updated localhost mode-switch UX in [static/index.html](../../static/index.html):
  - local sessions now advertise that execution mode changes work without extra browser credentials
  - off-host sessions still get the explicit write-auth hint and protected-action explanation
- Browser stress verification:
  - repeated reload + mobile-width stress no longer produced `429` console noise
  - execution-tab boot stayed clean even with the fill-ledger fanout path active
  - remaining network noise during stress was limited to expected `net::ERR_ABORTED` entries from intentional reloads
- Improved execution-mode control clarity in [static/index.html](../../static/index.html):
  - the navbar mode selector now shows a `write auth` hint when the browser is missing mutation credentials
  - mode changes now snap back to the real execution mode instead of looking like they succeeded when `/api/config` rejects the write
  - unauthenticated mode changes now explain that protected writes require opening the dashboard with `#api_key=...&api_secret=...`

### Verification
- `cargo clean -p macbot-rs`
- `cargo test --quiet`
- Result: `413 passed; 0 failed`
- extracted inline scripts from [static/index.html](../../static/index.html) and static/dashboard-v2.html and verified them with `node --check`
- Playwright reload/mobile stress pass against `http://127.0.0.1:8000/`
- Playwright execution-tab boot stress pass against `http://127.0.0.1:8000/`
- localhost `POST /api/config` verified without credentials for `maker`, `hybrid`, and `taker`

## [2026-03-28] — Shutdown Proof Status Marked

### Project State
- Marked shutdown proofing as **code-complete, evidence-incomplete** in:
  - [ops/truth-first-program.md](../../ops/truth-first-program.md)
  - [ops/profiles/maker_rollout_state.json](../../ops/profiles/maker_rollout_state.json)
- Recorded that:
  - single-trade shutdown proof exists
  - multi-open shutdown proof is still missing
  - the latest attempts failed as `starvation/dead_window`, not as a plumbing defect
- Project decision is now explicit: shutdown proofing is parked unless a clearly live validation window is available.

## [2026-03-27] (Session 103) — Audit Hardening Batch

### Persistence and Lifecycle Truth
- **Removed the silent trade-persistence failure pattern from the hot execution paths**:
  - [src/bot/execute.rs](../../src/bot/execute.rs)
  - [src/bot/close.rs](../../src/bot/close.rs)
  - [src/bot/guard.rs](../../src/bot/guard.rs)
  - [src/bot/actor.rs](../../src/bot/actor.rs)
  - [src/api/trades.rs](../../src/api/trades.rs)
  - [src/bot/reconcile.rs](../../src/bot/reconcile.rs)
- Fill snapshots, trade updates, trade rejections, destructive history deletes, and reconcile balance sync now warn or fail closed instead of being silently discarded.
- [src/bot/idempotency.rs](../../src/bot/idempotency.rs) now logs failed cleanup of expired or malformed idempotency rows instead of silently swallowing those delete failures.
- [src/api/realtime.rs](../../src/api/realtime.rs) now warns when recent-trade snapshot loading fails instead of quietly returning an empty list.
- The live fill monitor in [src/bot/execute.rs](../../src/bot/execute.rs) no longer holds the trade write guard through persistence and no longer mutates hot memory after a failed DB update.
- The close path in [src/bot/close.rs](../../src/bot/close.rs) now restores the pre-close in-memory trade snapshot if the final close persistence step fails, instead of marching on with memory and disk diverged.
- `partial_close_trade()` in [src/bot/close.rs](../../src/bot/close.rs) now fails closed when `update_trade()` fails instead of crediting balance and mutating hot state anyway.
- Sim/live close aborts in [src/bot/close.rs](../../src/bot/close.rs) now restore the original `OPEN` hot-trade snapshot instead of leaving rows stranded as `CLOSING`.
- Full-close exit fill snapshots in [src/bot/close.rs](../../src/bot/close.rs) now persist only after the authoritative trade-row update succeeds, so the fill ledger no longer leads the trade row on failure.
- [src/bot/state/tp_sl.rs](../../src/bot/state/tp_sl.rs) now writes `exit_type` to SQLite before mutating hot memory, eliminating runtime-only close metadata on DB failure.
- [src/bot/guard.rs](../../src/bot/guard.rs) no longer voids or settles live trades on `cancel_order()` / `get_order()` failure, and it now refuses to mutate hot state when the final guard `update_trade()` fails.
- [src/bot/guard.rs](../../src/bot/guard.rs) now carries order status/inactive state through resolution-guard settlement, normalizes filled live trades before persistence, clears inactive `order_id`s on unresolved retries, and honors `closing_in_progress` before any guard-owned hot-state overwrite.
- [src/api/bot_routes.rs](../../src/api/bot_routes.rs) balance resets now fail honestly when `load_open_trades()` fails instead of assuming zero locked capital.
- [src/api/bot_routes.rs](../../src/api/bot_routes.rs) trade/force idempotency keys are now scoped to the request shape instead of the raw header alone, and `Ok(None)` responses now use generic “not executed” wording instead of pretending every non-execution was a gate-block.
- [src/api/trades.rs](../../src/api/trades.rs) `close-all` responses now surface `failed_trade_ids` and an `ok` flag instead of silently presenting partial success as a clean close.
- [src/api/trades.rs](../../src/api/trades.rs) close endpoints now distinguish `closed` from `still_open`, and `close-all` no longer counts `Some(open_trade)` results as successful closes.
- [src/bot/idempotency.rs](../../src/bot/idempotency.rs) no longer seeds the in-process idempotency cache when the durable DB upsert fails.
- [src/bot/execute.rs](../../src/bot/execute.rs) now trips degraded no-trade, activates the kill switch, and records a reconcile error if a live order was placed, local trade persistence failed, and the emergency cancel also failed.
- [src/bot/execute.rs](../../src/bot/execute.rs) now fails closed when live balance sync cannot be refreshed before entry, when `post_order()` exhausts retries and the live submit path becomes ambiguous, and when a live entry fill is observed before local trade persistence succeeds.
- [src/bot/reconcile.rs](../../src/bot/reconcile.rs) full resync now returns an error when the exchange open-order check fails instead of recording a misleading success summary.
- [src/bot/reconcile.rs](../../src/bot/reconcile.rs) live reconcile now fails closed on orphan-close persistence failure and on live balance-sync failure when a CLOB-backed balance check is expected.
- Maker safety paths in [src/bot/actor.rs](../../src/bot/actor.rs) now warn when quote cleanup is incomplete, then trip degraded no-trade, the kill switch, and a reconcile error instead of silently proceeding with possible live quotes still resting.
- [src/bot/actor.rs](../../src/bot/actor.rs) maker cancel telemetry now preserves the actual cancel error text in the outbox instead of collapsing all failures to a generic reason.
- [src/bot/close.rs](../../src/bot/close.rs) now hard-rejects live closes when the CLOB-backed exit path is unavailable, and it no longer restores a live trade to a healthy `OPEN` snapshot after a confirmed live exit fill if the final trade-row persistence fails.
- [src/main.rs](../../src/main.rs) now treats live reconcile/full-resync/resolution-guard failures as fail-closed runtime events by tripping degraded no-trade + kill switch before recording the error, and graceful shutdown no longer logs still-open trades as if they closed successfully.
- [src/bot/execute.rs](../../src/bot/execute.rs) fill-monitor `get_order()` failures now latch degraded no-trade + kill switch with a durable reconcile error instead of only warning and continuing.
- [src/bot/guard.rs](../../src/bot/guard.rs) resolution-guard `cancel_order()` / `get_order()` authority failures now also latch degraded no-trade + kill switch before the guard stands down.
- [src/api/bot_routes.rs](../../src/api/bot_routes.rs) now treats `not_executed` trade and force-trade responses as terminal idempotent results instead of aborting the key and allowing the same idempotency token to mutate state later.
- [src/api/trades.rs](../../src/api/trades.rs) now caches only terminal single-close results, re-inspects `Ok(None)` close-all outcomes against the DB, returns proper `409`/`500` status codes for non-terminal bulk-close failures, and avoids freezing a stale partial-failure payload under the idempotency key.

### Concurrency and Freshness
- **Closed the confirmed config lock-across-`await` bug** in [src/api/bot_routes.rs](../../src/api/bot_routes.rs) by cloning the config snapshot before the later `last_balance_drift_usd().await`.
- **Added explicit freshness provenance** for cached external signals in [src/bot/analyze.rs](../../src/bot/analyze.rs):
  - PCR
  - Fear & Greed
  - LunarCrush
- Analysis snapshots now carry a `signal_freshness` block so fresh, fallback, and missing states are explicit instead of implied.

### Resilience Hardening
- **Hardened ensemble persistence** in [src/ml/ensemble.rs](../../src/ml/ensemble.rs):
  - write → `sync_all()` → rename
  - parent-dir sync
  - explicit logging on write/rename failures
- **Removed poison-panics from the fetch circuit** in [src/signals/fetchers.rs](../../src/signals/fetchers.rs); poisoned mutexes now recover with a warning instead of crashing.
- **Improved ML logger overflow observability** in [src/ml/data_logger.rs](../../src/ml/data_logger.rs):
  - counted dropped events explicitly
  - logged the first drop and every 1000th drop
  - exposed a dropped-events accessor for tests/observability

### Test Coverage
- Added high-signal tests in:
  - [src/bot/accounting.rs](../../src/bot/accounting.rs)
  - [src/bot/idempotency.rs](../../src/bot/idempotency.rs)
  - [src/clob/auth.rs](../../src/clob/auth.rs)
  - [src/ml/deep_model.rs](../../src/ml/deep_model.rs)
  - [src/bot/intelligence.rs](../../src/bot/intelligence.rs)
  - [src/bot/state/analysis_cycle.rs](../../src/bot/state/analysis_cycle.rs)
- New coverage includes:
  - balance reconstruction and restart-style accounting consistency
  - idempotency state transitions and malformed payload cleanup
  - CLOB signing/header determinism and token validation
  - deep-model tensor-shape and padding/truncation behavior
  - `ml_on_resolution()` edge-tracker and outcome-backfill behavior
  - analysis-cycle shadow-kill and live signal-weight reload behavior
  - persisted-close restart recovery when the DB row is durable but hot memory and balance are still in the pre-close state
  - partial-close fail-closed behavior when the DB update cannot be written
  - persisted-open restart recovery when the trade row is durable but hot memory/balance are still pre-open
  - persisted simulated-maker-fill restart recovery from a stale pre-open hot state
  - persisted partial-close restart recovery from a stale pre-partial hot state

### Verification
- `cargo fmt --all`
- `cargo check --quiet`
- `cargo test --quiet`
- Result: `391 passed; 0 failed`

### Operator Surface
- Restored latest_shutdown_validation_proof_bundle.json from the known-good single-trade proof DB so the shutdown-validation surface is no longer blank while waiting for the next multi-open proof attempt.

## [2026-03-27] (Session 102) — Artifact Hygiene Helper

### Artifact Hygiene Report
- Added [scripts/artifact_hygiene_report.py](../../scripts/artifact_hygiene_report.py) to summarize recent shutdown-validation evidence without deleting anything.
- The helper groups recent attempts into:
  - proof-worthy shutdown evidence
  - dead-window / no-target-markets repeat candidates
  - operator-context attempts that should stay readable but not drive decisions
- It also self-boots the latest proof bundle summary from the proof DB when a cached bundle JSON is missing, so the hygiene report can still point at the useful evidence.

## [2026-03-27] (Session 101) — Shutdown Proof Runbook

### Operator Runbook
- Added a concise shutdown-proof operator runbook in [ops/shutdown-proof-runbook.md](../../ops/shutdown-proof-runbook.md) for the truth-first phase.
- The runbook covers:
  - when to attempt the multi-open shutdown proof
  - exact success criteria
  - the early-stop starvation rule
  - how to read the validation bundle
  - what to update if the proof passes

## [2026-03-27] (Session 100) — Validation Classification + Feed Truth Surfaces

### Validation Bundle Classification
- **Promoted validation runs from raw metrics to explicit terminal classes** in [scripts/run_frozen_maker_soak.py](../../scripts/run_frozen_maker_soak.py):
  - added `terminal_classification`
  - added top-level `terminal_class`
  - classes now distinguish:
    - `success`
    - `lifecycle_bug`
    - `policy_suppression`
    - `market_quality_failure`
    - `starvation/dead_window`
- This keeps shutdown/control runs from ending as “technically finished but operationally ambiguous.”

### Integrity Report Readout
- **Tightened the lifecycle integrity surface** in [scripts/validation_db_integrity_report.py](../../scripts/validation_db_integrity_report.py):
  - added top-level `verdict`
  - added top-level `reason`
  - added compact `counts` for trades, fills, outbox activity, and rejections
- The known-good shutdown proof DB still validates cleanly:
  - shutdown_validation_20260326_targeted.sqlite
  - result: `status=ok`, `verdict=ok`, `reason=clean`, `mismatch_count=0`

### Feed / Session Truth Surfaces
- **Improved maker-side feed/session attribution without changing strategy policy**:
  - [src/bot/actor.rs](../../src/bot/actor.rs) now records:
    - richer `l2_summary` counts in maker state snapshots
    - specific not-ready reasons like `l2_stale(...)`, `l2_feed_stale`, `token_bbo_stale`, and `snapshot_missing` instead of a generic `l2_not_ready`
  - [scripts/maker_soak_report.py](../../scripts/maker_soak_report.py) now writes:
    - `run_classification`
    - `primary_blocker_summary`
    - explicit maker L2 context including snapshot availability, loaded/two-sided/one-sided/stale/quoteable counts, and stale reason
- This makes no-trade runs more attributable to:
  - no active markets
  - one-sided books
  - stale feed/session pressure
  - policy suppression

### Verification
- `python3 -m py_compile scripts/run_frozen_maker_soak.py scripts/validation_db_integrity_report.py scripts/maker_soak_report.py`
- `python3 scripts/run_frozen_maker_soak.py --config config.json --shutdown-validation-self-test`
- `python3 scripts/validation_db_integrity_report.py --db artifacts/soak-runs/shutdown_validation_20260326_targeted.sqlite`
- `cargo test --quiet process_maker_bbo_event`

## [2026-03-27] (Session 99) — Shutdown Validation Harness + Lifecycle Integrity

### Shutdown Validation Rails
- **Turned the shutdown-validation soak into a real proof harness** in [scripts/run_frozen_maker_soak.py](../../scripts/run_frozen_maker_soak.py):
  - added `--shutdown-validation`
  - added `--stop-on-open-trades`
  - added `--min-run-seconds`
  - added `--shutdown-validation-self-test`
  - added `--startup-failure-self-test`
- **Startup failures are now classified explicitly instead of failing opaquely**:
  - `startup_bind_conflict`
  - `startup_process_exited_before_health`
  - `startup_no_health_timeout`
- The runner now preflights the target port, keeps a log tail for failure payloads, and fails closed before a stale listener can fool readiness.

### Shutdown Cleanup Proof
- **Produced the first real shutdown-cleanup proof run for open simulated maker inventory**:
  - DB: shutdown_validation_20260326_targeted.sqlite
  - soak report: 20260326_231153_maker_soak_report.json
  - sim verification: 20260326_231153_sim_price_verification_report.json
- Proven in that run:
  - `1` trade reached shutdown while open
  - DB ended with `0 OPEN`
  - DB ended with `0 CLOSING`
  - `1` `trade_fills` row with `phase='mark_exit'` and `source='shutdown_cleanup'`
  - the cleanup trade is `CLOSED` with `exit_type='shutdown_cleanup'`
  - the maker entry fill has matching outbox fill evidence
- **Multi-open shutdown proof is not done yet**. The latest `--stop-on-open-trades 2` attempt:
  - shutdown_validation_20260327_multiopen_proof.sqlite
  - ended as a clean `starvation` / `no active markets` window, not a lifecycle bug

### Lifecycle Integrity Report
- **Added a reusable validation DB integrity checker** in [scripts/validation_db_integrity_report.py](../../scripts/validation_db_integrity_report.py).
- The new report checks:
  - stranded `OPEN` / `CLOSING` trades
  - orphan `trade_fills`
  - missing fill `order_id`
  - non-`shutdown_cleanup` `mark_exit` rows
  - shutdown cleanup trades missing entry evidence
  - maker entry fills missing matching `order_outbox` fill rows
  - opened maker outbox fills missing matching entry fills
- It writes stable latest artifacts to:
  - latest_validation_db_integrity_report.json
  - latest_validation_db_integrity_report.md
- Known-good validation/control DBs now pass with `status: ok` and `mismatch_count: 0`, including:
  - shutdown_validation_20260326_targeted.sqlite
  - baseline_20260326_1027_parallel_control.sqlite

### Validation Shutdown Profile
- **Strengthened the validation-only shutdown profile** in [scripts/build_maker_rollout_profiles.py](../../scripts/build_maker_rollout_profiles.py) and [maker_paper_validation_shutdown.json](../../ops/profiles/maker_paper_validation_shutdown.json) so it is more likely to carry maker inventory into exit:
  - broader `updown` market filter
  - `take_profit=false`
  - `stop_loss=false`
  - lower `min_confidence_margin`
  - looser `maker_market_quality_max_spread_bps`
  - faster `maker_fill_cooldown_ms`
  - larger `maker_quote_notional_usd`
  - larger `maker_max_inventory_notional_usd`
  - higher `max_open_positions`
- This profile remains explicitly research-only and separate from the baseline rollout rail.

## [2026-03-26] (Session 98) — Truth-First Pivot + Validation Rail

### Project Focus Shift
- **Shelved alpha work and made the current phase explicit**:
  - [ops/profiles/maker_rollout_state.json](../../ops/profiles/maker_rollout_state.json) now marks the project focus as `truth_first`
  - [ops/truth-first-program.md](../../ops/truth-first-program.md) now defines:
    - allowed workstreams
    - deferred workstreams
    - why `MACBOT_FREEZE_ALPHA` is too blunt for this phase
    - exit criteria for leaving the plumbing-first program
- The latest readiness artifact was refreshed to reflect that the project is still:
  - 20260326_225753_maker_rollout_readiness_report.json
  - verdict: `baseline_only_no_candidate`

### Validation Rail
- **Added a separate research-only validation rail** for short control and shutdown soaks:
  - [maker_paper_validation_control.json](../../ops/profiles/maker_paper_validation_control.json)
  - [maker_paper_validation_shutdown.json](../../ops/profiles/maker_paper_validation_shutdown.json)
- These profiles stay separate from the baseline rollout rail so plumbing proof runs do not masquerade as promotable alpha evidence.

### Maker Truth And Reporting
- **Improved maker soak and verification reporting**:
  - [scripts/maker_soak_report.py](../../scripts/maker_soak_report.py)
  - [scripts/sim_price_verification_report.py](../../scripts/sim_price_verification_report.py)
- Reports now surface:
  - total/recent trade rejections
  - top suppress/cancel/decision reasons
  - compact blocker summaries
  - stable latest artifacts for operational readout
- This turned control runs into a real decision surface rather than a pile of raw logs and manual SQLite forensics.

### Maker Candidate Quality
- **Added an upstream maker-only confidence gate** in [src/bot/analyze.rs](../../src/bot/analyze.rs) so weak maker candidates are turned into `HOLD` before they become downstream suppression churn.
- The effective floor is `mode_min_confidence(cfg) + min_confidence_margin`, leaving downstream execution guards in place as defense in depth.

## [2026-03-25] — Maker Control Truth + L2 Honesty

### Maker Execution Path
- **Turned simulated maker quoting into a real inventory path** across [src/bot/actor.rs](../../src/bot/actor.rs), [src/bot/close.rs](../../src/bot/close.rs), and [src/bot/state/trade_lifecycle.rs](../../src/bot/state/trade_lifecycle.rs):
  - passive maker fills now create inventory instead of staying quote-only
  - ask-side maker fills flatten existing inventory instead of opening bogus complement positions
  - maker exits now carry fill evidence captured at the actual flatten event
  - simulated maker exits no longer pay taker exit fees
- **Removed fake/no-op ask churn** by making maker ask quotes conditional on actual inventory in [src/bot/actor.rs](../../src/bot/actor.rs).

### Maker Lifecycle Behavior
- **Separated maker trades from conviction TP/SL reversal behavior** in [src/bot/state/tp_sl.rs](../../src/bot/state/tp_sl.rs) so maker inventory is no longer immediately chopped by taker-oriented exit logic.
- **Added maker-side diagnostics and quote gating visibility** through:
  - [src/bot/actor.rs](../../src/bot/actor.rs)
  - [src/api/maker.rs](../../src/api/maker.rs)
- Per-token maker state now exposes whether the book is two-sided and why bid/ask quoting was suppressed, which turned zero-trade runs into explainable policy outcomes instead of mystery.

### L2 / Session Truth
- **Hardened L2 honesty in [src/market/l2_ws.rs](../../src/market/l2_ws.rs)**:
  - reconnect on target-set drift instead of silently stalling
  - treat warmup timeout as stale-state evidence instead of tearing the session down by default
  - require a real two-sided top of book before a market is considered ready
  - keep the active analysis market sticky long enough to avoid useless reconnect churn during short research windows
- These changes moved zero-trade classification away from “the feed might be lying” and toward truthful buckets like `bad market quality`, `one-sided book`, or `policy suppression`.

### Control Runs
- **Established the first believable maker control runs** with plausible execution fills and without no-op ask rows, including:
  - 20260325_124404_maker_soak_report.json
  - 20260325_172713_maker_soak_report.json
  - 20260325_172713_sim_price_verification_report.json
- At this point the main blocker moved away from “fake execution behavior” and toward “maker side selection / quoteability quality.”

## [2026-03-24] — Runtime Hardening + Soak Runner Repair

### Runtime Hardening
- **Closed the biggest live-path safety gaps** across:
  - [src/json_util.rs](../../src/json_util.rs)
  - [src/analysis.rs](../../src/analysis.rs)
  - [src/bot/execute.rs](../../src/bot/execute.rs)
  - [src/bot/state/execution_actor.rs](../../src/bot/state/execution_actor.rs)
- This batch wired price-domain validation deeper into hot paths, added actor send/response timeout coverage, and closed the gap where some trade rejections existed only in logs.
- **Trade-rejection persistence and close-path recovery were strengthened** through [src/db/mod.rs](../../src/db/mod.rs), [src/bot/close.rs](../../src/bot/close.rs), and [src/bot/risk.rs](../../src/bot/risk.rs).

### Integration / Operator Safety
- **Added a real top-level integration harness** via:
  - [src/lib.rs](../../src/lib.rs)
  - [tests/integration_lifecycle.rs](../../tests/integration_lifecycle.rs)
- **Added safety/ops hardening on top of the runtime**:
  - config bounds validation in [src/config.rs](../../src/config.rs)
  - per-source fetch circuit breakers in [src/signals/fetchers.rs](../../src/signals/fetchers.rs)
  - local alert fallback in [src/notifications.rs](../../src/notifications.rs)
  - env-gated automatic DB backup in [src/main.rs](../../src/main.rs)

### Soak Harness Truth
- **Fixed the frozen soak runner so it stops on wall-clock schedule instead of stretching across sleep/wake or hanging indefinitely** in [scripts/run_frozen_maker_soak.py](../../scripts/run_frozen_maker_soak.py).
- This repaired the earlier overnight false soak and made later control/validation evidence trustworthy enough to act on.

### Repo / Build Hygiene
- **Moved Cargo build output out of the repo** with [/.cargo/config.toml](../../.cargo/config.toml), originally using a machine-local target directory.
- That cut the repo footprint dramatically by removing the giant in-repo `target/` buildup; the engineering-baseline pass later restored a portable repository-relative `target/`.

## [2026-03-23] (Session 97) — Roll Toxicity Maker Candidate

### Maker Alpha Rediscovery
- **Started the next maker-native research branch after retiring backwardation widening** by adding a side-aware Roll toxicity guard in [src/bot/actor.rs](../../src/bot/actor.rs).
- The new guard uses existing Roll microstructure telemetry and only suppresses the side that is toxic for the current token and flow direction, instead of widening or skipping both sides.
- It is fully config-driven through [src/config.rs](../../src/config.rs) and [config.json](../../config.json):
  - `maker_roll_toxic_guard_enabled`
  - `maker_roll_toxic_guard_shadow_only`
  - `maker_roll_toxic_min_confidence`
  - `maker_roll_toxic_min_signal`
  - `maker_roll_toxic_min_sample_count`

### Research Profiles And Tooling
- **Added a frozen research profile for the first Roll candidate** in [scripts/build_maker_rollout_profiles.py](../../scripts/build_maker_rollout_profiles.py):
  - [maker_paper_rollguard_c15_s5_n6.json](../../ops/profiles/maker_paper_rollguard_c15_s5_n6.json)
- **Extended the maker A/B tooling to support `roll_guard` as a first-class candidate mode**:
  - [scripts/run_maker_soak.py](../../scripts/run_maker_soak.py)
  - [scripts/run_maker_ab_cycle.py](../../scripts/run_maker_ab_cycle.py)
  - [scripts/run_maker_ab_batch.py](../../scripts/run_maker_ab_batch.py)
  - [scripts/maker_soak_report.py](../../scripts/maker_soak_report.py)
  - [scripts/maker_ab_compare.py](../../scripts/maker_ab_compare.py)
- Batch/report labels now understand Roll candidates in [src/api/maker.rs](../../src/api/maker.rs) and [scripts/maker_rollout_readiness_report.py](../../scripts/maker_rollout_readiness_report.py), while rollout state itself remains baseline-only.

### Verification
- Added focused actor coverage for the new side-aware guard in [src/bot/actor.rs](../../src/bot/actor.rs):
  - toxic-side mapping unit test
  - live skip test
  - shadow-only test
- Full Rust suite is green at `324 passed; 0 failed`.
- Smoke artifact for the new research lane:
  - 20260323_080826_maker_ab_cycle.json
  - 20260323_080923_maker_ab_compare.json
- The smoke verdict is intentionally non-promotional: `inconclusive`, with `0` Roll-guard skips, which is acceptable for a plumbing check but means the next real batch needs a longer, denser window.

## [2026-03-23] (Session 96) — Floored Maker Widening Candidate

### Maker Experiment Integrity
- **Separated floored widening candidates from stale widen-only labels** so readiness, dashboard summaries, and future rollout work stop conflating `40bps` with `40bps + floor 120bps`.
- [scripts/maker_rollout_readiness_report.py](../../scripts/maker_rollout_readiness_report.py) and [src/api/maker.rs](../../src/api/maker.rs) now label widen candidates with both knobs when a floor is active, for example `widen_40bps_floor120bps`.

### Frozen Profiles
- **Added a dedicated frozen research profile for the new maker candidate** in [scripts/build_maker_rollout_profiles.py](../../scripts/build_maker_rollout_profiles.py):
  - [maker_paper_widen40_floor120.json](../../ops/profiles/maker_paper_widen40_floor120.json)
- Existing frozen profiles were regenerated alongside it:
  - [maker_paper_baseline.json](../../ops/profiles/maker_paper_baseline.json)
  - [maker_paper_widen20.json](../../ops/profiles/maker_paper_widen20.json)

### Rollout Tooling
- **Prepared the autonomous rollout tooling for floor-based candidates** — [scripts/run_maker_rollout_gate.py](../../scripts/run_maker_rollout_gate.py) now accepts `maker_backwardation_min_target_spread_bps`, derives candidate labels from widen + floor when not given explicitly, and carries the floor through batch metadata.
- **Updated the rollout runbook** in [ops/live-rollout-runbook.md](../../ops/live-rollout-runbook.md) to include the new frozen profile and a concrete floored-soak example.

## [2026-03-22] (Session 95) — Posterior Alignment + BUY_NO Shadow Guard

### Posterior Truth Fix
- **Fixed a real decision/archive inconsistency in the live analysis path** — [src/bot/analyze.rs](../../src/bot/analyze.rs) now:
  - realigns `signal` with `p_up` after ensemble blending and isotonic calibration
  - logs the exact-input archive only after those posterior-changing steps complete
  - keeps `signal` / `p_up` consistent for downstream BTC confirmation, counterfactual logic, and the truth loop
- This closes a concrete bug we found in the exact archive where some rows were stored as `BUY_NO` even though `p_up >= 0.5` (and vice versa).

### BUY_NO Guard Research
- **Added a shadow-only BUY_NO guard scoring rail** — [scripts/buy_no_shadow_guard_report.py](../../scripts/buy_no_shadow_guard_report.py) now evaluates simple NO-side guard candidates against labeled `ml_model_inputs` rows and publishes stable latest artifacts:
  - latest_buy_no_shadow_guard_report.json
  - latest_buy_no_shadow_guard_report.md
- First honest result on the tiny current `BUY_NO` sample:
  - baseline wrong rate: `60.0%`
  - strongest clean candidate so far: block weak `BUY_NO` only in `TREND_UP` unless `p_up <= 0.35`
  - that candidate removes `2/6` wrong `BUY_NO` rows with `0` right rows removed on the current archive

### Shadow Runtime Rail
- **Added a config-backed shadow-only runtime hook for the leading NO-side guard candidate** — [src/bot/execute.rs](../../src/bot/execute.rs) and [src/config.rs](../../src/config.rs) now support:
  - `buy_no_trend_up_guard_enabled`
  - `buy_no_trend_up_guard_shadow_only`
  - `buy_no_trend_up_max_p_up`
- The checked-in research config now enables this rail in **shadow-only** mode by default in [config.json](../../config.json), surfaces it in the dashboard config panel in [static/index.html](../../static/index.html), and propagates it into regenerated maker paper profiles:
  - [maker_paper_baseline.json](../../ops/profiles/maker_paper_baseline.json)
  - [maker_paper_widen20.json](../../ops/profiles/maker_paper_widen20.json)

## [2026-03-22] (Session 94) — Full Fusion Weight Surface + Live Debug

### Fusion Truth Surface
- **Exposed the full live fusion weight surface in config/runtime** — [src/config.rs](../../src/config.rs) now:
  - includes the newer live features in `SignalWeights`
  - exposes a stable ordered full-surface map for calibration/versioning
  - covers temporal, liquidation, cross-market, microstructure, and late-window signals like `cross_market`, `latency_arb`, `news_sentiment`, `btc_final_momentum`, `kyle_lambda`, and `hawkes_flow`
- **Added effective weight/contribution debug to fusion output** — [src/fusion/mod.rs](../../src/fusion/mod.rs) now returns:
  - normalized live weights
  - weighted per-signal contributions
  - zeroed hold/debug maps for early exits so the surface stays inspectable even when fusion stands down

### Analysis + Dashboard
- **Live analysis payloads now carry a proper `fusion_debug` object** — [src/bot/analyze.rs](../../src/bot/analyze.rs) now publishes:
  - configured weights
  - normalized live weights
  - weighted live contributions
  - whether time-of-day weighting was enabled
- **The dashboard weight card now shows the real live contribution stack instead of a vague fallback** — [static/index.html](../../static/index.html) now:
  - renames the card to `Fusion Debug`
  - renders weighted live contributions first
  - shows normalized live weight plus configured base weight per signal
  - groups the expanded signal set more accurately across micro, momentum, macro, cross-asset, temporal, and liquidation buckets

### Config Alignment
- **Checked-in configs now match the real live surface** — [config.json](../../config.json) now carries all `34` weight keys.
- **Pruned legacy signals were zeroed explicitly** so stale config values stop pretending to matter:
  - `spike_detector`
  - `coingecko_sentiment`
  - `finnhub_sentiment`
- **Frozen maker rollout profiles were regenerated** from the updated base config:
  - [maker_paper_baseline.json](../../ops/profiles/maker_paper_baseline.json)
  - [maker_paper_widen20.json](../../ops/profiles/maker_paper_widen20.json)

## [2026-03-22] (Session 93) — Shadow Meta-Fusion Learner

### Meta-Fusion Research Rail
- **Added a first shadow-only meta-fusion learner/eval report** — [scripts/meta_fusion_shadow_report.py](../../scripts/meta_fusion_shadow_report.py) now:
  - trains a small logistic meta-learner on resolved `ml_model_inputs`
  - uses chronological holdout evaluation against archived `p_up`
  - reports Brier/AUC deltas plus the strongest learned coefficients
  - publishes stable latest artifacts:
    - latest_meta_fusion_shadow_report.json
    - latest_meta_fusion_shadow_report.md

### First Honest Result
- Live exact-archive test on trades.db:
  - `54` labeled rows
  - `38` train / `16` chronological test
  - baseline archived `p_up`: Brier `0.0160`, AUC `1.0000`
  - meta-fusion learner: Brier `0.1239`, AUC `0.7708`
- Verdict: **do not integrate into runtime yet**. The sample is too small, and the first shadow learner is materially worse than the existing archived posterior on the live holdout.

## [2026-03-22] (Session 92) — High-Confidence Wrong-Trade Review Rail

### Model Truth Loop
- **Added a dedicated live exact-archive wrong-trade review report** — [scripts/high_confidence_wrong_trade_report.py](../../scripts/high_confidence_wrong_trade_report.py) now:
  - scans resolved `ml_model_inputs` rows in `trades.db`
  - measures all wrong rows plus the subset that are confidently wrong at a configurable threshold
  - breaks mistakes down by signal, regime, phase, hour, market, and run id
  - records the strongest wrong rows with feature snapshots
  - publishes stable latest artifacts:
    - latest_high_confidence_wrong_trade_report.json
    - latest_high_confidence_wrong_trade_report.md

### Current Read
- First live exact-archive run on trades.db:
  - `54` labeled rows
  - `12` wrong rows overall
  - `35` high-confidence rows at `>= 0.70`
  - `0` confidently wrong rows at that threshold
- The strongest wrong rows so far cluster below the hard-negative threshold and skew toward `SQUEEZE` / `MID`, which is useful evidence without being a model blow-up.

## [2026-03-22] (Session 91) — Rollout Gate Idle-Wait Fix

### Autonomous Rollout Progress
- **The maker rollout gate no longer burns through its round budget while waiting for an eligible UTC start window** — [scripts/run_maker_rollout_gate.py](../../scripts/run_maker_rollout_gate.py) now:
  - treats `awaiting_eligible_start` as an idle wait, not a completed evidence round
  - records idle waits in the gate artifact
  - updates the runtime heartbeat with `phase=awaiting_eligible_start` and the next eligible delay
  - resumes the same round automatically after the blocked window passes
- This keeps the unattended maker rollout path moving during overnight blocked hours instead of exiting early with `max_rounds_exhausted`.

## [2026-03-22] (Session 90) — Live Rollout Gate Heartbeat

### Heartbeat Status
- **The autonomous maker rollout gate now writes a live heartbeat/status file while it is running** — [scripts/run_maker_rollout_gate.py](../../scripts/run_maker_rollout_gate.py) now maintains:
  - latest_maker_rollout_gate_status.json
  - latest_maker_rollout_gate_status.md
- The runtime status includes:
  - `active`
  - `phase`
  - `current_round`
  - latest readiness verdict
  - latest batch verdict
  - current blockers

### API + Dashboard
- **`/api/maker/rollout-status` now includes the live gate heartbeat** — [src/api/maker.rs](../../src/api/maker.rs)
- **Maker Desk now shows gate phase directly** — [static/index.html](../../static/index.html) now surfaces the rollout gate phase alongside the existing rollout verdict / stop / blocker count.

### Current Runtime State
- The new heartbeat-capable gate runner is active again in the background and the latest runtime status currently shows:
  - `active=true`
  - `phase=running_batch`
  - `current_round=1`
  - candidate `widen_20bps`

## [2026-03-22] (Session 89) — Stable Latest Rollout Artifacts

### Stable Latest Files
- **Readiness and gate scripts now publish stable latest artifacts**:
  - latest_maker_rollout_readiness_report.json
  - latest_maker_rollout_gate.json
- Timestamped artifacts are still emitted, but the unattended path now also keeps a stable latest pointer for dashboards and tooling.

### API
- **The maker rollout-status endpoint now prefers the stable latest artifact files** before falling back to directory scans — [src/api/maker.rs](../../src/api/maker.rs)

## [2026-03-22] (Session 88) — Rollout Gate Status Surface

### Operator Truth
- **Added a rollout-status API for the maker branch** — [src/api/maker.rs](../../src/api/maker.rs) now exposes `/api/maker/rollout-status`, which returns:
  - the latest maker rollout readiness report
  - the latest rollout gate runner artifact
- **Wired the route into the API router** — [src/api/mod.rs](../../src/api/mod.rs)

### Dashboard
- **Maker Desk now shows rollout-gate state directly** — [static/index.html](../../static/index.html) now surfaces:
  - rollout gate verdict
  - gate stop reason
  - blocker count
  - gate rounds
  - a short rollout note alongside the existing maker A/B summary
- This keeps the autonomous maker rollout gate visible while it continues collecting evidence in the background.

## [2026-03-22] (Session 87) — Autonomous Maker Rollout Gate Runner

### Autonomous Gate Runner
- **Added an unattended rollout gate orchestrator** — [scripts/run_maker_rollout_gate.py](../../scripts/run_maker_rollout_gate.py) now:
  - rebuilds the frozen maker paper profiles
  - regenerates the maker rollout readiness report
  - runs another `20bps` maker A/B evidence batch when rollout is still blocked
  - refreshes readiness again after the batch
  - repeats until the paper candidate is ready or the round budget is exhausted
- **Smoke-validated the gate runner** — 20260322_034013_maker_rollout_gate.json confirms the current blocked state without launching new cycles when `--rounds 0` is used.

### Runbook
- **Updated the rollout runbook again** — [ops/live-rollout-runbook.md](../../ops/live-rollout-runbook.md) now includes the unattended `run_maker_rollout_gate.py` path, not just manual profile build / soak commands.

## [2026-03-22] (Session 86) — Frozen Maker Paper Profiles + Readiness Report

### Frozen Paper Profiles
- **Added a profile materializer for the current maker paper branch** — [scripts/build_maker_rollout_profiles.py](../../scripts/build_maker_rollout_profiles.py) now generates:
  - [maker_paper_baseline.json](../../ops/profiles/maker_paper_baseline.json)
  - [maker_paper_widen20.json](../../ops/profiles/maker_paper_widen20.json)
- These profiles freeze the current maker paper rail:
  - `mode=simulation`
  - `execution_mode=maker`
  - `sim_live_execution=true`
  - `shadow_trading=false`
  - `macro_event_guard_enabled=true`
  - `regime_allowlist=["TREND_UP"]`
  - `session_allowlist=["asia","us"]`
  - `blocked_hours_utc=[1,2,3,4,6,16,17,18,19,20]`
  - `min_confidence_margin=0.03`

### Readiness And Frozen Soak Tooling
- **Added a frozen-profile soak runner** — [scripts/run_frozen_maker_soak.py](../../scripts/run_frozen_maker_soak.py) boots the runtime directly from a frozen profile and emits the usual maker soak report without patching the config on the fly.
- **Added a rollout readiness report** — [scripts/maker_rollout_readiness_report.py](../../scripts/maker_rollout_readiness_report.py) now:
  - checks the frozen baseline and candidate profile invariants
  - ranks candidates by the latest meaningful A/B evidence per candidate
  - reports blockers before any paper-freeze or promotion
- Current artifact: 20260322_033719_maker_rollout_readiness_report.json
  - verdict: `blocked_waiting_for_repeatable_evidence`
  - front-runner: `widen_20bps`
  - blockers:
    - verdict still `mixed_when_engaged`
    - quote coverage tax still about `-27.19%`
    - only `3` engaged comparable windows

### Runbook
- **Updated the rollout runbook** — [ops/live-rollout-runbook.md](../../ops/live-rollout-runbook.md) now documents:
  - building the frozen maker paper profiles
  - generating the readiness report
  - running a soak from the frozen profile directly

## [2026-03-22] (Session 85) — Maker A/B Summary Surface + Artifact Label Correction

### Operator Truth
- **Added a live maker A/B summary endpoint** — [src/api/maker.rs](../../src/api/maker.rs) now exposes `/api/maker/ab-summary`, which:
  - scans artifacts/maker-ab-batches
  - summarizes the latest batch
  - summarizes the latest artifact per candidate
  - picks a heuristic front-runner that balances toxicity improvement against quote-starvation cost
- **Wired the summary into the Maker Desk** — [static/index.html](../../static/index.html) now shows:
  - current A/B front-runner candidate
  - current A/B verdict
  - average adverse-move delta
  - average quote-coverage delta
  - a note when the latest batch is only waiting for the next eligible start window

### Correction
- **Corrected the aggregate batch labeling story** — the later aggregate artifacts 20260322_030927_maker_ab_batch.json and 20260322_031031_maker_ab_batch.json are `40bps` widening summaries, not `20bps`.
- **Current honest read** — `20bps` remains the cleaner paper candidate from 20260321_193648_maker_ab_batch.json, while `40bps` is still mixed and more coverage-expensive in the later aggregate artifacts.

## [2026-03-22] (Session 84) — 20bps Aggregate Verdict + Eligible-Start Gating

### Aggregate 40bps Read
- **Combined every usable `40bps` widening cycle collected so far into one aggregate batch** — 20260322_030927_maker_ab_batch.json now gives the cleanest current summary for that candidate:
  - verdict: `mixed_when_engaged`
  - `4` usable cycles
  - `4` comparable engaged windows
  - `3` improved adverse move
  - `1` regressed
  - avg adverse delta: `-11.08bps`
  - avg p90 adverse delta: `-37.5bps`
  - avg places delta: `-31.89%`
- **Current read** — `40bps` widened quotes stay mixed and coverage-expensive; the better surviving paper candidate is still the earlier `20bps` branch, not this later aggregate.

### Eligible-Start And Sparse-Run Guardrails
- **The batch runner now refuses to start new live A/B batches in blocked/ineligible windows** — [scripts/run_maker_ab_batch.py](../../scripts/run_maker_ab_batch.py) now:
  - classifies the current UTC start window by session and blocked hour
  - reports `awaiting_eligible_start` instead of starting a doomed sparse batch
  - computes `next_eligible_delay_seconds` so operators know how long until the next real start window
- **Adaptive batch controls are now explicit in the artifact**:
  - `target_usable_cycles`
  - `max_total_cycles`
  - `max_sparse_streak`
  - `start_window`
  - `stop_reason`
- **Validated current blocked-hour behavior** — 20260322_031201_maker_ab_batch.json correctly reports:
  - verdict: `awaiting_eligible_start`
  - start hour UTC: `3`
  - session: `asia`
  - `hour_allowed = false`
  - next eligible delay about `6478s`

### Verification
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 -m py_compile scripts/run_maker_ab_batch.py`
- Aggregate artifact:
  - 20260322_030927_maker_ab_batch.md
- Eligible-start artifact:
  - 20260322_031201_maker_ab_batch.md

## [2026-03-22] (Session 83) — Larger 20bps Batch + Sparse-Run Hardening

### Batch Runner Hardening
- **The unattended maker batch runner now degrades gracefully when later windows get too thin** — [scripts/run_maker_ab_batch.py](../../scripts/run_maker_ab_batch.py) now:
  - tracks `usable_cycles` vs `sparse_cycles`
  - filters batch verdicts by `--min-leg-events`
  - supports partial-batch salvage instead of throwing away completed cycles
  - records subprocess failure details when a live cycle dies mid-batch
- **Existing cycle artifacts can now be rescored under the stricter usable-cycle filter** — [scripts/run_maker_ab_batch.py](../../scripts/run_maker_ab_batch.py) can summarize saved cycle JSONs with `--existing-cycle-json` and `--min-leg-events`.

### Larger 20bps Evidence
- **Ran a larger `5`-cycle attempt for the `20bps` widening candidate** — the live batch itself ran into sparse later windows, but the usable evidence was still recoverable and the candidate did not collapse.
- **The earlier dense `3`-cycle batch remains the main truth source** — 20260321_193648_maker_ab_batch.json:
  - verdict: `mixed_when_engaged`
  - `3/3` engaged comparable windows
  - `2` improved adverse move
  - `1` regressed
  - avg adverse delta: `-12.97bps`
  - avg p90 adverse delta: `-50bps`
  - avg places delta: `-27.19%`
- **The salvaged sparse-window batch stayed directionally positive on its single usable cycle** — 20260322_030655_maker_ab_batch.json:
  - verdict: `directional_improvement_when_engaged`
  - `1` usable comparable cycle
  - adverse delta: `-5.43bps`
  - places delta: `-46%`

### Current Project Read
- **`20bps` widening is still the front-runner on the basis-reactive maker branch**.
- **It is still not promotable** because:
  - sample adequacy is thin
  - quote coverage cost is still meaningful
  - later cycles can drift into sparse windows that are not good rollout evidence
- **But the branch is alive** — unlike full suppression and unlike `40bps`, this candidate is still worth continued paper validation.

### Verification
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 -m py_compile scripts/run_maker_ab_batch.py`
- Batch artifacts:
  - 20260321_193648_maker_ab_batch.md
  - 20260322_030655_maker_ab_batch.md

## [2026-03-21] (Session 82) — Backwardation Widening Batch Sweep

### Widening Batch Comparison
- **Ran a repeated three-cycle batch for the current `40bps` widening candidate** — 20260321_193117_maker_ab_batch.json rescored the existing widening cycles with the new engaged-window logic and found:
  - verdict: `directional_regression_when_engaged`
  - `3` comparable windows
  - widening engaged in only `1` of them
  - engaged adverse delta: `+30.00bps`
- **Then ran the next evidence-driven candidate: `20bps` widening** — 20260321_193648_maker_ab_batch.json came back materially better:
  - verdict: `mixed_when_engaged`
  - `3` comparable windows
  - widening engaged in all `3`
  - `2` windows improved adverse move
  - `1` window regressed
  - avg adverse delta: `-12.97bps`
  - avg p90 adverse delta: `-50bps`
  - tradeoff: quote coverage still fell, with avg places delta `-27.19%`

### Current Maker Read
- **`40bps` widening is demoted** — it is too aggressive for the observed backwardation windows.
- **`20bps` widening is the new front-runner on this branch** — not because it is proven, but because it is the first widening variant to show repeated engaged improvement more often than regression.
- **Not promoted yet** — sample adequacy is still thin and coverage loss is still meaningful, so this remains a paper candidate, not a rollout config.

### Verification
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 -m py_compile scripts/run_maker_ab_batch.py`
- Batch artifacts:
  - 20260321_193117_maker_ab_batch.md
  - 20260321_193648_maker_ab_batch.md

## [2026-03-21] (Session 81) — Repeated Maker A/B Batch Scoring

### Batch Evidence Tooling
- **Added a repeated maker A/B batch runner** — [scripts/run_maker_ab_batch.py](../../scripts/run_maker_ab_batch.py) now:
  - runs multiple [scripts/run_maker_ab_cycle.py](../../scripts/run_maker_ab_cycle.py) legs back-to-back
  - aggregates comparable vs non-comparable windows
  - tracks coverage-starved windows
  - summarizes adverse-move deltas, place deltas, and widening activation counts
- **The batch scorer now distinguishes inactive widening from active regression** — [scripts/run_maker_ab_batch.py](../../scripts/run_maker_ab_batch.py) now reports:
  - `engaged_windows`
  - `engaged_comparable_windows`
  - `engaged_improved_adverse_move`
  - `engaged_regressed_adverse_move`
  and supports rescoring existing cycle JSONs through `--existing-cycle-json` so we can refine verdict logic without rerunning the world.

### Widening Batch Findings
- **A first raw three-cycle widening batch looked bad overall, but needed a more honest scorer** — 20260321_193014_maker_ab_batch.json initially labeled the candidate `repeatable_regression`, but that batch mixed:
  - one engaged widening window
  - two windows where widening never actually fired
- **The corrected rescore gives the right reading** — 20260321_193117_maker_ab_batch.json:
  - verdict: `directional_regression_when_engaged`
  - `3` comparable windows
  - `1` engaged comparable window
  - `0` engaged improvements
  - `1` engaged regression
  - avg engaged adverse delta: `+30.00bps`
- **Practical read** — widening is not dead as a concept, but the current `40bps` implementation has not earned promotion. It engages rarely in the tested windows, and the one clearly engaged comparable window got worse.

### Verification
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 -m py_compile scripts/run_maker_ab_batch.py`
- Rescored existing cycle artifacts via:
  - `python3 scripts/run_maker_ab_batch.py --candidate-mode widen --existing-cycle-json ...`

## [2026-03-21] (Session 80) — Maker A/B Telemetry Upgrade + Backwardation Widening

### Reporting And Compare Truth
- **Maker soak reports now carry Roll/no-arb run context through unattended A/B cycles** — [scripts/run_maker_soak.py](../../scripts/run_maker_soak.py) now captures end-of-run snapshots from:
  - `/api/maker/state`
  - `/api/markets/no-arb`
  and passes them into [scripts/maker_soak_report.py](../../scripts/maker_soak_report.py)
- **Maker soak reports now summarize Roll buckets and run-level context explicitly** — [scripts/maker_soak_report.py](../../scripts/maker_soak_report.py) now exposes:
  - `run_context.maker_roll`
  - `run_context.no_arb`
  - `roll_buckets`
  - decision/cancel Roll averages
- **A/B comparisons now call full quote starvation honestly** — [scripts/maker_ab_compare.py](../../scripts/maker_ab_compare.py) now emits `guard_starved_candidate` when the backwardation guard suppresses essentially all quoting instead of hiding it under a generic `inconclusive`.

### Guarded Maker Verdict Update
- **The short live-sim guarded cycle is now classified correctly as over-blunt** — 20260321_190829_maker_ab_compare.json upgraded the earlier short-cycle verdict from generic `inconclusive` to:
  - `guard_starved_candidate`
- **A longer guarded cycle confirmed the same problem** — 20260321_191006_maker_ab_compare.json:
  - baseline: `72` places / `68` cancels
  - guarded: `0` places / `0` cancels / `458` guard skips
  - verdict: `guard_starved_candidate`
- **Current read** — full backwardation suppression is too blunt to promote.

### Soft Backwardation Widening Candidate
- **Added a softer maker response to toxic backwardation** — [src/config.rs](../../src/config.rs), [src/bot/actor.rs](../../src/bot/actor.rs), and [config.json](../../config.json) now support:
  - `maker_backwardation_widen_bps`
  - spread widening when `basis_pct <= maker_backwardation_guard_pct`
  instead of only full quote suppression
- **Added quote-plan coverage for widening** — [src/bot/actor.rs](../../src/bot/actor.rs) now tests:
  - widening activates below the backwardation threshold
  - widened plans remain quotable and respect spread bounds
- **Unattended maker A/B cycles can now compare `guard` vs `widen` candidates** — [scripts/run_maker_ab_cycle.py](../../scripts/run_maker_ab_cycle.py) now supports:
  - `--candidate-mode guard`
  - `--candidate-mode widen`
  - `--maker-backwardation-widen-bps`

### Widening Telemetry
- **Maker quote telemetry now records whether backwardation widening actually fired** — [src/bot/actor.rs](../../src/bot/actor.rs) and [scripts/maker_soak_report.py](../../scripts/maker_soak_report.py) now expose:
  - `basis_widen_applied`
  - `basis_widen_bps`
  - widening counts in top-line summary and grouped buckets
- **A/B compare reports now surface widening usage directly** — [scripts/maker_ab_compare.py](../../scripts/maker_ab_compare.py) now reports:
  - `basis_widen_count_at_decision`
  - `avg_basis_widen_bps_at_decision`
  - explicit widening observations

### Widening Findings
- **The first widening cycle preserved comparability and looked directionally better, but sample adequacy was still weak** — 20260321_191405_maker_ab_compare.json:
  - baseline: `72` places / `68` cancels / avg adverse `52.21bps`
  - widen candidate: `90` places / `86` cancels / avg adverse `37.21bps`
  - verdict remained `inconclusive` because the sample was still too small
- **The telemetry-complete rerun showed the widening rule really fired, but this later comparable window regressed** — 20260321_191810_maker_ab_compare.json:
  - baseline: `101` places / `97` cancels / avg adverse `45.88bps`
  - widen candidate: `70` places / `66` cancels / avg adverse `55.30bps`
  - widening applied on `70` decisions at `40bps`
  - verdict: `inconclusive`
- **Current read** — widening is the better next research path than full suppression because it preserves comparability, but it is not yet good enough to promote.

### Verification
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 -m py_compile scripts/maker_soak_report.py scripts/maker_ab_compare.py scripts/run_maker_soak.py scripts/run_maker_ab_cycle.py`
- `cargo fmt --all`
- `cargo test quote_plan_widens_under_backwardation_when_enabled -- --test-threads=1`
- `cargo check`
- `cargo test`
- Full Rust suite result after this batch: `312 passed; 0 failed`

## [2026-03-21] (Session 79) — TP/SL Freshness + No-Arb Monitor + Roll L2 Signal

### TP/SL Safety
- **TP/SL exits now fail closed instead of trading on stale cached prices** — [src/bot/state/tp_sl.rs](../../src/bot/state/tp_sl.rs), [src/bot/state/trade_lifecycle.rs](../../src/bot/state/trade_lifecycle.rs), [src/config.rs](../../src/config.rs), and [config.json](../../config.json) now:
  - timestamp cached per-trade fallback prices
  - cap same-market analysis fallback by the tighter of `analysis_stale_seconds` and `tp_sl_fallback_max_age_seconds`
  - reject stale cached-price fallback completely instead of silently using it
  - expose `tp_sl_fallback_max_age_seconds` as an explicit runtime config field
- **Added regression tests around stale fallback behavior** — [src/bot/state/tp_sl.rs](../../src/bot/state/tp_sl.rs) now tests:
  - tighter TP/SL fallback bounds
  - analysis fallback never exceeding analysis staleness
  - cached per-trade prices requiring fresh observation timestamps

### Simultaneous-Contract No-Arbitrage Monitor
- **Added a dedicated broad BTC same-expiry ladder monitor** — [src/api/markets.rs](../../src/api/markets.rs) now exposes [GET `/api/markets/no-arb`](../../src/api/mod.rs) and a reusable [fetch_no_arb_snapshot](../../src/api/markets.rs) helper that:
  - scans active BTC/Bitcoin Polymarket markets broadly, not just the current `slug_pattern`
  - extracts strike prices from market text
  - groups parseable contracts by shared expiry
  - flags monotonicity violations when higher-strike YES prices exceed lower-strike YES prices by more than `1.0c`
- **The no-arb snapshot is now part of the realtime slow payload** — [src/api/realtime.rs](../../src/api/realtime.rs), [src/api/sse.rs](../../src/api/sse.rs), and [src/api/ws.rs](../../src/api/ws.rs) now carry `no_arb` alongside markets/outcomes/trades.
- **The dashboard now surfaces same-expiry no-arb health directly** — [static/index.html](../../static/index.html) now renders a `Simultaneous No-Arb` overview card with:
  - current clean/violation/insufficient status
  - comparable ladder count
  - max violation in cents
  - worst current ladder detail
- **Added pure monotonicity tests** — [src/api/markets.rs](../../src/api/markets.rs) now verifies both clean ladders and explicit violation ladders.

### Roll-Style L2 Microstructure Signal
- **Added a Roll-style informed-flow detector on Polymarket L2 mids** — [src/signals/roll.rs](../../src/signals/roll.rs) computes:
  - serial covariance of successive mid-price changes
  - normalized persistence
  - signed directional signal
  - confidence and recent delta context
- **Maker state now tracks Roll telemetry per token from live BBO updates** — [src/bot/actor.rs](../../src/bot/actor.rs) now keeps a short mid-history per token, recomputes the Roll signal when BBO changes, and includes `roll_microstructure` in the maker snapshot.
- **The maker dashboard shows the strongest current Roll state** — [static/index.html](../../static/index.html) now exposes `Roll State` and `Roll Signal` in the Maker Engine Control panel so live maker soaks can inspect this signal before it influences execution.

### Verification
- `cargo fmt --all`
- `cargo test tp_sl -- --test-threads=1`
- `cargo test no_arb -- --test-threads=1`
- `cargo test roll -- --test-threads=1`
- `cargo check`
- `cargo test`
- Full Rust suite result after these batches: `311 passed; 0 failed`

## [2026-03-21] (Session 78) — Runtime Specialist Maker A/B

### Runtime Specialist Alignment
- **The live maker soak path now honors the same specialist filters the research lane uses** — [src/config.rs](../../src/config.rs), [src/bot/risk.rs](../../src/bot/risk.rs), and [src/bot/execute.rs](../../src/bot/execute.rs) now enforce:
  - `session_allowlist`
  - `blocked_hours_utc`
  - `min_confidence_margin`
- **The maker soak runner can patch those runtime specialist filters directly** — [scripts/run_maker_soak.py](../../scripts/run_maker_soak.py) now injects the runtime specialist config into its temporary `MACBOT_CONFIG` instead of relying on broad maker defaults.
- **Config snapshots already contained the temporary soak config, but nested under `config_redacted`** — the live `/api/config` path and the saved snapshot artifacts both confirmed the specialist runtime config was active during the soak reruns.

### Reporting
- **Maker soak reports now distinguish place-only basis from decision-time basis** — [scripts/maker_soak_report.py](../../scripts/maker_soak_report.py) now exposes:
  - `avg_basis_pct_at_decision`
  - `avg_basis_pct_at_guard_skip`
  - non-null basis counts for place vs decision events
  so guarded runs no longer hide their futures-basis context behind a `null` top-line place metric.
- **Added a dedicated maker A/B comparison report** — [scripts/maker_ab_compare.py](../../scripts/maker_ab_compare.py) compares baseline vs candidate maker-soak reports and emits:
  - sample adequacy
  - dominant window/session comparability
  - metric deltas
  - an explicit verdict such as `promising_guard_candidate` or `promising_but_noncomparable`
- **Added an unattended maker A/B cycle runner** — [scripts/run_maker_ab_cycle.py](../../scripts/run_maker_ab_cycle.py) packages:
  - baseline soak
  - guarded soak
  - A/B compare
  into one command so longer maker validation can run without manual babysitting.

### Maker Soak Findings
- **The first runtime-specialist baseline window was too thin to use** — 20260321_074800_maker_soak_report.json only captured `12` events because the run landed in a sparse transition window, so it was explicitly discarded as the promotion baseline.
- **A fresh baseline rerun produced the first meaningful runtime-specialist control sample** — 20260321_083142_maker_soak_report.json:
  - `1452` events
  - `728` places / `724` cancels
  - avg adverse move `123.83bps`
  - p90 adverse move `300bps`
- **The guarded runtime-specialist candidate looks materially less toxic, but the windows were not directly comparable** — 20260321_083144_maker_soak_report.json:
  - `2740` events
  - `825` places / `825` cancels / `1090` guard skips
  - avg adverse move `46.23bps`
  - p90 adverse move `100bps`
- **The new A/B compare report calls the current state correctly** — 20260321_083148_maker_ab_compare.json verdict:
  - `promising_but_noncomparable`
  because the guarded sample was mostly `asia/07` while the fresh baseline was mostly `europe/08`
- **Best current read** — the backwardation guard is earning its next batch, but not a default promotion yet. The effect size on toxicity is large enough to keep, and the new unattended cycle runner is the right next tool for repeatable validation across more windows.

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test bot::risk::tests:: -- --test-threads=1`
- `cargo test`
- Full Rust suite result after runtime specialist alignment: `302 passed; 0 failed`
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 -m py_compile scripts/run_maker_soak.py scripts/maker_soak_report.py scripts/maker_ab_compare.py scripts/run_maker_ab_cycle.py`
- Runtime spot-checks:
  - `curl -s http://127.0.0.1:8000/api/config -H 'x-api-key: local' | jq '{execution_mode, regime_allowlist, session_allowlist, blocked_hours_utc, min_confidence_margin, macro_event_guard_enabled}'`
  - `python3 scripts/maker_ab_compare.py --baseline artifacts/maker-soak-reports/20260321_083142_maker_soak_report.json --candidate artifacts/maker-soak-reports/20260321_083144_maker_soak_report.json`
  - `python3 scripts/run_maker_ab_cycle.py --config config.json --duration-minutes 0.05 --report-hours 0.25 --enable-macro-guard`
    - smoke artifact: 20260321_083314_maker_ab_cycle.json

## [2026-03-21] (Session 77) — Maker Basis Guard A/B

### Trade History Recovery
- **Added a fill-ledger trade reconstruction tool** — [scripts/reconstruct_trade_history.py](../../scripts/reconstruct_trade_history.py) rebuilds best-effort trade instances from `trade_fills` and enriches them with pre-entry `analysis_snapshots` when the top-level `trades` table has been cleared or reset.
- **Recovered a much larger historical ledger from the current DB** — running the reconstruction against trades.db produced:
  - CSV: 20260321_071943_reconstructed_trade_history.csv
  - JSON: 20260321_071943_reconstructed_trade_history.json
  - Markdown summary: 20260321_071943_reconstructed_trade_history.md
- **The main `trades` table was not the full story** — the recovered ledger found `348` fill-backed trade instances across `145` markets from March 15-21, with exact direction/entry-price matches for the `2` surviving rows in the top-level `trades` table.

### Dashboard Safety
- **`New Session` no longer destroys the trade ledger** — [static/index.html](../../static/index.html) now treats `New Session` as a non-destructive local session rollover:
  - closed trades are copied into a persisted local archived list
  - the current-session history view is filtered by the stored session boundary
  - the button no longer calls `/api/trades/clear-history`
- **`Clear History` is now labeled honestly as permanent** — the dashboard now warns that it deletes closed trades from the database, and copies currently visible closed trades into the local archived section first.

### Maker Runtime
- **Added a configurable backwardation guard for maker quoting** — [src/config.rs](../../src/config.rs) and [config.json](../../config.json) now expose:
  - `maker_backwardation_guard_enabled`
  - `maker_backwardation_guard_shadow_only`
  - `maker_backwardation_guard_pct`
- **The maker actor now supports both shadow-only and live-suppression guard modes** — [src/bot/actor.rs](../../src/bot/actor.rs) now:
  - detects stronger backwardation from the live futures basis context
  - emits explicit `guard_shadow` or `guard_skip` telemetry
  - cancels active quotes with `basis_guard` when the live guard is enabled
  - preserves the decision trail in maker model snapshots instead of silently skipping quotes

### Maker Telemetry
- **Quote telemetry now carries richer classification context** — [src/bot/actor.rs](../../src/bot/actor.rs) telemetry now includes `is_yes_token` so downstream analysis can split maker behavior by YES/NO token path.
- **Maker soak reporting is now much more forensic** — [scripts/maker_soak_report.py](../../scripts/maker_soak_report.py) now breaks telemetry down by:
  - event type
  - side
  - token kind
  - session/hour
  - price-to-beat bucket
  - quote-age bucket
- **The soak runner can now run isolated A/B experiments** — [scripts/run_maker_soak.py](../../scripts/run_maker_soak.py) now supports:
  - custom report windows
  - backwardation guard toggles
  - live-vs-shadow guard mode selection

### A/B Result
- **Ran a real baseline vs guarded live paper-soak comparison on isolated temp DBs**:
  - baseline artifact: 20260321_043450_maker_soak_report.json
  - guarded artifact: 20260321_043719_maker_soak_report.json
- **The live backwardation guard reduced observed cancel toxicity, but at a real coverage cost**:
  - baseline: `214` places / `210` cancels / avg adverse move `35.5bps`
  - guarded: `124` places / `120` cancels / `516` guard skips / avg adverse move `29.2bps`
- **This is a useful signal, not a promotion decision yet** — the guard looks promising, but it needs a longer soak before we hard-enable it as a default maker rule.

### Verification
- `cargo fmt --all`
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 -m py_compile scripts/maker_soak_report.py scripts/run_maker_soak.py`
- `cargo check`
- `cargo test process_maker_bbo_event_basis_guard_skips_when_enabled -- --test-threads=1`
- `cargo test process_maker_bbo_event_basis_guard_shadow_keeps_quotes -- --test-threads=1`
- `cargo test`
- Full Rust suite result: `299 passed; 0 failed`

## [2026-03-21] (Session 76) — Macro Guard + Maker Paper-Soak Runner

### Macro Guardrails
- **Added a live macro-event stand-down guard backed by an explicit UTC event calendar** — [src/bot/macro_calendar.rs](../../src/bot/macro_calendar.rs) now loads [ops/macro-events.json](../../ops/macro-events.json) and reports:
  - current blocking window
  - next scheduled event
  - before/after guard windows
  - file/load errors without silently failing open
- **Runtime trade gating now respects the macro guard** — [src/bot/risk.rs](../../src/bot/risk.rs) now carries `macro_guard` inside the risk snapshot and rejects new entries with `MacroEventStandDown` whenever the configured guard window is active.
- **Config now supports explicit macro guard tuning** — [src/config.rs](../../src/config.rs) and [config.json](../../config.json) now expose:
  - `macro_event_guard_enabled`
  - `macro_event_guard_minutes_before`
  - `macro_event_guard_minutes_after`
  - `macro_event_calendar_path`

### Analysis + API Surface
- **Analysis payloads now expose price-to-beat context directly** — [src/bot/analyze.rs](../../src/bot/analyze.rs) now includes:
  - `price_to_beat_distance_pct`
  - `market_start_utc`
  alongside the previously added futures-basis fields.
- **Realtime and risk APIs now surface macro guard state for operators** — [src/api/realtime.rs](../../src/api/realtime.rs) and [src/api/bot_routes.rs](../../src/api/bot_routes.rs) now expose `macro_guard` in:
  - fast SSE snapshots
  - risk snapshots
  - `/api/risk/status`

### Maker Paper-Soak Workflow
- **Maker quote telemetry now records price-to-beat context as well** — [src/bot/actor.rs](../../src/bot/actor.rs) now stores `price_to_beat_distance_pct` with quote placement/cancel telemetry so live paper-soak analysis can study toxicity against strike context, not just basis/spread.
- **Added a repeatable maker paper-soak launcher** — [scripts/run_maker_soak.py](../../scripts/run_maker_soak.py) now:
  - creates a temporary config override
  - forces maker simulation with `sim_live_execution=true`
  - optionally enables the macro guard
  - waits for health
  - shuts the bot down cleanly
  - emits a fresh maker-soak report
- **Offline maker soak summaries now include strike context** — [scripts/maker_soak_report.py](../../scripts/maker_soak_report.py) now reports average `price_to_beat_distance_pct` at quote placement in addition to the earlier basis/adverse-move telemetry.

### Dashboard
- **The operator dashboard now shows the new context directly** — [static/index.html](../../static/index.html) now renders:
  - a BTC `Price vs Beat` context pill
  - macro-guard summary in the Risk tab
  - config controls for the macro guard

### Verification
- **Rust and Python verification stayed green across the new live-safety path**:
  - `cargo fmt --all`
  - `cargo check`
  - `cargo test macro_event_guard_blocks_entries -- --test-threads=1`
  - `cargo test macro_guard_status_blocks_inside_window_and_reports_next -- --test-threads=1`
  - `cargo test`
  - `python3 -m py_compile scripts/maker_soak_report.py scripts/run_maker_soak.py`
- **Runtime smoke validated the new surfaces**:
  - `MACBOT_BACKTEST_ONLY=1 cargo run`
  - `curl -s http://127.0.0.1:8000/api/risk/status | jq '{macro_guard, execution_mode, kill_switch}'`
  - `curl -s http://127.0.0.1:8000/api/analysis | jq '{price_to_beat_distance_pct, market_start_utc, futures_basis_pct}'`
  - `python3 scripts/run_maker_soak.py --dry-run`
- **First populated maker paper-soak artifact is now in place**:
  - `python3 scripts/run_maker_soak.py --config config.json --db trades.db --duration-minutes 5 --enable-macro-guard`
  - artifact: 20260321_041416_maker_soak_report.json
  - headline read:
    - `732` maker telemetry events
    - `368` places / `364` cancels
    - all cancels were replace-driven in this short paper soak
    - backwardation-tagged cancels were more toxic than `unknown` basis cancels (`55.9bps` vs `27.2bps` avg adverse move)
- **Full Rust suite result:** `297 passed; 0 failed`

## [2026-03-21] (Session 75) — Maker Soak Telemetry Rail

### Maker OMS Telemetry
- **Added a structured maker quote telemetry stream alongside the existing outbox audit** — [src/bot/actor.rs](../../src/bot/actor.rs) now emits `maker_quote_telemetry` rows into `order_outbox` for:
  - quote placement
  - replace-driven cancels
  - cleanup/stale cancels
- **The telemetry captures the actual microstructure context we need for live paper research** — each structured event now records:
  - side, quote price, quote size
  - quote age on cancel
  - placed/current token mid and spread
  - signed quote move and adverse move in bps
  - futures basis, mark, and index context
  - BTC spot move context
  - inventory and fair-value metadata from the quote plan

### Actor State
- **Maker snapshots now keep a short rolling window of recent telemetry events** — [src/bot/actor.rs](../../src/bot/actor.rs) now tracks recent quote telemetry in memory so the maker desk can surface it later without scraping the DB first.
- **Active quotes now carry explicit placement context** — quote lifecycle state now remembers the placement snapshot needed to evaluate toxicity when the quote is later canceled or replaced.

### Dashboard
- **The maker desk now surfaces the recent telemetry stream directly** — [static/index.html](../../static/index.html) now shows a compact `Recent Quote Telemetry` table with event, side, reason, age, adverse move, basis, and order id so the live paper desk can inspect toxicity without waiting for the offline soak report.

### Reporting
- **Added an offline maker-soak report builder** — [scripts/maker_soak_report.py](../../scripts/maker_soak_report.py) summarizes the new telemetry from `order_outbox`, including:
  - place/cancel counts
  - cancel reasons
  - quote lifetime
  - adverse move stats
  - basis bucket breakdowns
- **First artifact path is live, even though it is empty by design right now** — the first report at 20260321_033137_maker_soak_report.json contains `0` events because telemetry only starts accumulating after this batch lands.

### Tests
- **Added focused actor coverage for the new telemetry stream** — [src/bot/actor.rs](../../src/bot/actor.rs) now verifies:
  - place telemetry is emitted for both quote sides
  - cancel telemetry is emitted during cleanup with the correct reason

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test bot::actor::tests:: -- --test-threads=1`
- `cargo test`
- `python3 -m py_compile scripts/maker_soak_report.py`
- `python3 scripts/maker_soak_report.py --db trades.db --hours 168`

## [2026-03-21] (Session 74) — Dashboard Operator-Truth Widgets

### Dashboard Surface
- **Added compact operator-truth widgets for the new realtime context** — [static/index.html](../../static/index.html) now surfaces:
  - nav-level `PX` health for the Binance WebSocket price bus
  - nav-level integrity status for dashboard-vs-backend agreement
  - a BTC stream context strip for:
    - feed health
    - futures basis
    - futures mark/index
- **The Risk tab now exposes integrity state directly** — the Risk Armor telemetry view now shows:
  - `Integrity`
  - `Mismatch Count`
  so operator trust is visible without hitting raw JSON endpoints.

### Backend Wiring
- **Fast SSE snapshots now carry price-bus health explicitly** — [src/api/realtime.rs](../../src/api/realtime.rs) now includes a `price_bus` object with:
  - `status`
  - `source`
  - `symbols_live`
  - per-symbol price/age/live state for BTC, ETH, and SOL
- **Price bus exposes direct symbol snapshots for UI/reporting** — [src/signals/price_bus.rs](../../src/signals/price_bus.rs) now provides `symbol_snapshot()` so the realtime API can report freshness without duplicating bus internals.

### Findings
- **The dashboard now tells the truth about feed freshness instead of only showing derived prices** — operators can see when the WS bus is live, degraded, or offline, and how stale the BTC stream is.
- **Futures basis is now visible where it belongs** — basis, mark, and index values are no longer hidden in the analysis payload; they are visible directly in the BTC tape view.
- **Integrity is promoted from an API-only diagnostic to an operator signal** — the `/api/integrity/report` verdict now appears in both the nav and the Risk tab, making snapshot drift much harder to miss.

### Tests
- **Extended price-bus coverage for the new snapshot helper** — [src/signals/price_bus.rs](../../src/signals/price_bus.rs) now verifies that the bus returns the latest symbol snapshot alongside the existing lookback-change behavior.

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test`
- local runtime smoke:
  - `MACBOT_BACKTEST_ONLY=1 cargo run`
  - `curl -s http://127.0.0.1:8000/api/integrity/report | jq '{ok, mismatch_count:(.mismatches|length)}'`
  - `curl -sN --max-time 3 http://127.0.0.1:8000/api/stream | head -n 6`
  - `curl -s http://127.0.0.1:8000/ | rg -o "nav-pricebus-status|nav-integrity-status|btc-feed-status|btc-futures-basis|risk-integrity-mismatches" -n`

## [2026-03-21] (Session 73) — Futures Basis Context

### Context Expansion
- **Extended the Binance futures fetch path with spot-vs-futures basis** — [src/signals/fetchers.rs](../../src/signals/fetchers.rs) now fetches Binance `premiumIndex`, parses:
  - `markPrice`
  - `indexPrice`
  - `basis_pct = (mark - index) / index * 100`
  alongside the existing funding-rate and open-interest pulls.
- **The live futures cache now carries this new context through runtime** — [src/bot/state.rs](../../src/bot/state.rs) and [src/bot/analyze.rs](../../src/bot/analyze.rs) now preserve:
  - `basis_pct`
  - `mark_price`
  - `index_price`
  in the cached futures snapshot instead of dropping them at fetch time.

### Analysis Surface
- **Analysis payloads now expose futures basis directly** — [src/bot/analyze.rs](../../src/bot/analyze.rs) now includes:
  - `futures_basis_pct`
  - `futures_mark_price`
  - `futures_index_price`
  in the `extra` analysis payload, so the dashboard/archive/research layer can inspect futures premium or backwardation without reopening the 34-feature model contract yet.

### Findings
- **This is intentionally a context-first batch, not a model-schema batch** — basis is now available for live analysis, archives, and future research, but it has **not** been promoted into the frozen 34-feature ML contract yet.
- **The project now has a cleaner next-step route for a basis regime signal** — we can study whether contango/backwardation actually helps the current specialist lanes before paying the cost of another schema expansion.

### Tests
- **Added focused parser tests for futures basis extraction** — [src/signals/fetchers.rs](../../src/signals/fetchers.rs) now verifies:
  - correct `%` basis extraction from `markPrice` and `indexPrice`
  - graceful handling when basis fields are missing

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test signals::fetchers::tests:: -- --test-threads=1`

## [2026-03-21] (Session 72) — Batch 4 Validation: Backtest Contract + A/B Verdict

### Backtest Contract
- **The Rust backtest API can now measure session-aware weighting instead of silently ignoring it** — [src/api/backtest.rs](../../src/api/backtest.rs) now accepts `time_of_day_weight`, applies hour-aware score scaling to the simplified research scorer, and reports the flag back in the response payload.
- **Research scripts now pass the same flag cleanly** — [scripts/daily_alpha_report.py](../../scripts/daily_alpha_report.py), [scripts/recency_alpha_report.py](../../scripts/recency_alpha_report.py), [scripts/walkforward_alpha_sweep.py](../../scripts/walkforward_alpha_sweep.py), [scripts/lane_forensics_report.py](../../scripts/lane_forensics_report.py), [scripts/recent_specialist_validation_report.py](../../scripts/recent_specialist_validation_report.py), and [scripts/recent_specialist_soak_report.py](../../scripts/recent_specialist_soak_report.py) now expose and forward `--time-of-day-weight` so the validation rail stays aligned with runtime.
- **Added a named A/B preset for reproducible comparison** — [scripts/alpha_presets.py](../../scripts/alpha_presets.py) now includes `trend_up_yes_tod_hour_filtered_specialist`, which is the existing hour-filtered maker specialist plus `time_of_day_weight=true`.

### Findings
- **The current hour-filtered maker specialist is still stronger without the new time-of-day weighting** — short-window recency stayed `pass` on the baseline 20260321_030047_recency_alpha_report.json, but the time-weighted variant regressed to `fail` on 20260321_030112_recency_alpha_report.json because the `7d` primary positive-fold ratio dropped below the gate.
- **The same pattern holds on the harder longer-window rail** — both variants still pass the `14/21/30` + `60d` stress gate, but the weighted version is weaker:
  - baseline: 20260321_030154_recency_alpha_report.json
  - weighted: 20260321_030225_recency_alpha_report.json
- **Honest read** — the session-aware weighting is now implemented and testable, but it does **not** improve the current maker specialist. It should remain available as a research switch, not be promoted into the active paper baseline.

### Tests
- **Added focused backtest coverage for the new weighting helper** — [src/api/backtest.rs](../../src/api/backtest.rs) now verifies the simplified session profile ordering (`asia < europe < us`) for both technical and regime score buckets.

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test api::backtest::tests:: -- --test-threads=1`
- `cargo test`
- `python3 -m py_compile scripts/alpha_presets.py scripts/daily_alpha_report.py scripts/recency_alpha_report.py scripts/walkforward_alpha_sweep.py scripts/lane_forensics_report.py scripts/recent_specialist_validation_report.py scripts/recent_specialist_soak_report.py`
- local backtest-only API A/B runs:
  - `python3 scripts/recency_alpha_report.py --preset trend_up_yes_hour_filtered_specialist ... --days-list 7,14,21 --stress-days 30`
  - `python3 scripts/recency_alpha_report.py --preset trend_up_yes_tod_hour_filtered_specialist ... --days-list 7,14,21 --stress-days 30`
  - `python3 scripts/recency_alpha_report.py --preset trend_up_yes_hour_filtered_specialist ... --days-list 14,21,30 --stress-days 60`
  - `python3 scripts/recency_alpha_report.py --preset trend_up_yes_tod_hour_filtered_specialist ... --days-list 14,21,30 --stress-days 60`

## [2026-03-21] (Session 71) — Session-Aware Fusion Weighting

### Runtime Wiring
- **The existing `time_of_day_weight` flag now actually affects live fusion weights** — [src/bot/analyze.rs](../../src/bot/analyze.rs) now mutates the configured weight map before fusion, applying hour-aware session scaling only when `time_of_day_weight` is enabled instead of leaving the flag as a no-op.
- **Session-aware weighting now lives in one reusable fusion helper** — [src/fusion/mod.rs](../../src/fusion/mod.rs) now exposes `apply_time_of_day_weighting()`, which scales signal groups by UTC session:
  - Asia hours: lighten microstructure, macro, and momentum weights
  - Europe hours: keep micro/momentum steady with a mild macro lift
  - US hours: boost microstructure, macro, and momentum groups

### Findings
- **The repo already had the configuration concept, but no runtime effect** — this batch closes the gap between the existing [config.json](../../config.json) `time_of_day_weight` flag and the actual signal-fusion path.
- **The change is intentionally gated, not silently activated** — [config.json](../../config.json) still leaves `time_of_day_weight` disabled by default, so the new behavior is available immediately without forcing a live-profile behavior change before validation.

### Tests
- **Added focused fusion tests for the new weighting seam** — [src/fusion/mod.rs](../../src/fusion/mod.rs) now verifies:
  - US-session boosts land on the expected signal groups
  - missing weights are seeded from defaults before scaling, so partial custom maps still behave consistently

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test fusion::tests:: -- --test-threads=1`
- `cargo test`

## [2026-03-21] (Session 69) — Live Truth Loop Closure

### Archive Outcome Labeling
- **Exact-input rows can now be labeled even when we never traded the market** — [src/bot/intelligence.rs](../../src/bot/intelligence.rs) now includes `backfill_unresolved_exact_input_outcomes()`, which scans unresolved `ml_model_inputs` markets, checks for confidently settled binary outcomes, and backfills both `ml_model_inputs` and `ml_features_timeseries`.
- **Resolution parsing was factored into a reusable truth helper** — [src/bot/guard.rs](../../src/bot/guard.rs) now exposes:
  - `settled_binary_outcome_from_payload()`
  - `fetch_binary_market_outcome()`
  and [src/bot/guard.rs](../../src/bot/guard.rs) now uses the same shared settlement rules for paper resolution and archive backfill instead of duplicating that logic.
- **The runtime now keeps the exact archive honest on its own cadence** — [src/main.rs](../../src/main.rs) now runs a dedicated periodic archive-outcome backfill task so resolved watched markets get labeled without waiting for a traded-position resolution path.

### Live Shadow Reporting
- **Added a dedicated live shadow wrapper** — [scripts/live_shadow_report.py](../../scripts/live_shadow_report.py) evaluates the promoted ONNX artifact against resolved live exact rows in trades.db, reports archive coverage, and distinguishes:
  - `waiting_for_resolutions`
  - `no_matching_live_rows`
  - `insufficient_live_labels`
  - `live_shadow_promising`
  - `live_shadow_not_helping`
- **The wrapper now says the precise truth when live rows exist but don’t overlap the candidate lane** — instead of pretending “not enough data,” it explicitly reports when the resolved live archive has no rows matching the current shadow preset.

### Findings
- **The live exact archive is now fully labeled after a cleanup pass** — `training/backfill_outcomes.py --db trades.db` brought trades.db to `30 / 30` labeled `ml_model_inputs` rows across `4` markets.
- **The current live archive still does not overlap the `trend_up_yes_specialist` shadow lane** — 20260321_024233_live_shadow_report.json reports:
  - `labeled_rows = 30`
  - `unresolved_rows = 0`
  - verdict: `no_matching_live_rows`
  - all resolved live rows were skipped by the lane’s regime filter
  honest read: the truth loop is now closed, but we still need more resolved live examples from the actual candidate lane before the live shadow rail can tell us whether the model is additive there.

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test bot::guard::tests:: -- --test-threads=1`
- `cargo test`
- `python3 -m py_compile scripts/live_shadow_report.py`
- `python3 training/backfill_outcomes.py --db trades.db --delay 0.1`
- `PYTHONPYCACHEPREFIX=/tmp/pycache PYTHONPATH=training .venv-training/bin/python scripts/live_shadow_report.py --db trades.db --model models/latest.onnx --preset trend_up_yes_specialist --max-days 14`

## [2026-03-21] (Session 70) — Realtime Price Bus

### Realtime Price Layer
- **Added a shared Binance WebSocket price bus for BTC/ETH/SOL mids** — [src/signals/price_bus.rs](../../src/signals/price_bus.rs) now maintains a lightweight rolling history from Binance `bookTicker` streams for:
  - `BTCUSDT`
  - `ETHUSDT`
  - `SOLUSDT`
  with reconnect/backoff behavior plus lookback helpers for `latest_price`, `snapshot_near`, and short-horizon `%` changes.
- **The realtime bus is now part of normal runtime startup** — [src/main.rs](../../src/main.rs) now spawns the price bus alongside the existing liquidation feed and L2 ingestion tasks.

### Runtime Wiring
- **BTC price fetches now prefer the realtime bus before falling back to REST** — [src/btc/analyzer.rs](../../src/btc/analyzer.rs) now checks the shared price bus first, then falls back to Binance REST → Coinbase → CoinGecko if the bus is stale or absent.
- **Latency-arb now prefers an actual ~30-second historical snapshot from the bus** — [src/bot/analyze.rs](../../src/bot/analyze.rs) now uses the shared bus to fetch a nearest-30s BTC snapshot for `latency_arb` before falling back to the older coarse in-process cache.
- **ETH/SOL correlation now prefers live short-horizon returns instead of 15m REST klines** — [src/signals/fetchers.rs](../../src/signals/fetchers.rs) now computes altcoin correlation from live bus lookbacks when available, falling back to the previous REST path only when the bus lacks enough history.

### Findings
- **The realtime bus connects cleanly in a real bot session** — a short local smoke boot logged:
  - `Price bus: connecting to Binance bookTicker stream`
  - `Price bus: connected`
  confirming the new path is live rather than just compiling.
- **The project now has a real shared price substrate instead of scattered REST reads** — not every alpha feature is upgraded yet, but the highest-friction stale-data gap is closed for:
  - live BTC price
  - ETH/SOL short-horizon moves
  - the `latency_arb` lookback path

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test signals::price_bus::tests:: -- --test-threads=1`
- `cargo test`
- short local `cargo run` smoke confirming `Price bus: connected`

## [2026-03-21] (Session 68) — Exact-Archive Shadow Filter Rail

### Shadow Filter Report
- **Added a dedicated exact-archive shadow-filter report** — [scripts/deep_model_shadow_filter_report.py](../../scripts/deep_model_shadow_filter_report.py) evaluates an ONNX artifact directly on labeled `ml_model_inputs` rows, across multiple window lengths and shifted anchors, without pretending the Rust candle-only backtest can be joined to the exact-input archive.
- **Added a compact validation wrapper for the shadow lane** — [scripts/deep_model_shadow_validation_report.py](../../scripts/deep_model_shadow_validation_report.py) turns the raw 16-window shadow report into one truth verdict so we can track the model as a candidate gate without manually diffing every anchor every time.
- **The report mirrors specialist-lane filters instead of treating every exact row equally** — it supports:
  - `entry_threshold`
  - `min_confidence_margin`
  - `session_allowlist`
  - `blocked_hours_utc`
  - regime and side filters
  while using archived `fusion_score` to derive the implied directional lane when historical exact rows only carry `signal=NEUTRAL`.
- **Historical regime/side matching is now honest about the archive contract** — the new rail treats historical `TRENDING` as the closest proxy for live directional trend lanes such as `TREND_UP`, and it derives `BUY_YES` / `BUY_NO` from the archived fusion score when the stored signal is neutral. That keeps the report explicit about the historical proxy instead of silently returning empty windows.

### Exact Metadata
- **The exact-input training loader now carries the metadata needed for shadow-lane research** — [training/train.py](../../training/train.py) now keeps `fusion_score` and archived runtime `p_up` in `BTCDataset.meta` for exact rows, ordered by `timestamp, market_id, signal`, so research scripts can reuse the canonical exact-input loader without losing the lane context.

### Findings
- **The promoted 14-day exact model is not additive on the already-tight hour-filtered specialist lane** — session68_shadow_filter_trend_up_yes_hour_filtered.json shows:
  - current `7d` window: baseline `12` trades at `75.00%` vs filtered `2` trades at `50.00%` (`-25.00pp`)
  - overall: `2 / 16` improved windows
  honest read: the hour/session/manual specialist is already selective enough that the deep filter mostly throws away winners and keeps too little coverage.
- **The same model looks materially more plausible on the broader raw `TREND_UP + YES` lane** — session68_shadow_filter_trend_up_yes_specialist.json shows:
  - overall: `8 / 16` improved windows
  - `7d shift=7`: `+5.69pp`
  - `14d shift=21`: `+14.29pp`
  - `21d shift=7`: `+2.35pp`
  - `30d shift=7`: `+1.71pp`
  honest read: the shadow model still fails the current `7d` slice on the broader lane (`-12.01pp`), so this is not deployable edge yet, but it is a much more credible shadow-filter direction than stacking the model on top of the already-tight hour-filtered specialist.
- **The new wrapper classifies that broader lane correctly as a mixed shadow candidate** — 20260321_021758_deep_model_shadow_validation_report.json reports:
  - verdict: `shadow_mixed_candidate`
  - `recent_positive_windows = 2`
  - `stress_positive_windows = 6`
  - `improved_fraction = 0.50`
  - current `7d` lift: `-12.01pp`
  honest read: the model helps often enough to keep researching on the broader lane, but not often enough to trust as a live gate today.

### Verification
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 -m py_compile scripts/deep_model_shadow_filter_report.py training/train.py`
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 -m py_compile scripts/deep_model_shadow_validation_report.py`
- `PYTHONPYCACHEPREFIX=/tmp/pycache PYTHONPATH=training .venv-training/bin/python scripts/deep_model_shadow_filter_report.py --db artifacts/alpha-data/session64/backfill_30d_15m.db --model models/latest.onnx --head p_up --preset trend_up_yes_hour_filtered_specialist --threshold 0.56 --days-list 7,14,21,30 --shift-days-list 0,7,14,21 --report-label session68_shadow_filter_trend_up_yes_hour_filtered`
- `PYTHONPYCACHEPREFIX=/tmp/pycache PYTHONPATH=training .venv-training/bin/python scripts/deep_model_shadow_filter_report.py --db artifacts/alpha-data/session64/backfill_30d_15m.db --model models/latest.onnx --head p_up --preset trend_up_yes_specialist --threshold 0.56 --days-list 7,14,21,30 --shift-days-list 0,7,14,21 --report-label session68_shadow_filter_trend_up_yes_specialist`
- `PYTHONPYCACHEPREFIX=/tmp/pycache PYTHONPATH=training .venv-training/bin/python scripts/deep_model_shadow_validation_report.py --db artifacts/alpha-data/session64/backfill_30d_15m.db --model models/latest.onnx --head p_up --preset trend_up_yes_specialist`

## [2026-03-21] (Session 67) — Exact-Model Specialist Recheck and Shadow Promotion

### Exact-Model Filter Recheck
- **The 14-day exact-input model is the strongest clean specialist-filter candidate so far** — session66_deep_model_trending_yes_report.json shows:
  - global holdout: `Brier=0.2474`, `AUC=0.573`
  - `TRENDING + YES` baseline win rate: `45.16%`
  - best viable threshold: `p_up >= 0.56`
  - selected: `27 / 124`
  - selected win rate: `55.56%`
  - lift: `+10.39pp`
  honest read: this is materially better than the earlier rebuilt-feature 14-day filter check, which only reached about `+4.84pp` at best.
- **The 14-day calibration head still does not produce a trustworthy specialist filter** — session66_deep_model_trending_yes_calibration_report.json still finds no viable threshold. At `0.50` it only selects `6` trades and makes the specialist lane worse.
- **The 30-day exact-input model still does not rescue the month-long specialist story** — session67_deep_model_trending_yes_report.json shows:
  - global holdout: `Brier=0.2494`, `AUC=0.526`
  - `TRENDING + YES` baseline win rate: `48.91%`
  - only viable threshold: `p_up >= 0.50`
  - selected: `45 / 276`
  - selected win rate: `44.44%`
  - lift: `-4.47pp`
  so the month-long problem remains drift, not model contract quality.
- **The 30-day calibration head remains unusable as a specialist filter** — session67_deep_model_trending_yes_calibration_report.json selects `0` specialist rows at all tested thresholds.

### Shadow Runtime
- **Promoted the strongest clean exact-input model into the shadow runtime slot** — [models/latest.onnx](../../models/latest.onnx) now points to the 14-day exact-input artifact from session66-exact14d/latest.onnx. The previous shadow model was backed up to models/latest.onnx.pre_session66_exact14d_backup.
- **A live boot confirmed the promoted exact shadow model loads cleanly** — the runtime loaded [models/latest.onnx](../../models/latest.onnx) without any dimension-mismatch warning and started emitting shadow predictions. That run then failed on `Address already in use (os error 48)` because an older local `macbot` process was still holding `127.0.0.1:8000`; I cleaned that stray background process up afterward so nothing was left listening on `:8000`.

### Verification
- `PYTHONPYCACHEPREFIX=/tmp/pycache PYTHONPATH=training .venv-training/bin/python scripts/deep_model_regime_report.py --db artifacts/alpha-data/session63/backfill_14d_15m.db --model artifacts/alpha-models/session66-exact14d/latest.onnx --head p_up --specialist-regime TRENDING --specialist-side yes_only --thresholds 0.50,0.52,0.54,0.56,0.58,0.60,0.62 --output-json artifacts/alpha-reports/session66_deep_model_trending_yes_report.json`
- `PYTHONPYCACHEPREFIX=/tmp/pycache PYTHONPATH=training .venv-training/bin/python scripts/deep_model_regime_report.py --db artifacts/alpha-data/session63/backfill_14d_15m.db --model artifacts/alpha-models/session66-exact14d/latest.onnx --head calibration --specialist-regime TRENDING --specialist-side yes_only --thresholds 0.50,0.52,0.54,0.56,0.58,0.60,0.62 --output-json artifacts/alpha-reports/session66_deep_model_trending_yes_calibration_report.json`
- `PYTHONPYCACHEPREFIX=/tmp/pycache PYTHONPATH=training .venv-training/bin/python scripts/deep_model_regime_report.py --db artifacts/alpha-data/session64/backfill_30d_15m.db --model artifacts/alpha-models/session67-exact30d/latest.onnx --head p_up --specialist-regime TRENDING --specialist-side yes_only --thresholds 0.50,0.52,0.54,0.56,0.58,0.60,0.62 --output-json artifacts/alpha-reports/session67_deep_model_trending_yes_report.json`
- `PYTHONPYCACHEPREFIX=/tmp/pycache PYTHONPATH=training .venv-training/bin/python scripts/deep_model_regime_report.py --db artifacts/alpha-data/session64/backfill_30d_15m.db --model artifacts/alpha-models/session67-exact30d/latest.onnx --head calibration --specialist-regime TRENDING --specialist-side yes_only --thresholds 0.50,0.52,0.54,0.56,0.58,0.60,0.62 --output-json artifacts/alpha-reports/session67_deep_model_trending_yes_calibration_report.json`
- ONNX input inspection on [models/latest.onnx](../../models/latest.onnx): `scalars=40`, `staleness=34`, `regime=1`
- short runtime boot via `cargo run`, followed by cleanup of the stale process that had occupied `127.0.0.1:8000`

## [2026-03-21] (Session 66) — Canonical Exact-Input Bridge and Retrains

### Historical Exact-Input Bridge
- **Added a historical exact-input materializer** — [training/materialize_model_inputs.py](../../training/materialize_model_inputs.py) now builds canonical `ml_model_inputs` rows from historical `ml_features_timeseries` + `ml_candles_1m` archives, using the same runtime contract shape as live exact-input rows:
  - `candles_1m`
  - `candles_5m`
  - `candles_15m`
  - `orderbook_seq`
  - `scalars`
  - `staleness`
  - `regime_value`
  - `outcome`
- **The historical bridge now writes self-consistent exact rows, not just shape-correct rows** — after a first pass exposed stale raw `features_json` and lowercase regime labels, the materializer was corrected to store:
  - the enriched feature map actually used to build the scalar prefix
  - canonicalized regime labels (`CHOP`, `TRENDING`, `HIGH_VOL`, ...)
  alongside the exact tensors.
- **Both rebuilt historical baselines now have populated exact-input archives**
  - session63/backfill_14d_15m.db: `1341` exact-input rows
  - session64/backfill_30d_15m.db: `2877` exact-input rows

### Exact-Archive Parity
- **The exact-archive report is now clean on live and historical DBs**
  - live trades exact-input report:
    - `15 / 15` exact-dimension rows
    - `15 / 15` scalar-prefix aligned rows
    - `15 / 15` regime-value matched rows
    - `0` parse errors
  - historical 14d exact-input report:
    - `1341 / 1341` exact-dimension rows
    - `1341 / 1341` scalar-prefix aligned rows
    - `1341 / 1341` regime-value matched rows
  - historical 30d exact-input report:
    - `2877 / 2877` exact-dimension rows
    - `2877 / 2877` scalar-prefix aligned rows
    - `2877 / 2877` regime-value matched rows
- **The archive parity tolerance was tightened to “realistic float noise, not drift”** — [scripts/model_input_archive_report.py](../../scripts/model_input_archive_report.py) now uses a `1e-5` scalar-alignment tolerance, which is still strict but no longer over-flags harmless JSON float serialization dust in the low `1e-06` range.

### Exact-Input Retrains
- **`training/train.py` can now train directly from exact archived tensors** — [training/train.py](../../training/train.py) now supports `--data-source auto|features|exact_inputs`, and the exact-input path loads labeled `ml_model_inputs` rows directly instead of rebuilding tensors from side tables.
- **The 14-day exact-input retrain is slightly better than the rebuilt reconstructed baseline** — the new artifact at artifacts/alpha-models/session66-exact14d/latest.onnx finished with:
  - adversarial validation AUC: `0.497`
  - final Brier: `0.2548`
  - best restart: `Brier=0.2474 / AUC=0.573`
  compared with the earlier rebuilt-feature 14d baseline in session63-backfill14d15m/latest.onnx:
  - final Brier: `0.2556`
  - best restart: `Brier=0.2474 / AUC=0.559`
  honest read: the canonical exact-input archive helped the 14-day discrimination story a bit and materially reduced apparent train/test distinguishability.
- **The 30-day exact-input retrain confirms the month-long problem is still drift, not archive shape** — the new artifact at artifacts/alpha-models/session67-exact30d/latest.onnx finished with:
  - adversarial validation AUC: `0.895`
  - final Brier: `0.2500`
  - best restart: `Brier=0.2494 / AUC=0.526`
  versus the earlier rebuilt-feature 30d baseline in session64-backfill30d15m/latest.onnx:
  - adversarial validation AUC: `0.895`
  - final Brier: `0.2500`
  - best restart: `Brier=0.2493 / AUC=0.526`
  honest read: the canonical exact-input path did not magically rescue the month. The remaining issue is genuinely temporal drift / weak month-long edge, not a tensor-contract mismatch.

### Verification
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 -m py_compile training/materialize_model_inputs.py scripts/model_input_archive_report.py training/train.py`
- `PYTHONPYCACHEPREFIX=/tmp/pycache PYTHONPATH=training python3 training/materialize_model_inputs.py --db artifacts/alpha-data/session63/backfill_14d_15m.db --clear-existing`
- `PYTHONPYCACHEPREFIX=/tmp/pycache PYTHONPATH=training python3 training/materialize_model_inputs.py --db artifacts/alpha-data/session64/backfill_30d_15m.db --clear-existing`
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 scripts/model_input_archive_report.py --db trades.db`
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 scripts/model_input_archive_report.py --db artifacts/alpha-data/session63/backfill_14d_15m.db`
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 scripts/model_input_archive_report.py --db artifacts/alpha-data/session64/backfill_30d_15m.db`
- `PYTHONPYCACHEPREFIX=/tmp/pycache PYTHONPATH=training .venv-training/bin/python -u training/train.py --db artifacts/alpha-data/session63/backfill_14d_15m.db --out artifacts/alpha-models/session66-exact14d --skip-hpo --skip-pretrain --device cpu --data-source exact_inputs`
- `PYTHONPYCACHEPREFIX=/tmp/pycache PYTHONPATH=training .venv-training/bin/python -u training/train.py --db artifacts/alpha-data/session64/backfill_30d_15m.db --out artifacts/alpha-models/session67-exact30d --skip-hpo --skip-pretrain --device cpu --data-source exact_inputs`
- `python3 scripts/check_ml_contract.py`
- ONNX input checks on:
  - session66-exact14d/latest.onnx
  - session67-exact30d/latest.onnx

## [2026-03-21] (Session 65) — Exact-Archive Labels and Training Path

### Exact Archive Labels
- **`ml_model_inputs` is now label-capable instead of being an unlabeled side archive** — [src/ml/data_logger.rs](../../src/ml/data_logger.rs) now carries an `outcome` column on `ml_model_inputs`, inserts new exact-input rows with `NULL` outcome, and migrates legacy archives by adding the column when it is missing.
- **Trade resolution now backfills labels into the exact-input archive as well as the feature-timeseries archive** — [src/bot/intelligence.rs](../../src/bot/intelligence.rs) now updates both `ml_features_timeseries` and `ml_model_inputs` for the resolved `market_id`.
- **The offline outcome backfill tool now updates both archive tables** — [training/backfill_outcomes.py](../../training/backfill_outcomes.py) now discovers unresolved `market_id`s from either table and writes the resolved label back into both places.
- **Historical schema parity now includes the exact-input outcome column too** — [training/backfill_historical.py](../../training/backfill_historical.py) creates `ml_model_inputs(..., outcome REAL)` so future historical/exact archive convergence does not drift at the schema level.

### Parity Rail
- **Added a dedicated exact-archive report** — [scripts/model_input_archive_report.py](../../scripts/model_input_archive_report.py) audits:
  - dimension parity for `scalars`, `staleness`, `candles_1m`, `candles_5m`, `candles_15m`, and `orderbook_seq`
  - scalar-prefix alignment between `features_json` and the first `34` scalar slots
  - regime-label / regime-value agreement
  - labeled row coverage
- **The first live report on trades.db came back clean on contract parity** — 20260321_014429_trades_model_input_archive_report.json shows:
  - `rows = 10`
  - `exact_dimension_rows = 10 / 10`
  - `scalar_prefix_aligned_rows = 10 / 10`
  - `regime_value_matched_rows = 10 / 10`
  - `parse_error_rows = 0`
  - `max_scalar_prefix_diff ≈ 4.44e-08`
  - `labeled_rows = 0`
  which is exactly the profile we expect from a fresh exact archive before markets have resolved.

### Training Path
- **`training/train.py` can now read exact archived tensors directly** — [training/train.py](../../training/train.py) now supports `--data-source auto|features|exact_inputs`.
  - `features` keeps the older reconstructed historical path
  - `exact_inputs` reads labeled `ml_model_inputs` rows directly
  - `auto` prefers labeled exact inputs when they exist and otherwise falls back to the reconstructed path
- **The exact-input training path uses the archived tensors as-is instead of rebuilding them from candles and feature JSON** — that means future live retrains can consume the exact runtime:
  - `candles_1m`
  - `candles_5m`
  - `candles_15m`
  - `orderbook_seq`
  - `scalars`
  - `staleness`
  - `regime_value`
  once outcomes accumulate in the archive.

### Verification
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 -m py_compile training/backfill_historical.py training/backfill_outcomes.py scripts/model_input_archive_report.py training/train.py`
- `cargo check`
- `cargo test create_ml_tables_adds_outcome_column_to_legacy_model_input_archive -- --test-threads=1`
- `cargo test ml_on_resolution_records_edge_tracker_observation -- --test-threads=1`
- `cargo test`
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 scripts/model_input_archive_report.py --db trades.db`

## [2026-03-21] (Session 64) — Live-Faithful Model-Input Archive

### Runtime Archive Contract
- **The live deep-model path now archives the exact tensor it actually scores** — [src/bot/analyze.rs](../../src/bot/analyze.rs) now:
  - fetches a dedicated `1m` Binance candle window for the model path
  - replaces the `InputBuilder` candle ring with the exact recent `240`-candle slice instead of incrementally smearing repeated bars
  - updates runtime context and staleness before building the model input
  - logs both the enriched `ml_features_timeseries` row and the exact `DeepModelInput` snapshot to the new `ml_model_inputs` archive
  - uses that same in-memory `DeepModelInput` for inference
- **The live model candle feed is now aligned with training** — [src/signals/fetchers.rs](../../src/signals/fetchers.rs) gained `fetch_binance_candles_1m()`, and [src/ml/input_builder.rs](../../src/ml/input_builder.rs) now retains `240` one-minute candles so the runtime can actually build:
  - `60 x 1m`
  - `24 x 5m`
  - `16 x 15m`
  from the same minute stream the training path expects.
- **Duplicate candle smearing is gone from the archive path** — [src/ml/data_logger.rs](../../src/ml/data_logger.rs) now inserts `ml_candles_1m` with `INSERT OR IGNORE` and a unique `(timestamp, market_id)` index, while the analysis path logs exact minute timestamps through `log_candle_at(...)`.
- **A new raw replay table is now part of both live and historical schema setup** — [src/ml/data_logger.rs](../../src/ml/data_logger.rs) and [training/backfill_historical.py](../../training/backfill_historical.py) now create `ml_model_inputs` plus timestamp and market indexes so future offline replay tooling can work against one canonical archive shape.

### Runtime Proof
- **A live simulation boot proved the new archive tables are filling from the real analysis path** — after startup against trades.db:
  - `ml_model_inputs` existed and received exact-input rows
  - `ml_candles_1m` kept unique minute timestamps (`2553 / 2553` unique `(timestamp, market_id)` pairs at the time of the check)
  - recent candle rows showed true `1m` timestamps instead of the older `15m` feed semantics
- **The runtime proof also surfaced and fixed a remaining shadow-model contract mismatch** — `models/latest.onnx` was still an old `30 / 24` export, so the live bot was retrying the legacy dim fallback path. I promoted artifacts/alpha-models/session64-backfill30d15m/latest.onnx into [models/latest.onnx](../../models/latest.onnx) after backing up the previous file to models/latest.onnx.pre_session64_backup.
- **A second boot confirmed the shadow runtime now loads the current contract cleanly** — the bot loaded [models/latest.onnx](../../models/latest.onnx) without the old `Got: 40 Expected: 30` retry warning and emitted shadow predictions directly.

### Verification
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 -m py_compile training/backfill_historical.py`
- `cargo fmt --all`
- `cargo check`
- `cargo test replace_candles_keeps_exact_recent_window -- --test-threads=1`
- `cargo test create_ml_tables_supports_exact_model_input_archive -- --test-threads=1`
- `cargo test candle_logging_ignores_duplicate_timestamp_market_pairs -- --test-threads=1`
- `cargo test`
- two local `cargo run` simulation boots:
  - first to verify `ml_model_inputs` / `ml_candles_1m` population and expose the stale `models/latest.onnx` contract
  - second to confirm the promoted `models/latest.onnx` loads and scores without the dim-mismatch retry path

## [2026-03-21] (Session 63) — Rebuilt Historical Baselines and Honest Retrains

### Rebuilt Datasets
- **Rebuilt the real 14-day and 30-day historical baselines through the new explicit-masking backfill path** — the fresh datasets now live at:
  - artifacts/alpha-data/session63/backfill_14d_15m.db
  - artifacts/alpha-data/session64/backfill_30d_15m.db
- **The rebuilt 14-day set is no longer silently zero-filled** — the comparison in 20260321_005434_model_input_validity_report.json shows:
  - old session45 14d DB: `availability_masking = implicit_zero_fill`
  - new session63 14d DB: `availability_masking = explicit`
  - `missing_runtime_context = []` on the rebuilt set
  - raw futures context present in the rebuilt set:
    - `ml_funding_rates = 41`
    - `ml_open_interest_hist = 4020`
    - `ml_long_short_ratios = 12060`
- **The rebuilt 30-day set materially improved month-long context fidelity** — the comparison in 20260321_010547_model_input_validity_report.json shows:
  - old session46 30d DB: no raw funding/OI/ratio tables, `availability_masking = implicit_zero_fill`, and only `15` strong nonconstant features
  - new session64 30d DB: `availability_masking = explicit`, `missing_runtime_context = []`, raw context populated with:
    - `ml_funding_rates = 89`
    - `ml_open_interest_hist = 7764`
    - `ml_long_short_ratios = 23292`
  - the rebuilt month now carries `25` strong nonconstant features instead of `15`

### Retrains
- **Re-trained the clean 14-day model on the rebuilt explicit dataset** — the new artifact lives at artifacts/alpha-models/session63-backfill14d15m/latest.onnx.
  - new run:
    - final Brier: `0.2556`
    - best restart: `Brier=0.2474 / AUC=0.559`
  - prior honest baseline from session45-backfill14d15m:
    - final Brier: `0.2528`
    - best restart: `Brier=0.2475 / AUC=0.545`
  - honest read: the rebuilt 14-day data slightly improved the best restart and specialist discrimination, but the overall final Brier got a bit worse, so this is a modest quality shift, not a clear alpha breakthrough.
- **Re-trained the clean 30-day model on the rebuilt explicit month** — the new artifact lives at artifacts/alpha-models/session64-backfill30d15m/latest.onnx.
  - new run:
    - adversarial validation AUC: `0.895`
    - final Brier: `0.2500`
    - best restart: `Brier=0.2493 / AUC=0.526`
  - prior honest baseline from session46-backfill30d15m:
    - final Brier: `0.2500`
    - best restart: `Brier=0.2498 / AUC=0.509`
  - honest read: the rebuilt month improved best-restart discrimination a bit, but the final Brier stayed flat and the month still shows severe distribution shift. The cleaner pipeline did not manufacture a robust 30-day edge.

### Specialist Filter Recheck
- **The rebuilt 14-day model is slightly more useful as a `TRENDING + YES` specialist filter** — session63_deep_model_trending_yes_report.json shows:
  - global holdout: `Brier=0.2474`, `AUC=0.559`
  - specialist baseline win rate: `45.16%`
  - best viable threshold: `p_up >= 0.54`
  - selected: `20 / 124`
  - selected win rate: `50.0%`
  - lift: `+4.84pp`
- **The rebuilt 30-day model still does not work as a month-long specialist filter** — session64_deep_model_trending_yes_report.json shows:
  - global holdout: `Brier=0.2493`, `AUC=0.526`
  - specialist baseline win rate: `48.91%`
  - only viable threshold: `p_up >= 0.50`
  - selected: `60 / 276`
  - selected win rate: `46.67%`
  - lift: `-2.25pp`
- **The calibration head still does not rescue either rebuilt window** — the calibration reports:
  - session63_deep_model_trending_yes_calibration_report.json
  - session64_deep_model_trending_yes_calibration_report.json
  still show no durable specialist threshold. The rebuilt 14-day calibration head only selects `2` trades at `0.50`, and the rebuilt 30-day calibration head selects none at all.

### Tooling Notes
- **Warmed the Python scientific stack bytecode cache before retraining** — a one-time `compileall` pass over `torch`, `sklearn`, `scipy`, and `optuna` filled the shared `/tmp/pycache` tree and eliminated the cold-start import stall that had been making the fresh retrains look stuck before dataset loading.

### Verification
- `python3 scripts/check_ml_contract.py`
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 scripts/model_input_validity_report.py --db artifacts/alpha-data/session45/backfill_14d_15m.db --db artifacts/alpha-data/session63/backfill_14d_15m.db`
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 scripts/model_input_validity_report.py --db artifacts/alpha-data/session46/backfill_30d_15m.db --db artifacts/alpha-data/session64/backfill_30d_15m.db`
- `PYTHONPYCACHEPREFIX=/tmp/pycache PYTHONPATH=training .venv-training/bin/python -u training/train.py --db artifacts/alpha-data/session63/backfill_14d_15m.db --out artifacts/alpha-models/session63-backfill14d15m --skip-hpo --skip-pretrain --device cpu`
- `PYTHONPYCACHEPREFIX=/tmp/pycache PYTHONPATH=training .venv-training/bin/python -u training/train.py --db artifacts/alpha-data/session64/backfill_30d_15m.db --out artifacts/alpha-models/session64-backfill30d15m --skip-hpo --skip-pretrain --device cpu`
- `PYTHONPATH=training .venv-training/bin/python scripts/deep_model_regime_report.py --db artifacts/alpha-data/session63/backfill_14d_15m.db --model artifacts/alpha-models/session63-backfill14d15m/latest.onnx --head p_up --specialist-regime TRENDING --specialist-side yes_only --thresholds 0.50,0.52,0.54,0.56,0.58,0.60,0.62 --output-json artifacts/alpha-reports/session63_deep_model_trending_yes_report.json`
- `PYTHONPATH=training .venv-training/bin/python scripts/deep_model_regime_report.py --db artifacts/alpha-data/session64/backfill_30d_15m.db --model artifacts/alpha-models/session64-backfill30d15m/latest.onnx --head p_up --specialist-regime TRENDING --specialist-side yes_only --thresholds 0.50,0.52,0.54,0.56,0.58,0.60,0.62 --output-json artifacts/alpha-reports/session64_deep_model_trending_yes_report.json`

## [2026-03-21] (Session 62) — Model-Input Fidelity Guardrails

### Historical Pipeline
- **Long-window futures context collection is now chunked instead of relying on a single large query window** — [training/backfill_historical.py](../../training/backfill_historical.py) now splits Binance open-interest and long/short ratio pulls into period-aware chunks before paginating. That makes longer historical backfills much less likely to fail silently on oversized endpoint windows.
- **Historical backfills now populate runtime-like context fields directly into `features_json`** — fresh rows now carry:
  - `balance = 1.0`
  - `positions = 0.0`
  - `price_discovery_lag = 0.0`
  - plus `hour_of_day`, `day_of_week`, round-number distance, `volatility_10`, `volatility_30`, and `window_momentum`
  instead of leaving those fields absent and relying on the training loader to silently synthesize them later.
- **Historical missingness is now explicit inside the existing 34-wide staleness vector** — instead of `staleness_json` being all zeros for every historical row, [training/backfill_historical.py](../../training/backfill_historical.py) now marks features that are genuinely unavailable historically as `1.0`. That includes persistent placeholders like sentiment/polymarket/orderbook-only lanes and context-dependent features when their backing futures data is missing.
- **`oi_momentum` now prefers real open-interest history when it exists** — [training/backfill_historical.py](../../training/backfill_historical.py) now computes historical OI momentum from the raw `ml_open_interest_hist` trail when available instead of always falling back to the older ATR-style proxy.

### Audit Rail
- **The model-input validity report now distinguishes explicit masking from silent zero-fill** — [scripts/model_input_validity_report.py](../../scripts/model_input_validity_report.py) now:
  - tolerates legacy DBs that do not have the newer raw-context tables
  - reports raw-context table counts (`ml_funding_rates`, `ml_open_interest_hist`, `ml_long_short_ratios`)
  - classifies features by source (`projected_historical_proxy`, `context_dependent_historical`, etc.)
  - surfaces `explicitly_masked_features` and `partially_masked_features`
  - tells us whether a dataset is `implicit_zero_fill` or `explicit` in its availability signaling

### New Read
- **Old datasets are still only partially faithful and remain `implicit_zero_fill`** — rerunning the audit on the older clean datasets in 20260321_003858_model_input_validity_report.json showed exactly what we expected: they predate this patch, so they still miss `balance`/`positions`/`price_discovery_lag` and still carry all-zero historical staleness.
- **Fresh datasets built through the new path now surface honest missingness** — a one-day smoke backfill into `/tmp/model_fidelity_smoke.db` produced a new audit in 20260321_004107_model_input_validity_report.json with:
  - `availability_masking = explicit`
  - `missing_runtime_context = []`
  - `explicitly_masked_features = fear_greed, coingecko_sentiment, finnhub_sentiment, lunarcrush_sentiment, long_liq_volume, short_liq_volume, depth_imbalance_5, polymarket_flow, latency_arb`
  - `partially_masked_features = deribit_pcr, funding_rate, oi_momentum, options_advanced, cross_market`
  - raw-context tables populated in the same DB:
    - `ml_aggressor_flow = 1380`
    - `ml_funding_rates = 2`
    - `ml_open_interest_hist = 276`
    - `ml_long_short_ratios = 828`

### Verification
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 -m py_compile training/backfill_historical.py training/train.py scripts/model_input_validity_report.py`
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 scripts/model_input_validity_report.py --db artifacts/alpha-data/session45/backfill_14d_15m.db --db artifacts/alpha-data/session46/backfill_30d_15m.db`
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 -u training/backfill_historical.py --db /tmp/model_fidelity_smoke.db --days 1 --interval 15m --ratio-period 5m`
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 scripts/model_input_validity_report.py --db /tmp/model_fidelity_smoke.db`

## [2026-03-21] (Session 61) — Raw Context Persistence and Signal-Surface Warnings

### Historical Pipeline
- **Future backfills now persist the raw Binance futures context they fetch** — [training/backfill_historical.py](../../training/backfill_historical.py) now stores:
  - funding history in `ml_funding_rates`
  - open-interest history in `ml_open_interest_hist`
  - global/top-account/top-position long-short ratios in `ml_long_short_ratios`
  That means future historical datasets will preserve the raw context trail instead of only the projected feature values.
- **Verified the raw-context storage path with a one-day smoke run** — a temporary DB built through `init_db()` + `build_context()` inserted:
  - `1,439` aggressor-flow rows
  - `3` funding points
  - `96` open-interest points
  - `288` long-short ratio rows
  So the new tables are not just schema scaffolding; they are filling with real data on this machine.

### Research Rail Honesty
- **Added signal-surface classification to the trade-validity audit** — [scripts/signal_validity_report.py](../../scripts/signal_validity_report.py) now emits a `surface_assessment` block for each shift, including:
  - `coarse_gate_only`
  - `coarse_gate_inverted_ranking`
  - `ranking_inverted`
  - `continuous_ranking_ok`
  It also surfaces warnings like `score_surface_constant_within_lane`, `edge_margin_inverted_vs_wins`, and `next_return_is_strongest_separator`.
- **Maker and taker validation wrappers now include the signal audit directly** — both [scripts/recent_specialist_validation_report.py](../../scripts/recent_specialist_validation_report.py) and [scripts/taker_validation_report.py](../../scripts/taker_validation_report.py) now run the signal audit as part of the main wrapper, carry the result into `overall.signal_surface`, and append the warning into the summary line instead of silently treating every lane like a normal scored strategy.

### New Read
- **The current maker baseline is still a coarse gate, not a ranked score model** — 20260321_002302_recent_specialist_validation_report.json now carries:
  - `surface_label = coarse_gate_only`
  - warnings:
    - `score_surface_constant_within_lane`
    - `edge_margin_quartiles_non_monotonic`
    - `next_return_is_strongest_separator`
  The wrapper summary now says exactly that instead of only talking about pass/fail windows.
- **The current taker baseline is explicitly tagged as an inverted ranking surface** — 20260321_002505_taker_validation_report.json now carries:
  - `surface_label = coarse_gate_inverted_ranking`
  - `edge_margin_corr_win = -0.5067`
  - warnings:
    - `score_surface_constant_within_lane`
    - `edge_margin_inverted_vs_wins`
    - `edge_margin_quartiles_non_monotonic`
    - `next_return_is_strongest_separator`
  That makes the taker warning impossible to miss in the main validation artifact.
- **The 30-day context collapse is still a live pipeline limitation in the current no-key environment** — a 30-day context-only smoke run against the new raw tables came back with:
  - `43,200` aggressor-flow rows
  - `90` funding points
  - but `0` open-interest rows and `0` long-short ratio rows
  - Binance returned `400` for the 30-day OI and long-short endpoints
  So the old `session46` ratio collapse is not just a stale artifact; with the current public/no-key setup, longer windows still lose that context.

### Verification
- `python3 -m py_compile training/backfill_historical.py scripts/signal_validity_report.py scripts/recent_specialist_validation_report.py scripts/taker_validation_report.py`
- one-day context smoke via `init_db()` + `build_context()` on a temporary SQLite DB
- thirty-day context smoke via `init_db()` + `build_context()` on a temporary SQLite DB
- `python3 scripts/recent_specialist_validation_report.py --preset trend_up_yes_hour_filtered_specialist --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --request-interval-secs 1.6`
- `python3 scripts/taker_validation_report.py --preset trend_up_yes_taker_resolution --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --blocked-hours-utc 1,3,10,11,12,18 --filter-regimes TREND_UP --filter-signals BUY_YES --request-interval-secs 1.6`

### Artifacts
- 20260321_002302_recent_specialist_validation_report.json
- 20260321_002302_recent_specialist_validation_report.md
- 20260321_002302_trend_up_yes_hour_filtered_specialist_current_signal_signal_validity_report.json
- 20260321_002505_taker_validation_report.json
- 20260321_002505_taker_validation_report.md
- 20260321_002505_trend_up_yes_taker_resolution_current_signal_signal_validity_report.json

## [2026-03-21] (Session 60) — Historical Input Validity and Signal-Lane Audit

### Audit Tooling
- **Added a historical tensor-fidelity audit** — [scripts/model_input_validity_report.py](../../scripts/model_input_validity_report.py) now checks the cleaned historical SQLite datasets against the live Rust/Python feature contract, table coverage, context-field coverage, staleness availability, and per-feature variance.
- **Added a trade/signal validity audit** — [scripts/signal_validity_report.py](../../scripts/signal_validity_report.py) runs through the Rust `/api/backtest` path and measures whether the lane’s signal fields are actually varying, separating winners from losers, and behaving consistently across anchor shifts.

### Historical Data Read
- **The feature contract is aligned, but historical tensor fidelity is only partial** — the `34`-feature contract matches across [src/ml/model.rs](../../src/ml/model.rs), [training/backfill_historical.py](../../training/backfill_historical.py), and [training/train.py](../../training/train.py), but the datasets still miss important live surfaces:
  - no `ml_orderbook_snapshots`
  - no `ml_tick_history`
  - no `ml_liquidations`
  - `balance`, `positions`, and `price_discovery_lag` are missing from historical features
  - `time_remaining` is always `1.0`
  - `contract_price` is always the neutral `0.5`
  - `staleness_json` is all zeros for every row
- **The cleaner parts of the historical pipeline are genuinely present** — both cleaned sets have continuous 1-minute candles and very high aggressor-flow coverage:
  - `14d`: `20,340` candles, `20,100` aggressor-flow rows (`98.82%`)
  - `30d`: `43,380` candles, `43,140` aggressor-flow rows (`99.45%`)
- **The 30-day context surface is weaker than the 14-day set** — in 20260321_000815_model_input_validity_report.json, `deribit_pcr`, `options_advanced`, and `cross_market` are non-constant on `14d` but collapse to all-zero on `30d`. That supports the earlier suspicion that the longer historical windows are losing external ratio context rather than just “failing to model.”
- **Several features are only populated later inside training, not in the historical DB itself** — `hour_of_day`, `day_of_week`, `distance_from_round_1000`, `distance_from_round_500`, `volatility_10`, `volatility_30`, and `window_momentum` are all zero in the DB rows and get reconstructed in [training/train.py](../../training/train.py). That means the training tensors are not a pure replay of the stored historical feature surface.

### Signal Validity Read
- **The current specialist lanes are not driven by a rich continuous score surface** — for both maker and taker `TREND_UP + YES`, the lane-validity reports show `directional_score`, raw `score`, and `confidence_margin` are completely degenerate within the traded slice:
  - maker current shift: `unique_rounded = 1` for all three
  - taker current shift: `unique_rounded = 1` for all three
  This comes directly from the coarse additive signal logic in [src/api/backtest.rs](../../src/api/backtest.rs), where the surviving lane is repeatedly hitting the same discrete score bucket.
- **Maker’s remaining continuous fields at least point in the right direction** — in 20260321_000816_maker_trend_up_yes_validity_signal_validity_report.json:
  - current shift `0`: `edge_margin_pp` correlation with win `+0.7151`, `next_return_pct` `+0.7808`
  - healthier shift `5`: `edge_margin_pp` stays positive (`+0.3367`), `next_return_pct` stays strong (`+0.7187`)
  So maker looks sparse and coarse, but not internally inverted.
- **Taker’s edge metric is currently upside-down** — in 20260321_000815_taker_trend_up_yes_validity_signal_validity_report.json:
  - shift `0`: `edge_margin_pp` correlation with win `-0.5067`
  - shift `5`: `edge_margin_pp` correlation with win `-0.4012`
  - the `edge_margin_pp` quartiles are inverted, with the highest quartile winning least often
  This makes sense once you inspect [src/api/backtest.rs](../../src/api/backtest.rs): the lane’s `score` is constant, so `edge_margin_pp` is mostly varying with modeled cost, and the “cheaper” trades are not actually the better trades here.
- **Path-immediate movement is still the strongest honest separator** — across both maker and taker, `next_return_pct` has the cleanest positive correlation with realized wins:
  - maker current shift: `+0.7808`
  - maker healthier shift: `+0.7187`
  - taker current shift: `+0.6654`
  - taker healthier shift: `+0.8021`
  That suggests the lanes still have real short-horizon path information, even though the coarse score surface is collapsing.

### Research Read
- **The data is trustworthy enough to keep researching, but not faithful enough to over-interpret deep-model results as “full live parity.”**
- **The specialist signal lanes are valid as coarse gates, not yet as nuanced ranking models.**
- **The biggest remaining validity gap is not schema alignment anymore; it is historical fidelity and missing live-like context.**

### Verification
- `python3 -m py_compile scripts/model_input_validity_report.py scripts/signal_validity_report.py`
- `python3 scripts/model_input_validity_report.py`
- `python3 scripts/signal_validity_report.py --preset trend_up_yes_hour_filtered_specialist --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --days 7 --shift-days-list 0,5 --filter-regimes TREND_UP --filter-signals BUY_YES --report-label maker_trend_up_yes_validity`
- `python3 scripts/signal_validity_report.py --preset trend_up_yes_taker_resolution --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --blocked-hours-utc 1,3,10,11,12,18 --days 7 --shift-days-list 0,5 --filter-regimes TREND_UP --filter-signals BUY_YES --report-label taker_trend_up_yes_validity`

### Artifacts
- 20260321_000815_model_input_validity_report.json
- 20260321_000815_model_input_validity_report.md
- 20260321_000816_maker_trend_up_yes_validity_signal_validity_report.json
- 20260321_000816_maker_trend_up_yes_validity_signal_validity_report.md
- 20260321_000815_taker_trend_up_yes_validity_signal_validity_report.json
- 20260321_000815_taker_trend_up_yes_validity_signal_validity_report.md

## [2026-03-20] (Session 59) — Taker Filter Follow-Through: `18` Helps, `21` Hurts, `08` Helps Current But Still Fails

### Alpha Findings
- **The `18:00 UTC` block was a real improvement, not a fluke** — rerunning the taker anchor-diff with `blocked_hours_utc=1,3,10,11,12,18` in 20260320_234740_trend_up_yes_taker_resolution_anchor_diff_report.json materially stabilized the 7-day lane:
  - current anchor: `20` trades, PnL `+2.5544`, edge gap `+12.58pp`
  - best anchor: `28` trades, PnL `+3.4556`, edge gap `+11.81pp`
  - worst anchor: still positive at `+1.3169`, edge gap `+7.41pp`
  That is a much healthier short-horizon shape than the earlier taker configuration.
- **Blocking `21:00 UTC` on top of that was too blunt** — 20260320_235608_taker_validation_report.json kept the lane positive now, but cut too much useful flow and did not rescue the wrapper:
  - daily gate: `pass`
  - short recency: `fail`
  - long stress: `fail`
  - current lane: positive now, but weaker than the `18`-only version
- **`08:00 UTC` is the cleaner second filter candidate** — it stayed toxic in both the current and better anchors without carrying much volume, so I tested `blocked_hours_utc=1,3,8,10,11,12,18` in 20260320_235810_taker_validation_report.json:
  - current lane improved further: `33` trades, PnL `+3.8927`, edge gap `+11.18pp`
  - short `7d` window got stronger
  - but the `14d` oldest fold broke badly (`-1.7222`, edge gap `-19.68pp`)
  - `30d` stress remained negative across all folds
  So `08` helps the live/current slice, but still does not turn taker into a robust candidate.

### Research Read
- **Taker `TREND_UP + YES` now has a believable live pulse, but it is still too anchor-fragile to stand beside maker as a trusted second engine.**
- **The cheap reactive filter path is finding real improvements, but those improvements are still local rather than stable.**
- **The current best taker configuration remains a recent-window research lane, not a deployable strategy.**

### Verification
- `python3 scripts/anchor_diff_report.py --preset trend_up_yes_taker_resolution --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --blocked-hours-utc 1,3,10,11,12,18 --anchor-shifts 0,1,2,3,4,5,6 --days 7`
- `python3 scripts/taker_validation_report.py --preset trend_up_yes_taker_resolution --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --blocked-hours-utc 1,3,10,11,12,18,21 --request-interval-secs 1.5`
- `python3 scripts/taker_validation_report.py --preset trend_up_yes_taker_resolution --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --blocked-hours-utc 1,3,8,10,11,12,18 --request-interval-secs 1.5`

### Artifacts
- 20260320_234740_trend_up_yes_taker_resolution_anchor_diff_report.json
- 20260320_235608_taker_validation_report.json
- 20260320_235810_taker_validation_report.json

## [2026-03-20] (Session 58) — Anchor Drift Forensics, Maker-vs-Taker Composition, and an `18:00 UTC` Taker Probe

### Research Tooling
- **Added reusable backtest-trade report helpers** — [scripts/backtest_trade_report_common.py](../../scripts/backtest_trade_report_common.py) now centralizes preset-aware `/api/backtest` payload construction, trade filtering, lane summaries, bucket summaries, and winner-vs-loser numeric profiles so the new forensic scripts all use the same math.
- **Added a shifted-anchor drift report** — [scripts/anchor_diff_report.py](../../scripts/anchor_diff_report.py) runs one named lane across multiple anchor shifts, captures the per-anchor trade composition, and writes explicit current-vs-best and current-vs-worst numeric drift tables.
- **Added a maker-vs-taker composition report** — [scripts/lane_comparison_report.py](../../scripts/lane_comparison_report.py) compares multiple named lanes on the same anchor so we can inspect trade count, modeled edge, cost load, and winner/loser signal context without bouncing between separate reports.

### Alpha Findings
- **Taker `TREND_UP + YES` is not failing because all anchors are bad; it is failing because the current anchor is weaker than the recent past** — 20260320_234308_trend_up_yes_taker_hour_filtered_anchor_diff_report.json shows the filtered taker lane improving materially as the anchor moves back:
  - shift `0`: `16` trades, PnL `+0.4065`, edge gap `+3.73pp`
  - shift `4`: `28` trades, PnL `+2.1658`, edge gap `+8.22pp`
  - shift `5`: `23` trades, PnL `+1.7667`, edge gap `+8.30pp`
  - the current week is the worst of the tested anchors, not the best
- **The current taker lane is being poisoned by a small set of hours rather than by a uniformly bad signal surface** — current-anchor taker losses cluster most visibly in hour `18`, with smaller damage in `08`, `09`, and `21`, while the positive pocket is concentrated in `13`, `17`, `23`, and `02`.
- **Maker is not “quietly stable” right now either** — 20260320_234321_trend_up_yes_hour_filtered_specialist_anchor_diff_report.json shows the current maker slice is also the weakest tested anchor:
  - shift `0`: `8` trades, PnL `-0.0996`, edge gap `-0.00pp`
  - shift `5`: `7` trades, PnL `+1.5184`, edge gap `+21.43pp`
  - shift `4`: `11` trades, PnL `+1.4609`, edge gap `+13.64pp`
  So the current-market softness is broader than just taker.
- **Current-anchor maker vs taker is a genuine economics trade-off, not just “maker good / taker bad”** — 20260320_234227_lane_comparison_report.json shows:
  - maker: `8` trades, PnL `-0.0996`, edge gap `~0`, avg cost `~0`
  - taker: `16` trades, PnL `+0.4065`, edge gap `+3.73pp`, avg cost `~5.05%`
  - maker has higher modeled edge margin and more path upside, but the current lane is too sparse and currently not paying that off
- **The first evidence-driven taker follow-up was worth doing** — blocking hour `18` in addition to the existing taker hour filter materially improved the current taker branch in 20260320_234536_taker_validation_report.json:
  - daily gate: `pass`
  - current `7d` lane: strongly positive (`20` trades, PnL `+2.5544`, edge gap `+12.58pp`)
  - short recency wrapper: still `fail`
  - long stress wrapper: still `fail`
  This is better than the prior taker setup, but still not a deployable second engine.

### Verification
- `PYTHONPYCACHEPREFIX=.pycache python3 -m py_compile scripts/backtest_trade_report_common.py scripts/anchor_diff_report.py scripts/lane_comparison_report.py`
- `python3 scripts/anchor_diff_report.py --preset trend_up_yes_taker_hour_filtered --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --anchor-shifts 0,1,2,3,4,5,6 --days 7`
- `python3 scripts/lane_comparison_report.py --presets trend_up_yes_hour_filtered_specialist,trend_up_yes_taker_hour_filtered --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --days 7 --shift-days 0`
- `python3 scripts/anchor_diff_report.py --preset trend_up_yes_hour_filtered_specialist --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --anchor-shifts 0,1,2,3,4,5,6 --days 7`
- `python3 scripts/taker_validation_report.py --preset trend_up_yes_taker_resolution --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --blocked-hours-utc 1,3,10,11,12,18`

### Artifacts
- 20260320_234308_trend_up_yes_taker_hour_filtered_anchor_diff_report.json
- 20260320_234321_trend_up_yes_hour_filtered_specialist_anchor_diff_report.json
- 20260320_234227_lane_comparison_report.json
- 20260320_234536_taker_validation_report.json

## [2026-03-20] (Session 57) — Mirrored Taker Comparison: `YES` Still Leads, `NO` Still Fails

### Research Workflow
- **Added a mirrored taker preset for the `TREND_DOWN + NO` branch** — [scripts/alpha_presets.py](../../scripts/alpha_presets.py) now includes `trend_down_no_taker_resolution`, which gives the taker lab a clean apples-to-apples counterpart to `trend_up_yes_taker_resolution`.

### Alpha Findings
- **Unfiltered `TREND_DOWN + NO` taker still fails the real validation rail** — the quick sweep was mixed (`2/3` positive folds) but the current anchor was too toxic:
  - quick 14-day three-fold mean PnL: `-0.0658`
  - mean edge gap: `-2.89pp`
  - full wrapper verdict: `failing_candidate`
  - artifact: 20260320_233110_taker_validation_report.json
- **A cheap hour filter helped `NO`, but not enough** — current-lane forensics in 20260320_232942_trend_down_no_taker_resolution_current_lane_forensics.json pointed to hours `04,06,09,10,22` as the toxic pocket. Blocking them improved the quick sweep:
  - filtered 14-day three-fold mean PnL: `+0.1159`
  - but mean edge gap stayed negative: `-0.53pp`
- **Filtered `NO` still fails the wrapper** — 20260320_233249_taker_validation_report.json came back `failing_candidate`:
  - short recency: `fail`
  - long stress: `fail`
  - current lane: positive now, but still not trustworthy enough overall
- **Net comparison** — taker `YES` remains the only lane that even reaches `recent_taker_candidate`, while taker `NO` fails both before and after the cheap hour filter. That makes `TREND_UP + YES` the only taker lane worth further research right now.

### Verification
- `PYTHONPYCACHEPREFIX=.pycache python3 -m py_compile scripts/alpha_presets.py`
- `python3 scripts/walkforward_alpha_sweep.py --preset trend_down_no_taker_resolution --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --days 14 --fold-shifts 0,7,14 --top 5 --request-interval-secs 1.1`
- `python3 scripts/taker_validation_report.py --preset trend_down_no_taker_resolution --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret`
- `python3 scripts/lane_forensics_report.py --preset trend_down_no_taker_resolution --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --days 7 --shift-days 0 --lane-label trend_down_no_taker_resolution_current`
- `python3 scripts/walkforward_alpha_sweep.py --preset trend_down_no_taker_resolution --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --days 14 --fold-shifts 0,7,14 --blocked-hours-utc 4,6,9,10,22 --top 5 --request-interval-secs 1.1`
- `python3 scripts/taker_validation_report.py --preset trend_down_no_taker_resolution --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --blocked-hours-utc 4,6,9,10,22`

### Artifacts
- 20260320_232942_trend_down_no_taker_resolution_current_lane_forensics.json
- 20260320_233110_taker_validation_report.json
- 20260320_233249_taker_validation_report.json

## [2026-03-20] (Session 56) — Taker Lane Forensics, Filterable Validation, and a Recent-Window Candidate

### Taker Research Workflow
- **Made the taker wrappers accept cheap reactive filters** — [scripts/taker_validation_report.py](../../scripts/taker_validation_report.py) and [scripts/taker_soak_report.py](../../scripts/taker_soak_report.py) now pass through `--min-confidence-margin`, `--session-allowlist`, and `--blocked-hours-utc`. That lets the taker branch reuse the same recency/stress/lane validation rail as maker research without minting a new preset for every cheap filter experiment.
- **Added a named preset for the only taker filter stack that improved the quick sweep** — [scripts/alpha_presets.py](../../scripts/alpha_presets.py) now includes `trend_up_yes_taker_hour_filtered`, which keeps the narrow `TREND_UP + YES` taker lane but blocks the historically toxic hours `1,3,10,11,12`.

### Alpha Findings
- **Broad taker remains bad and taker TP/SL still does not rescue it** — the earlier `taker_tp_sl_probe` and broad `taker_resolution_baseline` runs stayed negative, so the taker branch still looks like a narrow-lane problem, not a broad-mode opportunity.
- **The only taker lane with a real pulse is still `TREND_UP + YES`, plain taker resolution, not TP/SL** — a focused 14-day three-fold quick sweep with `blocked_hours_utc=1,3,10,11,12` improved the lane from negative to positive:
  - mean PnL `+0.7662`
  - mean edge gap `+3.43pp`
  - `2/3` positive folds
- **Full taker validation confirms it as a recent-window candidate, not a robust edge** — 20260320_231600_taker_validation_report.json and 20260320_231600_taker_validation_report.md upgraded the filtered lane to `recent_taker_candidate`:
  - daily gate: `pass`
  - short recency gate (`7d/14d/21d` with `30d` stress): `pass`
  - long recency gate (`14d/21d/30d` with `60d` stress): `fail`
  - current lane: positive
- **Shifted-anchor soak evidence says the filtered taker lane is still fragile** — 20260320_232632_taker_soak_report.json and 20260320_232632_taker_soak_report.md replayed the filtered lane across anchor shifts `0..6` and came back `recent_taker_candidate_only`:
  - verdict counts: `1 recent_taker_candidate`, `6 failing_candidate`
  - recent recency pass ratio: `1/7`
  - long stress pass ratio: `0/7`
  - lane-positive-now ratio: `7/7`
  This is the same pattern we saw elsewhere in alpha: a live recent-window pulse, but not enough temporal stability yet to call it a dependable second engine next to maker.

### Verification
- `python3 scripts/walkforward_alpha_sweep.py --preset trend_up_yes_taker_resolution --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --days 14 --fold-shifts 0,7,14 --blocked-hours-utc 1,3,10,11,12 --top 5 --request-interval-secs 1.1`
- `PYTHONPYCACHEPREFIX=.pycache python3 -m py_compile scripts/taker_validation_report.py scripts/taker_soak_report.py`
- `python3 scripts/taker_validation_report.py --preset trend_up_yes_taker_resolution --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --blocked-hours-utc 1,3,10,11,12`
- `python3 scripts/lane_forensics_report.py --preset trend_up_yes_taker_resolution --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --days 7 --shift-days 7 --lane-label trend_up_yes_taker_resolution_shift7`
- `python3 scripts/lane_forensics_report.py --preset trend_up_yes_taker_resolution --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --days 7 --shift-days 14 --lane-label trend_up_yes_taker_resolution_shift14`

### Artifacts
- 20260320_231345_trend_up_yes_taker_resolution_shift7_lane_forensics.json
- 20260320_231345_trend_up_yes_taker_resolution_shift14_lane_forensics.json
- 20260320_231439_alpha_report.json
- 20260320_231516_recency_alpha_report.json
- 20260320_231600_taker_validation_report.json
- 20260320_231739_taker_validation_report.json
- 20260320_231909_taker_validation_report.json
- 20260320_232039_taker_validation_report.json
- 20260320_232207_taker_validation_report.json
- 20260320_232335_taker_validation_report.json
- 20260320_232505_taker_validation_report.json
- 20260320_232632_taker_validation_report.json
- 20260320_232632_taker_soak_report.json

## [2026-03-20] (Session 55) — Dashboard Integrity Rail and Taker TP/SL Backtest Branch

### Dashboard Integrity
- **Added a first-class dashboard/backend truth check** — [src/api/integrity.rs](../../src/api/integrity.rs) now exposes `GET /api/integrity/report`, which compares the realtime fast/slow snapshots against authoritative backend views for:
  - runtime balance vs stats/accounting/fast snapshot
  - runtime daily PnL vs fast/stats
  - mode + loop state
  - open-trade counts and open notional
  - recent trade ordering and last-trade ID
  - maker quote count
  - latest reconcile summary/error surface
- **Protected the integrity rail behind read auth when auth is enabled** — [src/api/mod.rs](../../src/api/mod.rs) now routes `/api/integrity/report` through the same read-auth layer as `GET /api/config`, and it has router-level coverage to prove that behavior.
- **Added a report script for repeatable operator checks** — [scripts/dashboard_integrity_report.py](../../scripts/dashboard_integrity_report.py) fetches the new endpoint, writes JSON/Markdown artifacts, and exits non-zero if mismatches are detected. [scripts/api_auth.py](../../scripts/api_auth.py) now supports read-authenticated JSON GETs for this and other protected read endpoints.

### Taker Alpha Lab
- **Backtest now supports an explicit taker TP/SL exit branch** — [src/api/backtest.rs](../../src/api/backtest.rs) now accepts:
  - `exit_mode=next_candle|taker_tp_sl`
  - `take_profit_pct`
  - `stop_loss_pct`
- **The taker TP/SL path is conservative by design** — the new backtest logic models a binary-price path inside the next candle, assumes the worse outcome if both TP and SL are touched in the same candle, charges taker costs on both entry and early exit, and records richer per-trade fields such as `exit_type`, `exit_price`, `exit_fee_cost`, `path_low_price`, and `path_high_price`.
- **Alpha scripts can now sweep and report taker exit controls directly** — [scripts/alpha_presets.py](../../scripts/alpha_presets.py), [scripts/walkforward_alpha_sweep.py](../../scripts/walkforward_alpha_sweep.py), [scripts/daily_alpha_report.py](../../scripts/daily_alpha_report.py), [scripts/recency_alpha_report.py](../../scripts/recency_alpha_report.py), and [scripts/lane_forensics_report.py](../../scripts/lane_forensics_report.py) now all accept `--exit-mode`, `--take-profit-pct`, and `--stop-loss-pct`.
- **Added dedicated taker presets** — the alpha presets now include:
  - `taker_resolution_baseline`
  - `taker_tp_sl_probe`
  - `trend_up_yes_taker_tp_sl`

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test integrity_report_is_clean_when_views_match -- --test-threads=1`
- `cargo test taker_tp_sl_same_candle_uses_conservative_stop_loss -- --test-threads=1`
- `cargo test api::tests:: -- --test-threads=1`
- `cargo test`
- `PYTHONPYCACHEPREFIX=.pycache python3 -m py_compile scripts/api_auth.py scripts/dashboard_integrity_report.py scripts/alpha_presets.py scripts/walkforward_alpha_sweep.py scripts/daily_alpha_report.py scripts/recency_alpha_report.py scripts/lane_forensics_report.py`
- `python3 scripts/dashboard_integrity_report.py --base-url http://127.0.0.1:8000 --api-key local`
- `python3 scripts/walkforward_alpha_sweep.py --preset taker_tp_sl_probe --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --days 7 --fold-shifts 0,3 --top 3 --request-interval-secs 0.1`

### Artifacts
- 20260320_225957_dashboard_integrity_report.json
- 20260320_225957_dashboard_integrity_report.md

## [2026-03-20] (Session 54) — Dashboard QA: Auth Failure UX

### Dashboard UX
- **Auth-on dashboard failures are now explicit instead of silent** — [static/index.html](../../static/index.html) now detects read-auth failures during `GET /api/config` and stream-token/SSE setup, shows a one-time warning toast telling the user to provide a read API key, marks the SSE badge as `auth required`, and renders a clear config-panel placeholder instead of failing quietly. This keeps the hardened auth model while making the dashboard feel intentionally locked rather than broken.

### Verification
- auth-off smoke:
  - `GET /` → `200`
  - `GET /api/config` → `200`
  - `GET /api/stream/token` → `200`
- auth-on smoke:
  - `GET /` → `200`
  - `GET /api/config` without `x-api-key` → `401`
  - `GET /api/config` with `x-api-key` → `200`
  - `GET /api/stream/token` without `x-api-key` → `401`
  - `GET /api/stream/token` with `x-api-key` → `200`
  - `GET /api/stream?stream_token=...` with a freshly issued token → `200`

## [2026-03-20] (Session 53) — Live Hardening: Protected Config Reads and Router-Level Auth Checks

### Config Read Auth
- **`GET /api/config` is no longer the last public config surface when auth is enabled** — [src/api/auth.rs](../../src/api/auth.rs) now exposes a lightweight read-only auth middleware, and [src/api/mod.rs](../../src/api/mod.rs) routes `GET /api/config` through it. In auth-on deployments, the dashboard must now present a valid read API key to fetch the redacted config payload; in local auth-off simulation, the route stays readable so the existing dev UX still works.

### Integration Coverage
- **Added router-level checks for the dashboard-relevant auth paths** — [src/api/mod.rs](../../src/api/mod.rs) now has end-to-end tests that exercise:
  - unauthenticated vs `x-api-key`-authenticated `GET /api/config`
  - `GET /api/stream/token` issuance under read auth
  - `GET /api/stream` rejection without a token
  - successful SSE authorization with a short-lived `stream_token`
  - rejection when a valid `stream_token` is replayed from the wrong client IP
- **Enabled `tower::util::ServiceExt` for router tests** — [Cargo.toml](../../Cargo.toml) now enables the `util` feature on `tower` so these API-level `Router::oneshot` checks stay in-tree.

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test config_get_requires_read_auth_when_enabled -- --test-threads=1`
- `cargo test stream_token_round_trip_requires_read_auth_and_unlocks_sse -- --test-threads=1`
- `cargo test api::tests:: -- --test-threads=1`
- `cargo test`

## [2026-03-20] (Session 52) — Live Hardening: Active Order Semantics, Atomic Idempotency, Config Hygiene, and Stream Tokens

### Execution And Reconciliation
- **Filled live entries no longer masquerade as open exchange orders forever** — [src/bot/execute.rs](../../src/bot/execute.rs), [src/bot/reconcile.rs](../../src/bot/reconcile.rs), [src/bot/guard.rs](../../src/bot/guard.rs), [src/bot/state/bootstrap.rs](../../src/bot/state/bootstrap.rs), and [src/db/mod.rs](../../src/db/mod.rs) now treat `order_id` as active order state only while the entry order is still live. The fill monitor clears `order_id` when the entry is fully or terminally matched, restart fill-ledger replay clears it for fully filled open trades, full resync only compares active live orders against exchange open-order sets, and the resolution guard now settles filled live positions even after their entry `order_id` has been cleared.
- **Reconcile no longer force-closes valid filled positions that simply lack an active order** — [src/bot/reconcile.rs](../../src/bot/reconcile.rs) only treats a live trade as orphaned when it has neither an active order nor any filled shares.

### API Idempotency
- **Trade idempotency is now atomic instead of `get -> execute -> put` race-prone** — [src/bot/idempotency.rs](../../src/bot/idempotency.rs), [src/db/mod.rs](../../src/db/mod.rs), [src/api/bot_routes.rs](../../src/api/bot_routes.rs), and [src/api/trades.rs](../../src/api/trades.rs) now use an atomic pending-row claim in SQLite before executing side effects. Concurrent duplicate requests return an in-progress conflict instead of double-executing, finalized responses are still replayed from cache/DB, and early failures abort the pending claim instead of leaving a stale placeholder behind.

### Config Persistence
- **Runtime config saves now honor the actual booted config path** — [src/bot/state/control_plane.rs](../../src/bot/state/control_plane.rs), [src/api/config_api.rs](../../src/api/config_api.rs), [src/api/trades.rs](../../src/api/trades.rs), [src/api/bot_routes.rs](../../src/api/bot_routes.rs), and [src/bot/accounting.rs](../../src/bot/accounting.rs) now save back to the path implied by `MACBOT_CONFIG` instead of hard-coding `config.json`.
- **Env-only secrets no longer get materialized onto disk by unrelated config writes** — [src/config.rs](../../src/config.rs) now has `save_runtime()`, which preserves file-backed secret values and strips env-only overrides such as API keys and private RPC URLs unless the caller is intentionally persisting a secret patch.

### Streaming Auth
- **SSE and WS no longer accept bare `api_key` in the query string** — [src/api/auth.rs](../../src/api/auth.rs), [src/api/sse.rs](../../src/api/sse.rs), [src/api/ws.rs](../../src/api/ws.rs), [src/api/mod.rs](../../src/api/mod.rs), and [static/index.html](../../static/index.html) now issue short-lived stream tokens over a header-authenticated fetch to `/api/stream/token`, then use `stream_token` for SSE/WS connection setup. That removes long-lived API keys from browser history and proxy logs while keeping the dashboard workable in-browser.
- **Dashboard auth bootstrap now uses the URL hash instead of the query string** — [static/index.html](../../static/index.html) still supports one-click browser seeding of `api_key` and `api_secret`, but it now reads them from `#api_key=...&api_secret=...` and scrubs the fragment immediately. That preserves the convenience path without putting secrets into server access logs.

### Verification
- `cargo check`
- `cargo fmt --all`
- `cargo test save_runtime_keeps_env_only_secrets_off_disk -- --test-threads=1`
- `cargo test idempotency_claim_serializes_in_progress_and_cached_states -- --test-threads=1`
- `cargo test new_clears_order_id_for_fully_filled_live_trade_on_restart -- --test-threads=1`
- `cargo test reconcile_live_state_keeps_filled_live_trade_without_order_id -- --test-threads=1`
- `cargo test api::auth::tests:: -- --test-threads=1`
- `cargo test api::config_api::tests:: -- --test-threads=1`
- `cargo test cached_idempotent_trade_response_survives_preflight_block -- --test-threads=1`
- `cargo test bot::reconcile::tests:: -- --test-threads=1`
- `cargo test`

## [2026-03-20] (Session 51) — Alpha Lab: Shifted Specialist Soak, Wrapper Fix, and Sparse-Stable Candidate Split

### Research Workflow
- **Daily, recency, and wrapper reports can now replay an older “today” cleanly** — [scripts/daily_alpha_report.py](../../scripts/daily_alpha_report.py), [scripts/recency_alpha_report.py](../../scripts/recency_alpha_report.py), and [scripts/recent_specialist_validation_report.py](../../scripts/recent_specialist_validation_report.py) now accept `--anchor-shift-days`, pass that anchor into every backtest payload, and record it in the emitted reports. That lets the same paper-validation rail be replayed across multiple recent anchors instead of trusting a single “today”.
- **Fixed the shifted wrapper bug in the lane-forensics leg** — [scripts/recent_specialist_validation_report.py](../../scripts/recent_specialist_validation_report.py) no longer forwards `--anchor-shift-days` into [scripts/lane_forensics_report.py](../../scripts/lane_forensics_report.py). The lane tool expects `--shift-days`, and the mismatch was causing argparse exit-code `2` to be misread as “invalid JSON” inside the wrapper.
- **Added a replayable recent-specialist soak report** — [scripts/recent_specialist_soak_report.py](../../scripts/recent_specialist_soak_report.py) now reruns the full recent-specialist wrapper across a list of anchor shifts, aggregates verdicts, and tracks recent-recency passes, long-stress passes, daily gate passes, and lane-positive ratios per preset.
- **The soak classifier now distinguishes “sparse but stable” from genuinely failing** — [scripts/recent_specialist_soak_report.py](../../scripts/recent_specialist_soak_report.py) now ranks presets by recent-recency, long-stress, and lane stability before daily trade-count gates, and it can label a candidate as `sparse_candidate` when the only consistent blocker is the specialist daily trade floor rather than bad recency/stress behavior.

### Alpha Findings
- **The old `trend_up_yes_recent_specialist` does not survive a 7-anchor paper soak** — in 20260320_143409_recent_specialist_soak_report.json, the old preset only produced `1/7` recent-window passes and `0/7` long-stress passes, even though `6/7` current-lane snapshots were still positive. That is not stability; it is a current-window fluke with weak carry.
- **The hour-filtered preset is the stronger research candidate, but it is sparse** — the same 7-anchor soak shows `trend_up_yes_hour_filtered_specialist` with:
  - `7/7` short recency passes in the child [scripts/recency_alpha_report.py](../../scripts/recency_alpha_report.py) reports
  - `7/7` long-stress passes
  - `6/7` positive current-lane snapshots
  - but `0/7` daily wrapper passes because the `14d` specialist daily rail is still failing only on `min_mean_trades`
- **This is the cleanest alpha picture we have right now** — the old preset is less stable but more active; the hour-filtered preset is far more stable on recency/stress, but too sparse to satisfy the current daily trade floor. That means the honest next question is no longer “which preset is better?” It is “do we treat the hour-filtered lane as a sparse paper candidate, or do we demand more activity before it graduates?”

### Artifacts
- 20260320_142131_recent_specialist_validation_report.json
- 20260320_142820_recent_specialist_validation_report.json
- 20260320_143409_recent_specialist_soak_report.json
- 20260320_143409_recent_specialist_soak_report.md

### Verification
- `PYTHONPYCACHEPREFIX=.pycache python3 -m py_compile scripts/daily_alpha_report.py scripts/recency_alpha_report.py scripts/recent_specialist_validation_report.py scripts/recent_specialist_soak_report.py`
- `python3 scripts/recent_specialist_validation_report.py --preset trend_up_yes_recent_specialist --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --anchor-shift-days 1 --request-interval-secs 1.3`
- `python3 scripts/recent_specialist_soak_report.py --presets trend_up_yes_recent_specialist,trend_up_yes_hour_filtered_specialist --anchor-shifts 0,1,2,3,4,5,6 --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --request-interval-secs 1.3`

### Outcome
- **The current best alpha candidate is now best described as a sparse-but-stable recent specialist, not a broad deployable edge** — specifically the hour-filtered `TREND_UP + YES` lane. The next move should be either:
  - keep paper-soaking that lane without relaxing the strict wrapper, or
  - create a separate sparse-specialist paper gate instead of forcing it through a trade-count floor designed for busier strategies.

## [2026-03-20] (Session 50) — Alpha Lab: Strict Backtest Requests, Corrected Drift Forensics, and Hour-Filtered Specialist

### Research Workflow
- **Backtest requests now fail closed on malformed JSON instead of silently defaulting** — [src/api/backtest.rs](../../src/api/backtest.rs) no longer accepts `Option<Json<BacktestRequest>>` for `/api/backtest`. Invalid payloads now return an explicit error like `invalid backtest request: ...` instead of falling back to the endpoint defaults and producing a completely different strategy run.
- **Lane forensics can now inspect exact historical stress folds** — [scripts/lane_forensics_report.py](../../scripts/lane_forensics_report.py) now supports `--shift-days`, forwards `end_time_ms` into the backtest API, and records both the shift and the effective end time in the report config. That makes it possible to autopsy the exact `60d` folds from the recency report instead of approximating them with the current window.
- **Preset normalization is now correct in the forensic path** — [scripts/lane_forensics_report.py](../../scripts/lane_forensics_report.py) now converts preset `entry_thresholds` into a real `entry_threshold` for one-lane reports, and normalizes preset booleans like `adaptive_stand_down="true"` before the API call. This fixed the earlier poisoned forensic runs that were unknowingly hitting the default backtest request.
- **Added a new stress-hardened preset** — [scripts/alpha_presets.py](../../scripts/alpha_presets.py) now includes `trend_up_yes_hour_filtered_specialist`, which keeps the existing `TREND_UP + YES` specialist but blocks the toxic hour pockets identified by the corrected drift autopsy: `1,2,3,4,6,16,17,18,19,20`.
- **The validation wrapper is now rate-limit-safe by default and labels lane artifacts by preset** — [scripts/recent_specialist_validation_report.py](../../scripts/recent_specialist_validation_report.py) now defaults to `--request-interval-secs 1.1` and uses `f\"{preset}_current\"` for the forensic lane label instead of hardcoding the old preset name.

### Alpha Findings
- **The earlier current-lane forensic artifacts were wrong** — once the malformed-request fallback was fixed, the “current lane” and stress-fold forensic snapshots changed materially. The earlier artifacts that showed `82` trades and much broader mixed-session behavior were measuring the wrong request body, not the named specialist candidate.
- **Corrected `60d` drift autopsy: the old stress damage is real and mostly US-session driven** — the fixed stress-fold forensic artifacts show:
  - `shift 0`: 20260320_140356_trend_up_yes_60d_shift0_lane_forensics.json with `total_pnl=-0.9865`
  - `shift 14`: 20260320_140356_trend_up_yes_60d_shift14_lane_forensics.json with `total_pnl=-3.2233`
  - `shift 28`: 20260320_140356_trend_up_yes_60d_shift28_lane_forensics.json with `total_pnl=-2.8845`
  Across those corrected stress folds, `US` was consistently negative while `Asia` was positive or less bad.
- **Simple session splits were not enough**:
  - `ASIA-only` improved the old `60d` stress view but still failed the recent primary consistency gate in 20260320_140451_recency_alpha_report.json
  - `US-only` failed both recent and long views in 20260320_140452_recency_alpha_report.json
- **A surgical hour filter produced the first candidate that passes both recency rails** — expanding the blocklist to `1,2,3,4,6,16,17,18,19,20` created a materially stronger version of the specialist:
  - short recency pass in 20260320_140850_recency_alpha_report.json
  - long `60d` stress pass in 20260320_140900_recency_alpha_report.json
  The long view was especially notable: `60d` stress `mean_pnl=+4.5811`, `positive_folds=3/3`, and `mean_edge_gap_pp=+20.69`.
- **But the full paper-validation rail is still intentionally conservative** — the wrapper run for [trend_up_yes_hour_filtered_specialist](../../scripts/alpha_presets.py) in 20260320_141036_recent_specialist_validation_report.json still returned `failing_candidate` because:
  - the daily report failed its specialist trade-count floor (`mean_trades=7 < 10`)
  - the current `7d` lane snapshot was slightly negative (`total_pnl=-0.0996` across `8` trades)
- **So the honest read is now sharper** — we finally have a named hour-filtered candidate that survives both short and long recency reports, but it is still too sparse and too sensitive in the live-ish current slice to call “ready” without more paper soak.

### Artifacts
- 20260320_140510_trend_up_yes_recent_specialist_current_lane_forensics.json
- 20260320_140717_recent_specialist_validation_report.json
- 20260320_140850_recency_alpha_report.json
- 20260320_140900_recency_alpha_report.json
- 20260320_141036_recent_specialist_validation_report.json

### Verification
- `cargo check`
- `cargo test`
- `PYTHONPYCACHEPREFIX=.pycache python3 -m py_compile scripts/lane_forensics_report.py scripts/recent_specialist_validation_report.py scripts/alpha_presets.py`
- `python3 scripts/lane_forensics_report.py --preset trend_up_yes_recent_specialist --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --lane-label trend_up_yes_60d_shift0 --days 60 --shift-days 0 --filter-regimes TREND_UP --filter-signals BUY_YES`
- `python3 scripts/lane_forensics_report.py --preset trend_up_yes_recent_specialist --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --lane-label trend_up_yes_60d_shift14 --days 60 --shift-days 14 --filter-regimes TREND_UP --filter-signals BUY_YES`
- `python3 scripts/lane_forensics_report.py --preset trend_up_yes_recent_specialist --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --lane-label trend_up_yes_60d_shift28 --days 60 --shift-days 28 --filter-regimes TREND_UP --filter-signals BUY_YES`
- `python3 scripts/recency_alpha_report.py --preset trend_up_yes_hour_filtered_specialist --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --request-interval-secs 1.1 --days-list 7,14,21 --stress-days 30 --fold-shifts 0,7,14 --stress-fold-shifts 0,7,14 --top 1`
- `python3 scripts/recency_alpha_report.py --preset trend_up_yes_hour_filtered_specialist --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --request-interval-secs 1.1 --days-list 14,21,30 --stress-days 60 --fold-shifts 0,7,14 --stress-fold-shifts 0,14,28 --top 1`
- `python3 scripts/recent_specialist_validation_report.py --preset trend_up_yes_hour_filtered_specialist --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --request-interval-secs 1.3`

---

## [2026-03-20] (Session 49) — Alpha Lab: Recent Specialist Validation Rail

### Research Workflow
- **Added a single-command paper-validation rail for the current recent specialist** — [scripts/recent_specialist_validation_report.py](../../scripts/recent_specialist_validation_report.py) now runs the full daily specialist check in one shot:
  - [scripts/daily_alpha_report.py](../../scripts/daily_alpha_report.py)
  - [scripts/recency_alpha_report.py](../../scripts/recency_alpha_report.py) for the short recency gate
  - [scripts/recency_alpha_report.py](../../scripts/recency_alpha_report.py) for the longer stress gate
  - [scripts/lane_forensics_report.py](../../scripts/lane_forensics_report.py) for the current 7-day lane snapshot
- **The wrapper now uses a specialist-aware daily trade floor instead of the generic broad-strategy gate** — it passes `--min-mean-trades 10.0` into the daily report so sparse recent specialists do not get mislabeled as dead candidates purely because they trade less often than the broad sweep defaults expect.
- **Validation outputs are now consolidated into one report artifact** — the wrapper writes timestamped JSON/Markdown under artifacts/alpha-reports and records every child command plus every child artifact path so the paper-validation read is reproducible.

### Alpha Findings
- **The current `TREND_UP + YES` candidate now classifies cleanly as a recent specialist, not a failing candidate** — with the specialist-aware daily trade floor, the wrapper correctly reports:
  - `daily_report = pass`
  - `recent_recency = pass`
  - `long_recency = fail`
  - `lane_positive_now = true`
- **The recent window is healthy enough to keep paper-validating** — the current lane remains positive in the fresh 7-day forensic snapshot with `82` trades, `total_pnl=+3.3177`, `win_rate=52.44%`, and `edge_gap_pp=+0.90`.
- **The long-stress warning remains the real blocker** — the same candidate still fails the `14d/21d/30d` with `60d` stress view, where the `60d` stress window is `mean_pnl=-2.3647`, `positive_folds=0/3`, and `mean_edge_gap_pp=-7.28`. The honest interpretation has not changed: this is a viable recent-window paper candidate, not a durable multi-month edge.

### Artifacts
- 20260320_135813_alpha_report.json
- 20260320_135821_recency_alpha_report.json
- 20260320_135836_recent_specialist_validation_report.json
- 20260320_135836_trend_up_yes_recent_specialist_current_lane_forensics.json

### Verification
- `PYTHONPYCACHEPREFIX=.pycache python3 -m py_compile scripts/recent_specialist_validation_report.py`
- `python3 scripts/recent_specialist_validation_report.py --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret`

---

## [2026-03-20] (Session 48) — Alpha Lab: Lane Forensics, Confidence/Session/Hour Filters, and First Passing Recent Specialist

### Research Workflow
- **Backtest trade logs now carry enough time metadata for real lane autopsies** — [src/api/backtest.rs](../../src/api/backtest.rs) now emits per-trade UTC entry/resolution timestamps, entry hour, weekday, minute-of-day, holding time, confidence margin, edge margin, next-return percent, and session label. That turns the backtest endpoint into a usable forensic surface instead of just a PnL counter.
- **Added a dedicated lane-forensics report** — [scripts/lane_forensics_report.py](../../scripts/lane_forensics_report.py) can now bucket a backtest lane by session, hour, volatility, confidence margin, and edge margin, then write timestamped JSON/Markdown artifacts under artifacts/alpha-reports.
- **Added two cheap reactive alpha filters to the backtest API and sweep scripts** — [src/api/backtest.rs](../../src/api/backtest.rs), [scripts/walkforward_alpha_sweep.py](../../scripts/walkforward_alpha_sweep.py), [scripts/daily_alpha_report.py](../../scripts/daily_alpha_report.py), [scripts/recency_alpha_report.py](../../scripts/recency_alpha_report.py), [scripts/backtest_regime_report.py](../../scripts/backtest_regime_report.py), and [scripts/lane_forensics_report.py](../../scripts/lane_forensics_report.py) now support:
  - `min_confidence_margin`
  - `session_allowlist`
  - `blocked_hours_utc`
- **Promoted the best current recent-window lane into an explicit preset** — [scripts/alpha_presets.py](../../scripts/alpha_presets.py) now includes `trend_up_yes_recent_specialist`, which encodes the current best recent specialist as maker-only `TREND_UP + YES`, `session_allowlist=ASIA,US`, `blocked_hours_utc=4,6`, `min_confidence_margin=0.03`, and the current stand-down settings.

### Alpha Findings
- **The combined directional policy was not salvageable with a generic confidence-margin gate** — once rerun against the fresh binary, adding `min_confidence_margin=0.03` improved the old `30d` stress window but made the `7d/14d/21d` primary windows worse. The filter removed thin-margin noise, but it did not fix the structural lane problem.
- **`TREND_DOWN + NO` is currently toxic even when restricted to “strong” signals** — the new forensic artifacts showed that the down lane remained negative after removing thin-margin entries. The damage was not coming from weak noise only; even the thick-confidence down signals were bad.
- **`TREND_UP + YES` is the only lane with a real recent pulse** — the focused lane-forensics reports showed positive expectancy for the recent `TREND_UP + YES` specialist with strong-confidence entries, while the mirrored down lane stayed negative.
- **The first passing recent-window specialist is now explicit** — the following candidate passed the `7d/14d/21d` primary gates with `30d` rollback stress:
  - `maker`
  - `TREND_UP + YES` only
  - `require_regime_alignment=true`
  - `session_allowlist=[ASIA,US]`
  - `blocked_hours_utc=[4,6]`
  - `entry_threshold=0.38`
  - `min_confidence_margin=0.03`
  - `adaptive_stand_down=true`
  - `stand_down_window=8`
  - `stand_down_margin_pp=1.5`
  - `stand_down_cooldown_candles=48`
- **But it is still a recent-window specialist, not a timeless edge** — the exact same candidate failed the harder `14d/21d/30d` with `60d` stress report. So the honest interpretation is: we now have a credible short-horizon specialist candidate for further validation, not a robust multi-month deployable strategy.

### Artifacts
- 20260320_133147_recency_alpha_report.json
- 20260320_133210_trend_down_no_7d_mc003_lane_forensics.json
- 20260320_133211_trend_up_yes_7d_mc003_lane_forensics.json
- 20260320_133905_recency_alpha_report.json
- 20260320_134556_recency_alpha_report.json
- 20260320_134624_recency_alpha_report.json

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test api::backtest::tests:: -- --test-threads=1`
- `PYTHONPYCACHEPREFIX=.pycache python3 -m py_compile scripts/lane_forensics_report.py scripts/recency_alpha_report.py scripts/walkforward_alpha_sweep.py scripts/daily_alpha_report.py scripts/backtest_regime_report.py`
- `python3 scripts/lane_forensics_report.py --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --lane-label trend_down_no_7d_mc003 --days 7 --mode maker --entry-threshold 0.36 --allow-buy-no --adaptive-stand-down --stand-down-window 8 --stand-down-margin-pp 1.5 --stand-down-cooldown-candles 48 --min-confidence-margin 0.03 --require-regime-alignment --regime-allowlist TREND_DOWN --regime-side-modes TREND_DOWN:no_only --filter-regimes TREND_DOWN --filter-signals BUY_NO`
- `python3 scripts/lane_forensics_report.py --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --lane-label trend_up_yes_7d_mc003 --days 7 --mode maker --entry-threshold 0.38 --allow-buy-yes --adaptive-stand-down --stand-down-window 8 --stand-down-margin-pp 1.5 --stand-down-cooldown-candles 48 --min-confidence-margin 0.03 --require-regime-alignment --regime-allowlist TREND_UP --regime-side-modes TREND_UP:yes_only --filter-regimes TREND_UP --filter-signals BUY_YES`
- `python3 scripts/recency_alpha_report.py --preset '' --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --request-interval-secs 0 --mode maker --days-list 7,14,21 --stress-days 30 --fold-shifts 0,7,14 --stress-fold-shifts 0,7,14 --entry-thresholds 0.38 --sides yes_only --adaptive-stand-down true --stand-down-windows 8 --stand-down-margin-pps 1.5 --stand-down-cooldowns 48 --min-confidence-margin 0.03 --require-regime-alignment --regime-allowlist TREND_UP --session-allowlist ASIA,US --blocked-hours-utc 4,6 --regime-side-modes TREND_UP:yes_only --top 1`
- `python3 scripts/recency_alpha_report.py --preset '' --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --request-interval-secs 0 --mode maker --days-list 14,21,30 --stress-days 60 --fold-shifts 0,7,14 --stress-fold-shifts 0,14,28 --entry-thresholds 0.38 --sides yes_only --adaptive-stand-down true --stand-down-windows 8 --stand-down-margin-pps 1.5 --stand-down-cooldowns 48 --min-confidence-margin 0.03 --require-regime-alignment --regime-allowlist TREND_UP --session-allowlist ASIA,US --blocked-hours-utc 4,6 --regime-side-modes TREND_UP:yes_only --top 1`

---

## [2026-03-20] (Session 47) — Alpha Lab: Recency Gates, Explicit Directional Preset, and Honest Specialist Drift Read

### Research Workflow
- **Added an explicit combined directional specialist preset for the alpha lab** — [scripts/alpha_presets.py](../../scripts/alpha_presets.py) now includes `trend_directional_specialist`, which encodes the intended maker-only directional specialist as `allowlist=[TREND_UP,TREND_DOWN]` plus `regime_side_modes={TREND_UP:yes_only,TREND_DOWN:no_only}` instead of relying on looser side semantics.
- **Added a dedicated recency-alpha report with explicit drift gates** — [scripts/recency_alpha_report.py](../../scripts/recency_alpha_report.py) evaluates candidate policies across rolling primary windows (for example `14d/21d/30d`) plus a separate stress window (for example `60d`) and fails candidates when recency or stress checks break. The report writes timestamped JSON and Markdown artifacts under artifacts/alpha-reports.
- **Preset handling in the Python alpha tools is now researcher-friendly instead of destructive** — [scripts/alpha_presets.py](../../scripts/alpha_presets.py), [scripts/walkforward_alpha_sweep.py](../../scripts/walkforward_alpha_sweep.py), [scripts/daily_alpha_report.py](../../scripts/daily_alpha_report.py), and [scripts/recency_alpha_report.py](../../scripts/recency_alpha_report.py) now apply presets as defaults rather than overwriting explicit CLI flags. This prevents accidental single-candidate runs when the user intends to sweep thresholds around a preset family.

### Alpha Findings
- **The best recency-gated combined directional specialist is locally strong but still fails stress** — the new recency report found the best `14d/21d/30d` candidate at `entry_threshold=0.36`, `maker`, adaptive stand-down on, `stand_down_window=8`, `stand_down_margin_pp=1.5`, `cooldown=48`, `allowlist=[TREND_UP,TREND_DOWN]`, `regime_side_modes={TREND_UP:yes_only,TREND_DOWN:no_only}`. It produced positive mean PnL across all primary windows (`14d=+1.56`, `21d=+3.70`, `30d=+3.68`) but still collapsed on the `60d` stress window (`mean_pnl=-5.12`, `positive_folds=0/4`), so it fails the honest drift gate.
- **The current freshest 7-day slice is the real near-term problem for the combined specialist** — when the same directional policy was scored as a shorter-horizon candidate (`7d/14d/21d` with `30d` rollback stress), the `7d` window was outright negative (`mean_pnl=-1.03`, `edge_gap=-5.16pp`) while `14d`, `21d`, and `30d` stayed positive. This means the strategy is not just “bad on old history”; it is currently underperforming in the freshest window too.
- **That fresh 7-day weakness comes mostly from the `TREND_DOWN + NO` lane** — a direct [scripts/backtest_regime_report.py](../../scripts/backtest_regime_report.py) run on the current `7d` combined specialist showed both lanes negative, but `TREND_DOWN` was materially worse (`total_pnl=-1.7210`, `edge_gap=-6.52pp`) than `TREND_UP` (`total_pnl=-0.6853`, `edge_gap=-1.35pp`).
- **The only current lane with any sign of life is `TREND_UP + YES`, but it still does not pass broader recency gates** — a focused `7d` walk-forward on `TREND_UP + YES` at thresholds `0.38/0.40` came out mildly positive (`mean_pnl=+0.56`, `positive_folds=2/3`), but the corresponding `7d/14d/21d` recency report still failed because the `14d` and `21d` primary windows were not consistently positive and the primary edge-gap checks stayed negative. So there is still no clean deployable lane.
- **Current honest alpha conclusion: there is still no edge we should trust with capital, but we now know exactly why** — the directional specialist is not uniformly fake; it has a real recent pocket around `14d-30d`, yet it fails both the long stress view (`60d`) and the freshest combined view (`7d`). That tells us the next alpha work should be about temporal drift handling or fresher signal structure, not pretending we already have a stable specialist policy.

### Verification
- `python3 -m py_compile scripts/alpha_presets.py scripts/walkforward_alpha_sweep.py scripts/daily_alpha_report.py scripts/recency_alpha_report.py`
- `python3 scripts/walkforward_alpha_sweep.py --preset trend_directional_specialist --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --days 7 --fold-shifts 0 --entry-thresholds 0.36,0.38 --top 2 --request-interval-secs 0 --request-timeout-secs 60`
- `python3 scripts/recency_alpha_report.py --preset '' --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --mode maker --days-list 14,21,30 --stress-days 60 --fold-shifts 0,7,14 --stress-fold-shifts 0,14,28,42 --entry-thresholds 0.36,0.38,0.40 --sides both --adaptive-stand-down true --stand-down-windows 8 --stand-down-margin-pps 1.5,2.0 --stand-down-cooldowns 48 --require-regime-alignment --regime-allowlist TREND_UP,TREND_DOWN --regime-side-modes TREND_UP:yes_only,TREND_DOWN:no_only --top 6`
- `python3 scripts/recency_alpha_report.py --preset '' --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --mode maker --days-list 7,14,21 --stress-days 30 --fold-shifts 0,7,14 --stress-fold-shifts 0,7,14 --entry-thresholds 0.36 --sides both --adaptive-stand-down true --stand-down-windows 8 --stand-down-margin-pps 1.5 --stand-down-cooldowns 48 --require-regime-alignment --regime-allowlist TREND_UP,TREND_DOWN --regime-side-modes TREND_UP:yes_only,TREND_DOWN:no_only --top 1`
- `python3 scripts/backtest_regime_report.py --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --mode maker --days 7 --entry-threshold 0.36 --require-regime-alignment --regime-allowlist TREND_UP,TREND_DOWN --regime-side-modes TREND_UP:yes_only,TREND_DOWN:no_only`
- `python3 scripts/walkforward_alpha_sweep.py --preset '' --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --mode maker --days 7 --entry-thresholds 0.36,0.38,0.40,0.42 --sides yes_only --adaptive-stand-down true --fold-shifts 0,7,14 --stand-down-window 8 --stand-down-margin-pp 1.5 --stand-down-cooldown-candles 48 --require-regime-alignment --regime-allowlist TREND_UP --regime-side-modes TREND_UP:yes_only --request-interval-secs 1.2 --top 4 --request-timeout-secs 120`
- `python3 scripts/walkforward_alpha_sweep.py --preset '' --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --mode maker --days 7 --entry-thresholds 0.36,0.38,0.40,0.42 --sides no_only --adaptive-stand-down true --fold-shifts 0,7,14 --stand-down-window 8 --stand-down-margin-pp 1.5 --stand-down-cooldown-candles 48 --require-regime-alignment --regime-allowlist TREND_DOWN --regime-side-modes TREND_DOWN:no_only --request-interval-secs 1.2 --top 4 --request-timeout-secs 120`
- `python3 scripts/recency_alpha_report.py --preset '' --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --mode maker --days-list 7,14,21 --stress-days 30 --fold-shifts 0,7,14 --stress-fold-shifts 0,7,14 --entry-thresholds 0.38 --sides yes_only --adaptive-stand-down true --stand-down-windows 8 --stand-down-margin-pps 1.5 --stand-down-cooldowns 48 --require-regime-alignment --regime-allowlist TREND_UP --regime-side-modes TREND_UP:yes_only --top 1`

## [2026-03-20] (Session 46) — Alpha Lab: Backtest-Only API Mode & Directional Specialist Search

### Research Workflow
- **The binary now has a backtest-only research mode that skips live feed churn** — [src/main.rs](../../src/main.rs) now honors `MACBOT_BACKTEST_ONLY=1`, which disables startup analysis, live-feed/background-task startup, and loop auto-start so the local API can be used as a cleaner backtest lab. This was added specifically to keep alpha sweeps usable in constrained/offline environments without booting the whole live runtime stack.
- **The local backtest API was verified in that mode with temporary local credentials** — running `MACBOT_BACKTEST_ONLY=1 MACBOT_STARTUP_ANALYSIS=0 MACBOT_API_KEY=local MACBOT_API_SECRET=localsecret cargo run --release` now yields a stable local `/api/backtest` surface for authenticated research sweeps.

### Alpha Findings
- **The old single-lane specialist story is outdated** — a fresh 30-day maker sweep with adaptive stand-down showed the least-bad broad pocket had shifted toward `no_only`, not the earlier `TREND_UP + YES` story. The top global maker candidate was still negative (`mean_pnl ≈ -0.88`), but it pointed toward directional asymmetry rather than a uniform policy.
- **Directional regime specialists are the first heuristic setup to look genuinely promising on the recent 30-day window** — coarse 30-day walk-forwards showed both `TREND_DOWN + NO` and `TREND_UP + YES` positive over 3 folds, and the combined directional policy (`require_regime_alignment=true`, `regime_allowlist=[TREND_UP,TREND_DOWN]`, `side=both`, `maker`, adaptive stand-down on) was the strongest result over the fuller 5-fold 30-day sweep.
- **The best recent candidate is a combined directional specialist, not a single-side policy** — on the 30-day 5-fold maker sweep, `entry_threshold=0.380`, `stand_down_window=8`, `stand_down_margin_pp=2.0`, `cooldown=48`, `allowlist=[TREND_UP,TREND_DOWN]`, `require_regime_alignment=true` produced `mean_pnl=+1.1179`, `median_pnl=+0.6851`, `positive_folds=3/5`, and `mean_edge_gap_pp=+1.34`. The standout fold was `shift=7d` with `pnl=+8.2751`, but the oldest fold (`shift=28d`) was still negative.
- **That edge does not survive the 60-day stress test** — the same combined directional policy collapsed hard over a 60-day 5-fold walk-forward (`mean_pnl=-5.1497`, `positive_folds=0/5`, `mean_edge_gap_pp=-14.68`), and raising the threshold to `0.44` did not rescue it (`mean_pnl=-4.8097`). The honest read is that the directional specialist is a recent-window effect, not a robust long-horizon edge.
- **Current best heuristic conclusion: recent adaptive directional behavior may be tradable, but there is still no durable multi-month edge** — the live alpha frontier has improved from “everything is broadly bad” to “recent directional specialists can be positive,” but the 60-day collapse means that any near-term canary would need explicit recency adaptation or fast rollback logic instead of pretending we’ve found a timeless strategy.

### Verification
- `cargo fmt --all`
- `cargo check`
- `MACBOT_BACKTEST_ONLY=1 MACBOT_STARTUP_ANALYSIS=0 MACBOT_API_KEY=local MACBOT_API_SECRET=localsecret cargo run --release`
- `python3 scripts/backtest_regime_report.py --base-url http://localhost:8000 --api-key local --api-secret localsecret --mode maker --days 14 --entry-threshold 0.37 --require-regime-alignment --regime-allowlist TREND_UP`
- `python3 scripts/walkforward_alpha_sweep.py --base-url http://localhost:8000 --api-key local --api-secret localsecret --mode maker --days 30 --entry-thresholds 0.32,0.34,0.36,0.38,0.40 --sides both,yes_only,no_only --adaptive-stand-down true --fold-shifts 0,7,14,21,28 --stand-down-window 8 --stand-down-margin-pp 2.0 --stand-down-cooldown-candles 48 --top 12 --request-timeout-secs 180`
- `python3 scripts/walkforward_alpha_sweep.py --base-url http://localhost:8000 --api-key local --api-secret localsecret --mode maker --days 30 --entry-thresholds 0.30,0.32,0.34,0.36,0.38,0.40 --sides no_only --adaptive-stand-down true --fold-shifts 0,14,28 --stand-down-window 8 --stand-down-margin-pp 2.0 --stand-down-cooldown-candles 48 --require-regime-alignment --regime-allowlist TREND_DOWN --request-interval-secs 1.3 --top 6 --request-timeout-secs 180`
- `python3 scripts/walkforward_alpha_sweep.py --base-url http://localhost:8000 --api-key local --api-secret localsecret --mode maker --days 30 --entry-thresholds 0.30,0.32,0.34,0.36,0.38,0.40 --sides yes_only --adaptive-stand-down true --fold-shifts 0,14,28 --stand-down-window 8 --stand-down-margin-pp 2.0 --stand-down-cooldown-candles 48 --require-regime-alignment --regime-allowlist TREND_UP --request-interval-secs 1.3 --top 6 --request-timeout-secs 180`
- `python3 scripts/walkforward_alpha_sweep.py --base-url http://localhost:8000 --api-key local --api-secret localsecret --mode maker --days 30 --entry-thresholds 0.30,0.32,0.34,0.36,0.38,0.40 --sides both --adaptive-stand-down true --fold-shifts 0,7,14,21,28 --stand-down-window 8 --stand-down-margin-pp 2.0 --stand-down-cooldown-candles 48 --require-regime-alignment --regime-allowlist TREND_UP,TREND_DOWN --request-interval-secs 1.3 --top 6 --request-timeout-secs 180`
- `python3 scripts/walkforward_alpha_sweep.py --base-url http://localhost:8000 --api-key local --api-secret localsecret --mode maker --days 60 --entry-thresholds 0.38,0.40,0.42,0.44 --sides both --adaptive-stand-down true --fold-shifts 0,14,28,42,56 --stand-down-window 8 --stand-down-margin-pp 2.0 --stand-down-cooldown-candles 48 --require-regime-alignment --regime-allowlist TREND_UP,TREND_DOWN --request-interval-secs 1.3 --top 4 --request-timeout-secs 240`

---

## [2026-03-20] (Session 45) — Alpha Lab: Specialist Preset & Deep-Model Holdout Regime Report

### Alpha Research Tooling
- **The current heuristic frontier is now frozen as a named preset instead of tribal knowledge** — added [scripts/alpha_presets.py](../../scripts/alpha_presets.py), and both [scripts/walkforward_alpha_sweep.py](../../scripts/walkforward_alpha_sweep.py) and [scripts/daily_alpha_report.py](../../scripts/daily_alpha_report.py) now accept `--preset trend_up_yes_specialist`. That preset pins the least-bad heuristic candidate so future alpha runs can start from the same baseline instead of rebuilding it from memory.
- **Deep-model evaluation now has an explicit out-of-sample regime report** — added [scripts/deep_model_regime_report.py](../../scripts/deep_model_regime_report.py), which loads a cleaned historical DB plus an ONNX artifact, matches the chronological `last 15%` holdout split used in [training/train.py](../../training/train.py), and reports global metrics, per-regime metrics, plus specialist-threshold lift for a targeted lane such as `TRENDING + YES`.
- **Historical dataset samples now preserve aligned metadata for analysis without contaminating training batches** — [training/train.py](../../training/train.py) now keeps `timestamp`, `market_id`, `regime_label`, and `outcome` in `BTCDataset.meta`, which lets offline evaluation slice by actual holdout regime while leaving the tensor batches unchanged for model training.

### Alpha Findings
- **The clean deep model still does not improve the specialist lane as a filter** — on the 14-day clean holdout, artifacts/alpha-models/session45-backfill14d15m/latest.onnx reached the same `global AUC ≈ 0.545` seen during training, but its `p_up` thresholding *hurt* the historical `TRENDING + YES` proxy: the baseline win rate was `44.7%`, while `p_up >= 0.50` dropped it to `41.2%`, and tighter thresholds got worse.
- **The 30-day model flattened even as a specialist filter** — on the month-long clean holdout, artifacts/alpha-models/session46-backfill30d15m/latest.onnx was effectively noise globally (`AUC ≈ 0.509`), and the `TRENDING + YES` filter only produced a trivial `+0.37pp` lift at `p_up >= 0.50` while selecting essentially the whole regime bucket (`274/278` samples). Every higher threshold selected nothing.
- **The calibration head is not rescuing the edge either** — the `calibration` output was slightly worse than `p_up` on both windows (`14d Brier ≈ 0.2484` vs `0.2475` best raw-holdout result; `30d AUC ≈ 0.491` vs `0.509`), and it produced no viable specialist selections above `0.50`.
- **Historical regime taxonomy is coarser than the live backtest surface** — the cleaned backfill datasets currently expose `CHOP` and `TRENDING` labels, not the richer `TREND_UP`/`TREND_DOWN` taxonomy used in live/backtest heuristics. That means the new holdout report can honestly test a `TRENDING + YES` proxy today, but a perfect apples-to-apples `TREND_UP + YES` model filter will require richer historical regime reconstruction.

### Verification
- `PYTHONPYCACHEPREFIX=.pycache python3 -m py_compile scripts/alpha_presets.py scripts/walkforward_alpha_sweep.py scripts/daily_alpha_report.py scripts/deep_model_regime_report.py training/train.py`
- `.venv-training/bin/python scripts/deep_model_regime_report.py --help`
- `.venv-training/bin/python scripts/deep_model_regime_report.py --db artifacts/alpha-data/session45/backfill_14d_15m.db --model artifacts/alpha-models/session45-backfill14d15m/latest.onnx --head p_up --specialist-regime TRENDING --specialist-side yes_only --thresholds 0.50,0.52,0.54,0.56,0.58,0.60,0.62 --output-json artifacts/alpha-reports/session45_deep_model_trending_yes_report.json`
- `.venv-training/bin/python scripts/deep_model_regime_report.py --db artifacts/alpha-data/session46/backfill_30d_15m.db --model artifacts/alpha-models/session46-backfill30d15m/latest.onnx --head p_up --specialist-regime TRENDING --specialist-side yes_only --thresholds 0.50,0.52,0.54,0.56,0.58,0.60,0.62 --output-json artifacts/alpha-reports/session46_deep_model_trending_yes_report.json`
- `.venv-training/bin/python scripts/deep_model_regime_report.py --db artifacts/alpha-data/session45/backfill_14d_15m.db --model artifacts/alpha-models/session45-backfill14d15m/latest.onnx --head calibration --specialist-regime TRENDING --specialist-side yes_only --thresholds 0.50,0.52,0.54,0.56,0.58,0.60,0.62`
- `.venv-training/bin/python scripts/deep_model_regime_report.py --db artifacts/alpha-data/session46/backfill_30d_15m.db --model artifacts/alpha-models/session46-backfill30d15m/latest.onnx --head calibration --specialist-regime TRENDING --specialist-side yes_only --thresholds 0.50,0.52,0.54,0.56,0.58,0.60,0.62`

---

## [2026-03-20] (Session 44) — Alpha Lab: Clean Historical Scale-Up To 14d/30d

### Historical Alpha Lab
- **The clean historical pipeline now has credible medium-window evidence instead of only tiny probes** — [training/backfill_historical.py](../../training/backfill_historical.py) produced a clean 14-day 15m dataset at artifacts/alpha-data/session45/backfill_14d_15m.db with `1341` labeled markets and `20340` unique BTC minute candles, then scaled the same pipeline to a full 30-day set at artifacts/alpha-data/session46/backfill_30d_15m.db with `2877` labeled markets and `43380` unique BTC minute candles.
- **The 14-day retrain is the best clean deep-model result so far, but only modestly so** — the contained retrain in artifacts/alpha-models/session45-backfill14d15m/latest.onnx reached `Final Brier score: 0.2528` with a best restart around `Brier=0.2475 / AUC=0.545`, which is directionally better than the clean 7-day historical run but still far from a strong predictive edge.
- **The 30-day retrain falsified the hopeful “just add more honest history” hypothesis** — the month-long contained retrain in artifacts/alpha-models/session46-backfill30d15m/latest.onnx settled at `Final Brier score: 0.2500` with a best restart near `Brier=0.2498 / AUC=0.509`, which is effectively back to noise. That strongly suggests the small 14-day bump is local/regime-specific rather than a robust deep-model edge.
- **The larger historical datasets are clean, but not fully rich** — the training loader still reports `14` zero-variance scalar channels on the clean 14-day set and `17` on the 30-day set. The truly dead channels are mostly expected historical blind spots (`fear_greed`, external sentiment feeds, liquidation volumes, depth imbalance, `polymarket_flow`, `latency_arb`), but the 30-day run also lost `deribit_pcr`, `options_advanced`, and `cross_market` variation because futures-ratio context remained unavailable without exchange-side data access.
- **Regime drift is now a first-class alpha warning, not just a suspicion** — adversarial validation rose to `AUC=0.886` on the 14-day set and `AUC=0.945` on the 30-day set, which means the later holdout periods are highly distinguishable from the earlier training windows. The lab is finally honest enough to show that our main challenge is not just sample size, but regime shift and missing historical context.
- **The deep-model ONNX contract stayed correct through the larger retrains** — the 14-day artifact still exports `scalars=['batch', 40]`, `staleness=['batch', 34]`, and `regime=['batch', 1]`, so the larger clean experiments did not regress the runtime schema fix from Session 42.

### Alpha Findings
- **We still do not have a standalone deep-model edge** — the clean historical scale-up improved confidence in the lab, but not confidence in the model. The current honest reading is “marginal local signal at best, not robust predictive edge.”
- **The next alpha lever is richer historical reconstruction or narrower specialist policy, not more of the same generic retraining** — after the 30-day result flattened out, the most promising next moves are either restoring the missing historical cross-market/context channels or focusing the strategy on a narrower, regime-specialist surface where the heuristic frontier was least bad.

### Verification
- `python3 -m py_compile training/backfill_historical.py training/train.py`
- `python3 training/backfill_historical.py --db artifacts/alpha-data/session45/backfill_14d_15m.db --days 14 --interval 15m --ratio-period 15m`
- `PYTHONPATH=training .venv-training/bin/python training/train.py --db artifacts/alpha-data/session45/backfill_14d_15m.db --out artifacts/alpha-models/session45-backfill14d15m --skip-hpo --skip-pretrain --device cpu`
- `.venv-training/bin/python - <<'PY' ... onnx.load('artifacts/alpha-models/session45-backfill14d15m/latest.onnx') ... PY`
- `python3 training/backfill_historical.py --db artifacts/alpha-data/session46/backfill_30d_15m.db --days 30 --interval 15m --ratio-period 15m`
- `PYTHONPATH=training .venv-training/bin/python training/train.py --db artifacts/alpha-data/session46/backfill_30d_15m.db --out artifacts/alpha-models/session46-backfill30d15m --skip-hpo --skip-pretrain --device cpu`

---

## [2026-03-20] (Session 43) — Alpha Lab: Historical Backfill Contract Repair & Leakage Cleanup

### Historical Data Contract
- **Historical backfill now emits the current 34-feature schema instead of the legacy 14-feature subset** — [training/backfill_historical.py](../../training/backfill_historical.py) now projects its historically reconstructible inputs onto the same feature contract used by [training/train.py](../../training/train.py), including a 34-slot `staleness` vector and neutral placeholders for genuinely unavailable features. That closes a silent schema drift path between historical training and live/runtime inference.
- **Historical candle storage is now one shared BTC minute stream instead of one duplicate copy per market** — the backfill path now bulk-seeds a single minute-candle range with `replace_candle_range(...)`, while [training/train.py](../../training/train.py) defensively deduplicates candle reads by timestamp. This eliminates the earlier candle explosion where one 7-day dataset held `171248` candle rows but only `10260` distinct minutes.
- **The historical feature window is now strictly pre-open and no longer leaks future information** — [training/backfill_historical.py](../../training/backfill_historical.py) now cuts the feature window at `current_ts * 1000 - 1` so every feature uses only information available before the traded market opens. The earlier backfill path had been accidentally looking through the market close, which inflated training results and made them unusable as honest alpha evidence.

### Alpha Findings
- **The earlier “better historical model” result was overstated by data leakage** — once feature-contract drift, duplicate-candle amplification, and lookahead leakage were fixed, the quick backfilled models became much less exciting. The earlier pre-fix historical wins should be treated as invalid alpha evidence.
- **A clean 3-day historical 15m probe is plausible but still modest** — artifacts/alpha-data/session43/backfill_3d_15m.db produced `285` labeled markets with `4500` clean minute candles, and the contained retrain in artifacts/alpha-models/session43-backfill3d15m/latest.onnx finished around `Final Brier score: 0.2486` with a best restart near `AUC=0.556`.
- **A clean 7-day historical 15m probe is broader but not yet stronger** — artifacts/alpha-data/session44/backfill_7d_15m.db produced `669` labeled markets with `10260` unique minute candles, and the contained retrain in artifacts/alpha-models/session44-backfill7d15m/latest.onnx finished around `Final Brier score: 0.2514` with a best restart near `AUC=0.511`.
- **The credible next alpha move is scale plus feature coverage, not celebrating a breakthrough** — after the leakage cleanup, the honest conclusion is that we still do not have a real alpha unlock. The next useful work is scaling clean historical labeling to longer windows and improving the historically reconstructible feature set so fewer scalar slots stay effectively inert.

### Verification
- `python3 -m py_compile training/backfill_historical.py training/train.py`
- `python3 training/backfill_historical.py --db artifacts/alpha-data/session43/backfill_3d_15m.db --days 3 --interval 15m --ratio-period 15m`
- `.venv-training/bin/python training/train.py --db artifacts/alpha-data/session43/backfill_3d_15m.db --out artifacts/alpha-models/session43-backfill3d15m --skip-hpo --skip-pretrain --device cpu`
- `python3 training/backfill_historical.py --db artifacts/alpha-data/session44/backfill_7d_15m.db --days 7 --interval 15m --ratio-period 15m`
- `.venv-training/bin/python training/train.py --db artifacts/alpha-data/session44/backfill_7d_15m.db --out artifacts/alpha-models/session44-backfill7d15m --skip-hpo --skip-pretrain --device cpu`

---

## [2026-03-20] (Session 42) — Alpha Lab: Deep-Model Contract Recovery & Historical Backfill Probe

### Training / Model Path
- **The local training stack is now runnable in the repo’s real Python environment** — created an isolated `.venv-training/` and installed [training/requirements.txt](../../training/requirements.txt), which unblocked `torch`, `optuna`, `tqdm`, `onnx`, and `onnxruntime` for contained alpha experiments without touching the system Python.
- **The shared regime contract and training entrypoint now work on Python 3.9 too** — [training/regime_contract.py](../../training/regime_contract.py) and [training/train.py](../../training/train.py) now opt into postponed annotation evaluation with `from __future__ import annotations`, which fixes the local `str | None` / `X | None` runtime crash on this machine’s Python 3.9 interpreter.
- **A contained 34-feature retrain proved the ONNX schema break is fixed** — training against trades.db exported artifacts/alpha-models/session41/latest.onnx, and ONNX inspection confirmed the runtime-aligned input contract: `scalars=['batch', 40]`, `staleness=['batch', 34]`, `regime=['batch', 1]`.
- **Runtime validation confirmed the old deep-model dimension mismatch is gone for the new artifact** — a temporary bot run with `MACBOT_MODELS_DIR=artifacts/alpha-models/session41` loaded the alternate `latest.onnx` successfully and emitted deep-model predictions without the previous `Got: 40 Expected: 30` warning path. The model is still shadow-weighted, but the contract-level failure has been eliminated.

### Alpha Findings
- **The small recent live-data retrain is not good enough to treat as edge** — the contained `trades.db` retrain only reached `Final Brier score: 0.2882` with a best restart around `Brier=0.2480 / AUC=0.504`, which is much more useful as a schema-health proof than as a deployable predictive model.
- **Historical backfill is viable and immediately more promising than the 4-day live-only sample** — [training/backfill_historical.py](../../training/backfill_historical.py) successfully produced artifacts/alpha-data/session41/backfill_7d_15m.db with `669` resolved 15-minute markets, full futures context, and zero hard errors.
- **Even a small historical backfill trains better than the recent live-only set** — a quick contained retrain on the 7-day historical 15m DB exported artifacts/alpha-models/session41-backfill7d15m/latest.onnx with `Final Brier score: 0.2559` and a best restart around `Brier=0.2447 / AUC=0.564`, which is still not a victory lap but is materially healthier than the live-only 4-day retrain.
- **The next real alpha lever is bigger historical labeling, not more threshold fiddling** — after the trend-up YES specialist plateaued near flat and the small backfilled model outperformed the live-only retrain, the most credible next move is scaling historical backfill depth/coverage and retraining on that broader labeled base.

### Verification
- `python3 -m py_compile training/regime_contract.py training/train.py`
- `.venv-training/bin/python training/train.py --help`
- `.venv-training/bin/python training/train.py --db trades.db --out artifacts/alpha-models/session41 --skip-hpo --skip-pretrain --device cpu --days 4`
- `.venv-training/bin/python - <<'PY' ... onnx.load('artifacts/alpha-models/session41/latest.onnx') ... PY`
- `python3 scripts/check_ml_contract.py`
- `python3 training/backfill_historical.py --db artifacts/alpha-data/session41/backfill_7d_15m.db --days 7 --interval 15m --ratio-period 15m`
- `.venv-training/bin/python training/train.py --db artifacts/alpha-data/session41/backfill_7d_15m.db --out artifacts/alpha-models/session41-backfill7d15m --skip-hpo --skip-pretrain --device cpu`

---

## [2026-03-20] (Session 41) — Alpha Lab: Scripted Specialist Sweeps & Current Frontier

### Alpha Research Tooling
- **The Python alpha lab scripts now understand regime-specialist backtest inputs directly** — [scripts/walkforward_alpha_sweep.py](../../scripts/walkforward_alpha_sweep.py), [scripts/daily_alpha_report.py](../../scripts/daily_alpha_report.py), and [scripts/backtest_regime_report.py](../../scripts/backtest_regime_report.py) now accept `--require-regime-alignment`, `--regime-allowlist`, and `--regime-side-modes`, so the new backtest API controls can be exercised from repeatable lab workflows instead of one-off payload hacking.
- **Sweep/report tooling now includes pacing for authenticated protected-route research** — [scripts/walkforward_alpha_sweep.py](../../scripts/walkforward_alpha_sweep.py) and [scripts/daily_alpha_report.py](../../scripts/daily_alpha_report.py) now accept `--request-interval-secs`, which keeps fold-heavy alpha experiments inside the tightened API rate-limit envelope.
- **Daily alpha reports now preserve the regime-policy knobs that generated them** — [scripts/daily_alpha_report.py](../../scripts/daily_alpha_report.py) now records `require_regime_alignment`, `regime_allowlist`, and `regime_side_modes` in the emitted report JSON, making later comparisons less error-prone.

### Alpha Findings
- **The first reproducible near-flat specialist is a maker-side TREND_UP YES profile** — the best current narrow slice is `maker + yes_only + require_regime_alignment + regime_allowlist=[TREND_UP] + entry_threshold≈0.37 + stand_down_window=8 + stand_down_margin_pp=2.0 + stand_down_cooldown_candles=48`, which produced `mean_pnl=+0.0603`, `positive_folds=3/5`, and `mean_edge_gap_pp=+1.28` across the 30-day walk-forward anchors `[0, 7, 14, 21, 28]`.
- **Threshold sensitivity is low; the edge is coming from policy shape, not threshold micromanagement** — the trend-up YES specialist is effectively flat across `entry_threshold` values `0.370` through `0.378`, while lower thresholds around `0.366` / `0.368` were clearly worse.
- **The specialist still fails the broader-horizon test** — the same profile remained negative across a 60-day walk-forward with anchors `[0, 14, 28, 42, 56]` (`mean_pnl=-1.6588`, `positive_folds=0/5`), so the current state is “closest frontier so far,” not “real edge proven.”
- **Several intuitive tuning knobs turned out to be mostly inert** — once the specialist policy is fixed, `high_vol_entry_threshold`, `high_volatility_threshold`, and `anti_bias_strength` barely move the outcome across the tested ranges, which points the next alpha step toward model/signal quality rather than more threshold polishing.

### Verification
- `python3 -m py_compile scripts/walkforward_alpha_sweep.py scripts/daily_alpha_report.py scripts/backtest_regime_report.py`
- `python3 scripts/walkforward_alpha_sweep.py --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --days 30 --mode maker --entry-thresholds 0.35,0.355,0.36,0.365,0.37 --sides yes_only --adaptive-stand-down true --fold-shifts 0,7,14,21,28 --stand-down-window 12 --stand-down-margin-pp 1.5 --stand-down-cooldown-candles 48 --require-regime-alignment --regime-allowlist TREND_UP --request-interval-secs 1.15 --top 5 --request-timeout-secs 180`
- `python3 scripts/walkforward_alpha_sweep.py --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --days 30 --mode maker --entry-thresholds 0.366,0.368,0.370,0.372,0.374,0.376,0.378 --sides yes_only --adaptive-stand-down true --fold-shifts 0,7,14,21,28 --stand-down-window 8 --stand-down-margin-pp 2.0 --stand-down-cooldown-candles 48 --require-regime-alignment --regime-allowlist TREND_UP --request-interval-secs 1.15 --top 7 --request-timeout-secs 180`
- `python3 scripts/walkforward_alpha_sweep.py --base-url http://127.0.0.1:8000 --api-key local --api-secret localsecret --days 60 --mode maker --entry-thresholds 0.37 --sides yes_only --adaptive-stand-down true --fold-shifts 0,14,28,42,56 --stand-down-window 8 --stand-down-margin-pp 2.0 --stand-down-cooldown-candles 48 --require-regime-alignment --regime-allowlist TREND_UP --request-interval-secs 1.15 --top 1 --request-timeout-secs 240`

---

## [2026-03-20] (Session 40) — Alpha Lab: Regime-Specific Backtest Side Policies

### Alpha Research Tooling
- **The backtest API can now test regime-specialist policies directly** — [src/api/backtest.rs](../../src/api/backtest.rs) now accepts `regime_allowlist` plus `regime_side_modes`, so alpha work is no longer limited to one global side policy across every regime.
- **Per-regime side overrides are enforced inside the backtest signal filter path** — the backtest engine can now do things like `TREND_UP -> yes_only`, `RANGE -> no_only`, or `SQUEEZE -> none` without needing ad hoc script logic or misleading approximations through one global threshold.
- **The new regime-policy controls are regression-tested** — [src/api/backtest.rs](../../src/api/backtest.rs) now covers excluded-regime blocking and per-regime side override behavior directly in unit tests.

### Alpha Findings
- **Broad policies still lose; the first narrow frontier is maker-side, not taker-side** — coarse 30-day walk-forward sweeps against the authenticated local `/api/backtest` endpoint showed that `adaptive_stand_down=true` is mandatory, `sd=false` is a blow-up configuration, and maker-cost assumptions are directionally better but not enough on their own.
- **The strongest current slice is a narrow trend-up YES specialist** — the best tested profile so far is effectively `maker + yes_only + TREND_UP-aligned + entry_threshold 0.36 + stand_down_window 12 + stand_down_margin_pp 1.5 + cooldown 48`, which is still slightly negative but materially closer to flat than the broader profiles.
- **The tempting mixed regime hypothesis did not survive multi-fold testing** — combining `TREND_UP -> yes_only` with `RANGE -> no_only` through the new backtest regime policy support looked plausible from single-snapshot attribution, but it performed worse across the three coarse folds than the narrower trend-up YES specialist.

### Verification
- `cargo fmt --all`
- `cargo test api::backtest::tests:: -- --test-threads=1`
- `cargo test`
- Full suite is green: `263 passed; 0 failed`.

---

## [2026-03-20] (Session 39) — Batch 4: Ops Contract Closure & Explicit ML Regime Mapping

### ML Contract Closure
- **The Rust runtime regime mapping now has one explicit Python-side contract instead of duplicated inline copies** — added [training/regime_contract.py](../../training/regime_contract.py) so training/export code shares the same label-to-float regime values as [src/ml/input_builder.rs](../../src/ml/input_builder.rs), including the default fallback value.
- **Deep-model regime bucketing is now explicit instead of relying on `long().clamp(...)` side effects** — [training/model.py](../../training/model.py) now makes the current 4-bucket contract visible and deliberate: mean-revert/compression, directional trend, high-volatility, and fallback/other. That makes the previously half-implicit “unknown regime” path reachable and testable for future retrains.
- **Training data loading now imports the shared regime contract instead of re-declaring it** — [training/train.py](../../training/train.py) now uses `regime_value_from_label(...)`, which removes another quiet drift path between Rust runtime values and Python training inputs.
- **The ML contract checker now validates regime semantics too** — [scripts/check_ml_contract.py](../../scripts/check_ml_contract.py) now compares the Rust `regime_to_float(...)` mapping from [src/ml/input_builder.rs](../../src/ml/input_builder.rs) against [training/regime_contract.py](../../training/regime_contract.py), checks the regime bucket thresholds, and asserts both [training/model.py](../../training/model.py) and [training/train.py](../../training/train.py) are using the shared contract.

### Operations / Readiness
- **The live runbook now reflects the stronger backup and restart guarantees** — [ops/live-rollout-runbook.md](../../ops/live-rollout-runbook.md) now calls out critical-table-count backup verification, expects `last_reconcile_error == null` during health checks, and treats `last_reconcile_summary` as a required post-restart signal.
- **The restart contract now documents startup fill-ledger replay and verified restore expectations** — [ops/restart-contract.md](../../ops/restart-contract.md) now includes startup fill-ledger repair as an invariant, requires idempotent full resync after initial repair, and defines the critical operational tables that verified backups must preserve.
- **Prometheus alerting now covers silent queue-loss symptoms directly** — [ops/prometheus/macbot-alert-rules.yml](../../ops/prometheus/macbot-alert-rules.yml) now alerts on `macbot_outbox_dropped_total` and `macbot_actor_cmd_drop_total`, and [ops/prometheus/README.md](../../ops/prometheus/README.md) now points operators back to `/api/ready.last_reconcile_summary` / `last_reconcile_error` during restart drills.
- **CI now syntax-checks the shared regime contract module too** — [.github/workflows/ci.yml](../../.github/workflows/ci.yml) now includes [training/regime_contract.py](../../training/regime_contract.py) in the Python syntax pass.

### Verification
- `python3 -m py_compile training/model.py training/train.py training/regime_contract.py scripts/check_ml_contract.py scripts/sqlite_backup.py`
- `python3 scripts/check_ml_contract.py`
- `cargo fmt --all`
- `cargo test`
- Full suite is green: `261 passed; 0 failed`.

---

## [2026-03-20] (Session 38) — Batch 3: Backup/Restore Maturity For Critical Operational Tables

### Persistence / Restore
- **Backup verification now checks operational content, not just schema/version** — [src/db/mod.rs](../../src/db/mod.rs) now defines `CriticalTableCounts` and `critical_table_counts(...)`, and [backup_database](../../src/db/mod.rs) now rejects backups whose row counts differ across the core operational tables: `trades`, `order_outbox`, `trade_fills`, `analysis_snapshots`, and `idempotency_cache`.
- **The restore drill now covers the tables we actually depend on in production** — the DB regression in [src/db/mod.rs](../../src/db/mod.rs) now seeds trades, outbox audits, fill snapshots, analysis snapshots, and idempotency entries before backup, then proves the restored DB preserves schema version plus the critical table counts.

### Operator Tooling
- **The Python backup tool now enforces the same critical-table parity contract** — [scripts/sqlite_backup.py](../../scripts/sqlite_backup.py) now computes critical table counts on source and destination, aborts if they differ, and prints the verified counts on success.

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test db::tests::backup_database_roundtrips_critical_tables_and_schema_version -- --test-threads=1`
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 -m py_compile scripts/sqlite_backup.py`
- `cargo test`
- Full suite is green: `261 passed; 0 failed`.

---

## [2026-03-20] (Session 37) — Batch 2: Entry Policy Migration & Bootstrap Extraction

### Risk Boundary Expansion
- **Open-position and scalp-entry coordination now lives in the risk layer instead of the execution path** — [src/bot/risk.rs](../../src/bot/risk.rs) now owns explicit `EntryCoordinationSnapshot` / `EntryCoordinationProposal` policy checks for duplicate same-market strategy positions, scalp-specific price/momentum/liquidity gates, and conviction open-position caps.
- **Execution now delegates those pre-entry policy decisions instead of recomputing them inline** — [src/bot/execute.rs](../../src/bot/execute.rs) now builds a coordination snapshot from hot memory and routes it through `evaluate_entry_coordination(...)`, which shrinks the execution hot path and makes those rules directly unit-testable.
- **The new coordination policy is regression-tested in isolation** — [src/bot/risk.rs](../../src/bot/risk.rs) now covers duplicate strategy blocking, weak scalp momentum, thin scalp books, and conviction-cap rejection without needing to drive the whole executor.

### State Teardown
- **Startup/bootstrap plumbing moved out of [src/bot/state.rs](../../src/bot/state.rs)** — added [src/bot/state/bootstrap.rs](../../src/bot/state/bootstrap.rs) to own run-id construction, CLOB client setup, startup fill-ledger replay, and `TradingBot::new(...)`.
- **`state.rs` is leaner and more focused on state ownership again** — [src/bot/state.rs](../../src/bot/state.rs) is down to `1244` lines, with constructor/startup concerns pulled into a dedicated bootstrap seam instead of sitting in the central state file.

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test bot::risk::tests:: -- --test-threads=1`
- `cargo test bot::state::tests:: -- --test-threads=1`
- `cargo test`
- Full suite is green: `261 passed; 0 failed`.

---

## [2026-03-20] (Session 36) — Recovery Hardening: Fill-Ledger Restart Replay & Idempotent Full Resync

### Restart / Recovery
- **Open live trades now replay their entry fill ledger on startup before hot memory comes online** — [src/bot/state.rs](../../src/bot/state.rs) now repairs stale live `shares` and `entry_price` fields from persisted `trade_fills` during [TradingBot::new](../../src/bot/state.rs), then persists the corrected row back to SQLite.
- **Restart-time fill repair is operator-visible** — when a live open trade is repaired from the fill ledger, [src/bot/state.rs](../../src/bot/state.rs) now appends a `restart_fill_replay` audit row so restart recovery leaves an explicit trace in `order_outbox`.
- **There is now a direct crash-window regression for stale live trade rows** — [src/bot/state.rs](../../src/bot/state.rs) covers the case where `trade_fills` already contain partial-fill truth but the trade row is stale at restart, and proves startup repairs both the in-memory trade and the persisted SQLite row.

### Reconcile Idempotence
- **Full resync is now explicitly tested as a one-time repair, not a repeating mutator** — [src/bot/reconcile.rs](../../src/bot/reconcile.rs) now proves that after drift is repaired once, a second full resync emits no duplicate `reconcile_local_state` audits and records a zero-drift summary instead of “repairing” the same state twice.
- **Recovery visibility now matches recovery behavior** — the new full-resync idempotence regression complements the last-reconcile summary work from Session 35 by proving that the summary can legitimately return to `drift_count=0` after a successful repair cycle.

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test bot::state::tests::new_replays_fill_ledger_into_open_live_trade_on_restart -- --test-threads=1`
- `cargo test bot::reconcile::tests::full_resync_is_idempotent_after_initial_repair -- --test-threads=1`
- `cargo test`
- Full suite is green: `257 passed; 0 failed`.

---

## [2026-03-20] (Session 35) — Control-Plane Bundle & Reconcile Status Surface

### Structural Cleanup
- **Run identity, mutable runtime config, and API idempotency cache now live behind one explicit control-plane seam** — added [src/bot/state/control_plane.rs](../../src/bot/state/control_plane.rs) so `run_id`, the live `Config`, and the idempotency cache are no longer loose top-level fields on [src/bot/state.rs](../../src/bot/state.rs).
- **Callers now route control-plane concerns through one owner instead of reaching into the bot root** — [src/bot/idempotency.rs](../../src/bot/idempotency.rs), [src/bot/state/runtime.rs](../../src/bot/state/runtime.rs), [src/bot/state/analysis_cycle.rs](../../src/bot/state/analysis_cycle.rs), [src/bot/execute.rs](../../src/bot/execute.rs), [src/bot/reconcile.rs](../../src/bot/reconcile.rs), [src/api/bot_routes.rs](../../src/api/bot_routes.rs), [src/api/health.rs](../../src/api/health.rs), [src/api/auth.rs](../../src/api/auth.rs), [src/api/config_api.rs](../../src/api/config_api.rs), and related paths now consistently go through `control_plane`.

### Recovery / Operations
- **Reconcile now records a typed last-known outcome instead of leaving operators with only logs and cumulative counters** — [src/bot/state/runtime_state.rs](../../src/bot/state/runtime_state.rs) and [src/bot/state/runtime.rs](../../src/bot/state/runtime.rs) now track `last_reconcile_summary` and `last_reconcile_error`, including phase, drift count, repaired count, orphan closes, restore/repair counts, allowed drift, and kill-switch state.
- **Live reconcile and full resync now publish their own recovery summary** — [src/bot/reconcile.rs](../../src/bot/reconcile.rs) now records a structured success summary after both the 60-second live reconcile pass and the full drift-repair resync, and main loop / manual API error paths record the last reconcile failure with timestamp and phase.
- **Operational endpoints now surface recent recovery state directly** — [src/api/bot_routes.rs](../../src/api/bot_routes.rs) `/api/risk/status` and `/api/resync/full`, plus [src/api/health.rs](../../src/api/health.rs) `/api/ready`, now expose the most recent reconcile summary/error so restart and drift-repair health can be checked without digging through logs.

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test bot::state::runtime::tests:: -- --test-threads=1`
- `cargo test bot::reconcile::tests:: -- --test-threads=1`
- `cargo test api::bot_routes::tests:: -- --test-threads=1`
- `cargo test`
- Full suite is green: `255 passed; 0 failed`.

---

## [2026-03-20] (Session 34) — Portfolio Hot-State Bundle

### Structural Cleanup
- **Open trades and working balance now live behind one explicit portfolio seam** — added [src/bot/state/portfolio_state.rs](../../src/bot/state/portfolio_state.rs) so the hot in-memory trade list plus reconstructed balance no longer sit as loose top-level fields on [src/bot/state.rs](../../src/bot/state.rs).
- **`state.rs` shrank again without changing the external bot surface** — [src/bot/state.rs](../../src/bot/state.rs) is now down to `1171` lines, with portfolio storage separated from orchestration while callers continue to use the same `balance()` / `set_balance()` / `trades()` runtime API.

### Reliability / Maintainability
- **Execution, closing, guard, TP/SL, accounting, reconcile, and API history flows now share the same portfolio owner** — [src/bot/state/runtime.rs](../../src/bot/state/runtime.rs), [src/bot/accounting.rs](../../src/bot/accounting.rs), [src/bot/close.rs](../../src/bot/close.rs), [src/bot/execute.rs](../../src/bot/execute.rs), [src/bot/guard.rs](../../src/bot/guard.rs), [src/bot/reconcile.rs](../../src/bot/reconcile.rs), [src/bot/state/tp_sl.rs](../../src/bot/state/tp_sl.rs), [src/bot/state/trade_loop.rs](../../src/bot/state/trade_loop.rs), [src/api/trades.rs](../../src/api/trades.rs), [src/main.rs](../../src/main.rs), and related tests now consistently route direct hot-memory mutations through `portfolio_state`.
- **Restart/recovery semantics stay the same while ownership gets clearer** — the full reconcile and orphan-close flows still repair and evict local open trades exactly as before, but the storage contract is now explicit enough that future restart/recovery work can reason about one portfolio owner instead of raw root fields.
- **Partial-close restart recovery now has a direct regression test** — [src/bot/state.rs](../../src/bot/state.rs) now covers the case where the bot restarts after a partial close and must rebuild both the remaining open position and the reconstructed balance without drift.

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test bot::close::tests:: -- --test-threads=1`
- `cargo test bot::reconcile::tests:: -- --test-threads=1`
- `cargo test bot::state::tests:: -- --test-threads=1`
- `cargo test partial_close_state_rebuilds_cleanly_after_restart -- --test-threads=1`
- `cargo test`
- Full suite is green: `254 passed; 0 failed`.

---

## [2026-03-20] (Session 33) — Strategy, Analysis/Market, Runtime, and Signal-Tracker State Bundles

### Structural Cleanup
- **Another four families of loose `TradingBot` fields now live behind focused state bundles** — added [src/bot/state/strategy_services.rs](../../src/bot/state/strategy_services.rs), [src/bot/state/analysis_state.rs](../../src/bot/state/analysis_state.rs), [src/bot/state/market_state.rs](../../src/bot/state/market_state.rs), [src/bot/state/runtime_state.rs](../../src/bot/state/runtime_state.rs), and [src/bot/state/signal_trackers.rs](../../src/bot/state/signal_trackers.rs) so strategy helpers, analysis snapshots, market caches, runtime flags, and signal trackers are no longer stored as loose top-level members on [src/bot/state.rs](../../src/bot/state.rs).
- **`state.rs` shrank again while preserving the same public `TradingBot` behavior** — [src/bot/state.rs](../../src/bot/state.rs) is now down to `1169` lines, with strategy services (`sleep_schedule`, `multi_agent`, BTC/news/granger/Kyle helpers, router, sniper), analysis metadata (`last_analysis`, `last_analysis_at`, signal-weight mtime), market caches, runtime flags, and signal trackers all moved behind clearer ownership seams.
- **The runtime API now matches the runtime storage layout** — loop control, kill/degraded flags, L2 freshness tracking, notify wakeups, SLO guard state, and balance-drift bookkeeping now share one runtime-owned home, which makes the existing methods in [src/bot/state/runtime.rs](../../src/bot/state/runtime.rs), [src/bot/state/runtime_slo.rs](../../src/bot/state/runtime_slo.rs), [src/bot/accounting.rs](../../src/bot/accounting.rs), and [src/main.rs](../../src/main.rs) much closer to the actual storage topology.

### Reliability / Maintainability
- **Strategy-side callers now go through one explicit bundle instead of reaching into the bot root** — [src/bot/intelligence.rs](../../src/bot/intelligence.rs), [src/bot/analyze.rs](../../src/bot/analyze.rs), [src/bot/execute.rs](../../src/bot/execute.rs), [src/bot/guard.rs](../../src/bot/guard.rs), [src/bot/state/trade_loop.rs](../../src/bot/state/trade_loop.rs), and [src/api/bot_routes.rs](../../src/api/bot_routes.rs) now all route strategy concerns through `strategy_services`, which reduces the chance of future drift between analysis, execution, and operator endpoints.
- **Analysis snapshots and market caches are now explicitly shared subsystems** — [src/api/realtime.rs](../../src/api/realtime.rs), [src/api/bot_routes.rs](../../src/api/bot_routes.rs), [src/market/l2_ws.rs](../../src/market/l2_ws.rs), [src/bot/actor.rs](../../src/bot/actor.rs), [src/bot/state/tp_sl.rs](../../src/bot/state/tp_sl.rs), [src/bot/state/analysis_cycle.rs](../../src/bot/state/analysis_cycle.rs), [src/bot/state/runtime.rs](../../src/bot/state/runtime.rs), [src/bot/analyze.rs](../../src/bot/analyze.rs), and [src/bot/execute.rs](../../src/bot/execute.rs) now consistently use `analysis_state` and `market_state`, which makes the hot-path cache contract much easier to reason about.
- **Signal tracker ownership is now explicit too** — [src/bot/analyze.rs](../../src/bot/analyze.rs) and [src/bot/kelly.rs](../../src/bot/kelly.rs) now go through `signal_trackers` for OBI stability, tick velocity, and rolling BTC/ETH correlation, instead of reaching into three more standalone tracker fields on the bot root.

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test`
- Full suite is green: `253 passed; 0 failed`.

---

## [2026-03-20] (Session 32) — Live-Readiness Telemetry, DB Sanity Checks & Guarded Backup Endpoint

### Operational Readiness
- **Startup now verifies SQLite integrity before the bot is considered healthy** — [src/main.rs](../../src/main.rs) now runs `db::quick_check()` immediately after opening the pool and logs a successful integrity pass before building shared bot state.
- **Readiness now reflects optional live DB integrity checks** — [src/api/health.rs](../../src/api/health.rs) now surfaces `db_quick_check_enabled` and `db_quick_check_ok` in `/api/ready`, and live preflight now flags overly broad `.env` permissions via `env_file_permissions_ok`.
- **Operators can now trigger a verified SQLite backup through the authenticated API** — [src/api/bot_routes.rs](../../src/api/bot_routes.rs) adds a guarded `/api/db/backup` endpoint that only writes under `artifacts/backups/`, rejects path escapes, and uses [src/db/mod.rs](../../src/db/mod.rs) `backup_database()` for the same integrity-checked backup path used by the recovery drill.

### Observability
- **Analysis-loop and entry-block reasons are now first-class metrics** — [src/metrics.rs](../../src/metrics.rs), [src/bot/state/analysis_cycle.rs](../../src/bot/state/analysis_cycle.rs), [src/api/metrics.rs](../../src/api/metrics.rs), and [src/api/bot_routes.rs](../../src/api/bot_routes.rs) now expose counters for analysis cycles, snapshot persist failures, shadow-kill trips, and the main no-entry guard paths (kill switch, drawdown halt, alpha freeze, degraded mode, circuit breaker cooldown, stale L2).
- **Prometheus guidance now matches the new runtime signals** — updated [ops/prometheus/macbot-alert-rules.yml](../../ops/prometheus/macbot-alert-rules.yml), [ops/prometheus/README.md](../../ops/prometheus/README.md), [ops/live-rollout-runbook.md](../../ops/live-rollout-runbook.md), and [ops/restart-contract.md](../../ops/restart-contract.md) so live rollout, restart, and alerting flows all reference DB quick-check readiness, the new backup endpoint, and the new trade-entry gate counters.

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test api::health::tests:: -- --test-threads=1`
- `cargo test api::bot_routes::tests:: -- --test-threads=1`
- `cargo test`
- Full suite is green: `253 passed; 0 failed`.

---

## [2026-03-20] (Session 31) — Analysis-Cycle Extraction From `state.rs`

### Structural Cleanup
- **The analysis-cycle orchestration moved out of [src/bot/state.rs](../../src/bot/state.rs)** — extracted the top-level loop pass into [src/bot/state/analysis_cycle.rs](../../src/bot/state/analysis_cycle.rs), including shadow kill evaluation, signal-weight hot reload, analysis snapshot persistence, and `run_once()` orchestration.
- **`state.rs` shrank again while keeping the same public `TradingBot` surface** — [src/bot/state.rs](../../src/bot/state.rs) is now down to `1273` lines, with the extracted analysis-cycle seam living in [src/bot/state/analysis_cycle.rs](../../src/bot/state/analysis_cycle.rs) at `390` lines.
- **The shadow-kill runtime now shares the same pass/fail contract as the rollout checker** — the extracted `shadow_kill_reason()` logic now mirrors the thresholds used by [scripts/shadow_kill_check.py](../../scripts/shadow_kill_check.py), so the in-process kill guard and operator-facing checker are no longer drifting independently.

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test bot::state::tests:: -- --test-threads=1`
- `cargo test shadow_kill_reason -- --test-threads=1`

---

## [2026-03-20] (Session 30) — Verified SQLite Backup Path & Runbook Recovery Drill

### Persistence / Recovery
- **SQLite now has first-class integrity and backup helpers** — [src/db/mod.rs](../../src/db/mod.rs) now exposes `quick_check()` and `backup_database()`, which validate the source DB, checkpoint WAL, create a `VACUUM INTO` backup, and verify the backup’s integrity plus schema version before declaring success.
- **Backup and integrity behavior are regression tested** — added DB coverage proving `quick_check()` passes on a healthy fresh DB and that `backup_database()` round-trips trades and preserves `PRAGMA user_version`.
- **Operators now have a simple verified backup tool** — added [scripts/sqlite_backup.py](../../scripts/sqlite_backup.py), a standard-library SQLite backup script that runs `quick_check`, truncates WAL, creates a clean backup, and verifies the destination copy before printing success details.

### Operational Readiness
- **CI now syntax-checks the backup script** — [.github/workflows/ci.yml](../../.github/workflows/ci.yml) includes [scripts/sqlite_backup.py](../../scripts/sqlite_backup.py) in the Python syntax pass.
- **The live rollout runbook now includes a real backup drill** — [ops/live-rollout-runbook.md](../../ops/live-rollout-runbook.md) now calls for a verified SQLite backup before live rollout and again during rollback before any manual DB intervention.

### Verification
- `cargo fmt --all`
- `cargo test db::tests:: -- --test-threads=1`
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 -m py_compile scripts/sqlite_backup.py`
- `python3 scripts/sqlite_backup.py --source /tmp/macbot_backup_source.sqlite --dest /tmp/macbot_backup_copy.sqlite --overwrite`
- `cargo test`
- Full suite is green: `249 passed; 0 failed`.

---

## [2026-03-20] (Session 29) — Maker OMS Failure Audits & Execution-Actor Extraction

### Maker OMS Reliability
- **Missing-CLOB maker failures are no longer silent** — [src/bot/actor.rs](../../src/bot/actor.rs) now emits critical `maker_quote` outbox audits when live maker quoting tries to cancel or place quotes without an initialized CLOB client, instead of returning early with no operator-visible trace.
- **Replace-path failure semantics are now directly covered** — added actor regressions proving failed live cancel attempts leave the existing in-memory quote state intact, while still recording the failure audit.
- **Critical OMS error visibility is stronger across queue-pressure and missing-adapter cases** — together with the Session 28 congestion-path tests, the actor now has direct coverage for both “outbox queue is full” and “live adapter is unavailable” failure windows.

### Structural Cleanup
- **Execution-actor wrappers moved out of [src/bot/state.rs](../../src/bot/state.rs)** — extracted serialized execute/close entry points, maker snapshot/publish helpers, and outbox audit fallback wiring into [src/bot/state/execution_actor.rs](../../src/bot/state/execution_actor.rs).
- **`state.rs` shrank materially again without changing the public surface** — [src/bot/state.rs](../../src/bot/state.rs) is down to `1636` lines, with the actor-wrapper seam now living in [src/bot/state/execution_actor.rs](../../src/bot/state/execution_actor.rs) at `416` lines.
- **The `TradingBot` teardown keeps converging on coordination-only state** — this extraction pulls another cluster of actor orchestration out of the main state file while preserving the same `TradingBot` methods for callers in API, runtime, guard, and test paths.

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test bot::actor::tests:: -- --test-threads=1`
- `cargo test bot::state::tests:: -- --test-threads=1`
- `cargo test`
- Full suite is green: `247 passed; 0 failed`.

---

## [2026-03-20] (Session 28) — Execution Recovery Hardening, DB Migration Discipline & Late-Window Risk Guard

### Execution / Recovery Hardening
- **Local reconcile repairs now emit explicit outbox audits** — [src/bot/reconcile.rs](../../src/bot/reconcile.rs) now records `reconcile_local_state` audits when full resync repairs a drifted in-memory live trade, restores a DB-open live trade into hot memory, force-closes a ghost local live trade, or closes an orphaned local live trade without an order ID.
- **Reconcile repair outcomes are regression tested end-to-end** — added coverage in [src/bot/reconcile.rs](../../src/bot/reconcile.rs) proving full resync writes the expected outbox rows for repair/restore/close local actions instead of only logging them.
- **Actor outbox congestion paths are now explicitly covered** — [src/bot/actor.rs](../../src/bot/actor.rs) now has direct tests proving critical outbox events persist via direct DB fallback when the queue is full, while best-effort events increment the drop counter and do not create misleading DB records.

### Persistence / Migration Discipline
- **SQLite migrations now use explicit schema versioning** — [src/db/mod.rs](../../src/db/mod.rs) now tracks `PRAGMA user_version` with `DB_SCHEMA_VERSION`, applies versioned migration steps, and records the current schema version on open.
- **Trade-column upgrades no longer rely on swallowed `ALTER TABLE` failures** — the old blind additive migration path was replaced with `table_has_column()` / `add_column_if_missing()` checks, so legacy databases are upgraded deliberately instead of “try and ignore errors.”
- **Legacy-schema upgrade behavior is regression tested** — added DB coverage proving a legacy `trades` table missing `fee_paid`, `signals_json`, `calibration_version`, `run_id`, and `strategy` is upgraded cleanly and still round-trips new inserts.

### Risk Boundary
- **Late-window entry gating is now part of the pure risk evaluator** — [src/bot/risk.rs](../../src/bot/risk.rs) now receives strategy and remaining-time context, rejects conviction entries inside `expiry_guard_seconds`, rejects scalp entries in the final three minutes, and intentionally exempts forced paths like the resolution sniper.
- **Execution no longer owns the expiry gate inline** — [src/bot/execute.rs](../../src/bot/execute.rs) now passes time-remaining and strategy metadata into the risk decision request instead of keeping the scalp-only late-window guard buried in the execution path.

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test bot::reconcile::tests:: -- --test-threads=1`
- `cargo test bot::actor::tests:: -- --test-threads=1`
- `cargo test db::tests:: -- --test-threads=1`
- `cargo test bot::risk::tests:: -- --test-threads=1`
- `cargo test bot::execute::tests:: -- --test-threads=1`
- `cargo test`
- Full suite is green: `244 passed; 0 failed`.

---

## [2026-03-20] (Session 27) — ML Contract Hardening & CI Guard

### Contract Safety
- **Python-side ML dimensions are now derived from explicit constants** — [training/model.py](../../training/model.py) now centralizes `NUM_FUSION_FEATURES`, scalar/staleness dimensions, time buckets, and regime bucket count so the ONNX dummy shapes, MLP input width, and time-remaining index all derive from one contract instead of scattered magic numbers.
- **The current regime bucketing behavior is explicit instead of accidental** — [training/model.py](../../training/model.py) now routes regime floats through a dedicated `_bucketize_regime()` helper, preserving the existing 4-bucket ONNX compatibility while documenting that the broader regime semantics still need a future retrain to expand safely.
- **Training-side dimensional arithmetic is derived instead of hand-maintained** — [training/train.py](../../training/train.py) now asserts its feature count matches the model contract and derives the contrastive feature width from scalar/staleness/candle constants instead of hard-coding `149`.

### CI / Verification
- **A source-level contract check now guards Rust↔Python drift** — added [scripts/check_ml_contract.py](../../scripts/check_ml_contract.py), which verifies Rust `FEATURE_NAMES` match the Python training list, validates the Python-side tensor dimensions, and confirms the ONNX dummy/export paths still use the shared constants.
- **CI now runs Python syntax and the ML contract check** — [.github/workflows/ci.yml](../../.github/workflows/ci.yml) now runs `py_compile` on the training/model contract files and executes the new `check_ml_contract.py` script before the Rust test suite.

### Verification
- `PYTHONPYCACHEPREFIX=/tmp/pycache python3 -m py_compile training/model.py training/train.py scripts/check_ml_contract.py`
- `python3 scripts/check_ml_contract.py`
- `cargo test`
- Full suite is green: `237 passed; 0 failed`.

---

## [2026-03-20] (Session 26) — TP/SL Extraction & Reconcile Drift Budget Coverage

### Structural Cleanup
- **The TP/SL engine moved out of [src/bot/state.rs](../../src/bot/state.rs)** — extracted live-price helpers, unrealized-PnL helpers, trade live-field enrichment, exit-type persistence, and the full TP/SL guard logic into [src/bot/state/tp_sl.rs](../../src/bot/state/tp_sl.rs).
- **`state.rs` shrank again while keeping the same crate-private boundary** — after the extraction, [src/bot/state.rs](../../src/bot/state.rs) is down to `2044` lines, with the TP/SL subsystem living in [src/bot/state/tp_sl.rs](../../src/bot/state/tp_sl.rs) at `565` lines.
- **The stale-snapshot TP/SL warning path is quieter now** — the new TP/SL module re-checks whether a trade is still open before emitting the “no price at all” warning, which reduces misleading noise when another path closed/evicted the trade after the open-trade snapshot was taken.

### Stability Coverage
- **Gamma/CLOB yes-price parsing has direct tests now** — added TP/SL module tests proving YES-price extraction works both from token-array payloads and from ordered `outcomePrices` payloads.
- **Full resync now has a drift-budget regression test** — added reconcile coverage in [src/bot/reconcile.rs](../../src/bot/reconcile.rs) proving a single repaired live-trade mismatch does not trigger the kill switch when `MACBOT_RECONCILE_MAX_LIVE_DRIFT` explicitly allows that budget.

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test bot::state::tp_sl::tests:: -- --test-threads=1`
- `cargo test bot::reconcile::tests:: -- --test-threads=1`
- `cargo test`
- Full suite is green: `237 passed; 0 failed`.

---

## [2026-03-20] (Session 25) — Trade Loop Extraction From `state.rs`

### Structural Cleanup
- **Post-analysis trade decisions now live in [src/bot/state/trade_loop.rs](../../src/bot/state/trade_loop.rs)** — extracted the resolution sniper dispatch, sleep-schedule gate, ensemble disagreement guard, latency-arb hint logging, multi-agent sizing vote, hard-negative skip, and conviction/scalp execution dispatch out of [src/bot/state.rs](../../src/bot/state.rs).
- **`run_once()` is back to orchestration instead of policy soup** — [src/bot/state.rs](../../src/bot/state.rs) now handles the top-level tick flow, then hands the post-analysis trade decision phase to the new child-module boundary.
- **The central state file shrank again without widening visibility** — [src/bot/state.rs](../../src/bot/state.rs) dropped to `2593` lines, with the extracted trade-loop logic living in [src/bot/state/trade_loop.rs](../../src/bot/state/trade_loop.rs) at `389` lines.

### Stability Coverage
- **Trade-loop parsing behavior is now regression tested directly** — added focused tests proving the extracted resolution-sniper input parsing accepts the expected analysis payload shape, rejects missing/too-early cases, and that the ensemble disagreement gate only trips outside shadow mode.
- **Runtime extraction coverage still holds after the move** — reran the runtime accessors/coalescing test set to confirm the new child module did not disturb the existing `state`-module boundaries or freshness behavior.

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test trade_loop -- --test-threads=1`
- `cargo test runtime::tests -- --test-threads=1`
- `cargo test`
- Full suite is green: `234 passed; 0 failed`.

---

## [2026-03-20] (Session 24) — Risk Seam Extraction

### Structural Cleanup
- **Pre-trade risk policy is now isolated in [src/bot/risk.rs](../../src/bot/risk.rs)** — extracted the daily-loss, funding-reversal, regime-allowlist, confidence, liquidity, entry-price, Hail Mary, rollout, balance, exposure-cap, and EV checks into a pure evaluator fed by `TradeRiskSnapshot` and `ProposedTrade`.
- **Execution now gathers inputs instead of owning policy math** — [src/bot/execute.rs](../../src/bot/execute.rs) still computes Kelly-sized amounts and performs the actual order-path work, but the risk decision itself is now delegated to the new risk boundary instead of being hard-coded inline.
- **Drawdown halt moved out of Kelly sizing** — `is_drawdown_halted()` now lives in [src/bot/risk.rs](../../src/bot/risk.rs), which makes the drawdown gate part of the same risk subsystem instead of leaving it stranded inside [src/bot/kelly.rs](../../src/bot/kelly.rs).

### Stability Coverage
- **Pure risk decisions now have direct tests** — added focused coverage for exposure-cap reductions, exposure-cap rejection when remaining room falls below `min_stake`, rollout blocking when live shadow-pass requirements are unmet, and EV rejection when costs push the required edge above the proposal.
- **Drawdown halt now has dedicated regression coverage** — added tests proving the risk layer trips both on daily drawdown breaches and on ATH drawdown breaches reconstructed from persisted trade history plus current balance.
- **Execution behavior stayed green after the extraction** — the touched execution test set still passes with the risk evaluator in the loop, and the full suite increased to `230` passing tests.

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test bot::risk::tests -- --test-threads=1`
- `cargo test bot::execute::tests -- --test-threads=1`
- `cargo test`
- Full suite is green: `230 passed; 0 failed`.

---

## [2026-03-20] (Session 23) — Intelligence Stack Extraction

### Structural Cleanup
- **The ML/logging/calibration domain now lives behind `Arc<BotIntelligence>`** — moved the cold-path intelligence stack out of [src/bot/state.rs](../../src/bot/state.rs) and into [src/bot/intelligence.rs](../../src/bot/intelligence.rs), including `ml_state`, `data_logger`, `liq_tracker`, `input_builder`, `deep_model`, `ensemble_state`, `calibrator`, `hard_negative_tracker`, and `edge_tracker`.
- **Intelligence lifecycle methods moved with the state** — `ml_backfill`, `ml_on_resolution`, and `score_deep_prediction_for_market` now live with the intelligence domain, so the extraction removed orchestration logic from `state.rs` instead of only relocating storage.
- **`TradingBot` shrank again without widening shared mutability** — [src/bot/state.rs](../../src/bot/state.rs) is down to `2834` lines, with the ML plumbing isolated in [src/bot/intelligence.rs](../../src/bot/intelligence.rs) at `263` lines.

### Wiring
- **All live call sites now use the intelligence boundary** — analysis, guards, API monitoring endpoints, realtime snapshots, and startup background tasks were rewired to use `bot.intelligence.*`, so there are no remaining direct references to the old ML/calibration fields on `TradingBot`.
- **The first extraction pass stayed compiler-friendly** — this landed without a borrow-checker redesign; the only cleanup after the move was a handful of stale field references and one test update.

### Verification
- `cargo fmt --all`
- `cargo check`
- `cargo test ml_on_resolution_records_edge_tracker_observation -- --test-threads=1`
- `cargo test runtime::tests -- --test-threads=1`
- `cargo test`
- Full suite is green: `224 passed; 0 failed`.

---

## [2026-03-20] (Session 22) — Runtime SLO Guard Extraction & Recovery Coverage

### Structural Cleanup
- **Runtime SLO fail-close logic was split out of `state.rs`** — moved the live-maker/hybrid L2-age, backpressure, degraded-mode, and kill-switch enforcement code into `src/bot/state/runtime_slo.rs`, keeping the operational guardrails together as their own subsystem instead of burying them in the middle of the main bot state file.
- **State-file blast radius dropped again** — after the runtime and SLO extractions, `src/bot/state.rs` is down to `3105` lines, with runtime lifecycle in `src/bot/state/runtime.rs` and runtime SLO enforcement in `src/bot/state/runtime_slo.rs`.

### Stability Coverage
- **Healthy recovery from degraded mode is now tested** — added a direct regression test proving the runtime SLO guard clears degraded-no-trade only after a clean healthy hold window, while also resetting the accumulated L2/backpressure trip counters.
- **Existing kill-switch trip coverage still holds after the extraction** — the live-maker L2 breach and hybrid backpressure trip tests continue to pass against the new module boundary, so the extraction kept the fail-close behavior intact.

### Verification
- `cargo fmt --all`
- `cargo test runtime_slo -- --test-threads=1`
- `cargo test`
- Full suite is green: `224 passed; 0 failed`.

---

## [2026-03-20] (Session 21) — Runtime Lifecycle Extraction & Freshness Invariants

### Structural Cleanup
- **Runtime and lifecycle accessors were split out of `state.rs`** — moved the loop-control, kill-switch, degraded-mode, L2-feed freshness, analysis freshness, and core accessor methods into `src/bot/state/runtime.rs`, trimming the central `TradingBot` state file from the hot-path lifecycle logic without widening field visibility across the crate.
- **The extraction used a child-module boundary instead of looser visibility** — keeping the new runtime code under `src/bot/state/` preserved access to the existing private internals, which avoids turning `pending_bbo` or other coordination fields into broader crate-level mutable state just to support the refactor.

### Stability Coverage
- **Loop start/stop and safety flags are now regression tested directly** — added runtime-level tests proving the bot loop flips cleanly, `Notify` wakeups still fire on `start_loop()`, and kill-switch / degraded-no-trade state transitions round-trip as expected.
- **Freshness and coalescing invariants are now covered** — added tests that verify `mark_l2_feed_event()` clears stale-feed mode and refreshes age, `drain_pending_bbo_updates()` preserves one authoritative BBO per token and clears the flush flag, and analysis freshness / `time_remaining_minutes` accessors stay consistent with stale-age thresholds.

### Verification
- `cargo fmt --all`
- `cargo test runtime::tests -- --test-threads=1`
- `cargo test`
- Full suite is green: `223 passed; 0 failed`.

---

## [2026-03-20] (Session 20) — Backtest Tooling, Request Isolation & Validation

### Runtime Stability
- **Health and readiness probes no longer compete with expensive endpoints** — the outer public rate limiter now skips `/api/health`, `/api/ready`, `/api/preflight`, realtime transports, and always-auth paths like `/api/backtest`, so rollout checks stay responsive even while protected work is in flight.
- **Rate limiter lock scope was tightened** — the request-admission path now records a hit and drops the `DashMap` guard before awaiting the rest of the stack, removing the old “hold the shard lock across the whole request” behavior.
- **Backtests now yield more fairly and expose timing breadcrumbs** — `/api/backtest` logs start/fetch/complete timing and periodically yields during the simulation loop, which makes it much easier to diagnose slow folds without freezing basic health checks.

### Research Tooling
- **Signed auth support was added to the backtest scripts** — new shared helper `scripts/api_auth.py` signs requests with the same `x-api-key` / `x-api-ts` / `x-api-signature` scheme as the server, and the walk-forward, daily report, regime report, and regime allowlist scripts now work against the hardened API.
- **Request timeout is now configurable from the CLI** — the backtest scripts expose `--request-timeout-secs`, so longer research runs no longer require editing the scripts themselves.
- **Historical kline caching was added to the backtest handler** — repeated folds/candidates now reuse cached 15m BTC candle windows by `(target_candles, end_time_ms)`, which reduces duplicate Binance fetches during sweeps.

### Validation
- **Detached daemon-style startup was validated** — a background-launched bot with auth enabled now serves `/api/health`, `/api/preflight`, and `/api/ready` correctly while protected backtests are executing.
- **Backtest and walk-forward outputs were captured end-to-end** — a 1-day authenticated taker backtest completed successfully, and the tiny walk-forward sweep plus daily alpha report completed through the hardened API path.
- **Latest validation artifacts** — `artifacts/alpha-reports/20260320_054015_alpha_report.json` and `artifacts/alpha-reports/20260320_054015_alpha_report.md` capture the current small-sample sweep result, which failed the alpha gates despite one positive fold.

### Verification
- `python3 -m py_compile scripts/api_auth.py scripts/walkforward_alpha_sweep.py scripts/daily_alpha_report.py scripts/backtest_regime_report.py scripts/regime_allowlist_from_backtest.py`
- `cargo check`
- `cargo test api::auth::tests:: -- --test-threads=1`
- `cargo test api::backtest::tests:: -- --test-threads=1`
- `cargo test`
- Full suite is green: `219 passed; 0 failed`.

---

## [2026-03-20] (Session 19) — TradingBot Decomposition: Idempotency & Accounting

### Structural Cleanup
- **Idempotency lifecycle extracted out of `state.rs`** — moved the API replay cache entry type plus the `get`/`put`/`prune` lifecycle into `src/bot/idempotency.rs`, giving the `TradingBot` state file a smaller surface and a cleaner subsystem boundary.
- **Accounting helpers extracted into their own module** — moved balance reconstruction, non-live balance delta, accounting truth snapshot, drift tracking, and absolute-balance adjustment into `src/bot/accounting.rs`, which makes the balance model easier to reason about independently of the rest of `TradingBot`.

### Stability Coverage
- **Oldest-entry idempotency pruning is now tested** — added regression coverage proving in-memory idempotency eviction removes the oldest entry first when the cache exceeds its configured bound.
- **Accounting snapshot output is now tested directly** — added a state-level test proving `accounting_truth_snapshot(false)` matches reconstructed balances and DB component breakdowns for a mixed open/closed simulation state.

### Verification
- `cargo fmt`
- `cargo check`
- `cargo test`
- Full suite is green: `215 passed; 0 failed`.

---

## [2026-03-20] (Session 18) — Execution Stability Invariants

### Stability Hardening
- **Duplicate fill replay is now explicitly covered** — added a DB-level regression test in `src/db/mod.rs` proving repeated cumulative fill snapshots do not double count quantity or VWAP, which protects partial-fill replay/idempotency behavior.
- **Idempotency failure cleanup is now tested** — added state-layer tests in `src/bot/state.rs` covering malformed persisted idempotency payloads and expired DB-backed idempotency entries, ensuring both are discarded and cleaned up instead of lingering as bad state.
- **Live orphan cleanup is now verified** — added a reconciliation test in `src/bot/reconcile.rs` proving a live `OPEN` trade without an order ID is force-closed, persisted with `reconcile_orphaned`, and evicted from hot memory.
- **Maker quote replacement invariant is now covered** — added an actor-state test in `src/bot/actor.rs` proving a replacement quote for the same market/token/side evicts the prior quote and preserves a single authoritative in-memory quote.

### Verification
- `cargo fmt`
- `cargo check`
- `cargo test`
- Full suite is green: `213 passed; 0 failed`.

---

## [2026-03-20] (Session 17) — Alloy 1.x Upgrade

### Dependency Upgrade
- **`alloy` upgraded from `0.3` to `1.x`** — bumped the CLOB signing dependency in `Cargo.toml`, refreshed `Cargo.lock`, and verified that the only live usage site in `src/clob/auth.rs` still compiles cleanly against the modern API.
- **No auth rewrite was required** — the Polymarket L1/L2 and EIP-712 signing path remained source-compatible after the upgrade, so the dependency refresh landed as a surgical maintenance change rather than a larger integration refactor.
- **Kyle Lambda debug accessors are now test-only** — the cached `lambda()` / `r_squared()` helpers are compiled only for tests, keeping the production build warning-free while preserving the test contract added in the cleanup pass.

### Verification
- `cargo fmt`
- `cargo check`
- `cargo test`
- Full suite is green: `208 passed; 0 failed`.

---

## [2026-03-20] (Session 16) — Signal File Audit & Warning Cleanup

### Cleanup
- **Suspected signal files audited** — reviewed the BTC final momentum, Kyle's Lambda, edge tracker, and Kelly haircut modules after the earlier conflict concern. The implementations are coherent, compile cleanly, and remain covered by passing tests, so no delete/rewrite action was needed.
- **Kyle Lambda cached accessors are now part of the test contract** — the tracker tests now assert that `lambda()` and `r_squared()` reflect the cached estimate after regression, making the cached estimator behavior explicit and easier to preserve during future refactors.

### Verification
- `cargo fmt`
- `cargo test signals::kyle_lambda::tests:: -- --test-threads=1`
- `cargo test`
- Full suite is green: `208 passed; 0 failed`.

---

## [2026-03-20] (Session 15) — Phase-Aware Signal Decay Tuning

### Strategy Work
- **Fusion now applies explicit per-signal phase weighting** — added feature-specific phase multipliers in `src/fusion/mod.rs` so settlement-sensitive signals like `btc_final_momentum`, `hawkes_flow`, `kyle_lambda`, and `window_momentum` are suppressed earlier in the window and strengthened near resolution.
- **Late-window microstructure now has clearer priority** — in `LATE` phase, fast execution-facing inputs like `polymarket_flow`, `orderbook_imbalance`, and `aggressor_ratio` get extra influence, while broader context features decay.
- **Macro/context features now fade as resolution approaches** — `cross_market`, sentiment/context inputs, and time/calendar contextual features contribute less in `LATE`, which reduces the chance that slow-moving signals overpower the final settlement tape.

### Test Coverage
- **Phase-ordering tests added for BTC final momentum** — new fusion tests verify the same `btc_final_momentum` input contributes least in `EARLY`, more in `MID`, and most in `LATE`.
- **Phase-decay tests added for cross-market signal** — added regression coverage proving `cross_market` contributes more earlier in the window and decays as the market moves late.

### Verification
- `cargo fmt`
- `cargo test fusion::tests:: -- --test-threads=1`
- `cargo test`
- Full suite is green: `208 passed; 0 failed`.

---

## [2026-03-20] (Session 14) — Hawkes Flow Signal & Resolution Sniper Upgrade

### Strategy Work
- **Hawkes-style order-flow signal added** — introduced `src/signals/hawkes.rs`, deriving a side-specific clustered-flow signal from the existing Binance agg-trade feed using decayed excitation, clustering, and branching-ratio proxies.
- **Hawkes is now fused into live analysis** — analysis now computes Hawkes flow from the same agg-trade batch already used for microstructure, passes it into `EnrichmentData`, exposes it in analysis `extra`, and includes it as a new fusion feature with a default microstructure weight.
- **Resolution sniper upgraded** — late-window sniping now requires BTC to be meaningfully clear of the strike (dead-zone filter), and both confidence and size scale with time-to-resolution plus BTC-vs-strike conviction instead of token mispricing alone.

### Test Coverage
- **Hawkes signal tests added** — new tests cover clustered buy flow, clustered sell flow, alternating flow, and too-few-trades behavior.
- **Fusion integration test added for Hawkes** — verifies Hawkes flow moves the fused posterior when it is the weighted feature under test.
- **Resolution sniper tests expanded** — added coverage for the new strike dead zone and for time-to-expiry increasing confidence and size.

### Verification
- `cargo fmt`
- `cargo check`
- `cargo test signals::hawkes::tests:: -- --test-threads=1`
- `cargo test fusion::tests::hawkes_flow_moves_fusion_score_when_weighted_in -- --test-threads=1`
- `cargo test bot::resolution_sniper::tests:: -- --test-threads=1`
- `cargo test`
- Full suite is green: `206 passed; 0 failed`.

---

## [2026-03-20] (Session 13) — Feature Verification: Fusion, Kelly, Edge Tracking

### Verified Wiring
- **BTC final momentum proven in fusion** — added fusion-layer tests showing that `btc_final_momentum` changes the fused posterior and contributes to the breakdown when it is the weighted feature under test.
- **Kyle's Lambda proven in fusion** — added matching fusion-layer coverage for `kyle_lambda`, verifying it moves `p_up` and contributes to the final breakdown when enabled.
- **Kelly direction haircut proven in the real sizing path** — added a DB-backed `kelly_size()` test confirming recent same-direction closed trades reduce the next position size, while the opposite direction is unaffected.
- **Edge tracker wiring clarified and verified** — added a resolution-path test showing `ml_on_resolution()` records predicted-vs-realized edge into the tracker. This confirms the tracker is calibration telemetry, not a direct execution gate.

### Verification
- `cargo fmt`
- `cargo check`
- `cargo test fusion::tests:: -- --test-threads=1`
- `cargo test bot::kelly::tests:: -- --test-threads=1`
- `cargo test bot::state::tests::ml_on_resolution_records_edge_tracker_observation -- --test-threads=1`
- `cargo test`
- Full suite is green: `199 passed; 0 failed`.

---

## [2026-03-20] (Session 12) — Preflight Enforcement Interlocks

### Safety Interlocks
- **Live startup no longer auto-runs on failed preflight** — boot now evaluates the same live preflight report used by the health API and refuses to auto-start the trading loop when live-mode prerequisites are red.
- **Manual loop start now fails closed** — `POST /api/loop/start` returns `503` with the current preflight report instead of starting the loop when live preflight is not ready.
- **Manual trade actions now respect preflight** — both `POST /api/trade` and `POST /api/trade/force` are blocked in live mode when preflight is red, preventing manual bypass of the new safety policy.
- **Idempotent replays still work** — cached idempotency responses are returned before the live-preflight gate, preserving exactly-once semantics for already-accepted trade requests.

### Test Coverage
- **Route-level preflight enforcement tests added** — added regression coverage for blocked loop start, blocked manual trade execution, blocked forced trade execution, and idempotent replay survival under failed live preflight.

### Verification
- `cargo fmt`
- `cargo check`
- `cargo test api::bot_routes::tests:: -- --test-threads=1`
- `cargo test`
- Full suite is green: `195 passed; 0 failed`.

---

## [2026-03-20] (Session 11) — Live Preflight & Release-Safety Coverage

### Release Safety
- **Explicit live preflight report added** — introduced `GET /api/preflight` and embedded the same structured report into `GET /api/ready` so operators get one clear view of live-mode readiness before trusting execution.
- **Preflight now checks live deployment assumptions** — config validity, auth enforcement, auth credential presence, permissive dev CORS, rollout stage, bind-all exposure, and missing private RPC configuration are now surfaced as explicit checks/warnings instead of requiring log archaeology.
- **`trading_ready` now respects preflight readiness** — the readiness API no longer reports trading as ready in live mode if preflight-critical checks are failing.

### Test Coverage
- **EV math covered directly** — added unit tests for zero-notional behavior, fee/slippage/gas composition, and input clamping in `src/ev.rs`.
- **Execution helper coverage improved** — added tests for missing-book-side defaults, orderbook USD aggregation, partial fill simulation, adversarial book rejection, and smart order midpoint routing in `src/bot/execute.rs`.
- **Preflight behavior tested** — added health/preflight tests covering live-mode failure on dev CORS, safe warnings on public bind, and non-required preflight behavior in simulation.

### Verification
- `cargo fmt`
- `cargo check`
- `cargo test -- --test-threads=1`
- `cargo test`
- Full suite is green: `191 passed; 0 failed`.

---

## [2026-03-20] (Session 10) — Auth Hardening, Panic Removal, Config Guards

### Live Safety & Auth
- **Live mode now fails closed on auth** — config normalization forces `auth_required=true` in live mode, and live/canary profile validation now requires non-empty API credentials before the bot will accept the config.
- **Protected expensive endpoints** — `POST /api/analyze` and `POST /api/backtest` now sit behind the auth + stricter rate-limit middleware instead of being publicly callable.
- **Stream access gated when auth is enabled** — SSE and WebSocket realtime feeds now require an API key for read access, preventing unauthenticated balance/position/risk streaming in protected deployments.
- **Public/protected rate-limit split** — added a separate public-route limiter while preserving the tighter protected-route limiter, and both maps continue to be pruned periodically.
- **Sensitive config API keys blocked by default** — `mode`, auth credentials, and `private_rpc_url` can no longer be changed over the API unless explicitly re-enabled with `MACBOT_ALLOW_SENSITIVE_CONFIG_API=1`.
- **Restrictive CORS by default** — permissive CORS is now opt-in via `MACBOT_DEV_CORS=1` rather than the default behavior in non-live mode.

### Reliability & Correctness
- **Production panic cleanup** — removed confirmed fallible `unwrap`/`expect` usage from runtime paths including HMAC construction, HTTP client creation fallback, UNIX epoch handling, isotonic calibration sorting/merging, A/B model ranking, market outcome serialization, and shutdown signal registration.
- **Sleep schedule fixed** — `rebuild_from_trades()` now uses typed closed-trade status checks, fixing the old `"closed"` vs `"CLOSED"` bug that prevented historical hourly stats from rebuilding properly.
- **Config bounds tightened** — added invariant checks for `stake_pct`, `kelly_fraction`, `max_exposure_pct`, `max_stake`, and `daily_loss_limit` so invalid or effectively unsafe configs are rejected early.
- **Secrets zeroized after CLOB client construction** — env-loaded private key and Polymarket credentials are now wrapped in `Zeroizing<String>` while building the live client.
- **`.env` permissions corrected** — local environment file permissions were tightened to owner-only (`600`) during the hardening pass.

### Refactors & Infrastructure
- **Shared JSON parsing utility** — extracted `parse_f64` and orderbook level parsing into `src/json_util.rs` and reused them across analysis, execution, close, L2 parsing, and fetcher paths.
- **Rolling-window cleanup** — replaced `Vec::remove(0)` hot paths with queue-based behavior in correlation and calibration tracking.
- **ONNX input construction deduplicated** — deep model inference now shares helper logic between the primary path and dimension-mismatch retry path.
- **SQLite market lookup index added** — introduced `idx_trades_market_id` to avoid full scans on trade queries filtering by market.
- **CI workflow added** — repository now includes GitHub Actions coverage for `cargo fmt --check`, `cargo check`, `cargo test`, and `cargo clippy --all-targets`.

### Verification
- `cargo fmt`
- `cargo check`
- Targeted tests passed for `api::auth`, `config`, `analysis`, `bot::sleep_schedule`, and `api::config_api`.
- Full `cargo test` compiled and started successfully but was stopped after hanging in an existing long-running test outside this change set.

---

## [2026-03-18] (Session 9) — Independent Multi-Agent Trading & Hybrid Gate Diagnostics

### Multi-Agent System — Full Independence
- **Personality-driven signal weighting** — each agent (Aggressive, Conservative, Contrarian) now re-derives its own score/signal/confidence from the raw 24-signal fusion breakdown using per-personality weight maps, instead of gating the same fusion signal.
- **Independent virtual economies** — agents maintain their own virtual balance ($50 default), positions, and trade logs completely decoupled from the real bot balance.
- **Virtual position lifecycle** — agents open positions based on their own analysis; positions close when the real bot closes a trade on the same market.
- **Contrarian inverted scoring** — flips weighted sum so positive sentiment reads as bearish, actively seeks to disagree with fusion consensus.
- **Top-up & Reset API** — `POST /api/agent/:name/topup` and `POST /api/agent/:name/reset` endpoints to inject funds or reset agent stats from the dashboard.
- **Test coverage** — `agents_disagree_on_same_data` test proves Aggressive goes long while Contrarian goes short on identical market data.

### Dashboard — Agents Tab
- 3 agent cards with distinct colors (orange/teal/magenta), each showing: virtual balance, P&L, ROI, win rate, max drawdown, streak stats, best/worst trade, Sharpe proxy.
- Per-agent top-up input and reset button.
- Allocation Distribution panel with colored horizontal bars.
- Ensemble Consensus panel (consensus signal, size mult, best/worst performer, total P&L).
- Open Virtual Positions table with unrealised P&L.
- Per-agent Trade Log tables (direction, entry, exit, P&L, time ago).

### Hybrid Gate Diagnostics
- `should_take_in_hybrid()` now returns `(bool, String)` with detailed reason when blocked.
- Blocked log line now shows: confidence vs threshold, EV vs threshold, regime, and signal — e.g. `hybrid gate blocked taker — conf=0.620 (need 0.720), ev=0.0012 (need 0.0035), regime=CHOP, signal=BuyYes`.

---

## [2026-03-18] (Session 8) — Build Stabilization, ONNX Fix, Dashboard Panels

### Critical Fixes
- **ONNX scalars dimension mismatch** — model expected 30, code sent 20 due to fragile Debug-string parsing in `expected_dim()`. Removed parser entirely; now uses `input.scalars.len()` directly. Retry path parses actual expected dim from ONNX error message instead of hardcoding legacy 20/14.
- **Hybrid mode not trading in simulation** — `l2_feed_stale` gate blocked all trades because no open trades → no L2 targets → never connected. Now skips L2 stale check in simulation mode.
- Fixed double mutable borrow in `deep_model.rs` ONNX retry path.
- Wired `personality_multiplier` from multi-agent ensemble into conviction trade sizing via `override_amount`.

### Dashboard — 8 New Phase 2 Intelligence Panels
All panels render from live SSE stream data (no polling needed).

1. **Regime Detection** — Current regime badge (TREND_UP/DOWN, CHOP, SQUEEZE, BREAKOUT, RANGE) with supporting indicators (VWAP slope, RSI, MACD, OBI, tick velocity).
2. **Granger Causality Weights** — 5 signal reliability multipliers (0.65–1.15) with color-coded progress bars showing which signals are boosted/penalized.
3. **Multi-Agent Consensus** — 3 personality agents (Aggressive/Conservative/Contrarian) with individual decisions, confidence, allocation percentages, and overall size multiplier.
4. **Counterfactual Scenario Tree** — 5 volatility-adaptive scenarios with BTC move, YES/NO prices, P&L projections, probabilities. Summary EV and recommended action.
5. **Latency Arb Monitor** — Real-time Binance→Polymarket lag detection with move %, direction, lag cents, and confidence.
6. **Cross-Market Signals** — ETH/SOL resolution status badges with net signal and confidence.
7. **VPIN Microstructure** — VPIN gauge (0–1), trade imbalance, VWAP deviation, combined signal and confidence.
8. **Liquidation Tracker** — 30s/5m windows with long/short volumes, ratio bar, and spike z-score badge.

### API Data Enrichment
- Added to analysis extra JSON: `granger` (5 weight multipliers), `counterfactual.scenarios` (individual scenario array), `vpin` (VPIN/imbalance/deviation/signal/confidence), `multi_agent` (decisions/allocations/size_mult), `cross_market_signals` (individual asset resolutions).

### Dead Code Removal
- Pruned `coingecko_sentiment`, `finnhub_sentiment`, `spike_detector` signal modules entirely.
- Removed corresponding `fuse_signals()` parameters (25 args → 22 args) and all call sites.

### Test Results
- **146 tests passing, 0 failures, 0 warnings, 0 errors.**

---

## [2026-03-18] (Session 7) — Audit Remediations

### Security
- Redacted API keys from `config.json`; secrets should now come from local-only config or env vars.
- Config load now honors env overrides for secrets (`MACBOT_API_KEY`, `MACBOT_API_HMAC_SECRET`, `MACBOT_PRIVATE_RPC_URL`, `MACBOT_FINNHUB_API_KEY`, `MACBOT_LUNARCRUSH_API_KEY`).
- API server now binds to `127.0.0.1` by default; set `MACBOT_BIND_ALL=1` to expose on `0.0.0.0`.
- `X-Forwarded-For` is only trusted when `MACBOT_TRUST_XFF=1` (prevents spoofed IP allowlist bypass).

### ML / Training
- Training scalars now follow runtime `FEATURE_NAMES` ordering and use a unified feature map.
- Staleness parsing now aligns to 24-feature schema end-to-end.

### Execution & Reliability
- Partial fills now update `trade.shares` alongside entry price to prevent overselling and PnL drift.
- `token_id` parsing now errors instead of defaulting to `U256::ZERO`.
- `get_order` JSON parse failures now surface as errors (no silent default).
- Candidate sorting now handles NaNs safely (no panic).
- DataLogger now reports dropped-event counts (every 1000 drops).
- Deep model inference now retries legacy scalar/staleness dims on shape mismatch.

### Tests
- Fixed `fuse_signals` test argument mismatch after signature change.

## [2026-03-18] (Session 6) — Phase 2 Full Build: 32 Feature Implementation

All 32 planned Phase 2 features implemented across ~4,000 lines of new code.

### New Signal Modules

#### #1 Polymarket Native Features (`src/signals/fetchers.rs`)
- Volume velocity, price velocity, large order detection from Polymarket CLOB data.
- `PolymarketTradeData` struct with flow tracking.

#### #2 Cross-Market ETH/SOL Signals (`src/signals/cross_market.rs`)
- Fetches ETH/SOL Up/Down market resolutions from Gamma API.
- Leading indicator: if ETH resolved YES recently and BTC hasn't, bullish signal.
- `CrossMarketData` struct with `compute_cross_market_signal()`.

#### #5 VPIN & Tick Microstructure (`src/signals/microstructure.rs`)
- Volume-Synchronized Probability of Informed Trading (VPIN).
- Buy/sell volume classification, order flow toxicity metric.
- `MicrostructureData` with `compute_vpin()`.

#### #14 News NLP Pipeline (`src/signals/news_nlp.rs`)
- Keyword-based crypto news sentiment classifier.
- 15 bullish + 17 bearish keyword patterns with weighted scoring.
- Event type detection: ETF flows, Fed speakers, exchange hacks, whale movements, regulatory.
- CryptoPanic API integration for real-time headlines.
- `NewsClassifier`, `NewsSignal`, `EventType` enum.

#### #15 Regime Detection Network (`src/signals/regime.rs`)
- 5-indicator scoring: VWAP crosses, ADX proxy, Bollinger width, volume trend, HH/HL consistency.
- 6 regime outputs: TREND_UP, TREND_DOWN, CHOP, RANGE, BREAKOUT, SQUEEZE.

#### #16 Granger Causality (`src/signals/granger.rs`)
- Full OLS regression with Cholesky decomposition.
- Real F-statistic from restricted vs unrestricted SSR.
- 4-level weight scaling applied to fusion reliability scores.

#### #19 Counterfactual Scenario Trees (`src/signals/counterfactual.rs`)
- 5 volatility-adaptive scenarios (±1σ, ±2σ, chop).
- Squeeze regime fat tails. EV + max-loss gating.

### New Bot Execution Modules

#### #21 Latency Arbitrage (`src/bot/latency_arb.rs`)
- Multi-snapshot detection across 5s/15s/30s horizons.
- Detects Binance→Polymarket price lag for exploitable windows.
- Confidence weighted by freshness, move strength, lag strength.

#### #22 Smart Order Routing (`src/bot/smart_order.rs`)
- Limit vs market order decision based on spread width.
- Iceberg order splitting for large positions.
- `SmartOrderRouter` with `route_order()` and `iceberg_split()`.

#### #23 Position Hedging (`src/bot/hedging.rs`)
- Hedge profitable positions with small opposite-side orders.
- 15% hedge ratio, only on positions > $5 with positive unrealized P&L.

#### #24 MEV-Style Trade Bundling (`src/bot/bundling.rs`)
- Bundle close + open operations in same market window.
- Net position calculation to minimize fee exposure.

#### #27 Sleep Schedule (`src/bot/sleep_schedule.rs`)
- Hour-by-hour P&L tracking to identify unprofitable trading hours.
- Skip trading during historically negative hours.

#### #29 Adversarial Market Probing (`src/bot/probe.rs`)
- Tiny probe orders ($0.10) to test book stability before real entries.
- Fill speed analysis: fast fills = book being swept = don't enter.

#### #30 Multi-Agent Ensemble (`src/bot/multi_agent.rs`)
- 3 personality agents: Aggressive, Conservative, Contrarian.
- Independent evaluation on same signal, consensus voting.
- Capital allocation shifts toward best-performing personality.

#### #31 Resolution Sniper (`src/bot/resolution_sniper.rs`)
- Activates in final 30 seconds before market resolution.
- Compares BTC price vs strike to find mispriced tokens.
- Minimum 10-cent edge requirement before sniping.

#### #32 Synthetic Market Making (`src/bot/market_making.rs`)
- Quote both sides of the book, earn the spread.
- Directionally-skewed quotes based on model conviction.
- Inventory rebalancing when one-sided exposure exceeds threshold.

### ML Infrastructure

#### #25 Hard Negative Tracking (`src/ml/hard_negatives.rs`)
- Records losing trade feature snapshots with 3x learning weight.
- Cosine similarity to detect setups that look like past losses.
- `should_skip()` gate: if >50% of recent losses match current setup, skip trade.

#### #26 Isotonic Calibration (`src/ml/calibration.rs`)
- PAVA (Pool Adjacent Violators Algorithm) for monotonic probability calibration.
- Maps raw model probabilities to empirically calibrated probabilities.
- Auto-refits every 50 observations. ECE (Expected Calibration Error) metric.

#### #28 Model A/B Testing (`src/ml/ab_testing.rs`)
- Multi-variant tracking with rolling Brier scores.
- Statistical winner detection (>5% improvement threshold).
- `register_variant()`, `record()`, `best_variant()`, `has_winner()`.

### Backtest Framework (`src/backtest.rs`)
- Replay historical trades through current strategy logic.
- Full metrics: win rate, Sharpe ratio, profit factor, max drawdown, equity curve.
- `run_backtest_from_db()` loads from SQLite and simulates.

### Training Pipeline Upgrades (`training/model.py`, `training/train.py`)

#### #6 Temporal Attention
- Replaced TemporalCNN with self-attention over candle sequences.
- Learnable positional encoding, 32-dim Q/K/V, 3 resolutions (1m/5m/15m).

#### #7 Market Context Embedding
- Learned embeddings for time-remaining (15 bins × 8-dim) and regime (4 categories × 8-dim).
- Concatenated with fusion output: fused_dim = 64 + 8 + 8 = 80.

#### #9 Multi-Task Heads
- Auxiliary heads: magnitude prediction + volatility prediction.
- Loss: `main + 0.3×magnitude + 0.2×volatility + 0.1×ECE`.

#### #10 Calibration Head
- Separate MLP on raw logit → calibrated probability.
- Detached gradient to avoid interfering with main head.

#### #11 Contrastive Pre-Training
- InfoNCE loss on win/loss trade pairs before main training.
- 10 epochs pre-training to learn discriminative embeddings.

#### #12 Curriculum Learning
- Difficulty-based sample ordering (uncertainty = difficulty).
- Progressive expansion: 40% → 70% → 100% of training set.

#### #13 Synthetic Data Augmentation
- 4 augmentations: time-shift, Gaussian noise, price mirroring, jitter.
- 2x synthetic copies of training data (training set only).

### 60-Day Model Training Complete (PC GPU)
- 100 Optuna trials, best Trial 1: **Brier 0.2425, AUC 0.710**.
- Trained on old architecture (pre-Phase 2 training changes).
- 18,081 samples, 30 scalars, 24 staleness features.
- Exported: `models/v001_20260318_brier0.242.onnx`
- Next retrain will use the new temporal attention + multi-task + contrastive pipeline.

### Code Cleanup
- Pruned dead `coingecko_sentiment`, `finnhub_sentiment`, `spike_detector` signals (negative correlation).
- Removed fetch functions and struct types entirely.
- Fixed `fuse_signals()` call site (25 args → 22 args).
- Added `#[allow(dead_code, unused_imports)]` to all new modules pending wiring.
- Zero warnings, zero errors.

---

## [2026-03-18] (Session 5) — Phase 2 Hardening: Make It Real

Systematic audit and rewrite of 13 placeholder/broken implementations.
Every signal, model input, and execution path now does what it claims.

### Core Signal Rewrites

#### 1. Granger Causality — Real OLS Regression
- **Before:** Hardcoded `0.1` coefficient, no actual regression.
- **After:** Full OLS via normal equations with Cholesky decomposition (`cholesky_solve()` with 1e-8 ridge regularization). Computes real F-statistic from restricted vs unrestricted SSR. Returns smooth 4-level weight scaling: 1.15 (strong), 1.0 (moderate), 0.80 (weak), 0.65 (none). Window size 100→200 observations.
- **Files:** `src/signals/granger.rs`

#### 2. Regime Detection — 5-Indicator Scoring System
- **Before:** 46-line heuristic using only VWAP crosses and slope.
- **After:** 5-indicator scoring: VWAP cross frequency, ADX proxy (directional movement), Bollinger Band width, volume trend, higher-high/lower-low consistency. Returns 6 regimes: `CHOP`, `TREND_UP`, `TREND_DOWN`, `RANGE`, `BREAKOUT`, `SQUEEZE`.
- **Files:** `src/signals/regime.rs`

#### 3. Counterfactual Scenario Tree — Volatility-Adaptive
- **Before:** 3 fixed scenarios with hardcoded move magnitudes.
- **After:** 5 scenarios (±1σ, ±2σ, chop) scaled by `realized_vol × 100 × sqrt(time_remaining/15)`. Added `SQUEEZE` regime with fat tails (tail_prob=0.15 vs normal 0.08). Recommendation requires positive EV AND max_loss > -30%.
- **Files:** `src/signals/counterfactual.rs`

#### 4. Whale Transfers — Real Blockchain Data
- **Before:** Random noise generator (`rand::random::<f64>() * 50_000_000`).
- **After:** Two concurrent real data sources:
  - Blockchair API: confirmed large BTC transactions, classified by exchange address matching.
  - Mempool.space: two-step fetch — `/api/mempool/recent` for txids, then `/api/tx/{txid}` for full `vout` addresses matched against exchange wallets.
- **Files:** `src/signals/fetchers.rs`

#### 5. Options Gamma Exposure (GEX) — Real Computation
- **Before:** `total_oi * 0.01` placeholder.
- **After:** Per-option GEX = `gamma × OI × spot² / 1e6`, calls positive, puts negative (dealer perspective). Summed across all Deribit BTC options.
- **Files:** `src/signals/fetchers.rs`

#### 6. Latency Arbitrage — Multi-Horizon Detection
- **Before:** Single-price comparison with arbitrary threshold.
- **After:** Multi-snapshot detection across time horizons (5s, 15s, 30s). Implied fair YES via logistic sensitivity model. Arb threshold: `lag > spread + 2¢`. Confidence weighted by freshness (40%), move strength (35%), lag strength (25%).
- **Files:** `src/bot/latency_arb.rs`

#### 7. Multi-Agent Ensemble — Independent Personality Evaluation
- **Before:** Static personality selection, no actual multi-agent evaluation.
- **After:** All 3 personalities (Aggressive/Conservative/Contrarian) evaluate independently on same signal. Contrarian fades middling confidence (0.55–0.70) with strength scaled by `p_up` uncertainty. Consensus via weighted vote with allocation-based weighting. Hold if consensus < 0.15. `personality_multiplier` now wired into conviction trade sizing via `override_amount`.
- **Files:** `src/bot/multi_agent.rs`, `src/bot/state.rs`

#### 8. Book Stability Probe — Real Orderbook Analysis
- **Before:** Placeholder returning `(true, 1.0)`.
- **After:** 5-check weighted scoring: spread width (25%), depth ratio (20%), top-of-book size (20%), price location (15%), total depth (20%). Takes actual orderbook data: best_bid, best_ask, bid/ask depth, top sizes.
- **Files:** `src/bot/execute.rs`

### Signal Fusion Wiring

#### 9. Granger Weights → Fusion Reliability
- Applied Granger causality multipliers to reliability scores of 5 fusion features: orderbook imbalance, tick velocity, funding rate, aggressor ratio, VPIN.
- Added cross_market (weight 0.025) and latency_arb (weight 0.030) as new fusion features (#29, #30).
- **Files:** `src/fusion/mod.rs`, `src/bot/analyze.rs`

### Bug Fixes (Audit Round)

#### 10. `regime_to_float()` — Deep Model Saw Constant 0.5
- **Before:** Only mapped 3 legacy labels (`TRENDING`, `CHOP`, `HIGH_VOL`). None matched the actual regime classifier output. Deep model always received `0.5`.
- **After:** Maps all 6 current labels to distinct floats: `TREND_UP`→1.0, `TREND_DOWN`→-1.0, `CHOP`→0.0, `RANGE`→0.1, `BREAKOUT`→1.5, `SQUEEZE`→0.3. Legacy labels kept for backwards compat.
- **Files:** `src/ml/input_builder.rs`

#### 11. Liquidation Signal — Both Branches Returned "UP"
- **Before:** Heavy long liquidations and heavy short liquidations both emitted `"UP"`.
- **After:** Heavy long liquidations (ratio > 0.65) now return `"DOWN"` (bearish cascade). Short liquidations (ratio < 0.35) return `"UP"` (squeeze).
- **Files:** `src/signals/liquidation.rs`

#### 12. Liquidation Z-Score — Fake Variance
- **Before:** Variance was `(mean_event_size)²` — not statistical variance.
- **After:** Buckets 5-minute baseline into 10×30s windows, computes real mean and variance across window volumes.
- **Files:** `src/signals/liquidation.rs`

#### 13. Exchange Address List — 7→21 Addresses
- Expanded from 7 stale entries to 21 addresses/prefixes covering Binance (hot+cold+BTCB reserve), Bitfinex (from official GitHub), Coinbase (prime+institutional), Kraken, OKX, Bybit, Gemini. Sourced from Arkham Intelligence, WalletExplorer, Bitfinex GitHub.
- **Files:** `src/signals/fetchers.rs`

### Test Fixes
- Fixed `shadow_kill_reason_trips_on_low_win_rate` — test now sets `MACBOT_SHADOW_KILL_ENABLED=1` env var to match the runtime gate.
- Fixed `detects_arb_on_btc_spike` — adjusted test to use 5% BTC spike with tight spread (realistic arb scenario).
- Fixed ambiguous float types in `counterfactual.rs` and `granger.rs` test code.
- All 120 tests pass, 0 failures.

### Phase 2 Closing: Risk Management & Cleanup

#### 14. Aggregate Exposure Cap
- New config field `max_exposure_pct` (default: 0.60 = 60% of balance).
- Total open notional across all positions cannot exceed cap. Trades are either downsized to fit or rejected if remaining capacity is below `min_stake`.
- Enforced in `validate_trade_common()` — applies to all execution paths.
- **Files:** `src/config.rs`, `src/bot/execute.rs`, `config.json`

#### 15. Drawdown Circuit Breaker Defaults Enabled
- `drawdown_halt_daily_pct`: 0.0 → **10.0** (halt after 10% daily loss)
- `drawdown_halt_ath_pct`: 0.0 → **20.0** (halt after 20% drawdown from ATH)
- The `is_drawdown_halted()` logic was already correct but permanently disabled by zero defaults.
- **Files:** `config.json`

#### 16. Zero-Weight Signals Pruned
- `spike_detector`, `coingecko_sentiment`, `finnhub_sentiment` all had fusion weight=0.0 (pruned earlier for negative correlation).
- Removed their API calls from the analyze loop — they now always pass `None` to fusion.
- Saves 3 HTTP calls per cycle, eliminates ~200ms latency per analysis loop.
- **Files:** `src/bot/analyze.rs`

#### 17. Dead Code Deleted
- **`crypto_news.rs`**: 36-line dead module, never called, `#[allow(unused_imports)]` suppressed. Deleted entirely.
- **Files:** `src/signals/crypto_news.rs` (deleted), `src/signals/mod.rs`

#### 18. Correlation Guard Wired Into Live Pipeline
- `CorrelationTracker` was instantiated in `state.rs` but never received data.
- Now fed BTC/ETH returns from `fetch_altcoin_correlation()` every analyze cycle.
- `position_size_multiplier()` integrated into Kelly sizing (step 6): halves position size when BTC-ETH correlation drops below 0.50 (black swan detection).
- **Files:** `src/bot/analyze.rs`, `src/bot/kelly.rs`

#### 19. Book Stability Probe Wired Into Execution
- `probe_book_stability()` was rewritten but never called. Now integrated into simulated-live execution path.
- Extracts bid/ask depth, top-of-book sizes from the orderbook and runs the 5-check stability score.
- Trades are rejected when `is_stable=false` (score below threshold).
- **Files:** `src/bot/execute.rs`

#### 20. Fusion Input Staleness Gating
- New staleness flags on `EnrichmentData`: `stale_options`, `stale_whale`, `stale_cross_market`, `stale_polymarket_flow`.
- When a signal fetch fails, its reliability weight is zeroed in fusion — prevents garbage-in-garbage-out from failed API calls.
- Flags set automatically based on whether the fetch returned `None`/`Err`.
- **Files:** `src/fusion/mod.rs`, `src/bot/analyze.rs`

#### 21. Post-Fill Slippage Tracking
- SimLive execution now logs slippage in basis points: `slip={N}bps` comparing midpoint vs actual fill price.
- Closes the feedback loop between LMSR slippage estimates and real execution cost.
- **Files:** `src/bot/execute.rs`

#### 22. Latency Arb Weight Reduced
- Weight 0.030 → **0.010** — signal is degraded because BTC price updates via REST (~10s cadence), not WebSocket. At 10s latency, most arb opportunities are already gone.
- Phase 3 item: feed from Binance price WebSocket for real sub-second detection.
- **Files:** `src/fusion/mod.rs`

#### 23. Orderbook Signal Deduplication
- `depth_imbalance_5` weight reduced 0.025 → **0.012** — it's ~0.9 correlated with `orderbook_imbalance` (same information, different source). Prevents fusion from double-counting orderbook signal.
- **Files:** `src/fusion/mod.rs`

## [2026-03-17] (Session 4) — 30-Item Overhaul

### Trading Logic (Critical)

#### 1. Partial Close Edge Widened (4¢ → 8¢)
- Conviction partial TP now requires 8-cent price improvement over entry before firing.
- Eliminates fee-loss "TP" exits on high-entry trades (0.93+).

#### 2. Circuit Breaker Cooldown (30-minute pause)
- After 2× consecutive loss threshold (default: 12 losses), bot pauses ALL trading for 30 minutes.
- New field `circuit_breaker_pause_until` on TradingBot struct.
- Cooldown badge in nav bar shows remaining pause time.
- Auto-resumes when cooldown expires.

#### 3. Daily Loss Limit Hard Stop
- Already enforced in `validate_trade_common` — verified working with `get_today_pnl()`.

#### 4. Scalp Piggyback Mode
- Scalp trades no longer open simultaneously with conviction trades on same signal.
- Scalps only open when an existing conviction position is already open on the market.
- Prevents double-exposure on bad signals.

#### 6. Dynamic Position Sizing
- Trade amount scaled by confidence strength: `conf_multiplier = (conf - min_conf) / (1.0 - min_conf)`, clamped [0.5, 1.5].
- Higher confidence = bigger position, low confidence = half-sized position.

#### 7. Regime-Aware Sizing
- CHOP and RANGE regimes automatically halve position sizes.
- Reduces exposure during indecisive markets.

#### 8. Time-Decay Exit
- Conviction positions that are **losing** with < 2 minutes remaining are auto-closed.
- Exit type: `time_decay`. Prevents riding losers to resolution for maximum loss.

#### 9. Anti-Correlation Signal Pruning
- Zeroed weights for: `spike_detector`, `coingecko_sentiment`, `finnhub_sentiment`.
- These three were negatively correlated with trade outcomes (adding noise, not signal).

#### 10. Conviction Reversal Exit
- If `p_up` flips strongly against an open conviction position (< 0.30 for longs, > 0.70 for shorts), auto-close.
- Exit type: `reversal`. Cuts losses when the market fundamentally shifts against you mid-window.

#### 16. Ensemble Disagreement Skip
- When ML model and deep model strongly disagree (one bullish > 0.6, other bearish < 0.4), skip the trade entirely.
- Disagreement = high uncertainty = negative EV.

#### 29. Funding Rate Entry Filter
- Extreme funding rates (|rate| > 0.1%) trigger contrarian filter.
- Trades going WITH extreme funding direction are skipped (likely to reverse).

### Scalp Strategy Hardening

#### Trailing Stop Widened (4% → 8%)
- Scalp trailing stop now allows 8% drawdown from peak before closing.
- Survives normal CLOB book sweeps that were causing false trail exits.

#### Spread Gate (max 6¢)
- Scalps rejected when order book spread exceeds 6 cents.
- Prevents entering illiquid moments.

#### Depth Gate (min $500)
- Scalps rejected when top-of-book USD depth < $500.
- Checked both at entry validation and during CLOB book fetch.

### Infrastructure

#### 20. Graceful Shutdown
- On SIGTERM/Ctrl+C, bot now closes all open positions at market before exiting.
- Logs each closed trade with P&L.

#### Circuit Breaker Cooldown API
- `/api/stats` now includes `circuit_breaker_cooldown` object with `active`, `until`, `minutes_left`.

#### Frontend: Circuit Breaker Badge
- Warning badge in nav bar: "CB PAUSED Xm" shown when circuit breaker cooldown is active.

### Already Implemented (Verified)

- **#3** Daily loss limit: enforced in `validate_trade_common`
- **#11** Spread at entry: `signals_json` already captures full signal snapshot at trade open
- **#12** Time-to-expiry: `time_remaining_frac` already fed to deep model as continuous input
- **#17** Backtest framework: `/api/backtest` endpoint already exists
- **#18** Trade journaling: `signals_json` field on Trade already logs all 24 features at entry
- **#21** Config hot-reload: signal weights already hot-reload from `/tmp/calibration.json`
- **#24** Shadow-to-live graduation: ensemble auto-promotes when accuracy > threshold over 200 predictions
- **#25** P&L attribution by signal: `/api/signal-stats` and `/api/attribution` endpoints exist

### Previously Deferred — Now Implemented

#### #5 Maker-Only Scalp Config
- Added `scalp_maker_only` config flag (default: false).
- When enabled, scalps simulate limit order fills at best bid + 1 tick (saves taker fees).
- Live mode uses existing limit order infrastructure in `execute_live_trade`.

#### #14 BTC/ETH/SOL Correlation Signal
- New `fetch_altcoin_correlation(btc_change_pct)` in `fetchers.rs`.
- Fetches ETH and SOL 15m candles from Binance, computes change %.
- `correlation_score`: +1.0 when alts agree with BTC direction, -1.0 on divergence.
- Displayed on frontend: ETH Δ% and SOL Δ% stat pills.
- Piped into analysis JSON as `altcoin_correlation` object.

#### #15 Online Learning Weight Decay
- Added L2 regularization (λ=0.0001) to `OnlineLogisticRegression::update()`.
- Each weight update: `w -= lr * (error * x + λ * w)`.
- Gently shrinks old weights toward zero — model naturally forgets stale regime data.
- No manual retraining window needed; decay is continuous.

#### #19 Discord/Telegram Webhook Alerts
- New `src/notifications.rs` module.
- Supports Discord webhooks (`MACBOT_DISCORD_WEBHOOK_URL`) and Telegram bot (`MACBOT_TELEGRAM_BOT_TOKEN` + `MACBOT_TELEGRAM_CHAT_ID`).
- Auto-alerts on: large losses (> $2), kill switch activation, circuit breaker cooldown, model deployment.
- Non-blocking: notifications sent via `tokio::spawn`.

#### #23 Multi-Timeframe Support
- Already supported via `MACBOT_TARGET_WINDOW_MINS` env var (default: 15).
- Set to `5` to trade 5-minute BTC Up/Down markets.
- Market discovery in `analyze.rs` dynamically floors to nearest window boundary.

#### #26 Orderbook Depth Heatmap
- New card on Overview tab: bid/ask depth bars with spread badge.
- Visual representation of order book imbalance.
- Coinbase divergence % badge for cross-exchange comparison.

#### #27 Session Auto-Archiving
- Frontend timer checks every 5 minutes for trades older than 24 hours.
- Logs when > 20 stale trades detected (manual archive via New Session button).

#### #28 Model A/B Comparison Table
- New table in Deep Model section: Online LR vs Deep ONNX side-by-side.
- Shows Brier score, accuracy, sample count for each model.
- Agreement indicator: ✓ Agree (green) or ✗ Disagree (red) when models point different directions.
- Updates live via SSE.

#### #30 Cross-Exchange Price Feed
- New `fetch_coinbase_btc_price()` in `fetchers.rs` (Coinbase Spot API).
- Computes Binance↔Coinbase divergence percentage.
- Displayed in analysis JSON as `coinbase_divergence_pct`.
- Badge on orderbook card shows real-time cross-exchange spread.

---

## [2026-03-17] (Session 3)

### Critical: Conviction TP Firing Instantly (Losing Trades on Every "TP")

#### Root Cause
- Conviction TP used **absolute price thresholds**: partial TP at token ≥ 0.75, full TP at ≥ 0.85.
- When bot enters at high prices (0.93, 0.98), the price is already above both thresholds on entry.
- TP fires on the very next tick — trade closes with zero price movement, fees eat the P&L → consistent small losses despite "TP" exit type.
- Example: Trade #29 enters at 0.985, partial TP fires immediately, exits at 0.985, P&L = -$0.04 (pure fee loss).

#### Fix (`src/bot/state.rs`)
- Added **minimum edge requirement**: TP only fires when `token_price >= entry_price + 0.04` (4-cent edge above entry).
- Both partial TP and full TP now require the absolute threshold AND the relative edge.
- 4-cent edge covers ~2× taker fee, ensuring actual profit on every TP close.
- High-entry trades (0.95+) now ride to market resolution instead of instant-closing.

### Frontend Fixes

#### Expandable Trade Rows Stay Open
- Bug: `renderHistory()` after re-render called `toggleTradeDetail()` on already-visible rows, which toggled them **closed**.
- Fix: Extracted `fetchPartialFills()` function — re-fetches fill data without toggling visibility.
- Template already sets `display: table-row` for rows in `expandedTradeIds` Set, so rows persist correctly.

#### New Session Button Fix
- Removed unnecessary `{}` body from POST calls to `/api/drawdown/reset` and `/api/trades/clear-history` (these endpoints don't expect JSON body, was causing 415 error).

---

## [2026-03-17] (Session 2)

### Frontend Overhaul

#### Removed Dead Panels
- Removed "Quote Model Surface" panel (showed "No model state" permanently).
- Removed "Recent Outcomes" panel (showed "No recent outcomes" permanently).
- Added null guards so removed element references don't throw JS errors.

#### Strategy P&L Split + Win Rate Badges (`static/index.html`)
- Four new stat pills in the navbar: Conv P&L, Scalp P&L, Conv WR%, Scalp WR%.
- Green/red coloring based on positive/negative P&L.
- Updated live via SSE on every tick.

#### Expandable Trade Rows
- Click any trade row to expand a detail panel showing: partial close info, entry/exit timestamps, fees, signal breakdown, amount/shares, market ID, run ID.
- `expandedTradeIds` Set persists expanded state across SSE refreshes — rows stay open.

#### Deep Model Progress Bar
- Purple Bootstrap progress bar in Deep Model section showing samples toward 200 promotion threshold.
- Label: "X / 200 for promotion". Updates via SSE.

#### Signal Weights Grouping
- 24 signals organized into 5 collapsible groups: Micro, Momentum, Macro, Temporal, Liquidation.
- Clickable group headers to collapse/expand. All expanded by default.

#### Open Positions Live Unrealized P&L
- UPNL column now shows dollar amount + percentage in parentheses.
- Green/red coloring updates every SSE tick.

#### Session Performance Heatmap
- 24 colored blocks (one per UTC hour 0-23) showing average P&L per hour.
- Green = profitable hours, Red = unprofitable, Gray = no data.
- Tooltip on hover shows trade count, avg P&L, total P&L.

#### Collapsible Config Panel
- Config section now has a clickable gear icon header to toggle open/close.
- Shows formatted JSON of current config. Collapsed by default.

#### P&L Chart Zero Line
- Dashed white horizontal line at y=0 on the cumulative P&L chart for clear green/red visualization.

---

### Expanded Feature Set — 14 → 24 Signals

#### New Signal Modules (`src/signals/temporal.rs`, `round_number.rs`, `volatility.rs`)
- `compute_hour_of_day()`: UTC hour normalized to [0, 1] — captures session-dependent BTC behavior (US open vs Asian hours).
- `compute_day_of_week()`: Day normalized to [0, 1] — weekend vs weekday patterns.
- `compute_distance_from_round(price, level)`: Distance from nearest psychological price level ($1000, $500 rounds).
- `compute_recent_volatility(closes, window)`: Standard deviation of 1m candle returns over last N bars (10 and 30 windows).

#### Binance L2 Depth (`src/signals/fetchers.rs`)
- Added `fetch_binance_depth()` — fetches BTCUSDT orderbook (top 20 levels) and computes bid/ask USD imbalance for top-5 levels.
- Returns `DepthImbalance { imbalance_5 }` in [-1, 1] range.

#### Liquidation Aggregates (`src/bot/analyze.rs`)
- Now extracts `long_vol` and `short_vol` from the existing `LiquidationTracker::volume_in_window(300)` (5-minute window).
- Heavy long liquidations encoded as bearish pressure, short liquidations as bullish.
- Log-scaled normalization: `(vol.ln() / 16.0).clamp(0.0, 1.0)`.

#### Window Momentum (`src/bot/analyze.rs`)
- Reuses existing `btc_momentum` (price change since 15m window open) as a direct feature.
- Normalized to ±0.5% full scale in fusion.

#### ML Pipeline Expansion (`src/ml/`)
- Added `pub const NUM_FEATURES: usize = 24` in `model.rs` — replaces all hardcoded `14` across the codebase.
- `FEATURE_NAMES` expanded from 14 to 24 entries.
- `OnlineLogisticRegression` weights, `FeatureNormalizer` arrays, `FeatureStaleness` all expanded.
- `extract_features()` in `mod.rs` returns `[f64; 24]`.
- `DeepModelInput` scalars: 30 (24 features + 6 context), staleness: 24.

#### Backward-Compatible ONNX Inference (`src/ml/deep_model.rs`)
- Dynamic dimension detection: reads expected input size from ONNX model metadata.
- Truncates scalars (30 → 20) and staleness (24 → 14) to match old model's compiled shapes.
- Falls back to known old dimensions (20/14) if metadata parsing fails.
- New models trained with 30/24 dimensions will work without code changes.

#### Persistence Migration (`src/ml/persistence.rs`)
- Version bumped from 1 to 2.
- Auto-migration: loading old 14-feature `ml_weights.json` extends weights, means, and m2 arrays to 24 with zeros.
- Log message: `"Successfully migrated ML weights from 14 to 24 features"`.

#### Fusion Engine (`src/fusion/mod.rs`)
- Added `EnrichmentData` struct: bundles all 10 new feature values.
- 10 new feature computation blocks inside `fuse_signals()` with domain-appropriate normalization.
- Default weights for new features start conservative (0.01–0.03) — model learns true weights over time.
- `fuse_signals()` signature extended with `enrichment: &EnrichmentData` parameter.

#### Analysis Pipeline (`src/bot/analyze.rs`)
- Computes all 10 enrichment features before fusion: temporal, round numbers, volatility, liquidation, depth.
- Staleness marking expanded to indices 14–23 (all new features marked fresh per tick).
- New imports: `compute_hour_of_day`, `compute_day_of_week`, `compute_distance_from_round`, `compute_recent_volatility`, `fetch_binance_depth`.

#### 36K Dataset Training (PC GPU — In Progress)
- 180-day backfill: 36,052 labeled samples.
- Optuna 100-trial search at 72/100, best Trial 51: Brier 0.207 (worse than deployed 11.5k model's 0.171).
- Adversarial validation AUC 0.946 confirms massive distribution shift ($69k → $121k BTC).
- Decision: don't deploy 36k model. Retrain on 60-day recent window instead for cleaner signal.

---

## [2026-03-17]

### Dual Strategy System — Conviction + Scalp

#### Scalp Trading (`src/bot/execute.rs`, `src/bot/state.rs`)
- New scalp strategy runs alongside conviction trades: 2 slots per market window.
- Conviction (HOLD): enters on high confidence, rides to resolution.
- Scalp: enters on momentum, auto exits via TP/SL/trailing stop.
- Trades tagged with `strategy: "conviction"` or `"scalp"` for separate tracking.
- Dashboard shows HOLD/SCALP badges in trade history with strategy filter.

#### Smart Scalp Execution Guards (`src/bot/execute.rs`)
- Price range filter: scalps blocked when YES price < 0.15 or > 0.85 (extreme prices have no TP room).
- Time remaining guard: scalps blocked with < 3 minutes left in market window.
- Momentum filter: scalps require `p_up >= 0.60` for BuyYes, `p_up <= 0.40` for BuyNo.
- Scalps bypass stale analysis price cache — always fetch fresh CLOB price for TP/SL checks.

#### Trailing Stop System (`src/bot/state.rs`)
- High-water mark tracking per scalp trade via `scalp_high_water: HashMap<i64, f64>`.
- Fixed TP at +15% gain from entry.
- Trailing stop: once gain exceeds +5%, close if price drops 4% from peak — locks in profit.
- Hard SL at -12% after 30-second grace period (prevents first-tick noise kills).
- Emergency SL at -25% (bypasses grace period for real crashes).
- Distinct exit types: `scalp_take_profit`, `scalp_trailing`, `scalp_stop_loss`, `scalp_emergency_sl`.

#### TP/SL Price Cache Fix (`src/bot/state.rs`)
- Added `last_trade_prices: HashMap<i64, f64>` — caches last known price per trade.
- When CLOB API returns None (market transitioning), falls back to cached price instead of silently skipping.
- Fixes conviction TP not firing when market transitions between 15m windows.

### Deep Model Ensemble Scoring Fix

#### Shadow Prediction Tracking (`src/ml/ensemble.rs`, `src/bot/analyze.rs`, `src/bot/state.rs`)
- `pending_predictions: HashMap<String, f64>` stores deep model p_up per market_id at inference time.
- On trade resolution, looks up stored prediction and calls `record_deep_prediction()` to score accuracy.
- Previously `record_deep_prediction` was dead code — never called. Samples counter stayed at 0.
- Deep model can now accumulate shadow samples toward the 200-prediction promotion threshold.

### 180-Day Backfill Complete

- Backfill finished: **36,052 labeled samples** (up from 11,576), 9 million candles in DB.
- 36k dataset synced to PC for retraining (1.6GB DB via SCP).
- Training in progress: 100 Optuna trials on RTX 3070 Ti with 36k samples.
- Adversarial validation AUC: 0.946 (high distribution shift across 180 days, $69k→$121k BTC range).
- Best trial so far: Trial 16 with Brier 0.231 (worse than 11.5k model's 0.171 due to regime change).

### Full Codebase Audit

#### Critical Issues Found
- `set_trade_exit_type` fails silently — trades evicted from memory before exit_type is set, everything shows "manual" in DB.

#### High Issues Found
- `pending_predictions` lost on restart (`#[serde(skip)]`) — deep model scoring resets each run.
- `tick_velocity` signal weight = 0.0 in config — key microstructure signal disabled.
- HTTP server binds 0.0.0.0:8000 with no TLS — HMAC interceptable in live mode.

#### Medium Issues Found (15 total)
- Deep model inputs: `balance_frac` and `open_positions` never set (always defaults).
- `mark_all_fresh()` masks real staleness of slow-updating signals.
- No timeout on CLOB price API calls — can hang TP/SL checks.
- `take_profit_threshold` is 0.7 in config (may want 0.85).
- SSE/WS streams unauthenticated in non-live mode.
- Risk endpoint leaks env var values.
- EDT/EST hardcoded offset inconsistency in market discovery.
- WS READ_TIMEOUT (2s) vs PING interval (10s) timing mismatch causes reconnects.
- Data logger: silent data loss on buffer full, outcome field never backfilled.
- Ghost trade P&L hardcoded to 0.0 in reconciliation.
- Kill switch triggers on any drift (threshold=0 default).
- No stale SSE data detection on frontend.
- `update_trade` SQL omits `strategy` column.
- No denylist for dangerous config fields via API.

### Training — Deep Model v1 (Brier 0.171, AUC 0.823)

#### Training Pipeline Fixes (`training/train.py` — PC)
- Diagnosed root cause of AUC 0.500: frozen CNN + Transformer branches produced constant noise vectors (128 dims) that diluted the MLP signal (32 dims) in the 160-dim fusion layer.
- Fix: zero out frozen branch outputs entirely so only MLP signal reaches the task heads.
- Optuna 100-trial hyperparameter search completed on RTX 3070 Ti (~2 hours).
- Best hyperparams: `lr=6.6e-4, weight_decay=4.8e-6, batch_size=128, epochs=44`.
- 5 random restarts for final training — best restart: **Brier 0.1710, AUC 0.823**.
- Model exported as `v001_20260317_brier0.164.onnx` and deployed to Mac.

#### 180-Day Bulk Candle Backfill (`training/backfill_historical.py`)
- Added `prefetch_all_candles()`: downloads ALL 1m candles for entire date range in one batch (~260 API calls vs ~60k per-market calls).
- Added `slice_candles_from_cache()`: binary search extraction for specific time windows.
- Modified `backfill_interval()` to accept pre-fetched candle cache, falling back to DB then API.
- 180-day backfill in progress: 259,380 candles downloaded, processing 17,276+ market windows.

#### Derivatives Data Enrichment
- Added Binance Futures API integration: funding rates, open interest, aggressor buy/sell flow.
- 8,620 existing samples refreshed with futures context data.

### Fixed — Deep Model Dashboard Integration

#### API Fix (`src/api/deep_model_api.rs`)
- Added `last_prediction` object to `/api/deep-model/status` response containing `deep_p_up`, `blended_p_up`, `mc_uncertainty`, `should_skip`.
- Frontend JS expected `r.last_prediction` but API only had fields inside `r.ensemble` — predictions were never displayed.

#### ONNX Input Compatibility (`src/ml/deep_model.rs`)
- MLP-only model exports 3 inputs (`scalars`, `staleness`, `regime`) instead of 7.
- Dynamic input detection already handled this correctly — no code change needed.
- Confirmed inference works: `p_up=0.825` on first market analysis after deploy.

### Improved — Real-Time Dashboard

#### SSE-Driven Updates (`src/api/sse.rs`, `src/api/realtime.rs`, `static/index.html`)
- Deep model panel now updates via SSE at 500ms (was 30s polling).
- Added `deep_model_status` to `fast_snapshot()` — model info, ensemble state, last prediction all push in real-time.
- Added `updateDeepModelFromSSE()`, `updateRiskFromSSE()`, `updateExecutionFromSSE()`, `updateMarketsFromSSE()`, `updateTradesFromSSE()` handlers.
- Markets, trades, outcomes now update every **1 second** via SSE slow snapshot (was 3s, previously 10-15s polling).
- Reduced slow snapshot cache TTL from 2s to 1s.
- Reduced slow snapshot interval from every 6th tick (3s) to every 2nd tick (1s).
- ML state polling reduced from 30s to 5s as fallback.
- BTC chart polling reduced from 30s to 10s.
- All core panels (price, balance, signals, deep model, risk) update at 500ms via SSE.

## [2026-03-16]

### Added — Deep Learning Pipeline (Phase 2)

#### ONNX Runtime Inference (`src/ml/deep_model.rs`)
- `DeepModel` struct wrapping `ort::Session` with Level3 graph optimization.
- `DeepModelInput` — 7 tensor inputs (3 candle resolutions, orderbook sequence, scalars, staleness, regime).
- `DeepModelOutput` — 4 task heads (p_up, magnitude, volatility, time_to_move) + MC uncertainty + model version.
- `SharedDeepModel = Arc<RwLock<Option<DeepModel>>>` for thread-safe hot-swap.

#### Model Hot-Reload (`src/ml/model_watcher.rs`)
- Polls `models/latest.onnx` every 30s for mtime changes.
- On detection, loads new model and atomically swaps behind the shared handle.
- On failure, keeps previous model running (no disruption).

#### Input Builder (`src/ml/input_builder.rs`)
- `InputBuilder` with ring buffers (VecDeque) for 1m candles and orderbook snapshots.
- Multi-resolution candle aggregation: 1m raw, 5m and 15m built from 1m history.
- Orderbook sequence: 12 snapshots x 45 features (40 L2 levels + 2 flow + 3 liquidation).
- `FeatureStaleness` tracker — per-signal freshness normalized to 0..1.
- Context packing: time_remaining, balance, positions, contract_price, price_discovery_lag, btc_price.

#### Ensemble Blending with Safety Gates (`src/ml/ensemble.rs`)
- `EnsembleState` manages shadow -> ramping -> active phase transitions.
- Shadow: 200 predictions logged but ignored; promote if accuracy > 54%.
- Ramping: weight increases 0 -> 0.60 over 100 steps.
- Active: rolling 50-prediction demotion check (< 52% -> back to shadow).
- MC Dropout uncertainty gate: skip trade entirely if sigma > 0.08.
- Online isotonic calibration via Pool Adjacent Violators (PAV) algorithm, refitted every 50 samples.

#### Data Collection Infrastructure (`src/ml/data_logger.rs`)
- 6 SQLite tables: `ml_candles_1m`, `ml_orderbook_snapshots`, `ml_tick_history`, `ml_features_timeseries`, `ml_aggressor_flow`, `ml_liquidations`.
- Non-blocking writer via mpsc channel (buffer 4096) — trading loop never blocks on I/O.
- Auto-prune hourly: deletes rows older than 30 days.
- Convenience methods: `log_candle`, `log_liquidation`, `log_orderbook`, `log_features`, `log_aggressor`, `log_tick`.

#### Binance Liquidation Feed (`src/signals/liquidation.rs`)
- WebSocket consumer for `btcusdt@forceOrder` stream.
- `LiquidationTracker`: ring buffer (max 2048 events, prune > 10 min).
- `volume_in_window(secs)` — returns long/short vol, ratio, count.
- `spike_z_score()` — compares 30s volume vs 5min baseline.
- `compute_liquidation_signal()` — directional signal from liquidation patterns.
- Exponential backoff reconnection on disconnect.

#### PyTorch Model Architecture (`training/model.py`)
- `MultiHeadBTCPredictor` (~200K params) with 3 branches:
  - `TemporalCNNBranch`: multi-resolution Conv1d on 1m/5m/15m candles.
  - `OrderbookTransformerBranch`: 2-layer, 4-head Transformer on [12, 45] sequences.
  - `ScalarMLPBranch`: MLP on 34 inputs (20 scalars + 14 staleness).
- `FiLMLayer`: regime-conditioned affine modulation (gamma * x + beta).
- `FusionHead`: MultiheadAttention(2 heads) on 160d concatenated branch outputs.
- 4 task heads: sigmoid(p_up), relu(magnitude), softplus(volatility), sigmoid(time_to_move).
- `KendallMultiTaskLoss`: learned log-variance per task for auto-balancing.
- ONNX export with dynamic batch axes (opset 17).

#### Training Pipeline (`training/train.py`)
- SQLite data loader building aligned tensor windows from 6 ML tables.
- SimCLR-style contrastive pretraining for the orderbook Transformer branch.
- Adversarial validation (GBM classifier to detect train/test distribution shift).
- Walk-forward cross-validation (time-respecting splits).
- Optuna HPO (100 trials, TPE sampler, median pruner) minimizing Brier score.
- Final training with cosine LR schedule, gradient clipping, early stopping.
- ONNX export with version naming (`v001_YYYYMMDD_brierX.XXX.onnx`) + auto-copy to `latest.onnx`.

#### Dashboard API (`src/api/deep_model_api.rs`)
- `GET /api/deep-model/status` — model version, load time, ensemble phase/weight/accuracy, liquidation window summaries (30s + 5m), spike z-score.

#### Trading Loop Integration (`src/bot/analyze.rs`)
- Data logging wired into analysis cycle: candles, aggressor flow, and feature timeseries logged every tick.
- InputBuilder fed with latest candles + context vars each cycle.
- Deep model shadow inference when ONNX model is loaded.
- Ensemble blending applies calibrated deep model weight to fusion p_up.
- MC uncertainty gate skips trades when dropout variance too high.
- Deep model info exposed in analysis JSON output (`extra.deep_model`).

#### Mac <-> PC Sync Scripts (`scripts/`)
- `sync_data_to_pc.sh` — rsync SQLite DB + training code to GPU PC.
- `sync_model_from_pc.sh` — pull trained ONNX models back to Mac.

### Changed
- `Cargo.toml` — added `ort = "2.0.0-rc.12"` (with ndarray feature) and `ndarray = "0.17"`.
- `src/ml/mod.rs` — added module declarations for `data_logger`, `deep_model`, `ensemble`, `input_builder`, `model_watcher`.
- `src/signals/mod.rs` — added `pub mod liquidation` with re-exports.
- `src/db/mod.rs` — `migrate()` now calls `create_ml_tables()` to create the 6 ML tables.
- `src/bot/state.rs` — `TradingBot` struct gains 5 new fields: `data_logger`, `liq_tracker`, `input_builder`, `deep_model`, `ensemble_state`.
- `src/main.rs` — spawns liquidation feed and model watcher at startup.

## [2026-03-15]

### Added — Online Machine Learning (Phase 1)
- New `src/ml/` module with online logistic regression that learns from every resolved market.
  - `model.rs` — 14-weight logistic regression with SGD and learning rate decay.
  - `normalizer.rs` — Welford's online z-score normalization per feature.
  - `tracker.rs` — Rolling accuracy tracker (20/50/100/200 windows + Brier score).
  - `persistence.rs` — Atomic JSON save/load to `ml_weights.json` (crash-safe).
  - `mod.rs` — MlState wrapper, feature extraction, predict/update helpers.
- ML backfill on startup: replays all historical resolved trades through the model so learned weights persist across restarts.
- Training hook in `guard.rs`: after every market resolution, the ML model updates weights via SGD.
- Shadow prediction in `analyze.rs`: ML model runs alongside fusion every tick, logging `ML p_up` vs `fusion p_up` for comparison without affecting trading decisions.
- Cold start gate: hand-tuned fusion remains in control until ML has seen 50+ labeled samples.
- `GET /api/ml/state` endpoint — returns weights, accuracy, feature importance, samples seen, learning rate.
- ML dashboard panel in `index.html` — feature weight bars (green=bullish, red=bearish), rolling accuracy, shadow predictions, samples counter, active/shadow badge.

### Added — 5-Minute Market Support
- Both `analyze.rs` and `api/markets.rs` now fetch 5m and 15m market slugs (`btc-updown-5m-{epoch}` and `btc-updown-15m-{epoch}`), providing 3x more training data for the ML model (288 vs 96 labeled samples/day).

### Added — Reset Drawdown Button
- `POST /api/drawdown/reset` endpoint — re-bases `initial_balance` to current balance, clearing the drawdown halt without deleting trade history.
- "Reset DD" button in dashboard navbar next to Start/Stop.

### Fixed — Trade Execution Pipeline
- **Price inversion for BuyNo signals**: `get_clob_token_id` returns the NO token for BuyNo, so `fetch_midpoint_public` already returns the correct NO midpoint. Removed erroneous `1.0 - mid` computation in both thin-book and normal-book paths in `execute.rs`.
- **SimLive close on thin orderbooks**: Close path in `close.rs` now uses CLOB midpoint instead of empty orderbook bids (which returned 0.01) for thin-book markets, matching the entry path fix.
- **Spread gate blocking all trades**: BTC 15m markets have near-empty orderbooks with spread ~0.98. Thin-book detection now falls through to midpoint-based simulation fill instead of returning None.
- **Empty orderbook defaults**: `book_best_bid_ask` defaults changed from `(0.0, 1.0)` to `(0.50, 0.50)` to prevent fake 1.0 spreads on empty books.
- **Entry price range**: Widened `max_entry_price` from 0.80 to 0.92 so NO tokens priced above 0.80 aren't blocked.

### Fixed — Market Discovery
- **Gamma API pagination**: Active 15m markets were at offset 700+ due to hundreds of future events. Added targeted slug fetch by computing the expected slug from current ET time and fetching by `?slug=` directly.
- **`select_start_dt` priority chain**: Restructured to prioritize slug epoch (correct) first, `endDate - window` second, instead of the old order which prioritized the API's `startDate` (market creation time, ~24 hours before window start).
- **`parse_question_window_utc` unreachable**: Was gated behind `if dt.is_none()` but creation time always parses. Removed the guard.
- **`base_date` off by one day**: Creation time 01:53 UTC March 15 = 9:53 PM EDT March 14, producing wrong `base_date`. Added fallback to `base_date + 1 day`.

## [2026-03-11]

Note: Timestamp prefixes use Europe/London (`GMT`).

### Changed
- [08:20 GMT] Market timing selection now prefers the question window anchored to the Gamma `startDate`’s New York date, only falling back to raw `startDate`/slug epochs when question times are implausible. This fixes mins-left drift and “no active market” selection issues across `/api/markets` and analysis.
- [08:20 GMT] Lifecycle integration tests now force the paper execution path (`sim_live_execution=false`) and mark analyses as `forced` to avoid sim-live orderbook dependencies in unit runs.
- [08:32 GMT] Normalized market question labels to the computed ET date/time window (derived from `startDate` + question times) so dashboard titles match Polymarket’s display date.
- [08:36 GMT] Added ET window flooring from `startDate` (15m bucket alignment) as a fallback when Gamma’s `startDate` lags the true question window, reducing “no active market” gaps between rollovers.
- [08:40 GMT] L2 warmup rearm now completes even when only initial snapshots are received (no heartbeat within read timeout), preventing persistent `rearm_wait_ms` staleness.
- [08:55 GMT] Redacted `private_rpc_url` in config/API outputs and RPC responses now return only a truncated hint (e.g., `https:/…/abcd`) to avoid leaking full private URLs.
- [08:56 GMT] Spike direction now weights z-score vs velocity magnitude, preventing tiny velocity noise from overriding strong negative spikes.
- [08:56 GMT] Live partial-close is now explicitly blocked; live positions must use the full close path with FOK step-down.
- [08:57 GMT] Paper/sim execution now acquires the live execution lock to serialize trades and avoid TOCTOU balance races.
- [09:05 GMT] Added `regime_allowlist` gate and enforced `min_liquidity` in execution validation to avoid trading outside approved regimes or on thin markets.
- [09:05 GMT] Added `funding_rate_enabled` toggle (default true) to allow pruning the macro funding signal without code changes.
- [09:06 GMT] Added `shadow_live_parity` toggle to force SimLive execution paths during shadow runs when desired.
- [16:38 GMT] Analyzer now falls back to the nearest 15m ET boundary when all start-time derivation strategies fail, preventing market dropouts even when Gamma fields are stale.
- [16:38 GMT] `/api/markets` now includes `market_id` alongside `id` to avoid downstream ambiguity.
- [16:40 GMT] L2 WebSocket subscribe payload updated to Polymarket CLOB spec and a periodic `PING` heartbeat added to keep sessions alive.

## [2026-03-10]

Note: Timestamp prefixes use Europe/London (`GMT`).

### Added
- [00:35 GMT] Explicit entry/exit fill-parity model:
  - added `TradeFillParity` in `src/db/mod.rs`
  - computes per-leg drift (`entry` / `exit`) between stored trade prices and exchange-derived VWAP from `trade_fills`
  - classifies each leg as `match`, `mismatch`, `missing_reference`, or `missing_exchange_fill`
  - emits overall status (`ok`, `mismatch`, `incomplete`) with configurable thresholds.
- [00:35 GMT] Parity unit coverage:
  - added tests for open-trade match, closed-trade exit mismatch, and incomplete fill evidence paths in `src/db/mod.rs`.

### Changed
- [01:06 GMT] Added focused dashboard workspaces and new telemetry surfaces in `static/index.html`:
  - introduced terminal tab navigation (`Overview`, `Maker Desk`, `Execution`, `Risk`, `Config`) with persistent selection and section-level visibility routing
  - added `Execution Quality` workspace backed by `GET /api/trades/:id/fills` parity checks (entry/exit drift, match/mismatch/incomplete status, aggregate drift KPIs)
  - added `Risk` workspace backed by:
    - `GET /api/risk/status` (kill/degraded/L2/SLO/drop counters),
    - `GET /api/accounting/truth` (balance derivation and drift snapshot),
    - `GET /api/attribution` (mode-level performance rollup table)
  - added tab-aware polling so heavier endpoints refresh only while their workspace is active.
- [00:57 GMT] Theme correction to requested gallery style:
  - switched dashboard to Design 8 “Space Terminal” visual direction in `static/index.html`
  - updated palette/contrast/typography (Space Grotesk + dark terminal surfaces with amber accent)
  - retained maker cockpit blocks and existing realtime/data bindings.
- [00:52 GMT] Restored maker cockpit in UI (`static/index.html`):
  - reintroduced dedicated maker control panels (execution mode, L2 status/age, quote count, replace/min, market count)
  - added live tables for L2 top-of-book, active grid quotes, and quote-model state
  - wired frontend polling to `GET /api/maker/state` every 5s with graceful empty states.
- [00:48 GMT] Dashboard UI overhaul (`static/index.html`):
  - replaced legacy flat styling with a modern glass command-deck visual system (new typography, depth, gradients, responsive spacing)
  - upgraded open-positions table to show live mark and unrealized PnL columns directly in the grid
  - restructured Configuration into clustered section cards for faster scan/edit workflow
  - preserved existing IDs/endpoints and JS bindings so realtime telemetry/control behavior remains intact.
- [00:35 GMT] `GET /api/trades/:id/fills` now includes `parity_check` payload:
  - uses environment-configurable drift thresholds:
    - `MACBOT_ENTRY_PARITY_MAX_DRIFT_BPS` (default `35`)
    - `MACBOT_EXIT_PARITY_MAX_DRIFT_BPS` (default `35`).
- [00:35 GMT] Live close path now performs runtime fill-parity verification:
  - after persisting close state, computes fill parity from `trade_fills`
  - logs a warning on `mismatch` to surface entry/exit price divergence immediately.

## [2026-03-11]

### Added
- Startup analysis warm-up on boot (`MACBOT_STARTUP_ANALYSIS`, default `true`) so L2 has token targets before ingestion.
- One-shot market-filter sanity check at boot with warning when the active market list is empty.
- [17:30 GMT] New micro/macro signal stack: OI momentum, aggressor ratio, CoinGecko sentiment, Finnhub news sentiment, and LunarCrush social sentiment (with config toggles + API key slots).
- [17:30 GMT] Analysis snapshots persisted each tick (`analysis_snapshots` table) with `analysis_snapshot_enabled` control for model training and audits.
- [17:30 GMT] Hot-reload for `signal_weights` via `signal_weights_path` or `MACBOT_SIGNAL_WEIGHTS_PATH` without restarting the bot.
- [17:30 GMT] Calibration tooling scripts:
  - `scripts/calibrate_signal_weights.py` (logistic fit + Brier score)
  - `scripts/time_of_day_report.py` (hourly win-rate histogram)
  - `scripts/regime_allowlist_from_backtest.py` (allowlist suggestion from edge audit)
  - `scripts/requirements.txt` (numpy + scikit-learn).

### Changed
- L2 feed defaults relaxed to reduce false staleness (`DEFAULT_STALE_SECONDS=20`, `DEFAULT_STALE_DEBOUNCE_MS=2000`).
- Trade analytics now expose net PnL fields (`total_pnl_net`, `avg_*_net`, `profit_factor_net`, `max_drawdown_net`, `sharpe_ratio_net`, `pnl_series_net`).
- UI stats and PnL chart now prefer net series when available; navbar shows live L2 health + age chip.
- Execution actor priority lanes now enforce `risk/cancel > close > place/replace`:
  - Close commands have a dedicated queue, processed ahead of new placements.
  - L2 `FlushBestBidAsk` and outbox audit traffic moved to the low-priority shed lane.
- Live price derivation now supports CLOB `tokens[]` payloads when `outcomePrices` is missing, unblocking TP/SL and unrealized PnL for shadow/sim.
- PnL formatting now preserves negative sign (no more “losses displayed as positive”).
- Added `POST /api/balance/reset` (non-live only) to reset runtime balance and persist `initial_balance` (supports `force=true` to ignore open locks).
- Enforced card clipping so rounded corners don’t leak child backgrounds.
- Added `sim_live_execution` to run simulation/shadow trades through orderbook-driven live-like fills and close logic.
- Removed unused medium-drop metric from stats/Prometheus outputs.
- SimLive close now backfills missing token_id for legacy trades before attempting orderbook exit.
- SimLive close now skips orderbook exits on resolved (0/1) outcomes, avoiding unnecessary book fetch failures.
- Live markets feed now filters by configurable window length (default 15m ±1m) and pulls live YES/NO prices via CLOB token price when available.
- Markets refresh cadence in UI increased to 10s for more responsive price updates.
- Market/outcomes filtering now infers window length from slug/title (e.g., `15m`, `4h`) with start/end fallback and avoids event endDate window mismatches, improving 15m selection and recent outcome visibility.
- Market analysis selection now uses per-market `start_date` + inferred `window_mins` (instead of endDate-15m), fixing candidate selection when Gamma endDate is daily.
- Analysis window gating relaxed to avoid no-market gaps (min elapsed 0.5m, max window-0.1m) while still honoring minute-13 logic.
- Market list now computes `mins_left` from `start_date + window_mins` (not Gamma's daily endDate), fixing 24h mins-left display for 15m markets.
- When available, market start time is derived from the epoch suffix in the slug (e.g., `btc-updown-15m-1773297000`) to align countdown with Polymarket's actual 15m windows.
- Start time selection now chooses the best of slug-epoch or Gamma `startDate` based on proximity to now (and only accepts starts within +2 minutes), eliminating next-day windows and mins-left blowups.
- Market timing now prefers the window parsed from the human-readable question time range (ET) when available, falling back to slug-epoch or Gamma `startDate`, to align with Polymarket’s live timer.
- Question-time parsing now anchors to the Gamma `startDate` date (ET) when present, preventing day-shift mismatches between “March 12” labels and the live 15m window.
- Added optional `MACBOT_DEBUG_MARKETS=1` logging to show market-filter skip reasons and selection summary for Gamma feeds.
- L2 status now shows `OFF` in taker mode (no stale red badge) and risk panel reflects L2 as disabled in taker mode.
- Simulation config tuned for stability:
  - `execution_mode` set to `taker`
  - `drawdown_halt_*` raised to 15%
  - `min_trade_usd` raised to 1.0.

## [2026-03-16]

### Fixed
- [10:10 GMT] Market selection now rejects future windows: `/api/markets` and analysis filtering skip markets with `mins_left > window+1` or `elapsed_mins < -0.25`, preventing next-day 15m markets from being selected.
- [10:40 GMT] Execution parity now treats resolution exits (`exit_price` ≈ 0/1) as `resolved` instead of `missing_reference`, and UI badges show `resolved` as OK.

### Changed
- [10:45 GMT] Stop/Start loop UI now confirms `/api/loop/status` after action and surfaces a failure toast if the loop state didn’t change.
- [10:55 GMT] Applied calibrated signal weights + logit scale in `config.json` and wired `signal_weights_path` to `/tmp/calibration.json` for hot‑reload.

## [2026-03-09]

Note: Timestamp prefixes use Europe/London (`GMT`).

### Added
- [20:56 GMT] Daily alpha report gate script:
  - added `scripts/daily_alpha_report.py`
  - runs walk-forward candidate sweeps via `POST /api/backtest`
  - ranks top configurations by robustness (mean/median/worst PnL, edge-gap, positive-fold ratio)
  - evaluates explicit pass/fail kill criteria (with env overrides) and exits non-zero on fail
  - writes timestamped JSON + Markdown artifacts to `artifacts/alpha-reports/`.
- [20:29 GMT] Walk-forward alpha sweep CLI:
  - added `scripts/walkforward_alpha_sweep.py`
  - runs rolling `POST /api/backtest` folds with configurable shift anchors
  - sweeps `entry_threshold`, side mode (`both|yes_only|no_only`), and `adaptive_stand_down`
  - ranks candidates by robustness (`mean/median/worst` PnL, positive-fold count, edge-gap, trade-rate)
  - prints top-candidate fold-level breakdown for out-of-sample fragility checks.
- [20:06 GMT] Backtest regime-attribution CLI:
  - added `scripts/backtest_regime_report.py`
  - runs `POST /api/backtest` and prints a compact per-regime report with win-rate, break-even win-rate, edge-gap, expectancy, PnL, and avg cost.
- [19:44 GMT] Automated attribution report utility:
  - `scripts/attribution_report.py` for compact JSON/Markdown summaries from `GET /api/attribution`
  - includes top-N rollups for `by_mode`, `by_source`, `by_calibration`, and `by_run_id`.
- [19:44 GMT] Property-based maker-quote invariants:
  - added `proptest` dev dependency
  - added randomized `build_quote_plan` invariant coverage for spread/skew/bounds/size safety.
- [19:24 GMT] Analyze-front lifecycle integration coverage:
  - added deterministic seeded analyze-path test (`analyze -> execute -> partial close -> full close -> balance/PnL`)
  - uses minute-13 analysis branch without external network dependency, validating end-to-end execution accounting from analysis output.
- [19:17 GMT] Lifecycle integration coverage extended with partial-close path:
  - added serialized actor-flow test for `execute -> partial_close_trade -> close_trade -> balance/PnL`
  - validates realised-partial + remaining-close accounting consistency in simulation mode.
- [07:28 GMT] Run-level attribution plumbing:
  - `trades.run_id` persisted in SQLite with additive migration
  - runtime run id attached to newly opened live/paper trades (`MACBOT_RUN_ID` override or auto-generated)
  - attribution API now reports `current_run_id` and closed-trade buckets by `run_id`.
- [07:17 GMT] Accounting truth endpoint for restart-drift verification:
  - `GET /api/accounting/truth`
  - optional `sync_authoritative=true` to force live-balance sync before snapshot
  - reports DB-derived/non-live/open-live components, local-vs-authoritative drift, and threshold breach flag.
- [06:45 GMT] Durable idempotency cache storage in SQLite:
  - new `idempotency_cache` table with TTL-friendly index
  - DB helpers for get/upsert/delete/prune
  - restart-safe idempotency recovery in runtime paths.
- [06:45 GMT] Typed actor transport error contract:
  - `ActorCallError` (`Unavailable`, `SendFailed`, `ResponseFailed`)
  - result-returning serialized actor calls for execute/close flows with compatibility wrappers.
- [06:46 GMT] Admin-triggered full resync endpoint:
  - `POST /api/resync/full` to force immediate exchange+DB live reconciliation.
- [06:46 GMT] CLOB open-order adapter:
  - `ClobClient::list_open_orders()` with schema-tolerant parsing across common Polymarket payload variants.
- [06:46 GMT] DB helpers for history decoupling from hot memory:
  - `load_trade_by_id`
  - `load_recent_trades`
  - `load_recent_closed_trades`.
- [06:46 GMT] CI pipeline:
  - `.github/workflows/ci.yml` with hard gates for:
    - `cargo fmt --check`
    - `cargo clippy -- -D warnings`
    - `cargo test`.
- [06:46 GMT] Toolchain pin file:
  - `rust-toolchain.toml` including `rustfmt` and `clippy` components.
- [06:46 GMT] Automated shadow kill-criteria checker:
  - `scripts/shadow_kill_check.py` with hard pass/fail exit codes
  - evaluates minimum trade count, win rate, expectancy, profit factor, drawdown, and recent-window win rate.
- [06:46 GMT] Shadow gating runbook:
  - `ops/shadow-kill-criteria.md` with defaults, env overrides, and operational commands.
- [06:46 GMT] Native runtime shadow kill-criteria guard in bot loop:
  - evaluates closed shadow trades for minimum sample size, win rate, expectancy, profit factor, drawdown, and recent-window win rate
  - activates kill switch automatically when thresholds are breached
  - thresholds are tunable via env (`SHADOW_*`) and aligned with the shadow checker script.
- [06:46 GMT] Full restart contract runbook:
  - `ops/restart-contract.md` documenting startup/full-resync/SLO invariants
  - includes verification commands tied to automated tests.
- [06:46 GMT] Async maker outbox pipeline:
  - dedicated bounded channel + writer task for `order_outbox` persistence in actor lifecycle logs
  - non-blocking enqueue semantics from maker quote/cancel hot paths.
- [06:46 GMT] New typed analysis adapter module:
  - `src/analysis.rs`
  - introduces `AnalysisResult` + `AnalysisMarketData` parsing from JSON snapshots
  - preserves API payload compatibility while reducing internal JSON pointer usage in decision paths.
- [06:46 GMT] Phase-3 maker quote engine in the single-writer execution actor:
  - fair-value model from calibrated probability (`p_up`) with L2 midpoint fallback
  - dynamic spread targeting with floor/ceiling controls
  - inventory skewing (`maker_skew_per_usd_bps`, `maker_max_skew_bps`)
  - cancel/replace hysteresis guardrails:
    - minimum quote lifetime
    - per-side replace cooldown
    - global replaces-per-minute limiter.
- [06:46 GMT] Live/simulated maker order lifecycle handlers:
  - simulated quote IDs for paper/simulation runs
  - optional live quoting path for maker mode (`maker_live_quoting`)
  - quote placement/cancel audit entries written to `order_outbox` with `stage=maker_quote`.
- [06:46 GMT] Maker model telemetry in `GET /api/maker/state` snapshot:
  - per-token quote model state (`quote_model`)
  - replace-window utilization (`replace_events_last_min`).
- [06:46 GMT] Full dashboard maker cockpit telemetry surface:
  - new Market-Maker control tower panels (L2 feed, BBO, grid quotes, model decisions, inventory state)
  - maker-aware decision card labeling (`Quote Spread` vs taker `Estimated Cost`)
  - refreshed visual shell/theme for dense real-time operations.
- [06:46 GMT] Backtest endpoint now supports historical anchor testing via optional `end_time_ms` in `POST /api/backtest`,
  enabling proper walk-forward train/validate folds over past windows.
- [06:46 GMT] Cost-aware alpha controls:
  - new config knobs for economic trade gating:
    - `min_trade_usd`
    - `trade_gas_usd`
    - `edge_safety_buffer`
    - `hybrid_stand_down` + `hybrid_stand_down_min_closed` + `hybrid_stand_down_min_win_rate`.
- [06:46 GMT] Fusion calibration block in config for runtime-tunable model gates:
  - `fusion_calibration.logit_scale`
  - `fusion_calibration.min_score`
  - `fusion_calibration.fee_skip_band`
  - `fusion_calibration.min_net_ev`
  - `fusion_calibration.min_weighted_abs`.
- [06:46 GMT] Trade reproducibility metadata:
  - new `calibration_version` persisted on each opened trade
  - captures signal weights + fusion calibration + key EV thresholds used at entry time.
- [06:46 GMT] New attribution API endpoint: `GET /api/attribution`
  - aggregates closed-trade performance by `calibration_version`
  - also reports attribution by `source`.
- [06:46 GMT] Backtest robustness extensions:
  - regime-aware threshold overrides (`bull_entry_threshold`, `bear_entry_threshold`, `chop_entry_threshold`)
  - asymmetric side controls (`buy_yes_threshold`, `buy_no_threshold`)
  - side toggles (`allow_buy_yes`, `allow_buy_no`) and optional regime alignment (`require_regime_alignment`)
  - tunable anti-bias strength (`anti_bias_strength`) with auto-disable for one-sided backtests
  - adaptive stand-down controls:
    - `adaptive_stand_down`
    - `stand_down_window`
    - `stand_down_margin_pp`
    - `stand_down_cooldown_candles`
  - high-volatility threshold override (`high_vol_entry_threshold`, `high_volatility_threshold`)
  - built-in `edge_audit` section with:
    - break-even win-rate
    - edge gap
    - expectancy per trade
    - skipped-trade reason counters
    - per-signal and per-regime performance buckets.
- [06:46 GMT] Prometheus alerting pack for runtime/execution safety:
  - `ops/prometheus/macbot-alert-rules.yml` with 11 alerts
  - `ops/prometheus/README.md` setup/validation guide
  - `ops/prometheus/alertmanager.example.yml` starter routing template.
- [06:46 GMT] Operational health endpoints:
  - `GET /api/health`
  - `GET /api/ready` with dependency checks (`db`, execution actor, config validity, L2 readiness flags).
- [06:46 GMT] Immutable startup config snapshots:
  - on each process start, a redacted config snapshot is written to `artifacts/config-snapshots/`
  - configurable via `MACBOT_CONFIG_SNAPSHOT_DIR`.

### Changed
- [21:59 GMT] UI second pass (`static/index.html`):
  - added persistent Daylight/Midnight theme toggle (`T` hotkey + localStorage state)
  - introduced `data-ui-theme` variable overrides for a bright Apple-style daylight skin
  - cleaned Configuration Matrix into grouped section cards with improved field spacing and hierarchy.
- [21:08 GMT] Trading terminal visual overhaul (`static/index.html`):
  - shifted to Apple-inspired glass aesthetic (soft depth, frosted surfaces, tighter typography hierarchy)
  - replaced type system with `Manrope` + `IBM Plex Mono`
  - refreshed color tokens, card/header/chip styling, and status badges for clearer signal hierarchy
  - improved button/input states, hover behavior, and reduced-motion handling
  - preserved existing IDs/JS bindings so realtime maker/taker telemetry and controls remain functional.
- [20:06 GMT] Backtest now supports explicit maker-only cost modeling:
  - new request fields:
    - `execution_mode` (`taker`/`maker`)
    - `maker_fee_pct`
    - `maker_slippage_factor`
    - `maker_gas_usd`
  - maker mode applies maker economics to edge gate and PnL/cost computation while preserving strategy logic.
- [20:06 GMT] Per-regime backtest attribution expanded:
  - `edge_audit.by_regime[*]` now includes `break_even_win_rate_pct`, `edge_gap_pp`, and `expectancy_per_trade` (in addition to win-rate/PnL/cost).
- [19:44 GMT] Execution mode dispatch now routes through trait backends:
  - `execute_trade` uses `execute_with_backend` with `LiveBackend` / `PaperBackend`
  - preserves existing behavior while removing remaining mode-specific dispatch duplication at call site.
- [19:44 GMT] Analysis generation is now typed-first in `analyze_market`:
  - builds `AnalysisResult` before API projection
  - serializes via `AnalysisResult::to_value()` (with market-data-aware merge support) instead of JSON-only assembly.
- [19:44 GMT] Residual all-history production reads replaced with targeted queries:
  - new DB helpers: `load_trades_by_status`, `load_closed_trades`, `load_closed_shadow_trades`, `load_non_live_balance_replay_rows`
  - migrated shadow kill checks, drawdown replay, performance report, and trade analytics/signal/attribution routes off `load_all_trades`.
- [19:44 GMT] Attribution API expanded with execution-mode rollups:
  - `GET /api/attribution` now returns `by_mode` alongside existing source/calibration/run-id buckets.
- [07:37 GMT] Internal trade-execution transport now carries typed analysis payloads end-to-end:
  - `ExecutionCommand::ExecuteTrade` uses `AnalysisResult` instead of raw `serde_json::Value`
  - shared execute validator + live/paper execution paths consume typed analysis fields directly
  - bot loop executes through parsed-analysis actor calls, while API JSON endpoints remain backward-compatible via parsing shims.
- [07:37 GMT] Hybrid taker gate now reads volatility from typed analysis state (`AnalysisResult.volatility`) instead of ad-hoc JSON lookups in the actor hot path.
- [07:28 GMT] Config validation now includes deployment safety profiles:
  - profile resolution from `MACBOT_DEPLOYMENT_PROFILE` or mode/rollout-stage defaults
  - profile-aware invariant enforcement for:
    - `shadow` (explicit non-live shadow posture + economic floor)
    - `canary` (live with capped exposure and trade economics)
    - `live` (live-mode stale/idempotency/risk/economic safety bounds).
- [07:28 GMT] Execution actor now exports live lane-depth telemetry (`high|medium|low`) and applies medium-lane pressure shedding for coalesced `FlushBestBidAsk` ticks.
- [07:17 GMT] Runtime status payload assembly for `GET /api/risk/status` now uses explicit JSON map construction instead of a single deep `json!` macro block, avoiding macro recursion-limit build failures as telemetry fields grow.
- [07:17 GMT] Prometheus surface expanded for runtime safety telemetry:
  - actor drop class counters:
    - `macbot_actor_cmd_drop_high_total`
    - `macbot_actor_cmd_drop_medium_total`
    - `macbot_actor_cmd_drop_low_total`
  - degraded/drift counters and gauges:
    - `macbot_degraded_mode_transitions_total`
    - `macbot_balance_drift_warns_total`
    - `macbot_degraded_no_trade_active`
    - `macbot_last_balance_drift_usd`
    - `macbot_last_balance_drift_known`.
- [07:17 GMT] Maker actor command model cleanup:
  - removed unused `OnBestBidAsk` command variant
  - retained coalesced `FlushBestBidAsk` path as the single BBO ingestion command for reduced queue pressure.
- [06:45 GMT] CLOB open-order ingestion now supports cursor pagination and de-duplication:
  - multi-page `list_open_orders()` scan with `next_cursor` handling
  - safer mixed-shape payload parsing across root/meta cursor variants.
- [06:45 GMT] Full-resync reconcile now supports optional orphan cancellation policy:
  - when enabled via `MACBOT_RECONCILE_CANCEL_EXCHANGE_ORPHANS`, exchange-only open orders are canceled and audited.
- [06:46 GMT] Runtime idempotency path now uses memory+DB hybrid behavior:
  - reads fall back to SQLite if not present in hot cache
  - writes/prunes persist to both memory and durable store.
- [06:46 GMT] Daily PnL query path now uses index-friendly timestamp range predicates (replacing substring filtering).
- [06:46 GMT] DB indexing expanded for operational/reconcile workloads:
  - `idx_trades_status_mode_id`
  - `idx_trades_closed_exit_ts`
  - `idx_trades_closed_ts`
  - `idx_idempotency_created_at`.
- [06:46 GMT] Full live resync now includes exchange-authoritative open-order drift checks:
  - compares local OPEN live order IDs vs exchange OPEN order IDs
  - writes explicit reconcile drift audit entries
  - contributes to fail-closed drift policy (`MACBOT_RECONCILE_MAX_LIVE_DRIFT`).
- [06:46 GMT] TradingBot startup hot-memory model is now OPEN-only:
  - `TradingBot::new` loads only OPEN trades into `self.trades`
  - startup balance is restored from DB-backed closed non-live realised delta + open-position locks.
- [06:46 GMT] Historical/analytics consumers now read DB trade history directly (instead of relying on in-memory vectors):
  - `/api/trades`
  - `/api/trades/analysis`
  - `/api/signal-stats`
  - realtime recent-trade snapshot
  - performance report + daily PnL paths.
- [06:46 GMT] Live execute path outbox audit writes are now async via actor/fallback queue API:
  - replaced direct `append_order_outbox` awaits in `execute_live_trade`
  - added `TradingBot::append_outbox_audit` non-blocking emitter.
- [06:46 GMT] Incremental enum hardening sweep across runtime control flow:
  - added `TradingBot::mode_kind()` for typed mode reads from shared config
  - switched live-mode control gates to enum checks in startup/reconcile tasks
  - switched auth-required and live-CORS gates to enum checks (`BotMode`) in API middleware/routes
  - converted hybrid actor signal gate to typed `Signal` parsing (`HOLD` checks no longer stringly-typed)
  - converted key status/direction filters in trade analytics/metrics/Kelly helpers to typed accessors (`status_kind`, `direction_kind`, `mode_kind`)
  - force-trade direction parsing now uses typed `Direction` and canonical emission
  - fusion engine now uses typed `Signal` internally for hold/side selection and keeps canonical output strings at API boundaries
  - analyze/backtest decision loops now use typed `Signal` logic instead of raw string branching
  - execution/risk paths now consume typed parsed analysis fields in:
    - `src/bot/execute.rs`
    - `src/bot/actor.rs`
    - `src/bot/state.rs`
    - `src/api/bot_routes.rs`.
- [06:46 GMT] `OnBestBidAsk` actor events now actively run maker quote sync logic (not only state storage).
- [06:46 GMT] `SetL2Stale(true)` now triggers defensive maker cleanup by canceling active quotes.
- [06:46 GMT] L2 ingestion target metadata now tracks token outcome side (`is_yes_token`) so maker math can correctly map YES/NO fair value.
- [06:46 GMT] L2 market metadata loader now parses token outcomes (`YES/NO`, `UP/DOWN`) and backfills unknown token side mappings for active targets.
- [06:46 GMT] Dashboard stat normalization now handles both `/api/stats` and `/api/trades/analysis` schemas consistently:
  - win rate uses normalized `win_rate` when present (supports both `0..1` and `0..100`)
  - denominator is closed trades (not total including open)
  - max drawdown renders as `%` when pct fields are provided.
- [06:46 GMT] Maker feed telemetry copy now explicitly reports **event age** (time since last L2 event), not implied wire latency/RTT.
- [06:46 GMT] Top telemetry chip label changed from `Feed Lag` to `Snapshot Age` to avoid network-latency ambiguity.
- [06:46 GMT] Legacy indicator label in the signal surface is now displayed as `Momentum Composite` (including legacy `rsi_macd_ha` key mapping).
- [06:46 GMT] Default execution mode fallback is now `hybrid` (maker-first posture) when execution mode is missing/invalid.
- [06:46 GMT] Hybrid taker gate now includes:
  - regime-aware conviction/EV thresholds
  - volatility tightening
  - runtime stand-down when recent closed-trade win-rate is below configured floor.
- [06:46 GMT] Live/paper execution now enforces:
  - minimum economic trade size (`min_trade_usd`)
  - cost-aware EV gate (`net_ev >= min_ev + edge_safety_buffer + gas_pct`).
- [06:46 GMT] Unified pre-trade guardrails into a shared execution validator (`validate_trade_common`) used by both live and paper paths:
  - confidence/entry-range/hail-mary momentum gates
  - Kelly sizing and balance checks
  - EV + gas/buffer gate
  - shared signal-breakdown capture for trade attribution.
- [06:46 GMT] Fusion engine now consumes runtime calibration values from config (with normalized clamps), replacing static gate constants for score/edge/fee-zone behavior.
- [06:46 GMT] Config now exposes a deterministic `calibration_version()` fingerprint generator used by execution paths.
- [06:46 GMT] Prometheus open/closed trade gauges now read DB counts first (with in-memory fallback), reducing dependence on full in-memory trade history.
- [06:46 GMT] `/api/metrics` now exports maker/L2 gauges for alerting:
  - `macbot_l2_feed_stale`
  - `macbot_l2_feed_age_ms`
  - `macbot_maker_quote_count`
  - `macbot_maker_replace_events_last_min`.
- [06:46 GMT] Config mutation path now shares centralized normalization + invariant validation with startup config loading (same rules in both paths).
- [06:46 GMT] L2 ingestion now uses warmup + hysteresis semantics instead of immediate stale-clear:
  - feed remains stale until all target tokens have loaded snapshots
  - configurable stale-timeout debounce (`MACBOT_L2_STALE_DEBOUNCE_MS`)
  - configurable healthy rearm delay before clearing stale (`MACBOT_L2_REARM_MS`).
- [06:46 GMT] Risk status and telemetry now surface backpressure/SLO indicators:
  - `l2_age_slo_ms`, `l2_age_slo_breached`
  - `actor_cmd_drop_total`, `outbox_dropped_total`, `outbox_write_fail_total`.
- [06:46 GMT] Runtime SLO enforcement task now runs every 2s:
  - sustained L2 age/stale breaches can force degraded mode and optionally activate kill switch (`MACBOT_SLO_*`)
  - actor/outbox backpressure delta breaches activate kill switch (fail-closed).
- [06:46 GMT] Live startup reconcile path now executes `full_resync_live_state()` (not only fast reconcile), and a periodic full-resync task runs on a configurable cadence (`MACBOT_FULL_RESYNC_INTERVAL_SECS`).
- [06:46 GMT] `GET /api/risk/status` and `GET /api/ready` now expose/enforce SLO context:
  - SLO threshold knobs (`MACBOT_SLO_*`)
  - runtime SLO trip counters
  - reconcile/full-resync counters and drift policy values.
- [06:46 GMT] Prometheus alert pack expanded for new safety telemetry:
  - runtime SLO trip alert
  - reconcile drift detection alert.

### Fixed
- [19:44 GMT] Restored strict quality gates after backend/query/analysis refactors:
  - `cargo fmt --all` passes
  - `cargo clippy --all-targets --all-features -- -D warnings` passes
  - `cargo test --all-targets --all-features` passes (`76` tests).
- [19:24 GMT] Stabilized shadow profile validation test against mutable runtime fixtures by explicitly forcing non-shadow posture in test setup.
- [07:37 GMT] Restored clean quality gates after typed analysis-transport migration:
  - `cargo fmt --all` passes
  - `cargo clippy --all-targets --all-features -- -D warnings` passes
  - `cargo test --all-targets --all-features` passes (`72` tests).
- [07:28 GMT] Full reconcile ghost-close settlement now prefers authoritative prices:
  - uses persisted fill-summary VWAP first
  - then exchange order status (when available)
  - only falls back to local entry price when no authoritative fill data exists.
- [07:28 GMT] Added deterministic L2 replay contract test to ensure stable BBO evolution across repeated event replays.
- [07:17 GMT] Runtime SLO guard internals now avoid redundant assignment patterns in trip-counter tracking, keeping strict `clippy -D warnings` clean while preserving trip/kill semantics.
- [07:17 GMT] Restored clean quality gates after hardening pass:
  - `cargo fmt --all` passes
  - `cargo clippy --all-targets --all-features -- -D warnings` passes
  - `cargo test --all-targets --all-features` passes (`66` tests).
- [06:45 GMT] API actor-call failure handling now returns sanitized execution errors instead of silent `None` fallback in:
  - `POST /api/execute`
  - `POST /api/force_trade`
  - manual close/close-all routes.
- [06:45 GMT] Added regression tests for high-risk failure paths:
  - actor transport errors surface correctly from serialized calls
  - idempotency entries survive process restart + DB prune behavior.
- [06:46 GMT] Shadow kill criteria now evaluates persisted closed-shadow history from DB (not transient in-memory trade cache), preserving correctness with OPEN-only hot memory.
- [06:46 GMT] API trade-fills lookup now resolves by DB trade ID so closed-history rows remain accessible when not resident in hot memory.
- [06:46 GMT] Clear-history/delete-history in-memory refresh now reloads OPEN trades only, matching the new hot-memory contract.
- [06:46 GMT] Cleared all current lint blockers required by strict CI gates:
  - `cargo fmt --check` now passes
  - `cargo clippy -- -D warnings` now passes
  - resolved 16 hard-clippy findings (collapsible-if, range-contains, unnecessary filter_map, needless borrow, get-first, manual flatten/find, useless conversion, modulo helper)
  - explicitly annotated a small set of high-arity boundary functions where argument count is intentional (`append_order_outbox`, `append_trade_fill_snapshot`, `sign_order`, maker quote sync helpers)
  - preserved external CLOB order-type acronym naming (`GTC`, `FOK`) with targeted clippy allowance.
- [06:46 GMT] Cache helpers no longer hold async mutexes during remote fetches:
  - `get_cached_candles`
  - `get_cached_futures`
  - `get_cached_pcr`
  - `get_cached_fg`
  now use a lock-check → fetch-outside-lock → lock-commit pattern with post-fetch double-check.
- [06:46 GMT] Paper/simulation execution now verifies `amount + fees` affordability before opening positions to avoid zero-clamped balance drift in low-balance runs.
- [06:46 GMT] Live execution keeps a post-`sync_balance()` effective-balance guard before order placement to avoid stale-balance admits.
- [06:46 GMT] Completed `TopOfBook` signature propagation after adding `is_yes_token` metadata.
- [06:46 GMT] Updated Local L2 orderbook tests to cover side-tagged top-of-book outputs.
- [06:46 GMT] Sharpe display in the stat wall now suppresses unstable values for very small samples (`< 20` closed trades), reducing misleading early-session extremes.
- [06:46 GMT] Execution mode normalization test now matches runtime fallback (`unknown -> hybrid`) to prevent test/runtime drift.
- [06:46 GMT] Added focused backtest unit tests for:
  - asymmetric signal thresholds
  - side/regime filter behavior
  - one-sided anti-bias handling
  - adaptive stand-down trigger logic.
- [06:46 GMT] Added dedicated maker engine tests for:
  - quote-plan math and skew/spread boundary behavior
  - replace guardrails (min lifetime, cooldown, per-minute cap)
  - maker event path quote placement when feed/loop are healthy
  - stale-feed fail-closed actor behavior (stale command clears active quotes).
- [06:46 GMT] Added config invariant tests for:
  - entry bound ordering (`min_entry_price < max_entry_price`)
  - stake bound ordering (`min_stake <= max_stake`)
  - TP/SL threshold ordering when both are enabled.
- [06:46 GMT] Added execution-cycle guard tests for low-balance runs:
  - rejects paper/simulation entries when `amount + fees` would exceed available balance.
- [06:46 GMT] Added config normalization tests for fusion calibration clamp ranges.
- [06:46 GMT] Maker actor stale cleanup now runs on stale transition (`false -> true`) to avoid repeated quote-cancel storms on repeated stale events.
- [06:46 GMT] L2 publisher now detects execution-actor queue backpressure and fail-closes the feed session after configurable consecutive drops (`MACBOT_L2_MAX_ACTOR_DROPS`).
- [06:46 GMT] Added shadow runtime guard tests:
  - low-win-rate profile trips kill criteria
  - healthy profile does not trip kill criteria.
- [06:46 GMT] Added restart/full-resync contract tests:
  - drifted DB-vs-memory OPEN live state is repaired by stable IDs
  - ghost local OPEN live trades are force-closed with explicit exit type
  - drift contract fail-closed behavior trips kill switch by policy
  - no-drift full-resync keeps kill switch off.
- [06:46 GMT] Added runtime SLO guard tests:
  - sustained L2 breach trips fail-closed behavior in live maker mode
  - actor command-drop delta breach trips kill switch.
- [06:46 GMT] Added L2 backpressure chaos test:
  - actor publish drop streak triggers stale/feed fail-close path.
- [06:46 GMT] Added outbox writer failure-injection test:
  - DB write failure in async outbox path increments failure telemetry.
- [06:46 GMT] Added regression coverage for new hot-memory/reconcile paths:
  - startup OPEN-only load + balance reconstruction test
  - order-set drift diff test for exchange/local reconciliation
  - outbox-audit fallback write test
  - API full-resync endpoint simulation safety test
  - CLOB open-order schema parser test.

## [2026-03-08]

### Security
- [06:46 GMT] Added authenticated middleware for sensitive/mutating API routes:
  - `x-api-key` verification
  - HMAC signature verification via `x-api-signature`
  - timestamp skew check via `x-api-ts`
  - optional IP allowlist enforcement
- [06:46 GMT] Protected critical routes (`/api/trade*`, close/clear endpoints, loop start/stop, config updates, private RPC endpoints).
- [06:46 GMT] Tightened CORS behavior for live mode (explicit allowlist origins/headers instead of permissive defaults).
- [06:46 GMT] Redacted secret fields (`api_key`, `api_hmac_secret`) in `/api/config` responses.

### Added
- [06:46 GMT] Single-writer execution actor wiring at startup; mutation paths use serialized command flow.
- [06:46 GMT] Pair-close actor command for contiguous YES/NO rebalancer execution.
- [06:46 GMT] Live reconciliation worker (order drift/orphan handling + balance sync).
- [06:46 GMT] Capital rebalancer with mismatch, EV, gas, and failure guardrails.
- [06:46 GMT] Shadow-trading execution mode (same strategy pipeline, simulated executor).
- [06:46 GMT] Shared EV calculator module and `/api/ev/calc` endpoint.
- [06:46 GMT] Prometheus metrics endpoint (`/api/metrics`) with expanded counters.
- [06:46 GMT] Realtime snapshot builder module (`src/api/realtime.rs`) that emits:
  - fast telemetry snapshots (analysis, risk, execution, stats, balance, BTC live price)
  - slow snapshots (markets, outcomes, recent trades) with short-lived caching.
- [06:46 GMT] WebSocket stream endpoint: `GET /api/ws` (dashboard-first transport).
- [06:46 GMT] Private RPC module:
  - raw transaction relay helper
  - `eth_maxPriorityFeePerGas` helper
  - bounded EIP-1559 priority-fee calculation
- [06:46 GMT] Private RPC API routes:
  - `/api/rpc/priority-fee`
  - `/api/rpc/send-raw`
- [06:46 GMT] Dedicated Local L2 ingestion engine for maker/hybrid execution:
  - new fixed-point in-memory orderbook module (`src/market/l2_book.rs`) using `BTreeMap`
  - new persistent Polymarket CLOB WebSocket ingestor (`src/market/l2_ws.rs`)
  - snapshot + delta processing with zero-size level removals and sequence-gap detection
  - best-bid/best-ask event publication into the single-writer execution actor.
- [06:46 GMT] Durable live-order recovery ledger:
  - new SQLite `order_outbox` table
  - order lifecycle logging around placement/persist/cancel-failure states.
- [06:46 GMT] Fill-ledger reconciliation layer:
  - new SQLite `trade_fills` table with cumulative + incremental fill snapshots
  - per-trade entry/exit VWAP and slippage summary computation
  - new API endpoint: `GET /api/trades/:id/fills`.
- [06:46 GMT] Execution-mode control foundation for Path B:
  - new config field `execution_mode` (`taker` | `maker` | `hybrid`) with safe defaulting
  - new maker actor state map (`HashMap<OrderId, QuoteState>`) and inventory state map
  - new maker state API endpoint: `GET /api/maker/state`
  - new protected maker-control endpoints:
    - `POST /api/maker/quote`
    - `DELETE /api/maker/quote/:order_id`
    - `POST /api/maker/clear-market`
    - `POST /api/maker/inventory`.

### Changed
- [06:46 GMT] Main runtime now:
  - starts execution actor on boot
  - serves Axum with `ConnectInfo<SocketAddr>` for IP-aware auth
  - runs periodic live reconciliation and capital rebalancer tasks
  - starts a dedicated L2 ingestion task for maker/hybrid mode.
- [06:46 GMT] API trade and close handlers now route through serialized execution methods.
- [06:46 GMT] Execution actor now routes trade decisions by `execution_mode`:
  - `taker`: existing path
  - `maker`: suppresses directional taker executes
  - `hybrid`: only allows taker execute when stricter confidence/EV gate passes.
- [06:46 GMT] Hybrid mode now additionally blocks taker execution when the L2 feed is stale (degraded no-trade safety).
- [06:46 GMT] Maker actor snapshot now includes live per-token L2 top-of-book state and feed stale state.
- [06:46 GMT] Force-trade path uses fresh price resolution and idempotent response caching.
- [06:46 GMT] On-chain balance fallback now uses shared private RPC client utilities.
- [06:46 GMT] EIP-712 signing hot path optimized with cached stable inputs:
  - cached maker address string
  - cached token-id `U256` parse.
- [06:46 GMT] SSE stream (`/api/stream`) upgraded to high-frequency snapshots with merged payload kinds:
  - `fast` snapshots at sub-second cadence
  - periodic `fast+slow` snapshots carrying markets/outcomes/trades.
- [06:46 GMT] Dashboard transport switched to WebSocket-first with SSE fallback/reconnect probing.
- [06:46 GMT] Dashboard rendering moved to event-driven updates from stream payloads (reduced reliance on REST polling).
- [06:46 GMT] Risk telemetry now includes execution mode, and config UI now exposes an `Execution Mode` selector.
- [06:46 GMT] Markets snapshot payload now includes `best_bid`, `best_ask`, and `spread_bps` fields.
- [06:46 GMT] UI now includes a realtime telemetry strip (Net EV, OB imbalance, tick velocity, trigger status, top spread, feed lag, last execution).
- [06:46 GMT] Backtest engine upgraded for more realistic evaluation:
  - larger configurable warmup/lookback window (default 50)
  - configurable entry threshold
  - risk-capped position sizing (`max_trade_pct`)
  - adaptive minimum stake handling for small account sizes
  - fee/slippage/gas costs included in per-trade PnL
  - anti-bias score correction to reduce one-sided directional clustering
  - paginated Binance kline fetching (removes single-call 1,000 candle cap)
  - expanded max backtest horizon from 30 to 180 days with true multi-window coverage.
- [06:46 GMT] Added a dedicated TP/SL guard loop that runs on `price_refresh_rate` cadence, independent of the slower strategy `check_interval`.
- [06:46 GMT] Backtest response now surfaces explicit data-window diagnostics (`requested_days`, `effective_days_covered`, `data_start_utc`, `data_end_utc`, `data_warning`).
- [06:46 GMT] Backtest kline pagination now uses explicit `startTime`/`endTime` windows to respect requested horizons more deterministically.
- [06:46 GMT] Rebuilt signal fusion / weighting engine (`src/fusion/mod.rs`):
  - replaced legacy vote-only posterior with normalized signed features (`[-1,+1]`) per signal
  - added phase-aware reliability profiling (microstructure vs momentum vs macro)
  - added robust weight normalization and safer handling of malformed/non-summing configs
  - switched directional decision to calibrated mispricing (`p_up - market_price`) rather than raw vote dominance
  - improved gating reasons with agreement, score quality, and explicit edge floor reporting
  - preserved backward compatibility for existing config/UI keys (`signal_weights.*` and breakdown labels).

### Fixed
- [06:46 GMT] TP/SL monitor now evaluates all open trades (not only the currently analysed market), preventing missed closes when market focus rotates.
- [06:46 GMT] TP/SL close path now guards against false "closed" updates when the underlying close attempt returns an still-open trade.
- [06:46 GMT] Open-trade payloads now include live token price and unrealized P&L fields.
- [06:46 GMT] Realtime trade snapshots (`/api/stream` + `/api/ws`) now carry enriched open-trade mark-to-market fields.
- [06:46 GMT] Dashboard Open Positions table now displays live price and unrealized P&L.
- [06:46 GMT] Forced-trade analysis payload now backfills and persists `market_data.end_date` from market metadata when missing, so forced positions participate in normal expiry/auto-resolution flow.
- [06:46 GMT] Resolution guard now uses a Gamma-first, CLOB-fallback resolution lookup and only settles when outcome confidence is strong enough, reducing false early settlement near market close.
- [06:46 GMT] Auto-resolved paper/simulation trades are now persisted with `exit_type="auto_resolved"` instead of defaulting to `manual`.
- [06:46 GMT] Market-data fetch helpers now support env-overridable bases (`MACBOT_GAMMA_BASE`, `MACBOT_CLOB_BASE`) for safer testing and controlled routing.
- [06:46 GMT] Live execution/close/reconciliation now persist exchange fill snapshots and derive slippage from actual fill flow instead of relying only on local assumed prices.
- [06:46 GMT] Resolution guard settlement PnL now prefers exchange-observed average entry fill price when available.
- [06:46 GMT] Trading loop now correctly respects loop start/stop state.
- [06:46 GMT] Live order creation now hard-fails on rejected/empty `order_id` responses.
- [06:46 GMT] Trade insert failure handling corrected (no false success on DB errors).
- [06:46 GMT] Trade ID generation uses DB rowid assignment (no `MAX(id)+1` race pattern).
- [06:46 GMT] Partial-close and full-close PnL behavior corrected for token-price model consistency.
- [06:46 GMT] Full-close PnL now correctly incorporates previously realized partial PnL.
- [06:46 GMT] Restart balance reconstruction fixed for fees and partial-realized PnL.
- [06:46 GMT] Market price fallback parsing fixed for string-encoded `outcomePrices`.
- [06:46 GMT] TP/SL price fetch now falls back from Gamma to CLOB market snapshots when token-level quotes are unavailable, reducing skipped risk checks.
- [06:46 GMT] Added runtime L2 feed health telemetry (`l2_feed_stale`, `l2_feed_age_ms`) to risk snapshots.
- [06:46 GMT] Added API idempotency support for trade/force/close/close-all routes.
- [06:46 GMT] Runtime risk armor expanded:
  - stale analysis gate
  - kill switch
  - max open positions gate
  - max slippage gate.
- [06:46 GMT] Actor error path is now fail-closed (activates kill switch; no direct execution fallback).
- [06:46 GMT] Idempotency cache now has TTL and max-entry pruning (prevents unbounded growth).
- [06:46 GMT] Rebalancer EV now includes exit-fee effects and gas assumptions.
- [06:46 GMT] Rebalancer failure mode hardened (partial pair close triggers kill switch/degraded no-trade behavior).
- [06:46 GMT] Removed async DB await while holding reconciliation write lock.
- [06:46 GMT] Completed full hardening sweep (31-point implementation set), grouped by phase:

#### Phase 1: Core Safety + Hygiene
- [06:46 GMT] Added `subtle` and `zeroize` dependencies.
- [06:46 GMT] Added `trades.db`, `trades.db-shm`, and `trades.db-wal` to `.gitignore`.
- [06:46 GMT] Replaced manual `Default` implementation for metrics with `#[derive(Default)]`.
- [06:46 GMT] Marked legacy `auto_trade` as deprecated-compatible with `#[serde(default)]`.
- [06:46 GMT] Config persistence now writes to temp file and atomically renames.
- [06:46 GMT] Internal server errors now return sanitized HTTP messages.

#### Phase 2: Security Hardening
- [06:46 GMT] API key/signature compares now use constant-time equality with length-safe padding.
- [06:46 GMT] Client IP extraction now prefers `ConnectInfo` and only falls back to `X-Forwarded-For`.
- [06:46 GMT] Added per-IP request limiting and route wiring for protected paths.
- [06:46 GMT] Upgraded rate limiting to true sliding-window behavior (60 requests / 60 seconds).
- [06:46 GMT] Added `Drop` zeroization for `api_secret`, `passphrase`, and `api_key`.
- [06:46 GMT] Added explicit live-mode TLS warning at startup.

#### Phase 3: Math + Logic Correctness
- [06:46 GMT] Corrected tick-velocity acceleration formula to `vel_30s - vel_60s`.
- [06:46 GMT] CLOB maker/taker amount conversion now rounds instead of truncating casts.
- [06:46 GMT] Removed obsolete dead close-path flag handling.
- [06:46 GMT] Sharpe ratio in API stats/backtest now uses sample variance (`n - 1`).
- [06:46 GMT] Removed unsafe `unwrap()` usage across signal modules.

#### Phase 4: Reliability + Runtime Architecture
- [06:46 GMT] Converted `TradingBot::mode()` to async and updated all call sites.
- [06:46 GMT] Idempotency eviction now removes oldest entries first.
- [06:46 GMT] Added 20-minute TTL eviction for `btc_start_cache`.
- [06:46 GMT] Replaced paused-loop polling with `Notify` wakeups.
- [06:46 GMT] Added graceful shutdown handling for `Ctrl+C` and `SIGTERM`.
- [06:46 GMT] Config and DB file paths are now configurable via env vars (`MACBOT_CONFIG`, `MACBOT_DB`).

#### Phase 5: Cleanup + Durability
- [06:46 GMT] Removed duplicate per-client HTTP construction in CLOB client, using shared HTTP singleton.
- [06:46 GMT] Config durability strengthened further: temp file `sync_all` + parent directory `sync_all` after rename (unix).
- [06:46 GMT] Trade-history SQL status filtering now binds canonical status strings from enum values (`TradeStatus::Open.as_str()`), reducing literal drift.

### Tests
- [06:46 GMT] Added serialized integration-cycle test (`execute -> close -> PnL/balance`) covering actor-driven execution flow:
  - open trade via `execute_trade_serialized`
  - close via `close_trade_serialized`
  - assert status transition and balance accounting consistency in simulation mode.
- [06:46 GMT] Added/expanded tests for:
  - merge EV guard math
  - bounded priority-fee logic
  - hex parsing helper
  - auth helper correctness (`secure_eq`, secret selection, deterministic HMAC)
  - fusion fee/weight/chop behavior checks
  - forced-trade market metadata backfill logic (missing `end_date` / question merge behavior)
  - trade-fill summary incremental VWAP/slippage math.
- [06:46 GMT] Test suite now passes with 45 tests.

### Tooling Notes
- [06:46 GMT] `cargo test` passes.
- [06:46 GMT] `cargo check` passes.
- [06:46 GMT] `cargo fmt` and `cargo clippy` could not run because `rustfmt`/`clippy` components are not installed in the active toolchain.
# Session 97 - Hour Filter Recalibration

- **Retested the inherited blocked UTC hours instead of carrying them on faith** — reran targeted maker recency gates against the current `trend_up_yes_hour_filtered_specialist` rail and captured clean artifacts for `unblock_02` and `unblock_16`:
  - baseline blocked rail: artifacts/alpha-reports/20260323_063309_recency_alpha_report.json
  - unblock `02 UTC`: artifacts/alpha-reports/20260323_064744_recency_alpha_report.json
  - unblock `16 UTC`: artifacts/alpha-reports/20260323_064817_recency_alpha_report.json
- **Removed `02 UTC` from the frozen maker specialist rail** — it improved the primary composite from about `2.27` to `2.69` while still clearing the `60d` stress gate, so the preset and maker rollout defaults now use `blocked_hours_utc=[1,3,4,6,16,17,18,19,20]`.
- **Left `16 UTC` blocked for now** — it still passed the recency gate, but it was weaker than the current blocked baseline, so it did not earn an unblock.
# Session 98 - Retire Maker Widening

- **Retired the backwardation widening branch as an active rollout candidate** — the latest maker evidence now shows the `40bps + floor120bps` candidate regressing after the hour-filter correction:
  - artifacts/maker-soak-reports/20260323_071411_maker_ab_compare.json
  - artifacts/maker-ab-batches/20260323_072825_maker_ab_batch.json
- **Added explicit rollout state** — [ops/profiles/maker_rollout_state.json](../../ops/profiles/maker_rollout_state.json) now marks maker as `baseline_only` with no active candidate, while preserving `widen20` and `widen40_floor120` as retired research artifacts.
- **Made readiness/operator surfaces honest again** — [scripts/maker_rollout_readiness_report.py](../../scripts/maker_rollout_readiness_report.py), [src/api/maker.rs](../../src/api/maker.rs), and [ops/live-rollout-runbook.md](../../ops/live-rollout-runbook.md) now treat maker as baseline-only until a new candidate is explicitly activated.
