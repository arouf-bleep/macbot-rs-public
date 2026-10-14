# Optional research container

This image runs the actual Rust dashboard in **simulation research mode with the
trading loop and feeds disabled**. It is not a funded deployment or hosted
service. The bundled model is the reproducible untrained demonstration model.
Full training/source validation remains a separate workflow.

Install your own container engine first. The helper installs no system tools.
Run these commands from the extracted source directory:

```bash
python3 -B scripts/container.py build --engine podman
python3 -B scripts/container.py start --engine podman
python3 -B scripts/container.py credentials --engine podman
```

Open <http://127.0.0.1:18084/> and use the printed dashboard key and secret to
unlock this browser. Credentials are random, private to the state volume and
never printed in startup logs. Treat the explicit credentials command's output
as private. Use `--port`, `--name`, `--volume` and `--image` to choose distinct
local installations; a build refuses to replace an existing image name.

Stop and remove the container while retaining its named state:

```bash
python3 -B scripts/container.py stop --engine podman
python3 -B scripts/container.py start --engine podman
```

The database, configuration and snapshots remain in `macbot-research-state`.
Do not delete that volume to update an image. Back up its data while the
container is stopped; existing database backup/recovery procedures still apply.
Its private layout marker distinguishes it from unrelated state; unmarked or
unsupported layouts are refused, including an interrupted first initialization.
Existing engine-owned database migrations remain subject to the normal schema
checks. No unrelated volume or older local installation is imported automatically.
An unrelated nonempty volume, unsafe permissions, invalid config, live mode or
live-operation settings are refused instead of replaced or silently downgraded.

The image runs as UID/GID10001, uses a read-only root filesystem, drops Linux
capabilities and forbids additional privileges. The helper publishes only host
loopback. The engine binds its container interface for port forwarding; do not
replace the `127.0.0.1` publish address with an unrestricted address. The helper
does not pass your shell credentials or a host `.env` into the container. Its
entrypoint also constructs a fixed research environment rather than inheriting
wallet or runtime-operation overrides. This is an optional research image;
use the separate reviewed source/local installer for runtime engineering.

The default-deny `.dockerignore` and explicit copies omit local `config.json`,
databases, credentials, private notes, private models, caches and training data.
Rust1.94.0 and Debian base images are pinned by official multi-architecture
index digest. ONNX Runtime1.31.0 selects its native ARM64/x64 official archive;
archive, selected libraries and notices are verified against the existing
manifest before assembly. Current Debian security package versions are fetched
at build time: the resulting image and installed package inventory must be
recorded for acceptance. This is a repeatable build recipe, not a claim of
bit-for-bit reproducibility across different package mirrors/dates.

`--engine docker` uses equivalent Docker commands. Actual platform/container
acceptance must identify the tested engine, architecture, source and image;
neither an ARM build nor the recipe alone establishes x64 or Docker Desktop
acceptance. No image is published, and ongoing platform acceptance must be checked separately.
