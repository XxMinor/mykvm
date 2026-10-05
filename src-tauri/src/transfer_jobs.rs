use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};

fn jobs() -> &'static Mutex<HashMap<String, Arc<AtomicBool>>> {
    static JOBS: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) struct TransferJob {
    id: String,
    cancelled: Arc<AtomicBool>,
}

impl TransferJob {
    pub fn new(id: &str) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        jobs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.into(), Arc::clone(&cancelled));
        Self {
            id: id.into(),
            cancelled,
        }
    }
    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

impl Drop for TransferJob {
    fn drop(&mut self) {
        if let Ok(mut jobs) = jobs().lock() {
            jobs.remove(&self.id);
        }
    }
}

pub(crate) fn cancel(id: &str) -> bool {
    jobs()
        .lock()
        .ok()
        .and_then(|jobs| jobs.get(id).cloned())
        .map(|job| job.store(true, Ordering::Relaxed))
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_only_affects_the_selected_active_job() {
        let a = TransferJob::new("test-cancel-a");
        let b = TransferJob::new("test-cancel-b");
        assert!(cancel("test-cancel-a"));
        assert!(a.cancelled());
        assert!(!b.cancelled());
        drop(a);
        assert!(!cancel("test-cancel-a"));
    }
}
