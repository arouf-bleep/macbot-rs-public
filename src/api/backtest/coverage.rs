//! Coverage of accepted candle observations; missing time is never reconstructed.
use super::{candles::trade_clock, KLINE_15M_MS};
use serde::Serialize;

pub(super) fn feature_outcome_clock(
    times: &[i64],
    feature_index: usize,
    warmup: usize,
) -> Option<(i64, i64)> {
    if warmup == 0 {
        return None;
    }
    let outcome_index = feature_index.checked_add(1)?;
    let first_feature = outcome_index.checked_sub(warmup)?;
    let window = times.get(first_feature..=outcome_index)?;
    if window
        .iter()
        .any(|time| *time < 0 || *time % KLINE_15M_MS != 0)
        || !window.windows(2).all(|pair| {
            pair[0]
                .checked_add(KLINE_15M_MS)
                .is_some_and(|next| next == pair[1])
        })
    {
        return None;
    }
    trade_clock(times[feature_index], times[outcome_index])
}

#[derive(Debug, Serialize)]
pub(super) struct CandleCoverage {
    pub(super) interval_ms: i64,
    pub(super) requested_completed_candles: usize,
    pub(super) observed_completed_candles: usize,
    pub(super) expected_latest_completed_open_ms: Option<i64>,
    pub(super) observed_latest_completed_open_ms: Option<i64>,
    pub(super) reaches_requested_end_horizon: bool,
    pub(super) gap_count: usize,
    pub(super) missing_intervals: u64,
    pub(super) candidate_windows: usize,
    pub(super) eligible_contiguous_windows: usize,
    pub(super) rejected_noncontiguous_windows: usize,
    pub(super) requested_window_complete: bool,
    pub(super) effective_days_basis: &'static str,
}

pub(super) fn candle_coverage(
    times: &[i64],
    requested: usize,
    end_time_ms: i64,
    warmup: usize,
) -> CandleCoverage {
    let expected_latest = end_time_ms
        .div_euclid(KLINE_15M_MS)
        .checked_mul(KLINE_15M_MS)
        .and_then(|aligned_end| aligned_end.checked_sub(KLINE_15M_MS))
        .filter(|open| *open >= 0);
    let observed_latest = times.last().copied();
    let mut gap_count = 0;
    let mut missing_intervals = 0u64;
    for pair in times.windows(2) {
        if let Some(delta) = pair[1].checked_sub(pair[0]) {
            if delta > KLINE_15M_MS {
                gap_count += 1;
                missing_intervals += (delta / KLINE_15M_MS - 1) as u64;
            }
        }
    }
    let candidate_windows = times.len().saturating_sub(warmup.saturating_add(1));
    let eligible = (warmup..times.len().saturating_sub(1))
        .filter(|index| feature_outcome_clock(times, *index, warmup).is_some())
        .count();
    let reaches_horizon = expected_latest.is_some() && observed_latest == expected_latest;
    CandleCoverage {
        interval_ms: KLINE_15M_MS,
        requested_completed_candles: requested,
        observed_completed_candles: times.len(),
        expected_latest_completed_open_ms: expected_latest,
        observed_latest_completed_open_ms: observed_latest,
        reaches_requested_end_horizon: reaches_horizon,
        gap_count,
        missing_intervals,
        candidate_windows,
        eligible_contiguous_windows: eligible,
        rejected_noncontiguous_windows: candidate_windows - eligible,
        requested_window_complete: times.len() >= requested && reaches_horizon && gap_count == 0,
        effective_days_basis: "eligible_contiguous_feature_and_outcome_windows_divided_by_96",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_feature_and_outcome_boundary_requires_a_contiguous_interval() {
        let warmup = 40;
        let baseline: Vec<_> = (0..=warmup + 1).map(|i| i as i64 * KLINE_15M_MS).collect();
        assert!(feature_outcome_clock(&baseline, warmup, warmup).is_some());
        // The live feature window here starts at index1 and ends at40;
        // outcome41 must follow all forty consecutive observations.
        for boundary in 2..=warmup + 1 {
            let mut gapped = baseline.clone();
            for time in &mut gapped[boundary..] {
                *time += KLINE_15M_MS;
            }
            assert!(
                feature_outcome_clock(&gapped, warmup, warmup).is_none(),
                "boundary {boundary}"
            );
        }
        assert!(feature_outcome_clock(&baseline, warmup, 0).is_none());
        assert!(feature_outcome_clock(&baseline, usize::MAX, warmup).is_none());
        assert!(feature_outcome_clock(&baseline[..warmup], warmup, warmup).is_none());
    }

    #[test]
    fn coverage_distinguishes_horizon_shortage_gaps_and_eligible_time() {
        let end = 10 * KLINE_15M_MS;
        let times: Vec<_> = (0..10).map(|i| i * KLINE_15M_MS).collect();
        for requested_end in [end, end + KLINE_15M_MS / 2] {
            let full = candle_coverage(&times, 10, requested_end, 3);
            assert!(full.requested_window_complete);
            assert_eq!(full.eligible_contiguous_windows, 6);
            assert_eq!(
                full.expected_latest_completed_open_ms,
                Some(9 * KLINE_15M_MS)
            );
        }
        let mut gap = times.clone();
        gap.remove(5);
        let coverage = candle_coverage(&gap, 10, end, 3);
        assert!(!coverage.requested_window_complete);
        assert!(coverage.reaches_requested_end_horizon);
        assert_eq!(coverage.gap_count, 1);
        assert_eq!(coverage.missing_intervals, 1);
        assert_eq!(coverage.eligible_contiguous_windows, 2);
        assert_eq!(coverage.rejected_noncontiguous_windows, 3);
        let early = candle_coverage(&times[..8], 10, end, 3);
        assert!(!early.reaches_requested_end_horizon);
        assert_eq!(early.gap_count, 0);
        assert!(!early.requested_window_complete);
    }
}
