//! Repaint pacing — the shell's defence against redraw storms.
//!
//! The core returns [`Cmd::Redraw`](ghost_ui_core::Cmd::Redraw) whenever the
//! scene might have changed: every batch of session output, every keystroke,
//! every resize step. On a software rasterizer (lavapipe) a full-window repaint
//! is far from free, and the event loop pumps sessions every 8 ms — so a single
//! chatty previewed session, or a held arrow key, would drive ~125 full repaints
//! a second and peg a core.
//!
//! [`FramePacer`] collapses that to a frame budget: it remembers when the window
//! last painted and, when a repaint is requested, either lets it through now or
//! defers it until the budget has elapsed, coalescing every request in between
//! into the one paint. It is pure (driven by an external millisecond clock) so
//! its behaviour is unit-testable without a window or GPU.

/// One paint per this many milliseconds, at most (~60 Hz). Comfortably above any
/// human input rate, so navigation stays responsive, while capping the cost of a
/// session that floods output.
pub const FRAME_BUDGET_MS: u64 = 16;

/// What the shell should do for a window right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pace {
    /// Repaint now, then call [`FramePacer::painted`].
    PaintNow,
    /// A repaint is pending but rate-limited; wake again after this many ms to
    /// paint it (the shell folds this into its control-flow deadline).
    WaitMs(u64),
    /// Nothing to paint.
    Idle,
}

/// Rate-limits repaints to one per [`FRAME_BUDGET_MS`], coalescing bursts.
#[derive(Clone, Copy, Debug, Default)]
pub struct FramePacer {
    budget_ms: u64,
    last_paint_ms: Option<u64>,
    pending: bool,
}

impl FramePacer {
    pub fn new(budget_ms: u64) -> Self {
        Self {
            budget_ms,
            last_paint_ms: None,
            pending: false,
        }
    }

    /// Note that a repaint was requested (a `Cmd::Redraw` was returned). Cheap and
    /// idempotent — many requests in one frame collapse to a single pending flag.
    pub fn request(&mut self) {
        self.pending = true;
    }

    /// Decide what to do at `now_ms`. The first paint, and any paint a full budget
    /// after the last, goes through immediately; a sooner one is deferred so the
    /// caller wakes to paint exactly when the budget expires.
    pub fn poll(&mut self, now_ms: u64) -> Pace {
        if !self.pending {
            return Pace::Idle;
        }
        match self.last_paint_ms {
            None => Pace::PaintNow,
            Some(last) => {
                let elapsed = now_ms.saturating_sub(last);
                if elapsed >= self.budget_ms {
                    Pace::PaintNow
                } else {
                    Pace::WaitMs(self.budget_ms - elapsed)
                }
            }
        }
    }

    /// Record that a frame actually landed at `now_ms` (presented, or verified
    /// identical to what's on screen), clearing the pending flag. Never call this
    /// at request time: the platform may drop a requested redraw (occluded window,
    /// another Space, the lock screen), and a repaint marked painted then would be
    /// lost — the window stays stale until some input forces a fresh request.
    pub fn painted(&mut self, now_ms: u64) {
        self.last_paint_ms = Some(now_ms);
        self.pending = false;
    }

    /// Resolve a repaint attempt by whether a frame actually landed: `painted` if it
    /// did, `request` (keep pending, retry) if it did not. The one call every paint
    /// path should end with, so a failed present — a blit whose surface acquire failed,
    /// a `FrameOutcome::Lost` — can never be mistaken for a landed frame and strand the
    /// window on stale content. See [`painted`](Self::painted).
    pub fn settle(&mut self, landed: bool, now_ms: u64) {
        if landed {
            self.painted(now_ms);
        } else {
            self.request();
        }
    }

    /// Whether a repaint is pending — requested but not yet confirmed painted.
    /// The render trace reads it to tell a stuck pacer from a stuck platform.
    pub fn pending(&self) -> bool {
        self.pending
    }

    /// One decision per event-loop pass: should the shell call
    /// `window.request_redraw()` now? Does NOT mark anything painted — that
    /// happens only when the resulting `RedrawRequested` is actually handled
    /// (see [`painted`](Self::painted)) — so an unconfirmed release keeps
    /// releasing every pass until a frame lands. `request_redraw` coalesces,
    /// so the retries are free while the platform is dropping them.
    pub fn release(&mut self, now_ms: u64) -> bool {
        self.poll(now_ms) == Pace::PaintNow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_until_a_repaint_is_requested() {
        let mut p = FramePacer::new(16);
        assert_eq!(p.poll(0), Pace::Idle);
        assert_eq!(p.poll(1000), Pace::Idle);
    }

    #[test]
    fn first_repaint_goes_through_immediately() {
        let mut p = FramePacer::new(16);
        p.request();
        assert_eq!(p.poll(0), Pace::PaintNow);
    }

    #[test]
    fn a_repaint_within_the_budget_is_deferred_to_the_deadline() {
        let mut p = FramePacer::new(16);
        p.request();
        assert_eq!(p.poll(0), Pace::PaintNow);
        p.painted(0);

        // 5 ms later another repaint is requested: defer the remaining 11 ms.
        p.request();
        assert_eq!(p.poll(5), Pace::WaitMs(11));
        // Re-polling before the deadline keeps deferring (no early paint).
        assert_eq!(p.poll(10), Pace::WaitMs(6));
    }

    #[test]
    fn the_pending_repaint_fires_once_the_budget_elapses() {
        let mut p = FramePacer::new(16);
        p.request();
        p.poll(0);
        p.painted(0);

        p.request();
        assert_eq!(p.poll(16), Pace::PaintNow);
        p.painted(16);
        // Having painted, nothing is pending.
        assert_eq!(p.poll(20), Pace::Idle);
    }

    #[test]
    fn a_burst_of_requests_coalesces_into_one_paint() {
        let mut p = FramePacer::new(16);
        p.request();
        p.poll(0);
        p.painted(0);

        // Output floods in: a request every 2 ms for the whole budget window.
        for t in [2, 4, 6, 8, 10, 12, 14] {
            p.request();
            assert_eq!(p.poll(t), Pace::WaitMs(16 - t), "deferred at {t}ms");
        }
        // One paint clears them all; no backlog of frames to render.
        assert_eq!(p.poll(16), Pace::PaintNow);
        p.painted(16);
        assert_eq!(p.poll(17), Pace::Idle);
    }

    #[test]
    fn a_dropped_redraw_request_is_retried_until_a_frame_lands() {
        // The platform is free to drop a requested redraw on the floor — macOS
        // delivers no RedrawRequested for a window on another Space or behind
        // the lock screen. A released repaint must therefore stay pending until
        // `painted` confirms a frame actually landed; otherwise the window
        // shows stale content until some input forces a fresh request.
        let mut p = FramePacer::new(16);
        p.request();
        assert!(p.release(0), "a pending repaint releases");
        // No painted(): the RedrawRequested never arrived. Keep asking.
        assert!(p.release(16), "an unconfirmed repaint must release again");
        // Only a frame that actually landed clears the pending repaint.
        p.painted(16);
        assert!(!p.release(32), "a painted frame is no longer pending");
    }

    #[test]
    fn settle_keeps_an_unpresented_frame_pending_but_clears_a_landed_one() {
        // The blit/resize path and the scene path both end by telling the pacer
        // whether a frame actually reached the glass. A frame that did NOT land (a
        // failed surface acquire) must stay pending so `release` retries it — the same
        // contract as a dropped redraw; marking it painted would strand the window on
        // a stale frame.
        let mut p = FramePacer::new(16);
        p.request();
        assert!(p.release(0), "a pending repaint releases");
        p.settle(false, 0); // the acquire failed: nothing was presented
        assert!(
            p.pending(),
            "a frame that did not land must stay pending for retry"
        );
        p.settle(true, 16); // a later attempt landed
        assert!(!p.pending(), "a landed frame clears the pending repaint");
    }

    #[test]
    fn input_rate_paints_are_never_throttled() {
        // Key-repeat is ~30 Hz (~33 ms apart) — always past a 16 ms budget, so
        // every keystroke paints immediately: navigation never feels rate-limited.
        let mut p = FramePacer::new(16);
        let mut now = 0;
        for _ in 0..10 {
            p.request();
            assert_eq!(p.poll(now), Pace::PaintNow);
            p.painted(now);
            now += 33;
        }
    }
}
