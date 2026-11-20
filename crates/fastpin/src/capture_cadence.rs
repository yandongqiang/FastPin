//! Adaptive capture cadence for long screenshots.
//!
//! Ported from snow-shot's `AdaptiveScrollingCaptureCadence`
//! (`.snow-shot-src/snow_shot/src/presentation/capture/adaptivescrollingcapturecadence.h`).
//! We deliberately re-implement the *algorithm* in Rust rather than translating
//! the C++ verbatim: snow-shot is GPL-3.0-or-later and the scheduling layer is
//! simple enough that a clean-room Rust implementation keeps FastPin's own
//! licensing boundary intact. The heavy lifting (ORB matching/stitching) stays
//! in the already-vendored `snow-stitch-images` crate.
//!
//! Why this exists: the old poll loop ran at a fixed 150 ms (~6.7 fps) no
//! matter how fast the machine could stitch or how far behind it fell. A fixed
//! rate either wastes CPU on a fast machine (polling for a frame that has not
//! changed) or queues frames the worker cannot keep up with on a slow one.
//! This module watches two costs per frame -- capture (UI thread, D3D readback)
//! and stitch (worker thread, ORB + canvas write) -- and picks a target frame
//! rate from the slower of the two, with `capacityHeadroom` so the pipeline
//! never runs at 100 % utilisation.
//!
//! The controller is pure arithmetic: no timers, no Win32, no GPUI. `long_tick`
//! feeds it timings and asks [`CaptureCadence::interval`] for the next poll
//! delay, which keeps the whole policy unit-testable.

use std::time::Duration;

/// Slowest allowed rate. snow-shot uses 1; FastPin keeps a floor of 2 fps so a
/// genuinely overloaded pipeline does not degenerate into a multi-second poll
/// that feels frozen.
pub const MIN_FPS: u32 = 2;
/// Hard ceiling, matching snow-shot's absolute maximum.
pub const MAX_FPS: u32 = 30;
/// Starting rate, close to the old fixed 150 ms (6.7 fps) so existing sessions
/// keep their familiar pace until the first few samples arrive.
pub const INITIAL_FPS: u32 = 8;

/// Leave headroom below the measured stage cost: a stage that takes 40 ms is
/// treated as if it took 50 ms when choosing the sustainable rate. snow-shot's
/// `capacityHeadroom`, avoids running the pipeline at 100 % utilisation.
const CAPACITY_HEADROOM: f64 = 1.25;
/// Weight of the newest sample in the EWMA of the limiting stage cost.
const EWMA_WEIGHT: f64 = 0.25;
/// Consecutive "sustainable is better" samples required before the target is
/// nudged up by one fps. Fast fall, slow rise -- identical to snow-shot.
const RECOVERY_SAMPLES: u32 = 4;
/// A mailbox at or above this depth counts as backpressure and drops the
/// target by 25 %. FastPin's gate is single-slot, so this is reached when
/// several consecutive polls found the worker still busy.
const PRESSURE_QUEUE_DEPTH: usize = 2;

/// Which stage is currently the bottleneck. Purely diagnostic -- the controller
/// always uses the slower stage, but exposing the name makes the status line /
/// logs explain *why* the rate changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitingStage {
    Capture,
    Stitch,
}

/// Stateful frame-rate controller. Cheap to clone, no allocations.
#[derive(Clone, Debug)]
pub struct CaptureCadence {
    target_fps: u32,
    /// EWMA of the slower stage's cost in milliseconds. `0.0` means "no sample
    /// yet" and is treated as the initial stage cost.
    ewma_cost_ms: f64,
    /// Consecutive samples that said "sustainable > target".
    recovery_samples: u32,
    limiting: LimitingStage,
}

impl Default for CaptureCadence {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureCadence {
    pub fn new() -> Self {
        Self {
            target_fps: INITIAL_FPS,
            ewma_cost_ms: 0.0,
            recovery_samples: 0,
            limiting: LimitingStage::Capture,
        }
    }

    /// Current target frame rate.
    pub fn target_fps(&self) -> u32 {
        self.target_fps
    }

    /// Delay the poll loop should wait before the next tick, derived directly
    /// from the target rate. Bounded so a pathological target cannot produce a
    /// zero-duration timer.
    pub fn interval(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.target_fps as f64)
    }

    /// The stage currently limiting throughput (diagnostics only).
    pub fn limiting_stage(&self) -> LimitingStage {
        self.limiting
    }

    /// Sustainable fps for a stage cost in milliseconds: how many frames per
    /// second fit into one second once `CAPACITY_HEADROOM` is reserved.
    fn sustainable_fps(cost_ms: f64) -> u32 {
        if !cost_ms.is_finite() || cost_ms <= 0.0 {
            return MAX_FPS;
        }
        let rate = (1000.0 / (cost_ms * CAPACITY_HEADROOM)).floor();
        (rate as i64).clamp(MIN_FPS as i64, MAX_FPS as i64) as u32
    }

    /// Slow-rise / fast-fall update of the target rate, matching snow-shot's
    /// `updateTarget`: if the sustainable rate is below the current target we
    /// drop immediately; if it is above we need `RECOVERY_SAMPLES` consecutive
    /// confirming samples before raising by one fps.
    fn update_target(&mut self, sustainable: u32) {
        if sustainable < self.target_fps {
            self.target_fps = sustainable.max(MIN_FPS);
            self.recovery_samples = 0;
        } else if sustainable > self.target_fps {
            self.recovery_samples = self.recovery_samples.saturating_add(1);
            if self.recovery_samples >= RECOVERY_SAMPLES {
                self.target_fps = (self.target_fps + 1).min(sustainable).min(MAX_FPS);
                self.recovery_samples = 0;
            }
        } else {
            self.recovery_samples = 0;
        }
    }

    /// Fold one fully processed frame's timings in. `capture_ms` is measured on
    /// the UI thread (D3D readback), `stitch_ms` on the worker (ORB + canvas).
    /// The slower stage is the bottleneck and drives the EWMA.
    pub fn record_frame(&mut self, capture_ms: f64, stitch_ms: f64) {
        let (cost, stage) = if capture_ms >= stitch_ms {
            (capture_ms, LimitingStage::Capture)
        } else {
            (stitch_ms, LimitingStage::Stitch)
        };
        self.limiting = stage;
        if !cost.is_finite() || cost <= 0.0 {
            return;
        }
        self.ewma_cost_ms = if self.ewma_cost_ms == 0.0 {
            cost
        } else {
            self.ewma_cost_ms * (1.0 - EWMA_WEIGHT) + cost * EWMA_WEIGHT
        };
        let sustainable = Self::sustainable_fps(self.ewma_cost_ms);
        self.update_target(sustainable);
    }

    /// Report backpressure (a dropped frame or a full queue). The target is
    /// cut by 25 % and clamped to the sustainable rate, then the recovery
    /// counter is cleared so a single burst cannot ratchet the rate back up.
    pub fn record_pressure(&mut self, queue_depth: usize) {
        if queue_depth < PRESSURE_QUEUE_DEPTH {
            return;
        }
        let reduced = (self.target_fps as f64 * 0.75).floor() as u32;
        let sustainable = Self::sustainable_fps(self.ewma_cost_ms);
        self.target_fps = self.target_fps.min(reduced).min(sustainable).max(MIN_FPS);
        self.recovery_samples = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_the_initial_rate() {
        let cadence = CaptureCadence::new();
        assert_eq!(cadence.target_fps(), INITIAL_FPS);
        assert_eq!(
            cadence.interval(),
            Duration::from_secs_f64(1.0 / INITIAL_FPS as f64)
        );
    }

    #[test]
    fn a_slow_stitch_drops_the_rate_immediately() {
        let mut cadence = CaptureCadence::new();
        // 200 ms stitch => sustainable = 1000 / (200 * 1.25) = 4 fps.
        for _ in 0..1 {
            cadence.record_frame(5.0, 200.0);
        }
        assert_eq!(cadence.target_fps(), 4);
        assert_eq!(cadence.limiting_stage(), LimitingStage::Stitch);
    }

    #[test]
    fn rate_rises_only_after_four_good_samples() {
        let mut cadence = CaptureCadence::new();
        // First slow sample knocks the target down.
        cadence.record_frame(5.0, 200.0);
        let lowered = cadence.target_fps();
        assert!(lowered < INITIAL_FPS);
        // Fast frames: the EWMA needs a few samples before sustainable exceeds
        // target, and then four confirming samples before a +1 nudge.
        for _ in 0..40 {
            cadence.record_frame(1.0, 1.0);
        }
        assert!(cadence.target_fps() > lowered);
        assert!(cadence.target_fps() <= MAX_FPS);
        assert_eq!(cadence.limiting_stage(), LimitingStage::Capture);
    }

    #[test]
    fn backpressure_cuts_the_target_by_a_quarter() {
        let mut cadence = CaptureCadence::new();
        // Establish a sustainable rate so pressure has something to clamp to.
        cadence.record_frame(20.0, 20.0);
        let before = cadence.target_fps();
        // Depth below the threshold is ignored.
        cadence.record_pressure(1);
        assert_eq!(cadence.target_fps(), before);
        // Depth at the threshold drops it.
        cadence.record_pressure(2);
        assert!(cadence.target_fps() < before);
        assert!(cadence.target_fps() >= MIN_FPS);
    }

    #[test]
    fn costs_are_bounded_and_finite() {
        let mut cadence = CaptureCadence::new();
        cadence.record_frame(f64::NAN, f64::INFINITY);
        assert!(cadence.target_fps() >= MIN_FPS);
        assert!(cadence.target_fps() <= MAX_FPS);
        cadence.record_frame(0.0, 0.0);
        assert!(cadence.interval() >= Duration::from_secs_f64(1.0 / MAX_FPS as f64));
    }
}
