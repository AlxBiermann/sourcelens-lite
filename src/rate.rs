//! Token-bucket request limits per key (API key id or user id).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::state::lock;

/// Requests per second a key may sustain.
const RATE_PER_SECOND: f64 = 10.0;
/// Requests a key may send at once after being idle.
const BURST: f64 = 20.0;
/// Buckets unused for this long are dropped (a full bucket is the same as no bucket).
const IDLE_TTL: Duration = Duration::from_secs(10 * 60);
/// How often idle buckets are looked for.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

pub struct RateLimiter {
    inner: Mutex<Inner>,
}

struct Inner {
    buckets: HashMap<i64, Bucket>,
    last_sweep: Instant,
}

struct Bucket {
    tokens: f64,
    updated: Instant,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                buckets: HashMap::new(),
                last_sweep: Instant::now(),
            }),
        }
    }
}

impl RateLimiter {
    /// Takes one request from `key`'s budget; `Err(seconds)` says when the next request is allowed.
    pub fn check(&self, key: i64) -> Result<(), u64> {
        self.check_at(key, Instant::now())
    }

    fn check_at(&self, key: i64, now: Instant) -> Result<(), u64> {
        let mut inner = lock(&self.inner);
        if now.saturating_duration_since(inner.last_sweep) >= SWEEP_INTERVAL {
            inner
                .buckets
                .retain(|_, bucket| now.saturating_duration_since(bucket.updated) < IDLE_TTL);
            inner.last_sweep = now;
        }
        let bucket = inner.buckets.entry(key).or_insert(Bucket {
            tokens: BURST,
            updated: now,
        });
        let elapsed = now.saturating_duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * RATE_PER_SECOND).min(BURST);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return Ok(());
        }
        let wait = (1.0 - bucket.tokens) / RATE_PER_SECOND;
        // At most a few seconds, so the cast cannot truncate.
        Err((wait.ceil() as u64).max(1))
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        lock(&self.inner).buckets.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_a_burst_then_limits() {
        let limiter = RateLimiter::default();
        let start = Instant::now();
        for _ in 0..20 {
            assert_eq!(limiter.check_at(1, start), Ok(()));
        }
        assert_eq!(limiter.check_at(1, start), Err(1));
        assert_eq!(
            limiter.check_at(2, start),
            Ok(()),
            "keys have separate budgets"
        );
    }

    #[test]
    fn refills_at_ten_per_second() {
        let limiter = RateLimiter::default();
        let start = Instant::now();
        for _ in 0..20 {
            limiter.check_at(1, start).unwrap();
        }
        let later = start + Duration::from_millis(500);
        for _ in 0..5 {
            assert_eq!(limiter.check_at(1, later), Ok(()));
        }
        assert!(limiter.check_at(1, later).is_err());
        let full = later + Duration::from_secs(10);
        for _ in 0..20 {
            assert_eq!(limiter.check_at(1, full), Ok(()));
        }
        assert!(
            limiter.check_at(1, full).is_err(),
            "the bucket never holds more than the burst"
        );
    }

    #[test]
    fn retry_after_rounds_up_to_whole_seconds() {
        let limiter = RateLimiter::default();
        let start = Instant::now();
        for _ in 0..20 {
            limiter.check_at(1, start).unwrap();
        }
        assert_eq!(limiter.check_at(1, start), Err(1));
        assert_eq!(
            limiter.check_at(1, start + Duration::from_millis(100)),
            Ok(())
        );
    }

    #[test]
    fn idle_buckets_are_dropped() {
        let limiter = RateLimiter::default();
        let start = Instant::now();
        limiter.check_at(1, start).unwrap();
        limiter
            .check_at(2, start + Duration::from_secs(9 * 60))
            .unwrap();
        limiter
            .check_at(3, start + Duration::from_secs(11 * 60))
            .unwrap();
        assert_eq!(limiter.tracked(), 2);
    }
}
