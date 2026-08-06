//! Resize coalescing — defer the expensive relayout during an interactive resize.
//!
//! A window resize is the costliest event the shell handles. In a single terminal
//! it reflows the screen and resizes the child PTY (a SIGWINCH the program usually
//! answers with a full repaint); in the fleet view every tile's preview texture
//! re-renders at the new size. Doing all that at every pixel of a drag pegs a
//! software rasterizer (lavapipe) and floods the children with resizes.
//!
//! Only a *drag* — a rapid stream of resizes — is worth coalescing. An isolated
//! resize (a maximize, a tiling snap, an un-maximize, or the very first grab of a
//! drag) is applied immediately and crisply: deferring it buys nothing (there is
//! no stream to collapse) and the blit + delayed surface/model resize only
//! shows a stale frame and can race the compositor's resize handshake. So
//! [`ResizeCoalescer::note`] returns [`Step::CommitNow`] for an isolated resize and
//! [`Step::Defer`] once resizes are streaming.
//!
//! For the deferred stream it records the latest requested size and reports *when*
//! to commit it via [`ResizeCoalescer::poll`]: once the drag settles (no new size
//! for [`SETTLE_MS`]), or — during a long continuous drag — every refresh
//! interval, so the content keeps refreshing instead of freezing. Between commits
//! the shell blits the last crisp frame (see the renderer's snapshot path),
//! a single textured quad that stays cheap no matter how many tiles are on screen.
//!
//! That interval is *measured*, not guessed: the shell charges the wall time each
//! commit actually costs it via [`ResizeCoalescer::charge`], and the next refresh
//! is scheduled a few multiples of that out (see [`COST_MULTIPLE`]). A fixed
//! interval has to assume the worst case — a fleet of tiles on a software
//! rasterizer — and so holds a single terminal on a GPU frozen for a quarter
//! second at a time when its relayout costs a millisecond or two. Measuring lets
//! the cheap case run nearly live and still backs the dear one off to [`MAX_MS`].
//!
//! It is pure (driven by an external millisecond clock) so its behaviour is
//! unit-testable without a window or GPU, exactly like [`crate::pacer`].

/// Commit once the window has held a size this long without changing (~5 frames):
/// short enough that releasing the drag snaps to crisp almost immediately, long
/// enough that an ordinary drag's stream of sizes coalesces into one relayout.
pub const SETTLE_MS: u64 = 80;

/// The widest a continuous drag's refresh interval can get, however dear the
/// relayout measures: past this the content reads as frozen rather than merely
/// coarse, so it is worth some stutter to refresh anyway.
pub const MAX_MS: u64 = 250;

/// The narrowest that interval can get. The event loop only re-checks every
/// ~8 ms and the frame pacer only presents every ~16 ms, so asking for commits
/// faster than this buys nothing that reaches the screen.
pub const MIN_MS: u64 = 8;

/// Refresh interval = the measured cost of a relayout × this, clamped to
/// [`MIN_MS`]..=[`MAX_MS`]. So a continuous drag spends at most a quarter of its
/// wall time relaying out, whatever the relayout happens to cost here — one
/// terminal on a GPU refreshes almost every frame, a fleet of tiles on a software
/// rasterizer backs off to the ceiling, and neither needs a constant guessed in
/// advance.
pub const COST_MULTIPLE: u64 = 4;

/// A resize counts as part of a drag only if it arrives within this long of the
/// previous one. Comfortably above a drag's per-frame cadence (~8–16 ms) yet well
/// below the gap between deliberate, separate actions (maximize, then un-maximize),
/// so one-shot resizes are recognised as isolated and applied at once.
pub const DRAG_GAP_MS: u64 = 100;

/// A resize target: the new surface size in physical pixels and the device scale.
pub type Target = (u32, u32, f64);

/// What to do with a resize the instant it arrives.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    /// Apply the relayout now — this resize stands alone (maximize, snap,
    /// un-maximize, or a drag's first grab), so there is nothing to coalesce.
    CommitNow(Target),
    /// A drag is streaming: blit and defer the relayout until [`poll`]
    /// reports the gesture has settled.
    ///
    /// [`poll`]: ResizeCoalescer::poll
    Defer,
}

/// Coalesces a burst of resize events into occasional commits — see the module
/// docs. Cheap to copy and `Default` (a zero-interval coalescer commits eagerly,
/// which no caller uses; construct with [`ResizeCoalescer::new`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct ResizeCoalescer {
    settle_ms: u64,
    max_ms: u64,
    drag_gap_ms: u64,
    /// The latest requested size awaiting commit, if any.
    pending: Option<Target>,
    /// When the most recent size was noted (`None` until the first ever), used for
    /// both the settle check and the is-this-a-drag gap.
    last_note_ms: Option<u64>,
    /// When the current uncommitted gesture began (for the refresh-interval check).
    anchor_ms: u64,
    /// Relayout work charged since the last commit, via [`ResizeCoalescer::charge`].
    cycle_ms: u64,
    /// Whether anything at all was charged for this cycle. Distinguishes "the
    /// relayout took under a millisecond" — a measurement, and the one that should
    /// pull the interval right in — from "nothing has been measured yet".
    charged: bool,
    /// What a relayout measured last, smoothed — the input to [`Self::interval_ms`].
    /// 0 until the first cycle has been charged.
    cost_ms: u64,
}

impl ResizeCoalescer {
    pub fn new(settle_ms: u64, max_ms: u64, drag_gap_ms: u64) -> Self {
        Self {
            settle_ms,
            max_ms,
            drag_gap_ms,
            pending: None,
            last_note_ms: None,
            anchor_ms: 0,
            cycle_ms: 0,
            charged: false,
            cost_ms: 0,
        }
    }

    /// Charge `ms` of real relayout work to the current cycle — the caller times
    /// the work a commit costs it (re-gridding and reflowing the sessions, then
    /// re-rendering the crisp frame) and hands the total back here. The next
    /// [`poll`] folds it into the measurement that paces the refreshes.
    ///
    /// [`poll`]: Self::poll
    pub fn charge(&mut self, ms: u64) {
        self.cycle_ms = self.cycle_ms.saturating_add(ms);
        self.charged = true;
    }

    /// How long to hold the last crisp frame during a continuous drag: the
    /// measured relayout cost times [`COST_MULTIPLE`], within the clamps.
    pub fn interval_ms(&self) -> u64 {
        self.cost_ms
            .saturating_mul(COST_MULTIPLE)
            .clamp(MIN_MS.min(self.max_ms), self.max_ms)
    }

    /// Record that the window was resized to `(w, h)` at device `scale` at
    /// `now_ms`, and decide what to do with it. An isolated resize (none pending
    /// and the previous one not recent) returns [`Step::CommitNow`] for the caller
    /// to apply at once; a resize that continues a rapid stream returns
    /// [`Step::Defer`], is stored as the latest pending size, and anchors the
    /// refresh-interval clock on the first deferred size of the gesture.
    pub fn note(&mut self, now_ms: u64, w: u32, h: u32, scale: f64) -> Step {
        let target = (w, h, scale);
        let continuing = self.pending.is_some()
            || self
                .last_note_ms
                .is_some_and(|t| now_ms.saturating_sub(t) <= self.drag_gap_ms);
        self.last_note_ms = Some(now_ms);
        if !continuing {
            self.pending = None;
            return Step::CommitNow(target);
        }
        if self.pending.is_none() {
            self.anchor_ms = now_ms;
        }
        self.pending = Some(target);
        Step::Defer
    }

    /// The size to commit now, if any: the drag has settled (no new size for
    /// `settle_ms`) or the refresh interval since the gesture began has elapsed.
    /// Returns the target and clears the pending size; otherwise returns `None`.
    pub fn poll(&mut self, now_ms: u64) -> Option<Target> {
        // Fold in whatever the last commit was charged before deciding, so a
        // relayout that turns expensive widens the very next interval rather than
        // the one after it. Whatever was charged since the last commit is what a
        // cycle costs here and now; take the worse of that sample and a decaying
        // average of it, so a jump is respected on the spot while one lucky fast
        // cycle only pulls the interval in gradually.
        if self.charged {
            self.cost_ms = self.cycle_ms.max((self.cost_ms * 3 + self.cycle_ms) / 4);
            self.cycle_ms = 0;
            self.charged = false;
        }
        let target = self.pending?;
        // `pending` is `Some`, so `last_note_ms` was set when it was stored.
        let last = self.last_note_ms.unwrap_or(now_ms);
        let settled = now_ms.saturating_sub(last) >= self.settle_ms;
        let due = now_ms.saturating_sub(self.anchor_ms) >= self.interval_ms();
        if settled || due {
            self.pending = None;
            // Re-anchor so a *mid-drag* (maxed) commit schedules the next refresh a
            // full interval out; after a settle commit `pending` is `None`, so the
            // next gesture's first `note` re-anchors anyway.
            self.anchor_ms = now_ms;
            Some(target)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coalescer() -> ResizeCoalescer {
        ResizeCoalescer::new(80, 250, 100)
    }

    #[test]
    fn nothing_to_commit_without_a_resize() {
        let mut c = coalescer();
        assert_eq!(c.poll(0), None);
        assert_eq!(c.poll(10_000), None);
    }

    #[test]
    fn an_isolated_resize_commits_immediately() {
        let mut c = coalescer();
        // A one-shot resize (maximize / snap / un-maximize): apply it at once, with
        // nothing left for `poll` to settle-commit later.
        assert_eq!(
            c.note(500, 1920, 1080, 1.0),
            Step::CommitNow((1920, 1080, 1.0))
        );
        assert_eq!(c.poll(600), None);
        assert_eq!(c.poll(10_000), None);
    }

    #[test]
    fn a_later_isolated_resize_commits_immediately_again() {
        let mut c = coalescer();
        assert_eq!(
            c.note(500, 1000, 600, 1.0),
            Step::CommitNow((1000, 600, 1.0))
        );
        // Much later (e.g. maximize, then un-maximize): still recognised as isolated.
        assert_eq!(
            c.note(2000, 800, 480, 1.0),
            Step::CommitNow((800, 480, 1.0))
        );
        assert_eq!(c.poll(2100), None);
    }

    #[test]
    fn a_drag_commits_the_first_step_then_defers_and_coalesces() {
        let mut c = coalescer();
        // The grab's first size lands immediately (one crisp relayout)...
        assert_eq!(
            c.note(500, 1000, 600, 1.0),
            Step::CommitNow((1000, 600, 1.0))
        );
        // ...which measures at 40 ms, dear enough to be worth coalescing at all.
        c.charge(40);
        // ...so the rapid stream that follows defers and coalesces.
        assert_eq!(c.note(516, 1010, 600, 1.0), Step::Defer);
        assert_eq!(c.note(532, 1020, 600, 1.0), Step::Defer);
        // Still moving — no commit yet (settle resets on every note).
        assert_eq!(c.poll(560), None);
        // 80 ms after the *last* note, the most recent size (only) commits.
        assert_eq!(c.poll(612), Some((1020, 600, 1.0)));
        assert_eq!(c.poll(613), None, "one commit drains the pending size");
    }

    #[test]
    fn a_pause_resets_the_gesture_so_the_next_resize_is_immediate() {
        let mut c = coalescer();
        // A drag: first step immediate, the next deferred and settle-committed.
        assert_eq!(
            c.note(500, 1000, 600, 1.0),
            Step::CommitNow((1000, 600, 1.0))
        );
        assert_eq!(c.note(516, 1100, 600, 1.0), Step::Defer);
        assert_eq!(c.poll(596), Some((1100, 600, 1.0)));
        // After a pause well beyond the drag gap, a fresh resize is isolated again.
        assert_eq!(
            c.note(900, 1200, 700, 1.0),
            Step::CommitNow((1200, 700, 1.0))
        );
        assert_eq!(c.poll(1000), None, "nothing deferred to settle-commit");
    }

    /// Drive a continuous drag (a size every 16 ms, never pausing) for `steps`,
    /// charging `cost_ms` of relayout work after every commit. Returns the times
    /// at which the coalescer asked for a real relayout.
    fn drag(c: &mut ResizeCoalescer, steps: u64, cost_ms: u64) -> Vec<u64> {
        let mut commits = Vec::new();
        assert_eq!(c.note(0, 1000, 600, 1.0), Step::CommitNow((1000, 600, 1.0)));
        c.charge(cost_ms);
        for step in 1..steps {
            let t = step * 16;
            assert_eq!(c.note(t, 1000 + step as u32, 600, 1.0), Step::Defer);
            // Poll the way the event loop does, a touch after the note.
            if c.poll(t + 1).is_some() {
                commits.push(t + 1);
                c.charge(cost_ms);
            }
        }
        commits
    }

    #[test]
    fn a_long_continuous_drag_refreshes_as_often_as_the_relayout_is_cheap() {
        // The whole point of measuring: on a window whose relayout costs ~2 ms
        // there is no reason to hold the content frozen for a quarter of a second,
        // so the drag reflows on nearly every frame and simply looks live.
        let mut c = coalescer();
        let commits = drag(&mut c, 64, 2);
        let gaps: Vec<u64> = commits.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(
            gaps.iter().all(|g| *g <= 32),
            "a 2 ms relayout should refresh about every frame, got gaps {gaps:?}"
        );
        assert!(
            commits.len() > 30,
            "a 1 s drag should refresh far more than the ceiling's 4 times, got {}",
            commits.len()
        );
    }

    #[test]
    fn a_ruinously_expensive_relayout_backs_off_to_the_ceiling() {
        // The other end: a fleet of tiles on a software rasterizer. Refreshing at
        // that cost would make the drag itself stutter, so it stays capped — the
        // guarantee the fixed ceiling used to give unconditionally.
        let mut c = coalescer();
        let commits = drag(&mut c, 128, 400);
        let gaps: Vec<u64> = commits.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(
            gaps.iter().all(|g| (MAX_MS..MAX_MS + 32).contains(g)),
            "an expensive relayout must refresh at the ceiling, got gaps {gaps:?}"
        );
    }

    #[test]
    fn a_measured_cost_sets_the_interval_between_the_two() {
        // In between, the interval tracks the measurement: a relayout is allowed
        // about a quarter of the drag's wall time.
        let mut c = coalescer();
        let commits = drag(&mut c, 128, 20);
        let gaps: Vec<u64> = commits.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(
            gaps.iter().all(|g| (20 * COST_MULTIPLE..).contains(g)),
            "must not refresh faster than the measured cost allows, got {gaps:?}"
        );
        assert!(
            gaps.iter().all(|g| *g < MAX_MS),
            "a 20 ms relayout is nowhere near the ceiling, got {gaps:?}"
        );
    }

    #[test]
    fn a_drag_that_turns_expensive_backs_off_at_once() {
        // Cost is not fixed for the life of a drag — dragging a window bigger makes
        // its own relayout dearer, and switching to the fleet mid-drag much dearer.
        // A jump must be respected on the very next interval, not averaged away over
        // several, or the stutter it guards against happens first.
        // One unbroken drag whose relayout costs 2 ms until half way through and
        // 400 ms after — the same shape as diving into the fleet mid-drag.
        let mut c = coalescer();
        let mut commits = Vec::new();
        assert_eq!(c.note(0, 1000, 600, 1.0), Step::CommitNow((1000, 600, 1.0)));
        c.charge(2);
        for step in 1..80u64 {
            let t = step * 16;
            assert_eq!(c.note(t, 1000 + step as u32, 600, 1.0), Step::Defer);
            if c.poll(t + 1).is_some() {
                commits.push(t + 1);
                c.charge(if step < 32 { 2 } else { 400 });
            }
        }
        let jump = commits
            .iter()
            .position(|t| *t > 32 * 16)
            .expect("the drag commits past the point the cost jumps");
        let after: Vec<u64> = commits[jump..].windows(2).map(|w| w[1] - w[0]).collect();
        assert!(!after.is_empty(), "the drag runs on past the jump");
        assert!(
            after.iter().all(|g| *g >= MAX_MS),
            "the very first interval after the jump must already be backed off, \
             got gaps {after:?}"
        );
    }

    #[test]
    fn an_unmeasured_drag_refreshes_eagerly() {
        // Before anything has been measured, err toward showing the user real
        // content: commit at the floor and let the first measurement correct it.
        let mut c = coalescer();
        assert_eq!(c.note(0, 1000, 600, 1.0), Step::CommitNow((1000, 600, 1.0)));
        assert_eq!(c.note(16, 1001, 600, 1.0), Step::Defer);
        assert_eq!(c.poll(16 + MIN_MS), Some((1001, 600, 1.0)));
    }

    #[test]
    fn settle_takes_priority_when_a_drag_stops_before_the_refresh_interval() {
        // A short drag that stops before its refresh interval is up must commit
        // via settle, once. A 40 ms relayout puts that interval at 160 ms.
        let mut c = coalescer();
        assert_eq!(c.note(0, 0, 0, 1.0), Step::CommitNow((0, 0, 1.0)));
        c.charge(40);
        for step in 1..5u64 {
            let t = step * 16; // last note at 64ms
            assert_eq!(c.note(t, step as u32, 0, 1.0), Step::Defer);
            assert_eq!(c.poll(t + 1), None, "still dragging at {t}ms");
        }
        // Stops at 64ms; settle fires at 64+80 = 144ms (< the 16+160 interval).
        assert_eq!(c.poll(143), None);
        assert_eq!(c.poll(144), Some((4, 0, 1.0)));
        assert_eq!(c.poll(300), None, "no spurious second commit");
    }
}
