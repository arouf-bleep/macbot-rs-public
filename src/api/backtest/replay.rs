//! Bounded HTTP adapter around the existing shared production-stage comparator.
//! Supplied captures are evidence of deterministic agreement, not authentic live history.

use std::{collections::HashSet, io::Cursor};

use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{request_error, RESULT_CONTRACT_VERSION};
use crate::replay::{self, DecisionRecord, ReplayReport};

const MAX_RECORDS: usize = 256;
const MAX_RECORD_BYTES: usize = 64 * 1024;
const MAX_REQUEST_BYTES: usize = 512 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayRequest {
    mode: String,
    records: Vec<DecisionRecord>,
    #[serde(default = "default_required_stages")]
    required_stages: Vec<String>,
}

fn default_required_stages() -> Vec<String> {
    replay::STAGES.iter().map(|stage| (*stage).into()).collect()
}

fn prepare(value: Value) -> Result<(Vec<u8>, Vec<String>), String> {
    if serde_json::to_vec(&value).map_err(|e| e.to_string())?.len() > MAX_REQUEST_BYTES {
        return Err("decision replay request exceeds 512 KiB".into());
    }
    let req: ReplayRequest = serde_json::from_value(value)
        .map_err(|e| format!("invalid decision replay request: {e}"))?;
    if req.mode != "decision_replay" || req.records.is_empty() || req.records.len() > MAX_RECORDS {
        return Err("decision_replay requires between 1 and 256 records".into());
    }
    let mut required = HashSet::new();
    if req.required_stages.is_empty() || req.required_stages.len() > replay::STAGES.len() {
        return Err(
            "required_stages must contain at least one known stage, without duplicates".into(),
        );
    }
    for stage in &req.required_stages {
        if !replay::STAGES.contains(&stage.as_str()) || !required.insert(stage) {
            return Err(format!("unsupported or duplicate required stage: {stage}"));
        }
    }
    let mut ids = HashSet::new();
    let mut bytes = Vec::new();
    for record in req.records {
        if record.id.is_empty() || record.id.len() > 128 || !ids.insert(record.id.clone()) {
            return Err("record IDs must be unique, nonempty, and at most 128 bytes".into());
        }
        if record.origin.is_empty()
            || record.origin.len() > 64
            || chrono::DateTime::parse_from_rfc3339(&record.captured_at).is_err()
        {
            return Err(
                "each record requires a bounded origin and valid captured_at UTC offset timestamp"
                    .into(),
            );
        }
        if record.implementation_id.len() != 64
            || !record
                .implementation_id
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
            || !replay::STAGES.contains(&record.stage.as_str())
        {
            return Err(
                "each record requires a SHA256 implementation ID and known decision stage".into(),
            );
        }
        let row = serde_json::to_vec(&record).map_err(|e| e.to_string())?;
        if row.len() > MAX_RECORD_BYTES {
            return Err("decision replay record exceeds 64 KiB".into());
        }
        bytes.extend_from_slice(&row);
        bytes.push(b'\n');
    }
    Ok((bytes, req.required_stages))
}

fn compare(bytes: Vec<u8>, required: Vec<String>) -> Result<ReplayReport, String> {
    let stages: Vec<&str> = required.iter().map(String::as_str).collect();
    replay::compare(Cursor::new(bytes), &stages)
        .map_err(|error| format!("decision replay failed: {error}"))
}

pub(super) async fn run(value: Value) -> Json<Value> {
    let (bytes, required) = match prepare(value) {
        Ok(input) => input,
        Err(error) => return request_error(error),
    };
    let required_output = required.clone();
    // Records are bounded; the shared pure cores run off the async request loop.
    let report = match tokio::task::spawn_blocking(move || compare(bytes, required)).await {
        Ok(Ok(report)) => report,
        Ok(Err(error)) => return request_error(error),
        Err(error) => return request_error(format!("decision replay worker failed: {error}")),
    };
    let all_stages_match = report.failed_records == 0
        && replay::STAGES.iter().all(|stage| {
            report
                .stages
                .get(*stage)
                .is_some_and(|s| s.records > 0 && s.errors == 0 && s.mismatched == 0)
        });
    Json(json!({
        "result_contract_version": RESULT_CONTRACT_VERSION,
        "mode": "decision_replay",
        "scope": "captured_shared_production_stages",
        "required_stages": required_output,
        "all_shared_stages_present_and_matched": all_stages_match,
        "capture_authenticity_verified": false,
        "input_hash_basis": "compact JSON serialization of each accepted DecisionRecord followed by LF, in request order",
        "no_execution_side_effects": true,
        "report": report,
        "limitations": [
            "Supplied records can be synthetic or edited; capture authenticity is not established.",
            "This compares shared pure decision/fill functions with recorded expected outputs. It does not run the execution actor, reconstruct missing books, or simulate a portfolio.",
            "No historical venue execution or trading performance is established."
        ]
    }))
}

#[cfg(test)]
mod tests;
