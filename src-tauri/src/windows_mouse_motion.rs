use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// SetCursorPos can re-enter the mouse hook before the caller releases its
/// remote-state lock. Recognize those moves before touching capture state.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) struct CursorWarps {
    pending: VecDeque<((i32, i32), Instant)>,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
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
/// deliberate push, even slow one-pixel steps, still crosses without a timer,
/// and a cursor swept across the edge crosses on the spot.
#[derive(Default)]
pub(crate) struct EdgeReturnGate {
    pressure: f64,
    last_push: Option<Instant>,
    /// The unbroken outward run the cursor is on: when it began, its last
    /// step, and how far it has gone.
    run: Option<(Instant, Instant, f64)>,
}

/// A run ends at a pause this long (or at any step back).
const RUN_PAUSE: Duration = Duration::from_millis(60);
/// A run this long and this fast (px, px/s) arriving at the edge is a sweep
/// across it, not a cursor that was resting there.
const SWEEP_DISTANCE: f64 = 16.0;
const SWEEP_SPEED: f64 = 300.0;

impl EdgeReturnGate {
    pub(crate) fn reset(&mut self) {
        self.pressure = 0.0;
        self.last_push = None;
        self.run = None;
    }

    /// A move that stayed on the remote screen. Travel toward the entry edge
    /// is remembered: without it a cursor swept across the edge stopped dead
    /// there for the 8 px a resting cursor has to push, a hitch on each return.
    pub(crate) fn approach(&mut self, outward: f64, now: Instant) {
        self.pressure = 0.0;
        self.last_push = None;
        self.extend_run(outward, now);
    }

    fn extend_run(&mut self, outward: f64, now: Instant) {
        if !outward.is_finite() || outward <= 0.0 {
            self.run = None;
            return;
        }
        match &mut self.run {
            Some((_, last, distance)) if now.saturating_duration_since(*last) <= RUN_PAUSE => {
                *last = now;
                *distance += outward;
            }
            _ => self.run = Some((now, now, outward)),
        }
    }

    fn swept(&self, now: Instant) -> bool {
        self.run.is_some_and(|(began, _, distance)| {
            distance >= SWEEP_DISTANCE
                && distance >= SWEEP_SPEED * now.saturating_duration_since(began).as_secs_f64()
        })
    }

    pub(crate) fn push(&mut self, outward: f64, now: Instant) -> bool {
        if !outward.is_finite() || outward <= 0.0 {
            self.reset();
            return false;
        }
        self.extend_run(outward, now);
        if self.swept(now) {
            self.reset();
            return true;
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

    #[test]
    fn a_cursor_swept_across_the_edge_returns_without_stopping_there() {
        let now = Instant::now();
        let step = |index: u64| now + Duration::from_millis(index * 8);
        // 5 px every 8 ms (625 px/s) up to the edge: the first step past it returns.
        let mut gate = EdgeReturnGate::default();
        for index in 0..10 {
            gate.approach(5.0, step(index));
        }
        assert!(gate.push(1.0, step(10)));
        // Creeping up to it (125 px/s, as when reaching for something at the
        // edge) still has to push.
        let mut gate = EdgeReturnGate::default();
        for index in 0..60 {
            gate.approach(1.0, step(index));
        }
        assert!(!gate.push(1.0, step(60)));
        // So does a sweep that stopped short of the edge, or turned back.
        let mut gate = EdgeReturnGate::default();
        for index in 0..10 {
            gate.approach(5.0, step(index));
        }
        assert!(!gate.push(1.0, step(10) + Duration::from_millis(200)));
        let mut gate = EdgeReturnGate::default();
        for index in 0..10 {
            gate.approach(5.0, step(index));
        }
        gate.approach(-1.0, step(10));
        assert!(!gate.push(1.0, step(11)));
    }
}
