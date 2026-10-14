#!/usr/bin/env python3
"""Optional local research container commands; never installs container tools.

The service is non-root, research-only and published on host loopback. Named
private state survives a stop/remove/recreate; this helper never removes it.
"""
from __future__ import annotations

import argparse
from pathlib import Path
import re
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def name(value: str) -> str:
    if not re.fullmatch(r"[a-z0-9][a-z0-9_.-]{0,63}", value):
        raise argparse.ArgumentTypeError("use a short lowercase local image/container/volume name")
    return value


def run_arguments(engine: str, image: str, container: str, volume: str, port: int) -> list[str]:
    command = [engine, "run", "--detach", "--name", container, "--read-only",
        "--cap-drop=ALL", "--security-opt=no-new-privileges", "--pids-limit=256",
        "--memory=1g", "--cpus=2", "--stop-timeout=20",
        "--publish", f"127.0.0.1:{port}:8000", "--mount", f"type=volume,source={volume},destination=/var/lib/macbot",
        "--tmpfs", "/tmp:rw,noexec,nosuid,nodev,size=16m,mode=1777"]
    if engine == "podman":
        command += ["--network=slirp4netns"]
    return command + [image]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("build", "start", "stop", "status", "credentials"))
    parser.add_argument("--engine", choices=("podman", "docker"), default="podman")
    parser.add_argument("--image", type=name, default="macbot-research-local")
    parser.add_argument("--name", type=name, default="macbot-research")
    parser.add_argument("--volume", type=name, default="macbot-research-state")
    parser.add_argument("--port", type=int, default=18084)
    args = parser.parse_args()
    if not 1024 <= args.port <= 65535:
        parser.error("choose an unprivileged port between 1024 and 65535")
    if not shutil.which(args.engine):
        parser.error("container engine not installed; this helper installs no system tools")
    if args.command == "build":
        found = subprocess.run([args.engine, "image", "inspect", args.image], capture_output=True, timeout=30)
        if found.returncode == 0:
            parser.error("image name already exists; choose a new --image to preserve prior builds")
        command = [args.engine, "build", "--tag", args.image, "--file", "Dockerfile", "--pull=always"]
        if args.engine == "podman":
            # OCI image format drops Dockerfile HEALTHCHECK on Podman.
            command += ["--network=slirp4netns", "--format=docker"]
        subprocess.run(command + ["."], cwd=ROOT, check=True, timeout=3600)
    elif args.command == "start":
        subprocess.run(run_arguments(args.engine, args.image, args.name, args.volume, args.port), check=True, timeout=60)
        print(f"Research dashboard: http://127.0.0.1:{args.port}/")
        print("Unlock credentials: rerun this helper with the credentials command. Do not share its output.")
    elif args.command == "stop":
        subprocess.run([args.engine, "stop", "--time", "20", args.name], check=True, timeout=40)
        subprocess.run([args.engine, "rm", args.name], check=True, timeout=30)
        print("Container removed; its named private state volume is retained.")
    elif args.command == "status":
        subprocess.run([args.engine, "inspect", "--format", "{{json .State}}", args.name], check=True, timeout=30)
    else:
        subprocess.run([args.engine, "exec", args.name, "python3", "-B", "/opt/macbot/container_entrypoint.py", "credentials"], check=True, timeout=30)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
