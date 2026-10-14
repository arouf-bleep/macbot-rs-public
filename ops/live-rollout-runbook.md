# Live Rollout Runbook

Use this runbook before any canary or live deployment.

This repo’s checked-in [config.json](../config.json) is a simulation/dev-shaped config, not a live deployment template. Treat live rollout as blocked until `GET /api/preflight` is green in the target environment.

See also:

- [restart-contract.md](./restart-contract.md)
- [shadow-kill-criteria.md](./shadow-kill-criteria.md)
- [prometheus/README.md](./prometheus/README.md)

## Assumptions

1. TLS is terminated at a reverse proxy in front of the bot.
2. Live mode requires auth and non-empty API credentials.
3. `MACBOT_DEV_CORS` is disabled for any canary/live rollout.
4. `MACBOT_BIND_ALL=1` is only used behind a trusted reverse proxy.
5. `.env` and any secret material are owner-readable only.

## Phase 0: Pre-deploy checks

Run these before switching any target to `mode=live`.

```bash
cargo fmt --check
cargo check
cargo test
python3 scripts/sqlite_backup.py --source trades.db --dest artifacts/backups/prelive.sqlite
ls -l .env
jq '.[0]' ops/macro-events.json
```

Expected:

- Rust checks pass cleanly.
- Backup script completes with `backup ok: ...` with schema version and critical counts.
- `.env` is not world-readable (`-rw-------` / `600`).
- `ops/macro-events.json` parses cleanly and contains upcoming UTC timestamps if macro guardrails are enabled.

Before the following diagnostic reads, set your read key privately in `MACBOT_API_KEY`. The examples pass it as a header; never place it in a query string.

## Phase 1: Boot target in simulation or shadow

Confirm the bot is healthy before any live flip.

```bash
curl -s -H "x-api-key: $MACBOT_API_KEY" http://127.0.0.1:8000/api/health | jq
curl -s -H "x-api-key: $MACBOT_API_KEY" http://127.0.0.1:8000/api/ready | jq
curl -s -H "x-api-key: $MACBOT_API_KEY" http://127.0.0.1:8000/api/preflight | jq
curl -s -H "x-api-key: $MACBOT_API_KEY" http://127.0.0.1:8000/api/config | jq
curl -s -H "x-api-key: $MACBOT_API_KEY" http://127.0.0.1:8000/api/risk/status | jq
curl -s -H "x-api-key: $MACBOT_API_KEY" http://127.0.0.1:8000/api/maker/state | jq
curl -s -H "x-api-key: $MACBOT_API_KEY" "http://127.0.0.1:8000/api/accounting/truth?sync_authoritative=false" | jq
```

Expected:

- `/api/health.status == "ok"`
- `/api/ready.service_ready == true`
- `/api/ready.checks.db_quick_check_ok == true`
- `/api/ready.last_reconcile_error == null`
- `/api/preflight.preflight.ready == true` before any live flip
- `/api/preflight.preflight.checks.env_file_permissions_ok == true`
- `/api/risk/status.kill_switch == false`
- `/api/risk/status.degraded_no_trade == false`
- `/api/risk/status.macro_guard.error == null`
- `/api/risk/status.macro_guard.blocking == false` unless you are intentionally inside an NFP/CPI/FOMC stand-down window
- `/api/accounting/truth.drift_vs_authoritative_usd == null` in simulation, or a small value in live

If macro-event guardrails are enabled, sanity-check the next scheduled event explicitly:

```bash
curl -s -H "x-api-key: $MACBOT_API_KEY" http://127.0.0.1:8000/api/risk/status | jq '.macro_guard'
```

Expected:

- `enabled == true`
- `next != null` during normal operation
- `active != null` only when the bot is intentionally standing down around a scheduled macro event

## Phase 1B: Optional Maker Paper Soak

Before promoting any maker strategy change, prefer a paper-soak run with the current runtime rather than relying only on historical wrappers.

Materialize the frozen paper profiles first:

```bash
python3 scripts/build_maker_rollout_profiles.py
python3 scripts/maker_rollout_readiness_report.py
```

Historical research profiles (not revalidated under the current latency, queue, depth and venue-minimum rules; small quotes may be refused):

- [maker_paper_baseline.json](./profiles/maker_paper_baseline.json)
- [maker_paper_widen20.json](./profiles/maker_paper_widen20.json)
- [maker_paper_widen40_floor120.json](./profiles/maker_paper_widen40_floor120.json)
- [maker_paper_rollguard_c15_s5_n6.json](./profiles/maker_paper_rollguard_c15_s5_n6.json)
- [maker_rollout_state.json](./profiles/maker_rollout_state.json)

Historical selection state, not current funded readiness:

- baseline is the only active maker rollout profile
- widening profiles are retained as retired research artifacts, not active rollout candidates
- roll-guard profile is research-only and not an active rollout candidate yet

For unattended evidence gathering plus readiness refresh, use:

```bash
python3 scripts/run_maker_rollout_gate.py
```

Expected:

- The gate runner rebuilds the frozen profiles.
- It refreshes the readiness report before and after each evidence batch.
- It idles across blocked UTC windows automatically and resumes when the next eligible maker start window opens.
- It stops only when the paper candidate becomes ready or the configured round budget is exhausted.

```bash
python3 scripts/run_maker_soak.py \
  --config config.json \
  --db trades.db \
  --duration-minutes 30 \
  --enable-macro-guard
```

Expected:

- The temporary runtime boots with `execution_mode=maker` and `sim_live_execution=true`.
- `/api/health` becomes healthy before the soak timer starts.
- A fresh maker soak artifact is emitted at the end of the run.
- Review the artifact for:
  - quote count and cancel reasons
  - adverse move after placement/fill
  - futures basis buckets
  - `price_to_beat_distance_pct` context

Do not promote a new maker rule if the paper soak shows systematic toxic fills or rapid adverse selection.

Once a candidate is frozen, prefer running the soak from the frozen profile rather than from ad-hoc flags:

```bash
python3 scripts/run_frozen_maker_soak.py \
  --config ops/profiles/maker_paper_widen20.json \
  --db trades.db \
  --duration-minutes 30
```

Example for the retired floor-based widening research candidate:

```bash
python3 scripts/run_frozen_maker_soak.py \
  --config ops/profiles/maker_paper_widen40_floor120.json \
  --db trades.db \
  --duration-minutes 30
```

Expected:

- The runtime boots with the exact frozen maker paper profile.
- The readiness artifact from `scripts/maker_rollout_readiness_report.py` is reviewed before promotion.
- The frozen profile is not changed during the soak window.
- If [maker_rollout_state.json](./profiles/maker_rollout_state.json) has no active candidate, treat any widening soak as research-only rather than a promotable rollout path.

## Phase 2: Shadow / canary gate

Run the shadow checker before promoting a strategy or config to canary/live.

```bash
MACBOT_API_BASE="http://127.0.0.1:8000" \
scripts/shadow_kill_check.py
```

Expected:

- Exit code `0`
- Shadow trade sample meets the thresholds in [shadow-kill-criteria.md](./shadow-kill-criteria.md)

If the checker fails, do not promote. Investigate recent closed shadow trades and `GET /api/risk/status` first.

## Phase 3: Canary promotion

Promote in phases with explicit health gates.

1. Keep the bot behind the reverse proxy with TLS termination.
2. Enable auth credentials in config/env.
3. Confirm `GET /api/preflight` is green.
4. Start the loop only after the target is healthy:

```bash
curl -s -H "x-api-key: $MACBOT_API_KEY" http://127.0.0.1:8000/api/preflight | jq
curl -s -H "x-api-key: $MACBOT_API_KEY" http://127.0.0.1:8000/api/ready | jq
```

Expected:

- `/api/preflight.preflight.required == true` in live mode
- `/api/preflight.preflight.ready == true`
- `/api/ready.trading_ready == true`

Monitor during canary:

```bash
watch -n 5 'curl -s -H "x-api-key: $MACBOT_API_KEY" http://127.0.0.1:8000/api/risk/status | jq'
watch -n 5 'curl -s -H "x-api-key: $MACBOT_API_KEY" http://127.0.0.1:8000/api/accounting/truth | jq'
watch -n 5 'curl -s -H "x-api-key: $MACBOT_API_KEY" http://127.0.0.1:8000/api/ready | jq'
```

Watch for:

- `kill_switch == false`
- `l2_feed_stale == false`
- `l2_age_slo_breached == false`
- `macro_guard.error == null`
- `db_quick_check_ok == true`
- `last_reconcile_error == null`
- `actor_cmd_drop_total` and `outbox_dropped_total` staying stable
- `drift_vs_authoritative_usd` staying near zero
- `trade_entry_block_*_total` counters not climbing unexpectedly without a known reason

## Auth notes

Mutating routes require:

- `x-api-key`
- `x-api-ts`
- `x-api-signature`

Canonical HMAC payload format is:

```text
{timestamp}:{METHOD}:{path_and_query}:{body}
```

Detailed read endpoints require `x-api-key` when auth is enabled or live. Minimal health/readiness stay public; add the read key to readiness requests for diagnostic fields. Browser SSE/WS access obtains an authenticated short-lived stream token.

## Rollback triggers

Rollback immediately if any of these trip during canary/live:

1. `GET /api/preflight` turns red.
2. `GET /api/ready` reports `trading_ready=false`.
3. `kill_switch=true`.
4. Sustained L2 stale / age SLO breach.
5. `drift_vs_authoritative_usd` exceeds your operating threshold.
6. Reconcile drift events spike or repeated orphan repairs appear.
7. Shadow/canary hit rate or expectancy breaks the rollout contract.

## Rollback procedure

1. Stop new trading.
2. Preserve current telemetry snapshots from `/api/risk/status`, `/api/accounting/truth`, and `/api/preflight`.
3. Create a verified SQLite backup before any manual DB intervention:

```bash
python3 scripts/sqlite_backup.py --source trades.db --dest artifacts/backups/rollback.sqlite
```

Both backup tools verify schema version, SQLite integrity and all fifteen operational tables listed in [restart-contract.md](restart-contract.md). The API requires a fresh filename; CLI overwrite replaces only an inactive backup after successful validation. Use the actual `MACBOT_DB` path if it differs from `trades.db`. See [recovery-runbook.md](recovery-runbook.md) before any restore or repair.

Or, if the authenticated API is healthy, trigger the built-in verified backup path:

```bash
curl -s -X POST http://127.0.0.1:8000/api/db/backup \
  -H 'Content-Type: application/json' \
  -H "x-api-key: $MACBOT_API_KEY" \
  -H "x-api-ts: $(date +%s)" \
  -H "x-api-signature: <signed-request>" \
  -d '{"path":"artifacts/backups/rollback.sqlite"}' | jq
```

4. If needed, force kill switch via the authenticated risk endpoint.
5. Restart only under the [restart-contract.md](./restart-contract.md) invariants.
6. Do not re-enable live trading until preflight and readiness are green again.

## Post-deploy verification

After any restart or promotion:

```bash
curl -s -H "x-api-key: $MACBOT_API_KEY" http://127.0.0.1:8000/api/preflight | jq
curl -s -H "x-api-key: $MACBOT_API_KEY" http://127.0.0.1:8000/api/ready | jq
curl -s -H "x-api-key: $MACBOT_API_KEY" http://127.0.0.1:8000/api/risk/status | jq
curl -s -H "x-api-key: $MACBOT_API_KEY" http://127.0.0.1:8000/api/accounting/truth | jq
```

Expected:

- No preflight errors
- `service_ready=true`
- `db_quick_check_ok=true`
- `trading_ready=true` for live
- `last_reconcile_error == null`
- `last_reconcile_summary` shows a clean or bounded-drift repair outcome after restart
- No unexpected balance drift
- No stale L2 / backpressure trip indicators
