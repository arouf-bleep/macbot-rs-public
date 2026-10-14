# Macbot

Macbot is a Rust research and execution engine for short-duration Polymarket markets. It includes market-data feeds, maker and taker order management, receipt-based accounting, risk controls, a browser dashboard and Python research tools.

The project has no established profitable strategy. The included model is an untrained demonstration fixture. Local tests cover engineering behavior; funded venue acceptance and ongoing external-service compatibility require separate validation. Read [the current limitations](docs/KNOWN_LIMITATIONS.md).

## Start here

| Task | Entry point |
| --- | --- |
| Preview the interface with synthetic data | `python3.11 -B scripts/preview_dashboard.py` |
| Check prerequisites and run the research engine | [Quick start](docs/QUICKSTART.md) |
| Create a private local installation | `python3.11 -B scripts/install_local.py --destination /path/to/new-directory` |
| Run project verification | `./scripts/verify.sh` |
| Verify or create a source archive | [Source distribution](docs/SOURCE_DISTRIBUTION.md) |
| Find research and operations commands | [Tool guide](scripts/README.md) |

Open the HTTP URL printed by the preview or launcher. Opening `static/index.html` directly does not run the server or load its API. The synthetic preview runs no engine, model, wallet, database or exchange. The research launcher starts with feeds and trading disabled.

## Repository layout

| Directory | Contents |
| --- | --- |
| `src/` | Rust engine, API, signals, accounting and model runtime |
| `static/` | Dashboard and locally vendored interface assets |
| `scripts/` | Setup, verification, research and operations commands |
| `tests/` | Rust acceptance tests, Python tool tests and dashboard checks |
| `training/` | Dataset preparation, model training and evaluation tests |
| `models/` | Reproducible demonstration model and provenance |
| `docs/` | Architecture, setup, limitations, dependency and research guides |
| `ops/` | Research profiles, monitoring configuration and recovery runbooks |

## Technical guides

- [Architecture and execution ownership](docs/ARCHITECTURE.md)
- [Quick start and supported prerequisites](docs/QUICKSTART.md)
- [Recovery and backup procedures](ops/recovery-runbook.md)
- [Research replay and simulated fills](docs/REPLAY_AND_FILL_FIDELITY.md)
- [Demonstration model](docs/MODEL_ARTIFACT.md)
- [Dependency disclosure](docs/DEPENDENCY_DISCLOSURE.md)
- [Maintenance roadmap](docs/CURRENT_STATE_ROADMAP.md)
- [Historical changelog](docs/history/CHANGELOG.md)

## Rights and security

The original code is proprietary; public visibility grants no open-source licence. See [LICENSE.md](LICENSE.md), [third-party notices](THIRD_PARTY_NOTICES.md) and [security guidance](SECURITY.md). Never commit credentials, wallets, runtime databases or private model state.
