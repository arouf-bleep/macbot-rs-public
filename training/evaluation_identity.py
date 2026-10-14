"""Bind evaluation reports to loaded samples, exported bytes and source snapshots.

These are content identities, not signatures or proof of profitability. Dataset
hashes cover ordered base samples and metadata supplied to selection/evaluation;
random augmentations follow the recorded source and parameters. Database paths,
output paths, credentials and full market metadata are not copied to the report.
"""
from __future__ import annotations

import hashlib
import importlib.metadata
import json
from pathlib import Path
import platform
import struct

import numpy as np

SOURCE_FILES = (
    "Cargo.toml", "Cargo.lock", "rust-toolchain.toml",
    "training/train.py", "training/model.py", "training/model_artifact.py",
    "training/evaluation_split.py", "training/evaluation_identity.py",
    "training/regime_contract.py", "training/requirements.txt",
    "training/requirements-verify.lock", "src/ml/model.rs",
    "src/ml/input_builder.rs", "src/ml/data_logger.rs", "src/ml/deep_model.rs",
    "src/bot/analyze.rs",
)
ARGUMENTS = (
    "trials", "skip_pretrain", "skip_hpo", "device", "days",
    "holdout_fraction", "embargo_seconds", "epochs", "restarts",
    "publish_latest", "data_source",
)


def canonical_json(value) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"),
                      ensure_ascii=False, allow_nan=False).encode("utf-8")


def framed_update(digest, label: str, payload: bytes) -> None:
    label_bytes = label.encode("utf-8")
    digest.update(struct.pack("<Q", len(label_bytes)))
    digest.update(label_bytes)
    digest.update(struct.pack("<Q", len(payload)))
    digest.update(payload)


def file_identity(path: Path) -> dict:
    digest = hashlib.sha256()
    size = 0
    with Path(path).open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            size += len(chunk)
            digest.update(chunk)
    return {"bytes": size, "sha256": digest.hexdigest()}


def dataset_identity(dataset) -> dict:
    if len(dataset.samples) != len(dataset.meta):
        raise ValueError("every identity sample requires metadata")
    digest = hashlib.sha256()
    framed_update(digest, "schema", b"macbot-base-samples-v1")
    framed_update(digest, "source_kind", canonical_json(dataset.source_kind))
    for index, (sample, metadata) in enumerate(zip(dataset.samples, dataset.meta)):
        framed_update(digest, "row", canonical_json(index))
        framed_update(digest, "metadata", canonical_json(metadata))
        for key in sorted(sample):
            values = np.asarray(sample[key])
            if values.dtype.kind not in "biuf" or not np.isfinite(values).all():
                raise ValueError(f"identity tensor {key} must be finite numeric data")
            # Normalize only byte order/layout; retain numeric dtype, shape and
            # all bits. A float32 sample differs from a float64 sample.
            values = np.ascontiguousarray(values.astype(values.dtype.newbyteorder("<"), copy=False))
            framed_update(digest, f"{key}:shape_dtype",
                          canonical_json({"shape": list(np.asarray(sample[key]).shape),
                                          "dtype": values.dtype.str}))
            framed_update(digest, f"{key}:values", values.tobytes(order="C"))
    return {"samples": len(dataset.samples), "source_kind": dataset.source_kind,
            "sha256": digest.hexdigest()}


def source_snapshot(root: Path, paths=SOURCE_FILES) -> dict:
    root = Path(root).resolve()
    records = []
    for relative in sorted(set(paths)):
        relative_path = Path(relative)
        if relative_path.is_absolute() or ".." in relative_path.parts:
            raise ValueError(f"source identity requires a project-relative path: {relative}")
        path = root / relative_path
        if path.is_symlink() or not path.is_file() or not path.resolve().is_relative_to(root):
            raise ValueError(f"source identity requires a regular project file: {relative}")
        records.append({"path": relative, **file_identity(path)})
    return {"scope": "listed training sources and runtime feature contract",
            "files": records, "sha256": hashlib.sha256(canonical_json(records)).hexdigest()}


def verify_source_snapshot(root: Path, expected: dict) -> None:
    current = source_snapshot(root, [record["path"] for record in expected["files"]])
    if current != expected:
        raise ValueError("evaluation source files changed during training; rerun before publishing")


def runtime_versions() -> dict:
    versions = {"python": platform.python_version(), "numpy": np.__version__}
    for name in ("torch", "optuna", "scikit-learn", "onnx"):
        try:
            versions[name] = importlib.metadata.version(name)
        except importlib.metadata.PackageNotFoundError:
            versions[name] = None
    return versions


def build_evaluation_identity(*, model: Path, model_version: str, sources: dict,
                              data: dict, arguments, hyperparameters: dict,
                              metrics: dict, versions=None) -> dict:
    public_arguments = {key: getattr(arguments, key) for key in ARGUMENTS if hasattr(arguments, key)}
    identity = {
        "schema_version": 1,
        "scope": "content identities; not signed attestation, deterministic training or profitability proof",
        "model_version": model_version,
        "model": file_identity(model),
        "sources": sources,
        "data": data,
        "arguments": public_arguments,
        "hyperparameters": hyperparameters,
        "holdout_metrics": metrics,
        "runtime_versions": runtime_versions() if versions is None else versions,
    }
    identity["sha256"] = hashlib.sha256(canonical_json(identity)).hexdigest()
    return identity


def verify_identity_record(identity: dict) -> None:
    payload = {key: value for key, value in identity.items() if key != "sha256"}
    if hashlib.sha256(canonical_json(payload)).hexdigest() != identity.get("sha256"):
        raise ValueError("evaluation identity record content does not match its digest")


def verified_model_bytes(model: Path, expected: dict) -> bytes:
    content = Path(model).read_bytes()
    if len(content) != expected["bytes"] or hashlib.sha256(content).hexdigest() != expected["sha256"]:
        raise ValueError("exported model changed after evaluation identity was recorded")
    return content
