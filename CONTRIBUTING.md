# Contributing

## Development baseline

- Rust 1.94.0 as pinned in `rust-toolchain.toml`, with `rustfmt` and `clippy`
- Python 3.11.17 or newer within the 3.11 branch for the pinned verification environment
- Node.js 22 or newer

Create `.env` from `.env.example`, keep it mode `0600`, and never commit credentials or runtime databases.

## Before opening a change

Run:

```bash
./scripts/verify.sh
```

For a quick Rust-only loop:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
```

## Change expectations

- Preserve fail-closed behavior in live execution, reconciliation, and risk gates.
- Add regression coverage for lifecycle, accounting, schema, or configuration changes.
- Keep exchange payload transformations isolated in `src/clob` or explicit adapter helpers.
- Keep statuses and persisted enum-like strings compatible with existing data.
- Update `docs/history/CHANGELOG.md` for operator-visible behavior changes.
- Do not claim strategy improvement from in-sample or small-sample results.

## Codebase conventions

These are load-bearing. Breaking them corrupts stored data or silently changes behaviour.

- Trade statuses are persisted uppercase: `"OPEN"`, `"CLOSING"`, `"CLOSED"`. Never lowercase.
- Signals return `Option<f64>`; `None` means the signal is unavailable, which is distinct from zero.
- `market_data.price` represents YES in analysis snapshots. Entry sizing, spread checks and execution
  must use the selected token's book. Never treat `1.0 - YES price` as an observed NO quote.
- Config persistence uses atomic write: temporary file, rename, fsync.
- Secrets implement `zeroize` on drop, and HMAC comparison uses constant-time equality.
- Values use parameterised SQL binds. Dynamic identifiers are allowlisted; the backup destination literal is strictly validated.
- Treat unsafe OS and native-runtime integration as review points; preserve validation and targeted
  tests for their assumptions.

## Module map

| Directory | Purpose |
| --- | --- |
| `src/bot` | Trading state, analysis, execution actor, risk, reconciliation, lifecycle |
| `src/signals`, `src/fusion` | Feature generation and signal fusion |
| `src/market`, `src/clob` | L2 ingestion and Polymarket integration |
| `src/ml`, `training` | Online inference, ONNX integration, and offline training |
| `src/api`, `static` | REST, realtime transports, metrics, and the operator dashboard |
| `src/db` | SQLite schema, persistence, backups, and integrity plumbing |
| `src/replay` | Decision capture and offline replay comparison |

State mutations are serialised through a single-writer actor in `src/bot/actor.rs`. Deployment
profiles escalate Relaxed, Shadow, Canary, Live, with validation tightening at each step; live mode
additionally enforces a credential floor that no profile override can lower.

## Operational safety

Development and automated checks should use backtest-only, simulation, or shadow modes. Enabling live mode, supplying funded credentials, or exposing the service off-host is an operator decision outside the normal development workflow.
