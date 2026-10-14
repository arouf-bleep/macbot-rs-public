# Bundled model provenance

The source snapshot includes an untrained scalar model to demonstrate the ONNX inference contract. It is initialized from a fixed random seed, uses no market or customer training data, and carries no learned trading strategy or performance claim. The runtime, architecture and training pipeline remain available for the operator to train and evaluate a model independently.

`models/provenance.json` identifies the exact artifact and SHA-256. The ONNX file also contains metadata identifying its synthetic origin, seed and generator. Only the documented untrained fixture is included.

## Reproduce the artifact

Install the training requirements in an environment outside the source archive, then run:

```bash
python training/export_runtime_fixture.py --out /tmp/macbot-model-fixture
```

`scalar_only.onnx` is the bundled model. The replacement export uses Torch 2.13.0 and ONNX 1.22.0; the verification environment uses CPU ONNX Runtime 1.31.0. The earlier synthetic artifact was regenerated using these patched dependencies, without training data or any private trained model. Serialization bytes can differ with library versions. Verify your output against `models/provenance.json` when using the recorded environment.

Publication rejects external tensors in every protobuf location before native checking, so models remain self-contained. This is not a sandbox for hostile model/checkpoint files: only trusted local artifacts should be loaded.

The exporter also produces a full model and twelve synthetic prediction cases. `scripts/verify_decision_parity.sh` compares those predictions through Python, ONNX and Rust. Passing this check establishes numerical compatibility, not forecast quality or profitability.

The supported supervised output is `p_up`, the binary direction probability. The four-output ONNX interface is retained for compatibility, but `magnitude`, `volatility` and `time_to_move` are not supported forecasts: the recorded inputs do not contain verified forward targets for those quantities. The supplied trainer excludes their losses, current exports return zero compatibility values, and runtime analysis marks them unavailable. An older compatible model's numeric auxiliary outputs are also treated as unavailable.

Ensemble approval, accuracy history, calibration and pending predictions belong to the exact SHA-256 of the bytes loaded into the inference session. Replacing those bytes resets approval to shadow evaluation; reloading identical bytes preserves their state. Legacy ensemble state without a content identity also begins in shadow.

Online logistic-regression calibration is fitted and applied only to that predictor. Its snapshot stores consumed resolution identities together with weights, normalization and calibration, so restart can learn missing durable outcomes without learning older outcomes twice. Legacy online snapshots without those identities rebuild from retained resolution rows instead of guessing which observations they contain; information absent from retained history cannot be reconstructed. This migration can change research predictions and establishes no trading-performance claim.
