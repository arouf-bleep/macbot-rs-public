use tokio::sync::Mutex;

use crate::signals::{OBIStabilityTracker, TickVelocityTracker};

pub struct SignalTrackers {
    pub obi_tracker: Mutex<OBIStabilityTracker>,
    pub tick_vel: Mutex<TickVelocityTracker>,
    pub correlation_tracker: Mutex<crate::bot::correlation_guard::CorrelationTracker>,
}

impl Default for SignalTrackers {
    fn default() -> Self {
        Self {
            obi_tracker: Mutex::new(OBIStabilityTracker::default()),
            tick_vel: Mutex::new(TickVelocityTracker::new()),
            correlation_tracker: Mutex::new(
                crate::bot::correlation_guard::CorrelationTracker::new(60),
            ),
        }
    }
}
