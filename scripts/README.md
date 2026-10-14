# Macbot tool guide

Run commands from the repository root. Python tools use the environment specified in [Quick start](../docs/QUICKSTART.md); generated output belongs in ignored `artifacts/` or an explicit private directory.

## Setup and preview

| Tool | Purpose |
| --- | --- |
| `preview_dashboard.py` | Read-only dashboard with labelled synthetic examples |
| `check_environment.py` | Local prerequisites, package versions and native runtime checks |
| `install_local.py` | New private research installation with a launcher and random credentials |
| `installation_credentials.py` | Explicit local credential retrieval without starting the engine |
| `run_demo.sh` | Disposable research engine using a temporary database |
| `setup_onnx_runtime.py` | Checksum-pinned native runtime installation |
| `provision_openssl.py`, `provision_sqlite.py` | Pinned native dependency provisioning |

## Verification and distribution

`verify.sh` runs the project checks. `verify_source_package.sh` adds source-boundary, privacy and model-parity checks. `package_source.py` creates a verified immutable archive. `verify_decision_parity.sh` checks captured production decisions and Python/ONNX/Rust predictions.

Tool regressions live in `tests/python/`; dashboard checks live in `tests/dashboard/`. Training tests remain beside the training pipeline. `check_project_files.py`, `check_ml_contract.py` and the JavaScript field/syntax checks run through the main verifier.

## Research

The `backtest_*`, `walkforward_*`, `recency_*`, `lane_*`, `regime_*` and `attribution_*` tools inspect strategy results across periods and conditions. `model_input_*`, `deep_model_*` and `meta_fusion_*` inspect model evidence and input quality. `calibrate_signal_weights.py` updates research weights from documented inputs.

Reports retain their execution and data assumptions. Synthetic-price backtests and uncalibrated simulated fills do not establish historical venue execution or a trading edge.

## Operations and diagnostics

`sqlite_backup.py` handles checked backup and restore operations. `recovery_report.py`, `reconcile_dashboard.py` and `verify_trade_truth.py` inspect accounting and recovery evidence. `run_*soak*`, `run_maker_*`, `*_soak_report.py` and `*_validation_report.py` collect scoped operational evidence. Container tools are described in [the container guide](../ops/container.md).

Private state and credentials stay outside version control. Consult the [recovery runbook](../ops/recovery-runbook.md) before changing an active installation.
