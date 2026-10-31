//! Pairs, sessions and forwarding (spec FR-4.31 … FR-4.36).
//!
//! A pair is created by the bot through the API and names exactly two Mumble certificate hashes.
//! Each side connects (QUIC or TCP), proves its certificate in the TLS handshake and sends
//! `bind` with the pair's token. The first side waits; when the second binds, the first side's
//! task becomes the session and copies inner packets between the two links unchanged. The relay
//! never forwards to anyone outside the pair and never dials out, so it cannot be used as a proxy.

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quinn::SendDatagramError;
use serde::Deserialize;
use tokio::io::{AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, watch};
use tokio::time::{sleep_until, timeout};

use crate::config::{Config, Limits};
use crate::limits::{PairRate, SourceLimits, TokenBucket, lock, mbit_to_bytes};
use crate::tcp::{self, FRAME_CONTROL, FRAME_PACKET, MAX_FRAME};
use crate::{base64url, hex, random, sha256};

/// Without an API call for this long the bot counts as gone (FR-4.34).
const BOT_GONE_AFTER: Duration = Duration::from_secs(90);
/// When the bot is gone, sessions end after this much silence even with a longer idle timeout.
const BOT_GONE_IDLE: Duration = Duration::from_secs(60);
/// Expired pairs stay known for a while so a late `bind` gets `expired`, not `unknown_token`.
const EXPIRED_KEPT: Duration = Duration::from_secs(300);
const TOTALS_EVERY: Duration = Duration::from_secs(300);
/// Upper bound for sending a goodbye before the link is closed anyway.
const GOODBYE_TIMEOUT: Duration = Duration::from_secs(1);

pub type TlsStream = tokio_rustls::server::TlsStream<TcpStream>;

/// What the bot announces to clients; reported by `GET /v1/status`.
#[derive(Debug, Clone)]
pub struct Announced {
    pub host: String,
    pub udp: u16,
    pub tcp: u16,
    pub fp: String,
}

/// Why a `bind` or connection was refused; the code is sent to the client (spec §6.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    UnknownToken,
    Expired,
    NotAllowed,
    BotGone,
    RelayFull,
    PeerLimit,
    RateLimited,
    Version,
    BadRequest,
}

impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Self::UnknownToken => "unknown_token",
            Self::Expired => "expired",
            Self::NotAllowed => "not_allowed",
            Self::BotGone => "bot_gone",
            Self::RelayFull => "relay_full",
            Self::PeerLimit => "peer_limit",
            Self::RateLimited => "rate_limited",
            Self::Version => "version",
            Self::BadRequest => "bad_request",
        }
    }

    /// Refusals that point at a wrong or forged client rather than at the relay's state; they
    /// count against the failed-handshake limit of the source.
    pub fn is_failure(self) -> bool {
        matches!(
            self,
            Self::UnknownToken | Self::NotAllowed | Self::Version | Self::BadRequest
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Udp,
    Tcp,
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Udp => "udp",
            Self::Tcp => "tcp",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndReason {
    Closed,
    Idle,
    Revoked,
    MaxDuration,
    BotGone,
}

impl fmt::Display for EndReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Closed => "closed",
            Self::Idle => "idle",
            Self::Revoked => "revoked",
            Self::MaxDuration => "max_duration",
            Self::BotGone => "bot_gone",
        })
    }
}

/// One authenticated outer connection of a client.
pub enum Link {
    /// Control messages go over the client's bidirectional stream, packets as datagrams.
    Quic {
        conn: quinn::Connection,
        ctl: quinn::SendStream,
    },
    /// Control messages and packets share one framed TLS stream.
    Tcp {
        rd: ReadHalf<TlsStream>,
        wr: WriteHalf<TlsStream>,
    },
}

impl Link {
    fn transport(&self) -> Transport {
        match self {
            Self::Quic { .. } => Transport::Udp,
            Self::Tcp { .. } => Transport::Tcp,
        }
    }

    async fn send_control(&mut self, message: &str) -> std::io::Result<()> {
        match self {
            Self::Quic { ctl, .. } => {
                let mut line = Vec::with_capacity(message.len() + 1);
                line.extend_from_slice(message.as_bytes());
                line.push(b'\n');
                ctl.write_all(&line).await.map_err(std::io::Error::other)
            }
            Self::Tcp { wr, .. } => tcp::write_frame(wr, FRAME_CONTROL, message.as_bytes()).await,
        }
    }

    /// Sends the error message and closes the connection.
    pub async fn fail(mut self, refusal: Refusal) {
        let message = format!(r#"{{"v":1,"t":"error","code":"{}"}}"#, refusal.code());
        let _ = timeout(GOODBYE_TIMEOUT, self.send_control(&message)).await;
        match self {
            Self::Quic { conn, mut ctl } => {
                // Closing the connection discards unsent stream data, so give the error message
                // a moment to arrive; the close reason carries the code as well.
                if ctl.finish().is_ok() {
                    let _ = timeout(GOODBYE_TIMEOUT, ctl.stopped()).await;
                }
                conn.close(1u32.into(), refusal.code().as_bytes());
            }
            Self::Tcp { mut wr, .. } => {
                let _ = timeout(GOODBYE_TIMEOUT, wr.shutdown()).await;
            }
        }
    }

    async fn close(self) {
        let (_, tx) = self.split();
        tx.close().await;
    }

    /// Resolves when the client closes; anything it sends meanwhile is discarded (packets before
    /// `ready` have nowhere to go).
    async fn closed(&mut self) {
        match self {
            Self::Quic { conn, .. } => {
                conn.closed().await;
            }
            Self::Tcp { rd, .. } => {
                let mut buf = [0u8; MAX_FRAME];
                while tcp::read_frame(rd, &mut buf).await.is_ok() {}
            }
        }
    }

    fn split(self) -> (LinkRx, LinkTx) {
        match self {
            Self::Quic { conn, .. } => (LinkRx::Quic(conn.clone()), LinkTx::Quic(conn)),
            Self::Tcp { rd, wr } => (LinkRx::Tcp(rd, Box::new([0u8; MAX_FRAME])), LinkTx::Tcp(wr)),
        }
    }
}

enum LinkRx {
    Quic(quinn::Connection),
    Tcp(ReadHalf<TlsStream>, Box<[u8; MAX_FRAME]>),
}

impl LinkRx {
    /// The next inner packet; `None` when the link closed or broke the framing.
    async fn recv(&mut self) -> Option<bytes::Bytes> {
        match self {
            Self::Quic(conn) => conn.read_datagram().await.ok(),
            Self::Tcp(rd, buf) => loop {
                let (kind, len) = tcp::read_frame(rd, buf).await.ok()?;
                match kind {
                    FRAME_PACKET => return Some(bytes::Bytes::copy_from_slice(&buf[..len])),
                    // `bind` is the only client control message; later ones are ignored.
                    FRAME_CONTROL => {}
                    _ => return None,
                }
            },
        }
    }
}

enum LinkTx {
    Quic(quinn::Connection),
    Tcp(WriteHalf<TlsStream>),
}

enum Sent {
    Ok,
    Dropped,
    Closed,
}

impl LinkTx {
    async fn send(&mut self, packet: bytes::Bytes) -> Sent {
        match self {
            Self::Quic(conn) => match conn.send_datagram(packet) {
                Ok(()) => Sent::Ok,
                // Bigger than this path carries: like a lost packet for the inner QUIC.
                Err(SendDatagramError::TooLarge) => Sent::Dropped,
                Err(_) => Sent::Closed,
            },
            Self::Tcp(wr) => match tcp::write_frame(wr, FRAME_PACKET, &packet).await {
                Ok(()) => Sent::Ok,
                Err(_) => Sent::Closed,
            },
        }
    }

    async fn close(self) {
        match self {
            Self::Quic(conn) => conn.close(0u32.into(), b"closed"),
            Self::Tcp(mut wr) => {
                let _ = timeout(GOODBYE_TIMEOUT, wr.shutdown()).await;
            }
        }
    }
}

struct Pair {
    hashes: [String; 2],
    expires_at: Instant,
    bound: [bool; 2],
    /// Set by the first `bind`; from then on the pair counts as a session.
    session: Option<String>,
    /// The waiting first side's handle for the second side's link.
    waiting: Option<oneshot::Sender<Link>>,
    end: watch::Sender<Option<EndReason>>,
}

impl Pair {
    fn has(&self, hash: &str) -> bool {
        self.hashes.iter().any(|h| h == hash)
    }

    fn expired(&self, now: Instant) -> bool {
        self.session.is_none() && now >= self.expires_at
    }

    fn revoke(&self) {
        self.end.send_replace(Some(EndReason::Revoked));
    }
}

enum Bind {
    First {
        key: [u8; 32],
        id: String,
        expires_at: Instant,
        partner: oneshot::Receiver<Link>,
        end: watch::Receiver<Option<EndReason>>,
    },
    Second(oneshot::Sender<Link>),
}

pub enum PairError {
    RateLimited,
    RelayFull,
}

pub struct Relay {
    pub announced: Announced,
    pub boot_id: String,
    limits: Limits,
    pub sources: SourceLimits,
    pair_rate: PairRate,
    pairs: Mutex<HashMap<[u8; 32], Pair>>,
    bot_seen: Mutex<Option<Instant>>,
    total: Mutex<TokenBucket>,
    bytes_total: AtomicU64,
}

impl Relay {
    pub fn new(config: &Config, announced: Announced) -> Arc<Self> {
        let limits = config.limits.clone();
        Arc::new(Self {
            announced,
            boot_id: hex(&random::<8>()),
            sources: SourceLimits::new(&limits),
            pair_rate: PairRate::new(limits.pairs_per_min),
            total: Mutex::new(TokenBucket::new(mbit_to_bytes(limits.total_mbit))),
            limits,
            pairs: Mutex::new(HashMap::new()),
            bot_seen: Mutex::new(None),
            bytes_total: AtomicU64::new(0),
        })
    }

    pub fn max_sessions(&self) -> u32 {
        self.limits.max_sessions
    }

    pub fn token_ttl_s(&self) -> u32 {
        self.limits.token_ttl_s
    }

    /// Pairs with at least one bound side.
    pub fn sessions(&self) -> usize {
        let pairs = lock(&self.pairs);
        pairs.values().filter(|p| p.session.is_some()).count()
    }

    /// Called on every authenticated API request.
    pub fn bot_seen(&self) {
        *lock(&self.bot_seen) = Some(Instant::now());
    }

    fn bot_gone(&self) -> bool {
        lock(&self.bot_seen).is_none_or(|seen| seen.elapsed() >= BOT_GONE_AFTER)
    }

    /// A new pair for two different certificate hashes (validated by the caller). A pair for the
    /// same two hashes is replaced: its token stops working and its session ends.
    pub fn create_pair(&self, a: String, b: String) -> Result<String, PairError> {
        if !self.pair_rate.allow() {
            return Err(PairError::RateLimited);
        }
        let mut pairs = lock(&self.pairs);
        let sessions = pairs.values().filter(|p| p.session.is_some()).count();
        if sessions >= self.limits.max_sessions as usize {
            return Err(PairError::RelayFull);
        }
        pairs.retain(|_, pair| {
            let same = pair.has(&a) && pair.has(&b);
            if same {
                pair.revoke();
            }
            !same
        });
        let token = base64url(&random::<32>());
        let ttl = Duration::from_secs(self.limits.token_ttl_s.into());
        pairs.insert(
            sha256(token.as_bytes()),
            Pair {
                hashes: [a, b],
                expires_at: Instant::now() + ttl,
                bound: [false, false],
                session: None,
                waiting: None,
                end: watch::channel(None).0,
            },
        );
        Ok(token)
    }

    /// Removes `hash` from the allowlist: all its pairs are revoked and their sessions end.
    pub fn drop_peer(&self, hash: &str) {
        lock(&self.pairs).retain(|_, pair| {
            let has = pair.has(hash);
            if has {
                pair.revoke();
            }
            !has
        });
    }

    /// Whether `hash` is in a pair (the allowlist, FR-4.32). Recently expired pairs still count,
    /// so a late client learns `expired` instead of `not_allowed`; such a pair grants nothing
    /// else.
    pub fn allowed(&self, hash: &str) -> bool {
        lock(&self.pairs).values().any(|pair| pair.has(hash))
    }

    fn bind(&self, token: &str, hash: &str) -> Result<Bind, Refusal> {
        if self.bot_gone() {
            return Err(Refusal::BotGone);
        }
        let key = sha256(token.as_bytes());
        let now = Instant::now();
        let mut pairs = lock(&self.pairs);
        let pair = pairs.get(&key).ok_or(Refusal::UnknownToken)?;
        if now >= pair.expires_at {
            return Err(Refusal::Expired);
        }
        let side = (0..2)
            .find(|&i| pair.hashes[i] == hash && !pair.bound[i])
            .ok_or(Refusal::NotAllowed)?;
        if pair.session.is_some() {
            let pair = pairs.get_mut(&key).ok_or(Refusal::UnknownToken)?;
            let partner = pair.waiting.take().ok_or(Refusal::NotAllowed)?;
            pair.bound[side] = true;
            return Ok(Bind::Second(partner));
        }

        let active = || pairs.values().filter(|p| p.session.is_some());
        if active().count() >= self.limits.max_sessions as usize {
            return Err(Refusal::RelayFull);
        }
        let per_peer = self.limits.max_sessions_per_peer as usize;
        if pair
            .hashes
            .iter()
            .any(|h| active().filter(|p| p.has(h)).count() >= per_peer)
        {
            return Err(Refusal::PeerLimit);
        }
        let id = hex(&random::<4>());
        let (tx, rx) = oneshot::channel();
        let pair = pairs.get_mut(&key).ok_or(Refusal::UnknownToken)?;
        pair.bound[side] = true;
        pair.session = Some(id.clone());
        pair.waiting = Some(tx);
        Ok(Bind::First {
            key,
            id,
            expires_at: pair.expires_at,
            partner: rx,
            end: pair.end.subscribe(),
        })
    }

    /// Takes over an authenticated link whose first control message was `bind_message`.
    pub async fn serve(self: Arc<Self>, link: Link, bind_message: &[u8], hash: &str, ip: IpAddr) {
        let result = parse_bind(bind_message).and_then(|token| self.bind(&token, hash));
        match result {
            Err(refusal) => {
                tracing::debug!(peer = &hash[..8], code = refusal.code(), "bind refused");
                if refusal.is_failure() {
                    self.sources.failed(ip);
                }
                link.fail(refusal).await;
            }
            Ok(Bind::Second(partner)) => {
                tracing::debug!(peer = &hash[..8], "bound second side");
                if let Err(link) = partner.send(link) {
                    // The first side left in the same moment.
                    link.close().await;
                }
            }
            Ok(Bind::First {
                key,
                id,
                expires_at,
                partner,
                end,
            }) => {
                tracing::debug!(session = %id, peer = &hash[..8], "bound first side");
                self.wait_for_partner(link, key, id, expires_at, partner, end)
                    .await;
            }
        }
    }

    async fn wait_for_partner(
        &self,
        mut link: Link,
        key: [u8; 32],
        id: String,
        expires_at: Instant,
        partner: oneshot::Receiver<Link>,
        mut end: watch::Receiver<Option<EndReason>>,
    ) {
        if link.send_control(r#"{"v":1,"t":"waiting"}"#).await.is_err() {
            self.forget(&key);
            return;
        }
        tokio::select! {
            other = partner => match other {
                Ok(other) => self.run_session(key, id, link, other, end).await,
                Err(_) => link.close().await,
            },
            () = sleep_until(expires_at.into()) => {
                self.unbind(&key);
                link.fail(Refusal::Expired).await;
            }
            _ = end.changed() => link.close().await,
            () = link.closed() => self.forget(&key),
        }
    }

    /// The pair's TTL ran out with only one side bound: it becomes an expired pair again, so the
    /// other side's late `bind` gets `expired`.
    fn unbind(&self, key: &[u8; 32]) {
        if let Some(pair) = lock(&self.pairs).get_mut(key) {
            pair.bound = [false, false];
            pair.session = None;
            pair.waiting = None;
        }
    }

    fn forget(&self, key: &[u8; 32]) {
        lock(&self.pairs).remove(key);
    }

    async fn run_session(
        &self,
        key: [u8; 32],
        id: String,
        mut first: Link,
        mut second: Link,
        mut end: watch::Receiver<Option<EndReason>>,
    ) {
        const READY: &str = r#"{"v":1,"t":"ready"}"#;
        let started = Instant::now();
        let transports = format!("{}/{}", first.transport(), second.transport());
        let (ready_a, ready_b) =
            tokio::join!(first.send_control(READY), second.send_control(READY));
        let (mut a_rx, mut a_tx) = first.split();
        let (mut b_rx, mut b_tx) = second.split();
        let stats = [Direction::default(), Direction::default()];
        let last_packet = AtomicU64::new(0);

        let reason = if ready_a.is_err() || ready_b.is_err() {
            EndReason::Closed
        } else {
            tokio::select! {
                r = self.pump(&mut a_rx, &mut b_tx, &stats[0], started, &last_packet) => r,
                r = self.pump(&mut b_rx, &mut a_tx, &stats[1], started, &last_packet) => r,
                r = self.watch(started, &last_packet, &mut end) => r,
            }
        };
        tokio::join!(a_tx.close(), b_tx.close());
        self.forget(&key);

        let [ab, ba] = &stats;
        tracing::info!(
            session = %id,
            transports = %transports,
            duration_s = started.elapsed().as_secs(),
            bytes_ab = ab.bytes.load(Ordering::Relaxed),
            bytes_ba = ba.bytes.load(Ordering::Relaxed),
            dropped = ab.dropped.load(Ordering::Relaxed) + ba.dropped.load(Ordering::Relaxed),
            reason = %reason,
            "session ended"
        );
    }

    /// Copies packets one way, unchanged, within the session and total bandwidth limits.
    async fn pump(
        &self,
        rx: &mut LinkRx,
        tx: &mut LinkTx,
        stats: &Direction,
        started: Instant,
        last_packet: &AtomicU64,
    ) -> EndReason {
        let mut bucket = TokenBucket::new(mbit_to_bytes(self.limits.session_mbit));
        loop {
            let Some(packet) = rx.recv().await else {
                return EndReason::Closed;
            };
            let now = Instant::now();
            let millis = u64::try_from(now.duration_since(started).as_millis()).unwrap_or(u64::MAX);
            last_packet.store(millis, Ordering::Relaxed);
            let len = packet.len();
            if !bucket.take(len, now) || !lock(&self.total).take(len, now) {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            match tx.send(packet).await {
                Sent::Ok => {
                    stats.bytes.fetch_add(len as u64, Ordering::Relaxed);
                    self.bytes_total.fetch_add(len as u64, Ordering::Relaxed);
                }
                Sent::Dropped => {
                    stats.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Sent::Closed => return EndReason::Closed,
            }
        }
    }

    /// Ends the session on revocation, idle time, maximum duration or a gone bot.
    async fn watch(
        &self,
        started: Instant,
        last_packet: &AtomicU64,
        end: &mut watch::Receiver<Option<EndReason>>,
    ) -> EndReason {
        let idle_timeout = Duration::from_secs(self.limits.idle_timeout_s.into());
        let max_duration = (self.limits.max_session_s > 0)
            .then(|| Duration::from_secs(self.limits.max_session_s.into()));
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                changed = end.changed() => {
                    // A dropped sender means the pair is gone, which only a revocation does.
                    return changed.ok().and_then(|()| *end.borrow()).unwrap_or(EndReason::Revoked);
                }
                _ = tick.tick() => {}
            }
            let now = started.elapsed();
            let idle =
                now.saturating_sub(Duration::from_millis(last_packet.load(Ordering::Relaxed)));
            if idle >= idle_timeout {
                return EndReason::Idle;
            }
            if max_duration.is_some_and(|max| now >= max) {
                return EndReason::MaxDuration;
            }
            if idle >= BOT_GONE_IDLE && self.bot_gone() {
                return EndReason::BotGone;
            }
        }
    }
}

#[derive(Default)]
struct Direction {
    bytes: AtomicU64,
    dropped: AtomicU64,
}

#[derive(Deserialize)]
struct BindMessage {
    v: Option<u64>,
    t: Option<String>,
    tok: Option<String>,
}

/// The token from a `bind` message (spec §6.5).
fn parse_bind(message: &[u8]) -> Result<String, Refusal> {
    let message: BindMessage = serde_json::from_slice(message).map_err(|_| Refusal::BadRequest)?;
    if message.v != Some(1) {
        return Err(if message.v.is_some() {
            Refusal::Version
        } else {
            Refusal::BadRequest
        });
    }
    let token = message.tok.filter(|_| message.t.as_deref() == Some("bind"));
    match token {
        Some(token)
            if token.len() == 43
                && token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
        {
            Ok(token)
        }
        _ => Err(Refusal::BadRequest),
    }
}

/// Forgets long-expired pairs and idle rate-limit entries, and logs totals now and then.
pub async fn housekeeping(relay: Arc<Relay>) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    let mut last_totals = Instant::now();
    loop {
        tick.tick().await;
        let now = Instant::now();
        lock(&relay.pairs)
            .retain(|_, pair| !pair.expired(now) || now < pair.expires_at + EXPIRED_KEPT);
        relay.sources.sweep();
        if last_totals.elapsed() >= TOTALS_EVERY {
            last_totals = now;
            tracing::info!(
                sessions = relay.sessions(),
                bytes = relay.bytes_total.load(Ordering::Relaxed),
                "totals"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

    fn relay(limits: &str) -> Arc<Relay> {
        let config =
            Config::parse(&format!("public_host = \"relay.test\"\n[limits]\n{limits}")).unwrap();
        let announced = Announced {
            host: "relay.test".into(),
            udp: 443,
            tcp: 443,
            fp: "00".repeat(32),
        };
        let relay = Relay::new(&config, announced);
        relay.bot_seen();
        relay
    }

    fn refusal(result: Result<Bind, Refusal>) -> Refusal {
        match result {
            Err(refusal) => refusal,
            Ok(_) => panic!("bind should be refused"),
        }
    }

    #[test]
    fn parses_bind() {
        let tok = "A".repeat(43);
        let ok = format!(r#"{{"v":1,"t":"bind","tok":"{tok}"}}"#);
        assert_eq!(parse_bind(ok.as_bytes()), Ok(tok.clone()));
        let v2 = format!(r#"{{"v":2,"t":"bind","tok":"{tok}"}}"#);
        assert_eq!(parse_bind(v2.as_bytes()), Err(Refusal::Version));
        assert_eq!(
            parse_bind(br#"{"v":1,"t":"bind","tok":"short"}"#),
            Err(Refusal::BadRequest)
        );
        assert_eq!(parse_bind(b"not json"), Err(Refusal::BadRequest));
        let other = format!(r#"{{"v":1,"t":"other","tok":"{tok}"}}"#);
        assert_eq!(parse_bind(other.as_bytes()), Err(Refusal::BadRequest));
    }

    #[test]
    fn only_the_pair_may_bind() {
        let relay = relay("");
        let token = relay.create_pair(A.into(), B.into()).ok().unwrap();
        assert!(relay.allowed(A) && relay.allowed(B) && !relay.allowed(C));
        assert_eq!(refusal(relay.bind(&token, C)), Refusal::NotAllowed);
        assert_eq!(
            refusal(relay.bind(&"x".repeat(43), A)),
            Refusal::UnknownToken
        );
        assert!(matches!(relay.bind(&token, A), Ok(Bind::First { .. })));
        // The same side twice is refused.
        assert_eq!(refusal(relay.bind(&token, A)), Refusal::NotAllowed);
        assert_eq!(relay.sessions(), 1);
        assert!(matches!(relay.bind(&token, B), Ok(Bind::Second(_))));
        assert_eq!(refusal(relay.bind(&token, B)), Refusal::NotAllowed);
    }

    #[test]
    fn new_pair_and_drop_peer_revoke() {
        let relay = relay("");
        let old = relay.create_pair(A.into(), B.into()).ok().unwrap();
        let new = relay.create_pair(B.into(), A.into()).ok().unwrap();
        assert_eq!(refusal(relay.bind(&old, A)), Refusal::UnknownToken);
        relay.drop_peer(B);
        assert_eq!(refusal(relay.bind(&new, A)), Refusal::UnknownToken);
        assert!(!relay.allowed(A));
    }

    #[test]
    fn session_limits() {
        let relay = relay("max_sessions = 2\nmax_sessions_per_peer = 1");
        let ab = relay.create_pair(A.into(), B.into()).ok().unwrap();
        let ac = relay.create_pair(A.into(), C.into()).ok().unwrap();
        let bind = relay.bind(&ab, A);
        assert!(bind.is_ok());
        assert_eq!(refusal(relay.bind(&ac, C)), Refusal::PeerLimit);
        let relay = self::relay("max_sessions = 1");
        let ab = relay.create_pair(A.into(), B.into()).ok().unwrap();
        let bc = relay.create_pair(B.into(), C.into()).ok().unwrap();
        let bind = relay.bind(&ab, A);
        assert!(bind.is_ok());
        assert_eq!(refusal(relay.bind(&bc, C)), Refusal::RelayFull);
        assert!(matches!(
            relay.create_pair(A.into(), C.into()),
            Err(PairError::RelayFull)
        ));
    }

    #[test]
    fn bot_gone_refuses_binds() {
        let relay = relay("");
        let token = relay.create_pair(A.into(), B.into()).ok().unwrap();
        *lock(&relay.bot_seen) = Some(Instant::now() - BOT_GONE_AFTER);
        assert_eq!(refusal(relay.bind(&token, A)), Refusal::BotGone);
    }

    #[test]
    fn pairs_per_minute() {
        let relay = relay("pairs_per_min = 1");
        assert!(relay.create_pair(A.into(), B.into()).is_ok());
        assert!(matches!(
            relay.create_pair(A.into(), C.into()),
            Err(PairError::RateLimited)
        ));
    }
}
