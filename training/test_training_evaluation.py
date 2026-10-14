"""Pipeline tests requiring the optional training environment."""

import contextlib
import io
import json
import hashlib
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

from test_evaluation_split import observations

try:
    import numpy as np
    import torch
    import train
except ModuleNotFoundError as error:
    if error.name not in {"numpy", "torch", "optuna", "sklearn", "tqdm"}:
        raise
    train = None

# Only the published-artifact test needs onnx. Keep it optional so a missing
# onnx skips that one test instead of failing the whole module mid-run.
try:
    import onnx
except ModuleNotFoundError:
    onnx = None


@unittest.skipIf(train is None, "install training dependencies to run model pipeline tests")
class TrainingEvaluationTests(unittest.TestCase):
    def dataset(self, count=200, orderbook=False):
        dataset = train.BTCDataset.__new__(train.BTCDataset)
        dataset.meta = observations(count)
        dataset.source = dataset.source_kind = "synthetic_test"
        dataset.has_orderbook = orderbook
        dataset.samples = []
        for index in range(count):
            dataset.samples.append({
                "candles_1m": np.zeros((60, 5), dtype=np.float32),
                "candles_5m": np.zeros((24, 5), dtype=np.float32),
                "candles_15m": np.zeros((16, 5), dtype=np.float32),
                "orderbook_seq": np.full((12, 45), float(orderbook), dtype=np.float32),
                "scalars": np.full(40, index / count, dtype=np.float32),
                "staleness": np.zeros(34, dtype=np.float32),
                "regime": np.zeros(1, dtype=np.float32),
                "label_p_up": np.float32(index >= int(count * 0.8)),
                "label_magnitude": np.float32(0),
                "label_volatility": np.float32(0),
                "label_time": np.float32(0.5),
            })
        return dataset

    def constant_model(self):
        class ConstantModel(torch.nn.Module):
            def forward(self, candles, *args):
                output = torch.full((candles.shape[0], 1), 0.8)
                return output, output, output, output, output

            def count_parameters(self):
                return 0
        return ConstantModel()

    def test_exact_archive_has_only_observed_binary_supervision(self):
        import sqlite3
        with tempfile.TemporaryDirectory() as directory:
            database = Path(directory) / "exact.sqlite"
            candles = np.full((60, 5), [60000, 61010, 59990, 60000, 20], dtype=np.float32)
            candles[-1, 3] = 61000
            scalars = np.zeros(40, dtype=np.float32)
            scalars[34] = 0.8
            with sqlite3.connect(database) as connection:
                connection.execute("CREATE TABLE ml_model_inputs(timestamp,market_id,signal,regime_label,regime_value,scalars_json,staleness_json,candles_1m_json,candles_5m_json,candles_15m_json,orderbook_seq_json,outcome,fusion_score,p_up,run_id)")
                connection.execute("INSERT INTO ml_model_inputs VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)", (
                    "2026-01-01T00:00:00Z", "fixture-market", "BUY_YES", "TREND_UP", 1.0,
                    json.dumps(scalars.tolist()), json.dumps([0.0] * 34), json.dumps(candles.tolist()),
                    json.dumps(np.zeros((24, 5)).tolist()), json.dumps(np.zeros((16, 5)).tolist()),
                    json.dumps(np.zeros((12, 45)).tolist()), 1, 0.4, 0.7, "runtime-fixture"))
            with contextlib.redirect_stdout(io.StringIO()):
                dataset = train.BTCDataset(str(database), source="exact_inputs")
            self.assertEqual(len(dataset.samples), 1)
            labels = {key for key in dataset.samples[0] if key.startswith("label_")}
            self.assertEqual(labels, {"label_p_up"})
            self.assertEqual(float(dataset.samples[0]["label_p_up"]), 1.0)

    def test_primary_training_loss_uses_binary_observations_only(self):
        prediction = torch.tensor([[0.8]], requires_grad=True)
        calibrated = torch.tensor([[0.7]], requires_grad=True)
        labels = torch.tensor([1.0])
        expected = (torch.nn.functional.binary_cross_entropy(prediction.squeeze(-1), labels)
                    + 0.1 * train.expected_calibration_error(calibrated.squeeze(-1), labels))
        loss = train.primary_training_loss(prediction, calibrated, labels)
        self.assertAlmostEqual(float(loss), float(expected))
        loss.backward()
        self.assertIsNotNone(prediction.grad)

    def test_model_auxiliary_outputs_are_zero_compatibility_placeholders(self):
        from model import MultiHeadBTCPredictor
        model = MultiHeadBTCPredictor().eval()
        previous_threads = torch.get_num_threads()
        torch.set_num_threads(1)
        try:
            with torch.no_grad():
                outputs = model(torch.zeros(1, 60, 5), torch.zeros(1, 24, 5),
                                torch.zeros(1, 16, 5), torch.zeros(1, 12, 45),
                                torch.zeros(1, 40), torch.zeros(1, 34), torch.zeros(1, 1))
            self.assertEqual(len(outputs), 5)
            self.assertTrue(0 <= float(outputs[0]) <= 1)
            for output in outputs[1:4]:
                self.assertTrue(torch.equal(output, torch.zeros(1, 1)))
        finally:
            torch.set_num_threads(previous_threads)

    def test_metrics_use_holdout_only_and_baseline_uses_development_only(self):
        development, holdout, _ = train.split_evaluation_data(self.dataset())
        result = train.evaluate_holdout(self.constant_model(), development, holdout)
        self.assertEqual(result["sample_count"], 40)
        self.assertAlmostEqual(result["brier"], 0.04, places=6)
        self.assertEqual(result["baseline_probability"], 0)
        self.assertEqual(result["baseline_brier"], 1)
        self.assertIsNone(result["auc"])

    def test_main_excludes_holdout_from_selection_and_requires_explicit_publication(self):
        dataset = self.dataset(orderbook=True)
        events = []
        encoder = object()

        def load(target, *args, **kwargs):
            target.__dict__.update(dataset.__dict__)

        def hpo(development, **kwargs):
            events.append("hpo")
            self.assertTrue(all(float(s["label_p_up"]) == 0 for s in development.samples))
            self.assertIsNone(kwargs["pretrained_encoder"])
            return {"epochs": 1, "batch_size": 16}

        def pretrain(prefix, **kwargs):
            self.assertEqual(events[-1], "hpo")
            events.append("pretrain")
            self.assertEqual(len(prefix), int(159 * 0.85) - 1)
            return encoder

        def final(development, parameters, **kwargs):
            self.assertEqual(events[-1], "pretrain")
            events.append("final")
            self.assertEqual(len(development), 159)
            self.assertIs(kwargs["pretrained_encoder"], encoder)
            return self.constant_model()

        with tempfile.TemporaryDirectory() as directory:
            latest = Path(directory) / "latest.onnx"
            latest.write_bytes(b"existing runtime model")
            arguments = ["train.py", "--db", "unused", "--out", directory]
            with patch.object(train.BTCDataset, "_load", load), \
                 patch.object(train, "adversarial_validation"), \
                 patch.object(train, "optuna_hpo", side_effect=hpo), \
                 patch.object(train, "pretrain_orderbook_encoder", side_effect=pretrain), \
                 patch.object(train, "train_final", side_effect=final), \
                 patch.object(train, "export_onnx", side_effect=lambda model, path: Path(path).write_bytes(b"candidate")), \
                 contextlib.redirect_stdout(io.StringIO()):
                with patch.object(sys, "argv", arguments):
                    train.main()
                self.assertEqual(latest.read_bytes(), b"existing runtime model")
                reports = list(Path(directory).glob("v002*.json"))
                self.assertEqual(len(reports), 1)
                report = json.loads(reports[0].read_text())
                self.assertEqual(report["holdout_count"], 40)
                self.assertEqual(report["purged_count"], 1)
                self.assertAlmostEqual(report["holdout_metrics"]["brier"], 0.04, places=6)
                from evaluation_identity import verify_identity_record
                identity = report["evaluation_identity"]
                verify_identity_record(identity)
                self.assertEqual(identity["model"]["sha256"], hashlib.sha256(b"candidate").hexdigest())
                self.assertEqual(identity["data"]["development"]["samples"], 159)
                self.assertEqual(identity["data"]["holdout"]["samples"], 40)
                self.assertEqual(identity["holdout_metrics"], report["holdout_metrics"])
                with patch.object(sys, "argv", arguments + ["--publish-latest"]):
                    train.main()
                self.assertEqual(latest.read_bytes(), b"candidate")
                self.assertFalse(list(Path(directory).glob("*.tmp")))

    def test_mutated_base_inputs_cannot_be_published_with_original_evaluation_identity(self):
        dataset = self.dataset(orderbook=True)
        def load(target, *args, **kwargs):
            target.__dict__.update(dataset.__dict__)
        def mutate_during_training(development, *args, **kwargs):
            development.samples[0]["scalars"][0] += np.float32(1.0)
            return self.constant_model()
        with tempfile.TemporaryDirectory() as directory:
            latest = Path(directory) / "latest.onnx"
            latest.write_bytes(b"existing runtime model")
            arguments = ["train.py", "--db", "unused", "--out", directory,
                         "--skip-hpo", "--skip-pretrain", "--epochs", "1", "--restarts", "1", "--publish-latest"]
            with patch.object(train.BTCDataset, "_load", load), \
                 patch.object(train, "adversarial_validation"), \
                 patch.object(train, "train_final", side_effect=mutate_during_training), \
                 patch.object(train, "export_onnx", side_effect=lambda model, path: Path(path).write_bytes(b"candidate")), \
                 patch.object(sys, "argv", arguments), contextlib.redirect_stdout(io.StringIO()):
                with self.assertRaisesRegex(ValueError, "base samples changed"):
                    train.main()
            self.assertEqual(latest.read_bytes(), b"existing runtime model")
            self.assertFalse(list(Path(directory).glob("v002*.json")))

    @unittest.skipIf(onnx is None, "install onnx to verify the published artifact")
    def test_publication_writes_a_real_loadable_onnx_artifact(self):
        """Publish through the real exporter, not a stub.

        The neighbouring gate test patches export_onnx to write placeholder
        bytes, which proves the gate logic but would pass even if the exporter
        produced something the Rust runtime cannot load. This exercises the
        genuine export path and asserts the published artifact satisfies the
        contract src/ml/deep_model.rs depends on.
        """
        from model import MultiHeadBTCPredictor

        dataset = self.dataset(orderbook=True)

        def load(target, *args, **kwargs):
            target.__dict__.update(dataset.__dict__)

        # A real architecture, so the export is a real export. Untrained weights
        # are deliberate: this checks the serialisation contract, not quality.
        torch.manual_seed(20260914)
        real_model = MultiHeadBTCPredictor().eval()

        previous_threads = torch.get_num_threads()
        torch.set_num_threads(1)
        try:
            with tempfile.TemporaryDirectory() as directory:
                arguments = ["train.py", "--db", "unused", "--out", directory, "--publish-latest"]
                with patch.object(train.BTCDataset, "_load", load), \
                     patch.object(train, "adversarial_validation"), \
                     patch.object(train, "optuna_hpo", return_value={"epochs": 1, "batch_size": 16}), \
                     patch.object(train, "pretrain_orderbook_encoder", return_value=None), \
                     patch.object(train, "train_final", return_value=real_model), \
                     patch.object(sys, "argv", arguments), \
                     contextlib.redirect_stdout(io.StringIO()):
                    train.main()

                published = Path(directory) / "latest.onnx"
                self.assertTrue(published.exists(), "publication must write latest.onnx")
                self.assertGreater(published.stat().st_size, 1024,
                                   "a real model is not a few placeholder bytes")

                # Must be a structurally valid ONNX graph.
                onnx.checker.check_model(str(published))
                graph = onnx.load(str(published), load_external_data=False).graph

                # Must be self-contained: the Rust runtime copies a single file.
                self.assertFalse(
                    any(tensor.external_data for tensor in graph.initializer),
                    "published model must not depend on external weight sidecars",
                )

                # Input and output names are the contract the Rust runtime binds to.
                self.assertEqual(
                    [i.name for i in graph.input],
                    ["candles_1m", "candles_5m", "candles_15m",
                     "orderbook_seq", "scalars", "staleness", "regime"],
                )
                self.assertEqual(
                    [o.name for o in graph.output][:4],
                    ["p_up", "magnitude", "volatility", "time_to_move"],
                    "Rust reads the first four outputs positionally",
                )
                # No leftover temporary files from the atomic publish.
                self.assertFalse(list(Path(directory).glob("*.tmp")))
                report = json.loads(next(Path(directory).glob("v002*.json")).read_text())
                self.assertEqual(report["evaluation_identity"]["model"]["sha256"],
                                 hashlib.sha256(published.read_bytes()).hexdigest())
        finally:
            torch.set_num_threads(previous_threads)

    def test_real_model_can_train_and_evaluate_on_an_isolated_synthetic_split(self):
        previous_threads = torch.get_num_threads()
        torch.set_num_threads(1)
        try:
            development, holdout, _ = train.split_evaluation_data(self.dataset(100))
            with contextlib.redirect_stdout(io.StringIO()):
                model = train.train_final(
                    development, {"epochs": 1, "batch_size": 16},
                    freeze_branches=True, n_restarts=1,
                )
            result = train.evaluate_holdout(model, development, holdout)
            self.assertEqual(result["sample_count"], 20)
            self.assertTrue(np.isfinite(result["brier"]))
        finally:
            torch.set_num_threads(previous_threads)

    def test_selection_partitions_purge_repeated_markets_and_target_horizon(self):
        dataset = self.dataset(100)
        dataset.meta[4]["market_id"] = dataset.meta[90]["market_id"]
        training, validation = train.development_validation_indices(dataset, 85)
        self.assertNotIn(4, training)
        self.assertNotIn(84, training)
        self.assertEqual(validation, list(range(85, 100)))
        self.assertFalse({dataset.meta[i]["market_id"] for i in training}
                         & {dataset.meta[i]["market_id"] for i in validation})

    def test_augmentation_preserves_observed_outcome_candles_and_context(self):
        sample = self.dataset(20).samples[10]
        for augmented in train.augment_sample(sample):
            self.assertEqual(augmented["label_p_up"], sample["label_p_up"])
            np.testing.assert_array_equal(augmented["scalars"][34:], sample["scalars"][34:])
            for key in ("candles_1m", "candles_5m", "candles_15m"):
                np.testing.assert_array_equal(augmented[key], sample[key])

    def test_time_embedding_uses_fraction_of_window(self):
        from model import MultiHeadBTCPredictor
        values = torch.zeros(6, 40)
        values[:, 34] = torch.tensor([-1.0, 0.0, 1 / 15, 0.5, 1.0, 2.0])
        buckets = MultiHeadBTCPredictor()._discretize_time_remaining(values)
        self.assertEqual(buckets.tolist(), [0, 0, 1, 7, 14, 14])

    def test_calibration_accounts_for_probability_one(self):
        error = train.expected_calibration_error(torch.tensor([1.0]), torch.tensor([0.0]))
        self.assertEqual(float(error), 1.0)


if __name__ == "__main__":
    unittest.main()
