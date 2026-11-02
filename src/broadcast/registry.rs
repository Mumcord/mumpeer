//! The broadcasts the bot created, their publisher and viewer tokens, and their limits.
//!
//! Tokens are kept only as SHA-256 hashes, as for pairs. A broadcast or a viewer that is removed
//! signals its `end` watch, so a bound link can close (and, for a viewer, tell the publisher).

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::config::Limits;
use crate::limits::lock;
use crate::{base64url, random, sha256};

/// A broadcast id: a random u64, written as 16 lowercase hex digits (spec 07 §6.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BroadcastId(pub u64);

impl BroadcastId {
    /// Accepts exactly 16 hex digits in either case.
    pub fn parse(text: &str) -> Option<Self> {
        if text.len() != 16 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u64::from_str_radix(text, 16).ok().map(Self)
    }
}

impl fmt::Display for BroadcastId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// Why a broadcast or one of its viewers was removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcastEnd {
    /// The bot deleted the broadcast or the viewer.
    Deleted,
    /// The bot dropped the certificate hash (`DELETE /v1/peers/{hash}`), or re-admitted the
    /// viewer with a new token.
    Revoked,
    /// The token ran out before its side connected.
    Expired,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CreateError {
    /// `max_broadcasts` broadcasts exist already (or broadcasts are off).
    RelayFull,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AdmitError {
    UnknownBroadcast,
    /// The viewer is the publisher itself.
    Publisher,
    BroadcastFull,
}

/// One admitted viewer. `bound` is set once its subscriber link is bound.
struct Viewer {
    #[expect(dead_code, reason = "the subscriber bind (next step) looks tokens up")]
    token: [u8; 32],
    expires_at: Instant,
    bound: bool,
    end: watch::Sender<Option<BroadcastEnd>>,
}

impl Viewer {
    fn waiting_expired(&self, now: Instant) -> bool {
        !self.bound && now >= self.expires_at
    }

    fn remove(&self, reason: BroadcastEnd) {
        self.end.send_replace(Some(reason));
    }
}

/// One broadcast. `publisher_bound` is set once the publisher link is bound.
struct Broadcast {
    publisher: String,
    #[expect(dead_code, reason = "the publisher bind (next step) looks tokens up")]
    publisher_token: [u8; 32],
    expires_at: Instant,
    publisher_bound: bool,
    max_viewers: u16,
    viewers: HashMap<String, Viewer>,
    end: watch::Sender<Option<BroadcastEnd>>,
}

impl Broadcast {
    fn has(&self, hash: &str) -> bool {
        self.publisher == hash || self.viewers.contains_key(hash)
    }

    fn end(&self, reason: BroadcastEnd) {
        for viewer in self.viewers.values() {
            viewer.remove(reason);
        }
        self.end.send_replace(Some(reason));
    }
}

pub struct Registry {
    max_broadcasts: usize,
    max_viewers: u16,
    token_ttl: Duration,
    broadcasts: Mutex<HashMap<BroadcastId, Broadcast>>,
}

impl Registry {
    pub fn new(limits: &Limits) -> Self {
        Self {
            max_broadcasts: limits.max_broadcasts as usize,
            // The config validates the range; the fallback is never used.
            max_viewers: u16::try_from(limits.max_viewers_per_broadcast).unwrap_or(u16::MAX),
            token_ttl: Duration::from_secs(limits.token_ttl_s.into()),
            broadcasts: Mutex::new(HashMap::new()),
        }
    }

    /// Viewers per broadcast as announced to clients; `None` when broadcasts are off.
    pub fn max_viewers(&self) -> Option<u16> {
        (self.max_broadcasts > 0).then_some(self.max_viewers)
    }

    /// Broadcasts that exist right now.
    pub fn count(&self) -> usize {
        lock(&self.broadcasts).len()
    }

    /// A new broadcast for `publisher` (a validated certificate hash) with at most `max_viewers`
    /// viewers, capped by the relay's own limit. Returns its id and the publisher's token.
    pub fn create(
        &self,
        publisher: String,
        max_viewers: u16,
    ) -> Result<(BroadcastId, String), CreateError> {
        let mut broadcasts = lock(&self.broadcasts);
        if broadcasts.len() >= self.max_broadcasts {
            return Err(CreateError::RelayFull);
        }
        let id = loop {
            let id = BroadcastId(u64::from_be_bytes(random::<8>()));
            if !broadcasts.contains_key(&id) {
                break id;
            }
        };
        let token = base64url(&random::<32>());
        broadcasts.insert(
            id,
            Broadcast {
                publisher,
                publisher_token: sha256(token.as_bytes()),
                expires_at: Instant::now() + self.token_ttl,
                publisher_bound: false,
                max_viewers: max_viewers.min(self.max_viewers),
                viewers: HashMap::new(),
                end: watch::channel(None).0,
            },
        );
        Ok((id, token))
    }

    /// Admits `viewer` (a validated certificate hash) and returns its token. Admitting the same
    /// viewer again replaces its token and removes its earlier link.
    pub fn admit(&self, id: BroadcastId, viewer: String) -> Result<String, AdmitError> {
        let now = Instant::now();
        let mut broadcasts = lock(&self.broadcasts);
        let broadcast = broadcasts
            .get_mut(&id)
            .ok_or(AdmitError::UnknownBroadcast)?;
        if broadcast.publisher == viewer {
            return Err(AdmitError::Publisher);
        }
        // Tokens that ran out unused must not keep a seat.
        broadcast.viewers.retain(|_, v| {
            let expired = v.waiting_expired(now);
            if expired {
                v.remove(BroadcastEnd::Expired);
            }
            !expired
        });
        let others = broadcast.viewers.len() - usize::from(broadcast.viewers.contains_key(&viewer));
        if others >= usize::from(broadcast.max_viewers) {
            return Err(AdmitError::BroadcastFull);
        }
        if let Some(old) = broadcast.viewers.remove(&viewer) {
            old.remove(BroadcastEnd::Revoked);
        }
        let token = base64url(&random::<32>());
        broadcast.viewers.insert(
            viewer,
            Viewer {
                token: sha256(token.as_bytes()),
                expires_at: now + self.token_ttl,
                bound: false,
                end: watch::channel(None).0,
            },
        );
        Ok(token)
    }

    /// Removes one viewer; unknown broadcasts and viewers are ignored.
    pub fn remove_viewer(&self, id: BroadcastId, viewer: &str) {
        if let Some(broadcast) = lock(&self.broadcasts).get_mut(&id)
            && let Some(old) = broadcast.viewers.remove(viewer)
        {
            old.remove(BroadcastEnd::Deleted);
        }
    }

    /// Ends a broadcast and all its viewers; an unknown id is ignored.
    pub fn delete(&self, id: BroadcastId) {
        if let Some(broadcast) = lock(&self.broadcasts).remove(&id) {
            broadcast.end(BroadcastEnd::Deleted);
        }
    }

    /// Removes `hash` everywhere: broadcasts it publishes end, and it stops viewing all others.
    pub fn drop_peer(&self, hash: &str) {
        let mut broadcasts = lock(&self.broadcasts);
        broadcasts.retain(|_, broadcast| {
            if broadcast.publisher == hash {
                broadcast.end(BroadcastEnd::Revoked);
                return false;
            }
            if let Some(old) = broadcast.viewers.remove(hash) {
                old.remove(BroadcastEnd::Revoked);
            }
            true
        });
    }

    /// Whether `hash` is the publisher or an admitted viewer of any broadcast; together with the
    /// pairs this is the relay's allowlist.
    pub fn allowed(&self, hash: &str) -> bool {
        lock(&self.broadcasts).values().any(|b| b.has(hash))
    }

    /// Forgets broadcasts whose publisher never connected and viewer tokens that ran out unused.
    pub fn sweep(&self, now: Instant) {
        lock(&self.broadcasts).retain(|_, broadcast| {
            if !broadcast.publisher_bound && now >= broadcast.expires_at {
                broadcast.end(BroadcastEnd::Expired);
                return false;
            }
            broadcast.viewers.retain(|_, v| {
                let expired = v.waiting_expired(now);
                if expired {
                    v.remove(BroadcastEnd::Expired);
                }
                !expired
            });
            true
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    const P: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const V: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const W: &str = "cccccccccccccccccccccccccccccccccccccccc";

    fn registry(limits: &str) -> Registry {
        let config =
            Config::parse(&format!("public_host = \"relay.test\"\n[limits]\n{limits}")).unwrap();
        Registry::new(&config.limits)
    }

    fn viewer_end(
        registry: &Registry,
        id: BroadcastId,
        hash: &str,
    ) -> watch::Receiver<Option<BroadcastEnd>> {
        lock(&registry.broadcasts)[&id].viewers[hash]
            .end
            .subscribe()
    }

    fn broadcast_end(
        registry: &Registry,
        id: BroadcastId,
    ) -> watch::Receiver<Option<BroadcastEnd>> {
        lock(&registry.broadcasts)[&id].end.subscribe()
    }

    #[test]
    fn ids_are_16_hex_digits() {
        let id = BroadcastId(0x00ab_cdef_0123_4567);
        assert_eq!(id.to_string(), "00abcdef01234567");
        assert_eq!(BroadcastId::parse("00ABCDEF01234567"), Some(id));
        assert_eq!(BroadcastId::parse("abcdef01234567"), None);
        assert_eq!(BroadcastId::parse("00abcdef0123456g"), None);
        assert_eq!(BroadcastId::parse("+0abcdef01234567"), None);
    }

    #[test]
    fn create_respects_max_broadcasts() {
        let registry = registry("max_broadcasts = 2");
        assert_eq!(registry.max_viewers(), Some(25));
        let (a, token) = registry.create(P.into(), 25).unwrap();
        assert_eq!(token.len(), 43);
        let (b, _) = registry.create(P.into(), 25).unwrap();
        assert_ne!(a, b);
        assert_eq!(registry.create(V.into(), 25), Err(CreateError::RelayFull));
        registry.delete(a);
        assert!(registry.create(V.into(), 25).is_ok());
        assert_eq!(registry.count(), 2);

        let off = self::registry("max_broadcasts = 0");
        assert_eq!(off.max_viewers(), None);
        assert_eq!(off.create(P.into(), 25), Err(CreateError::RelayFull));
    }

    #[test]
    fn admit_caps_viewers_at_the_smaller_limit() {
        let registry = registry("max_viewers_per_broadcast = 2");
        let (id, _) = registry.create(P.into(), 10).unwrap();
        assert_eq!(registry.admit(id, P.into()), Err(AdmitError::Publisher));
        assert_eq!(registry.admit(id, V.into()).unwrap().len(), 43);
        assert!(registry.admit(id, W.into()).is_ok());
        let third = "d".repeat(40);
        assert_eq!(
            registry.admit(id, third.clone()),
            Err(AdmitError::BroadcastFull)
        );
        // Re-admitting a viewer replaces its seat instead of taking a new one.
        let old = viewer_end(&registry, id, V);
        assert!(registry.admit(id, V.into()).is_ok());
        assert_eq!(*old.borrow(), Some(BroadcastEnd::Revoked));
        registry.remove_viewer(id, W);
        assert!(registry.admit(id, third).is_ok());

        let (small, _) = registry.create(P.into(), 1).unwrap();
        assert!(registry.admit(small, V.into()).is_ok());
        assert_eq!(
            registry.admit(small, W.into()),
            Err(AdmitError::BroadcastFull)
        );
        assert_eq!(
            registry.admit(BroadcastId(1), V.into()),
            Err(AdmitError::UnknownBroadcast)
        );
    }

    #[test]
    fn expired_viewer_tokens_free_their_seat() {
        let registry = registry("max_viewers_per_broadcast = 1");
        let (id, _) = registry.create(P.into(), 1).unwrap();
        registry.admit(id, V.into()).unwrap();
        lock(&registry.broadcasts)
            .get_mut(&id)
            .unwrap()
            .viewers
            .get_mut(V)
            .unwrap()
            .expires_at = Instant::now();
        assert!(registry.admit(id, W.into()).is_ok());
        assert!(!registry.allowed(V));
    }

    #[test]
    fn removals_signal_the_links() {
        let registry = registry("");
        let (id, _) = registry.create(P.into(), 25).unwrap();
        registry.admit(id, V.into()).unwrap();
        registry.admit(id, W.into()).unwrap();
        let v = viewer_end(&registry, id, V);
        let w = viewer_end(&registry, id, W);
        let b = broadcast_end(&registry, id);

        registry.remove_viewer(id, V);
        assert_eq!(*v.borrow(), Some(BroadcastEnd::Deleted));
        assert!(!registry.allowed(V));
        assert_eq!(*b.borrow(), None);

        registry.delete(id);
        assert_eq!(*w.borrow(), Some(BroadcastEnd::Deleted));
        assert_eq!(*b.borrow(), Some(BroadcastEnd::Deleted));
        assert!(!registry.allowed(P) && !registry.allowed(W));
        // Deleting again or removing from an unknown broadcast is harmless.
        registry.delete(id);
        registry.remove_viewer(id, W);
    }

    #[test]
    fn drop_peer_ends_its_broadcasts_and_views() {
        let registry = registry("");
        let (own, _) = registry.create(P.into(), 25).unwrap();
        let (other, _) = registry.create(V.into(), 25).unwrap();
        registry.admit(own, W.into()).unwrap();
        registry.admit(other, P.into()).unwrap();
        let own_end = broadcast_end(&registry, own);
        let w = viewer_end(&registry, own, W);
        let p_viewing = viewer_end(&registry, other, P);

        registry.drop_peer(P);
        assert_eq!(*own_end.borrow(), Some(BroadcastEnd::Revoked));
        assert_eq!(*w.borrow(), Some(BroadcastEnd::Revoked));
        assert_eq!(*p_viewing.borrow(), Some(BroadcastEnd::Revoked));
        assert!(!registry.allowed(P) && !registry.allowed(W));
        assert!(registry.allowed(V));
        assert_eq!(registry.count(), 1);
    }

    #[test]
    fn sweep_forgets_unused_broadcasts() {
        let registry = registry("");
        let (id, _) = registry.create(P.into(), 25).unwrap();
        registry.admit(id, V.into()).unwrap();
        let end = broadcast_end(&registry, id);
        registry.sweep(Instant::now());
        assert_eq!(registry.count(), 1);
        registry.sweep(Instant::now() + Duration::from_secs(121));
        assert_eq!(registry.count(), 0);
        assert_eq!(*end.borrow(), Some(BroadcastEnd::Expired));
    }

    #[test]
    fn sweep_keeps_bound_publishers_and_viewers() {
        let registry = registry("");
        let (id, _) = registry.create(P.into(), 25).unwrap();
        registry.admit(id, V.into()).unwrap();
        registry.admit(id, W.into()).unwrap();
        {
            let mut broadcasts = lock(&registry.broadcasts);
            let broadcast = broadcasts.get_mut(&id).unwrap();
            broadcast.publisher_bound = true;
            broadcast.viewers.get_mut(V).unwrap().bound = true;
        }
        registry.sweep(Instant::now() + Duration::from_secs(121));
        assert!(registry.allowed(P) && registry.allowed(V));
        assert!(!registry.allowed(W));
    }
}
