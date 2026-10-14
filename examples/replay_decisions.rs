//! Usage: cargo run --locked --example replay_decisions -- INPUT.jsonl REPORT.json [STAGE,...]
use std::{fs::File, io::BufReader};

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if !(3..=4).contains(&args.len()) {
        anyhow::bail!("usage: replay_decisions INPUT.jsonl REPORT.json [STAGE,...]");
    }
    let required = args
        .get(3)
        .map(|s| s.split(',').collect::<Vec<_>>())
        .unwrap_or_else(|| macbot_rs::replay::STAGES.to_vec());
    let report = macbot_rs::replay::compare(BufReader::new(File::open(&args[1])?), &required)?;
    std::fs::write(&args[2], serde_json::to_vec_pretty(&report)?)?;
    println!(
        "{} records; {} failures; missing stages: {:?}; report: {}",
        report.records, report.failed_records, report.missing_stages, args[2]
    );
    if !report.matches_required_stages {
        anyhow::bail!("replay comparison incomplete or mismatched");
    }
    Ok(())
}
