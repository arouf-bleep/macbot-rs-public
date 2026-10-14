//! Whale alert and on-chain flow signals.
//! Large BTC transfers to exchanges = sell pressure incoming (30-60s lead time).

#[derive(Debug, Clone, Default)]
pub struct WhaleData {
    /// Total USD value of large BTC transfers to exchanges in last 5 min
    pub exchange_inflow_usd: f64,
    /// Total USD value of large BTC transfers FROM exchanges in last 5 min
    pub exchange_outflow_usd: f64,
    /// Net flow: positive = net outflow (bullish), negative = net inflow (bearish)
    pub net_flow_usd: f64,
    /// Signal in [-1, 1]: negative = sell pressure, positive = accumulation
    pub signal: f64,
    /// Confidence [0, 1]
    pub confidence: f64,
}

/// Compute whale signal from on-chain transfer data.
///
/// # Parameters
/// - `inflow_usd`: total USD value of BTC transfers TO exchanges (sell pressure)
/// - `outflow_usd`: total USD value of BTC transfers FROM exchanges (accumulation)
pub fn compute_whale_signal(inflow_usd: f64, outflow_usd: f64) -> WhaleData {
    let net_flow = outflow_usd - inflow_usd;
    let _total = (inflow_usd + outflow_usd).max(1.0);

    // Normalize: $10M+ flows are max signal
    let inflow_norm = (inflow_usd / 10_000_000.0).clamp(0.0, 1.0);
    let outflow_norm = (outflow_usd / 10_000_000.0).clamp(0.0, 1.0);

    // Signal: net outflow is bullish (accumulation), net inflow is bearish (distribution)
    let net_signal = ((outflow_norm - inflow_norm) * 2.0).clamp(-1.0, 1.0);

    // Confidence scales with total flow magnitude
    let confidence = ((inflow_norm + outflow_norm) / 2.0).clamp(0.0, 1.0);

    WhaleData {
        exchange_inflow_usd: inflow_usd,
        exchange_outflow_usd: outflow_usd,
        net_flow_usd: net_flow,
        signal: net_signal,
        confidence,
    }
}
