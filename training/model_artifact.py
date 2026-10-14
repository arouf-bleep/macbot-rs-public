"""Validate trusted, self-contained ONNX artifacts before publication.

This forbids sidecar file access; ONNX/protobuf still parse the model in native
code. It is not a sandbox for models received from an untrusted source.
"""
from __future__ import annotations

import argparse
from pathlib import Path

import onnx
from google.protobuf.message import Message


def reject_external_tensors(model: onnx.ModelProto) -> None:
    """Check every tensor, including nested graphs and non-initializer fields."""
    pending: list[tuple[Message, str]] = [(model, "model")]
    while pending:
        message, location = pending.pop()
        if isinstance(message, onnx.TensorProto):
            if message.data_location == onnx.TensorProto.EXTERNAL or message.external_data:
                raise ValueError(f"Model must be self-contained: external tensor at {location}")
        # Walking the protobuf rather than only graph.initializer also covers
        # Constant attributes, function bodies, sparse tensors and training_info.
        for field, value in message.ListFields():
            if field.message_type is None:
                continue
            if field.is_repeated:
                pending.extend((child, f"{location}.{field.name}[{index}]")
                               for index, child in enumerate(value))
            else:
                pending.append((value, f"{location}.{field.name}"))


def validate_model_artifact(path: str | Path) -> onnx.ModelProto:
    """Parse without loading sidecars, reject them, then check graph validity."""
    model = onnx.load(str(path), load_external_data=False)
    reject_external_tensors(model)
    # Passing the in-memory object avoids native check_model_path reading any
    # external files before our complete tensor scan has finished.
    onnx.checker.check_model(model)
    return model


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("model", type=Path)
    args = parser.parse_args()
    validate_model_artifact(args.model)
    print(f"Self-contained ONNX artifact checked: {args.model.name}")


if __name__ == "__main__":
    main()
