//! Per-source failed-handshake throttle.
//!
//! Without it, anyone who can complete the transport handshake (anyone at all
//! for `tcp`, anyone holding the server public key for Noise NK) can guess
//! service tokens as fast as the network allows, and every failure logs a line.
//! After `limit` failures within `window`, connections from that source are
//! dropped before the transport handshake until `ban` has passed.
//!
//! IPv6 sources are grouped by /64, because one host usually controls a whole
//! /64 and could otherwise rotate addresses to get unlimited tries.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Entries beyond this are pruned of expired ones, bounding memory under a flood
const MAX_TRACKED: usize = 65536;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceKey(IpAddr);

impl SourceKey {
    pub fn from_ip(ip: IpAddr) -> SourceKey {
        match ip {
            IpAddr::V4(_) => SourceKey(ip),
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => SourceKey(IpAddr::V4(v4)),
                None => {
                    let mut o = v6.octets();
                    o[8..].fill(0);
                    SourceKey(IpAddr::V6(o.into()))
                }
            },
        }
    }
}

impl std::fmt::Display for SourceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            IpAddr::V4(v4) => write!(f, "{}", v4),
            IpAddr::V6(v6) => write!(f, "{}/64", v6),
        }
    }
}

struct Entry {
    failures: u32,
    window_start: Instant,
    banned_until: Option<Instant>,
}

pub struct AuthThrottle {
    limit: u32,
    window: Duration,
    ban: Duration,
    entries: Mutex<HashMap<SourceKey, Entry>>,
}

/// What `record_failure` did
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// Counted; the source may keep trying
    Counted(u32),
    /// This failure reached the limit; the source is now banned
    Banned,
    /// The source was already banned (a connection accepted before the ban)
    AlreadyBanned,
}

impl AuthThrottle {
    /// `limit == 0` disables throttling.
    pub fn new(limit: u32, window: Duration, ban: Duration) -> AuthThrottle {
        AuthThrottle {
            limit,
            window,
            ban,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn ban_duration(&self) -> Duration {
        self.ban
    }

    pub fn is_blocked(&self, ip: IpAddr) -> bool {
        self.is_blocked_at(SourceKey::from_ip(ip), Instant::now())
    }

    pub fn record_failure(&self, ip: IpAddr) -> Failure {
        self.record_failure_at(SourceKey::from_ip(ip), Instant::now())
    }

    pub fn record_success(&self, ip: IpAddr) {
        if self.limit == 0 {
            return;
        }
        self.entries.lock().unwrap().remove(&SourceKey::from_ip(ip));
    }

    fn is_blocked_at(&self, key: SourceKey, now: Instant) -> bool {
        if self.limit == 0 {
            return false;
        }
        let mut entries = self.entries.lock().unwrap();
        match entries.get(&key).and_then(|e| e.banned_until) {
            Some(until) if now < until => true,
            Some(_) => {
                entries.remove(&key);
                false
            }
            None => false,
        }
    }

    fn record_failure_at(&self, key: SourceKey, now: Instant) -> Failure {
        if self.limit == 0 {
            return Failure::Counted(0);
        }
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= MAX_TRACKED && !entries.contains_key(&key) {
            let window = self.window;
            entries.retain(|_, e| match e.banned_until {
                Some(until) => now < until,
                None => now.duration_since(e.window_start) < window,
            });
        }
        let e = entries.entry(key).or_insert(Entry {
            failures: 0,
            window_start: now,
            banned_until: None,
        });
        if e.banned_until.is_some() {
            return Failure::AlreadyBanned;
        }
        if now.duration_since(e.window_start) >= self.window {
            e.failures = 0;
            e.window_start = now;
        }
        e.failures += 1;
        if e.failures >= self.limit {
            e.banned_until = Some(now + self.ban);
            Failure::Banned
        } else {
            Failure::Counted(e.failures)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn throttle(limit: u32) -> AuthThrottle {
        AuthThrottle::new(limit, Duration::from_secs(60), Duration::from_secs(300))
    }

    fn key(s: &str) -> SourceKey {
        SourceKey::from_ip(s.parse().unwrap())
    }

    #[test]
    fn bans_after_limit_and_expires() {
        let t = throttle(3);
        let k = key("192.0.2.1");
        let now = Instant::now();
        assert_eq!(t.record_failure_at(k, now), Failure::Counted(1));
        assert_eq!(t.record_failure_at(k, now), Failure::Counted(2));
        assert!(!t.is_blocked_at(k, now));
        assert_eq!(t.record_failure_at(k, now), Failure::Banned);
        assert!(t.is_blocked_at(k, now + Duration::from_secs(299)));
        assert!(!t.is_blocked_at(k, now + Duration::from_secs(300)));
        // the ban is cleared on expiry, so counting starts over
        let later = now + Duration::from_secs(301);
        assert_eq!(t.record_failure_at(k, later), Failure::Counted(1));
    }

    #[test]
    fn window_resets_count() {
        let t = throttle(3);
        let k = key("192.0.2.1");
        let now = Instant::now();
        t.record_failure_at(k, now);
        t.record_failure_at(k, now);
        let later = now + Duration::from_secs(61);
        assert_eq!(t.record_failure_at(k, later), Failure::Counted(1));
    }

    #[test]
    fn success_clears() {
        let t = throttle(2);
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        t.record_failure(ip);
        t.record_success(ip);
        assert_eq!(t.record_failure(ip), Failure::Counted(1));
    }

    #[test]
    fn ipv6_grouped_by_64_and_mapped_v4_normalized() {
        assert_eq!(key("2001:db8:1:2::1"), key("2001:db8:1:2:ffff::9"));
        assert_ne!(key("2001:db8:1:2::1"), key("2001:db8:1:3::1"));
        assert_eq!(key("::ffff:192.0.2.1"), key("192.0.2.1"));
        assert_eq!(key("2001:db8:1:2::1").to_string(), "2001:db8:1:2::/64");
    }

    #[test]
    fn zero_limit_disables() {
        let t = throttle(0);
        let k = key("192.0.2.1");
        let now = Instant::now();
        for _ in 0..100 {
            t.record_failure_at(k, now);
        }
        assert!(!t.is_blocked_at(k, now));
    }
}
