#!/usr/bin/env bash
# Retain synthetic captures from real production functions and compare them
# offline, then check actual Python -> ONNX -> Rust predictions.
set -euo pipefail
MACBOT_PARITY_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MACBOT_PARITY_OUT="${1:-$(mktemp -d "${TMPDIR:-/tmp}/macbot-parity.XXXXXX")}"
mkdir -p "$MACBOT_PARITY_OUT"
MACBOT_PARITY_OUT="$(cd "$MACBOT_PARITY_OUT" && pwd)"
MACBOT_PARITY_PYTHON="${MACBOT_PARITY_PYTHON:-python3}"
cd "$MACBOT_PARITY_ROOT"
MACBOT_ONNX_RUNTIME_LIB="$("$MACBOT_PARITY_PYTHON" -B scripts/setup_onnx_runtime.py)"
export MACBOT_ONNX_RUNTIME_LIB
if [ -e "$MACBOT_PARITY_OUT/decisions.jsonl" ]; then
    echo "Use a fresh output directory; refusing to mix old and new captures." >&2
    exit 1
fi
export PYTHONPYCACHEPREFIX="$MACBOT_PARITY_OUT/pycache"
export MACBOT_ML_WEIGHTS_PATH="$MACBOT_PARITY_OUT/ml_weights.json"
export MACBOT_ENSEMBLE_PATH="$MACBOT_PARITY_OUT/ensemble_state.json"
export MACBOT_ALERT_FALLBACK_PATH="$MACBOT_PARITY_OUT/alerts.jsonl"
# Test-generated alerts stay local and cannot reach configured real recipients.
unset MACBOT_DISCORD_WEBHOOK_URL MACBOT_TELEGRAM_BOT_TOKEN MACBOT_TELEGRAM_CHAT_ID
MACBOT_DECISION_CAPTURE_PATH="$MACBOT_PARITY_OUT/decisions.jsonl" \
    cargo test --locked --lib --all-features --quiet
cargo run --locked --example replay_decisions -- \
    "$MACBOT_PARITY_OUT/decisions.jsonl" "$MACBOT_PARITY_OUT/replay-report.json"
"$MACBOT_PARITY_PYTHON" training/export_runtime_fixture.py --out "$MACBOT_PARITY_OUT/models"
cargo run --locked --example check_model_runtime -- \
    "$MACBOT_PARITY_OUT/models/runtime-fixture.json" "$MACBOT_PARITY_OUT/model-report.json"
echo "Synthetic decision and model parity checks passed. Evidence: $MACBOT_PARITY_OUT"
