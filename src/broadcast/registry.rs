//! The broadcasts the bot created, their publisher and viewer tokens, and their limits.
//! Screen-share and plugin-data broadcasts (spec 09 FR-9.84, FR-9.86) are counted and capped
//! separately, and a link only binds while the broadcast's use is switched on.
//!
//! Tokens are kept only as SHA-256 hashes, as for pairs. A broadcast or a viewer that is removed
//! signals its `end` watch, so a bound link can close (and, for a viewer, tell the publisher).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use super::session::{Session, Subscriber};
use crate::config::{Config, PluginData, Uses};
use crate::limits::lock;
use crate::relay::Refusal;
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
    /// The publisher closed its link (or, for a subscriber, the subscriber closed its own).
    Closed,
    /// The publisher sent nothing for `idle_timeout_s`.
    Idle,
}

/// What a broadcast carries; it decides the limits and which units its publisher may send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcastUse {
    ScreenShare,
    Camera,
    PluginData,
}

impl BroadcastUse {
    /// The name in `[uses]` and in the API.
    pub fn name(self) -> &'static str {
        match self {
            Self::ScreenShare => "screen_share",
            Self::Camera => "camera",
            Self::PluginData => "plugin_data",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum CreateError {
    /// The use's `max_broadcasts` broadcasts exist already (or its broadcasts are off).
    RelayFull,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AdmitError {
    UnknownBroadcast,
    /// The viewer is the publisher itself.
    Publisher,
    BroadcastFull,
}

/// One admitted viewer. `slot` is set once its subscriber link is bound.
struct Viewer {
    token: [u8; 32],
    expires_at: Instant,
    bound: bool,
    slot: Option<u16>,
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
    use_: BroadcastUse,
    publisher: String,
    publisher_token: [u8; 32],
    expires_at: Instant,
    publisher_bound: bool,
    max_viewers: u16,
    viewers: HashMap<String, Viewer>,
    end: watch::Sender<Option<BroadcastEnd>>,
    session: Arc<Session>,
    next_slot: u16,
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

/// A bound publisher link: what it needs to run the broadcast.
pub struct PublisherBind {
    pub id: BroadcastId,
    pub session: Arc<Session>,
    pub end: watch::Receiver<Option<BroadcastEnd>>,
}

/// A bound subscriber link. `token` tells its own entry apart from a later re-admission.
pub struct ViewerBind {
    pub id: BroadcastId,
    pub session: Arc<Session>,
    pub subscriber: Arc<Subscriber>,
    pub token: [u8; 32],
    pub end: watch::Receiver<Option<BroadcastEnd>>,
}

pub struct Registry {
    max_broadcasts: usize,
    max_camera_broadcasts: usize,
    camera_max_kbps: u32,
    max_viewers: u16,
    token_ttl: Duration,
    in_mbit: u32,
    out_mbit: u32,
    uses: Uses,
    plugin_data: PluginData,
    broadcasts: Mutex<HashMap<BroadcastId, Broadcast>>,
}

impl Registry {
    pub fn new(config: &Config) -> Self {
        let limits = &config.limits;
        Self {
            max_broadcasts: limits.max_broadcasts as usize,
            max_camera_broadcasts: limits.max_camera_broadcasts as usize,
            camera_max_kbps: limits.camera_max_kbps,
            // The config validates the range; the fallback is never used.
            max_viewers: u16::try_from(limits.max_viewers_per_broadcast).unwrap_or(u16::MAX),
            token_ttl: Duration::from_secs(limits.token_ttl_s.into()),
            in_mbit: limits.broadcast_in_mbit,
            out_mbit: limits.broadcast_out_mbit,
            uses: config.uses,
            plugin_data: config.plugin_data,
            broadcasts: Mutex::new(HashMap::new()),
        }
    }

    /// Viewers per broadcast as announced to clients; `None` when neither screen-share nor
    /// camera broadcasts can be created (by limit or by `[uses]`).
    pub fn max_viewers(&self) -> Option<u16> {
        (self.screen_shares_on() || self.max_camera() > 0).then_some(self.max_viewers)
    }

    /// Camera broadcasts at the same time; 0 when cameras are off.
    pub fn max_camera(&self) -> usize {
        if self.uses.camera {
            self.max_camera_broadcasts
        } else {
            0
        }
    }

    fn screen_shares_on(&self) -> bool {
        self.uses.screen_share && self.max_broadcasts > 0
    }

    /// Broadcasts that exist right now.
    pub fn count(&self) -> usize {
        lock(&self.broadcasts).len()
    }

    /// A new broadcast for `publisher` (a validated certificate hash) with at most `max_viewers`
    /// viewers, capped by the relay's own limit. Returns its id and the publisher's token. The
    /// caller checked that the use is switched on.
    pub fn create(
        &self,
        publisher: String,
        max_viewers: u16,
        use_: BroadcastUse,
    ) -> Result<(BroadcastId, String), CreateError> {
        let mut broadcasts = lock(&self.broadcasts);
        let max = match use_ {
            BroadcastUse::ScreenShare => self.max_broadcasts,
            BroadcastUse::Camera => self.max_camera_broadcasts,
            BroadcastUse::PluginData => self.plugin_data.max_broadcasts as usize,
        };
        if broadcasts.values().filter(|b| b.use_ == use_).count() >= max {
            return Err(CreateError::RelayFull);
        }
        let id = loop {
            let id = BroadcastId(u64::from_be_bytes(random::<8>()));
            if !broadcasts.contains_key(&id) {
                break id;
            }
        };
        let token = base64url(&random::<32>());
        let session = match use_ {
            BroadcastUse::ScreenShare => Session::new(self.in_mbit, self.out_mbit),
            BroadcastUse::Camera => Session::camera(self.camera_max_kbps, self.out_mbit),
            BroadcastUse::PluginData => Session::plugin_data(&self.plugin_data, self.out_mbit),
        };
        broadcasts.insert(
            id,
            Broadcast {
                use_,
                publisher,
                publisher_token: sha256(token.as_bytes()),
                expires_at: Instant::now() + self.token_ttl,
                publisher_bound: false,
                max_viewers: max_viewers.min(self.max_viewers),
                viewers: HashMap::new(),
                end: watch::channel(None).0,
                session: Arc::new(session),
                next_slot: 0,
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
                slot: None,
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
        self.end(id, BroadcastEnd::Deleted);
    }

    /// Ends a broadcast for `reason`; an unknown id is ignored.
    pub fn end(&self, id: BroadcastId, reason: BroadcastEnd) {
        if let Some(broadcast) = lock(&self.broadcasts).remove(&id) {
            broadcast.end(reason);
        }
    }

    /// Binds the publisher link of the broadcast with this token.
    pub fn bind_publisher(&self, token: &str, hash: &str) -> Result<PublisherBind, Refusal> {
        let key = sha256(token.as_bytes());
        let mut broadcasts = lock(&self.broadcasts);
        let (id, broadcast) = broadcasts
            .iter_mut()
            .find(|(_, b)| b.publisher_token == key)
            .ok_or(Refusal::UnknownToken)?;
        if !self.uses.allows(broadcast.use_.name()) {
            return Err(Refusal::UseNotAllowed);
        }
        if broadcast.publisher != hash || broadcast.publisher_bound {
            return Err(Refusal::NotAllowed);
        }
        if Instant::now() >= broadcast.expires_at {
            return Err(Refusal::Expired);
        }
        broadcast.publisher_bound = true;
        Ok(PublisherBind {
            id: *id,
            session: broadcast.session.clone(),
            end: broadcast.end.subscribe(),
        })
    }

    /// Binds a subscriber link and gives it a slot, unique within its broadcast.
    pub fn bind_viewer(&self, token: &str, hash: &str) -> Result<ViewerBind, Refusal> {
        let key = sha256(token.as_bytes());
        let mut broadcasts = lock(&self.broadcasts);
        let (id, broadcast) = broadcasts
            .iter_mut()
            .find(|(_, b)| b.viewers.values().any(|v| v.token == key))
            .ok_or(Refusal::UnknownToken)?;
        if !self.uses.allows(broadcast.use_.name()) {
            return Err(Refusal::UseNotAllowed);
        }
        let viewer = broadcast
            .viewers
            .get(hash)
            .filter(|v| v.token == key && !v.bound)
            .ok_or(Refusal::NotAllowed)?;
        if Instant::now() >= viewer.expires_at {
            return Err(Refusal::Expired);
        }
        if broadcast.session.viewers() >= usize::from(broadcast.max_viewers) {
            return Err(Refusal::BroadcastFull);
        }
        let slot = loop {
            let slot = broadcast.next_slot;
            broadcast.next_slot = slot.wrapping_add(1);
            if !broadcast.viewers.values().any(|v| v.slot == Some(slot)) {
                break slot;
            }
        };
        let viewer = broadcast.viewers.get_mut(hash).ok_or(Refusal::NotAllowed)?;
        viewer.bound = true;
        viewer.slot = Some(slot);
        Ok(ViewerBind {
            id: *id,
            session: broadcast.session.clone(),
            subscriber: broadcast.session.add(slot),
            token: key,
            end: viewer.end.subscribe(),
        })
    }

    /// A subscriber link ended: its seat is freed unless the bot admitted the viewer again
    /// meanwhile (then the entry has a new token).
    pub fn viewer_left(&self, id: BroadcastId, hash: &str, token: &[u8; 32]) {
        if let Some(broadcast) = lock(&self.broadcasts).get_mut(&id)
            && broadcast
                .viewers
                .get(hash)
                .is_some_and(|v| v.token == *token)
        {
            broadcast.viewers.remove(hash);
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

    const SCREEN: BroadcastUse = BroadcastUse::ScreenShare;
    const PLUGIN: BroadcastUse = BroadcastUse::PluginData;
    const CAMERA: BroadcastUse = BroadcastUse::Camera;

    const P: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const V: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const W: &str = "cccccccccccccccccccccccccccccccccccccccc";

    fn registry(limits: &str) -> Registry {
        let config =
            Config::parse(&format!("public_host = \"relay.test\"\n[limits]\n{limits}")).unwrap();
        Registry::new(&config)
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
        let (a, token) = registry.create(P.into(), 25, SCREEN).unwrap();
        assert_eq!(token.len(), 43);
        let (b, _) = registry.create(P.into(), 25, SCREEN).unwrap();
        assert_ne!(a, b);
        assert_eq!(
            registry.create(V.into(), 25, SCREEN),
            Err(CreateError::RelayFull)
        );
        registry.delete(a);
        assert!(registry.create(V.into(), 25, SCREEN).is_ok());
        assert_eq!(registry.count(), 2);

        // Camera broadcasts alone keep `max_viewers` announced.
        let screen_off = self::registry("max_broadcasts = 0");
        assert_eq!(screen_off.max_viewers(), Some(25));
        let off = self::registry("max_broadcasts = 0\nmax_camera_broadcasts = 0");
        assert_eq!(off.max_viewers(), None);
        assert_eq!(
            off.create(P.into(), 25, SCREEN),
            Err(CreateError::RelayFull)
        );
    }

    #[test]
    fn admit_caps_viewers_at_the_smaller_limit() {
        let registry = registry("max_viewers_per_broadcast = 2");
        let (id, _) = registry.create(P.into(), 10, SCREEN).unwrap();
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

        let (small, _) = registry.create(P.into(), 1, SCREEN).unwrap();
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
        let (id, _) = registry.create(P.into(), 1, SCREEN).unwrap();
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
        let (id, _) = registry.create(P.into(), 25, SCREEN).unwrap();
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
        let (own, _) = registry.create(P.into(), 25, SCREEN).unwrap();
        let (other, _) = registry.create(V.into(), 25, SCREEN).unwrap();
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
        let (id, _) = registry.create(P.into(), 25, SCREEN).unwrap();
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
        let (id, _) = registry.create(P.into(), 25, SCREEN).unwrap();
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

    #[test]
    fn binds_give_unique_slots_and_survive_re_admission() {
        let registry = registry("");
        let (id, token) = registry.create(P.into(), 25, SCREEN).unwrap();
        assert_eq!(
            registry.bind_publisher(&token, V).err(),
            Some(Refusal::NotAllowed)
        );
        assert!(registry.bind_publisher(&token, P).is_ok());
        assert_eq!(
            registry.bind_publisher(&token, P).err(),
            Some(Refusal::NotAllowed)
        );

        let v_token = registry.admit(id, V.into()).unwrap();
        let w_token = registry.admit(id, W.into()).unwrap();
        let v = registry.bind_viewer(&v_token, V).unwrap();
        let w = registry.bind_viewer(&w_token, W).unwrap();
        assert_ne!(v.subscriber.slot, w.subscriber.slot);
        assert_eq!(
            registry.bind_viewer(&v_token, V).err(),
            Some(Refusal::NotAllowed)
        );
        assert_eq!(
            registry.bind_viewer(&"A".repeat(43), V).err(),
            Some(Refusal::UnknownToken)
        );

        // The old link of a re-admitted viewer leaving must not free the new seat.
        registry.admit(id, V.into()).unwrap();
        registry.viewer_left(id, V, &v.token);
        assert!(registry.allowed(V));
        registry.viewer_left(id, W, &w.token);
        assert!(!registry.allowed(W));
        // Bound sides survive the sweep.
        registry.sweep(Instant::now() + Duration::from_secs(121));
        assert_eq!(registry.count(), 1);
    }

    #[test]
    fn each_use_has_its_own_broadcast_cap() {
        let registry = registry(
            "max_broadcasts = 1\n[uses]\nplugin_data = true\n[plugin_data]\nmax_broadcasts = 2",
        );
        assert!(registry.create(P.into(), 25, SCREEN).is_ok());
        assert_eq!(
            registry.create(V.into(), 25, SCREEN),
            Err(CreateError::RelayFull)
        );
        assert!(registry.create(P.into(), 8, PLUGIN).is_ok());
        assert!(registry.create(V.into(), 8, PLUGIN).is_ok());
        assert_eq!(
            registry.create(W.into(), 8, PLUGIN),
            Err(CreateError::RelayFull)
        );
        assert_eq!(registry.count(), 3);

        // Screen sharing off does not stop plugin-data broadcasts.
        let plugins_only =
            self::registry("max_broadcasts = 0\n[uses]\nscreen_share = false\nplugin_data = true");
        assert!(plugins_only.create(P.into(), 8, PLUGIN).is_ok());
    }

    #[test]
    fn a_link_binds_only_while_its_use_is_on() {
        // Broadcasts that exist when the use is off can only come from an older config; the
        // check keeps a bind from carrying a use the operator did not allow.
        let registry = registry("[uses]\nscreen_share = false");
        let (id, token) = registry.create(P.into(), 25, SCREEN).unwrap();
        let viewer = registry.admit(id, V.into()).unwrap();
        assert_eq!(
            registry.bind_publisher(&token, P).err(),
            Some(Refusal::UseNotAllowed)
        );
        assert_eq!(
            registry.bind_viewer(&viewer, V).err(),
            Some(Refusal::UseNotAllowed)
        );
        let (_, plugin) = registry.create(P.into(), 8, PLUGIN).unwrap();
        assert_eq!(
            registry.bind_publisher(&plugin, P).err(),
            Some(Refusal::UseNotAllowed)
        );

        let on = self::registry("[uses]\nplugin_data = true");
        let (_, plugin) = on.create(P.into(), 8, PLUGIN).unwrap();
        assert!(on.bind_publisher(&plugin, P).is_ok());
    }

    #[test]
    fn cameras_have_their_own_cap_and_announcement() {
        let registry = registry("max_broadcasts = 1\nmax_camera_broadcasts = 2");
        assert_eq!(registry.max_camera(), 2);
        assert!(registry.create(P.into(), 25, SCREEN).is_ok());
        assert!(registry.create(P.into(), 25, CAMERA).is_ok());
        assert!(registry.create(V.into(), 25, CAMERA).is_ok());
        assert_eq!(
            registry.create(W.into(), 25, CAMERA),
            Err(CreateError::RelayFull)
        );
        assert_eq!(
            registry.create(W.into(), 25, SCREEN),
            Err(CreateError::RelayFull)
        );
        assert_eq!(registry.count(), 3);

        // `max_viewers_per_broadcast` caps cameras too.
        let capped = self::registry("max_viewers_per_broadcast = 2");
        let (id, _) = capped.create(P.into(), 25, CAMERA).unwrap();
        assert!(capped.admit(id, V.into()).is_ok());
        assert!(capped.admit(id, W.into()).is_ok());
        assert_eq!(
            capped.admit(id, "d".repeat(40)),
            Err(AdmitError::BroadcastFull)
        );

        let no_camera = self::registry("max_broadcasts = 0\n[uses]\ncamera = false");
        assert_eq!(no_camera.max_camera(), 0);
        assert_eq!(no_camera.max_viewers(), None);
        let (_, token) = no_camera.create(P.into(), 25, CAMERA).unwrap();
        assert_eq!(
            no_camera.bind_publisher(&token, P).err(),
            Some(Refusal::UseNotAllowed)
        );
    }
}
