#!/usr/bin/env python3
"""Start a non-root, fixed research-only image with private persistent state.

Only explicit `credentials` prints dashboard credentials. Startup logs never
print them. Runtime operation, wallet environment and arbitrary engine flags
are deliberately absent from this optional image entrypoint.
"""
from __future__ import annotations

import fcntl
import json
import os
from pathlib import Path
import secrets
import stat
import sys
import tempfile
import time
import urllib.request

INSTALL = Path("/opt/macbot")
STATE = Path("/var/lib/macbot")
STATE_LAYOUT = {"application": "macbot-research", "layout_version": 1}


def owned_file(path: Path) -> None:
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o077:
        raise ValueError("private state files must be regular, owned by this user and mode 0600")
    if info.st_size > 64 * 1024:
        raise ValueError("private configuration exceeds 64KiB")


def validate_config(config: dict) -> None:
    if not isinstance(config, dict) or config.get("mode") != "simulation":
        raise ValueError("this container only accepts simulation research configuration")
    for name in ("maker_live_quoting", "live_partial_take_profit", "shadow_trading", "sim_live_execution", "macro_event_auto_refresh"):
        if config.get(name, False) is not False:
            raise ValueError("runtime-operation settings are refused in the research container")
    if config.get("auth_required") is not True:
        raise ValueError("dashboard authentication must remain enabled")
    for name in ("private_rpc_url", "finnhub_api_key", "lunarcrush_api_key", "signal_weights_path"):
        if config.get(name, ""):
            raise ValueError("external credentials/paths are refused in the research container")
    for name in ("api_key", "api_hmac_secret"):
        value = config.get(name)
        if not isinstance(value, str) or len(value) < 32 or len(value) > 512 or any(character.isspace() for character in value):
            raise ValueError("private dashboard credentials must contain at least 32 characters")


def initialize_state(state: Path, template: Path, *, lock_timeout: float = 5.0) -> dict:
    if os.geteuid() == 0:
        raise ValueError("run the research image as its non-root user")
    info = state.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.geteuid():
        raise ValueError("state volume must be a real directory owned by the image user")
    state.chmod(0o700)
    os.umask(0o077)
    descriptor = os.open(state / ".initialize.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        owned_file(state / ".initialize.lock")
        deadline = time.monotonic() + lock_timeout
        while True:
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise ValueError("another initializer holds this state volume; retry after it finishes")
                time.sleep(0.05)
        config_path = state / "config.json"
        layout_path = state / ".state-layout.json"
        if config_path.exists() or config_path.is_symlink():
            owned_file(config_path)
            if not layout_path.exists() and not layout_path.is_symlink():
                raise ValueError("existing volume lacks its research state layout marker; files are preserved")
            owned_file(layout_path)
            layout = json.loads(layout_path.read_text())
            if layout != STATE_LAYOUT or type(layout.get("layout_version")) is not int:
                raise ValueError("existing volume does not belong to this research state layout")
            config = json.loads(config_path.read_text())
        else:
            # A nonempty unrelated/old volume is never treated as a new install.
            if any(path.name != ".initialize.lock" for path in state.iterdir()):
                raise ValueError("existing state has no valid configuration; use a new volume")
            config = json.loads(template.read_text())
            config.update(api_key=secrets.token_hex(32), api_hmac_secret=secrets.token_hex(32))
            validate_config(config)
            fd, name = tempfile.mkstemp(prefix=".config-", dir=state)
            temporary = Path(name)
            try:
                with os.fdopen(fd, "w") as stream:
                    json.dump(config, stream, indent=2)
                    stream.write("\n")
                    stream.flush()
                    os.fsync(stream.fileno())
                # flock serializes cooperating initializers; hard-link publication
                # also refuses an unexpected concurrent destination without overwrite.
                os.link(temporary, config_path, follow_symlinks=False)
                # An interrupted first install without its layout marker is
                # refused on retry, preserving all files for inspection.
                with layout_path.open("x") as stream:
                    json.dump(STATE_LAYOUT, stream)
                    stream.write("\n")
                    stream.flush()
                    os.fsync(stream.fileno())
                directory = os.open(state, os.O_RDONLY)
                try:
                    os.fsync(directory)
                finally:
                    os.close(directory)
            finally:
                temporary.unlink(missing_ok=True)
        validate_config(config)
        return config
    finally:
        os.close(descriptor)


def runtime_environment(install: Path, state: Path) -> dict[str, str]:
    # Never pass caller-supplied wallet/API credentials, dotenv selection,
    # bind override, library injection or runtime-operation flags to the engine.
    return {"PATH": "/usr/local/bin:/usr/bin:/bin", "LANG": "C.UTF-8", "TZ": "UTC",
        "HOME": str(state), "MACBOT_CONFIG": str(state / "config.json"),
        "MACBOT_DOTENV_PATH": str(install / "empty.env"),
        "MACBOT_DB": str(state / "trades.sqlite"),
        "MACBOT_CONFIG_SNAPSHOT_DIR": str(state / "config-snapshots"),
        "MACBOT_ML_WEIGHTS_PATH": str(state / "ml_weights.json"),
        "MACBOT_ENSEMBLE_PATH": str(state / "ensemble_state.json"),
        "MACBOT_ALERT_FALLBACK_PATH": str(state / "alerts.jsonl"),
        "MACBOT_MODELS_DIR": str(install / "models"),
        "MACBOT_ONNX_RUNTIME_LIB": str(install / "runtime/libonnxruntime.so"),
        "MACBOT_BACKTEST_ONLY": "1", "MACBOT_BIND_ALL": "1", "MACBOT_PORT": "8000"}


def main() -> int:
    command = sys.argv[1] if len(sys.argv) == 2 else "serve" if len(sys.argv) == 1 else "invalid"
    if command not in ("serve", "check", "credentials"):
        raise ValueError("allowed commands: serve, check, credentials")
    if command == "check":
        with urllib.request.urlopen("http://127.0.0.1:8000/api/ready", timeout=3) as response:
            result = json.loads(response.read(64 * 1024 + 1))
        return 0 if result.get("service_ready") is True and result.get("trading_ready") is False else 1
    if command == "credentials":
        owned_file(STATE / ".state-layout.json")
        if json.loads((STATE / ".state-layout.json").read_text()) != STATE_LAYOUT:
            raise ValueError("state volume layout is not a research installation")
        owned_file(STATE / "config.json")
        config = json.loads((STATE / "config.json").read_text())
        validate_config(config)
        print(json.dumps({name: config[name] for name in ("api_key", "api_hmac_secret")}))
        return 0
    initialize_state(STATE, INSTALL / "research-template.json")
    os.chdir(INSTALL)
    print("Starting authenticated research dashboard; feeds and trading remain disabled.", flush=True)
    os.execve(INSTALL / "bin/macbot", [str(INSTALL / "bin/macbot")], runtime_environment(INSTALL, STATE))
    return 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, KeyError) as error:
        print(f"Research container refused startup: {error}", file=sys.stderr)
        raise SystemExit(1)
