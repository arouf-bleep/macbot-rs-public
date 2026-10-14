# Dependency security disclosure

The expanded 11 October 2026 review replaces the earlier dependency snapshot. The current Rust lockfile is audited without a vulnerability exception. The successful online RustSec check used database commit `7eebec69c352c7191b1f13eb95dd510eeca5d1de`, updated 9 October 2026. Final package identities and test results are recorded in the accompanying archive manifest and verification log.

## Replacements and remaining warnings

| Component | Current treatment |
| --- | --- |
| SQLx / native SQLite | SQLx 0.9.0 and exact bundled SQLite 3.51.3 replace the older library. Startup checks the actual native version before migrations; Python tools also refuse shared database writes with an older runtime. This addresses the documented [SQLite WAL-reset corruption race](https://www.sqlite.org/wal.html#walreset). |
| Optional RSA | Removed from the lockfile by the SQLx replacement. The previous RUSTSEC-2023-0071 exception is removed from CI. |
| Torch / ONNX | Torch 2.13.0 and ONNX 1.22.0 replace versions with known published advisories. Unused plotting/dataframe dependencies were removed. The model publication guard refuses external tensors throughout the protobuf, before native checking can read sidecars. |
| Python / native TLS | The verification gate requires Python 3.11.17 or newer within the 3.11 series and a reviewed patched native OpenSSL branch. Python package pins cannot update either component. OpenSSL 3.5.9 LTS replaces the host's older TLS library in an isolated validation environment; HTTPS tools refuse an unreviewed runtime. This includes the [29 September OpenSSL certificate-memory fixes](https://openssl-library.org/news/secadv/20260929.txt) and the [Python 3.11.17 security release](https://www.python.org/downloads/release/python-31117/). Explicit distributor backport evidence is an operator assertion, not automatic verification of a backport. |
| Python package installer | pip 26.2.1 is explicitly pinned for bootstrap and validation after reviewing four distinct published CVEs in the earlier installer. Workflow setup, the lockfile and the verification gate all require the checked version. |
| ONNX Runtime | Rust and Python use the official CPU 1.31.0 release. The Rust binding is exact `ort` rc13 using stable API 27. The old automatic static-binary downloader is disabled. An explicit installer checks pinned archive and library hashes, retains upstream notices outside the source tree, and the Rust loader verifies the actual native artifact before using it. Release-note fixes were reviewed separately from advisory databases. |
| derivative 2.2.0 | [RUSTSEC-2024-0388](https://rustsec.org/advisories/RUSTSEC-2024-0388.html): unmaintained optional dependency, absent from the selected all-feature/all-target build graph. |
| paste 1.0.15 | [RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436.html): unmaintained dependency active through Alloy. This maintenance warning remains disclosed. |

The earlier vulnerable `lru` dependency remains removed. Browser assets retain their upstream licences, including Popper embedded in Bootstrap and the colour library embedded in Chart.js. Their exact distributed bytes are checked against the reviewed asset manifest during verification.

## Reproduce the checks

```bash
cargo audit --json
cargo tree --locked --all-features --target all -i derivative
cargo tree --locked --all-features --target all -i paste
python3.11 scripts/setup_onnx_runtime.py
```

Python and browser package versions were also queried against OSV. The dated expanded review record gives the final inventory, scan results, selected source reads and limitations. `THIRD_PARTY_NOTICES.md` contains the regenerated Rust licence inventory with no unresolved declarations.

## Trust and coverage limits

Known-advisory scans, source checksums and selected source inspection are separate forms of evidence. A zero-advisory result does not prove that a package is safe; the native runtime release notes identified fixes that the advisory scan did not report. Neither native binaries, browser/font parsers, operating-system libraries nor all upstream functions were exhaustively audited.

Only trusted, locally generated model/checkpoint files should enter this pipeline. Rejecting external tensors prevents the identified sidecar access; it does not sandbox native model parsing or inference. Runtime cache verification assumes the process user's filesystem is trusted. A hostile process with that user's permissions can alter code, libraries or credentials.

The checks ran on the existing macOS ARM host. The native runtime manifest now includes the official Linux ARM64 release, with archive, library, provider and notice hashes checked independently. The 35 existing Python pins have compatible ARM wheel metadata; this is availability evidence, not a Linux dependency installation or application pass. Those dependency-review artifact/metadata checks were historical preparation. Later Ubuntu 26.04.1 ARM64 installed dependencies, full gate, model comparisons and research lifecycle passed as recorded in platform evidence; no funded venue order was submitted. Linux x64 and Windows remain unverified. Recheck current advisories, venue compatibility and the agreed operator platform before relying on this software operationally.
