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

// The glide's shape, fitted to macOS's own coasting (`fixtures/scroll/macos.txt`,
// replayed by the tests below). Speeds are logical px — macOS's points, which
// is what the fingers' scroll travel is on both platforms — per 60 Hz frame,
// the unit macOS's curve is regular in.

/// A frame at 60 Hz, ms.
const FRAME_MS: f64 = 1000.0 / 60.0;

/// macOS coasts faster than the fingers left the pad, more so the harder the
/// flick: `BOOST_BASE + BOOST_PER_LN * ln(lift speed)` times the lift speed
/// (px/frame), within [`BOOST_MIN`]..=[`BOOST_MAX`] — about 1.1x for a gentle
/// flick, 2x for a hard one.
const BOOST_BASE: f64 = 0.61;
const BOOST_PER_LN: f64 = 0.30;
const BOOST_MIN: f64 = 1.0;
const BOOST_MAX: f64 = 2.5;

/// It holds that speed this long (ms)…
const PLATEAU_MS: f64 = 130.0;

/// …then slows exponentially with this time constant (ms)…
const TAU_MS: f64 = 205.0;

/// …and stops below this speed (px/frame).
const STOP_BELOW: f64 = 0.76;

/// Slower lifts than this (px/frame) don't coast at all: macOS's line lies
/// between 2.66 (still) and 2.99 (coasts) — below it the fingers were
/// settling, not flicking.
const MIN_FLING: f64 = 2.8;

/// A lift this long (ms) after the last motion had the fingers at rest first:
/// no flick. libinput's axis stop trails a real flick's last motion by up to
/// ~45 ms; fingers held still show 80 ms and more.
const STOPPED_MS: f64 = 65.0;

/// The fingers' speed at the lift is the average of their last this-many
/// frames, like macOS's.
const LIFT_FRAMES: usize = 3;

/// The glide: the fling's speed boosted, held, then slowing to a stop — the
/// coasting macOS does after the same flick.
#[derive(Clone, Debug)]
pub struct Glide {
    /// Coasting speed, px/ms, signed like [`crate::WheelDelta`] (up = +).
    speed: f64,
    /// When it comes to rest, ms after the lift.
    end_ms: f64,
    start_ms: u64,
    /// Travel already handed out, px.
    travelled: f64,
    done: bool,
}

impl Glide {
    /// The glide after a lift at `px_per_s` (device px, at `scale` device px
    /// per logical px), or `None` for a lift too slow to coast on.
    pub fn new(px_per_s: f64, scale: f64, now_ms: u64) -> Option<Glide> {
        let per_frame = px_per_s.abs() / scale * FRAME_MS / 1000.0;
        if !per_frame.is_finite() || per_frame < MIN_FLING {
            return None;
        }
        let boost = (BOOST_BASE + BOOST_PER_LN * per_frame.ln()).clamp(BOOST_MIN, BOOST_MAX);
        let speed = px_per_s / 1000.0 * boost;
        let stop = STOP_BELOW / FRAME_MS * scale;
        Some(Glide {
            speed,
            end_ms: PLATEAU_MS + TAU_MS * (speed.abs() / stop).ln(),
            start_ms: now_ms,
            travelled: 0.0,
            done: false,
        })
    }

    /// Distance covered `t` ms after the lift.
    fn position(&self, t: f64) -> f64 {
        let t = t.min(self.end_ms);
        if t <= PLATEAU_MS {
            self.speed * t
        } else {
            self.speed * (PLATEAU_MS + TAU_MS * (1.0 - (-(t - PLATEAU_MS) / TAU_MS).exp()))
        }
    }

    /// The travel (px) since the previous step, or `None` once the glide has
    /// run out. The step that comes to rest still hands out its last travel.
    pub fn step(&mut self, now_ms: u64) -> Option<f64> {
        if self.done {
            return None;
        }
        let t = now_ms.saturating_sub(self.start_ms) as f64;
        let pos = self.position(t);
        let d = pos - self.travelled;
        self.travelled = pos;
        self.done = t >= self.end_ms;
        Some(d)
    }
}

/// Measures how fast the fingers were moving when they lifted, from the
/// scroll deltas that led up to it.
#[derive(Clone, Debug, Default)]
pub struct VelocityTracker {
    /// `(time ms, delta px)` of the gesture's recent motion, oldest first.
    samples: VecDeque<(f64, f64)>,
}

impl VelocityTracker {
    /// Record a finger delta (px, signed like [`crate::WheelDelta`]) at `t_ms`.
    pub fn push(&mut self, t_ms: f64, dy: f64) {
        self.samples.push_back((t_ms, dy));
        // Enough to find the device's cadence as well as the last frames.
        while self.samples.len() > 2 * LIFT_FRAMES + 1 {
            self.samples.pop_front();
        }
    }

    /// The fingers lifted at `t_ms`: their speed then, in px/s, and a fresh
    /// start for the next gesture. Zero when they had come to rest first.
    ///
    /// Speed is travel per event interval, the interval being the device's
    /// cadence (the median gap between events) rather than the gaps' own
    /// timings: the events arrive at a steady rate but get stamped when read,
    /// which jitters — and a flick is often only one event long, with no gap
    /// to time at all, when it is one frame at the usual 60 Hz.
    pub fn lift(&mut self, t_ms: f64) -> f64 {
        let samples: Vec<(f64, f64)> = std::mem::take(&mut self.samples).into();
        let Some(&(last, _)) = samples.last() else {
            return 0.0;
        };
        if t_ms - last > STOPPED_MS {
            return 0.0;
        }
        let mut gaps: Vec<f64> = samples
            .windows(2)
            .map(|w| w[1].0 - w[0].0)
            .filter(|&g| g > 2.0)
            .collect();
        gaps.sort_by(f64::total_cmp);
        let cadence = gaps
            .get(gaps.len() / 2)
            .map_or(FRAME_MS, |g| g.clamp(8.0, 34.0));
        let recent = &samples[samples.len().saturating_sub(LIFT_FRAMES)..];
        let per_event = recent.iter().map(|&(_, dy)| dy).sum::<f64>() / recent.len() as f64;
        per_event / cadence * 1000.0
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

    fn run(g: &mut Glide, from: u64) -> Vec<f64> {
        let mut steps = Vec::new();
        let mut t = from;
        while let Some(d) = g.step(t) {
            steps.push(d);
            t += 16;
        }
        steps
    }

    #[test]
    fn a_glide_holds_then_slows_to_a_stop() {
        let steps = run(&mut Glide::new(-900.0, 1.0, 0).unwrap(), 16);
        assert!(steps.iter().all(|&d| d <= 0.0), "keeps the flick's sign");
        assert!(
            (steps[0] - steps[5]).abs() < 1e-9,
            "held at first: {steps:?}"
        );
        assert!(
            steps[10..].windows(2).all(|w| w[1].abs() <= w[0].abs()),
            "{steps:?}"
        );
    }

    #[test]
    fn a_glide_is_the_same_on_a_hidpi_screen_in_logical_px() {
        let one: f64 = run(&mut Glide::new(600.0, 1.0, 0).unwrap(), 16)
            .iter()
            .sum();
        let two: f64 = run(&mut Glide::new(1200.0, 2.0, 0).unwrap(), 16)
            .iter()
            .sum();
        assert!((two - 2.0 * one).abs() < 1e-6, "{one} x2 vs {two}");
    }

    #[test]
    fn a_slow_or_bogus_lift_does_not_glide() {
        assert!(Glide::new(0.0, 1.0, 0).is_none());
        assert!(Glide::new(-100.0, 1.0, 0).is_none());
        assert!(Glide::new(f64::NAN, 1.0, 0).is_none());
    }

    #[test]
    fn a_one_event_flick_reads_as_one_frame_of_travel() {
        let mut v = VelocityTracker::default();
        v.push(0.0, 12.0);
        let speed = v.lift(14.0);
        assert!((speed - 12.0 * 60.0).abs() < 1e-6, "{speed}");
        assert_eq!(v.lift(15.0), 0.0, "a lift starts the next gesture fresh");
    }

    #[test]
    fn the_speed_is_the_last_frames_at_the_devices_cadence() {
        let mut v = VelocityTracker::default();
        // ~120 Hz, with read-time jitter (gaps 8, 9, 7, 9: the cadence is
        // their median, 9); a slow start, then 10px an event.
        for (t, dy) in [
            (0.0, 1.0),
            (8.0, 2.0),
            (17.0, 10.0),
            (24.0, 10.0),
            (33.0, 10.0),
        ] {
            v.push(t, dy);
        }
        let speed = v.lift(40.0);
        assert!((speed - 10.0 / 9.0 * 1000.0).abs() < 1e-6, "{speed}");
    }

    #[test]
    fn fingers_at_rest_before_the_lift_measure_no_flick() {
        let mut v = VelocityTracker::default();
        for i in 0..10 {
            v.push(16.7 * i as f64, -20.0);
        }
        assert_eq!(v.lift(150.0 + STOPPED_MS + 10.0), 0.0);
    }

    /// Replays of real trackpad captures (`fixtures/scroll`, recorded with
    /// `tools/scroll-probe`): macOS's own coasting is the reference the glide
    /// must reproduce, and the Linux VM's stream is what it gets to work with.
    mod replay {
        use super::super::{Glide, VelocityTracker};

        const FRAME_MS: f64 = 1000.0 / 60.0;

        struct Gesture {
            label: Option<String>,
            finger: Vec<(f64, f64)>,
            lift: f64,
            momentum: Vec<(f64, f64)>,
        }

        fn parse(text: &str) -> Vec<Gesture> {
            let mut out: Vec<Gesture> = Vec::new();
            for line in text.lines() {
                let w: Vec<&str> = line.split_whitespace().collect();
                let num = |i: usize| w[i].parse::<f64>().unwrap();
                match w.first().copied() {
                    Some("gesture") => out.push(Gesture {
                        label: w.get(1).map(|s| s.to_string()),
                        finger: Vec::new(),
                        lift: 0.0,
                        momentum: Vec::new(),
                    }),
                    Some("f") => out.last_mut().unwrap().finger.push((num(1), num(2))),
                    Some("lift") => out.last_mut().unwrap().lift = num(1),
                    Some("m") => out.last_mut().unwrap().momentum.push((num(1), num(2))),
                    _ => {}
                }
            }
            out
        }

        /// Replay a gesture's finger travel and lift through the tracker, then
        /// play the glide at 60 fps: `(distance px, duration ms)`, or `None`
        /// when it doesn't coast. Times are offset so the lift lands on a
        /// whole-ms tick like the shell's clock.
        fn glide_after(g: &Gesture) -> Option<(f64, f64)> {
            let mut tracker = VelocityTracker::default();
            for &(t, dy) in &g.finger {
                tracker.push(t, dy);
            }
            let lift_ms = g.lift.round() as u64;
            let mut glide = Glide::new(tracker.lift(g.lift), 1.0, lift_ms)?;
            let (mut total, mut frames, mut t) = (0.0, 0.0, lift_ms as f64);
            loop {
                t += FRAME_MS;
                match glide.step(t.round() as u64) {
                    Some(d) => {
                        total += d;
                        if d != 0.0 {
                            frames += 1.0;
                        }
                    }
                    None => break,
                }
            }
            Some((total, frames * FRAME_MS))
        }

        fn macos() -> Vec<Gesture> {
            parse(include_str!("../fixtures/scroll/macos.txt"))
        }

        #[test]
        fn a_flick_coasts_exactly_when_macos_coasted() {
            for (i, g) in macos().iter().enumerate() {
                assert_eq!(
                    glide_after(g).is_some(),
                    !g.momentum.is_empty(),
                    "macOS gesture #{i}: coasting on the same flick"
                );
            }
        }

        #[test]
        fn a_glide_goes_as_far_and_as_long_as_macos_on_the_same_flick() {
            let mut close = 0;
            let coasting: Vec<Gesture> = macos()
                .into_iter()
                .filter(|g| !g.momentum.is_empty())
                .collect();
            for (i, g) in coasting.iter().enumerate() {
                let want: f64 = g.momentum.iter().map(|&(_, dy)| dy).sum();
                let want_ms = g.momentum.last().unwrap().0 - g.momentum[0].0 + FRAME_MS;
                let (got, got_ms) = glide_after(g).expect("it coasts");
                let ratio = got / want;
                assert!(
                    (0.5..=1.35).contains(&ratio),
                    "#{i}: glided {got:.0}px where macOS coasted {want:.0}px"
                );
                assert!(
                    (0.85..=1.15).contains(&(got_ms / want_ms)),
                    "#{i}: glided {got_ms:.0}ms where macOS coasted {want_ms:.0}ms"
                );
                if (0.85..=1.15).contains(&ratio) {
                    close += 1;
                }
            }
            // The outliers: flicks that sped up into the lift (macOS reads
            // the fingers' last instant; the tracker a frame's average) and
            // the very fastest, where macOS's boost levels off.
            assert!(
                close * 10 >= coasting.len() * 8,
                "{close}/{} within 15% of macOS",
                coasting.len()
            );
        }

        #[test]
        fn every_flick_in_the_vm_capture_coasts_and_nothing_else_does() {
            // What the Linux VM delivers: 60 Hz, flicks often only one to
            // three events long, the axis stop up to ~45 ms after the last.
            let gestures = parse(include_str!("../fixtures/scroll/linux-vm.txt"));
            for (i, g) in gestures.iter().enumerate() {
                let label = g.label.as_deref().unwrap();
                let glide = glide_after(g);
                if label == "flick" {
                    let (px, _) = glide.unwrap_or_else(|| panic!("#{i}: a flick must coast"));
                    let last = g.finger.last().unwrap().1;
                    assert!(
                        px.signum() == last.signum() && px.abs() >= 10.0 * last.abs(),
                        "#{i}: a flick coasts on its way, well past its last frame: \
                         {px:.0}px after a {last}px frame"
                    );
                } else {
                    assert!(glide.is_none(), "#{i}: a {label} must not coast: {glide:?}");
                }
            }
        }
    }
}
