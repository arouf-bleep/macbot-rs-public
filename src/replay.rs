//! Offline comparison of captured production decision boundaries.
//!
//! This does not reconstruct missing historical inputs or simulate a portfolio.
//! Every expected result comes from a capture; replay never manufactures it.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::{BufRead, Write},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
};

use crate::{bot::risk, config::Config, fusion::replay::FusionInput};

pub const STAGES: &[&str] = &[
    "fusion",
    "kelly_sizing",
    "entry_sizing",
    "trade_risk",
    "entry_coordination",
    // The static book-walk capture is retained for synthetic regression
    // fixtures; runtime fills are captured by the arrival stages below.
    "simulated_buy_fill",
    "maker_paper_fill",
    "taker_paper_fill",
    "taker_paper_sell",
];
const SCHEMA_VERSION: u32 = 1;
const ABS_TOLERANCE: f64 = 1e-9;
const REL_TOLERANCE: f64 = 1e-9;
static CAPTURE_LOCK: Mutex<()> = Mutex::new(());
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn implementation_id() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| {
        let mut hash = Sha256::new();
        for source in [
            include_str!("fusion/mod.rs"),
            include_str!("fusion/replay.rs"),
            include_str!("signals/lmsr.rs"),
            include_str!("json_util.rs"),
            include_str!("domain.rs"),
            include_str!("bot/risk.rs"),
            include_str!("bot/kelly.rs"),
            include_str!("bot/execute.rs"),
            include_str!("bot/simulation.rs"),
            include_str!("config.rs"),
            include_str!("replay.rs"),
            include_str!("../Cargo.lock"),
        ] {
            hash.update(source.as_bytes());
            hash.update([0]);
        }
        format!("{:x}", hash.finalize())
    })
}

pub(crate) fn capture_enabled() -> bool {
    std::env::var("MACBOT_DECISION_CAPTURE_PATH").is_ok_and(|s| !s.trim().is_empty())
}

/// Opt-in diagnostic capture. No file or serialization work when disabled.
/// Capture failure is reported and never changes the trading decision.
pub(crate) fn capture(stage: &str, input: impl FnOnce() -> Value, output: &impl Serialize) {
    let Ok(path) = std::env::var("MACBOT_DECISION_CAPTURE_PATH") else {
        return;
    };
    if path.trim().is_empty() {
        return;
    }
    let result = (|| -> Result<()> {
        let record = DecisionRecord {
            schema_version: SCHEMA_VERSION,
            id: format!(
                "{}:{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ),
            captured_at: chrono::Utc::now().to_rfc3339(),
            origin: if cfg!(test) { "unit_test" } else { "runtime" }.into(),
            implementation_id: implementation_id().into(),
            stage: stage.into(),
            input: input(),
            expected: serde_json::to_value(output)?,
        };
        let mut bytes = serde_json::to_vec(&record)?;
        bytes.push(b'\n');
        let _guard = CAPTURE_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("capture lock poisoned"))?;
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)?.write_all(&bytes)?;
        Ok(())
    })();
    if let Err(error) = result {
        tracing::warn!(%error, stage, "Decision capture failed; replay coverage has a gap");
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionRecord {
    pub schema_version: u32,
    pub id: String,
    pub captured_at: String,
    pub origin: String,
    pub implementation_id: String,
    pub stage: String,
    pub input: Value,
    pub expected: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RiskInput {
    config: Config,
    snapshot: risk::TradeRiskSnapshot,
    proposal: risk::ProposedTrade,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CoordinationInput {
    config: Config,
    snapshot: risk::EntryCoordinationSnapshot,
    proposal: risk::EntryCoordinationProposal,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SizingInput {
    config: Config,
    state: crate::bot::kelly::KellySizingInput,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntrySizingInput {
    config: Config,
    regime: String,
    effective_confidence: f64,
    has_override: bool,
    amount: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FillInput {
    book: Value,
    limit_price: f64,
    size: f64,
}

pub fn evaluate(record: &DecisionRecord) -> Result<Value> {
    if record.schema_version != SCHEMA_VERSION {
        bail!("unsupported decision schema {}", record.schema_version);
    }
    if record.implementation_id != implementation_id() {
        bail!(
            "decision implementation mismatch: recorded {}, current {}; use captures from the current implementation",
            record.implementation_id,
            implementation_id()
        );
    }
    // Deliberately do not Config::load/normalize: those consult today's env.
    // These are the effective, redacted settings captured with the decision.
    match record.stage.as_str() {
        "fusion" => {
            let input: FusionInput = serde_json::from_value(record.input.clone())?;
            Ok(serde_json::to_value(input.evaluate())?)
        }
        "trade_risk" => {
            let input: RiskInput = serde_json::from_value(record.input.clone())?;
            Ok(serde_json::to_value(risk::evaluate_trade_guardrails_core(
                &input.config,
                &input.snapshot,
                &input.proposal,
            ))?)
        }
        "entry_coordination" => {
            let input: CoordinationInput = serde_json::from_value(record.input.clone())?;
            Ok(serde_json::to_value(
                risk::evaluate_entry_coordination_core(
                    &input.config,
                    &input.snapshot,
                    &input.proposal,
                ),
            )?)
        }
        "kelly_sizing" => {
            let input: SizingInput = serde_json::from_value(record.input.clone())?;
            Ok(serde_json::to_value(
                crate::bot::kelly::evaluate_kelly_size(&input.config, &input.state),
            )?)
        }
        "entry_sizing" => {
            let input: EntrySizingInput = serde_json::from_value(record.input.clone())?;
            Ok(serde_json::to_value(
                crate::bot::execute::scale_entry_amount_core(
                    &input.config,
                    &input.regime,
                    input.effective_confidence,
                    input.has_override,
                    input.amount,
                ),
            )?)
        }
        "simulated_buy_fill" => {
            let input: FillInput = serde_json::from_value(record.input.clone())?;
            Ok(serde_json::to_value(
                crate::bot::execute::simulate_buy_fill_core(
                    &input.book,
                    input.limit_price,
                    input.size,
                ),
            )?)
        }
        "maker_paper_fill" => {
            let input: crate::bot::simulation::MakerPaperInput =
                serde_json::from_value(record.input.clone())?;
            Ok(serde_json::to_value(
                crate::bot::simulation::maker_fill_step(&input),
            )?)
        }
        "taker_paper_fill" => {
            let input: crate::bot::simulation::TakerPaperInput =
                serde_json::from_value(record.input.clone())?;
            Ok(serde_json::to_value(
                crate::bot::simulation::taker_arrival_fill_core(&input),
            )?)
        }
        "taker_paper_sell" => {
            let input: crate::bot::simulation::TakerPaperSellInput =
                serde_json::from_value(record.input.clone())?;
            Ok(serde_json::to_value(
                crate::bot::simulation::taker_arrival_sell_core(&input),
            )?)
        }
        other => bail!("unsupported decision stage {other}"),
    }
}

#[derive(Debug, Serialize)]
pub struct Difference {
    pub path: String,
    pub expected: Value,
    pub actual: Value,
}

fn differences(expected: &Value, actual: &Value, path: &str, output: &mut Vec<Difference>) {
    match (expected, actual) {
        (Value::Number(a), Value::Number(b)) => {
            let (a, b) = (a.as_f64().unwrap(), b.as_f64().unwrap());
            if (a - b).abs() <= ABS_TOLERANCE + REL_TOLERANCE * a.abs().max(b.abs()) {
                return;
            }
        }
        (Value::Object(a), Value::Object(b)) => {
            let keys: std::collections::BTreeSet<_> = a.keys().chain(b.keys()).collect();
            for key in keys {
                let child = format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"));
                match (a.get(key), b.get(key)) {
                    (Some(av), Some(bv)) => differences(av, bv, &child, output),
                    (av, bv) => output.push(Difference {
                        path: child,
                        expected: json!({"present": av.is_some(), "value": av}),
                        actual: json!({"present": bv.is_some(), "value": bv}),
                    }),
                }
            }
            return;
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (index, (av, bv)) in a.iter().zip(b).enumerate() {
                differences(av, bv, &format!("{path}/{index}"), output);
            }
            return;
        }
        _ if expected == actual => return,
        _ => {}
    }
    output.push(Difference {
        path: path.into(),
        expected: expected.clone(),
        actual: actual.clone(),
    });
}

#[derive(Debug, Default, Serialize)]
pub struct StageSummary {
    pub records: usize,
    pub matched: usize,
    pub mismatched: usize,
    pub errors: usize,
}

#[derive(Debug, Serialize)]
pub struct CaseResult {
    pub line: usize,
    pub id: String,
    pub stage: String,
    pub differences: Vec<Difference>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ReplayReport {
    pub scope: &'static str,
    pub input_sha256: String,
    pub implementation_id: String,
    pub recorded_implementations: BTreeMap<String, usize>,
    pub origins: BTreeMap<String, usize>,
    pub stages: BTreeMap<String, StageSummary>,
    pub records: usize,
    pub failed_records: usize,
    pub missing_stages: Vec<String>,
    pub matches_required_stages: bool,
    pub absolute_tolerance: f64,
    pub relative_tolerance: f64,
    pub failures: Vec<CaseResult>,
    pub limitations: Vec<&'static str>,
}

pub fn compare(mut reader: impl BufRead, required_stages: &[&str]) -> Result<ReplayReport> {
    for stage in required_stages {
        if !STAGES.contains(stage) {
            bail!("unknown required stage {stage}");
        }
    }
    let mut report = ReplayReport {
        scope: "captured production stages; not end-to-end trading or profitability proof",
        input_sha256: String::new(), implementation_id: implementation_id().into(),
        recorded_implementations: BTreeMap::new(), origins: BTreeMap::new(), stages: BTreeMap::new(),
        records: 0, failed_records: 0, missing_stages: Vec::new(), matches_required_stages: false,
        absolute_tolerance: ABS_TOLERANCE, relative_tolerance: REL_TOLERANCE,
        failures: Vec::new(), limitations: vec![
            "No feature reconstruction, post-fusion ML/ensemble blending, or top-level strategy scheduling is replayed.",
            "Risk and sizing use captured state; this is not a causal portfolio simulation across records.",
            "Paper admission latency, depth haircut and maker queue/partial-fill assumptions are configurable heuristics, not measured venue execution; cancellation races and actual queue position remain unmodelled.",
            "Only captured calls are checked. Upstream suppressed decisions and capture failures are not evidence of agreement.",
            "Unit-test captures are synthetic regression evidence, not fresh shadow or live observations.",
            "Records from a different decision implementation are rejected, even when their expected numeric result would match.",
        ],
    };
    let mut hash = Sha256::new();
    let mut line_number = 0;
    loop {
        let mut bytes = Vec::new();
        if reader
            .read_until(b'\n', &mut bytes)
            .context("read capture")?
            == 0
        {
            break;
        }
        line_number += 1;
        let index = line_number - 1;
        hash.update(&bytes);
        if bytes.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        report.records += 1;
        let parsed = serde_json::from_slice::<DecisionRecord>(&bytes);
        let record = match parsed {
            Ok(record) => record,
            Err(error) => {
                report.failed_records += 1;
                report.failures.push(CaseResult {
                    line: index + 1,
                    id: String::new(),
                    stage: String::new(),
                    differences: Vec::new(),
                    error: Some(format!("invalid capture: {error}")),
                });
                continue;
            }
        };
        *report.origins.entry(record.origin.clone()).or_default() += 1;
        *report
            .recorded_implementations
            .entry(record.implementation_id.clone())
            .or_default() += 1;
        let stage = report.stages.entry(record.stage.clone()).or_default();
        stage.records += 1;
        let mut case = CaseResult {
            line: index + 1,
            id: record.id.clone(),
            stage: record.stage.clone(),
            differences: Vec::new(),
            error: None,
        };
        match evaluate(&record) {
            Ok(actual) => {
                differences(&record.expected, &actual, "", &mut case.differences);
                if case.differences.is_empty() {
                    stage.matched += 1;
                } else {
                    stage.mismatched += 1;
                }
            }
            Err(error) => {
                stage.errors += 1;
                case.error = Some(format!("{error:#}"));
            }
        }
        if !case.differences.is_empty() || case.error.is_some() {
            report.failed_records += 1;
            report.failures.push(case);
        }
    }
    report.input_sha256 = format!("{:x}", hash.finalize());
    report.missing_stages = required_stages
        .iter()
        .filter(|s| !report.stages.contains_key(**s))
        .map(|s| (*s).into())
        .collect();
    report.matches_required_stages =
        report.records > 0 && report.failed_records == 0 && report.missing_stages.is_empty();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn fill_record() -> DecisionRecord {
        DecisionRecord {
            schema_version: SCHEMA_VERSION,
            id: "partial-depth".into(),
            captured_at: "2026-09-14T00:00:00Z".into(),
            origin: "synthetic_fixture".into(),
            implementation_id: implementation_id().into(),
            stage: "simulated_buy_fill".into(),
            input: json!({"book":{"asks":[{"price":0.4,"size":1.0},{"price":0.5,"size":1.0},{"price":0.8,"size":3.0}]},"limit_price":0.5,"size":4.0}),
            // Only two shares are available within the limit, at a 0.45 VWAP.
            expected: json!([0.45, 2.0]),
        }
    }

    fn report(record: &DecisionRecord) -> ReplayReport {
        compare(
            Cursor::new(serde_json::to_vec(record).unwrap()),
            &["simulated_buy_fill"],
        )
        .unwrap()
    }

    #[test]
    fn partial_depth_capture_matches_the_production_fill_algorithm() {
        let report = report(&fill_record());
        assert!(report.matches_required_stages);
        assert_eq!(report.origins["synthetic_fixture"], 1);
        assert_eq!(report.recorded_implementations[implementation_id()], 1);
    }

    #[test]
    fn different_implementation_cannot_pass_even_when_the_fill_would_match() {
        let mut record = fill_record();
        assert!(report(&record).matches_required_stages);
        record.implementation_id = "0".repeat(64);
        let report = report(&record);
        assert!(!report.matches_required_stages);
        assert_eq!(report.failed_records, 1);
        assert_eq!(report.stages["simulated_buy_fill"].matched, 0);
        assert_eq!(report.stages["simulated_buy_fill"].errors, 1);
        assert_eq!(
            report.recorded_implementations[&record.implementation_id],
            1
        );
        assert!(report.failures[0].differences.is_empty());
        assert!(report.failures[0]
            .error
            .as_deref()
            .unwrap()
            .contains("implementation mismatch"));
    }

    #[test]
    fn changed_fill_depth_reports_the_exact_mismatch() {
        let mut record = fill_record();
        record.input["book"]["asks"][0]["size"] = json!(2.0);
        let report = report(&record);
        assert!(!report.matches_required_stages);
        assert_eq!(report.failed_records, 1);
        assert!(report.failures[0]
            .differences
            .iter()
            .any(|d| d.path == "/1" && d.actual == json!(3.0)));
    }

    #[test]
    fn missing_input_or_unsupported_schema_is_an_error_not_a_match() {
        let mut record = fill_record();
        record.input.as_object_mut().unwrap().remove("size");
        assert_eq!(report(&record).stages["simulated_buy_fill"].errors, 1);
        record = fill_record();
        record.schema_version += 1;
        assert!(report(&record).failures[0]
            .error
            .as_ref()
            .unwrap()
            .contains("unsupported"));
    }

    #[test]
    fn empty_and_incomplete_captures_cannot_pass() {
        assert!(
            !compare(Cursor::new(b""), &[])
                .unwrap()
                .matches_required_stages
        );
        let report = compare(
            Cursor::new(serde_json::to_vec(&fill_record()).unwrap()),
            STAGES,
        )
        .unwrap();
        assert!(!report.matches_required_stages);
        assert!(report.missing_stages.contains(&"fusion".to_owned()));
        assert_eq!(
            compare(Cursor::new(b"{broken\n"), &[])
                .unwrap()
                .failed_records,
            1
        );
    }

    #[test]
    fn numeric_tolerance_does_not_hide_signal_or_schema_changes() {
        let mut out = Vec::new();
        differences(
            &json!({"signal":"HOLD","amount":1.0,"optional":null}),
            &json!({"signal":"BUY_YES","amount":1.0+1e-12}),
            "",
            &mut out,
        );
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|d| d.path == "/signal"));
        assert!(out.iter().any(|d| d.path == "/optional"));
    }

    #[test]
    fn replay_uses_recorded_risk_state_and_redacted_config() {
        let mut cfg: Config = serde_json::from_str(include_str!("../config.json")).unwrap();
        cfg.daily_loss_limit = 10.0;
        cfg.api_key = "must-not-appear-in-the-capture".into();
        let mut record = fill_record();
        record.stage = "trade_risk".into();
        record.input = json!({
            "config": cfg.redacted_value(),
            "snapshot": {"today_pnl":-11.0,"available_balance":100.0,"current_exposure":0.0,
                "macro_guard":crate::bot::macro_calendar::MacroGuardStatus::default(),
                "apply_live_rollout_guardrails":false,"require_shadow_pass":false,
                "shadow_passed":false,"rollout_stage":"full","canary_max_usd":null},
            "proposal":{"signal":"BuyYes","strategy":"conviction","confidence":0.8,
                "funding_rate":null,"regime":"TREND_UP","token_price":0.4,"liquidity":10000.0,
                "entry_hour_utc":12,"time_remaining_secs":300.0,"btc_change_pct":1.0,
                "net_ev":0.2,"forced":false,"hail_mary":false,"requested_amount":1.0}
        });
        assert!(!record.input.to_string().contains("must-not-appear"));
        let actual = evaluate(&record).unwrap();
        assert_eq!(
            actual,
            json!({"Err":{"DailyLossLimit":{"today_pnl":-11.0,"limit":10.0}}})
        );
        record.input["snapshot"]["today_pnl"] = json!(0.0);
        assert_ne!(evaluate(&record).unwrap(), actual);
    }
}
