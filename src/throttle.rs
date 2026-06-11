//! Per-IP failure throttling for the two password-guessing surfaces (the web
//! login form and the WebDAV Basic auth gate).
//!
//! The threat is a brute-force loop on a hostile LAN: the server binds
//! `0.0.0.0` by default and the default password is a short code read off the
//! device screen, so unthrottled guessing is the real risk (issue #27 / review
//! §1.7). A few free attempts absorb honest typos; after that each failure
//! doubles the wait before the next attempt is even considered, capped so a
//! locked-out user is never more than [`MAX_DELAY`] away from a retry.
//!
//! Memory is strictly bounded — this runs on 1 GB handhelds. The map holds at
//! most [`MAX_ENTRIES`] IPs: when full, idle entries are pruned, and if a
//! hostile LAN keeps it full anyway, the stalest entry is evicted.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::{
    extract::{ConnectInfo, FromRequestParts},
    http::{header, request::Parts, StatusCode},
    response::{IntoResponse, Response},
};

/// Failures allowed before any delay kicks in (honest typos of the screen code).
const FREE_FAILURES: u32 = 3;
/// Delay after the first throttled failure; doubles per further failure.
const BASE_DELAY: Duration = Duration::from_secs(1);
/// Hard cap on the per-attempt delay (~6 more failures after the free ones).
const MAX_DELAY: Duration = Duration::from_secs(30);
/// Entries idle this long are dropped when the map needs room.
const IDLE_EXPIRY: Duration = Duration::from_secs(15 * 60);
/// Hard cap on tracked IPs — bounds memory on a hostile LAN.
const MAX_ENTRIES: usize = 1024;

struct Entry {
    failures: u32,
    last_failure: Instant,
}

/// Shared per-IP auth-failure tracker. All methods take `now` explicitly so
/// tests can drive time without sleeping; production passes `Instant::now()`.
#[derive(Default)]
pub struct Throttle {
    inner: Mutex<HashMap<IpAddr, Entry>>,
}

impl Throttle {
    pub fn new() -> Throttle {
        Throttle::default()
    }

    /// If `ip` must still wait before its next attempt, the remaining wait.
    /// `None` means the attempt may proceed.
    pub fn retry_after(&self, ip: IpAddr, now: Instant) -> Option<Duration> {
        let map = self.inner.lock().expect("throttle lock");
        let entry = map.get(&ip)?;
        let ready = entry.last_failure + penalty(entry.failures)?;
        ready.checked_duration_since(now).filter(|d| !d.is_zero())
    }

    /// Record a failed attempt (wrong password presented) from `ip`.
    pub fn record_failure(&self, ip: IpAddr, now: Instant) {
        let mut map = self.inner.lock().expect("throttle lock");

        // Keep the map bounded: drop idle entries first; if a hostile LAN
        // keeps it at the cap anyway, evict the stalest tracked IP.
        if map.len() >= MAX_ENTRIES && !map.contains_key(&ip) {
            map.retain(|_, e| now.duration_since(e.last_failure) < IDLE_EXPIRY);
            if map.len() >= MAX_ENTRIES {
                if let Some(stalest) = map
                    .iter()
                    .min_by_key(|(_, e)| e.last_failure)
                    .map(|(k, _)| *k)
                {
                    map.remove(&stalest);
                }
            }
        }

        let entry = map.entry(ip).or_insert(Entry {
            failures: 0,
            last_failure: now,
        });
        entry.failures = entry.failures.saturating_add(1);
        entry.last_failure = now;
    }

    /// Forget `ip` after a successful authentication.
    pub fn record_success(&self, ip: IpAddr) {
        self.inner.lock().expect("throttle lock").remove(&ip);
    }
}

/// Delay imposed after `failures` consecutive failures: none within the free
/// budget, then 1s doubling per failure, capped at [`MAX_DELAY`].
fn penalty(failures: u32) -> Option<Duration> {
    let over = failures.checked_sub(FREE_FAILURES)?;
    // 2^over saturates well past the cap; 30 bits is already > MAX_DELAY.
    Some((BASE_DELAY * 2u32.saturating_pow(over.min(30))).min(MAX_DELAY))
}

/// The throttling key: the TCP peer address. The server serves connections
/// directly (no reverse proxy in the normal deployment), so the socket peer is
/// the truth — `X-Forwarded-For` is deliberately NOT consulted, since any
/// client could spoof it to dodge the throttle. `None` only happens off the
/// network path (router tests driving the service via `oneshot`); those fall
/// back to a shared sentinel so throttling still applies rather than being
/// silently disabled.
pub fn client_ip(peer: Option<SocketAddr>) -> IpAddr {
    peer.map(|a| a.ip()).unwrap_or(IpAddr::from([0, 0, 0, 0]))
}

/// Infallible extractor form of [`client_ip`] for handlers that use axum's
/// extractor signature (a bare `Option<ConnectInfo<_>>` is not an extractor
/// in axum 0.8).
pub struct ClientIp(pub IpAddr);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> Result<Self, std::convert::Infallible> {
        Ok(ClientIp(client_ip(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ci| ci.0),
        )))
    }
}

/// The throttled response: `429 Too Many Requests` with a `Retry-After`
/// header (whole seconds, rounded up).
pub fn too_many_attempts(wait: Duration) -> Response {
    let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, secs.to_string())],
        "too many failed attempts; try again later\n",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(last: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, last])
    }

    #[test]
    fn free_failures_then_exponential_capped_delay() {
        let t = Throttle::new();
        let now = Instant::now();

        // The free budget: no delay while honest typos are plausible.
        for _ in 0..FREE_FAILURES {
            assert_eq!(t.retry_after(ip(1), now), None);
            t.record_failure(ip(1), now);
        }

        // Past the budget the delay doubles each failure: 1s, 2s, 4s, …
        let mut expected = BASE_DELAY;
        for _ in 0..10 {
            assert_eq!(t.retry_after(ip(1), now), Some(expected));
            t.record_failure(ip(1), now);
            expected = (expected * 2).min(MAX_DELAY);
        }
        // …and never exceeds the cap.
        assert_eq!(t.retry_after(ip(1), now), Some(MAX_DELAY));
    }

    #[test]
    fn delay_expires_as_time_passes() {
        let t = Throttle::new();
        let now = Instant::now();
        for _ in 0..=FREE_FAILURES {
            t.record_failure(ip(1), now);
        }
        // Four failures → a 2s penalty; the remaining wait shrinks with time.
        let later = now + Duration::from_millis(500);
        assert_eq!(
            t.retry_after(ip(1), later),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(
            t.retry_after(ip(1), later + Duration::from_millis(250)),
            Some(Duration::from_millis(1250))
        );
        // Once the penalty has elapsed, attempts may proceed again.
        assert_eq!(t.retry_after(ip(1), later + Duration::from_secs(2)), None);
    }

    #[test]
    fn success_resets_the_counter() {
        let t = Throttle::new();
        let now = Instant::now();
        for _ in 0..=FREE_FAILURES {
            t.record_failure(ip(1), now);
        }
        assert!(t.retry_after(ip(1), now).is_some());

        t.record_success(ip(1));
        assert_eq!(t.retry_after(ip(1), now), None);
        // The free budget is restored too.
        t.record_failure(ip(1), now);
        assert_eq!(t.retry_after(ip(1), now), None);
    }

    #[test]
    fn ips_are_throttled_independently() {
        let t = Throttle::new();
        let now = Instant::now();
        for _ in 0..=FREE_FAILURES {
            t.record_failure(ip(1), now);
        }
        assert!(t.retry_after(ip(1), now).is_some());
        assert_eq!(t.retry_after(ip(2), now), None);
    }

    // The map must never exceed MAX_ENTRIES no matter how many distinct
    // source IPs fail — this is the memory bound for a 1 GB device.
    #[test]
    fn map_stays_bounded_and_prunes_idle_entries() {
        let t = Throttle::new();
        let start = Instant::now();

        // Fill the map to the cap from distinct IPv6 sources.
        for i in 0..MAX_ENTRIES as u32 {
            let ip = IpAddr::from([0, 0, 0, 0, 0, 0, 0x1000 + (i / 0x10000) as u16, i as u16]);
            t.record_failure(ip, start);
        }
        assert_eq!(t.inner.lock().unwrap().len(), MAX_ENTRIES);

        // A new IP while full and fresh: the stalest entry is evicted, the
        // cap holds.
        t.record_failure(ip(1), start + Duration::from_secs(1));
        assert_eq!(t.inner.lock().unwrap().len(), MAX_ENTRIES);

        // A new IP once the old entries have idled out: they are pruned.
        t.record_failure(ip(2), start + IDLE_EXPIRY + Duration::from_secs(2));
        assert!(t.inner.lock().unwrap().len() < MAX_ENTRIES);
    }
}
