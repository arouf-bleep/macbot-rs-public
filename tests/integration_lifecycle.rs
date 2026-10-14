use std::sync::Arc;

use macbot_rs::bot::state::TradingBot;
use macbot_rs::config::Config;
use macbot_rs::db;
use serde_json::json;

async fn integration_bot(
    label: &str,
) -> anyhow::Result<(Arc<TradingBot>, std::path::PathBuf, String)> {
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_integ_ext_{label}_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await?;

    let mut cfg: Config = serde_json::from_str(include_str!("../config.json"))?;
    cfg.mode = "simulation".to_string();
    cfg.execution_mode = "taker".to_string();
    cfg.shadow_trading = false;
    cfg.sim_live_execution = false;
    cfg.auto_trading = true;
    cfg.min_entry_price = 0.2;
    cfg.max_entry_price = 0.9;
    cfg.min_ev = 0.0;
    cfg.edge_safety_buffer = 0.0;
    cfg.trade_gas_usd = 0.0;

    let bot = Arc::new(TradingBot::new(cfg, pool).await?);
    bot.start_execution_actor().await;
    Ok((bot, path, path_str))
}

fn cleanup_sqlite(path: &std::path::Path, path_str: &str) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
}

fn buy_yes_analysis(market_id: &str, price: f64) -> serde_json::Value {
    json!({
        "signal": "BUY_YES",
        "confidence": 0.95,
        "raw_confidence": 0.95,
        "net_ev": 0.20,
        "forced": true,
        "reason": "integration-test",
        "regime": "BULL",
        "p_up": 0.71,
        "btc_change_pct": 1.2,
        "market_data": {
            "market_id": market_id,
            "question": format!("test-market-{market_id}"),
            "price": price,
            "end_date": "2099-01-01T00:00:00Z",
            "liquidity": 20000.0
        },
        "signals": {"orderbook_imbalance": 0.6}
    })
}

#[tokio::test]
async fn open_close_db_matches_hot_state() -> anyhow::Result<()> {
    let (bot, path, path_str) = integration_bot("db_match").await?;
    let initial = bot.balance().await;

    let analysis = buy_yes_analysis("mkt-db-match-ext", 0.55);
    let opened = bot
        .execute_trade_serialized_result(&analysis, false, Some(1.0))
        .await
        .expect("actor call should succeed")
        .expect("trade should open");
    assert!(opened.is_open());

    let hot_open = bot
        .portfolio_state
        .trades
        .read()
        .await
        .iter()
        .filter(|t| t.is_open())
        .count();
    let db_open = db::load_open_trades(&bot.db).await?.len();
    assert_eq!(hot_open, db_open, "hot vs DB open count after open");

    let closed = bot
        .close_trade_serialized(opened.id, Some(0.80))
        .await
        .expect("trade should close");

    let hot_open_after = bot
        .portfolio_state
        .trades
        .read()
        .await
        .iter()
        .filter(|t| t.is_open())
        .count();
    let db_open_after = db::load_open_trades(&bot.db).await?.len();
    assert_eq!(hot_open_after, 0);
    assert_eq!(db_open_after, 0);

    let db_trade = db::load_trade_by_id(&bot.db, opened.id)
        .await?
        .expect("closed trade should be in DB");
    assert_eq!(db_trade.status, "CLOSED");
    assert!(db_trade.pnl.is_some());
    assert!(db_trade.exit_price.is_some());

    let final_bal = bot.balance().await;
    let expected = initial + closed.pnl.unwrap_or(0.0) - closed.fee_paid;
    assert!(
        (final_bal - expected).abs() < 1e-9,
        "balance mismatch: got {final_bal}, expected {expected}"
    );

    drop(bot);
    cleanup_sqlite(&path, &path_str);
    Ok(())
}
