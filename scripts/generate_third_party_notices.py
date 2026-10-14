#!/usr/bin/env python3
"""Generate THIRD_PARTY_NOTICES.md from Cargo.lock.

Reads every crate in the lockfile and resolves the licence each one declares in
its own manifest, using the local Cargo registry cache. Run `cargo fetch` first
so every crate is present; crates that cannot be resolved are listed as
UNRESOLVED rather than silently omitted.

This produces a diligence aid, not legal advice.
"""

from __future__ import annotations

import argparse
import collections
import datetime
import glob
import pathlib
import re
import sys

SELF_CRATE = "macbot-rs"


def read_lockfile(path: pathlib.Path) -> list[tuple[str, str]]:
    text = path.read_text()
    packages: list[tuple[str, str]] = []
    for block in text.split("[[package]]")[1:]:
        name = re.search(r'^name = "(.+)"', block, re.M)
        version = re.search(r'^version = "(.+)"', block, re.M)
        if name and version and name.group(1) != SELF_CRATE:
            packages.append((name.group(1), version.group(1)))
    return packages


def registry_index() -> dict[str, pathlib.Path]:
    index: dict[str, pathlib.Path] = {}
    for root in glob.glob(str(pathlib.Path.home() / ".cargo/registry/src/*")):
        for entry in pathlib.Path(root).iterdir():
            index[entry.name] = entry
    return index


def resolve_license(directory: pathlib.Path | None) -> str | None:
    if directory is None:
        return None
    manifest = directory / "Cargo.toml"
    if not manifest.exists():
        return None
    try:
        text = manifest.read_text(errors="replace")
    except OSError:
        return None
    declared = re.search(r'^license\s*=\s*"(.+?)"', text, re.M)
    if declared:
        return declared.group(1)
    if re.search(r"^license-file\s*=", text, re.M):
        return "(license-file; see crate)"
    return None


def render(packages, licenses, counts) -> str:
    lines = [
        "# Third-party notices\n",
        f"Generated {datetime.date.today().isoformat()} from `Cargo.lock`.\n",
        "This inventory lists every Rust dependency in the lockfile, including target-specific and optional crates, with the licence each "
        "crate declares in its own manifest. It is a diligence aid produced by "
        "`scripts/generate_third_party_notices.py`, not legal advice. Licence texts "
        "ship inside each crate in the Cargo registry. Browser assets vendored here include their licence texts in `static/vendor/licenses/`. Include the required notices for any compiled dependencies you distribute.\n",
        f"Total crates: **{len(packages)}**\n",
        "## Licence summary\n",
        "| Licence | Crates |",
        "| --- | ---: |",
    ]
    lines += [f"| {lic} | {n} |" for lic, n in counts.most_common()]
    lines += [
        "\n## Notes\n",
        "- Python tooling under `scripts/` and `training/` declares its own dependencies "
        "in the respective `requirements.txt` files and is not covered by this inventory.\n"
        "- ONNX Runtime is pulled as a prebuilt binary by the `ort` crate at build time "
        "and is licensed by Microsoft under the MIT licence.\n"
        "- The operator dashboard vendors its browser dependencies under `static/vendor/`: Bootstrap and Bootstrap Icons (MIT), Chart.js (MIT), and the Space Grotesk and IBM Plex Mono webfonts (SIL Open Font License 1.1). Nothing is fetched from a CDN at runtime.\n",
        "## Full inventory\n",
        "| Crate | Version | Licence |",
        "| --- | --- | --- |",
    ]
    lines += [
        f"| {name} | {version} | {licenses[(name, version)]} |"
        for (name, version) in sorted(licenses)
    ]
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lockfile", default="Cargo.lock", type=pathlib.Path)
    parser.add_argument("--output", default="THIRD_PARTY_NOTICES.md", type=pathlib.Path)
    parser.add_argument(
        "--strict",
        action="store_true",
        help="exit non-zero if any crate licence cannot be resolved",
    )
    args = parser.parse_args()

    packages = read_lockfile(args.lockfile)
    index = registry_index()

    licenses: dict[tuple[str, str], str] = {}
    unresolved: list[tuple[str, str]] = []
    for name, version in packages:
        licence = resolve_license(index.get(f"{name}-{version}"))
        if licence is None:
            unresolved.append((name, version))
            licence = "UNRESOLVED"
        licenses[(name, version)] = licence

    counts = collections.Counter(licenses.values())
    args.output.write_text(render(packages, licenses, counts))

    print(f"{args.output}: {len(packages)} crates, {len(unresolved)} unresolved")
    for licence, count in counts.most_common(10):
        print(f"  {count:4d}  {licence}")
    if unresolved:
        print("\nUnresolved (run `cargo fetch` and re-run):", file=sys.stderr)
        for name, version in unresolved[:20]:
            print(f"  {name} {version}", file=sys.stderr)
        if args.strict:
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
