"""Mutation checks for evaluation model/data/source binding and publication guards."""
from __future__ import annotations

import argparse
import copy
import hashlib
from pathlib import Path
import tempfile
import unittest

import numpy as np

from evaluation_identity import (
    build_evaluation_identity, canonical_json, dataset_identity, source_snapshot,
    verified_model_bytes, verify_identity_record, verify_source_snapshot,
)


class Dataset:
    def __init__(self):
        self.source_kind = "synthetic_test"
        self.samples = [{"scalars": np.array([0.25, 0.75], dtype=np.float32),
                         "label_p_up": np.float32(1.0)},
                        {"scalars": np.array([0.50, 0.50], dtype=np.float32),
                         "label_p_up": np.float32(0.0)}]
        self.meta = [{"timestamp": "2026-01-01T00:00:00Z", "market_id": "first"},
                     {"timestamp": "2026-01-01T00:15:00Z", "market_id": "second"}]


class EvaluationIdentityTests(unittest.TestCase):
    def test_canonical_data_identity_preserves_values_dtype_shape_and_order(self):
        dataset = Dataset()
        expected = dataset_identity(dataset)
        reordered_keys = copy.deepcopy(dataset)
        reordered_keys.samples = [dict(reversed(list(sample.items()))) for sample in dataset.samples]
        self.assertEqual(dataset_identity(reordered_keys), expected)
        opposite_endian = copy.deepcopy(dataset)
        opposite_endian.samples[0]["scalars"] = dataset.samples[0]["scalars"].astype(">f4")
        self.assertEqual(dataset_identity(opposite_endian), expected)
        for mutate in (
            lambda d: d.samples[0]["scalars"].__setitem__(0, 0.30),
            lambda d: d.samples[0].__setitem__("label_p_up", np.float32(0.0)),
            lambda d: d.samples[0].__setitem__("scalars", d.samples[0]["scalars"].astype(np.float64)),
            lambda d: d.samples[0].__setitem__("scalars", d.samples[0]["scalars"].reshape(1, 2)),
            lambda d: d.meta[0].__setitem__("market_id", "different-market"),
            lambda d: d.samples.reverse(),
        ):
            changed = copy.deepcopy(dataset)
            mutate(changed)
            self.assertNotEqual(dataset_identity(changed)["sha256"], expected["sha256"])

    def test_missing_metadata_non_finite_and_object_values_fail_closed(self):
        dataset = Dataset()
        dataset.meta.pop()
        with self.assertRaises(ValueError):
            dataset_identity(dataset)
        for value in (np.array([float("nan")]), np.array([float("inf")]), np.array(["object"])):
            dataset = Dataset()
            dataset.samples[0]["scalars"] = value
            with self.assertRaises(ValueError):
                dataset_identity(dataset)
        with self.assertRaises(ValueError):
            canonical_json({"invalid_metric": float("nan")})

    def fixture(self, root, model):
        sources = source_snapshot(root, ["train.py", "model.py"])
        data = {"development": dataset_identity(Dataset()), "holdout": dataset_identity(Dataset()), "purged_count": 1}
        arguments = argparse.Namespace(epochs=2, holdout_fraction=0.2, db="/private/operator/database.db",
                                      out="/private/operator/models", api_key="private-fixture-secret")
        return build_evaluation_identity(model=model, model_version="test-v1", sources=sources,
                                         data=data, arguments=arguments, hyperparameters={"lr": 0.001},
                                         metrics={"brier": 0.20}, versions={"python": "test-version"})

    def test_model_source_split_metrics_and_parameters_each_change_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            model = root / "model.onnx"
            model.write_bytes(b"first model bytes")
            (root / "train.py").write_text("training source")
            (root / "model.py").write_text("model source")
            identity = self.fixture(root, model)
            self.assertEqual(identity, self.fixture(root, model))
            verify_identity_record(identity)
            self.assertEqual(identity["model"]["sha256"], hashlib.sha256(model.read_bytes()).hexdigest())
            model.write_bytes(b"second model bytes")
            self.assertNotEqual(self.fixture(root, model)["sha256"], identity["sha256"])
            model.write_bytes(b"first model bytes")
            (root / "train.py").write_text("changed training source")
            self.assertNotEqual(self.fixture(root, model)["sha256"], identity["sha256"])
            for key, changed in [("data", {"purged_count": 2}), ("arguments", {"epochs": 3}),
                                 ("holdout_metrics", {"brier": 0.25}), ("model_version", "test-v2")]:
                tampered = copy.deepcopy(identity)
                tampered[key] = changed
                with self.assertRaises(ValueError):
                    verify_identity_record(tampered)

    def test_source_snapshot_checks_exact_list_and_rejects_escape_or_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "train.py"
            source.write_text("initial source")
            snapshot = source_snapshot(root, ["train.py"])
            verify_source_snapshot(root, snapshot)
            source.write_text("changed source")
            with self.assertRaises(ValueError):
                verify_source_snapshot(root, snapshot)
            with self.assertRaises(ValueError):
                source_snapshot(root, ["../outside.py"])
            with self.assertRaises(ValueError):
                source_snapshot(root, [source.as_posix()])
            link = root / "linked.py"
            link.symlink_to(source)
            with self.assertRaises(ValueError):
                source_snapshot(root, ["linked.py"])

    def test_report_never_copies_paths_or_unlisted_secret_arguments(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            model = root / "model.onnx"
            model.write_bytes(b"fixture model")
            for name in ("train.py", "model.py"):
                (root / name).write_text("source")
            encoded = canonical_json(self.fixture(root, model)).decode()
            for private in ("/private/operator", "database.db", "private-fixture-secret", directory):
                self.assertNotIn(private, encoded)
            self.assertIn('"epochs":2', encoded)

    def test_changed_model_is_refused_before_runtime_publication(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            model = root / "model.onnx"
            model.write_bytes(b"recorded model")
            for name in ("train.py", "model.py"):
                (root / name).write_text("source")
            identity = self.fixture(root, model)
            self.assertEqual(verified_model_bytes(model, identity["model"]), b"recorded model")
            model.write_bytes(b"different model")
            with self.assertRaises(ValueError):
                verified_model_bytes(model, identity["model"])


if __name__ == "__main__":
    unittest.main()
