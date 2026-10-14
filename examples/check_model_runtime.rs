//! Compare real Python-exported models with the production Rust input/output path.
use macbot_rs::ml::deep_model::{DeepModel, DeepModelInput};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Deserialize)]
struct Case {
    id: String,
    model: String,
    model_sha256: String,
    input: DeepModelInput,
    expected: BTreeMap<String, f64>,
}

#[derive(Deserialize)]
struct Fixture {
    origin: String,
    cases: Vec<Case>,
}

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        anyhow::bail!("usage: check_model_runtime FIXTURE.json REPORT.json");
    }
    let bytes = std::fs::read(&args[1])?;
    let fixture: Fixture = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(!fixture.cases.is_empty(), "empty model fixture");
    let mut results = Vec::new();
    let mut max_error = 0.0_f64;
    let mut failures = 0;
    for case in fixture.cases {
        let observed_hash = format!("{:x}", Sha256::digest(std::fs::read(&case.model)?));
        anyhow::ensure!(
            observed_hash == case.model_sha256,
            "model hash mismatch for {}",
            case.id
        );
        let mut model = DeepModel::load(&case.model)?;
        let prediction = model.predict(&case.input)?;
        let actual = BTreeMap::from([
            ("p_up".to_string(), prediction.p_up),
            ("magnitude".to_string(), prediction.magnitude),
            ("volatility".to_string(), prediction.volatility),
            ("time_to_move".to_string(), prediction.time_to_move),
        ]);
        anyhow::ensure!(
            actual.keys().eq(case.expected.keys()),
            "missing/unknown expected output for {}",
            case.id
        );
        let mut passed = true;
        for (name, expected) in &case.expected {
            let error = (actual[name] - expected).abs();
            max_error = max_error.max(error);
            passed &= actual[name].is_finite()
                && expected.is_finite()
                && error <= 1e-5 + expected.abs() * 1e-5;
        }
        if !passed {
            failures += 1;
        }
        results.push(json!({"id":case.id,"model_sha256":observed_hash,"expected":case.expected,"actual":actual,"passed":passed}));
    }
    let report = json!({"origin":fixture.origin,"cases":results,"failures":failures,
        "fixture_sha256":format!("{:x}",Sha256::digest(&bytes)),
        "max_absolute_error":max_error,"absolute_tolerance":1e-5,"relative_tolerance":1e-5,
        "scope":"Python -> exported ONNX -> production Rust DeepModel::predict; no profitability claim"});
    std::fs::write(&args[2], serde_json::to_vec_pretty(&report)?)?;
    println!(
        "{} cases; {failures} failures; max absolute error {max_error:.9}; {}",
        results.len(),
        args[2]
    );
    anyhow::ensure!(failures == 0, "model predictions differ");
    Ok(())
}
