#!/usr/bin/env bash
set -euo pipefail

MACBOT_REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MACBOT_VERIFY_TMP="$(mktemp -d "${TMPDIR:-/tmp}/macbot-verify.XXXXXX")"

cleanup() {
    rm -rf -- "$MACBOT_VERIFY_TMP"
}
trap cleanup EXIT INT TERM

if [ -n "${MACBOT_VERIFY_TARGET_DIR:-}" ]; then
    export CARGO_TARGET_DIR="$MACBOT_VERIFY_TARGET_DIR"
fi
export PYTHONPYCACHEPREFIX="$MACBOT_VERIFY_TMP/pycache"
# Tests must not leave mutable state in a operator's extracted source directory.
export MACBOT_ML_WEIGHTS_PATH="$MACBOT_VERIFY_TMP/ml_weights.json"
export MACBOT_ENSEMBLE_PATH="$MACBOT_VERIFY_TMP/ensemble_state.json"
export MACBOT_ALERT_FALLBACK_PATH="$MACBOT_VERIFY_TMP/alerts.jsonl"
# Test-generated alerts stay local and cannot reach configured real recipients.
unset MACBOT_DISCORD_WEBHOOK_URL MACBOT_TELEGRAM_BOT_TOKEN MACBOT_TELEGRAM_CHAT_ID
MACBOT_VERIFY_PYTHON="${MACBOT_VERIFY_PYTHON:-python3}"

cd "$MACBOT_REPO_ROOT"
MACBOT_ONNX_RUNTIME_LIB="$("$MACBOT_VERIFY_PYTHON" scripts/setup_onnx_runtime.py)"
export MACBOT_ONNX_RUNTIME_LIB

cargo fmt --all -- --check
cargo check --locked --all-targets --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
"$MACBOT_VERIFY_PYTHON" -m py_compile scripts/*.py tests/python/*.py training/*.py
"$MACBOT_VERIFY_PYTHON" scripts/check_ml_contract.py
"$MACBOT_VERIFY_PYTHON" - <<'PY'
import os
import sys
import unittest

suite = unittest.defaultTestLoader.discover('training', pattern='test_*.py')
result = unittest.TextTestRunner(verbosity=1).run(suite)
if not result.wasSuccessful():
    sys.exit(1)
if os.environ.get('MACBOT_VERIFY_REQUIRE_TRAINING') == '1' and result.skipped:
    print('Full project verification requires training dependencies; skipped model tests are blocking.', file=sys.stderr)
    sys.exit(1)
PY
"$MACBOT_VERIFY_PYTHON" - <<'PYTEST'
import pathlib
import sys
import unittest
sys.path.insert(0, str(pathlib.Path.cwd() / "scripts"))
suite = unittest.defaultTestLoader.discover('tests/python', pattern='test_*.py')
result = unittest.TextTestRunner(verbosity=1).run(suite)
sys.exit(0 if result.wasSuccessful() else 1)
PYTEST
"$MACBOT_VERIFY_PYTHON" scripts/check_project_files.py
node scripts/check_inline_js.mjs
node scripts/check_analysis_fields.mjs
node tests/dashboard/test_dashboard_freshness.mjs
node tests/dashboard/test_dashboard_absent_values.mjs
node tests/dashboard/test_dashboard_auth.mjs
node tests/dashboard/test_dashboard_security.mjs
node tests/dashboard/test_dashboard_sse_retry.mjs
node tests/dashboard/test_dashboard_read_timeout.mjs
node tests/dashboard/test_dashboard_live_book.mjs
node tests/dashboard/test_dashboard_unresolved_exit.mjs
node tests/dashboard/test_dashboard_accounting.mjs
bash -n scripts/*.sh
./scripts/smoke_test.sh

echo "Project verification passed."
