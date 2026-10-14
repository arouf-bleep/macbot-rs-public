# Engineering baseline — 2026-09-13

This is a sanitized engineering snapshot. It records what was directly checked and separates code quality from strategy performance.

## Reproducible source checks

Environment used: macOS arm64 and ubuntu-latest, Rust 1.94.0. Last verified 2026-09-14.

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Pass |
| `cargo check --locked --all-targets --all-features` | Pass |
| `cargo clippy --locked --all-targets --all-features -- -D warnings` | Pass |
| `cargo test --locked --all-features` | Pass |
| Rust test execution | 441 library + 441 binary-module + 5 operational acceptance + 1 integration per run; 0 failures |
| `cargo audit` | 1 advisory: RUSTSEC-2023-0071 (rsa Marvin timing sidechannel), reachable via the alloy stack, no upstream fix. Enforced in CI with that one exception. |
| Python source compilation | All `scripts/*.py` and `training/*.py` passed `py_compile` |
| Training pipeline tests | 10 tests pass, including a real-exporter publication check |
| Dashboard inline JavaScript parsing | `index.html` passed |
| Dashboard credential and reconnect tests | Pass |
| Repository portability and safe-default check | All tracked text files passed |
| Isolated backtest-only startup smoke | Health, readiness, and dashboard routes passed on loopback |
| Decision replay | 125 captured records across all 6 stages replayed with 0 divergences |
| Model parity | 12 Python-to-ONNX-to-Rust prediction cases, 0 failures, max absolute error 6e-8 against a 1e-5 tolerance |

Continuous integration runs the same checks on every push across three jobs: `verify`, `audit`, and
`training-tests`, plus a separate decision-and-model-parity workflow that uploads its evidence as
build artifacts.

The Rust toolchain is pinned to 1.94.0 in `rust-toolchain.toml`. A floating `stable` channel
previously overrode the version CI installed, which silently drifted onto newer Clippy lints and
kept the build red while the documentation claimed 1.94.0 was the baseline.

The operator dashboard no longer loads anything from a public CDN. Bootstrap, Bootstrap Icons,
Chart.js and the webfonts are vendored under `static/vendor/`, so the interface works fully offline.

## Sanitized shadow snapshot

The local database inspected for this report is excluded from Git and is not offered as proof of deployable alpha.

| Metric | Observed value |
| --- | ---: |
| Closed shadow trades | 66 |
| Markets | 32 |
| Runs | 5 |
| First/last close dates | 2026-03-21 to 2026-04-03 |
| Wins / win rate | 38 / 57.58% |
| Net P&L | $1.9684 |
| Gross profit / loss | $12.6918 / $10.7234 |
| Profit factor | 1.184 |
| Expectancy per trade | $0.029825 |
| Maximum observed drawdown | -$3.9254 |

The database also contained 815 fill rows, 1,593 rejection rows, 271,909 outbox rows, 10,748 analysis snapshots, and 877 model-input rows (874 labeled). Counts demonstrate instrumentation breadth, not profitability.

## Model/readiness evidence

- Latest readiness verdict: `baseline_only_no_candidate`
- Latest rollout stop reason: `no_active_candidate`
- A BUY-NO guard report had only 10 rows and is not decision-grade.
- A meta-fusion experiment used 54 rows (38 train, 16 test); its holdout Brier score was worse than the baseline despite the tiny baseline holdout looking unusually strong.
- The latest shutdown validation control bundle reported an integrity status of `ok`, but the run was classified as starvation/dead-window with zero target markets and zero trades. It does not complete the multi-open shutdown proof.

## Interpretation

This baseline supports the claim that the repository is substantial, testable, instrumented, and
reproducible from a clean checkout. Decision replay additionally demonstrates that identical inputs
and configuration reproduce identical decisions, and model parity demonstrates that the Rust runtime
reproduces the Python model's predictions.

It does not support a claim that the strategy is profitable or ready for unsupervised live capital.
Determinism is not edge. See `docs/REPLAY_AND_FILL_FIDELITY.md` for where the simulated fill model
diverges from real execution, and `docs/KNOWN_LIMITATIONS.md` for what remains unproven.
