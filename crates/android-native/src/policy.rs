//! Pure decisions about when the phone's audio devices are open.

use std::time::{Duration, Instant};

use audiobridge_core::session::HubStatus;

/// The output stream is kept open this long after the PC stops sending audio.
pub const OUTPUT_IDLE_CLOSE: Duration = Duration::from_secs(30);

/// Mic capture runs iff some connected PC wants the phone mic (`mic_wanted`: mic enabled on both sides and
/// an app there is consuming the virtual mic) and Kotlin reports that recording is allowed
/// (permission + microphone FGS + user toggle).
pub fn mic_should_capture(status: &HubStatus, allowed: bool) -> bool {
    allowed && status.mic_wanted
}

/// Output stream lifetime: open as soon as PC audio is active, close after
/// [`OUTPUT_IDLE_CLOSE`] of continuous inactivity.
#[derive(Debug, Default)]
pub struct OutputPolicy {
    open: bool,
    idle_since: Option<Instant>,
}

impl OutputPolicy {
    /// Feeds the current activity flag; returns whether the output stream should be open now.
    pub fn update(&mut self, active: bool, now: Instant) -> bool {
        if active {
            self.open = true;
            self.idle_since = None;
        } else if self.open {
            let since = *self.idle_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= OUTPUT_IDLE_CLOSE {
                self.open = false;
                self.idle_since = None;
            }
        }
        self.open
    }

    /// When `update` must be called again even if nothing changes (the idle close deadline).
    pub fn deadline(&self) -> Option<Instant> {
        self.idle_since.map(|t| t + OUTPUT_IDLE_CLOSE)
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Retry delay for failed device opens / device errors: 200 ms doubling up to 10 s.
#[derive(Debug)]
pub struct Backoff {
    next: Duration,
}

impl Backoff {
    const FIRST: Duration = Duration::from_millis(200);
    const MAX: Duration = Duration::from_secs(10);

    /// Returns the delay to wait now and grows the following one.
    pub fn fail(&mut self) -> Duration {
        let d = self.next;
        self.next = (self.next * 2).min(Self::MAX);
        d
    }

    pub fn reset(&mut self) {
        self.next = Self::FIRST;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff { next: Self::FIRST }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_opens_on_activity_and_closes_after_idle_period() {
        let t0 = Instant::now();
        let mut p = OutputPolicy::default();
        assert!(!p.update(false, t0), "never opens without audio");
        assert_eq!(p.deadline(), None);

        assert!(p.update(true, t0));
        assert!(p.update(false, t0 + Duration::from_secs(1)));
        assert_eq!(p.deadline(), Some(t0 + Duration::from_secs(1) + OUTPUT_IDLE_CLOSE));
        assert!(p.update(false, t0 + Duration::from_secs(30)), "still inside the idle window");
        assert!(!p.update(false, t0 + Duration::from_secs(31)));
        assert_eq!(p.deadline(), None);
    }

    #[test]
    fn activity_restarts_the_idle_window() {
        let t0 = Instant::now();
        let mut p = OutputPolicy::default();
        p.update(true, t0);
        p.update(false, t0 + Duration::from_secs(20));
        assert!(p.update(true, t0 + Duration::from_secs(29)));
        assert!(p.update(false, t0 + Duration::from_secs(40)));
        assert!(p.update(false, t0 + Duration::from_secs(69)));
        assert!(!p.update(false, t0 + Duration::from_secs(70)));
    }

    #[test]
    fn reset_closes_immediately() {
        let t0 = Instant::now();
        let mut p = OutputPolicy::default();
        p.update(true, t0);
        p.reset();
        assert!(!p.update(false, t0));
    }

    #[test]
    fn backoff_doubles_to_cap_and_resets() {
        let mut b = Backoff::default();
        let seq: Vec<u64> = (0..9).map(|_| b.fail().as_millis() as u64).collect();
        assert_eq!(seq, [200, 400, 800, 1600, 3200, 6400, 10_000, 10_000, 10_000]);
        b.reset();
        assert_eq!(b.fail(), Duration::from_millis(200));
    }
}
