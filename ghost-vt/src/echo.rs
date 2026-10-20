//! The host's echo acknowledgement and the client's round-trip clock — the two
//! timing facts predictive local echo needs from the wire (see
//! `docs/predictive-echo-design.md`). Both are mosh's: `Complete::set_echo_ack`
//! in `statesync/completeterminal.cc` and the RFC 6298 estimator in
//! `network/network.cc`. Pure: every call takes the time it runs at.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long after an input reaches the child its echo is expected on screen.
/// Past this, the screen a client holds "should" show what the input did.
pub const ECHO_TIMEOUT: Duration = Duration::from_millis(50);

/// The host side, one per display connection: which numbered inputs have been
/// written to the child, when, and the newest one old enough to acknowledge.
#[derive(Debug, Default)]
pub struct EchoAck {
    /// Numbered inputs still in the PTY queue, as `(end, seq)`: `end` is how
    /// many queued child-bound bytes must go out before this input's last one
    /// has.
    queued: VecDeque<(usize, u64)>,
    /// Inputs the child's PTY has taken, with when, oldest first. The front is
    /// the acknowledged one or older; everything behind it is waiting.
    written: VecDeque<(u64, Instant)>,
    ack: u64,
}

impl EchoAck {
    /// Input `seq` was queued for the child, its last byte at position `end`
    /// of the child-bound queue (the queue's length right after queuing it).
    pub fn queued(&mut self, seq: u64, end: usize) {
        self.queued.push_back((end, seq));
    }

    /// The PTY took the next `n` bytes of the child-bound queue at `now`:
    /// every input those complete is written.
    pub fn drained(&mut self, n: usize, now: Instant) {
        for (end, _) in &mut self.queued {
            *end = end.saturating_sub(n);
        }
        while let Some(&(0, seq)) = self.queued.front() {
            self.queued.pop_front();
            self.written.push_back((seq, now));
        }
    }

    /// Advance the acknowledgement to the newest input written at least
    /// [`ECHO_TIMEOUT`] before `now`. Returns it when it moved.
    pub fn advance(&mut self, now: Instant) -> Option<u64> {
        let newest = self
            .written
            .iter()
            .take_while(|(_, at)| *at + ECHO_TIMEOUT <= now)
            .last()
            .map(|(seq, _)| *seq)?;
        while self.written.front().is_some_and(|(seq, _)| *seq < newest) {
            self.written.pop_front();
        }
        (newest > self.ack).then(|| {
            self.ack = newest;
            newest
        })
    }

    /// When [`advance`](Self::advance) next has something to say, if anything
    /// is waiting — the host must wake then even if the child stays silent,
    /// since silence is exactly what proves a prediction wrong.
    pub fn next_due(&self) -> Option<Instant> {
        self.written
            .iter()
            .find(|(seq, _)| *seq > self.ack)
            .map(|(_, at)| *at + ECHO_TIMEOUT)
    }
}

/// Smoothed round-trip time, as mosh keeps it: RFC 6298 with α = 1/8, β = 1/4,
/// starting at 1 s so a link is treated as slow until it proves otherwise.
#[derive(Debug, Clone, Copy)]
pub struct Rtt {
    srtt_ms: f64,
    rttvar_ms: f64,
    sampled: bool,
}

impl Default for Rtt {
    fn default() -> Self {
        Rtt {
            srtt_ms: 1000.0,
            rttvar_ms: 500.0,
            sampled: false,
        }
    }
}

impl Rtt {
    /// Fold in one measured round trip.
    pub fn sample(&mut self, rtt: Duration) {
        let r = rtt.as_secs_f64() * 1000.0;
        if self.sampled {
            const ALPHA: f64 = 1.0 / 8.0;
            const BETA: f64 = 1.0 / 4.0;
            self.rttvar_ms = (1.0 - BETA) * self.rttvar_ms + BETA * (self.srtt_ms - r).abs();
            self.srtt_ms = (1.0 - ALPHA) * self.srtt_ms + ALPHA * r;
        } else {
            self.srtt_ms = r;
            self.rttvar_ms = r / 2.0;
            self.sampled = true;
        }
    }

    pub fn srtt(&self) -> Duration {
        Duration::from_secs_f64(self.srtt_ms / 1000.0)
    }
}

/// How often a client re-times the link while the user types.
pub const PING_INTERVAL: Duration = Duration::from_millis(250);

/// The client side: numbers this connection's input and times its round trip
/// with pings — one outstanding at a time, sent on attach and then while the
/// user types, so an idle session never chatters.
#[derive(Debug, Default)]
pub struct InputClock {
    seq: u64,
    next_ping: u64,
    outstanding: Option<(u64, Instant)>,
    last_ping: Option<Instant>,
    typed_since_ping: bool,
    rtt: Rtt,
}

impl InputClock {
    /// The number for the next input message (from 1).
    pub fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.typed_since_ping = true;
        self.seq
    }

    /// The last number handed out, if any input has been numbered yet.
    pub fn last_seq(&self) -> Option<u64> {
        (self.seq > 0).then_some(self.seq)
    }

    /// The id of a ping to send now, if one is due: none is in flight, and
    /// this is the first, or the user has typed since the last one and it is
    /// [`PING_INTERVAL`] old.
    pub fn ping_due(&mut self, now: Instant) -> Option<u64> {
        if self.outstanding.is_some() {
            return None;
        }
        let due = match self.last_ping {
            None => true,
            Some(at) => self.typed_since_ping && now.duration_since(at) >= PING_INTERVAL,
        };
        if !due {
            return None;
        }
        self.next_ping += 1;
        self.outstanding = Some((self.next_ping, now));
        self.last_ping = Some(now);
        self.typed_since_ping = false;
        Some(self.next_ping)
    }

    /// The host answered ping `id` at `now`.
    pub fn pong(&mut self, id: u64, now: Instant) {
        if let Some((sent, at)) = self.outstanding
            && sent == id
        {
            self.outstanding = None;
            self.rtt.sample(now.duration_since(at));
        }
    }

    pub fn srtt(&self) -> Duration {
        self.rtt.srtt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn an_input_is_acknowledged_once_written_for_the_echo_timeout() {
        let t0 = Instant::now();
        let mut e = EchoAck::default();
        e.queued(1, 3);
        e.drained(2, t0);
        assert_eq!(e.next_due(), None, "not written yet: nothing to wait for");
        e.drained(1, t0);
        assert_eq!(e.next_due(), Some(t0 + ECHO_TIMEOUT));
        assert_eq!(e.advance(t0 + ms(49)), None, "too soon to judge the echo");
        assert_eq!(e.advance(t0 + ms(50)), Some(1));
        assert_eq!(e.advance(t0 + ms(80)), None, "acknowledged once");
        assert_eq!(e.next_due(), None);
    }

    #[test]
    fn the_acknowledgement_jumps_to_the_newest_old_enough_input() {
        let t0 = Instant::now();
        let mut e = EchoAck::default();
        for (seq, end) in [(1, 1), (2, 2), (3, 3)] {
            e.queued(seq, end);
        }
        e.drained(2, t0);
        e.drained(1, t0 + ms(30));
        assert_eq!(
            e.advance(t0 + ms(60)),
            Some(2),
            "3 is still inside its window"
        );
        assert_eq!(e.next_due(), Some(t0 + ms(80)));
        assert_eq!(e.advance(t0 + ms(80)), Some(3));
    }

    #[test]
    fn the_rtt_starts_slow_then_follows_rfc_6298() {
        let mut r = Rtt::default();
        assert_eq!(r.srtt(), ms(1000), "unmeasured: assume a slow link");
        r.sample(ms(400));
        assert_eq!(r.srtt(), ms(400), "the first sample is taken whole");
        r.sample(ms(0));
        assert_eq!(r.srtt(), ms(350), "then smoothed by 1/8");
    }

    #[test]
    fn pings_go_on_attach_then_only_while_typing() {
        let t0 = Instant::now();
        let mut c = InputClock::default();
        let first = c.ping_due(t0).expect("the first ping goes at once");
        assert_eq!(c.ping_due(t0 + ms(500)), None, "one in flight at a time");
        c.pong(first, t0 + ms(200));
        assert_eq!(c.srtt(), ms(200));
        assert_eq!(c.ping_due(t0 + ms(900)), None, "idle: no chatter");
        assert_eq!(c.next_seq(), 1);
        assert_eq!(c.next_seq(), 2);
        assert_eq!(c.last_seq(), Some(2));
        assert!(
            c.ping_due(t0 + ms(900)).is_some(),
            "typing re-times the link"
        );
    }

    #[test]
    fn a_stale_pong_is_ignored() {
        let t0 = Instant::now();
        let mut c = InputClock::default();
        let id = c.ping_due(t0).unwrap();
        c.pong(id + 1, t0 + ms(10));
        assert_eq!(c.srtt(), ms(1000));
    }
}
