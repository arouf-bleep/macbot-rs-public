//! Dashboard API endpoints for deep model monitoring.

use axum::{extract::State, response::IntoResponse, Json};
use serde_json::json;
use std::sync::Arc;

use crate::bot::state::TradingBot;

/// GET /api/deep-model/status — Deep model + ensemble overview.
pub async fn deep_model_status(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    let model_info = {
        let guard = bot.intelligence.deep_model.read().await;
        match guard.as_ref() {
            Some(m) => json!({
                "loaded": true,
                "version": m.version,
                "loaded_ago_secs": m.loaded_at.elapsed().as_secs(),
            }),
            None => json!({
                "loaded": false,
                "version": null,
                "loaded_ago_secs": null,
            }),
        }
    };

    let (ensemble_info, last_prediction) = {
        let guard = bot.intelligence.ensemble_state.read().await;
        let ej = guard.to_json();
        let lp = json!({
            "deep_p_up": guard.last_deep_p_up,
            "blended_p_up": guard.last_blended_p_up,
            "mc_uncertainty": guard.last_mc_uncertainty,
            "should_skip": guard.last_should_skip,
        });
        (ej, lp)
    };

    let liq_info = {
        let guard = bot.intelligence.liq_tracker.lock().await;
        let summary_30s = guard.volume_in_window(30);
        let summary_5m = guard.volume_in_window(300);
        let spike = guard.spike_z_score();
        json!({
            "window_30s": {
                "long_vol": summary_30s.long_vol,
                "short_vol": summary_30s.short_vol,
                "ratio": summary_30s.ratio,
                "count": summary_30s.count,
            },
            "window_5m": {
                "long_vol": summary_5m.long_vol,
                "short_vol": summary_5m.short_vol,
                "ratio": summary_5m.ratio,
                "count": summary_5m.count,
            },
            "spike_z_score": spike,
        })
    };

    Json(json!({
        "model": model_info,
        "ensemble": ensemble_info,
        "last_prediction": last_prediction,
        "liquidations": liq_info,
    }))
}
