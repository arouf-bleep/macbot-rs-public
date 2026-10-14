#!/usr/bin/env bash
set -euo pipefail

MACBOT_REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MACBOT_DEMO_DIR="$(mktemp -d "${TMPDIR:-/tmp}/macbot-demo.XXXXXX")"

MACBOT_DEMO_PID=""
cleanup() {
    if [ -n "$MACBOT_DEMO_PID" ] && kill -0 "$MACBOT_DEMO_PID" 2>/dev/null; then
        kill -TERM "$MACBOT_DEMO_PID" 2>/dev/null || true
        for _ in $(seq 1 120); do
            kill -0 "$MACBOT_DEMO_PID" 2>/dev/null || break
            sleep 0.25
        done
        if kill -0 "$MACBOT_DEMO_PID" 2>/dev/null; then
            kill -KILL "$MACBOT_DEMO_PID" 2>/dev/null || true
        fi
        wait "$MACBOT_DEMO_PID" 2>/dev/null || true
    fi
    rm -rf -- "$MACBOT_DEMO_DIR"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

cp "$MACBOT_REPO_ROOT/config.json" "$MACBOT_DEMO_DIR/config.json"
export MACBOT_CONFIG="$MACBOT_DEMO_DIR/config.json"
export MACBOT_DOTENV_PATH="$MACBOT_DEMO_DIR/.env"
export MACBOT_DB="$MACBOT_DEMO_DIR/demo.sqlite"
export MACBOT_ML_WEIGHTS_PATH="$MACBOT_DEMO_DIR/ml_weights.json"
export MACBOT_ENSEMBLE_PATH="$MACBOT_DEMO_DIR/ensemble_state.json"
export MACBOT_CONFIG_SNAPSHOT_DIR="$MACBOT_DEMO_DIR/config-snapshots"
export MACBOT_ALERT_FALLBACK_PATH="$MACBOT_DEMO_DIR/alerts.jsonl"
export MACBOT_MODELS_DIR="$MACBOT_REPO_ROOT/models"
export MACBOT_BACKTEST_ONLY=1
export MACBOT_STARTUP_ANALYSIS=0
export MACBOT_BIND_ALL=0
export MACBOT_PORT="${MACBOT_PORT:-8000}"
export MACBOT_API_KEY=local
export MACBOT_API_SECRET=localsecret
export MACBOT_API_HMAC_SECRET=localsecret
export RUST_LOG="${RUST_LOG:-macbot=info}"

cd "$MACBOT_REPO_ROOT"
MACBOT_ONNX_RUNTIME_LIB="$("${MACBOT_DEMO_PYTHON:-python3.11}" -B scripts/setup_onnx_runtime.py)"
export MACBOT_ONNX_RUNTIME_LIB
echo "Starting the research-only demo at http://127.0.0.1:${MACBOT_PORT}/#api_key=local&api_secret=localsecret"
echo "Temporary state: $MACBOT_DEMO_DIR"
if [ "${MACBOT_DEMO_RELEASE:-0}" = "1" ]; then
    cargo build --locked --release --bin macbot
    MACBOT_DEMO_PROFILE=release
else
    cargo build --locked --bin macbot
    MACBOT_DEMO_PROFILE=debug
fi
MACBOT_DEMO_TARGET="$(cargo metadata --locked --format-version 1 --no-deps \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
"$MACBOT_DEMO_TARGET/$MACBOT_DEMO_PROFILE/macbot" &
MACBOT_DEMO_PID="$!"
wait "$MACBOT_DEMO_PID"
