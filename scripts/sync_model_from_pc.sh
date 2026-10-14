#!/usr/bin/env bash
# sync_model_from_pc.sh — Pull trained ONNX model from PC back to Mac.
#
# Usage:
#   ./scripts/sync_model_from_pc.sh [PC_HOST]
#
# Pulls latest.onnx + versioned model from PC → local models/ directory.
# The Rust ModelWatcher will auto-detect the new latest.onnx and hot-swap.

set -euo pipefail

MACBOT_SYNC_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$MACBOT_SYNC_ROOT"

PC_HOST="${1:-${PC_HOST:-gaming-pc}}"
REMOTE_ROOT="${MACBOT_TRAINING_REMOTE_ROOT:-C:/macbot-training}"
REMOTE_DIR="${REMOTE_ROOT}/models"
LOCAL_MODELS="./models"

echo "── Pulling trained model from ${PC_HOST} ──"

mkdir -p "$LOCAL_MODELS"
MODEL_STAGE="$(mktemp -d "$LOCAL_MODELS/.model-sync.XXXXXX")"
trap 'rm -rf "$MODEL_STAGE"' EXIT

# Pull all .onnx files
scp "${PC_HOST}:${REMOTE_DIR}/*.onnx" "$MODEL_STAGE/"

if [ -s "$MODEL_STAGE/latest.onnx" ]; then
    # Publish complete files from the same filesystem; the watcher must never
    # observe a half-written transfer at its active model path.
    for model in "$MODEL_STAGE"/*.onnx; do
        [ -s "$model" ] || { echo "Refusing empty model: $model" >&2; exit 1; }
        # Graph validity is not a sandbox. Accept trusted, self-contained models
        # only; do not let tensors hidden in subgraphs read sidecar files.
        "${MACBOT_SYNC_PYTHON:-python3}" training/model_artifact.py "$model"
    done
    for model in "$MODEL_STAGE"/*.onnx; do
        mv -f "$model" "$LOCAL_MODELS/$(basename "$model")"
    done
    echo "── Model pulled successfully ──"
    echo "latest.onnx size: $(du -h "${LOCAL_MODELS}/latest.onnx" | cut -f1)"
    echo "The model watcher will validate the model before replacing its current model. Check its load result in the bot log."
else
    echo "Refusing transfer: no non-empty latest.onnx received. Active models were preserved." >&2
    exit 1
fi
