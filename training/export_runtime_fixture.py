"""Export synthetic models and reference predictions for the Rust runtime check.

This checks numerical/serialization contracts, not model quality. No training
data is read and latest.onnx is never published.
"""
import argparse
import hashlib
import json
from pathlib import Path

import onnx
import torch
from model import MultiHeadBTCPredictor, export_onnx
from model_artifact import validate_model_artifact


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    torch.set_num_threads(1)
    names = ["candles_1m", "candles_5m", "candles_15m", "orderbook_seq", "scalars", "staleness"]
    shapes = [(60, 5), (24, 5), (16, 5), (12, 45), (40,), (34,)]
    cases = []
    for disabled in (False, True):
        torch.manual_seed(20260914)
        model = MultiHeadBTCPredictor(disable_cnn_tf=disabled).eval()
        path = args.out / ("scalar_only.onnx" if disabled else "full.onnx")
        export_onnx(model, str(path))
        exported = validate_model_artifact(path)
        onnx.helper.set_model_props(exported, {
            "origin": "synthetic_untrained_test_model",
            "training_data": "none",
            "seed": "20260914",
            "generator": "training/export_runtime_fixture.py",
            "purpose": "Runtime contract demonstration; no trading edge",
        })
        onnx.save(exported, str(path))
        for index in range(6):
            torch.manual_seed(900 + index)
            inputs = [torch.zeros(1, *shape) if index == 0 else torch.randn(1, *shape) for shape in shapes]
            # Runtime staleness is nonnegative; cover each regime embedding.
            inputs[-1] = inputs[-1].abs()
            regime = [-1.0, 0.3, 0.5, 1.0, 1.5, 2.0][index]
            with torch.no_grad():
                outputs = model(*inputs, torch.tensor([[regime]]))
            values = [float(value.item()) for value in outputs]
            # Mirror the documented runtime output bounds, not the model math.
            expected = {"p_up": max(0.001, min(0.999, values[0])),
                        "magnitude": max(0.0, values[1]), "volatility": max(0.0, values[2]),
                        "time_to_move": max(0.0, min(1.0, values[3]))}
            inputs_json = {name: tensor.flatten().tolist() for name, tensor in zip(names, inputs)}
            inputs_json["regime"] = regime
            cases.append({"id": f"{path.stem}-{index}", "model": str(path.resolve()),
                          "model_sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
                          "input": inputs_json, "expected": expected})
    manifest = {"origin": "synthetic_untrained_models", "torch_version": torch.__version__,
                "onnx_version": onnx.__version__, "cases": cases,
                "scope": "The four outputs consumed by Rust; calibration head is not consumed by this runtime."}
    destination = args.out / "runtime-fixture.json"
    destination.write_text(json.dumps(manifest, indent=2))
    print(f"Wrote {len(cases)} prediction cases to {destination}")


if __name__ == "__main__":
    main()
