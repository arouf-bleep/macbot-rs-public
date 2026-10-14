#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PhaseData {
    pub phase: String, // "EARLY" | "MID" | "LATE"
    pub edge_threshold: f64,
    pub conf_threshold: f64,
}

impl Default for PhaseData {
    fn default() -> Self {
        PhaseData {
            phase: "MID".into(),
            edge_threshold: 0.10,
            conf_threshold: 0.60,
        }
    }
}

/// Port of signals.py:compute_phase_decay()
pub fn compute_phase_decay(time_remaining_mins: f64) -> PhaseData {
    if time_remaining_mins > 10.0 {
        PhaseData {
            phase: "EARLY".into(),
            edge_threshold: 0.05,
            conf_threshold: 0.55,
        }
    } else if time_remaining_mins > 5.0 {
        PhaseData {
            phase: "MID".into(),
            edge_threshold: 0.10,
            conf_threshold: 0.60,
        }
    } else {
        PhaseData {
            phase: "LATE".into(),
            edge_threshold: 0.20,
            conf_threshold: 0.65,
        }
    }
}
