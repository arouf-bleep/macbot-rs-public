use std::collections::HashMap;
use std::sync::atomic::AtomicBool;

use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::task::JoinHandle;

use crate::market::l2_book::TopOfBook;

pub struct ExecutionState {
    pub execution_tx: RwLock<Option<mpsc::Sender<super::super::actor::ExecutionCommand>>>,
    pub execution_task: Mutex<Option<JoinHandle<Result<(), String>>>>,
    pub pending_bbo: Mutex<HashMap<String, TopOfBook>>,
    pub bbo_flush_enqueued: AtomicBool,
    pub live_execution_lock: Mutex<()>,
    /// Excludes operator unfreezing/configuration changes during receipt repair.
    /// API handlers use try_lock, so a repair never blocks emergency stops.
    pub recovery_control_gate: Mutex<()>,
}

impl Default for ExecutionState {
    fn default() -> Self {
        Self {
            execution_tx: RwLock::new(None),
            execution_task: Mutex::new(None),
            pending_bbo: Mutex::new(HashMap::new()),
            bbo_flush_enqueued: AtomicBool::new(false),
            live_execution_lock: Mutex::new(()),
            recovery_control_gate: Mutex::new(()),
        }
    }
}
