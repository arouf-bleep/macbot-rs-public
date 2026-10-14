/// Distance from nearest round number, normalized.
/// Returns 0.0 when price is exactly on a round number, 1.0 at maximum distance (midpoint).
pub fn compute_distance_from_round(price: f64, round_level: f64) -> f64 {
    let remainder = price % round_level;
    let half = round_level / 2.0;
    let distance = if remainder > half {
        round_level - remainder
    } else {
        remainder
    };
    distance / half // 0.0 at round number, 1.0 at midpoint
}
