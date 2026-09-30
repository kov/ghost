//! Kinetic scrolling for platforms whose OS stops dead when the fingers lift.
//!
//! macOS coasts on after a trackpad flick by itself, sending momentum-phase
//! scroll events (winit's `momentum` flag, our [`WheelDelta::Momentum`]).
//! libinput does not: a two-finger scroll ends at the lift with an axis stop,
//! and every toolkit on Linux synthesizes its own glide. Ghost does the same in
//! two halves: the shell measures the flick with a [`VelocityTracker`] and
//! reports the lift as [`UiEvent::Fling`](crate::UiEvent::Fling); the window
//! then plays a [`Glide`] on its tick clock, feeding the travel to whichever
//! view is showing as ordinary `Momentum` wheels.
//!
//! The glide emits what macOS would have sent — its deceleration curve — so
//! a flick coasts the same on both platforms.
//!
//! [`WheelDelta::Momentum`]: crate::WheelDelta::Momentum

use std::collections::VecDeque;

/// Velocity kept per millisecond of glide: Apple's normal scroll deceleration
/// rate (`UIScrollView.DecelerationRate.normal`), the curve macOS momentum
/// follows. The glide covers `v0 * TAU_MS` in all, most of it in the first
/// half second.
const DECAY_PER_MS: f64 = 0.998;

/// Slower flicks than this (px/s) don't glide: a lift after the fingers
/// settled measures a little residual motion, and drifting on from it reads as
/// the view slipping.
const MIN_FLING: f64 = 150.0;

/// Cap on a measured flick (px/s), against an outlier from events that arrived
/// bunched together (a busy event loop, or a VM delivering input in bursts).
const MAX_FLING: f64 = 8000.0;

/// The glide ends once it has slowed to this (px/s): the remaining travel is
/// under a pixel.
const STOP_BELOW: f64 = 20.0;

/// How far back (ms) from the lift the tracker looks to measure the flick: the
/// fingers' speed as they left the pad, not the whole gesture's average.
const WINDOW_MS: f64 = 100.0;

/// A lift this long (ms) after the last motion had the fingers at rest first:
/// no flick.
const PAUSED_MS: f64 = 50.0;

/// Shortest span (ms) a measurement divides by, so events that arrived
/// bunched together don't read as an instant — and hence enormous — flick.
const MIN_SPAN_MS: f64 = 8.0;

fn tau_ms() -> f64 {
    -1.0 / DECAY_PER_MS.ln()
}

/// The coast after a flick: an exponential slowdown from the lift velocity.
#[derive(Clone, Debug)]
pub struct Glide {
    /// Lift velocity, px/ms, signed like [`crate::WheelDelta`] (up = +).
    v0: f64,
    start_ms: u64,
    /// Travel already handed out, px.
    travelled: f64,
    done: bool,
}

impl Glide {
    /// A glide from a lift at `px_per_s`, or `None` for a flick too slow to
    /// coast on.
    pub fn new(px_per_s: f64, now_ms: u64) -> Option<Glide> {
        if !px_per_s.is_finite() || px_per_s.abs() < MIN_FLING {
            return None;
        }
        Some(Glide {
            v0: px_per_s.clamp(-MAX_FLING, MAX_FLING) / 1000.0,
            start_ms: now_ms,
            travelled: 0.0,
            done: false,
        })
    }

    /// The travel (px) since the previous step, or `None` once the glide has
    /// run out. The step that comes to rest still hands out its last travel.
    pub fn step(&mut self, now_ms: u64) -> Option<f64> {
        if self.done {
            return None;
        }
        let t = now_ms.saturating_sub(self.start_ms) as f64;
        let decay = DECAY_PER_MS.powf(t);
        let pos = self.v0 * tau_ms() * (1.0 - decay);
        let d = pos - self.travelled;
        self.travelled = pos;
        self.done = (self.v0 * decay).abs() * 1000.0 < STOP_BELOW;
        Some(d)
    }
}

/// Measures how fast the fingers were moving when they lifted, from the
/// scroll deltas that led up to it.
#[derive(Clone, Debug, Default)]
pub struct VelocityTracker {
    /// `(time ms, delta px)`, oldest first, trimmed to [`WINDOW_MS`].
    samples: VecDeque<(f64, f64)>,
}

impl VelocityTracker {
    /// Record a finger delta (px, signed like [`crate::WheelDelta`]) at `t_ms`.
    pub fn push(&mut self, t_ms: f64, dy: f64) {
        self.samples.push_back((t_ms, dy));
        while self
            .samples
            .front()
            .is_some_and(|&(t, _)| t < t_ms - WINDOW_MS)
        {
            self.samples.pop_front();
        }
    }

    /// The fingers lifted at `t_ms`: their velocity then, in px/s, and a fresh
    /// start for the next gesture. Zero when they had come to rest first, or
    /// when too little motion was seen to tell.
    pub fn lift(&mut self, t_ms: f64) -> f64 {
        let samples = std::mem::take(&mut self.samples);
        let recent: Vec<(f64, f64)> = samples
            .into_iter()
            .filter(|&(t, _)| t >= t_ms - WINDOW_MS)
            .collect();
        let (Some(&(first, _)), Some(&(last, _))) = (recent.first(), recent.last()) else {
            return 0.0;
        };
        if recent.len() < 2 || t_ms - last > PAUSED_MS {
            return 0.0;
        }
        // Each delta covers the interval since the one before it, so the
        // first sample only marks where the measured span starts.
        let travel: f64 = recent[1..].iter().map(|&(_, dy)| dy).sum();
        travel / (last - first).max(MIN_SPAN_MS) * 1000.0
    }

    /// Forget the gesture in progress (a wheel click or another device took
    /// over).
    pub fn reset(&mut self) {
        self.samples.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(g: &mut Glide, from: u64) -> (f64, u64) {
        let (mut total, mut t) = (0.0, from);
        while let Some(d) = g.step(t) {
            total += d;
            t += 16;
        }
        (total, t)
    }

    #[test]
    fn a_glide_covers_the_velocity_times_its_time_constant_and_stops() {
        let mut g = Glide::new(2000.0, 500).unwrap();
        let (total, end) = run(&mut g, 500);
        // Short of the full `v0 * tau` only by the sub-pixel tail cut off
        // below STOP_BELOW.
        let expected = 2.0 * tau_ms();
        let tail = STOP_BELOW / 1000.0 * tau_ms();
        assert!(
            expected - total <= tail && total < expected,
            "{total} vs {expected}"
        );
        assert!(
            (2_000..4_000).contains(&(end - 500)),
            "settles in a few seconds: {end}"
        );
    }

    #[test]
    fn a_glide_decelerates() {
        let mut g = Glide::new(-3000.0, 0).unwrap();
        let steps: Vec<f64> = (1..20).map(|i| g.step(i * 16).unwrap()).collect();
        assert!(steps.iter().all(|&d| d < 0.0), "keeps the flick's sign");
        assert!(
            steps.windows(2).all(|w| w[1].abs() < w[0].abs()),
            "{steps:?}"
        );
    }

    #[test]
    fn a_slow_or_bogus_lift_does_not_glide() {
        assert!(Glide::new(0.0, 0).is_none());
        assert!(Glide::new(-100.0, 0).is_none());
        assert!(Glide::new(f64::NAN, 0).is_none());
    }

    #[test]
    fn an_outlier_flick_is_capped() {
        let (capped, _) = run(&mut Glide::new(1e6, 0).unwrap(), 0);
        let (max, _) = run(&mut Glide::new(MAX_FLING, 0).unwrap(), 0);
        assert_eq!(capped, max);
    }

    #[test]
    fn the_tracker_measures_the_fingers_speed_at_the_lift() {
        let mut v = VelocityTracker::default();
        // A slow start, then 20px every 8ms up to the lift: 2500 px/s.
        v.push(0.0, 2.0);
        v.push(50.0, 2.0);
        for i in 0..10 {
            v.push(100.0 + 8.0 * i as f64, 20.0);
        }
        let speed = v.lift(175.0);
        assert!((speed - 2500.0).abs() < 1.0, "{speed}");
        assert_eq!(v.lift(180.0), 0.0, "a lift starts the next gesture fresh");
    }

    #[test]
    fn fingers_at_rest_before_the_lift_measure_no_flick() {
        let mut v = VelocityTracker::default();
        for i in 0..10 {
            v.push(8.0 * i as f64, -20.0);
        }
        assert_eq!(v.lift(72.0 + PAUSED_MS + 10.0), 0.0);
    }

    #[test]
    fn a_bunched_burst_is_not_an_instant_flick() {
        let mut v = VelocityTracker::default();
        v.push(10.0, 30.0);
        v.push(10.0, 30.0);
        v.push(10.0, 30.0);
        let speed = v.lift(12.0);
        assert!(
            (speed - 60.0 / MIN_SPAN_MS * 1000.0).abs() < 1e-6,
            "{speed}"
        );
    }

    #[test]
    fn a_reversal_nets_out() {
        let mut v = VelocityTracker::default();
        for i in 0..6 {
            v.push(8.0 * i as f64, if i < 3 { 20.0 } else { -20.0 });
        }
        // Each half alone reads 2500 px/s.
        assert!(v.lift(40.0).abs() < 1000.0);
    }
}
