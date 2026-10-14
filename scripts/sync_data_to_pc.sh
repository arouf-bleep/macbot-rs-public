#!/usr/bin/env bash
# sync_data_to_pc.sh — Push ML training data from Mac to PC for GPU training.
#
# Usage:
#   ./scripts/sync_data_to_pc.sh [PC_HOST]
#
# Requires: SSH key auth to PC.
# Syncs: SQLite DB + training code → PC's macbot-training/

set -euo pipefail

MACBOT_SYNC_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$MACBOT_SYNC_ROOT"

PC_HOST="${1:-${PC_HOST:-gaming-pc}}"
REMOTE_ROOT="${MACBOT_TRAINING_REMOTE_ROOT:-C:/macbot-training}"
LOCAL_DB="${MACBOT_DB:-./trades.db}"
LOCAL_TRAINING="./training"

echo "── Syncing ML data to ${PC_HOST} ──"

# Create remote directory structure
ssh "$PC_HOST" "powershell -NoProfile -Command \"New-Item -ItemType Directory -Force -Path '${REMOTE_ROOT}/data','${REMOTE_ROOT}/training','${REMOTE_ROOT}/models' | Out-Null\""

# A live SQLite database may contain committed rows only in its WAL. Transfer
# a verified snapshot, never the main file alone while the bot is writing.
if [ -f "$LOCAL_DB" ]; then
    echo "Syncing database..."
    SYNC_STAGE="$(mktemp -d "${TMPDIR:-/tmp}/macbot-training-sync.XXXXXX")"
    trap 'rm -rf "$SYNC_STAGE"' EXIT
    "${MACBOT_SYNC_PYTHON:-python3}" scripts/sqlite_backup.py --source "$LOCAL_DB" --dest "$SYNC_STAGE/macbot.db"
    scp "$SYNC_STAGE/macbot.db" "${PC_HOST}:${REMOTE_ROOT}/data/macbot.db"
else
    echo "Warning: DB not found at ${LOCAL_DB}. Set MACBOT_DB env var."
fi

# Sync training code file by file
echo "Syncing training code..."
for f in "$LOCAL_TRAINING"/*; do
    [ -f "$f" ] || continue
    fname=$(basename "$f")
    echo "  $fname"
    scp "$f" "${PC_HOST}:${REMOTE_ROOT}/training/${fname}"
done

echo "── Sync complete ──"
echo "On the training PC, open ${REMOTE_ROOT} and run:"
echo "  python training/train.py --db data/macbot.db --out models/ --device cuda"
