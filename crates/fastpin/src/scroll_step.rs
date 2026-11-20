//! Configurable scroll step for long screenshots.
//!
//! snow-shot hard-codes its 120 px step in two places
//! (`screenshotscrollingautoscroller.h` and `scrollingstepinput.h`) and its
//! interval bounds in a third (`screenshotscrollingtypes.h`). This module is
//! the single source of truth for all three: the beat interval, the step
//! magnitude, and their clamps. It is pure arithmetic (no Win32, no GPUI), so
//! the policy is unit-testable and cannot drift between an auto beat and a
//! manual single step.
//!
//! Note on units: `WM_MOUSEWHEEL` carries a *signed delta* in the high word of
//! `wParam`, not a pixel count. `WHEEL_DELTA` (120) is one native notch; each
//! app decides how many pixels that means. A step of N notches therefore posts
//! `N * WHEEL_DELTA`, which is the closest cross-app equivalent of snow-shot's
//! fixed 120 px and keeps working on apps that do not expose a scroll API.

use std::time::Duration;

/// Default auto-scroll beat. 600 ms keeps each notch inside the ORB estimator's
/// `max_motion_ratio` window while reading as a smooth, deliberate step.
pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(600);
/// Fastest beat snow-shot allows (its `minimumScrollingIntervalMs`).
pub const MIN_INTERVAL: Duration = Duration::from_millis(128);
/// Slowest beat snow-shot allows (its `maximumScrollingIntervalMs`).
pub const MAX_INTERVAL: Duration = Duration::from_millis(1000);
/// One native notch per step, matching today's behaviour.
pub const DEFAULT_NOTCHES: u32 = 1;
/// Upper bound so a single step can never outrun the estimator's motion window
/// (many apps scroll ~150 px/notch, so 8 notches is already a large jump).
pub const MAX_NOTCHES: u32 = 8;

/// The full scroll-step configuration: how often to post, and how far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollStep {
    pub interval: Duration,
    pub notches: u32,
}

impl Default for ScrollStep {
    fn default() -> Self {
        Self::new()
    }
}

impl ScrollStep {
    pub fn new() -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
            notches: DEFAULT_NOTCHES,
        }
    }

    /// Sets the beat interval, clamped to snow-shot's `[128, 1000] ms` range.
    pub fn set_interval(&mut self, interval: Duration) {
        self.interval = interval.clamp(MIN_INTERVAL, MAX_INTERVAL);
    }

    /// Sets the number of native notches per step, clamped to `[1, 8]`.
    pub fn set_notches(&mut self, notches: u32) {
        self.notches = notches.clamp(DEFAULT_NOTCHES, MAX_NOTCHES);
    }

    /// Wheel-delta multiplier for one step. The caller multiplies by
    /// `WHEEL_DELTA` to get the value for `wParam`'s high word.
    pub fn notch_multiplier(&self) -> i32 {
        self.notches as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_todays_behaviour() {
        let step = ScrollStep::new();
        assert_eq!(step.interval, DEFAULT_INTERVAL);
        assert_eq!(step.notches, DEFAULT_NOTCHES);
        assert_eq!(step.notch_multiplier(), 1);
    }

    #[test]
    fn interval_is_clamped_to_the_snow_shot_range() {
        let mut step = ScrollStep::new();
        step.set_interval(Duration::from_millis(10));
        assert_eq!(step.interval, MIN_INTERVAL);
        step.set_interval(Duration::from_secs(5));
        assert_eq!(step.interval, MAX_INTERVAL);
        step.set_interval(Duration::from_millis(200));
        assert_eq!(step.interval, Duration::from_millis(200));
    }

    #[test]
    fn notches_are_clamped() {
        let mut step = ScrollStep::new();
        step.set_notches(0);
        assert_eq!(step.notches, DEFAULT_NOTCHES);
        step.set_notches(100);
        assert_eq!(step.notches, MAX_NOTCHES);
        step.set_notches(3);
        assert_eq!(step.notch_multiplier(), 3);
    }
}
