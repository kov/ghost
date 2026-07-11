//! Foreground render-stall diagnosis.
//!
//! The recurring bug: an attached session (Claude Code) keeps producing output —
//! its fleet preview stays live — but the foreground single view stops presenting,
//! and a scroll (or any input) unsticks it. It has been "fixed" many times, each
//! closing one repaint-suppression path (a synchronized-output hold that never
//! releases, a dropped `request_redraw`, a feed dirty-hint that missed a row, a
//! scene-equality skip). We still can't SEE which gate is stuck when it recurs.
//! (A recurrence of the scene-equality class is the one gate this watchdog cannot
//! flag: it reports `Clean`, which re-baselines by design — see
//! [`StallClass::StaleNoPresent`].)
//!
//! [`RenderTrace`] is a per-window watchdog + kick oracle. The shell timestamps the
//! foreground repaint pipeline (redraw commands, release ticks, present outcomes,
//! input) and, once per event-loop pass, folds in the core's [`TermTrace`] counters
//! and asks [`RenderTrace::verdict`] whether the foreground is stalled. The insight:
//! when stalled, the core keeps feeding (`feeds_seen` advances) while `last_present`
//! freezes — and the classified verdict plus the raw field dump say WHICH gate is
//! stuck. When a present finally lands after a stall (the user's recovering scroll),
//! [`RenderTrace::saw_outcome`] reports it: the diff between what was stuck and what
//! unstuck it is the diagnosis, self-reported at the moment of recovery.
//!
//! Everything here is pure (an external millisecond clock), so the classifier is
//! unit-tested without a window or GPU — the same shape as [`crate::pacer`]. The
//! shell only runs it under `RUST_LOG=ghost::render=trace`, so a normal run pays
//! nothing.

use ghost_ui_core::TermTrace;

/// A synchronized-output hold latched longer than this is stuck, not batching: the
/// core's own backstop releases a legitimate frame in 150 ms.
const STALL_HOLD_MS: u64 = 1_000;
/// How long the foreground may go without presenting (while the core keeps feeding)
/// before we call it stalled. Well above the 16 ms frame budget and the 150 ms hold
/// backstop, so a healthy view never trips it.
const STALL_QUIET_MS: u64 = 2_000;
/// How many feeds may arrive producing NO visible change before we suspect the
/// dirty-row hint is dropping updates (query replies alone never reach this from a
/// streaming app).
const FEEDS_NOT_VISIBLE_MIN: u64 = 20;
/// A continuing stall re-emits at most this often, so a persistent freeze leaves a
/// periodic breadcrumb without flooding the log.
const EMIT_EVERY_MS: u64 = 5_000;

/// The present pipeline's verdict for a foreground frame, mirrored from
/// `ghost_renderer::FrameOutcome` so this module stays renderer-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A frame was drawn and presented.
    Presented,
    /// The scene was identical to the last presented one; nothing drawn.
    Clean,
    /// The surface wasn't acquirable; nothing landed.
    Lost,
}

/// Which repaint gate the classifier believes is stuck.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StallClass {
    /// A DEC-2026 synchronized-output hold has been open too long — its release
    /// tick was dropped (the recurring latch). The report's `sync_holds` /
    /// `released_by_tick` counts and `pending_tick` sub-classify it.
    HeldTooLong,
    /// Feeds keep arriving but none change anything visible — the core's per-feed
    /// dirty-row hint is dropping updates, so no repaint is even requested.
    FeedsNotVisible,
    /// The core produced visible changes and the surface DID acquire, but the window
    /// still hasn't presented in a while — a redraw request the platform dropped or a
    /// stuck pacer. This is the recurring foreground bug. The report's
    /// `pacer_pending`, `last_release`, `last_redraw_event` and `last_outcome`
    /// discriminate among them. Note the one blind spot: a repaint that DID run but
    /// skipped on scene equality reports `Clean`, which re-baselines and disarms —
    /// the compare is exact (`SceneCache::damage` is `PartialEq`, never a hash), so
    /// a Clean loop hiding a stale *scene build* is invisible here by design (the
    /// alternative false-alarms on every redundant-content feed, e.g. a spinner).
    StaleNoPresent,
    /// The core produced visible changes but every present attempt comes back `Lost`
    /// — the surface isn't acquirable, so the platform (not our repaint pipeline) is
    /// withholding the drawable: the window is occluded / minimized / on another
    /// Space, or the display is asleep. Split from [`StallClass::StaleNoPresent`] so a
    /// `stale-no-present` line means acquire SUCCEEDED yet nothing painted — the bug
    /// worth chasing — and this benign, self-correcting condition doesn't drown it.
    SurfaceLost,
}

impl std::fmt::Display for StallClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            StallClass::HeldTooLong => "held-too-long",
            StallClass::FeedsNotVisible => "feeds-not-visible",
            StallClass::StaleNoPresent => "stale-no-present",
            StallClass::SurfaceLost => "surface-lost",
        };
        f.write_str(s)
    }
}

/// A flattened snapshot of the trace for one log line: the classifier's inputs as
/// ages (ms-ago) plus the load-bearing core counts, so a human reading the log can
/// sub-classify a stall the coarse [`StallClass`] only points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderReport {
    pub class: StallClass,
    pub stalled_for_ms: u64,
    pub held_for_ms: Option<u64>,
    pub since_feed_ms: Option<u64>,
    pub since_visible_feed_ms: Option<u64>,
    pub since_redraw_cmd_ms: Option<u64>,
    pub since_release_ms: Option<u64>,
    pub since_redraw_event_ms: Option<u64>,
    pub since_present_ms: Option<u64>,
    pub since_input_ms: Option<u64>,
    pub last_outcome: Option<Outcome>,
    pub pacer_pending: bool,
    pub feeds_seen: u64,
    pub visible_feeds: u64,
    pub feeds_while_held: u64,
    pub sync_holds: u64,
    pub released_by_tick: u64,
    pub released_by_reset: u64,
    pub pending_tick: bool,
    pub presents: u64,
}

impl std::fmt::Display for RenderReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ago = |o: Option<u64>| o.map_or(-1i64, |v| v as i64);
        write!(
            f,
            "class={} stalled_for={}ms held_for={} feed_ago={} visible_ago={} \
             redraw_cmd_ago={} release_ago={} redraw_event_ago={} present_ago={} \
             input_ago={} last_outcome={:?} pacer_pending={} pending_tick={} \
             feeds_seen={} visible={} while_held={} holds={} rel_tick={} rel_reset={} \
             presents={}",
            self.class,
            self.stalled_for_ms,
            ago(self.held_for_ms),
            ago(self.since_feed_ms),
            ago(self.since_visible_feed_ms),
            ago(self.since_redraw_cmd_ms),
            ago(self.since_release_ms),
            ago(self.since_redraw_event_ms),
            ago(self.since_present_ms),
            ago(self.since_input_ms),
            self.last_outcome,
            self.pacer_pending,
            self.pending_tick,
            self.feeds_seen,
            self.visible_feeds,
            self.feeds_while_held,
            self.sync_holds,
            self.released_by_tick,
            self.released_by_reset,
            self.presents,
        )
    }
}

/// Per-window render-stall watchdog. Cheap to poll every event-loop pass; the shell
/// only does so under `RUST_LOG=ghost::render=trace`.
#[derive(Clone, Copy, Debug, Default)]
pub struct RenderTrace {
    // Shell-stamped pipeline timestamps.
    last_redraw_cmd_ms: Option<u64>,
    last_release_ms: Option<u64>,
    last_redraw_event_ms: Option<u64>,
    last_present_ms: Option<u64>,
    last_tick_scheduled_ms: Option<u64>,
    last_tick_fired_ms: Option<u64>,
    last_input_ms: Option<u64>,
    last_outcome: Option<Outcome>,
    // Folded from the core's TermTrace deltas.
    last_feed_ms: Option<u64>,
    last_visible_feed_ms: Option<u64>,
    held_since_ms: Option<u64>,
    feeds_at_last_visible: u64,
    visible_at_last_present: u64,
    last_core: TermTrace,
    // Oracle + rate limiting.
    stalled: Option<(StallClass, u64)>,
    last_emit_ms: Option<u64>,
}

impl RenderTrace {
    pub fn new() -> Self {
        Self::default()
    }

    /// A `Cmd::Redraw` was processed for this window.
    pub fn saw_redraw_cmd(&mut self, now_ms: u64) {
        self.last_redraw_cmd_ms = Some(now_ms);
    }

    /// A release tick was scheduled (a synchronized-output backstop, or an
    /// animation tick).
    pub fn saw_tick_scheduled(&mut self, now_ms: u64) {
        self.last_tick_scheduled_ms = Some(now_ms);
    }

    /// A due tick fired into the model.
    pub fn saw_tick_fired(&mut self, now_ms: u64) {
        self.last_tick_fired_ms = Some(now_ms);
    }

    /// The pacer released a repaint (`request_redraw` was called).
    pub fn saw_release(&mut self, now_ms: u64) {
        self.last_release_ms = Some(now_ms);
    }

    /// A `RedrawRequested` reached the window (the platform delivered the release).
    pub fn saw_redraw_event(&mut self, now_ms: u64) {
        self.last_redraw_event_ms = Some(now_ms);
    }

    /// User input (key / pointer / wheel) — the "kick" label.
    pub fn saw_input(&mut self, now_ms: u64) {
        self.last_input_ms = Some(now_ms);
    }

    /// A present pipeline outcome landed. A `Presented` that ends an armed stall is
    /// the kick oracle: it returns the report of the stall it just recovered (for a
    /// warn-level "recovered" line), and clears the armed state.
    pub fn saw_outcome(
        &mut self,
        outcome: Outcome,
        now_ms: u64,
        core: Option<TermTrace>,
        pacer_pending: bool,
    ) -> Option<RenderReport> {
        self.last_outcome = Some(outcome);
        match outcome {
            // A frame was drawn: the surface now shows this scene. Advance the present
            // baseline, and a present that ends an armed stall is the kick oracle —
            // report the frozen state it just recovered (the user's scroll).
            Outcome::Presented => {
                self.last_present_ms = Some(now_ms);
                if let Some(c) = core {
                    self.visible_at_last_present = c.visible_feeds;
                }
                self.stalled.take().map(|(class, since)| {
                    self.build_report(
                        class,
                        now_ms,
                        now_ms.saturating_sub(since),
                        core,
                        pacer_pending,
                    )
                })
            }
            // Nothing to draw: the renderer compared the scene byte-for-byte and it
            // already matches what's on screen, so the display is provably up to date
            // as of now. Advance the baseline too (else an idle window that settles
            // into Clean frames looks permanently stale — the pending-visible gap is a
            // fossil), and silently disarm any false alarm: a Clean confirms the screen
            // is correct, it is not a recovery worth a "recovered" line.
            Outcome::Clean => {
                self.last_present_ms = Some(now_ms);
                if let Some(c) = core {
                    self.visible_at_last_present = c.visible_feeds;
                }
                self.stalled = None;
                None
            }
            // The surface wasn't acquirable: nothing landed and the baseline is
            // unchanged, so an ongoing stall stays armed (and classifies SurfaceLost).
            Outcome::Lost => None,
        }
    }

    /// Once per event-loop pass: fold the foreground's core counters, classify, and
    /// return a report to emit when a stall is newly detected or continues past the
    /// re-emit interval. `core` is `None` in the fleet overview (no single foreground)
    /// and `visible` is `false` when the window is occluded — in either case the trace
    /// resets and never fires, since a non-presenting hidden surface isn't our bug.
    pub fn poll(
        &mut self,
        now_ms: u64,
        core: Option<TermTrace>,
        pacer_pending: bool,
        has_snapshot: bool,
        visible: bool,
    ) -> Option<RenderReport> {
        // Nothing to watchdog when there is no single foreground (the fleet overview,
        // `core: None`) OR the window can't present at all (occluded / minimized / on
        // another Space — `visible: false`): the platform withholds the drawable there,
        // so a "stall" would be expected, not our bug. Re-baseline every derived
        // timestamp so returning to a visible single view starts clean — no stall fires
        // until a genuine new one develops (needs a fresh present baseline; the
        // opening-frame path handles a never-presenting window).
        let core = match core {
            Some(c) if visible => c,
            _ => {
                self.stalled = None;
                self.last_core = TermTrace::default();
                self.feeds_at_last_visible = 0;
                self.visible_at_last_present = 0;
                self.last_feed_ms = None;
                self.last_visible_feed_ms = None;
                self.last_present_ms = None;
                self.held_since_ms = None;
                return None;
            }
        };
        // Fold the core deltas into timestamps.
        if core.feeds_seen > self.last_core.feeds_seen {
            self.last_feed_ms = Some(now_ms);
        }
        if core.visible_feeds > self.last_core.visible_feeds {
            self.last_visible_feed_ms = Some(now_ms);
            self.feeds_at_last_visible = core.feeds_seen;
        }
        if core.sync_held && !self.last_core.sync_held {
            self.held_since_ms = Some(now_ms);
        }
        if !core.sync_held {
            self.held_since_ms = None;
        }
        self.last_core = core;

        let Some(class) = self.verdict(now_ms, core, pacer_pending, has_snapshot) else {
            // No longer classifiable as stalled from the gates — but do NOT clear an
            // armed stall here: the stale frame is still on screen until a present
            // (the recovery). `saw_outcome` clears it.
            return None;
        };
        // Arm / re-classify, and rate-limit the emission.
        let changed = self.stalled.map(|(c, _)| c) != Some(class);
        if changed {
            self.stalled = Some((class, now_ms));
        }
        let due = self
            .last_emit_ms
            .is_none_or(|e| now_ms.saturating_sub(e) >= EMIT_EVERY_MS);
        if changed || due {
            self.last_emit_ms = Some(now_ms);
            let since = self.stalled.map_or(now_ms, |(_, s)| s);
            return Some(self.build_report(
                class,
                now_ms,
                now_ms.saturating_sub(since),
                Some(core),
                pacer_pending,
            ));
        }
        None
    }

    /// The pure classifier: which gate, if any, is stuck. Ordered most-specific
    /// first (a hold masks the feed/present checks — a held frame is legitimately
    /// not presenting).
    pub fn verdict(
        &self,
        now_ms: u64,
        core: TermTrace,
        pacer_pending: bool,
        has_snapshot: bool,
    ) -> Option<StallClass> {
        // A synchronized-output hold: healthy within the backstop, stuck past it.
        if core.sync_held {
            return self
                .held_since_ms
                .filter(|&t| now_ms.saturating_sub(t) >= STALL_HOLD_MS)
                .map(|_| StallClass::HeldTooLong);
        }
        // Feeds arriving but nothing visible: the dirty-row hint is dropping updates.
        let invisible = core.feeds_seen.saturating_sub(self.feeds_at_last_visible);
        if invisible >= FEEDS_NOT_VISIBLE_MIN
            && now_ms.saturating_sub(self.last_visible_feed_ms.unwrap_or(0)) >= STALL_QUIET_MS
        {
            return Some(StallClass::FeedsNotVisible);
        }
        // Visible changes produced but not presented for a while: a dropped redraw
        // or a stuck pacer (a scene-equality skip lands as Clean, which re-baselines
        // — see `StallClass::StaleNoPresent`). Suppressed mid-resize (the
        // stretch-blit snapshot is intentionally holding the last frame).
        let visible_pending = core
            .visible_feeds
            .saturating_sub(self.visible_at_last_present);
        if !has_snapshot
            && visible_pending > 0
            && self
                .last_present_ms
                .is_some_and(|p| now_ms.saturating_sub(p) >= STALL_QUIET_MS)
        {
            let _ = pacer_pending; // carried into the report, not the decision
            // Discriminate the two shapes of "visible but not presented": a surface
            // stuck returning `Lost` is unpresentable (occluded/off-Space/asleep) — the
            // platform withholding the drawable, self-correcting on visibility — while
            // any other last outcome means acquire SUCCEEDED yet the frame never landed:
            // the real repaint bug (a dropped redraw or a stuck pacer).
            return Some(if self.last_outcome == Some(Outcome::Lost) {
                StallClass::SurfaceLost
            } else {
                StallClass::StaleNoPresent
            });
        }
        None
    }

    fn build_report(
        &self,
        class: StallClass,
        now_ms: u64,
        stalled_for_ms: u64,
        core: Option<TermTrace>,
        pacer_pending: bool,
    ) -> RenderReport {
        let ago = |o: Option<u64>| o.map(|t| now_ms.saturating_sub(t));
        let c = core.unwrap_or(self.last_core);
        RenderReport {
            class,
            stalled_for_ms,
            held_for_ms: ago(self.held_since_ms),
            since_feed_ms: ago(self.last_feed_ms),
            since_visible_feed_ms: ago(self.last_visible_feed_ms),
            since_redraw_cmd_ms: ago(self.last_redraw_cmd_ms),
            since_release_ms: ago(self.last_release_ms),
            since_redraw_event_ms: ago(self.last_redraw_event_ms),
            since_present_ms: ago(self.last_present_ms),
            since_input_ms: ago(self.last_input_ms),
            last_outcome: self.last_outcome,
            pacer_pending,
            feeds_seen: c.feeds_seen,
            visible_feeds: c.visible_feeds,
            feeds_while_held: c.feeds_while_held,
            sync_holds: c.sync_holds,
            released_by_tick: c.sync_released_by_tick,
            released_by_reset: c.sync_released_by_reset,
            pending_tick: self.last_tick_scheduled_ms > self.last_tick_fired_ms,
            presents: c.presents_marked,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A core snapshot with the fields a test cares about; the rest default.
    fn core(feeds: u64, visible: u64, held: bool) -> TermTrace {
        TermTrace {
            feeds_seen: feeds,
            visible_feeds: visible,
            sync_held: held,
            ..TermTrace::default()
        }
    }

    /// Drive a healthy lockstep: every pass a visible feed and a present. Never stalls.
    #[test]
    fn a_healthy_lockstep_never_stalls() {
        let mut t = RenderTrace::new();
        let mut now = 0;
        for i in 1..=50 {
            // A present lands, then a visible feed, each ~16 ms apart.
            assert_eq!(
                t.saw_outcome(Outcome::Presented, now, Some(core(i, i, false)), false),
                None
            );
            now += 8;
            assert_eq!(
                t.poll(now, Some(core(i, i, false)), false, false, true),
                None
            );
            now += 8;
        }
    }

    #[test]
    fn a_latched_hold_is_flagged_only_after_the_backstop_window() {
        let mut t = RenderTrace::new();
        // The hold opens at t=0 and stays set.
        assert_eq!(t.poll(0, Some(core(1, 1, true)), false, false, true), None);
        // Within the backstop window: still healthy (a real frame releases in 150 ms).
        assert_eq!(
            t.poll(500, Some(core(2, 2, true)), false, false, true),
            None
        );
        // Past a second still held: stuck.
        let r = t
            .poll(1_100, Some(core(3, 3, true)), false, false, true)
            .expect("a latched hold is a stall");
        assert_eq!(r.class, StallClass::HeldTooLong);
        assert!(r.held_for_ms.unwrap() >= 1_000);
    }

    #[test]
    fn feeds_that_never_show_are_flagged() {
        let mut t = RenderTrace::new();
        // One visible feed to baseline, presented.
        t.saw_outcome(Outcome::Presented, 0, Some(core(1, 1, false)), false);
        t.poll(0, Some(core(1, 1, false)), false, false, true);
        // Then a flood of invisible feeds (feeds_seen climbs, visible frozen).
        for i in 2..=25 {
            assert_eq!(
                t.poll(10 * i, Some(core(i, 1, false)), false, false, true),
                None
            );
        }
        // >20 invisible feeds and >2 s since the last visible one: flagged.
        let r = t
            .poll(3_000, Some(core(30, 1, false)), false, false, true)
            .expect("feeds not becoming visible is a stall");
        assert_eq!(r.class, StallClass::FeedsNotVisible);
    }

    #[test]
    fn visible_output_that_never_presents_is_flagged() {
        let mut t = RenderTrace::new();
        // Present once to set a baseline present time and visible count.
        t.saw_outcome(Outcome::Presented, 0, Some(core(1, 1, false)), false);
        t.poll(0, Some(core(1, 1, false)), false, false, true);
        // Visible feeds keep coming, but no present lands.
        assert_eq!(
            t.poll(500, Some(core(2, 2, false)), true, false, true),
            None
        );
        let r = t
            .poll(2_600, Some(core(5, 5, false)), true, false, true)
            .expect("visible output with no present is a stall");
        assert_eq!(r.class, StallClass::StaleNoPresent);
    }

    #[test]
    fn a_resize_snapshot_suppresses_the_no_present_stall() {
        let mut t = RenderTrace::new();
        t.saw_outcome(Outcome::Presented, 0, Some(core(1, 1, false)), false);
        t.poll(0, Some(core(1, 1, false)), false, false, true);
        // Same stale-no-present shape, but a resize blit is intentionally holding the
        // last frame — not a stall.
        assert_eq!(
            t.poll(2_600, Some(core(5, 5, false)), true, true, true),
            None,
            "a mid-resize blit is not a foreground stall"
        );
    }

    #[test]
    fn a_present_recovers_and_reports_a_stall() {
        let mut t = RenderTrace::new();
        t.saw_outcome(Outcome::Presented, 0, Some(core(1, 1, false)), false);
        t.poll(0, Some(core(1, 1, false)), false, false, true);
        // Fall into a stale-no-present stall.
        let r = t.poll(2_600, Some(core(5, 5, false)), true, false, true);
        assert_eq!(r.unwrap().class, StallClass::StaleNoPresent);
        // The recovering present reports the stall it ended (the kick oracle).
        let recovered = t
            .saw_outcome(Outcome::Presented, 2_700, Some(core(5, 5, false)), false)
            .expect("the present recovers the armed stall");
        assert_eq!(recovered.class, StallClass::StaleNoPresent);
        assert!(recovered.stalled_for_ms >= 100);
        // A second present with nothing armed reports nothing.
        assert_eq!(
            t.saw_outcome(Outcome::Presented, 2_800, Some(core(6, 6, false)), false),
            None
        );
    }

    #[test]
    fn a_continuing_stall_re_emits_only_on_the_interval() {
        let mut t = RenderTrace::new();
        t.saw_outcome(Outcome::Presented, 0, Some(core(1, 1, false)), false);
        t.poll(0, Some(core(1, 1, false)), false, false, true);
        // Enter the stall (first emit).
        assert!(
            t.poll(2_600, Some(core(5, 5, false)), true, false, true)
                .is_some()
        );
        // Shortly after: still stalled, but within the re-emit interval → quiet.
        assert!(
            t.poll(3_000, Some(core(6, 6, false)), true, false, true)
                .is_none()
        );
        // Past the interval: one more breadcrumb.
        assert!(
            t.poll(7_700, Some(core(9, 9, false)), true, false, true)
                .is_some()
        );
    }

    #[test]
    fn the_fleet_overview_never_stalls_and_re_baselines() {
        let mut t = RenderTrace::new();
        // Build up a stall in the single view.
        t.saw_outcome(Outcome::Presented, 0, Some(core(1, 1, false)), false);
        t.poll(0, Some(core(1, 1, false)), false, false, true);
        assert!(
            t.poll(2_600, Some(core(5, 5, false)), true, false, true)
                .is_some()
        );
        // Switch to the fleet: no foreground, so no stall and the state resets.
        assert_eq!(t.poll(2_700, None, false, false, true), None);
        // Back in a single view, the deltas start clean (a fresh baseline present).
        assert_eq!(
            t.poll(2_800, Some(core(5, 1, false)), false, false, true),
            None,
            "returning to a single view re-baselines rather than firing immediately"
        );
    }

    #[test]
    fn a_lost_surface_is_surface_lost_not_the_repaint_bug() {
        let mut t = RenderTrace::new();
        // Baseline present, then every present attempt comes back Lost: the surface
        // isn't acquirable (occluded/off-Space/display asleep) — the platform, not our
        // pipeline, is withholding the drawable.
        t.saw_outcome(Outcome::Presented, 0, Some(core(1, 1, false)), false);
        t.poll(0, Some(core(1, 1, false)), false, false, true);
        t.saw_outcome(Outcome::Lost, 100, Some(core(2, 2, false)), true);
        let r = t
            .poll(2_600, Some(core(5, 5, false)), true, false, true)
            .expect("a surface stuck Lost is still flagged");
        assert_eq!(
            r.class,
            StallClass::SurfaceLost,
            "a Lost-looping surface is not the stale-no-present repaint bug"
        );
    }

    #[test]
    fn acquire_ok_but_no_present_is_still_the_repaint_bug() {
        let mut t = RenderTrace::new();
        // The last present succeeded (acquire OK), then visible output piles up with no
        // present: this is the real foreground bug, kept distinct from SurfaceLost.
        t.saw_outcome(Outcome::Presented, 0, Some(core(1, 1, false)), false);
        t.poll(0, Some(core(1, 1, false)), false, false, true);
        let r = t
            .poll(2_600, Some(core(5, 5, false)), true, false, true)
            .expect("visible output with no present is a stall");
        assert_eq!(r.class, StallClass::StaleNoPresent);
    }

    #[test]
    fn a_clean_present_refreshes_the_baseline_so_idle_windows_dont_stall() {
        let mut t = RenderTrace::new();
        // A visible feed lands and is presented.
        t.saw_outcome(Outcome::Presented, 0, Some(core(1, 1, false)), false);
        t.poll(0, Some(core(1, 1, false)), false, false, true);
        // More visible feeds arrive, then the renderer confirms the scene is byte-for-
        // byte unchanged (Clean) — the screen is up to date — and the window goes idle.
        t.poll(100, Some(core(3, 3, false)), true, false, true);
        t.saw_outcome(Outcome::Clean, 120, Some(core(3, 3, false)), false);
        // Long after, still idle (no new feeds): a Clean means the screen matches the
        // scene, so this must NOT be reported as a stall.
        assert_eq!(
            t.poll(5_000, Some(core(3, 3, false)), false, false, true),
            None,
            "a Clean present confirms the screen is current — an idle window is not stalled"
        );
    }

    #[test]
    fn an_occluded_window_never_stalls_and_re_baselines() {
        let mut t = RenderTrace::new();
        t.saw_outcome(Outcome::Presented, 0, Some(core(1, 1, false)), false);
        t.poll(0, Some(core(1, 1, false)), false, false, true);
        // While occluded (visible=false) the surface can't present and feeds pile up,
        // but it must not be flagged — the platform is withholding, not our pipeline.
        assert_eq!(
            t.poll(3_000, Some(core(9, 9, false)), true, false, false),
            None
        );
        // Coming back visible re-baselines: no immediate fire from the hidden backlog.
        assert_eq!(
            t.poll(3_100, Some(core(9, 1, false)), false, false, true),
            None,
            "returning from occlusion re-baselines rather than firing on the hidden backlog"
        );
    }
}
