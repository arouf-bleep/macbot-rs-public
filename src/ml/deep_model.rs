//! ONNX Runtime inference for the deep learning model.
//!
//! Loads a `.onnx` model file and runs inference to produce a `DeepModelOutput`.
//! The model is held behind `Arc<RwLock>` for hot-swap via `ModelWatcher`.

use anyhow::Result;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::model::NUM_FEATURES;

#[path = "native_runtime.rs"]
mod native_runtime;

// ── Output ──────────────────────────────────────────────────────────────────

/// Prediction output from the deep model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeepModelOutput {
    /// Probability that BTC goes up (primary task head).
    pub p_up: f64,
    /// Numeric ABI compatibility output; no verified forward magnitude target.
    pub magnitude: f64,
    /// Numeric ABI compatibility output; unavailable as a volatility forecast.
    pub volatility: f64,
    /// Numeric ABI compatibility output; unavailable as a time-to-move forecast.
    pub time_to_move: f64,
    /// MC Dropout uncertainty (std dev across N forward passes). None if single-pass.
    pub mc_uncertainty: Option<f64>,
    /// Model version string (e.g. "v003_20260318_brier0.232").
    pub model_version: String,
    /// Identity of the exact bytes loaded into this inference session.
    #[serde(default)]
    pub model_identity: String,
}

// ── Input tensor shapes ─────────────────────────────────────────────────────

/// Assembled input tensors for the deep model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeepModelInput {
    /// Branch 1: TemporalCNN — 1m candles [60, 5] (OHLCV)
    pub candles_1m: Vec<f32>,
    /// Branch 1: TemporalCNN — 5m candles [24, 5]
    pub candles_5m: Vec<f32>,
    /// Branch 1: TemporalCNN — 15m candles [16, 5]
    pub candles_15m: Vec<f32>,
    /// Branch 2: Transformer — L2 depth + order flow + liquidation [12, 45]
    pub orderbook_seq: Vec<f32>,
    /// Branch 3: ScalarMLP — NUM_FEATURES fusion features + 6 context scalars
    pub scalars: Vec<f32>,
    /// Feature staleness vector — age in seconds for each of NUM_FEATURES signals, normalized
    pub staleness: Vec<f32>,
    /// Market regime one-hot or embedding index
    pub regime: f32,
}

impl DeepModelInput {
    pub fn zeros() -> Self {
        Self {
            candles_1m: vec![0.0; 60 * 5],
            candles_5m: vec![0.0; 24 * 5],
            candles_15m: vec![0.0; 16 * 5],
            orderbook_seq: vec![0.0; 12 * 45],
            scalars: vec![0.0; NUM_FEATURES + 6], // 34 + 6 = 40
            staleness: vec![0.0; NUM_FEATURES],   // 34
            regime: 0.0,
        }
    }
}

// ── Deep Model Handle ───────────────────────────────────────────────────────

/// Handle to a loaded ONNX model. Wraps ort Session behind Arc for sharing.
pub struct DeepModel {
    session: Session,
    pub version: String,
    pub identity: String,
    pub loaded_at: std::time::Instant,
}

impl DeepModel {
    /// Load an ONNX model from disk.
    pub fn load(path: &str) -> Result<Self> {
        native_runtime::initialize()?;
        let bytes = std::fs::read(path)?;
        let identity = format!("{:x}", Sha256::digest(&bytes));
        let session = Session::builder()
            .map_err(|e| anyhow::anyhow!("failed to create ONNX session builder: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| anyhow::anyhow!("failed to set optimization level: {e}"))?
            .commit_from_memory(&bytes)
            .map_err(|e| anyhow::anyhow!("failed to load ONNX model {path}: {e}"))?;
        for (index, name) in ["p_up", "magnitude", "volatility", "time_to_move"]
            .iter()
            .enumerate()
        {
            anyhow::ensure!(
                session
                    .outputs()
                    .get(index)
                    .is_some_and(|output| output.name() == *name),
                "model output {index} must be {name}"
            );
        }

        let version = Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        tracing::info!("Deep model loaded: {version} from {path}");

        Ok(Self {
            session,
            version,
            identity,
            loaded_at: std::time::Instant::now(),
        })
    }

    /// Run single-pass inference (no MC Dropout — deterministic).
    ///
    /// Dynamically detects which inputs the ONNX model expects so it works
    /// with both full models (7 inputs) and MLP-only models (3 inputs).
    /// Tensor shapes are read from the session's expected input dimensions
    /// to handle both old (14/20) and new (24/30) models.
    pub fn predict(&mut self, input: &DeepModelInput) -> Result<DeepModelOutput> {
        validate_input(input)?;
        // Helper to convert ort errors
        let ort_err = |e: ort::Error| anyhow::anyhow!("tensor error: {e}");

        // Discover which inputs the model actually expects
        let expected: std::collections::HashSet<String> = self
            .session
            .inputs()
            .iter()
            .map(|i| i.name().to_string())
            .collect();

        let mut ort_inputs: Vec<(String, ort::value::DynValue)> = Vec::new();
        append_optional_inputs(&expected, input, &mut ort_inputs, &ort_err)?;

        // MLP inputs — always present.
        // Use the actual input length (34 features + 6 context = 40 scalars).
        // Previous approach parsed ONNX debug strings which was fragile and sometimes
        // returned wrong dimensions.
        let expected_scalars_dim = input.scalars.len();
        let expected_staleness_dim = input.staleness.len();
        tracing::debug!(
            "Deep model input dims: scalars={}, staleness={}",
            expected_scalars_dim,
            expected_staleness_dim
        );

        append_core_inputs(
            input,
            &mut ort_inputs,
            expected_scalars_dim,
            expected_staleness_dim,
            &ort_err,
        )?;

        let extract = |outputs: ort::session::SessionOutputs| -> Result<DeepModelOutput> {
            // Extract outputs — model exports 4 scalar outputs
            let p_up = extract_scalar(&outputs, 0)?;
            let magnitude = extract_scalar(&outputs, 1)?.max(0.0);
            let volatility = extract_scalar(&outputs, 2)?.max(0.0);
            let time_to_move = extract_scalar(&outputs, 3)?.clamp(0.0, 1.0);

            Ok(DeepModelOutput {
                p_up: p_up.clamp(0.001, 0.999),
                magnitude,
                volatility,
                time_to_move,
                mc_uncertainty: None,
                model_version: self.version.clone(),
                model_identity: self.identity.clone(),
            })
        };

        let msg = match self.session.run(ort_inputs) {
            Ok(out) => return extract(out),
            Err(e) => format!("{e}"),
        };

        // Parse the "Expected: N" dimension from the error message for retry
        let parse_expected = |msg: &str, input_name: &str| -> Option<usize> {
            // Pattern: "input: <name> ... Expected: <N>"
            if let Some(pos) = msg.find(input_name) {
                let tail = &msg[pos..];
                if let Some(exp_pos) = tail.find("Expected: ") {
                    let after = &tail[exp_pos + 10..];
                    let num_str: String =
                        after.chars().take_while(|c| c.is_ascii_digit()).collect();
                    return num_str.parse().ok();
                }
            }
            None
        };

        let is_dim_mismatch = msg.contains("invalid dimensions")
            && (msg.contains("scalars") || msg.contains("staleness"));
        if is_dim_mismatch {
            let retry_scalars = parse_expected(&msg, "scalars").unwrap_or(expected_scalars_dim);
            // ONNX only reports the first mismatched input, so if staleness isn't
            // in the error message, probe legacy dimensions (24 from v2, 14 from v1).
            let retry_staleness = parse_expected(&msg, "staleness").unwrap_or({
                // Try to infer: if scalars expects 30 (v2), staleness likely expects 24
                // If scalars expects 20 (v1), staleness likely expects 14
                if retry_scalars <= 20 {
                    14
                } else {
                    24
                }
            });
            tracing::warn!(
                "Deep model dim mismatch, retrying with expected dims (scalars={}, staleness={}): {msg}",
                retry_scalars, retry_staleness
            );
            let mut retry_inputs: Vec<(String, ort::value::DynValue)> = Vec::new();
            append_optional_inputs(&expected, input, &mut retry_inputs, &ort_err)?;
            append_core_inputs(
                input,
                &mut retry_inputs,
                retry_scalars,
                retry_staleness,
                &ort_err,
            )?;

            let out = self
                .session
                .run(retry_inputs)
                .map_err(|e| anyhow::anyhow!("ONNX inference failed on retry: {e}"))?;
            return extract(out);
        }

        Err(anyhow::anyhow!("ONNX inference failed: {msg}"))
    }
}

fn append_optional_inputs<F>(
    expected: &std::collections::HashSet<String>,
    input: &DeepModelInput,
    ort_inputs: &mut Vec<(String, ort::value::DynValue)>,
    ort_err: &F,
) -> Result<()>
where
    F: Fn(ort::Error) -> anyhow::Error,
{
    native_runtime::initialize()?;
    if expected.contains("candles_1m") {
        let t = Tensor::from_array(ndarray::Array::from_shape_vec(
            (1, 60, 5),
            input.candles_1m.clone(),
        )?)
        .map_err(ort_err)?;
        ort_inputs.push(("candles_1m".into(), t.into_dyn()));
    }
    if expected.contains("candles_5m") {
        let t = Tensor::from_array(ndarray::Array::from_shape_vec(
            (1, 24, 5),
            input.candles_5m.clone(),
        )?)
        .map_err(ort_err)?;
        ort_inputs.push(("candles_5m".into(), t.into_dyn()));
    }
    if expected.contains("candles_15m") {
        let t = Tensor::from_array(ndarray::Array::from_shape_vec(
            (1, 16, 5),
            input.candles_15m.clone(),
        )?)
        .map_err(ort_err)?;
        ort_inputs.push(("candles_15m".into(), t.into_dyn()));
    }
    if expected.contains("orderbook_seq") {
        let t = Tensor::from_array(ndarray::Array::from_shape_vec(
            (1, 12, 45),
            input.orderbook_seq.clone(),
        )?)
        .map_err(ort_err)?;
        ort_inputs.push(("orderbook_seq".into(), t.into_dyn()));
    }
    Ok(())
}

fn append_core_inputs<F>(
    input: &DeepModelInput,
    ort_inputs: &mut Vec<(String, ort::value::DynValue)>,
    scalars_dim: usize,
    staleness_dim: usize,
    ort_err: &F,
) -> Result<()>
where
    F: Fn(ort::Error) -> anyhow::Error,
{
    native_runtime::initialize()?;
    let scalars_data = scalars_for_width(&input.scalars, scalars_dim);
    let scalars = Tensor::from_array(ndarray::Array::from_shape_vec(
        (1, scalars_dim),
        scalars_data,
    )?)
    .map_err(ort_err)?;
    ort_inputs.push(("scalars".into(), scalars.into_dyn()));

    let mut staleness_data = input.staleness.clone();
    staleness_data.resize(staleness_dim, 0.0);
    let staleness = Tensor::from_array(ndarray::Array::from_shape_vec(
        (1, staleness_dim),
        staleness_data,
    )?)
    .map_err(ort_err)?;
    ort_inputs.push(("staleness".into(), staleness.into_dyn()));

    let regime = Tensor::from_array(ndarray::Array::from_shape_vec((1, 1), vec![input.regime])?)
        .map_err(ort_err)?;
    ort_inputs.push(("regime".into(), regime.into_dyn()));

    Ok(())
}

fn extract_scalar(outputs: &ort::session::SessionOutputs<'_>, idx: usize) -> Result<f64> {
    anyhow::ensure!(idx < outputs.len(), "model is missing output index {idx}");
    let (_shape, data) = outputs[idx]
        .try_extract_tensor::<f32>()
        .map_err(|e| anyhow::anyhow!("failed to extract tensor for output index {idx}: {e}"))?;
    anyhow::ensure!(
        data.len() == 1,
        "model output {idx} must contain one scalar"
    );
    let value = data[0] as f64;
    anyhow::ensure!(value.is_finite(), "model output {idx} is not finite");
    Ok(value)
}

fn scalars_for_width(scalars: &[f32], width: usize) -> Vec<f32> {
    // Legacy models have 14 or 24 features followed by the same six context
    // values. Truncating a current input would replace that context with features.
    if scalars.len() == NUM_FEATURES + 6 && matches!(width, 20 | 30) {
        let mut data = scalars[..width - 6].to_vec();
        data.extend_from_slice(&scalars[NUM_FEATURES..]);
        data
    } else {
        let mut data = scalars.to_vec();
        data.resize(width, 0.0);
        data
    }
}

fn validate_input(input: &DeepModelInput) -> Result<()> {
    anyhow::ensure!(
        input.regime.is_finite()
            && [
                &input.candles_1m,
                &input.candles_5m,
                &input.candles_15m,
                &input.orderbook_seq,
                &input.scalars,
                &input.staleness,
            ]
            .iter()
            .all(|values| values.iter().all(|value| value.is_finite())),
        "model input contains a non-finite value"
    );
    Ok(())
}

// ── Shared model handle ─────────────────────────────────────────────────────

/// Thread-safe handle to the current deep model. `None` when no model loaded.
pub type SharedDeepModel = Arc<RwLock<Option<DeepModel>>>;

pub fn new_shared() -> SharedDeepModel {
    Arc::new(RwLock::new(None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn same_filename_replacement_has_new_content_identity_and_keeps_old_session_bound(
    ) -> anyhow::Result<()> {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_model_identity_{unique}.onnx"));
        let original = include_bytes!("../../models/latest.onnx");
        std::fs::write(&path, original)?;
        let mut old = DeepModel::load(path.to_str().unwrap())?;
        let mut replacement = original.to_vec();
        // ModelProto.doc_string is field 6. Changing metadata produces a valid
        // self-contained model with identical inference but different bytes.
        replacement.extend_from_slice(b"\x32\x0aidentity-b");
        std::fs::write(&path, &replacement)?;
        let mut new = DeepModel::load(path.to_str().unwrap())?;
        assert_eq!(old.version, new.version);
        assert_ne!(old.identity, new.identity);
        assert_eq!(old.identity, format!("{:x}", Sha256::digest(original)));
        assert_eq!(new.identity, format!("{:x}", Sha256::digest(&replacement)));
        assert_eq!(
            old.predict(&DeepModelInput::zeros())?.model_identity,
            old.identity
        );
        assert_eq!(
            new.predict(&DeepModelInput::zeros())?.model_identity,
            new.identity
        );
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn zeros_uses_expected_tensor_sizes() {
        let input = DeepModelInput::zeros();

        assert_eq!(input.candles_1m.len(), 60 * 5);
        assert_eq!(input.candles_5m.len(), 24 * 5);
        assert_eq!(input.candles_15m.len(), 16 * 5);
        assert_eq!(input.orderbook_seq.len(), 12 * 45);
        assert_eq!(input.scalars.len(), NUM_FEATURES + 6);
        assert_eq!(input.staleness.len(), NUM_FEATURES);
        assert_eq!(input.regime, 0.0);
    }

    #[test]
    fn legacy_scalar_adaptation_preserves_context() {
        let scalars: Vec<f32> = (0..NUM_FEATURES + 6).map(|i| i as f32).collect();
        for width in [20, 30] {
            let adapted = scalars_for_width(&scalars, width);
            assert_eq!(&adapted[..width - 6], &scalars[..width - 6]);
            assert_eq!(&adapted[width - 6..], &scalars[NUM_FEATURES..]);
        }
    }

    #[test]
    fn rejects_non_finite_inputs_before_inference() {
        let mut input = DeepModelInput::zeros();
        input.scalars[0] = f32::NAN;
        assert!(validate_input(&input).is_err());
        input.scalars[0] = 0.0;
        input.regime = f32::INFINITY;
        assert!(validate_input(&input).is_err());
    }

    #[test]
    fn tensor_assembly_pads_truncates_and_keeps_shapes() -> anyhow::Result<()> {
        let input = DeepModelInput {
            candles_1m: vec![0.0; 60 * 5],
            candles_5m: vec![0.0; 24 * 5],
            candles_15m: vec![0.0; 16 * 5],
            orderbook_seq: vec![0.0; 12 * 45],
            scalars: vec![1.0, 2.0],
            staleness: vec![3.0],
            regime: 0.75,
        };

        let expected: HashSet<String> =
            ["candles_1m", "candles_5m", "candles_15m", "orderbook_seq"]
                .into_iter()
                .map(str::to_string)
                .collect();

        let ort_err = |e: ort::Error| anyhow::anyhow!("tensor error: {e}");
        let mut ort_inputs: Vec<(String, ort::value::DynValue)> = Vec::new();
        append_optional_inputs(&expected, &input, &mut ort_inputs, &ort_err)?;
        append_core_inputs(&input, &mut ort_inputs, 4, 3, &ort_err)?;

        let names: Vec<&str> = ort_inputs.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "candles_1m",
                "candles_5m",
                "candles_15m",
                "orderbook_seq",
                "scalars",
                "staleness",
                "regime",
            ]
        );

        let (shape, data) = ort_inputs[4].1.try_extract_tensor::<f32>()?;
        assert_eq!(**shape, [1, 4]);
        assert_eq!(data, &[1.0, 2.0, 0.0, 0.0]);

        let (shape, data) = ort_inputs[5].1.try_extract_tensor::<f32>()?;
        assert_eq!(**shape, [1, 3]);
        assert_eq!(data, &[3.0, 0.0, 0.0]);

        let (shape, data) = ort_inputs[6].1.try_extract_tensor::<f32>()?;
        assert_eq!(**shape, [1, 1]);
        assert_eq!(data, &[0.75]);

        Ok(())
    }
}
