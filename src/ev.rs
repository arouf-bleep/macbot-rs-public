use crate::fusion::estimate_taker_fee;
use crate::signals::lmsr::estimate_lmsr_slippage;

#[derive(Debug, Clone, Copy)]
pub struct EvInputs {
    pub price: f64,
    pub amount_usd: f64,
    pub liquidity_usd: f64,
    pub edge_pct: f64,
    pub gas_usd: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct EvBreakdown {
    pub taker_fee_pct: f64,
    pub lmsr_slippage_pct: f64,
    pub gas_pct: f64,
    pub total_cost_pct: f64,
    pub gross_edge_pct: f64,
    pub net_edge_pct: f64,
    pub gross_ev_usd: f64,
    pub net_ev_usd: f64,
}

pub fn calculate_net_profit(inputs: EvInputs) -> EvBreakdown {
    let amount = inputs.amount_usd.max(0.0);
    let price = inputs.price.clamp(0.0001, 0.9999);
    let liquidity = inputs.liquidity_usd.max(0.0);
    let gas = inputs.gas_usd.max(0.0);

    if amount <= 0.0 {
        return EvBreakdown {
            taker_fee_pct: 0.0,
            lmsr_slippage_pct: 0.0,
            gas_pct: 0.0,
            total_cost_pct: 0.0,
            gross_edge_pct: inputs.edge_pct,
            net_edge_pct: inputs.edge_pct,
            gross_ev_usd: 0.0,
            net_ev_usd: 0.0,
        };
    }

    let taker_fee_pct = estimate_taker_fee(price);
    let lmsr_slippage_pct = estimate_lmsr_slippage(amount, price, liquidity);
    let gas_pct = gas / amount;
    let total_cost_pct = taker_fee_pct + lmsr_slippage_pct + gas_pct;
    let gross_edge_pct = inputs.edge_pct;
    let net_edge_pct = gross_edge_pct - total_cost_pct;
    let gross_ev_usd = gross_edge_pct * amount;
    let net_ev_usd = net_edge_pct * amount;

    EvBreakdown {
        taker_fee_pct,
        lmsr_slippage_pct,
        gas_pct,
        total_cost_pct,
        gross_edge_pct,
        net_edge_pct,
        gross_ev_usd,
        net_ev_usd,
    }
}

#[cfg(test)]
mod tests {
    use super::{calculate_net_profit, EvInputs};
    use crate::fusion::estimate_taker_fee;
    use crate::signals::lmsr::estimate_lmsr_slippage;

    fn approx_eq(a: f64, b: f64) {
        assert!(
            (a - b).abs() < 1e-12,
            "expected {a} to be approximately {b}"
        );
    }

    #[test]
    fn zero_amount_short_circuits_costs_and_ev() {
        let breakdown = calculate_net_profit(EvInputs {
            price: 0.55,
            amount_usd: 0.0,
            liquidity_usd: 1_000.0,
            edge_pct: 0.04,
            gas_usd: 0.25,
        });

        approx_eq(breakdown.taker_fee_pct, 0.0);
        approx_eq(breakdown.total_cost_pct, 0.0);
        approx_eq(breakdown.gross_edge_pct, 0.04);
        approx_eq(breakdown.net_edge_pct, 0.04);
        approx_eq(breakdown.gross_ev_usd, 0.0);
        approx_eq(breakdown.net_ev_usd, 0.0);
    }

    #[test]
    fn ev_breakdown_matches_fee_slippage_and_gas_components() {
        let inputs = EvInputs {
            price: 0.50,
            amount_usd: 100.0,
            liquidity_usd: 500.0,
            edge_pct: 0.08,
            gas_usd: 1.25,
        };

        let breakdown = calculate_net_profit(inputs);
        let expected_taker_fee = estimate_taker_fee(inputs.price);
        let expected_slippage =
            estimate_lmsr_slippage(inputs.amount_usd, inputs.price, inputs.liquidity_usd);
        let expected_gas_pct = inputs.gas_usd / inputs.amount_usd;
        let expected_total_cost = expected_taker_fee + expected_slippage + expected_gas_pct;

        approx_eq(breakdown.taker_fee_pct, expected_taker_fee);
        approx_eq(breakdown.lmsr_slippage_pct, expected_slippage);
        approx_eq(breakdown.gas_pct, expected_gas_pct);
        approx_eq(breakdown.total_cost_pct, expected_total_cost);
        approx_eq(breakdown.gross_ev_usd, inputs.edge_pct * inputs.amount_usd);
        approx_eq(
            breakdown.net_ev_usd,
            (inputs.edge_pct - expected_total_cost) * inputs.amount_usd,
        );
    }

    #[test]
    fn inputs_are_clamped_before_costs_are_computed() {
        let raw = calculate_net_profit(EvInputs {
            price: 1.5,
            amount_usd: 50.0,
            liquidity_usd: -10.0,
            edge_pct: 0.03,
            gas_usd: -2.0,
        });
        let clamped = calculate_net_profit(EvInputs {
            price: 0.9999,
            amount_usd: 50.0,
            liquidity_usd: 0.0,
            edge_pct: 0.03,
            gas_usd: 0.0,
        });

        approx_eq(raw.taker_fee_pct, clamped.taker_fee_pct);
        approx_eq(raw.lmsr_slippage_pct, clamped.lmsr_slippage_pct);
        approx_eq(raw.gas_pct, clamped.gas_pct);
        approx_eq(raw.total_cost_pct, clamped.total_cost_pct);
        approx_eq(raw.net_ev_usd, clamped.net_ev_usd);
    }
}
