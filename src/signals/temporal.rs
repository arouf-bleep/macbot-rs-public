use chrono::{Datelike, Timelike, Utc};

/// Hour of day normalized to [0, 1] (0 = midnight UTC, 1 = 23:00 UTC)
pub fn compute_hour_of_day() -> f64 {
    Utc::now().hour() as f64 / 23.0
}

/// Day of week normalized to [0, 1] (0 = Monday, 1 = Sunday)
pub fn compute_day_of_week() -> f64 {
    Utc::now().weekday().num_days_from_monday() as f64 / 6.0
}
