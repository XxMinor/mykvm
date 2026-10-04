use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// SetCursorPos can re-enter the mouse hook before the caller releases its
/// remote-state lock. Recognize those moves before touching capture state.
pub(crate) struct CursorWarps {
    pending: VecDeque<((i32, i32), Instant)>,
}

impl CursorWarps {
    pub(crate) const fn new() -> Self {
        Self {
            pending: VecDeque::new(),
        }
    }

    pub(crate) fn record(&mut self, point: (i32, i32), now: Instant) {
        self.expire(now);
        if let Some((previous, time)) = self.pending.back_mut() {
            if *previous == point {
                *time = now;
                return;
            }
        }
        if self.pending.len() == 8 {
            self.pending.pop_front();
        }
        self.pending.push_back((point, now));
    }

    pub(crate) fn take(&mut self, point: (i32, i32), now: Instant) -> bool {
        self.expire(now);
        let Some(index) = self
            .pending
            .iter()
            .position(|(expected, _)| *expected == point)
        else {
            return false;
        };
        self.pending.remove(index);
        true
    }

    fn expire(&mut self, now: Instant) {
        self.pending
            .retain(|(_, time)| now.saturating_duration_since(*time) <= Duration::from_millis(100));
    }
}

/// Small jitter at the client's entry edge must not hand control back. A
/// deliberate push, even slow one-pixel steps, still crosses without a timer.
#[derive(Default)]
pub(crate) struct EdgeReturnGate {
    pressure: f64,
    last_push: Option<Instant>,
}

impl EdgeReturnGate {
    pub(crate) fn reset(&mut self) {
        self.pressure = 0.0;
        self.last_push = None;
    }

    pub(crate) fn push(&mut self, outward: f64, now: Instant) -> bool {
        if !outward.is_finite() || outward <= 0.0 {
            self.reset();
            return false;
        }
        if self
            .last_push
            .is_none_or(|last| now.saturating_duration_since(last) > Duration::from_millis(200))
        {
            self.pressure = 0.0;
        }
        self.last_push = Some(now);
        self.pressure += outward;
        if self.pressure < 8.0 {
            return false;
        }
        self.reset();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_warps_are_consumed_once_without_swallowing_real_motion() {
        let now = Instant::now();
        let mut warps = CursorWarps::new();
        warps.record((1280, 720), now);
        warps.record((0, 600), now);
        assert!(!warps.take((1279, 720), now));
        assert!(warps.take((1280, 720), now));
        assert!(warps.take((0, 600), now));
        assert!(!warps.take((0, 600), now));
        warps.record((100, 200), now);
        assert!(!warps.take((100, 200), now + Duration::from_millis(101)));
    }

    #[test]
    fn isolated_edge_jitter_never_returns_but_a_slow_or_fast_push_does() {
        let now = Instant::now();
        let mut gate = EdgeReturnGate::default();
        for i in 0..20 {
            assert!(!gate.push(1.0, now + Duration::from_secs(i)));
        }
        gate.reset();
        for i in 0..7 {
            assert!(!gate.push(1.0, now + Duration::from_millis(i * 10)));
        }
        assert!(gate.push(1.0, now + Duration::from_millis(70)));
        assert!(gate.push(40.0, now + Duration::from_secs(1)));
        assert!(!gate.push(7.0, now + Duration::from_secs(2)));
        gate.reset(); // An inward move or a screen switch clears edge pressure.
        assert!(!gate.push(1.0, now + Duration::from_secs(2)));
    }
}
