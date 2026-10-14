# Macbot source distribution

The source package contains the Rust application, dashboard, research tools, automated tests, operations guides and an untrained demonstration model. Runtime databases, credentials, private notes, raw datasets and build output are excluded.

## Create an archive

From a clean committed checkout, use the pinned validation environment described in [QUICKSTART.md](QUICKSTART.md#source-package-verification):

```bash
python3.11 -B scripts/package_source.py --ref HEAD --output artifacts/source-package/macbot-source.tar.gz
```

The builder verifies a temporary extraction with the full test and model-parity gate. It writes an archive only when checks pass and every delivered file remains unchanged. Keep the archive with its manifest and verification log; the manifest records the commit and SHA-256 of the archive and individual files.

## Verify and install

Extract into a fresh directory, verify the archive hash, and follow [QUICKSTART.md](QUICKSTART.md). `scripts/check_environment.py --full` checks the selected local interpreter, pinned dependencies and cached native runtime. `scripts/verify_source_package.sh` runs the complete package gate.

For a read-only synthetic dashboard preview, run `python3.11 -B scripts/preview_dashboard.py`. For a persistent research installation, use `scripts/install_local.py --destination /path/to/new-directory`. The installation creates private state and credentials, starts no service, and refuses an existing destination.

Use the served HTTP URL rather than opening `static/index.html` directly. Review [KNOWN_LIMITATIONS.md](KNOWN_LIMITATIONS.md), [MODEL_ARTIFACT.md](MODEL_ARTIFACT.md), [DEPENDENCY_DISCLOSURE.md](DEPENDENCY_DISCLOSURE.md) and the [recovery runbook](../ops/recovery-runbook.md) before operating the engine.

The original project remains proprietary under [LICENSE.md](../LICENSE.md). Third-party components retain their own licences and required notices.
