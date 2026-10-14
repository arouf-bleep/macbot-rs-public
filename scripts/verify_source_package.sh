#!/usr/bin/env bash
# Verify that a source export is safe to hand to a third party.
#
# Works on a git checkout and on a plain extracted archive. In a git repository
# the export is taken to be the tracked files; otherwise it is every file
# present. Checks the distribution boundary: private state absent, no credentials
# or personal paths, required technical documentation present, canonical model intact.
#
# Usage:
#   ./scripts/verify_source_package.sh              # full run
#   ./scripts/verify_source_package.sh --quick      # skip the compile/test pass
#   ./scripts/verify_source_package.sh /path/to/export
set -euo pipefail

SELF_REL="scripts/verify_source_package.sh"
RUN_VERIFY=1
TARGET=""

for arg in "$@"; do
    case "$arg" in
        --quick) RUN_VERIFY=0 ;;
        -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
        *) TARGET="$arg" ;;
    esac
done

if [ -n "$TARGET" ]; then
    cd "$TARGET"
else
    cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fi
EXPORT_ROOT="$(pwd)"
if [ -n "${MACBOT_VERIFY_TARGET_DIR:-}" ]; then
    # Both the main verifier and the subsequent replay must use this cache.
    export CARGO_TARGET_DIR="$MACBOT_VERIFY_TARGET_DIR"
fi

FAILURES=0
WARNINGS=0
fail() { printf 'FAIL  %s\n' "$*"; FAILURES=$((FAILURES + 1)); }
warn() { printf 'WARN  %s\n' "$*"; WARNINGS=$((WARNINGS + 1)); }
pass() { printf 'ok    %s\n' "$*"; }

HAS_GIT=0
if [ -e "$EXPORT_ROOT/.git" ] && git -C "$EXPORT_ROOT" rev-parse --git-dir >/dev/null 2>&1; then
    HAS_GIT=1
fi

# The set of files the operator actually receives.
if [ "$HAS_GIT" -eq 1 ]; then
    EXPORT_FILES="$(git ls-files)"
    SOURCE_DESC="git-tracked files"
else
    EXPORT_FILES="$(find . -type f -not -path './.git/*' | sed 's|^\./||' | sort)"
    SOURCE_DESC="every file present (no git metadata)"
fi

if [ -z "$EXPORT_FILES" ]; then
    echo "FAIL  no files found at $EXPORT_ROOT"
    exit 1
fi

in_export() { printf '%s\n' "$EXPORT_FILES" | grep -qxF "$1"; }
matching()  { printf '%s\n' "$EXPORT_FILES" | grep -E "$1" || true; }
grep_export() {
    printf '%s\n' "$EXPORT_FILES" \
        | grep -vxF "$SELF_REL" \
        | tr '\n' '\0' \
        | xargs -0 grep -nIE -- "$1" 2>/dev/null \
        | grep -v '^Binary file' || true
}

echo "== Source package contents =="
FILE_COUNT="$(printf '%s\n' "$EXPORT_FILES" | wc -l | tr -d ' ')"
TOTAL_BYTES="$(printf '%s\n' "$EXPORT_FILES" | python3 -c 'import pathlib,sys; total=sum(pathlib.Path(p.rstrip("\n")).stat().st_size for p in sys.stdin); print(f"{total / 1048576:.1f}")')"
printf '      root:    %s\n' "$EXPORT_ROOT"
printf '      source:  %s\n' "$SOURCE_DESC"
printf '      files:   %s  (%s MiB)\n' "$FILE_COUNT" "$TOTAL_BYTES"
printf '      layout:  %s\n' "$(printf '%s\n' "$EXPORT_FILES" | awk -F/ 'NF>1{print $1"/"} NF==1{print "(root)"}' \
    | sort -u | tr '\n' ' ')"

echo
echo "== Excluded assets =="
# Pattern, then a human description. These must never reach a operator.
check_absent() {
    local label="$1" pattern="$2"
    local hits
    hits="$(matching "$pattern")"
    if [ -n "$hits" ]; then
        fail "$label present in the export:"
        printf '        %s\n' $hits
    else
        pass "no $label"
    fi
}
# .env.example is the one environment file that should ship; every other
# .env variant carries real credentials.
env_hits="$(matching '(^|/)\.env($|\.)' | grep -vxF '.env.example' || true)"
if [ -n "$env_hits" ]; then
    fail "environment files present in the export:"
    printf '        %s\n' $env_hits
else
    pass "no environment files beyond .env.example"
fi
check_absent "databases"              '\.(db|sqlite)(-wal|-shm)?$'
check_absent "key material"           '\.(key|pem|p12|pfx)$'
check_absent "raw datasets"           '^data/'
check_absent "generated artifacts"    '^artifacts/'
check_absent "build output"           '^target'
check_absent "private notes" '^notes/'
check_absent "local tool metadata"    '^\.claude/|^\.playwright-cli/'
check_absent "mutable runtime state" '(^|/)(ml_weights|ensemble_state)\.json$|^output/|(^|/)(__pycache__|\.venv[^/]*|node_modules)/'

# .env.example is the one environment file that should ship.
if in_export ".env.example"; then
    pass ".env.example present (the template that should ship)"
else
    warn ".env.example is missing; the operator has no environment template"
fi

echo
echo "== Secret and personal-data scan =="
# A 64-hex-digit literal has the shape of an Ethereum private key. The CLOB auth
# test uses a well-known dummy, excluded here.
hits="$(grep_export '0x[0-9a-fA-F]{64}' \
    | grep -v '0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef' || true)"
if [ -n "$hits" ]; then
    fail "possible private key literals:"
    printf '%s\n' "$hits" | awk -F: '{print "        " $1 ":" $2 " (value redacted)"}' | head -10
else
    pass "no private key literals outside the known test fixture"
fi

scan() {
    local label="$1" pattern="$2" hits
    hits="$(grep_export "$pattern")"
    if [ -n "$hits" ]; then
        fail "$label:"
        printf '%s\n' "$hits" | awk -F: '{print "        " $1 ":" $2 " (value redacted)"}' | head -10
    else
        pass "no $label"
    fi
}
scan "AWS-style access keys"          'AKIA[0-9A-Z]{16}'
scan "private key headers"           '-----BEGIN (RSA |EC |OPENSSH )?PRIVATE KEY-----'
scan "GitHub-style tokens"           '(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{30,}'
scan "OpenAI-style tokens"           'sk-(proj-)?[A-Za-z0-9_-]{40,}'
scan "bearer/authorization literals"  '(Bearer|Authorization:)[[:space:]]+[A-Za-z0-9._-]{24,}'
scan "home directory paths"           '/(Users|home)/[a-zA-Z0-9._-]+/'
scan "personal email addresses"       '[a-zA-Z0-9._%+-]+@(gmail|icloud|outlook|hotmail|yahoo)\.com'

echo
echo "== operator documentation =="
for doc in README.md LICENSE.md SECURITY.md THIRD_PARTY_NOTICES.md \
           docs/ARCHITECTURE.md docs/QUICKSTART.md docs/KNOWN_LIMITATIONS.md \
           docs/SOURCE_DISTRIBUTION.md docs/DEPENDENCY_DISCLOSURE.md docs/MODEL_ARTIFACT.md \
           ops/recovery-runbook.md training/requirements-verify.lock \
           models/provenance.json static/vendor/licenses/bootstrap.txt \
           static/vendor/licenses/bootstrap-icons.txt static/vendor/licenses/chartjs.txt \
           static/vendor/licenses/space-grotesk.txt static/vendor/licenses/ibm-plex-mono.txt; do
    if in_export "$doc"; then pass "present: $doc"; else fail "missing: $doc"; fi
done


echo
echo "== Placeholder credentials =="
if [ -f config.json ] \
   && grep -qE '"api_(key|hmac_secret)":[[:space:]]*"(local|localsecret)"' config.json; then
    pass "config.json uses documented public local-demo credentials; live mode refuses them"
else
    pass "config.json carries no known placeholder credentials"
fi

echo
echo "== Canonical model =="
if in_export "models/latest.onnx" && [ -f models/latest.onnx ]; then
    if command -v shasum >/dev/null 2>&1; then
        pass "models/latest.onnx  sha256=$(shasum -a 256 models/latest.onnx | cut -d' ' -f1)"
    else
        pass "models/latest.onnx present"
    fi
else
    fail "models/latest.onnx is missing; the runtime contract requires it"
fi

echo
echo "== Reproducibility =="
if [ "$HAS_GIT" -eq 1 ]; then
    if [ -n "$(git status --porcelain)" ]; then
        warn "working tree is dirty; export from a clean tagged commit, not this directory"
    else
        pass "working tree is clean  commit=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
    fi
else
    pass "no git metadata in the export (expected for an extracted archive)"
fi

for f in Cargo.lock rust-toolchain.toml; do
    if in_export "$f"; then pass "present: $f"; else fail "missing: $f (build is not reproducible)"; fi
done

echo
echo "== Project verification =="
if [ "$RUN_VERIFY" -eq 1 ]; then
    if [ -x scripts/verify.sh ]; then
        if "${MACBOT_VERIFY_PYTHON:-python3}" -B scripts/check_verification_environment.py \
            && MACBOT_VERIFY_REQUIRE_TRAINING=1 ./scripts/verify.sh; then
            pass "scripts/verify.sh passed"
            if MACBOT_PARITY_PYTHON="${MACBOT_VERIFY_PYTHON:-python3}" ./scripts/verify_decision_parity.sh; then
                pass "synthetic decision replay and Python/ONNX/Rust model parity passed"
            else
                fail "decision/model parity verification failed"
            fi
        else
            fail "scripts/verify.sh failed"
        fi
    else
        fail "scripts/verify.sh is missing or not executable"
    fi
else
    warn "skipped scripts/verify.sh (--quick); run without --quick before distribution"
fi

echo
echo "======================================"
printf 'failures: %d   warnings: %d\n' "$FAILURES" "$WARNINGS"
if [ "$FAILURES" -gt 0 ]; then
    echo "Technical source package verification failed."
    exit 1
fi
echo "Technical source package checks passed. Licence terms remain in LICENSE.md."
