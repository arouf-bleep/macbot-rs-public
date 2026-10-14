use super::*;

fn record() -> DecisionRecord {
    DecisionRecord {
        schema_version: 1,
        id: "bounded-depth".into(),
        captured_at: "2026-09-14T00:00:00Z".into(),
        origin: "synthetic_fixture".into(),
        implementation_id: replay::implementation_id().into(),
        stage: "simulated_buy_fill".into(),
        input: json!({"book":{"asks":[{"price":0.4,"size":1.0},{"price":0.5,"size":1.0},{"price":0.8,"size":3.0}]},"limit_price":0.5,"size":4.0}),
        expected: json!([0.45, 2.0]),
    }
}
fn request(record: DecisionRecord) -> Value {
    json!({"mode":"decision_replay","records":[record],"required_stages":["simulated_buy_fill"]})
}

#[tokio::test]
async fn bounded_http_replay_calls_actual_shared_fill_core_and_reports_changed_depth() {
    let out = run(request(record())).await.0;
    assert!(out["error"].is_null(), "{out}");
    assert_eq!(out["report"]["matches_required_stages"], true);
    assert_eq!(out["report"]["stages"]["simulated_buy_fill"]["matched"], 1);
    assert_eq!(out["capture_authenticity_verified"], false);
    assert_eq!(out["all_shared_stages_present_and_matched"], false);
    assert!(out["trades"].is_null());
    assert!(out["total_pnl"].is_null());
    let mut changed = record();
    changed.input["book"]["asks"][0]["size"] = json!(2.0);
    let out = run(request(changed)).await.0;
    assert_eq!(out["report"]["matches_required_stages"], false);
    assert_eq!(out["report"]["failed_records"], 1);
    assert!(out["report"]["failures"][0]["differences"]
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["path"] == "/1" && d["actual"] == 3.0));
}

#[tokio::test]
async fn replay_exposes_missing_stages_bad_schema_and_current_implementation_mismatch() {
    let out = run(json!({"mode":"decision_replay","records":[record()]}))
        .await
        .0;
    assert_eq!(out["report"]["matches_required_stages"], false);
    assert!(out["report"]["missing_stages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s == "fusion"));
    let mut wrong = record();
    wrong.schema_version = 2;
    let out = run(request(wrong)).await.0;
    assert_eq!(out["report"]["stages"]["simulated_buy_fill"]["errors"], 1);
    assert!(out["report"]["failures"][0]["error"]
        .as_str()
        .unwrap()
        .contains("unsupported decision schema"));
    let mut wrong = record();
    wrong.implementation_id = "0".repeat(64);
    let out = run(request(wrong)).await.0;
    assert_eq!(out["report"]["matches_required_stages"], false);
    assert!(out["report"]["failures"][0]["error"]
        .as_str()
        .unwrap()
        .contains("implementation mismatch"));
}

#[test]
fn replay_rejects_empty_duplicate_oversized_and_unknown_inputs_before_comparison() {
    let mut value = request(record());
    value["records"] = json!([]);
    assert!(prepare(value).unwrap_err().contains("between 1 and 256"));
    let mut value = request(record());
    value["records"] = json!([record(), record()]);
    assert!(prepare(value).unwrap_err().contains("unique"));
    let mut value = request(record());
    let mut records = Vec::new();
    for i in 0..257 {
        let mut rec = record();
        rec.id = format!("{i}");
        records.push(rec);
    }
    value["records"] = json!(records);
    assert!(prepare(value).unwrap_err().contains("between 1 and 256"));
    let mut value = request(record());
    value["records"][0]["expected"] = json!("x".repeat(MAX_RECORD_BYTES));
    assert!(prepare(value).unwrap_err().contains("64 KiB"));
    let mut value = request(record());
    value["records"][0]["expected"] = json!("x".repeat(MAX_REQUEST_BYTES));
    assert!(prepare(value).unwrap_err().contains("512 KiB"));
    for stages in [
        json!([]),
        json!(["future_stage"]),
        json!(["simulated_buy_fill", "simulated_buy_fill"]),
    ] {
        let mut value = request(record());
        value["required_stages"] = stages;
        assert!(prepare(value).is_err());
    }
    let mut value = request(record());
    value["initial_balance"] = json!(100.0);
    assert!(prepare(value).unwrap_err().contains("unknown field"));
    let mut value = request(record());
    value["records"][0]["captured_at"] = json!("unknown");
    assert!(prepare(value).unwrap_err().contains("timestamp"));
    // The JSON/typed boundary refuses non-finite numbers, rather than treating them as zero.
    assert!(serde_json::from_str::<Value>("{\"records\":[1e999]}").is_err());
}

#[tokio::test]
async fn http_replay_preserves_explicit_fok_and_legacy_ioc_shared_policies() {
    let mut rec = record();
    rec.stage = "taker_paper_fill".into();
    rec.input = json!({"arrival_book":{"asks":[{"price":0.4,"size":1.0}]},"limit_price":0.5,"requested_shares":2.0,"available_depth_fraction":1.0,"fill_policy":"fill_or_kill"});
    rec.expected = Value::Null;
    let make_request = |rec: DecisionRecord| json!({"mode":"decision_replay","records":[rec],"required_stages":["taker_paper_fill"]});
    let out = run(make_request(rec.clone())).await.0;
    assert_eq!(out["report"]["matches_required_stages"], true, "{out}");
    rec.input.as_object_mut().unwrap().remove("fill_policy");
    rec.expected = json!([0.4, 1.0]);
    let out = run(make_request(rec)).await.0;
    assert_eq!(out["report"]["matches_required_stages"], true, "{out}");
}

#[tokio::test]
async fn accepted_record_hash_is_deterministic_and_binds_expected_outputs() {
    let value = request(record());
    let first = run(value.clone()).await.0;
    let second = run(value).await.0;
    assert_eq!(
        first["report"]["input_sha256"],
        second["report"]["input_sha256"]
    );
    let mut changed = record();
    changed.expected = json!([0.45, 3.0]);
    let changed = run(request(changed)).await.0;
    assert_ne!(
        first["report"]["input_sha256"],
        changed["report"]["input_sha256"]
    );
}
