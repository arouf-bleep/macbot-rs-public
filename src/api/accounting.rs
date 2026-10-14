use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use crate::bot::state::TradingBot;

#[derive(Debug, Deserialize, Default)]
pub struct AccountingTruthQuery {
    pub sync_authoritative: Option<bool>,
}

// GET /api/accounting/truth
pub async fn accounting_truth(
    State(bot): State<Arc<TradingBot>>,
    Query(q): Query<AccountingTruthQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if q.sync_authoritative.unwrap_or(false) {
        return Err((
            StatusCode::BAD_REQUEST,
            "balance synchronization requires signed POST /api/resync/full".into(),
        ));
    }
    Ok(Json(bot.accounting_truth_snapshot(false).await))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_only_accounting_endpoint_refuses_balance_mutation() -> anyhow::Result<()> {
        let pool = crate::db::open(":memory:").await?;
        let cfg = serde_json::from_str(include_str!("../../config.json"))?;
        let bot = Arc::new(TradingBot::new(cfg, pool).await?);
        let before = bot.balance().await;
        let error = accounting_truth(
            State(Arc::clone(&bot)),
            Query(AccountingTruthQuery {
                sync_authoritative: Some(true),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(bot.balance().await, before);
        bot.db.close().await;
        Ok(())
    }
}
