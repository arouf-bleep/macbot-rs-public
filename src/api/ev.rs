use axum::{http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;
use serde_json::json;

use crate::ev::{calculate_net_profit, EvInputs};

#[derive(Debug, Deserialize)]
pub struct EvRequest {
    pub price: f64,
    pub amount_usd: f64,
    pub liquidity_usd: f64,
    pub expected_edge_pct: f64,
    pub gas_usd: Option<f64>,
}

pub async fn calc_ev(
    Json(req): Json<EvRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    if !(0.0..=1.0).contains(&req.price) {
        return Err((
            StatusCode::BAD_REQUEST,
            "price must be in [0, 1]".to_string(),
        ));
    }
    if !req.amount_usd.is_finite() || req.amount_usd <= 0.0 {
        return Err((
            StatusCode::BAD_REQUEST,
            "amount_usd must be > 0".to_string(),
        ));
    }
    if !req.liquidity_usd.is_finite() || req.liquidity_usd < 0.0 {
        return Err((
            StatusCode::BAD_REQUEST,
            "liquidity_usd must be finite and >= 0".to_string(),
        ));
    }
    if !req.expected_edge_pct.is_finite() {
        return Err((
            StatusCode::BAD_REQUEST,
            "expected_edge_pct must be finite".to_string(),
        ));
    }
    let gas_usd = req.gas_usd.unwrap_or(0.0);
    if !gas_usd.is_finite() || gas_usd < 0.0 {
        return Err((
            StatusCode::BAD_REQUEST,
            "gas_usd must be finite and >= 0".to_string(),
        ));
    }

    let out = calculate_net_profit(EvInputs {
        price: req.price,
        amount_usd: req.amount_usd,
        liquidity_usd: req.liquidity_usd,
        edge_pct: req.expected_edge_pct,
        gas_usd,
    });

    if [
        out.taker_fee_pct,
        out.lmsr_slippage_pct,
        out.gas_pct,
        out.total_cost_pct,
        out.gross_edge_pct,
        out.net_edge_pct,
        out.gross_ev_usd,
        out.net_ev_usd,
    ]
    .iter()
    .any(|value| !value.is_finite())
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "EV calculation exceeds finite numeric limits".to_string(),
        ));
    }

    Ok(Json(json!({
        "inputs": {
            "price": req.price,
            "amount_usd": req.amount_usd,
            "liquidity_usd": req.liquidity_usd,
            "expected_edge_pct": req.expected_edge_pct,
            "gas_usd": gas_usd,
        },
        "taker_fee_pct": out.taker_fee_pct,
        "lmsr_slippage_pct": out.lmsr_slippage_pct,
        "gas_pct": out.gas_pct,
        "total_cost_pct": out.total_cost_pct,
        "gross_edge_pct": out.gross_edge_pct,
        "net_edge_pct": out.net_edge_pct,
        "gross_ev_usd": out.gross_ev_usd,
        "net_ev_usd": out.net_ev_usd,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> EvRequest {
        EvRequest {
            price: 0.5,
            amount_usd: 10.0,
            liquidity_usd: 1_000.0,
            expected_edge_pct: 0.1,
            gas_usd: Some(0.0),
        }
    }

    #[tokio::test]
    async fn invalid_cost_inputs_are_refused_instead_of_silently_clamped() {
        for value in [-1.0, f64::INFINITY, f64::NAN] {
            let mut req = input();
            req.liquidity_usd = value;
            assert!(matches!(
                calc_ev(Json(req)).await,
                Err((StatusCode::BAD_REQUEST, _))
            ));
            let mut req = input();
            req.gas_usd = Some(value);
            assert!(matches!(
                calc_ev(Json(req)).await,
                Err((StatusCode::BAD_REQUEST, _))
            ));
        }
        let mut req = input();
        req.amount_usd = f64::INFINITY;
        assert!(calc_ev(Json(req)).await.is_err());
        let mut req = input();
        req.expected_edge_pct = f64::NAN;
        assert!(calc_ev(Json(req)).await.is_err());
        assert!(calc_ev(Json(input())).await.is_ok());
    }

    #[tokio::test]
    async fn overflow_is_an_input_error_instead_of_a_successful_null_result() {
        let mut req = input();
        req.amount_usd = 1e308;
        req.expected_edge_pct = 1e308;
        assert!(matches!(
            calc_ev(Json(req)).await,
            Err((StatusCode::BAD_REQUEST, _))
        ));
    }
}
