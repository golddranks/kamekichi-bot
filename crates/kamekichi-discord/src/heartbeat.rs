use std::time::{Duration, Instant};

use kamekichi_ws as ws;

use crate::gateway::OP_HEARTBEAT;
use crate::{Error, Result};

/// Reject heartbeat intervals above this threshold (5 minutes).
/// Discord typically sends ~41.25 s; anything beyond this is garbage.
const MAX_HEARTBEAT_INTERVAL_MS: u64 = 300_000;

/// Gateway heartbeat state machine.
///
/// Tracks the heartbeat interval, next-due time, and ACK state to
/// detect zombie connections.  Two independent checks catch zombies:
/// the per-heartbeat `ack_pending` flag, and a wall-clock backstop
/// (`last_ack` elapsed > 2× interval).
pub(crate) struct HeartbeatState {
    interval: Duration,
    next_heartbeat: Instant,
    /// Whether a heartbeat ACK is outstanding.
    ///
    /// This is a boolean rather than a counter because server-requested
    /// heartbeats (OP 1) can cause two in-flight heartbeats.  Discord
    /// sends two ACKs — the first clears `ack_pending`, the second is
    /// absorbed as a spurious ACK by `receive_ack`.  This is safe because
    /// `send` resets `next_heartbeat`, so the normal "overdue +
    /// ack_pending" check won't fire until a full interval after the
    /// second send, and the backstop (`last_ack` elapsed > 2×interval) is
    /// independent of `next_heartbeat`.
    ack_pending: bool,
    last_ack: Instant,
}

impl HeartbeatState {
    pub(crate) fn new(interval_ms: u64, jitter: f64) -> Result<Self> {
        debug_assert!(
            (0.0..=1.0).contains(&jitter),
            "jitter must be in [0.0, 1.0], got {jitter}",
        );
        if interval_ms == 0 || interval_ms > MAX_HEARTBEAT_INTERVAL_MS {
            return Err(Error::BadHeartbeatInterval(interval_ms));
        }
        let interval = Duration::from_millis(interval_ms);
        let now = Instant::now();
        Ok(Self {
            interval,
            next_heartbeat: now + interval.mul_f64(jitter),
            ack_pending: false,
            last_ack: now,
        })
    }

    pub(crate) fn interval(&self) -> Duration {
        self.interval
    }

    /// Maximum time we wait for a heartbeat ACK before declaring the
    /// connection dead.  Used both for zombie detection and as the
    /// READY-wait deadline during handshake.
    pub(crate) fn backstop_window(&self) -> Duration {
        self.interval.saturating_mul(2)
    }

    /// Wall-clock backstop for zombie detection.
    ///
    /// Returns `Err(ConnectionZombied)` if an ACK has been outstanding for
    /// more than the backstop window.
    fn backstop_check(&self) -> Result {
        let ack_elapsed = self.last_ack.elapsed();
        if self.ack_pending && ack_elapsed > self.backstop_window() {
            return Err(Error::ConnectionZombied);
        }
        Ok(())
    }

    /// Check whether a heartbeat should be sent.
    ///
    /// Returns `Ok(true)` if due, `Ok(false)` if not yet, or
    /// `Err(ConnectionZombied)` if the connection is dead.
    fn heartbeat_check(&self) -> Result<bool> {
        self.backstop_check()?;
        if Instant::now() >= self.next_heartbeat {
            if self.ack_pending {
                return Err(Error::ConnectionZombied);
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Handle a heartbeat ACK from the server.
    ///
    /// Spurious ACKs (no pending heartbeat) are silently absorbed.
    pub(crate) fn receive_ack(&mut self) {
        if self.ack_pending {
            self.ack_pending = false;
            self.last_ack = Instant::now();
        }
    }

    /// Compute the read timeout for the next poll iteration.
    ///
    /// Returns the minimum of the time until the next heartbeat and the
    /// time until the backstop fires.
    pub(crate) fn poll_timeout(&self) -> Duration {
        let mut remaining = self
            .next_heartbeat
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1));
        if self.ack_pending {
            let backstop = self
                .backstop_window()
                .checked_sub(self.last_ack.elapsed())
                .unwrap_or(Duration::ZERO)
                .max(Duration::from_millis(1));
            remaining = remaining.min(backstop);
        }
        remaining
    }

    /// Send a heartbeat frame and mark it as pending ACK.
    pub(crate) fn send<S: std::io::Read + std::io::Write>(
        &mut self,
        ws: &mut ws::WebSocket<S, impl rand_core::Rng>,
        sequence: Option<u64>,
        buf: &mut Vec<u8>,
    ) -> Result {
        use std::io::Write;
        buf.clear();
        match sequence {
            Some(s) => write!(buf, r#"{{"op":{OP_HEARTBEAT},"d":{s}}}"#),
            None => write!(buf, r#"{{"op":{OP_HEARTBEAT},"d":null}}"#),
        }
        .expect("write to Vec cannot fail");
        match ws.send_text(std::str::from_utf8(buf).expect("heartbeat payload is ASCII")) {
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }
        self.next_heartbeat = Instant::now() + self.interval;
        self.ack_pending = true;
        Ok(())
    }

    /// Send a heartbeat if the interval has elapsed, or error if zombied.
    pub(crate) fn send_if_due<S: std::io::Read + std::io::Write>(
        &mut self,
        ws: &mut ws::WebSocket<S, impl rand_core::Rng>,
        sequence: Option<u64>,
        buf: &mut Vec<u8>,
    ) -> Result {
        if self.heartbeat_check()? {
            self.send(ws, sequence, buf)?;
        }
        Ok(())
    }

    /// Handle a server-requested heartbeat (OP 1).
    ///
    /// Checks the backstop and sends a heartbeat.
    pub(crate) fn server_requested<S: std::io::Read + std::io::Write>(
        &mut self,
        ws: &mut ws::WebSocket<S, impl rand_core::Rng>,
        sequence: Option<u64>,
        buf: &mut Vec<u8>,
    ) -> Result {
        self.backstop_check()?;
        self.send(ws, sequence, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INT: Duration = Duration::from_secs(30);

    fn hb(ack_pending: bool, heartbeat_ago: Duration, ack_ago: Duration) -> HeartbeatState {
        let now = Instant::now();
        HeartbeatState {
            interval: INT,
            next_heartbeat: now + INT.checked_sub(heartbeat_ago).unwrap_or(Duration::ZERO),
            ack_pending,
            last_ack: now - ack_ago,
        }
    }

    // ---- Jitter ----

    #[test]
    fn jitter_zero_fires_immediately() {
        let hb = HeartbeatState::new(41_000, 0.0).unwrap();
        // deadline ≈ now, so heartbeat is due immediately
        let until = hb.next_heartbeat.saturating_duration_since(Instant::now());
        assert!(until < Duration::from_millis(100));
    }

    #[test]
    fn jitter_one_fires_after_full_interval() {
        let hb = HeartbeatState::new(41_000, 1.0).unwrap();
        // deadline ≈ now + interval
        let until = hb.next_heartbeat.saturating_duration_since(Instant::now());
        assert!(until >= Duration::from_secs(40));
        assert!(until <= Duration::from_secs(42));
    }

    #[test]
    fn jitter_half_fires_after_half_interval() {
        let hb = HeartbeatState::new(40_000, 0.5).unwrap();
        // deadline ≈ now + 20s
        let until = hb.next_heartbeat.saturating_duration_since(Instant::now());
        assert!(until >= Duration::from_secs(19));
        assert!(until <= Duration::from_secs(21));
    }

    // ---- Heartbeat state machine ----

    #[test]
    fn heartbeat_not_due_yet() {
        let r = hb(false, Duration::from_secs(10), Duration::from_secs(10)).heartbeat_check();
        assert_eq!(r.unwrap(), false);
    }

    #[test]
    fn heartbeat_due_no_ack_pending() {
        let r = hb(false, Duration::from_secs(31), Duration::from_secs(5)).heartbeat_check();
        assert_eq!(r.unwrap(), true);
    }

    #[test]
    fn heartbeat_overdue_no_ack_pending() {
        let r = hb(false, Duration::from_secs(45), Duration::from_secs(5)).heartbeat_check();
        assert_eq!(r.unwrap(), true);
    }

    #[test]
    fn heartbeat_due_ack_pending_zombied() {
        let r = hb(true, Duration::from_secs(31), Duration::from_secs(31)).heartbeat_check();
        assert!(matches!(r, Err(Error::ConnectionZombied)));
    }

    #[test]
    fn backstop_zombied_before_heartbeat_due() {
        // Heartbeat not due yet, but ACK overdue beyond 2x interval.
        let r = hb(true, Duration::from_secs(10), Duration::from_secs(62)).heartbeat_check();
        assert!(matches!(r, Err(Error::ConnectionZombied)));
    }

    #[test]
    fn backstop_within_window_not_zombied() {
        // Well within 2x -- not zombied.
        let r = hb(true, Duration::from_secs(10), Duration::from_secs(50)).heartbeat_check();
        assert_eq!(r.unwrap(), false);
    }

    #[test]
    fn ack_pending_within_backstop_not_due() {
        // Waiting for ACK, heartbeat not due yet, within 2x window -- keep waiting.
        let r = hb(true, Duration::from_secs(10), Duration::from_secs(40)).heartbeat_check();
        assert_eq!(r.unwrap(), false);
    }

    // ---- backstop_check unit tests ----

    #[test]
    fn backstop_no_ack_pending() {
        assert!(
            hb(false, Duration::ZERO, Duration::from_secs(999))
                .backstop_check()
                .is_ok()
        );
    }

    #[test]
    fn backstop_within_window() {
        assert!(
            hb(true, Duration::ZERO, Duration::from_secs(50))
                .backstop_check()
                .is_ok()
        );
    }

    #[test]
    fn backstop_exceeded() {
        assert!(matches!(
            hb(true, Duration::ZERO, Duration::from_secs(62)).backstop_check(),
            Err(Error::ConnectionZombied)
        ));
    }
}
