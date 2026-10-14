/// Estimate market impact (slippage) for an AMM trade under LMSR pricing.
/// Port of signals.py:estimate_lmsr_slippage()
pub fn estimate_lmsr_slippage(amount_usd: f64, price: f64, liquidity_usd: f64) -> f64 {
    if liquidity_usd <= 0.0 || price <= 0.0 || price >= 1.0 {
        return 0.0;
    }
    let b = liquidity_usd / 2_f64.ln();
    let slippage = amount_usd * (1.0 - price) / (2.0 * b);
    slippage.min(0.5) // cap at 50%
}
