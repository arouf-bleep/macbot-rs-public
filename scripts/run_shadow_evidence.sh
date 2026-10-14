#!/usr/bin/env bash
# Run one pre-registered shadow evidence window.
#
# Enforces the freeze described in ops/shadow-evidence-protocol.md: the profile
# and model must hash to the pre-registered values, or the run refuses to start.
# A window whose inputs drifted is not evidence about one strategy.
#
# Execution is disabled. This never places an order and needs no credentials.
#
# Usage: ./scripts/run_shadow_evidence.sh ARTIFACT_DIR
set -euo pipefail

MACBOT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$MACBOT_ROOT"

PROFILE="ops/profiles/shadow_trend_up_yes_frozen.json"
MODEL="models/latest.onnx"
EXPECTED_PROFILE_SHA="30a76baca8f13050c350e20dacfbc8a03067a85fdee9523af8710120c227c602"
EXPECTED_MODEL_SHA="85ecf8da7b4f0191a9694ef2b27ed2aabd3885d92bf4f2491a3ce6e5fa906f8c"

OUT="${1:-}"
if [ -z "$OUT" ]; then
    echo "usage: $0 ARTIFACT_DIR" >&2
    exit 2
fi
if [ -e "$OUT/run-manifest.json" ]; then
    echo "FAIL  $OUT already holds a window. Use a fresh directory; mixing windows destroys the sample." >&2
    exit 1
fi
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"

sha() { shasum -a 256 "$1" | cut -d' ' -f1; }

PROFILE_SHA="$(sha "$PROFILE")"
MODEL_SHA="$(sha "$MODEL")"

if [ "$PROFILE_SHA" != "$EXPECTED_PROFILE_SHA" ]; then
    echo "FAIL  profile hash drifted from the pre-registered value." >&2
    echo "      expected $EXPECTED_PROFILE_SHA" >&2
    echo "      actual   $PROFILE_SHA" >&2
    echo "      Editing the frozen profile voids the window. Start a new one." >&2
    exit 1
fi
if [ "$MODEL_SHA" != "$EXPECTED_MODEL_SHA" ]; then
    echo "FAIL  model hash drifted from the pre-registered value." >&2
    echo "      expected $EXPECTED_MODEL_SHA" >&2
    echo "      actual   $MODEL_SHA" >&2
    exit 1
fi

RUN_ID="shadow-$(date -u +%Y%m%dT%H%M%SZ)"
cat > "$OUT/run-manifest.json" <<JSON
{
  "run_id": "$RUN_ID",
  "started_at_utc": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "protocol": "ops/shadow-evidence-protocol.md",
  "profile": "$PROFILE",
  "profile_sha256": "$PROFILE_SHA",
  "model": "$MODEL",
  "model_sha256": "$MODEL_SHA",
  "execution": "disabled (shadow)",
  "git_commit": "$(git rev-parse HEAD 2>/dev/null || echo unknown)",
  "git_dirty": $( [ -n "$(git status --porcelain 2>/dev/null)" ] && echo true || echo false )
}
JSON

cp "$PROFILE" "$OUT/config.json"
export MACBOT_CONFIG="$OUT/config.json"
export MACBOT_DB="$OUT/shadow.sqlite"
export MACBOT_ML_WEIGHTS_PATH="$OUT/ml_weights.json"
export MACBOT_ENSEMBLE_PATH="$OUT/ensemble_state.json"
export MACBOT_CONFIG_SNAPSHOT_DIR="$OUT/config-snapshots"
export MACBOT_ALERT_FALLBACK_PATH="$OUT/alerts.jsonl"
export MACBOT_MODELS_DIR="$MACBOT_ROOT/models"
export MACBOT_DECISION_CAPTURE_PATH="$OUT/decisions.jsonl"
export MACBOT_BIND_ALL=0
export MACBOT_PORT="${MACBOT_PORT:-8010}"
export RUST_LOG="${RUST_LOG:-macbot=info}"
# Shadow mode is the execution guard. Credentials are intentionally absent.
unset PRIVATE_KEY POLYMARKET_API_KEY POLYMARKET_SECRET POLYMARKET_PASSPHRASE || true

echo "Shadow evidence window $RUN_ID"
echo "  protocol:  ops/shadow-evidence-protocol.md"
echo "  artifacts: $OUT"
echo "  dashboard: http://127.0.0.1:${MACBOT_PORT}"
echo "  execution: DISABLED"
echo
echo "Do not edit the profile or model while this runs. Evaluate only after"
echo "200 closed trades across 60 markets and 21 days, against the criteria"
echo "fixed in the protocol."
echo

cargo run --locked --release
