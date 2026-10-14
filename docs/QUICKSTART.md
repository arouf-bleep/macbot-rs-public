# Quick start

## Read-only operator preview

For an immediate look at the dashboard, only Python 3.11's standard library is needed:

```bash
python3.11 -B scripts/preview_dashboard.py
```

Open the printed URL, normally <http://127.0.0.1:18082/>. The banner switches between mixed sample wins/losses, a stale-feed halt and an unresolved exit. Controls are read-only, and no bot, model, database, credentials or exchange is loaded. Prices, trades, signals and fees are synthetic illustrations, not real-engine evidence or a performance claim. Stop with `Ctrl-C`; use `--port 18083` if the default port is occupied.

The following prerequisites and installation steps are for the Rust engine demo and full source verification. They are not needed for this interface preview.

## Prerequisites

- Rust 1.94.0 through `rustup`
- Python 3.11.17 or newer within the 3.11 series for the pinned training/verification environment
- macOS 14 or newer on ARM64, or Linux x64/ARM64 where listed in `scripts/onnx_runtime_manifest.json`; actual Ubuntu 26.04.1 ARM64 acceptance is recorded; Linux x64 and Windows remain unverified
- Node.js 22 or newer for dashboard JavaScript validation; Node 22 LTS is the documented baseline
- Internet access during the first build for Rust crates and the ONNX Runtime binary

The Rust service embeds pinned SQLite 3.51.3; a separate database server is not required. Python database-writing tools use the interpreter's own SQLite and also require 3.51.3 or newer.

Python HTTPS tools require a patched OpenSSL branch: 3.4.8+, 3.5.9+, 3.6.5+ or 4.0.3+. Updating Python packages alone does not update its native TLS library. The verification gate checks the actual loaded versions; keep the interpreter and operating system patched too.

The demo and verification wrappers install the checksum-pinned official CPU ONNX Runtime into a user cache outside the source tree. Set `MACBOT_ONNX_RUNTIME_CACHE` to an absolute external cache directory if needed. Downloads require internet access on first use; cached bytes are verified again. The manifest is the authoritative list of supplied targets; a listed download is not proof that the full application has passed on that platform.

The demo installer uses `python3.11` by default. Set `MACBOT_DEMO_PYTHON` to the absolute path of your patched validation interpreter when using a virtual environment.

Before running the demo, check the same interpreter you intend to use:

```bash
python3.11 -B scripts/check_environment.py
```

This is a read-only prerequisite check. It does not install packages, download a runtime, read credentials, start the bot or verify Linux acceptance. The normal check permits an uninstalled ONNX cache because the demo installs it explicitly on first use. `--full` additionally requires the verified cached runtime and all pinned validation packages.

## Fresh Ubuntu setup

These are installation instructions for an updated **Ubuntu 24.04 LTS** machine using Bash, from the extracted source directory. This Ubuntu 24.04 recipe has not been executed in the retained acceptance evidence. A fresh Ubuntu 26.04.1 ARM64 VM separately passed pinned native setup, the full gate and installed research lifecycle; see the exact platform/source binding. Use a fresh user-owned tools directory and keep it outside the extracted source. The commands install tools for your user; they do not replace Ubuntu's Python, OpenSSL or SQLite. A listed ARM64 ONNX artifact does not establish that every pinned Python wheel is available on ARM64.

### 1. Install build prerequisites

```bash
set -euo pipefail
sudo apt-get update
sudo apt-get install --no-install-recommends \
  build-essential make perl pkg-config curl ca-certificates xz-utils unzip git \
  libssl-dev zlib1g-dev libbz2-dev libreadline-dev libsqlite3-dev \
  libffi-dev liblzma-dev libncurses-dev uuid-dev tk-dev
```

Choose a directory that does not already contain a prior installation:

```bash
export MACBOT_LOCAL_TOOLS="$HOME/.local/share/macbot-local-tools"
test ! -e "$MACBOT_LOCAL_TOOLS" || { echo 'Choose a fresh MACBOT_LOCAL_TOOLS directory'; exit 1; }
mkdir -p "$MACBOT_LOCAL_TOOLS"
macbot_setup_tmp="$(mktemp -d)"
```

Keep this shell open for the following steps. Downloads and Rust/Python builds require disk space beyond the small source archive.

### 2. Build the pinned Python for this user

Python 3.11.17 is a source-only security release. This recipe checks the XZ archive against the checksum on [the official Python release page](https://www.python.org/downloads/release/python-31117/), then builds against Ubuntu's development headers. The next step selects patched native libraries and probes what this interpreter actually loads.

```bash
curl -q --proto '=https' --proto-redir '=https' --tlsv1.2 --fail --location \
  https://www.python.org/ftp/python/3.11.17/Python-3.11.17.tar.xz \
  --output "$macbot_setup_tmp/Python-3.11.17.tar.xz"
printf '%s  %s\n' \
  bfb74ad39efae27cda510f134ab408e00f9992c56851cfc0b1cdb5646da11599 \
  "$macbot_setup_tmp/Python-3.11.17.tar.xz" | sha256sum --check -
tar -xJf "$macbot_setup_tmp/Python-3.11.17.tar.xz" -C "$macbot_setup_tmp"
(
  set -e
  cd "$macbot_setup_tmp/Python-3.11.17"
  ./configure --prefix="$MACBOT_LOCAL_TOOLS/python-3.11.17" --with-ensurepip=install
  make -j2
  make install
)
export MACBOT_LOCAL_PYTHON="$MACBOT_LOCAL_TOOLS/python-3.11.17/bin/python3.11"
"$MACBOT_LOCAL_PYTHON" --version
```

Stop if a download, checksum or build fails. An older `python3.11` elsewhere on `PATH` does not satisfy the gate.

### 3. Select patched OpenSSL and SQLite

The existing helpers download authenticated release sources, build in temporary directories, and probe the interpreter before publishing their libraries. Each prefix must be new. Their initial download still relies on a maintained operating-system TLS installation.

```bash
macbot_tls_lib="$("$MACBOT_LOCAL_PYTHON" -B scripts/provision_openssl.py \
  --prefix "$MACBOT_LOCAL_TOOLS/openssl-3.5.9" --jobs 2)" || exit 1
export LD_LIBRARY_PATH="$macbot_tls_lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
macbot_sqlite_lib="$("$MACBOT_LOCAL_PYTHON" -B scripts/provision_sqlite.py \
  --prefix "$MACBOT_LOCAL_TOOLS/sqlite-3.51.3")" || exit 1
export LD_LIBRARY_PATH="$macbot_sqlite_lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
"$MACBOT_LOCAL_PYTHON" -c 'import ssl,sqlite3; print(ssl.OPENSSL_VERSION); print(sqlite3.sqlite_version); ssl.create_default_context()'
```

The printed values must be OpenSSL **3.5.9** and SQLite **3.51.3** for this recipe. If a helper cannot replace the interpreter's loaded library, stop and use a compatible dynamically linked interpreter. Package installation does not repair a statically bundled or incompatible native TLS library.

### 4. Install Rust 1.94.0 and Node 22

Use [the official Rust installer](https://rust-lang.github.io/rustup/installation/index.html), with the pinned toolchain and verification components:

```bash
curl -q --proto '=https' --proto-redir '=https' --tlsv1.2 --fail --location \
  https://sh.rustup.rs --output "$macbot_setup_tmp/rustup-init.sh"
sh "$macbot_setup_tmp/rustup-init.sh" -y --profile minimal --default-toolchain 1.94.0
export PATH="$HOME/.cargo/bin:$PATH"
rustup component add --toolchain 1.94.0 rustfmt clippy
```

For Node, select the Ubuntu/glibc binary matching the machine, verify its release checksum, and unpack it into the external tools directory. The current patched Node 22 release is selected from [the official release directory](https://nodejs.org/dist/latest-v22.x/).

```bash
case "$(uname -m)" in
  x86_64) macbot_node_arch=x64 ;;
  aarch64|arm64) macbot_node_arch=arm64 ;;
  *) echo 'This recipe supports x64 or ARM64 only'; exit 1 ;;
esac
curl -q --proto '=https' --proto-redir '=https' --tlsv1.2 --fail --location \
  https://nodejs.org/dist/latest-v22.x/SHASUMS256.txt \
  --output "$macbot_setup_tmp/node-SHASUMS256.txt"
macbot_node_file="$(awk -v arch="$macbot_node_arch" \
  '$2 ~ ("^node-v22\\.[0-9]+\\.[0-9]+-linux-" arch "\\.tar\\.xz$") {print $2; exit}' \
  "$macbot_setup_tmp/node-SHASUMS256.txt")"
test -n "$macbot_node_file" || { echo 'No matching official Node 22 binary'; exit 1; }
macbot_node_version="${macbot_node_file#node-}"
macbot_node_version="${macbot_node_version%-linux-*}"
curl -q --proto '=https' --proto-redir '=https' --tlsv1.2 --fail --location \
  "https://nodejs.org/dist/$macbot_node_version/$macbot_node_file" \
  --output "$macbot_setup_tmp/$macbot_node_file"
(
  cd "$macbot_setup_tmp"
  awk -v file="$macbot_node_file" '$2 == file' node-SHASUMS256.txt | sha256sum --check -
) || exit 1
mkdir "$MACBOT_LOCAL_TOOLS/node22"
tar -xJf "$macbot_setup_tmp/$macbot_node_file" --strip-components=1 -C "$MACBOT_LOCAL_TOOLS/node22"
export PATH="$MACBOT_LOCAL_TOOLS/node22/bin:$PATH"
rustc --version
node --version
```

### 5. Check prerequisites and run the local demo

```bash
export MACBOT_DEMO_PYTHON="$MACBOT_LOCAL_PYTHON"
export MACBOT_ONNX_RUNTIME_CACHE="$MACBOT_LOCAL_TOOLS/runtime-cache"
export MACBOT_VERIFY_TARGET_DIR="$MACBOT_LOCAL_TOOLS/rust-target"
export CARGO_TARGET_DIR="$MACBOT_VERIFY_TARGET_DIR"
"$MACBOT_LOCAL_PYTHON" -B scripts/check_environment.py
./scripts/run_demo.sh
```

Open the **URL printed by the demo**. Open the served `http://127.0.0.1:...` address, not `static/index.html` as a local file: the dashboard needs the service to provide its assets and API. Stop the demo with `Ctrl-C`.

For full verification, continue with [Source package verification](#source-package-verification). Keep the native libraries and external tool directories after setup. You can delete the downloaded/build source directory with `rm -rf -- "$macbot_setup_tmp"` once installation has completed.

In a **new shell**, restore these selections before starting any demo, database tool or verification command:

```bash
export MACBOT_LOCAL_TOOLS="$HOME/.local/share/macbot-local-tools"
export MACBOT_LOCAL_PYTHON="$MACBOT_LOCAL_TOOLS/python-3.11.17/bin/python3.11"
export LD_LIBRARY_PATH="$MACBOT_LOCAL_TOOLS/sqlite-3.51.3/lib:$MACBOT_LOCAL_TOOLS/openssl-3.5.9/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
export PATH="$MACBOT_LOCAL_TOOLS/node22/bin:$HOME/.cargo/bin:$PATH"
export MACBOT_DEMO_PYTHON="$MACBOT_LOCAL_PYTHON"
export MACBOT_ONNX_RUNTIME_CACHE="$MACBOT_LOCAL_TOOLS/runtime-cache"
export MACBOT_VERIFY_TARGET_DIR="$MACBOT_LOCAL_TOOLS/rust-target"
export CARGO_TARGET_DIR="$MACBOT_VERIFY_TARGET_DIR"
```

Use the same custom directory if you chose a different value originally. This shell configuration is essential: switching back to the system interpreter or omitting its native library path can make an otherwise installed setup fail the gate.

## Local installation

After the prerequisite check passes, build a contained local installation in a **new** destination:

```bash
python3.11 -B scripts/install_local.py --destination /path/to/new-directory
/path/to/new-directory/run.sh credentials
/path/to/new-directory/run.sh
```

Use the same selected patched interpreter and external Cargo/native cache paths described above. Setup starts no service, installs no system tools or Python packages, and refuses to replace an existing destination. It copies the engine, reviewed dashboard assets, checksum-pinned native runtime and reproducible untrained demonstration model. It uses the reviewed stopped research configuration rather than copying local settings, and generates independent private dashboard credentials for each installation. Mutable config, database and snapshots live under `state/`; the source archive remains the separate research/development distribution.

The launcher defaults to loopback port8000 and research-only operation. Open the served HTTP address and unlock it with the saved key and secret shown by `run.sh credentials`. This explicit command only reads the private installation's saved configuration; it starts no engine and changes no data. Normal startup logs do not print these keys. Keep its output private. If you explicitly override credentials through the environment or `state/.env`, the saved values reported by this command may differ from the running engine's credentials. `MACBOT_PORT` selects another port. Stop with `Ctrl-C`; this installation preserves its private database. Its explicitly selected `state/.env` prevents discovering another project's environment file in an ancestor directory. Keep private runtime credentials there only after reviewing the documented operation boundaries and setting owner-only permissions.

Dashboard settings saves persist only credentials explicitly supplied in that update. Editing one provider key keeps unrelated environment-only credentials off disk. A blank redacted credential input is left untouched. Clearing the optional `signal_weights_path` and saving sends an empty path, which disables future file reloads and clears the loader's file identity; weights already applied remain active. Selecting another weights file reloads that file even when its modification time is older than the previous selection.

`--offline` requires already installed prerequisites, cached Cargo dependencies and a fully verified runtime cache. Alternatively combine it with `--runtime-archive /path/to/pinned-official-runtime.tgz`. Offline mode never invokes a fallback downloader. Debug builds are the default; `--profile release` explicitly selects an optimized build. Artifact assembly and prerequisite checks do not replace platform acceptance.

## Optional macro calendar renewal

The bundled static calendar is not a subscription. To renew selected CPI, Employment Situation and regular FOMC dates, run:

```bash
python3.11 -B scripts/update_macro_calendar.py --output ops/macro-events.json
```

The updater uses the official BLS iCalendar and Federal Reserve meeting page, retains source hashes, publishes atomically and requires future coverage of each selected event type. It does not cover every event or emergency announcements. Failure preserves the previous file; refreshed documents expire after36hours.

For non-research runtime operation, `macro_event_guard_enabled=true` and `macro_event_auto_refresh=true` opt into hourly renewal in a separate worker. Select the patched interpreter through `MACBOT_MACRO_CALENDAR_PYTHON`. The worker has an absolute30second deadline; the standalone Python CLI has10second socket timeouts. An enabled guard refuses expired or invalid coverage. Both flags default off. The BLS source returned403 during this review's public-network probe; the full end-to-end live refresh has not passed on that network.

## Research-only demo

From the repository root:

```bash
./scripts/run_demo.sh
```

Open the URL printed by the script. The default is <http://127.0.0.1:8000/#api_key=local&api_secret=localsecret>.
The dashboard imports the demo credentials from the fragment, then removes them from the address bar. Protected actions require request signatures even on localhost. These fixed credentials are for the local demo only.

The script sets:

- `MACBOT_BACKTEST_ONLY=1`
- `MACBOT_STARTUP_ANALYSIS=0`
- loopback-only binding
- a fresh temporary SQLite database
- a temporary copy of the configuration; dashboard changes do not alter the repository configuration

It serves the API and dashboard without starting exchange feeds, the analysis loop or live execution. Analysis and execution controls are disabled in this demo; historical backtests remain available and may fetch public data. Reading the dashboard never starts an analysis or fetches a current price. Backtest-only startup refuses a live configuration.

Useful local checks:

```bash
curl -s http://127.0.0.1:8000/api/health | "${MACBOT_DEMO_PYTHON:-python3.11}" -m json.tool
curl -s http://127.0.0.1:8000/api/ready | "${MACBOT_DEMO_PYTHON:-python3.11}" -m json.tool
PYTHONPATH=scripts "${MACBOT_DEMO_PYTHON:-python3.11}" - <<'PY'
from api_auth import post_json
print(post_json("http://127.0.0.1:8000/api/backtest",
                {"days": 14, "initial_balance": 50},
                api_key="local", api_secret="localsecret", require_auth=True))
PY
```

Stop with `Ctrl-C`; the demo database is deleted by the wrapper.

The backtest endpoint defaults to `mode="synthetic_ta"`. Its optional
`entry_price_yes` and `entry_price_no` are independent fixed hypothetical purchase
prices (0.50 by default), not historical venue quotes. Version 2 corrects return
and cost units, so results can differ from older reports even with default inputs.
Unsupported non-0.50 TP/SL or adaptive win-rate combinations are refused before
fetching candles. The separately supported `mode="decision_replay"` accepts
bounded current-implementation decision records and compares the existing shared
functions without trading or changing the portfolio. Read
[replay scope](REPLAY_AND_FILL_FIDELITY.md) before treating either mode as evidence.

For an optional container that keeps the dashboard and private persistent state
together in research mode, follow [the container instructions](../ops/container.md).
This recipe starts no feeds or trading loop and installs no container engine.

Set `MACBOT_DEMO_RELEASE=1` when you specifically want an optimized build; the default debug build starts faster for local evaluation.

Set `MACBOT_PORT` to use a port other than `8000`. The automated smoke test uses `18080` by default to reduce conflicts:

```bash
./scripts/smoke_test.sh
```

## Manual setup

```bash
test -e .env || cp .env.example .env
chmod 600 .env
MACBOT_ONNX_RUNTIME_LIB="$("${MACBOT_DEMO_PYTHON:-python3.11}" -B scripts/setup_onnx_runtime.py)" || exit 1
export MACBOT_ONNX_RUNTIME_LIB
cargo run --locked --release
```

The committed `config.json` is a development profile in `simulation` mode. It contains local placeholder API credentials, not deployable secrets. Keep the service on loopback during evaluation.

Funded paths reconcile actual quantities, execution prices and fees from confirmed account trades and finalized exchange receipts. Live maker/hybrid operation requires `maker_live_quoting=true`; hybrid crosses must cancel and settle maker orders through the execution actor before a taker submission. Direct hybrid submission remains refused. Manual live partial closes require an idempotency key; automatic live partial take profit requires the separate `live_partial_take_profit=true` opt-in. These paths have local contract and restart coverage, not funded-deployment acceptance. The bundled demonstration model has no trading edge. Read [KNOWN_LIMITATIONS.md](KNOWN_LIMITATIONS.md) before changing research defaults.

For an independently validated funded evaluation, configure the CLOB credentials in `.env`. `PRIVATE_KEY` belongs to the account signer. Choose `POLYMARKET_SIGNATURE_TYPE`: `0` for EOA, `1` for Polymarket Proxy, `2` for Gnosis Safe, or `3` for Deposit Wallet. Types 1–3 require `POLYMARKET_FUNDER` to identify the deployed wallet that holds the assets; an EOA funder must equal its signer. Deposit Wallets require V2/V3 orders. Request authentication still uses the signer address.

Set `POLYGON_RPC_URL` to an explicit trusted HTTPS Polygon mainnet RPC that supports finalized blocks and transaction receipts. A nonempty `private_rpc_url` in the configuration overrides that environment value. Funded submission fails closed when the provider is absent, uses the wrong chain, or lacks finalized evidence. Balances are refreshed and read for the authenticated funding wallet; unrelated wallet balances and other collateral tokens are not combined into its budget.

`POST /api/trade` and `POST /api/trade/force` keep the ordinary Trade response for a confirmed entry. An unresolved BUY returns HTTP200 with `status="pending"`, an `execution_status` of `pending`, `audit_uncertain` or `unavailable`, a reason, and a nested `trade` snapshot. A pending response is not proof of filled inventory. A retry with the same execution idempotency key receives that original response without another submission; refresh current trade/recovery data to observe later settlement.

REST and stream rows identify unfinished BUY evidence with `entry_execution_status`; the stored lifecycle may still be `OPEN`. The dashboard labels these entries separately, shows no unconfirmed mark/P&L, and offers no Close action while their entry evidence is unresolved. A retained order ID alone is insufficient: a rejected SELL can also leave an `OPEN` residual position. Recorded shares come from accounting evidence, never from the intended cash budget.

Run preflight and verify venue approvals, wallet permissions and risk controls before enabling any funded profile. Uncertain orders are saved before submission and recovered without replaying them; incomplete settlement blocks new exposure. Recovery does not automatically clear a kill switch. Inventory below venue minimums, older unreconciled live positions and expired inventory awaiting actual redemption remain visible for explicit reconciliation. Gamma resolution alone never credits the funded ledger with a payout.

## Verification

```bash
./scripts/verify.sh
```

The script checks formatting, all Rust targets/features, Clippy with warnings denied, Rust tests, Python syntax, evaluation splits, the Rust/Python model contract, tracked configuration, and dashboard syntax/signing. Model-level Python tests run when training dependencies are installed; otherwise they are reported as skipped. CI installs the pinned validation environment and runs the same full verification gate against a fresh source export. Python cache files are isolated from the repository; set `MACBOT_VERIFY_TARGET_DIR` when an isolated Rust target is desired.

## Source package verification

Full package verification requires the training dependencies, refuses skipped model tests, and runs synthetic decision replay plus actual Python/ONNX/Rust prediction parity:

```bash
macbot_validation_base="${MACBOT_LOCAL_PYTHON:-$(command -v python3.11)}"
macbot_validation_env="$(cd .. && pwd)/macbot-validation-env"
"$macbot_validation_base" -m venv "$macbot_validation_env"
export MACBOT_VERIFY_PYTHON="$macbot_validation_env/bin/python"
"$MACBOT_VERIFY_PYTHON" -m pip install --upgrade pip==26.2.1
if [ "$(uname -s)" = Linux ]; then
  "$MACBOT_VERIFY_PYTHON" -m pip install --only-binary=:all: \
    torch==2.13.0 --index-url https://download.pytorch.org/whl/cpu
fi
"$MACBOT_VERIFY_PYTHON" -m pip install --only-binary=:all: -r training/requirements-verify.lock
"$MACBOT_VERIFY_PYTHON" -m pip check
MACBOT_ONNX_RUNTIME_LIB="$("$MACBOT_VERIFY_PYTHON" -B scripts/setup_onnx_runtime.py)" || exit 1
export MACBOT_ONNX_RUNTIME_LIB
"$MACBOT_VERIFY_PYTHON" -B scripts/check_environment.py --full
./scripts/verify_source_package.sh
```

Use Python 3.11.17+ within the 3.11 branch and the absolute path to the validation environment's Python, with the same native library selection used during setup. The version-pinned transitive snapshot is validated on macOS ARM; Linux CI uses the same gate and a CPU Torch wheel, but the retained full Linux run establishes Ubuntu 26.04.1 ARM64 acceptance only; other Linux architectures and the Ubuntu 24.04 recipe remain unverified. Linux CPU Torch is installed before the lockfile to avoid unrelated GPU packages. `--only-binary` refuses a missing platform wheel instead of silently starting a new dependency build; if a required wheel is unavailable, that platform's full environment is not ready. Keep the environment outside the extracted source directory. Tests isolate mutable state and Python caches; `MACBOT_VERIFY_TARGET_DIR` can point to an external Rust build cache. These checks establish technical reproducibility, not profitable alpha or ownership rights.

Use a fresh virtual environment. If `macbot-validation-env` already exists, choose another external path rather than mixing this lock with earlier packages. Record the doctor output and full gate log with the archive identity. A passing doctor alone does not replace the full gate.

The full gate also checks the native SQLite used by Python. If it is older than 3.51.3, upgrade that runtime before using the database-writing tools. On Linux, use a fresh external directory and the same Python used by the gate:

```bash
sqlite_lib="$(python3.11 scripts/provision_sqlite.py --prefix /your/new/macbot-sqlite)" || exit 1
export LD_LIBRARY_PATH="$sqlite_lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
```

The helper checks the archive and the published C source hash, then compiles and probes the replacement. Linux CI uses this step explicitly; package pins alone do not establish the native requirement. Actual Ubuntu 26.04.1 ARM64 acceptance is recorded in platform evidence; other platforms remain unverified.

If Python loads an older OpenSSL, the optional helper builds the checksum-pinned 3.5.9 LTS release in a fresh external directory. It requires a C compiler, make, Perl and curl. It does not replace the operating system's library:

```bash
tls_lib="$(python3.11 scripts/provision_openssl.py --prefix /your/new/macbot-openssl)" || exit 1
# Linux:
export LD_LIBRARY_PATH="$tls_lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
# macOS, instead:
# export DYLD_LIBRARY_PATH="$tls_lib${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
python3.11 -c 'import ssl; print(ssl.OPENSSL_VERSION); print(ssl.create_default_context().cert_store_stats())'
```

Use the same interpreter as the validation environment and set the selected library path before starting each Python process. Confirm that your trusted certificate store is populated; `SSL_CERT_FILE` can identify the existing operating-system CA bundle if required. The initial helper download uses the host's curl, so that bootstrap still requires a maintained system TLS installation. CI explicitly provisions and probes both OpenSSL and SQLite before installing Python dependencies.

Create a verified archive from a clean committed source tree:

```bash
MACBOT_VERIFY_PYTHON="$(cd ../macbot-validation-env && pwd)/bin/python" \
python3 scripts/package_source.py --ref HEAD --output artifacts/source-package/macbot-source.tar.gz
```

The tool verifies a temporary extracted copy, checks that source files remain unchanged, and then writes an immutable archive, a per-file SHA-256 manifest and the complete verification log. Keep the evidence alongside the archive. It refuses to overwrite a previous package.

## Authentication and reverse proxies

Loopback connections follow the same authentication requirements as remote connections. Detailed runtime, accounting, history and metrics reads use `x-api-key`; health and minimal readiness stay public. Protected mutations also require `x-api-ts`, a fresh random 32-byte hex `x-api-nonce`, and `x-api-signature`: lowercase hex HMAC-SHA256 over the compact UTF-8 JSON array `[timestamp_string, nonce, METHOD, path_and_query, idempotency_key_or_empty, exact_body_text]` using the exact transmitted body text as the final string (not a parsed or reformatted JSON object). Use the shared `scripts/api_auth.py` client or dashboard rather than constructing old signatures. The accepted timestamp skew is 1–300 seconds (default 30). Reusing a signed request is refused; retries use a fresh signature/nonce and the same execution idempotency key.

The dashboard accepts `#api_key=...&api_secret=...` on its initial URL and stores them for this browser session by default; Remember opts into local storage. Use a trusted browser profile and HTTPS outside localhost. An older database restore rewinds replay claims: rotate the HMAC secret or wait at least eleven minutes with every pre-restore process stopped before starting the restored service. See [the restore drill](../ops/recovery-runbook.md#restore-drill).

Signed controls refuse a backward UTC clock step until the stored authentication time is reached, including after restart. Keep system time synchronized; do not edit replay evidence to bypass this check.

Dashboards use their own origin and port by default. The dashboard refuses a different API origin, including one supplied through `?api=...`, to prevent forwarding stored credentials to another host. Serve the dashboard and API through the same trusted origin. A reverse proxy must forward authentication headers. The application uses the TCP peer for IP allowlisting; for a local proxy that peer is the proxy, not the forwarded end user. Forwarded IP headers do not bypass authentication.

## Modes and authority

| Mode | Purpose | External execution |
| --- | --- | --- |
| Backtest-only | API/dashboard evaluation and historical research | Disabled |
| Simulation | Live-data or synthetic evaluation with virtual state | Disabled by intended profile |
| Shadow | Observe decisions and collect evidence | Disabled |
| Canary | Minimal funded exposure after all gates pass | Possible; requires explicit operator action |
| Live | Production execution | Possible; high risk |

Do not move from research modes to canary/live by changing a single flag. Follow [../ops/live-rollout-runbook.md](../ops/live-rollout-runbook.md), run `/api/preflight`, rotate credentials, use a minimally funded wallet, verify reconciliation and shutdown behavior, and define loss/rollback limits first.

## Training environment

Training dependencies are intentionally separate from the Rust runtime:

```bash
python3.11 -m venv .venv-training
source .venv-training/bin/activate
python -m pip install --upgrade pip==26.2.1
python -m pip install -r training/requirements-verify.lock
python training/sanity_check.py
```

Historical datasets are not included in Git. The training scripts document the expected local inputs and generate their outputs outside the tracked source tree.

Training now reserves the newest 20% of observations before pretraining or parameter selection. It removes earlier observations from held-out markets and applies a 900-second embargo; increase `--embargo-seconds` if the prediction horizon is longer. Equal timestamps remain in the same partition. At least 50 development and 20 holdout samples must remain after purging; these are execution minimums, not statistical acceptance criteria.

```bash
python training/train.py --db path/to/research.sqlite \
  --out artifacts/alpha-models/candidate --skip-hpo --skip-pretrain
python -m unittest discover -s training -p 'test_*.py'
```

Each run writes a versioned ONNX candidate and JSON evaluation report with holdout Brier/AUC, a baseline estimated from development labels, sample/market counts, and split dates. It leaves `latest.onnx` alone unless `--publish-latest` is explicitly supplied. That flag atomically replaces the model observed by the runtime watcher; it is not a readiness or profitability gate.

Freeze the evaluation period before research and avoid repeatedly tuning against it. Reusing an old holdout does not create independent evidence. The previous full-dataset final Brier scores are not comparable to the new holdout scores.
