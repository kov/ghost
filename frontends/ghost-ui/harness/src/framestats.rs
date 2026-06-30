//! Frame-pacing instrumentation — measures whether the *real window* holds its
//! frame budget during an animation, which the headless `ghost-shot bench` cannot
//! (it renders offscreen, with no swapchain present, no vsync block, and on
//! whatever Vulkan the test host has rather than the live GPU).
//!
//! Gated by the `GHOST_FRAME_STATS` env var (set [`FrameStats::from_env`]). When
//! on, the shell feeds each presented frame's timing — the model `view()`, the
//! build+submit, the (vsync-blocking) present, and the present-to-present interval
//! — to a [`DiveAccum`]; when an animation (a dive) ends, the accumulated summary
//! is printed to stderr. The accumulator is pure (millisecond samples, no clock)
//! so the drop accounting is unit-testable without a window.
//!
//! Reading the summary: `interval` is the true on-screen cadence (includes the
//! vsync wait), so a steady 60 Hz dive shows ~16.7 ms intervals and zero drops; a
//! frame whose work overran a refresh shows up as a doubled (~33 ms) interval and
//! a counted drop. `cpu` (model + build+submit, *excluding* the vsync block) is
//! the work the main thread must finish within a refresh to not drop — over-budget
//! `cpu` frames are guaranteed drops regardless of the GPU.

use std::time::{Duration, Instant};

/// 60 Hz refresh budget. An interval much over this is a missed vsync (a drop);
/// `cpu` work over this can't sustain 60 fps on any GPU.
pub const BUDGET_MS: f32 = 1000.0 / 60.0;

/// A frame can't keep 60 fps if its present-to-present interval exceeds the budget
/// by this factor — i.e. it slipped a whole refresh. (1.5× so a frame that merely
/// grazes the budget isn't miscounted as a drop.)
const DROP_FACTOR: f32 = 1.5;

/// An interval longer than this (ms) is treated as idle between two animations, not
/// an in-dive frame — it splits the run so each dive is summarised separately. Well
/// above any plausible dropped-frame interval, below the harness's inter-dive gap.
const IDLE_GAP_MS: f32 = 150.0;

/// One presented frame's timings, in milliseconds.
#[derive(Clone, Copy, Debug)]
pub struct FrameSample {
    /// `RootModel::view()` — builds the `Scene` (clones the frozen world during a
    /// dive), the per-frame CPU cost ahead of the GPU.
    pub model_ms: f32,
    /// Damage check + scene build + instance upload + render-pass submit, up to but
    /// not including the present.
    pub build_ms: f32,
    /// The `present()` call — on Fifo this blocks for vsync, so it is mostly *wait*,
    /// not work; informational, not counted against the budget.
    pub present_ms: f32,
    /// Wall-clock since the previous presented frame: the real on-screen cadence.
    /// `None` for the first frame of a run (no prior present to measure against).
    pub interval_ms: Option<f32>,
}

impl FrameSample {
    /// The main-thread work that must fit inside a refresh to avoid dropping —
    /// everything except the vsync-blocking present.
    fn cpu_ms(&self) -> f32 {
        self.model_ms + self.build_ms
    }
}

/// Accumulates one animation's frame samples into a [`DiveSummary`]. Pure: it is
/// fed millisecond samples, never a clock, so the drop accounting is testable.
#[derive(Clone, Debug, Default)]
pub struct DiveAccum {
    samples: Vec<FrameSample>,
}

impl DiveAccum {
    pub fn push(&mut self, s: FrameSample) {
        self.samples.push(s);
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Each sample's interval in order (`None` = excluded), for the verbose dump.
    pub fn ordered_intervals(&self) -> Vec<Option<f32>> {
        self.samples.iter().map(|s| s.interval_ms).collect()
    }

    /// Summarise the run against the 60 Hz budget. Returns `None` if no frames were
    /// recorded.
    pub fn summary(&self) -> Option<DiveSummary> {
        let n = self.samples.len();
        if n == 0 {
            return None;
        }
        let nf = n as f32;
        let mean = |f: &dyn Fn(&FrameSample) -> f32| self.samples.iter().map(f).sum::<f32>() / nf;
        let max =
            |f: &dyn Fn(&FrameSample) -> f32| self.samples.iter().map(f).fold(0.0_f32, f32::max);
        // Intervals (skip frames with no recorded interval — the first of each run,
        // whose gap is idle time before the animation, not a frame cadence).
        let mut intervals: Vec<f32> = self.samples.iter().filter_map(|s| s.interval_ms).collect();
        let dropped = intervals
            .iter()
            .filter(|ms| **ms > BUDGET_MS * DROP_FACTOR)
            .count();
        let worst_interval = intervals.iter().copied().fold(0.0_f32, f32::max);
        let avg_interval = if intervals.is_empty() {
            0.0
        } else {
            intervals.iter().sum::<f32>() / intervals.len() as f32
        };
        intervals.sort_by(f32::total_cmp);
        DiveSummary {
            frames: n,
            avg_model_ms: mean(&|s| s.model_ms),
            avg_build_ms: mean(&|s| s.build_ms),
            avg_present_ms: mean(&|s| s.present_ms),
            avg_cpu_ms: mean(&|s| s.cpu_ms()),
            worst_cpu_ms: max(&|s| s.cpu_ms()),
            over_budget_cpu: self
                .samples
                .iter()
                .filter(|s| s.cpu_ms() > BUDGET_MS)
                .count(),
            avg_interval_ms: avg_interval,
            p50_interval_ms: percentile(&intervals, 0.50),
            p90_interval_ms: percentile(&intervals, 0.90),
            worst_interval_ms: worst_interval,
            dropped,
        }
        .into()
    }
}

/// The `q`-quantile (0..=1) of an already-sorted slice, by nearest-rank; 0 for an
/// empty slice. The median (`q=0.5`) is the robust typical cadence — unlike the
/// mean it isn't dragged up by a couple of stall outliers.
fn percentile(sorted: &[f32], q: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((q * sorted.len() as f32).ceil() as usize).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)]
}

/// Verdict for one animation run.
#[derive(Clone, Copy, Debug)]
pub struct DiveSummary {
    pub frames: usize,
    pub avg_model_ms: f32,
    pub avg_build_ms: f32,
    pub avg_present_ms: f32,
    pub avg_cpu_ms: f32,
    pub worst_cpu_ms: f32,
    /// Frames whose CPU work (model + build) alone exceeded the 60 Hz budget — drops
    /// no GPU can save.
    pub over_budget_cpu: usize,
    pub avg_interval_ms: f32,
    /// Median interval — the robust typical cadence (a couple of stalls don't drag
    /// it up the way they do the mean).
    pub p50_interval_ms: f32,
    pub p90_interval_ms: f32,
    pub worst_interval_ms: f32,
    /// Frames whose on-screen interval slipped a whole refresh (a real drop).
    pub dropped: usize,
}

impl DiveSummary {
    /// A one-line stderr report.
    pub fn report(&self) -> String {
        let fps = if self.avg_interval_ms > 0.0 {
            1000.0 / self.avg_interval_ms
        } else {
            0.0
        };
        let p50_fps = if self.p50_interval_ms > 0.0 {
            1000.0 / self.p50_interval_ms
        } else {
            0.0
        };
        format!(
            "ghost frame-stats: dive {} frames | interval p50 {:.1} ({:.0} fps) p90 {:.1} \
             avg {:.1} worst {:.1} ms, {} dropped | cpu avg {:.2} worst {:.2} ms \
             ({} over {:.1} ms budget) | model {:.2}, build {:.2}, present {:.2} ms avg | {:.0} fps avg",
            self.frames,
            self.p50_interval_ms,
            p50_fps,
            self.p90_interval_ms,
            self.avg_interval_ms,
            self.worst_interval_ms,
            self.dropped,
            self.avg_cpu_ms,
            self.worst_cpu_ms,
            self.over_budget_cpu,
            BUDGET_MS,
            self.avg_model_ms,
            self.avg_build_ms,
            self.avg_present_ms,
            fps,
        )
    }
}

/// Debug: every interval (ms) of a run, in order, for `GHOST_FRAME_STATS=2`.
pub fn intervals_debug(samples_dump: &[Option<f32>]) -> String {
    let v: Vec<String> = samples_dump
        .iter()
        .map(|o| match o {
            Some(ms) => format!("{ms:.0}"),
            None => "·".to_string(),
        })
        .collect();
    format!("  intervals: [{}]", v.join(" "))
}

/// Shell-side collector: holds the live clock state and the current run's
/// accumulator. Cheap and inert unless `GHOST_FRAME_STATS` is set.
#[derive(Default)]
pub struct FrameStats {
    enabled: bool,
    /// `GHOST_FRAME_STATS=2`: also dump every per-frame interval, to see the stall
    /// pattern (is the dive a uniform low fps, or 60 fps with isolated hitches?).
    verbose: bool,
    last_present: Option<Instant>,
    accum: DiveAccum,
    was_animating: bool,
}

impl FrameStats {
    pub fn from_env() -> Self {
        let var = std::env::var("GHOST_FRAME_STATS").ok();
        Self {
            enabled: var.is_some(),
            verbose: var.as_deref() == Some("2"),
            ..Self::default()
        }
    }

    /// Record a just-presented frame. `animating` is the model's state for this
    /// frame; `model`/`build`/`present` are its measured phases and `now` the
    /// post-present instant. Returns a [`DiveSummary`] on the frame an animation
    /// ends (so the caller can print it); `None` otherwise. A no-op when disabled.
    pub fn record(
        &mut self,
        animating: bool,
        model: Duration,
        build: Duration,
        present: Duration,
        now: Instant,
    ) -> Option<DiveSummary> {
        if !self.enabled {
            return None;
        }
        let interval_ms = self.last_present.map(|t| (now - t).as_secs_f32() * 1000.0);
        self.last_present = Some(now);

        if animating {
            // A big interval mid-run is an idle gap between two animations the model
            // ran back-to-back without a presented settle frame to flush the first.
            // Close the prior run here so each dive is summarised on its own (else its
            // gap would masquerade as one giant in-dive stall).
            let split = self.was_animating && interval_ms.is_some_and(|ms| ms > IDLE_GAP_MS);
            let flushed = if split { self.flush_run() } else { None };
            // The first frame of a run follows idle time, so its interval is not a
            // frame cadence — exclude it from the stats.
            let interval_ms = if self.was_animating {
                interval_ms
            } else {
                None
            };
            self.accum.push(FrameSample {
                model_ms: model.as_secs_f32() * 1000.0,
                build_ms: build.as_secs_f32() * 1000.0,
                present_ms: present.as_secs_f32() * 1000.0,
                interval_ms,
            });
            self.was_animating = true;
            return flushed;
        }
        // Animation just ended: flush the run's summary.
        if self.was_animating {
            return self.flush_run();
        }
        None
    }

    /// Emit the accumulated run's summary (and, when verbose, its raw intervals),
    /// then reset for the next run.
    fn flush_run(&mut self) -> Option<DiveSummary> {
        self.was_animating = false;
        if self.verbose {
            eprintln!("{}", intervals_debug(&self.accum.ordered_intervals()));
        }
        let summary = self.accum.summary();
        self.accum.clear();
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(cpu: f32, interval: Option<f32>) -> FrameSample {
        FrameSample {
            model_ms: cpu / 2.0,
            build_ms: cpu / 2.0,
            present_ms: 0.0,
            interval_ms: interval,
        }
    }

    #[test]
    fn empty_accum_has_no_summary() {
        assert!(DiveAccum::default().summary().is_none());
    }

    #[test]
    fn a_steady_60hz_dive_drops_nothing() {
        let mut a = DiveAccum::default();
        a.push(sample(8.0, None)); // first frame: no interval
        for _ in 0..10 {
            a.push(sample(8.0, Some(16.6)));
        }
        let s = a.summary().unwrap();
        assert_eq!(s.frames, 11);
        assert_eq!(s.dropped, 0, "16.6 ms intervals are on-budget");
        assert_eq!(s.over_budget_cpu, 0, "8 ms cpu is well under budget");
    }

    #[test]
    fn a_slipped_refresh_counts_as_a_drop() {
        let mut a = DiveAccum::default();
        a.push(sample(8.0, None));
        a.push(sample(8.0, Some(16.6)));
        a.push(sample(20.0, Some(33.3))); // a doubled interval = one dropped frame
        a.push(sample(8.0, Some(16.6)));
        let s = a.summary().unwrap();
        assert_eq!(s.dropped, 1, "the ~33 ms interval slipped a vsync");
        assert_eq!(
            s.over_budget_cpu, 1,
            "the 20 ms cpu frame can't hold 60 fps"
        );
        assert!((s.worst_interval_ms - 33.3).abs() < 0.01);
    }

    #[test]
    fn record_flushes_only_when_animation_ends() {
        let mut st = FrameStats {
            enabled: true,
            ..FrameStats::default()
        };
        let t0 = Instant::now();
        let d = Duration::from_millis;
        // Three animating frames: no summary yet.
        assert!(st.record(true, d(4), d(4), d(8), t0).is_none());
        assert!(st.record(true, d(4), d(4), d(8), t0 + d(16)).is_none());
        // The settle frame (not animating) flushes the run.
        let summary = st.record(false, d(4), d(4), d(8), t0 + d(32));
        assert!(summary.is_some());
        assert_eq!(summary.unwrap().frames, 2);
        // A further idle frame produces nothing.
        assert!(st.record(false, d(4), d(4), d(8), t0 + d(48)).is_none());
    }

    #[test]
    fn disabled_stats_never_record() {
        let mut st = FrameStats::default(); // enabled = false
        let t0 = Instant::now();
        let d = Duration::from_millis;
        assert!(st.record(true, d(4), d(4), d(8), t0).is_none());
        assert!(st.record(false, d(4), d(4), d(8), t0 + d(16)).is_none());
    }
}
