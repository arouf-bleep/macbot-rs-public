#!/usr/bin/env bash
set -euo pipefail

MACBOT_REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MACBOT_SMOKE_TMP="$(mktemp -d "${TMPDIR:-/tmp}/macbot-smoke.XXXXXX")"
MACBOT_SMOKE_PID=""
MACBOT_SMOKE_PORT="${MACBOT_SMOKE_PORT:-18080}"
MACBOT_SMOKE_URL="http://127.0.0.1:${MACBOT_SMOKE_PORT}"

cleanup() {
    if [ -n "$MACBOT_SMOKE_PID" ] && kill -0 "$MACBOT_SMOKE_PID" 2>/dev/null; then
        kill -TERM "$MACBOT_SMOKE_PID" 2>/dev/null || true
        wait "$MACBOT_SMOKE_PID" 2>/dev/null || true
    fi
    rm -rf -- "$MACBOT_SMOKE_TMP"
}
trap cleanup EXIT INT TERM

cp "$MACBOT_REPO_ROOT/config.json" "$MACBOT_SMOKE_TMP/config.json"
export MACBOT_CONFIG="$MACBOT_SMOKE_TMP/config.json"
export MACBOT_DB="$MACBOT_SMOKE_TMP/smoke.sqlite"
export MACBOT_ML_WEIGHTS_PATH="$MACBOT_SMOKE_TMP/ml_weights.json"
export MACBOT_ENSEMBLE_PATH="$MACBOT_SMOKE_TMP/ensemble_state.json"
export MACBOT_CONFIG_SNAPSHOT_DIR="$MACBOT_SMOKE_TMP/config-snapshots"
export MACBOT_ALERT_FALLBACK_PATH="$MACBOT_SMOKE_TMP/alerts.jsonl"
export MACBOT_MODELS_DIR="$MACBOT_REPO_ROOT/models"
export MACBOT_BACKTEST_ONLY=1
export MACBOT_STARTUP_ANALYSIS=0
export MACBOT_BIND_ALL=0
export MACBOT_PORT="$MACBOT_SMOKE_PORT"
export MACBOT_API_KEY=local
export MACBOT_API_SECRET=localsecret
export MACBOT_API_HMAC_SECRET=localsecret

cd "$MACBOT_REPO_ROOT"
# Build before the health poll starts its clock. `cargo test` earlier in
# verify.sh compiles this crate as a test harness, not as a plain binary, so the
# first `cargo run` still has to compile and link. On a cold machine that
# exceeds the poll budget below and the smoke test fails with a bare curl
# "failed to connect" and no diagnostic, which looks like a broken service
# rather than a slow build.
cargo build --locked

MACBOT_SMOKE_TARGET_DIR="$(cargo metadata --locked --format-version 1 --no-deps \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
"$MACBOT_SMOKE_TARGET_DIR/debug/macbot" >"$MACBOT_SMOKE_TMP/server.log" 2>&1 &
MACBOT_SMOKE_PID="$!"

MACBOT_SMOKE_ATTEMPTS="${MACBOT_SMOKE_ATTEMPTS:-240}"
MACBOT_SMOKE_READY=0
for _ in $(seq 1 "$MACBOT_SMOKE_ATTEMPTS"); do
    if curl --max-time 1 --fail --silent "$MACBOT_SMOKE_URL/api/health" >/dev/null; then
        MACBOT_SMOKE_READY=1
        break
    fi
    if ! kill -0 "$MACBOT_SMOKE_PID" 2>/dev/null; then
        echo "Smoke-test server exited before becoming healthy" >&2
        sed -n '1,200p' "$MACBOT_SMOKE_TMP/server.log" >&2
        exit 1
    fi
    sleep 0.25
done

# Say why, rather than letting the next curl fail with a bare exit 7.
if [ "$MACBOT_SMOKE_READY" -ne 1 ]; then
    echo "Smoke-test server did not become healthy at $MACBOT_SMOKE_URL within $((MACBOT_SMOKE_ATTEMPTS / 4))s." >&2
    echo "The process is still alive, so this is a slow start rather than a crash." >&2
    echo "Raise MACBOT_SMOKE_ATTEMPTS if this machine is simply slow. Server log:" >&2
    sed -n '1,200p' "$MACBOT_SMOKE_TMP/server.log" >&2
    exit 1
fi

health_json="$(curl --max-time 5 --fail --silent "$MACBOT_SMOKE_URL/api/health")"
ready_json="$(curl --max-time 5 --fail --silent "$MACBOT_SMOKE_URL/api/ready")"
curl --max-time 5 --fail --silent "$MACBOT_SMOKE_URL/" >/dev/null

python3 -c 'import json,sys; payload=json.loads(sys.argv[1]); assert payload.get("status") == "ok"' "$health_json"
python3 -c 'import json,sys; payload=json.loads(sys.argv[1]); assert isinstance(payload, dict) and payload' "$ready_json"

# A dashboard read on a fresh, feed-free process must return absent analysis,
# not start network analysis and hold every other panel behind that request.
curl --max-time 5 --fail --silent -H 'x-api-key: local' "$MACBOT_SMOKE_URL/api/analysis" > "$MACBOT_SMOKE_TMP/analysis.json"
python3 -c 'import json,sys; p=json.load(open(sys.argv[1])); assert p["analysis_freshness"]["never_run"] is True; assert p["analysis_freshness"]["age_secs"] is None' "$MACBOT_SMOKE_TMP/analysis.json"
curl --max-time 5 --fail --silent -H 'x-api-key: local' "$MACBOT_SMOKE_URL/api/btc-chart" > "$MACBOT_SMOKE_TMP/btc-chart.json"
python3 -c 'import json,sys; p=json.load(open(sys.argv[1])); assert p["candles"] == []; assert p["current_price"] is None' "$MACBOT_SMOKE_TMP/btc-chart.json"

# Exercise the operator's authenticated read/export path against the real process.
unauth_code="$(curl --max-time 5 --silent -o /dev/null -w '%{http_code}' "$MACBOT_SMOKE_URL/api/recovery")"
[ "$unauth_code" = "401" ] || { echo "Recovery evidence must require authentication" >&2; exit 1; }
curl --max-time 5 --fail --silent -H 'x-api-key: local' "$MACBOT_SMOKE_URL/api/recovery" > "$MACBOT_SMOKE_TMP/recovery.json"
python3 -c 'import json,sys; p=json.load(open(sys.argv[1])); assert p["schema_version"] == 8; assert "live_orders" in p["evidence"]; assert "realized_pnl_events" in p["evidence"]; assert "redemption_receipts" in p["evidence"]; assert "api_secret" not in p' "$MACBOT_SMOKE_TMP/recovery.json"
MACBOT_API_KEY=local "${MACBOT_VERIFY_PYTHON:-python3}" scripts/recovery_report.py \
    --base-url "$MACBOT_SMOKE_URL" --order-id unknown-order --out "$MACBOT_SMOKE_TMP/order-evidence.json"

MACBOT_SMOKE_URL="$MACBOT_SMOKE_URL" PYTHONPATH=scripts "${MACBOT_VERIFY_PYTHON:-python3}" - <<'PY'
import os
from api_auth import post_json, get_json
base = os.environ['MACBOT_SMOKE_URL']
for route in ['/api/loop/start', '/api/analyze', '/api/trade']:
    try:
        post_json(base + route, {}, timeout=5, api_key='local', api_secret='localsecret', require_auth=True)
    except RuntimeError as exc:
        assert 'HTTP 409' in str(exc), (route, str(exc))
        assert 'research-only demo' in str(exc)
    else:
        raise AssertionError(f'{route} unexpectedly accepted in a feed-free demo')
state = get_json(base + '/api/loop/status', timeout=5, read_auth=True)
assert state['backtest_only'] and not state['running']
for route in ['/api/rpc/send-raw', '/api/rpc/priority-fee', '/api/maker/quote', '/api/maker/clear-market', '/api/maker/inventory']:
    try:
        post_json(base + route, {'raw_tx': '0x01'}, timeout=5, api_key='local', api_secret='localsecret', require_auth=True)
    except RuntimeError as exc:
        assert 'HTTP 404' in str(exc), (route, str(exc))
    else:
        raise AssertionError(f'{route} unexpectedly exposed an untracked RPC operation')
print('Signed demo controls refuse analysis and execution without false running state.')
PY


kill -TERM "$MACBOT_SMOKE_PID"
if ! wait "$MACBOT_SMOKE_PID"; then
    MACBOT_SMOKE_PID=""
    echo "Smoke-test server returned an error during graceful shutdown" >&2
    sed -n '1,240p' "$MACBOT_SMOKE_TMP/server.log" >&2
    exit 1
fi
MACBOT_SMOKE_PID=""
if ! grep -q "Execution actor drained and stopped" "$MACBOT_SMOKE_TMP/server.log" \
   || ! grep -q "Shutdown complete" "$MACBOT_SMOKE_TMP/server.log"; then
    echo "Smoke-test server exited without proving actor drain and shutdown completion" >&2
    sed -n '1,240p' "$MACBOT_SMOKE_TMP/server.log" >&2
    exit 1
fi

# No background task may die during a clean run.
#
# rustls 0.23 refuses to choose a crypto provider when both aws-lc-rs and ring
# are in the dependency graph. Before a provider was installed explicitly, the
# l2_ingestion, price_bus and liquidation_feed tasks each panicked on their
# first TLS handshake about fourteen seconds after startup, and because
# l2_ingestion is monitored as critical the task monitor engaged the kill
# switch. The bot halted itself on every run with the order book empty, and
# nothing in the logs said the halt and the panic were the same event. This
# check exists so that cannot recur unnoticed.
if grep -q "panicked" "$MACBOT_SMOKE_TMP/server.log"; then
    echo "Smoke-test server logged a panic during a clean run:" >&2
    grep -n -A3 "panicked" "$MACBOT_SMOKE_TMP/server.log" | sed -n '1,40p' >&2
    exit 1
fi
if grep -q "CryptoProvider" "$MACBOT_SMOKE_TMP/server.log"; then
    echo "Smoke-test server could not select a TLS provider, so every TLS task will die:" >&2
    grep -n -B2 -A6 "CryptoProvider" "$MACBOT_SMOKE_TMP/server.log" | sed -n '1,40p' >&2
    exit 1
fi
if grep -q "Halting trading" "$MACBOT_SMOKE_TMP/server.log"; then
    echo "Smoke-test server halted trading during a clean run:" >&2
    grep -n "Halting trading" "$MACBOT_SMOKE_TMP/server.log" >&2
    exit 1
fi

echo "Backtest-only smoke test passed at $MACBOT_SMOKE_URL."
