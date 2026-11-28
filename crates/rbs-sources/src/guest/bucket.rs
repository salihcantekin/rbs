//! The incoming rate limit: 50 Mbit/s with a 2 s burst (ADR-0139 decision 11). A peer
//! above it is closed with `guest_limits`.

use rbs_core::time::Timestamp;

/// The guest rate: 50 Mbit/s.
pub const GUEST_RATE_BPS: u64 = 50_000_000;
/// The guest burst: 2 s at [`GUEST_RATE_BPS`].
pub const GUEST_BURST_BYTES: u64 = GUEST_RATE_BPS / 8 * 2;

const NANOS: u128 = 1_000_000_000;

/// A token bucket counted in bytes, refilled from the time passed in. Exact: the
/// part of a byte earned between calls is kept, so many small calls refill the same
/// as one large one.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    rate_bytes: u64,
    burst: u64,
    tokens: u64,
    /// Byte-nanoseconds earned but not yet a whole byte.
    carry: u128,
    last: Option<Timestamp>,
}

impl TokenBucket {
    /// A full bucket of `burst` bytes refilled at `rate_bps` bits per second.
    pub fn new(rate_bps: u64, burst: u64) -> Self {
        Self {
            rate_bytes: rate_bps / 8,
            burst,
            tokens: burst,
            carry: 0,
            last: None,
        }
    }

    /// The guest source's bucket: 50 Mbit/s, 2 s burst.
    pub fn guest() -> Self {
        Self::new(GUEST_RATE_BPS, GUEST_BURST_BYTES)
    }

    /// Takes `bytes` at `now`; false (and nothing taken) when the bucket holds fewer.
    /// A time before the last one refills nothing.
    pub fn take(&mut self, now: Timestamp, bytes: u64) -> bool {
        self.refill(now);
        if bytes > self.tokens {
            return false;
        }
        self.tokens -= bytes;
        true
    }

    /// Bytes the bucket holds now.
    pub fn tokens(&self) -> u64 {
        self.tokens
    }

    fn refill(&mut self, now: Timestamp) {
        let elapsed = match self.last {
            Some(last) => now.saturating_since(last).as_nanos(),
            None => 0,
        };
        if self.last.is_none_or(|last| now > last) {
            self.last = Some(now);
        }
        if self.tokens >= self.burst {
            self.carry = 0;
            return;
        }
        let earned = elapsed * u128::from(self.rate_bytes) + self.carry;
        let whole = earned / NANOS;
        self.carry = earned % NANOS;
        let room = u128::from(self.burst - self.tokens);
        if whole >= room {
            self.tokens = self.burst;
            self.carry = 0;
        } else {
            // `whole < room <= u64::MAX`, so it fits.
            self.tokens += u64::try_from(whole).unwrap_or(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn at(ms: u64) -> Timestamp {
        Timestamp::ZERO + Duration::from_millis(ms)
    }

    #[test]
    fn the_burst_passes_then_the_rate_holds() {
        let mut b = TokenBucket::guest();
        assert!(b.take(at(0), GUEST_BURST_BYTES), "the whole burst at once");
        assert!(!b.take(at(0), 1), "then nothing");
        // 10 ms at 50 Mbit/s is 62 500 bytes.
        assert!(!b.take(at(10), 62_501));
        assert!(b.take(at(10), 62_500));
        assert_eq!(b.tokens(), 0);
    }

    #[test]
    fn small_steps_refill_exactly_like_one_large_one() {
        let mut steps = TokenBucket::new(8_000, 1_000_000); // 1000 bytes/s
        let mut once = steps.clone();
        assert!(steps.take(at(0), 1_000_000));
        assert!(once.take(at(0), 1_000_000));
        for ms in 1..=1000 {
            steps.take(at(ms), 0);
        }
        once.take(at(1000), 0);
        assert_eq!(steps.tokens(), 1000);
        assert_eq!(once.tokens(), 1000);
    }

    #[test]
    fn it_never_holds_more_than_the_burst_and_ignores_time_going_back() {
        let mut b = TokenBucket::new(8_000, 500);
        assert!(b.take(at(0), 500));
        b.take(at(60_000), 0);
        assert_eq!(b.tokens(), 500);
        assert!(b.take(at(60_000), 500));
        b.take(at(1), 0);
        assert_eq!(b.tokens(), 0, "an earlier time refills nothing");
        b.take(at(60_500), 0);
        assert_eq!(b.tokens(), 500, "half a second from the latest time");
    }

    #[test]
    fn a_sustained_rate_above_the_limit_is_refused_within_the_burst_time() {
        // 60 Mbit/s as 7500-byte frames, 1000 a second: refused after about
        // burst / (60 - 50 Mbit/s) = 10 s.
        let mut b = TokenBucket::guest();
        let frame = 7_500_u64;
        let mut refused_at = None;
        for ms in 0..20_000 {
            if !b.take(at(ms), frame) {
                refused_at = Some(ms);
                break;
            }
        }
        let ms = refused_at.unwrap_or(u64::MAX);
        assert!((9_900..=10_100).contains(&ms), "refused at {ms} ms");
    }
}
