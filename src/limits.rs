//! Bandwidth and rate limits (spec FR-4.35). Over a bandwidth limit packets are dropped, and the
//! inner QUIC connection slows down by itself; over a count limit the caller refuses.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::Limits;

const WINDOW: Duration = Duration::from_secs(60);
/// How long a source that failed too many handshakes stays blocked.
const BLOCK: Duration = Duration::from_secs(600);

/// Bytes per second from Mbit/s.
pub fn mbit_to_bytes(mbit: u32) -> f64 {
    f64::from(mbit) * 125_000.0
}

/// Bytes per second from kbit/s.
pub fn kbit_to_bytes(kbit: u32) -> f64 {
    f64::from(kbit) * 125.0
}

/// Classic token bucket: `rate` bytes per second, bursts up to 100 ms worth of traffic (at least
/// 64 KiB, so a single key frame of a screen share is not cut).
pub struct TokenBucket {
    rate: f64,
    capacity: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(bytes_per_s: f64) -> Self {
        let capacity = (bytes_per_s / 10.0).max(65_536.0);
        Self {
            rate: bytes_per_s,
            capacity,
            tokens: capacity,
            last: Instant::now(),
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
    }

    /// Takes `bytes` if available; false means the packet must be dropped.
    pub fn take(&mut self, bytes: usize, now: Instant) -> bool {
        self.refill(now);
        let bytes = bytes as f64;
        if self.tokens >= bytes {
            self.tokens -= bytes;
            true
        } else {
            false
        }
    }
}

impl TokenBucket {
    /// For whole broadcast units, which can be larger than the burst (a keyframe of several
    /// MiB): a unit passes while the bucket is not empty and may leave it in debt. `Err` is the
    /// time until the debt is paid off; the caller either waits that long or drops the unit.
    pub fn spend(&mut self, bytes: usize, now: Instant) -> Result<(), Duration> {
        self.refill(now);
        self.check_spend()?;
        self.tokens -= bytes as f64;
        Ok(())
    }

    fn check_spend(&self) -> Result<(), Duration> {
        if self.tokens > 0.0 {
            Ok(())
        } else {
            Err(Duration::from_secs_f64(-self.tokens / self.rate + 0.001))
        }
    }

    /// The lowest priority (plugin data, spec 09 FR-9.85): `bytes` pass only while half the burst
    /// stays in the bucket for everything else. A unit larger than that waits for a full bucket,
    /// which only an otherwise idle relay has.
    fn check_spare(&self, bytes: usize) -> Result<(), Duration> {
        let need = (bytes as f64 + self.capacity / 2.0).min(self.capacity);
        if self.tokens >= need {
            Ok(())
        } else {
            Err(Duration::from_secs_f64(
                (need - self.tokens) / self.rate + 0.001,
            ))
        }
    }
}

/// The relay-wide budgets: `total` counts every forwarded byte, `plugin_data` the share of it
/// plugin data may use (`plugin_data.max_share_percent` of `total_mbit`).
pub struct Bandwidth {
    pub total: Mutex<TokenBucket>,
    plugin_data: Mutex<TokenBucket>,
}

impl Bandwidth {
    pub fn new(total_mbit: u32, plugin_share_percent: u32) -> Self {
        let total = mbit_to_bytes(total_mbit);
        Self {
            total: Mutex::new(TokenBucket::new(total)),
            plugin_data: Mutex::new(TokenBucket::new(
                total * f64::from(plugin_share_percent) / 100.0,
            )),
        }
    }

    /// Plugin data passes within its share and only from spare total bandwidth, so voice and
    /// media are never held up by it. `Err` is the time until it may try again; nothing is taken
    /// then.
    pub fn spend_plugin_data(&self, bytes: usize, now: Instant) -> Result<(), Duration> {
        let mut total = lock(&self.total);
        let mut share = lock(&self.plugin_data);
        total.refill(now);
        share.refill(now);
        total.check_spare(bytes)?;
        share.check_spend()?;
        total.tokens -= bytes as f64;
        share.tokens -= bytes as f64;
        Ok(())
    }
}

/// Counts events in fixed one-minute windows.
#[derive(Clone, Copy)]
struct Window {
    start: Instant,
    count: u32,
}

impl Window {
    fn new(now: Instant) -> Self {
        Self {
            start: now,
            count: 0,
        }
    }

    /// Adds one event and returns the count in the current window.
    fn hit(&mut self, now: Instant) -> u32 {
        if now.saturating_duration_since(self.start) >= WINDOW {
            *self = Self::new(now);
        }
        self.count = self.count.saturating_add(1);
        self.count
    }
}

/// Global limit on pair creation.
pub struct PairRate {
    max: u32,
    window: Mutex<Window>,
}

impl PairRate {
    pub fn new(max: u32) -> Self {
        Self {
            max,
            window: Mutex::new(Window::new(Instant::now())),
        }
    }

    pub fn allow(&self) -> bool {
        lock(&self.window).hit(Instant::now()) <= self.max
    }
}

struct Source {
    conns: Window,
    failed: Window,
    blocked_until: Option<Instant>,
}

/// Per-source limits: new connections per minute and failed handshakes per minute (then a
/// block). IPv6 sources are counted per /64, because one host can use a whole /64.
pub struct SourceLimits {
    conn_per_min: u32,
    failed_per_min: u32,
    sources: Mutex<HashMap<IpAddr, Source>>,
}

impl SourceLimits {
    pub fn new(limits: &Limits) -> Self {
        Self {
            conn_per_min: limits.conn_per_ip_per_min,
            failed_per_min: limits.failed_per_ip_per_min,
            sources: Mutex::new(HashMap::new()),
        }
    }

    /// Whether a new relay connection from `ip` may start its handshake.
    pub fn admit(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut sources = lock(&self.sources);
        let source = sources.entry(source_key(ip)).or_insert_with(|| Source {
            conns: Window::new(now),
            failed: Window::new(now),
            blocked_until: None,
        });
        if source.blocked_until.is_some_and(|until| now < until) {
            return false;
        }
        let count = source.conns.hit(now);
        if count == self.conn_per_min + 1 {
            tracing::warn!(
                "too many new connections from {}; refusing for the rest of the minute",
                log_prefix(ip)
            );
        }
        count <= self.conn_per_min
    }

    /// Whether `ip` is blocked for failed handshakes (API callers are only checked for this).
    pub fn blocked(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        lock(&self.sources)
            .get(&source_key(ip))
            .and_then(|source| source.blocked_until)
            .is_some_and(|until| now < until)
    }

    /// Records a failed handshake or API authentication.
    pub fn failed(&self, ip: IpAddr) {
        let now = Instant::now();
        let mut sources = lock(&self.sources);
        let source = sources.entry(source_key(ip)).or_insert_with(|| Source {
            conns: Window::new(now),
            failed: Window::new(now),
            blocked_until: None,
        });
        if source.failed.hit(now) > self.failed_per_min && source.blocked_until.is_none() {
            source.blocked_until = Some(now + BLOCK);
            tracing::warn!(
                "too many failed handshakes from {}; blocked for 10 minutes",
                log_prefix(ip)
            );
        }
    }

    /// Forgets sources with nothing left to remember, so the table stays small.
    pub fn sweep(&self) {
        let now = Instant::now();
        lock(&self.sources).retain(|_, source| {
            if let Some(until) = source.blocked_until {
                if now < until {
                    return true;
                }
                source.blocked_until = None;
            }
            now.saturating_duration_since(source.conns.start) < WINDOW
                || now.saturating_duration_since(source.failed.start) < WINDOW
        });
    }
}

fn source_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => {
            let mut octets = v6.octets();
            octets[8..].fill(0);
            IpAddr::from(octets)
        }
        v4 => v4,
    }
}

/// The source network for logs: /24 for IPv4, /48 for IPv6. Full addresses are never logged.
pub fn log_prefix(ip: IpAddr) -> String {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            format!("{a}.{b}.{c}.0/24")
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}::/48", s[0], s[1], s[2])
        }
    }
}

/// A poisoned lock only means another task panicked while holding it; the counters are still
/// usable, and a relay must not stop forwarding because of it.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(conn: u32, failed: u32) -> Limits {
        Limits {
            conn_per_ip_per_min: conn,
            failed_per_ip_per_min: failed,
            ..Limits::default()
        }
    }

    #[test]
    fn plugin_data_gets_only_spare_bandwidth_within_its_share() {
        // 8 Mbit/s: 1 MB/s with a 100 KB burst; plugin data may use a quarter of it.
        let bandwidth = Bandwidth::new(8, 25);
        let start = Instant::now();
        // Half the burst is kept for other traffic.
        assert!(bandwidth.spend_plugin_data(40_000, start).is_ok());
        assert!(bandwidth.spend_plugin_data(20_000, start).is_err());
        assert!(lock(&bandwidth.total).take(60_000, start));
        // Media using the total leaves plugin data nothing, however long it waits for its turn.
        let mut now = start;
        for _ in 0..100 {
            now += Duration::from_millis(10);
            assert!(lock(&bandwidth.total).take(10_000, now));
            assert!(bandwidth.spend_plugin_data(1_000, now).is_err());
        }
        // An idle relay lets it through again, but never past its 250 KB/s share.
        now += Duration::from_secs(1);
        let mut passed = 0;
        for _ in 0..100 {
            now += Duration::from_millis(10);
            while bandwidth.spend_plugin_data(1_000, now).is_ok() {
                passed += 1_000;
            }
        }
        assert!(
            (240_000..=360_000).contains(&passed),
            "{passed} bytes in 1 s"
        );
    }

    #[test]
    fn bucket_drops_over_rate_and_refills() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(100_000.0);
        assert!(bucket.take(65_536, start));
        assert!(!bucket.take(1_000, start));
        assert!(bucket.take(1_000, start + Duration::from_millis(20)));
    }

    #[test]
    fn spend_lets_big_units_pass_into_debt() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(100_000.0);
        assert_eq!(bucket.spend(1_000_000, start), Ok(()));
        let wait = bucket.spend(1, start).unwrap_err();
        assert!(wait > Duration::from_secs(9) && wait < Duration::from_secs(10));
        assert_eq!(bucket.spend(1, start + wait), Ok(()));
    }

    #[test]
    fn connections_per_minute() {
        let sources = SourceLimits::new(&limits(2, 10));
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        assert!(sources.admit(ip));
        assert!(sources.admit(ip));
        assert!(!sources.admit(ip));
        assert!(sources.admit("192.0.2.2".parse().unwrap()));
    }

    #[test]
    fn failures_block_the_source() {
        let sources = SourceLimits::new(&limits(100, 2));
        let ip: IpAddr = "2001:db8::1".parse().unwrap();
        sources.failed(ip);
        sources.failed(ip);
        assert!(!sources.blocked(ip));
        sources.failed(ip);
        assert!(sources.blocked(ip));
        // The same /64 is the same source.
        assert!(!sources.admit("2001:db8::2".parse().unwrap()));
        sources.sweep();
        assert!(sources.blocked(ip));
    }

    #[test]
    fn pair_rate() {
        let rate = PairRate::new(2);
        assert!(rate.allow() && rate.allow());
        assert!(!rate.allow());
    }

    #[test]
    fn log_prefixes_hide_the_host() {
        assert_eq!(
            log_prefix("203.0.113.77".parse().unwrap()),
            "203.0.113.0/24"
        );
        assert_eq!(
            log_prefix("2001:db8:1:2::5".parse().unwrap()),
            "2001:db8:1::/48"
        );
        assert_eq!(
            log_prefix("::ffff:203.0.113.77".parse().unwrap()),
            "203.0.113.0/24"
        );
    }
}
