//! Per-client request limiting and byte-range parsing used by the server.
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Token bucket per key: `rate` requests refill per second, up to `burst` saved up.
pub struct RateLimiter {
    rate: f64,
    burst: f64,
    buckets: Mutex<HashMap<String, (f64, Instant)>>,
}

impl RateLimiter {
    /// `rate <= 0` disables limiting.
    pub fn new(rate: f64, burst: f64) -> Self {
        Self { rate, burst: burst.max(1.0), buckets: Mutex::new(HashMap::new()) }
    }
    pub fn enabled(&self) -> bool {
        self.rate > 0.0
    }
    /// Ok(()) if the request may go ahead, Err(wait) with how long until one is allowed.
    pub fn check_at(&self, key: &str, now: Instant) -> Result<(), Duration> {
        if !self.enabled() {
            return Ok(());
        }
        let mut b = self.buckets.lock().unwrap();
        if b.len() > 100_000 {
            b.retain(|_, (_, t)| now.duration_since(*t) < Duration::from_secs(600));
        }
        let (tokens, last) = b.entry(key.to_string()).or_insert((self.burst, now));
        let elapsed = now.saturating_duration_since(*last).as_secs_f64();
        *tokens = (*tokens + elapsed * self.rate).min(self.burst);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - *tokens) / self.rate))
        }
    }
    pub fn check(&self, key: &str) -> Result<(), Duration> {
        self.check_at(key, Instant::now())
    }
}

/// Parse a single `Range: bytes=a-b` / `bytes=a-` / `bytes=-n` against a body of `len`
/// bytes. Returns the inclusive (start, end), or Err(()) if unsatisfiable.
#[allow(clippy::result_unit_err)] // the only failure is "no answer"; callers map it to 416 / conflict
pub fn parse_range(h: &str, len: u64) -> Result<(u64, u64), ()> {
    let spec = h.trim().strip_prefix("bytes=").ok_or(())?;
    if spec.contains(',') {
        return Err(());
    }
    let (a, b) = spec.split_once('-').ok_or(())?;
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().map_err(|_| ())?;
            if n == 0 {
                return Err(());
            }
            (len.saturating_sub(n), len.checked_sub(1).ok_or(())?)
        }
        (s, "") => (s.parse().map_err(|_| ())?, len.checked_sub(1).ok_or(())?),
        (s, e) => (s.parse().map_err(|_| ())?, e.parse::<u64>().map_err(|_| ())?.min(len.saturating_sub(1))),
    };
    if start >= len || start > end {
        return Err(());
    }
    Ok((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bucket_allows_burst_then_blocks_then_refills() {
        let l = RateLimiter::new(2.0, 3.0);
        let t = Instant::now();
        for _ in 0..3 {
            assert!(l.check_at("a", t).is_ok());
        }
        let wait = l.check_at("a", t).unwrap_err();
        assert!(wait > Duration::from_millis(400) && wait <= Duration::from_millis(500));
        assert!(l.check_at("b", t).is_ok(), "keys are independent");
        assert!(l.check_at("a", t + Duration::from_millis(600)).is_ok(), "refilled");
    }
    #[test]
    fn zero_rate_disables() {
        let l = RateLimiter::new(0.0, 1.0);
        for _ in 0..1000 {
            assert!(l.check("a").is_ok());
        }
    }
    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-", 10), Ok((0, 9)));
        assert_eq!(parse_range("bytes=4-", 10), Ok((4, 9)));
        assert_eq!(parse_range("bytes=2-5", 10), Ok((2, 5)));
        assert_eq!(parse_range("bytes=2-99", 10), Ok((2, 9)));
        assert_eq!(parse_range("bytes=-3", 10), Ok((7, 9)));
        assert!(parse_range("bytes=10-", 10).is_err());
        assert!(parse_range("bytes=5-2", 10).is_err());
        assert!(parse_range("bytes=0-1,4-5", 10).is_err());
        assert!(parse_range("items=0-", 10).is_err());
    }
}
