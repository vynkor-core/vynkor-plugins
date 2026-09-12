//! Antiban: per-account rate limiting + FloodWait handling + circuit breaker.
//!
//! Every MTProto call is gated by [`Antiban::check`], which enforces a token
//! bucket (8 rps per account) and refuses to proceed once an account's circuit
//! breaker has tripped (3 consecutive FloodWaits). FloodWait errors are
//! absorbed by [`Antiban::on_flood_wait`], which sleeps a jittered fraction of
//! the server-mandated wait and arms the breaker.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{bail, Result};
use tokio::time::Instant;

/// Token bucket capacity per account (burst allowance).
const BUCKET_CAPACITY: f64 = 8.0;
/// Token refill rate, in tokens per second (≈8 rps per account).
const BUCKET_RATE: f64 = 8.0;
/// Consecutive FloodWaits that trip an account's circuit breaker.
const CIRCUIT_OPEN_THRESHOLD: u32 = 3;

/// Simple in-memory token bucket: fills at `rate` tokens/sec up to `capacity`.
struct TokenBucket {
    tokens: f64,
    capacity: f64,
    rate: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(capacity: f64, rate: f64) -> Self {
        Self::new_at(capacity, rate, Instant::now())
    }

    fn new_at(capacity: f64, rate: f64, now: Instant) -> Self {
        Self {
            tokens: capacity,
            capacity,
            rate,
            last_refill: now,
        }
    }

    /// Add tokens accrued since the last refill, capped at capacity.
    fn refill(&mut self, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
            self.last_refill = now;
        }
    }

    /// Consume one token at `now`. On success returns `Ok(())`; when the bucket
    /// is exhausted returns the estimated wait until the next token is ready.
    fn try_acquire_at(&mut self, now: Instant) -> Result<(), Duration> {
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            let missing = 1.0 - self.tokens;
            Err(Duration::from_secs_f64(missing / self.rate))
        }
    }
}

/// Per-account antiban state: a token bucket plus FloodWait streak tracking.
struct AccountState {
    bucket: TokenBucket,
    consecutive_flood_waits: u32,
    circuit_open: bool,
}

impl AccountState {
    fn new() -> Self {
        Self {
            bucket: TokenBucket::new(BUCKET_CAPACITY, BUCKET_RATE),
            consecutive_flood_waits: 0,
            circuit_open: false,
        }
    }
}

/// Per-account rate limiter and circuit breaker.
///
/// The internal [`Mutex`] is only ever held for a non-awaiting critical
/// section, so no guard ever crosses an `.await` point.
pub struct Antiban {
    state: Mutex<HashMap<String, AccountState>>,
}

impl Antiban {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, HashMap<String, AccountState>> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Gate a request for `account`. Blocks (async-sleeping) until a token is
    /// available; errors immediately if the account's circuit is open.
    pub async fn check(&self, account: &str) -> Result<()> {
        loop {
            let now = Instant::now();
            let wait = {
                let mut state = self.lock_state();
                let entry = state
                    .entry(account.to_string())
                    .or_insert_with(AccountState::new);
                if entry.circuit_open {
                    bail!("antiban: circuit open for account {account}");
                }
                match entry.bucket.try_acquire_at(now) {
                    Ok(()) => {
                        // A request got through → the FloodWait streak is broken.
                        entry.consecutive_flood_waits = 0;
                        return Ok(());
                    }
                    Err(wait) => wait,
                }
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// Record a FloodWait and sleep a jittered fraction of the mandated wait
    /// (0.2..0.8 × seconds) so co-accounts don't retry in lockstep. Three
    /// consecutive FloodWaits trip the circuit breaker.
    pub async fn on_flood_wait(&self, account: &str, seconds: u64) {
        let sleep = Duration::from_secs_f64(seconds as f64 * jitter_factor());
        tokio::time::sleep(sleep).await;

        let mut state = self.lock_state();
        let entry = state
            .entry(account.to_string())
            .or_insert_with(AccountState::new);
        entry.consecutive_flood_waits += 1;
        if entry.consecutive_flood_waits >= CIRCUIT_OPEN_THRESHOLD {
            entry.circuit_open = true;
        }
    }

    /// Whether the account's circuit breaker is currently open.
    pub fn is_circuit_open(&self, account: &str) -> bool {
        self.lock_state()
            .get(account)
            .map(|s| s.circuit_open)
            .unwrap_or(false)
    }
}

impl Default for Antiban {
    fn default() -> Self {
        Self::new()
    }
}

/// Uniform multiplier in `[0.2, 0.8)`. A tiny xorshift PRNG keeps the module
/// dependency-free; entropy is mixed in from the wall clock.
fn jitter_factor() -> f64 {
    0.2 + 0.6 * rand_f64()
}

fn rand_f64() -> f64 {
    static STATE: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
    let entropy = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut x = STATE.load(Ordering::Relaxed);
    x ^= x.wrapping_shl(13);
    x ^= x.wrapping_shr(7);
    x ^= x.wrapping_shl(17);
    x = x.wrapping_add(entropy);
    if x == 0 {
        x = 0x9E37_79B9_7F4A_7C15;
    }
    STATE.store(x, Ordering::Relaxed);
    (x as f64) / (u64::MAX as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_allows_up_to_capacity() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new_at(BUCKET_CAPACITY, BUCKET_RATE, start);
        for _ in 0..8 {
            assert!(
                bucket.try_acquire_at(start).is_ok(),
                "first 8 tokens should succeed"
            );
        }
        assert!(
            bucket.try_acquire_at(start).is_err(),
            "9th token should be refused"
        );
    }

    #[test]
    fn bucket_refills_at_rate() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new_at(BUCKET_CAPACITY, BUCKET_RATE, start);
        for _ in 0..8 {
            assert!(bucket.try_acquire_at(start).is_ok());
        }
        // At 8 tokens/s, 125ms refills exactly one token.
        let later = start + Duration::from_millis(125);
        assert!(
            bucket.try_acquire_at(later).is_ok(),
            "refilled token should succeed"
        );
        assert!(
            bucket.try_acquire_at(later).is_err(),
            "bucket should be empty again"
        );
    }

    #[test]
    fn bucket_refills_to_capacity_ceiling() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new_at(BUCKET_CAPACITY, BUCKET_RATE, start);
        for _ in 0..8 {
            assert!(bucket.try_acquire_at(start).is_ok());
        }
        // After a full second the bucket is back to capacity, never above it.
        let later = start + Duration::from_secs(1);
        for _ in 0..8 {
            assert!(bucket.try_acquire_at(later).is_ok());
        }
        assert!(bucket.try_acquire_at(later).is_err());
    }

    #[tokio::test]
    async fn circuit_opens_after_three_flood_waits() {
        let antiban = Antiban::new();
        assert!(!antiban.is_circuit_open("personal"));
        antiban.on_flood_wait("personal", 0).await;
        antiban.on_flood_wait("personal", 0).await;
        assert!(
            !antiban.is_circuit_open("personal"),
            "two waits should not trip it"
        );
        antiban.on_flood_wait("personal", 0).await;
        assert!(
            antiban.is_circuit_open("personal"),
            "three waits should open the circuit"
        );
    }

    #[tokio::test]
    async fn check_errors_when_circuit_open() {
        let antiban = Antiban::new();
        for _ in 0..3 {
            antiban.on_flood_wait("personal", 0).await;
        }
        assert!(antiban.check("personal").await.is_err());
    }
}
