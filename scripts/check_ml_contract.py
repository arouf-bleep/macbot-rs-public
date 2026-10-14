#!/usr/bin/env python3
"""Fail-fast contract checks between Rust runtime ML code and Python training code."""

from __future__ import annotations

import re
import sys
import ast
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
RUST_MODEL = ROOT / "src" / "ml" / "model.rs"
RUST_INPUT_BUILDER = ROOT / "src" / "ml" / "input_builder.rs"
TRAIN_PY = ROOT / "training" / "train.py"
MODEL_PY = ROOT / "training" / "model.py"
REGIME_PY = ROOT / "training" / "regime_contract.py"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def fail(message: str) -> None:
    print(f"ML contract check failed: {message}", file=sys.stderr)
    raise SystemExit(1)


def parse_feature_list(text: str, marker: str) -> list[str]:
    pattern = re.escape(marker) + r"\s*\[(.*?)\]"
    match = re.search(pattern, text, re.DOTALL)
    if not match:
        fail(f"could not find feature list after marker {marker!r}")
    block = match.group(1)
    return re.findall(r'"([^"]+)"', block)


def parse_usize_constant(text: str, name: str) -> int:
    match = re.search(rf"{re.escape(name)}\s*[:=][^0-9]*([0-9]+)", text)
    if not match:
        fail(f"could not parse integer constant {name}")
    return int(match.group(1))


def parse_python_constant_expressions(text: str) -> dict[str, int]:
    module = ast.parse(text)
    values: dict[str, int] = {}

    def eval_expr(node: ast.AST) -> int:
        if isinstance(node, ast.Constant) and isinstance(node.value, int):
            return node.value
        if isinstance(node, ast.Name) and node.id in values:
            return values[node.id]
        if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
            return eval_expr(node.left) + eval_expr(node.right)
        raise ValueError(ast.dump(node, include_attributes=False))

    for stmt in module.body:
        if not isinstance(stmt, ast.Assign) or len(stmt.targets) != 1:
            continue
        target = stmt.targets[0]
        if not isinstance(target, ast.Name):
            continue
        try:
            values[target.id] = eval_expr(stmt.value)
        except Exception:
            continue
    return values


def parse_python_literal_assignment(text: str, name: str):
    module = ast.parse(text)
    for stmt in module.body:
        if not isinstance(stmt, ast.Assign) or len(stmt.targets) != 1:
            continue
        target = stmt.targets[0]
        if isinstance(target, ast.Name) and target.id == name:
            return ast.literal_eval(stmt.value)
    fail(f"could not parse python literal assignment {name}")


def parse_rust_regime_contract(text: str) -> tuple[dict[str, float], float]:
    start = text.find("fn regime_to_float")
    end = text.find("#[cfg(test)]", start)
    if start == -1 or end == -1:
        fail("could not locate regime_to_float in src/ml/input_builder.rs")
    block = text[start:end]

    mapping: dict[str, float] = {}
    default_value: float | None = None
    for raw_line in block.splitlines():
        line = raw_line.split("//", 1)[0].strip().rstrip(",")
        if "=>" not in line:
            continue
        lhs, rhs = [part.strip() for part in line.split("=>", 1)]
        try:
            value = float(rhs)
        except ValueError:
            continue
        if lhs == "_":
            default_value = value
            continue
        labels = [label.strip().strip('"') for label in lhs.split("|")]
        for label in labels:
            mapping[label] = value

    if default_value is None:
        fail("could not parse default regime value from src/ml/input_builder.rs")
    return mapping, default_value


def require_contains(text: str, snippet: str, label: str) -> None:
    if snippet not in text:
        fail(f"{label} is missing required snippet: {snippet}")


def main() -> None:
    rust_text = read(RUST_MODEL)
    rust_input_builder_text = read(RUST_INPUT_BUILDER)
    train_text = read(TRAIN_PY)
    model_text = read(MODEL_PY)
    regime_text = read(REGIME_PY)
    model_consts = parse_python_constant_expressions(model_text)
    regime_consts = parse_python_constant_expressions(regime_text)

    rust_num_features = parse_usize_constant(rust_text, "pub const NUM_FEATURES")
    rust_features = parse_feature_list(rust_text, "pub const FEATURE_NAMES: [&str; NUM_FEATURES] =")
    if len(rust_features) != rust_num_features:
        fail(
            f"Rust FEATURE_NAMES length {len(rust_features)} does not match NUM_FEATURES {rust_num_features}"
        )

    train_features = parse_feature_list(train_text, "FEATURE_NAMES =")
    if train_features != rust_features:
        fail("training/train.py FEATURE_NAMES drifted from src/ml/model.rs")

    rust_regime_map, rust_default_regime = parse_rust_regime_contract(rust_input_builder_text)
    py_regime_map = parse_python_literal_assignment(regime_text, "REGIME_FLOATS")
    py_default_regime = parse_python_literal_assignment(regime_text, "DEFAULT_REGIME_FLOAT")
    if py_regime_map != rust_regime_map:
        fail("training/regime_contract.py REGIME_FLOATS drifted from src/ml/input_builder.rs")
    if py_default_regime != rust_default_regime:
        fail(
            "training/regime_contract.py DEFAULT_REGIME_FLOAT drifted from src/ml/input_builder.rs"
        )

    model_num_fusion = model_consts.get("NUM_FUSION_FEATURES")
    model_num_context = model_consts.get("NUM_CONTEXT_FEATURES")
    model_num_scalars = model_consts.get("NUM_SCALAR_FEATURES")
    model_num_staleness = model_consts.get("NUM_STALENESS_FEATURES")
    model_scalar_input = model_consts.get("SCALAR_MLP_INPUT_DIM")
    if None in (
        model_num_fusion,
        model_num_context,
        model_num_scalars,
        model_num_staleness,
        model_scalar_input,
    ):
        fail("could not evaluate required python ML dimension constants")

    if model_num_fusion != rust_num_features:
        fail(
            f"training/model.py NUM_FUSION_FEATURES {model_num_fusion} does not match Rust NUM_FEATURES {rust_num_features}"
        )
    if model_num_scalars != model_num_fusion + model_num_context:
        fail("training/model.py NUM_SCALAR_FEATURES no longer equals fusion + context")
    if model_num_staleness != model_num_fusion:
        fail("training/model.py NUM_STALENESS_FEATURES no longer equals fusion feature count")
    if model_scalar_input != model_num_scalars + model_num_staleness:
        fail("training/model.py SCALAR_MLP_INPUT_DIM no longer matches scalars + staleness")

    regime_bucket_count = regime_consts.get("NUM_REGIME_BUCKETS")
    regime_mean_revert_max = parse_python_literal_assignment(regime_text, "REGIME_MEAN_REVERT_MAX")
    regime_trend_min = parse_python_literal_assignment(regime_text, "REGIME_TREND_MIN")
    regime_high_vol_min = parse_python_literal_assignment(regime_text, "REGIME_HIGH_VOL_MIN")
    if regime_bucket_count != 4:
        fail(f"expected NUM_REGIME_BUCKETS == 4, got {regime_bucket_count}")
    if not (regime_mean_revert_max < regime_trend_min < regime_high_vol_min):
        fail("regime bucket thresholds are not strictly ordered")

    require_contains(
        model_text,
        "time_remaining = scalars[:, NUM_FUSION_FEATURES]",
        "training/model.py time_remaining contract",
    )
    require_contains(
        model_text,
        "torch.randn(batch, NUM_SCALAR_FEATURES)",
        "training/model.py ONNX scalar dummy shape",
    )
    require_contains(
        model_text,
        "torch.randn(batch, NUM_STALENESS_FEATURES)",
        "training/model.py ONNX staleness dummy shape",
    )
    require_contains(
        model_text,
        "regime <= REGIME_MEAN_REVERT_MAX",
        "training/model.py mean-revert regime bucket",
    )
    require_contains(
        model_text,
        "regime >= REGIME_TREND_MIN",
        "training/model.py trend regime bucket",
    )
    require_contains(
        model_text,
        "regime >= REGIME_HIGH_VOL_MIN",
        "training/model.py high-vol regime bucket",
    )
    require_contains(
        train_text,
        "regime_value_from_label(regime_str)",
        "training/train.py shared regime contract import",
    )
    require_contains(
        model_text,
        "from regime_contract import (",
        "training/model.py shared regime contract import",
    )
    require_contains(
        train_text,
        "from regime_contract import regime_value_from_label",
        "training/train.py shared regime contract import",
    )

    require_contains(
        train_text,
        "assert NUM_FEATURES == NUM_FUSION_FEATURES",
        "training/train.py feature-count assertion",
    )

    print(
        "ML contract check passed:"
        f" features={rust_num_features}, scalars={model_num_scalars}, staleness={model_num_staleness},"
        f" scalar_mlp_input={model_scalar_input}, regime_buckets={regime_bucket_count}"
    )


if __name__ == "__main__":
    main()
