use std::time::{Duration, Instant};

/// A screenshot owns local input until its overlay closes. If the shortcut
/// simply repeats a capture without an overlay, restore after a short grace.
pub(crate) struct LocalCaptureSession {
    started: Instant,
    saw_overlay: bool,
    closed_since: Option<Instant>,
}
impl LocalCaptureSession {
    pub(crate) fn new(overlay: bool, now: Instant) -> Self {
        Self {
            started: now,
            saw_overlay: overlay,
            closed_since: None,
        }
    }
    pub(crate) fn ready_to_restore(&mut self, overlay: bool, now: Instant) -> bool {
        self.saw_overlay |= overlay;
        if overlay {
            self.closed_since = None;
            return false;
        }
        if self.saw_overlay {
            let closed = self.closed_since.get_or_insert(now);
            now.saturating_duration_since(*closed) >= Duration::from_millis(250)
        } else {
            now.saturating_duration_since(self.started) >= Duration::from_secs(2)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn screenshot_does_not_restore_before_its_overlay_opens_and_closes() {
        let now = Instant::now();
        let mut capture = LocalCaptureSession::new(false, now);
        assert!(!capture.ready_to_restore(false, now + Duration::from_millis(100)));
        assert!(!capture.ready_to_restore(true, now + Duration::from_millis(300)));
        assert!(!capture.ready_to_restore(true, now + Duration::from_secs(30)));
        assert!(!capture.ready_to_restore(false, now + Duration::from_secs(31)));
        assert!(capture.ready_to_restore(false, now + Duration::from_millis(31_250)));
    }
    #[test]
    fn overlay_already_opened_by_a_global_hook_restores_when_closed() {
        let now = Instant::now();
        let mut capture = LocalCaptureSession::new(true, now);
        assert!(!capture.ready_to_restore(true, now));
        assert!(!capture.ready_to_restore(false, now + Duration::from_millis(500)));
        assert!(capture.ready_to_restore(false, now + Duration::from_millis(750)));
    }
    #[test]
    fn repeat_capture_without_an_overlay_cannot_leave_input_local_forever() {
        let now = Instant::now();
        let mut capture = LocalCaptureSession::new(false, now);
        assert!(!capture.ready_to_restore(false, now + Duration::from_millis(1_999)));
        assert!(capture.ready_to_restore(false, now + Duration::from_secs(2)));
    }
}
