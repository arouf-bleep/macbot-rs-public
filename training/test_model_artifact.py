"""Regression checks for sidecar tensors hidden outside graph.initializer."""
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import onnx
from onnx import TensorProto, helper
from model_artifact import reject_external_tensors, validate_model_artifact


def external_tensor() -> TensorProto:
    tensor = TensorProto(name="outside", data_type=TensorProto.FLOAT, dims=[1],
                         data_location=TensorProto.EXTERNAL)
    tensor.external_data.add(key="location", value="must-not-read.bin")
    return tensor


def plain_model() -> onnx.ModelProto:
    return helper.make_model(helper.make_graph(
        [helper.make_node("Identity", ["x"], ["y"])], "test",
        [helper.make_tensor_value_info("x", TensorProto.FLOAT, [1])],
        [helper.make_tensor_value_info("y", TensorProto.FLOAT, [1])],
    ))


class ModelArtifactTests(unittest.TestCase):
    def test_valid_self_contained_model(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "model.onnx"
            onnx.save(plain_model(), path)
            model = validate_model_artifact(path)
            self.assertEqual(model.graph.name, "test")

    def test_hidden_external_tensor_locations(self):
        models = []
        model = plain_model()
        model.graph.initializer.add().CopyFrom(external_tensor())
        models.append(("initializer", model))
        model = plain_model()
        model.graph.node[0].attribute.add(name="value", type=onnx.AttributeProto.TENSOR).t.CopyFrom(external_tensor())
        models.append(("Constant tensor attribute", model))
        model = plain_model()
        model.graph.node[0].attribute.add(name="values", type=onnx.AttributeProto.TENSORS).tensors.add().CopyFrom(external_tensor())
        models.append(("tensor list attribute", model))
        model = plain_model()
        nested = model.graph.node[0].attribute.add(name="branch", type=onnx.AttributeProto.GRAPH).g
        nested.initializer.add().CopyFrom(external_tensor())
        models.append(("subgraph", model))
        model = plain_model()
        nested = model.graph.node[0].attribute.add(name="branches", type=onnx.AttributeProto.GRAPHS).graphs.add()
        nested.initializer.add().CopyFrom(external_tensor())
        models.append(("subgraph list", model))
        model = plain_model()
        model.functions.add(name="f", domain="test").node.add(op_type="Constant").attribute.add(name="value", type=onnx.AttributeProto.TENSOR).t.CopyFrom(external_tensor())
        models.append(("function body", model))
        for component in ("values", "indices"):
            model = plain_model()
            getattr(model.graph.sparse_initializer.add(), component).CopyFrom(external_tensor())
            models.append((f"sparse {component}", model))
        for graph in ("initialization", "algorithm"):
            model = plain_model()
            getattr(model.training_info.add(), graph).initializer.add().CopyFrom(external_tensor())
            models.append((f"training {graph}", model))
        for label, model in models:
            with self.subTest(location=label), self.assertRaisesRegex(ValueError, "self-contained"):
                reject_external_tensors(model)

    def test_external_marker_without_metadata_is_rejected(self):
        model = plain_model()
        model.graph.initializer.add(name="missing", data_location=TensorProto.EXTERNAL)
        with self.assertRaisesRegex(ValueError, "self-contained"):
            reject_external_tensors(model)

    def test_external_metadata_without_marker_is_rejected(self):
        model = plain_model()
        model.graph.initializer.add(name="ambiguous").external_data.add(key="location", value="outside")
        with self.assertRaisesRegex(ValueError, "self-contained"):
            reject_external_tensors(model)

    def test_rejection_happens_before_checker_or_sidecar_load(self):
        model = plain_model()
        model.graph.initializer.add().CopyFrom(external_tensor())
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "model.onnx"
            # Save serialized bytes directly: onnx.save can itself write sidecars.
            path.write_bytes(model.SerializeToString())
            with patch.object(onnx.checker, "check_model") as checker, \
                 patch("onnx.load_external_data_for_model", side_effect=AssertionError("sidecar access")):
                with self.assertRaisesRegex(ValueError, "self-contained"):
                    validate_model_artifact(path)
                checker.assert_not_called()


if __name__ == "__main__":
    unittest.main()
