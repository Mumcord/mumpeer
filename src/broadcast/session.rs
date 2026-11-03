//! The data plane of one broadcast (spec 07 FR-7.41, FR-7.45, FR-7.50): the publisher's link
//! uploads units once, the relay copies media into every subscriber's queue and routes the few
//! control units between them.
//!
//! Units travel on QUIC as one unidirectional stream each (small control units may also come as
//! a datagram), on TCP as a byte stream cut into `0x01` frames. The relay parses only the clear
//! header; media payloads are end-to-end encrypted and forwarded unchanged.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::{BufMut, Bytes, BytesMut};
use quinn::{Connection, RecvStream, SendStream};
use tokio::io::{AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::watch;
use tokio::time::timeout;

use super::control::{Assembler, Content, ControlKind, HEADER_LEN, Header, MAX_KEY_BODY, Unit};
use super::queue::{ControlQueue, Queue};
use super::registry::{BroadcastEnd, BroadcastId};
use crate::limits::{TokenBucket, lock, mbit_to_bytes};
use crate::relay::{Link, Relay, TlsStream};
use crate::tcp::{self, FRAME_CONTROL, FRAME_PACKET, MAX_FRAME};

/// Keyframe requests one subscriber may cause (spec 07 FR-7.45).
const KEYFRAME_PER_SUBSCRIBER: Duration = Duration::from_secs(2);
/// Keyframe requests the publisher gets at most, from all subscribers together.
const KEYFRAME_PER_BROADCAST: Duration = Duration::from_millis(500);
/// Control units waiting for the publisher (`keyreq`, `kf`, `gone`).
const PUBLISHER_CONTROL: usize = 64;
const GOODBYE_TIMEOUT: Duration = Duration::from_secs(1);
const READY: &str = r#"{"v":1,"t":"ready"}"#;

/// One bound subscriber.
pub struct Subscriber {
    pub slot: u16,
    queue: Queue,
    last_keyframe_request: Mutex<Option<Instant>>,
}

impl Subscriber {
    fn new(slot: u16) -> Self {
        Self {
            slot,
            queue: Queue::default(),
            last_keyframe_request: Mutex::new(None),
        }
    }
}

/// Shared state of one broadcast's links. The registry owns it; locks are held only to move a
/// unit in or out of a queue, never across I/O.
pub struct Session {
    subscribers: Mutex<Vec<Arc<Subscriber>>>,
    to_publisher: ControlQueue,
    last_keyframe_request: Mutex<Option<Instant>>,
    inbound: Mutex<TokenBucket>,
    outbound: Mutex<TokenBucket>,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    peak_viewers: AtomicUsize,
}

impl Session {
    pub fn new(in_mbit: u32, out_mbit: u32) -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            to_publisher: ControlQueue::new(PUBLISHER_CONTROL),
            last_keyframe_request: Mutex::new(None),
            inbound: Mutex::new(TokenBucket::new(mbit_to_bytes(in_mbit))),
            outbound: Mutex::new(TokenBucket::new(mbit_to_bytes(out_mbit))),
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
            peak_viewers: AtomicUsize::new(0),
        }
    }

    pub fn viewers(&self) -> usize {
        lock(&self.subscribers).len()
    }

    /// A newly bound subscriber; it gets nothing until the next keyframe.
    pub fn add(&self, slot: u16) -> Arc<Subscriber> {
        let subscriber = Arc::new(Subscriber::new(slot));
        let mut subscribers = lock(&self.subscribers);
        subscribers.push(subscriber.clone());
        self.peak_viewers
            .fetch_max(subscribers.len(), Ordering::Relaxed);
        subscriber
    }

    /// The subscriber left; the publisher learns its slot through `gone` so it can rekey.
    fn remove(&self, slot: u16) {
        let removed = {
            let mut subscribers = lock(&self.subscribers);
            let before = subscribers.len();
            subscribers.retain(|s| s.slot != slot);
            subscribers.len() < before
        };
        if removed {
            self.to_publisher.push(Unit::gone(slot));
        }
    }

    /// Forwards a keyframe request unless one went out too recently, for this subscriber or for
    /// the broadcast as a whole.
    fn request_keyframe(&self, from: Option<&Subscriber>, now: Instant) {
        let recent =
            |last: &Option<Instant>, gap| last.is_some_and(|at| now.duration_since(at) < gap);
        let mut own = from.map(|s| lock(&s.last_keyframe_request));
        if own
            .as_deref()
            .is_some_and(|last| recent(last, KEYFRAME_PER_SUBSCRIBER))
        {
            return;
        }
        {
            let mut broadcast = lock(&self.last_keyframe_request);
            if recent(&broadcast, KEYFRAME_PER_BROADCAST) {
                return;
            }
            *broadcast = Some(now);
        }
        if let Some(own) = own.as_deref_mut() {
            *own = Some(now);
        }
        self.to_publisher.push(Unit::keyframe_request());
    }

    /// A unit from the publisher. `false` means the link broke the protocol.
    pub fn publisher_unit(&self, unit: Unit, total: &Mutex<TokenBucket>, now: Instant) -> bool {
        match unit.header.content {
            Content::Video { .. } | Content::Audio => {
                let len = unit.bytes.len();
                let over = lock(&self.inbound).spend(len, now).is_err()
                    || lock(total).spend(len, now).is_err();
                if over {
                    // A lost picture breaks every later one until the next keyframe.
                    if matches!(unit.header.content, Content::Video { .. }) {
                        for subscriber in lock(&self.subscribers).iter() {
                            subscriber.queue.with(|policy| policy.resync());
                        }
                        self.request_keyframe(None, now);
                    }
                    return true;
                }
                self.bytes_in.fetch_add(len as u64, Ordering::Relaxed);
                let mut need_keyframe = Vec::new();
                for subscriber in lock(&self.subscribers).iter() {
                    if subscriber.queue.push(unit.clone(), now).need_keyframe {
                        need_keyframe.push(subscriber.clone());
                    }
                }
                for subscriber in need_keyframe {
                    self.request_keyframe(Some(&subscriber), now);
                }
                true
            }
            Content::Control(ControlKind::Key) => {
                let Some((slot, body)) = super::control::split_slot(unit.payload()) else {
                    return false;
                };
                let target = lock(&self.subscribers)
                    .iter()
                    .find(|s| s.slot == slot)
                    .cloned();
                // A subscriber that left meanwhile needs no key.
                if let Some(target) = target {
                    target.queue.push(Unit::key(body), now);
                }
                true
            }
            // Other control units are not the publisher's to send; ignored for compatibility.
            Content::Control(_) => true,
        }
    }

    /// A unit from a subscriber (only control units get this far). `false` means the link broke
    /// the protocol.
    pub fn subscriber_unit(&self, from: &Subscriber, unit: Unit, now: Instant) -> bool {
        match unit.header.content {
            Content::Control(ControlKind::KeyRequest) => {
                if unit.header.payload_len > MAX_KEY_BODY {
                    return false;
                }
                self.to_publisher
                    .push(Unit::key_request(from.slot, unit.payload()));
                true
            }
            Content::Control(ControlKind::KeyframeRequest) => {
                self.request_keyframe(Some(from), now);
                true
            }
            Content::Control(ControlKind::Layers) => {
                let layer = unit.payload().first().copied().unwrap_or(0);
                from.queue.with(|policy| policy.set_wanted_layer(layer));
                true
            }
            Content::Control(_) => true,
            Content::Video { .. } | Content::Audio => false,
        }
    }
}

/// The receiving half of a broadcast link.
enum Rx {
    Quic(Connection),
    Tcp(ReadHalf<TlsStream>, Box<[u8; MAX_FRAME]>),
}

impl Rx {
    /// Reads units until the link closes or breaks the format. `accept` sees every header
    /// before its payload is read; `on_unit` gets every complete unit. Either returning `false`
    /// ends the link.
    async fn run(
        &mut self,
        accept: impl Fn(&Header) -> bool,
        mut on_unit: impl FnMut(Unit) -> bool,
    ) {
        match self {
            Self::Quic(conn) => loop {
                tokio::select! {
                    stream = conn.accept_uni() => {
                        let Ok(mut stream) = stream else { return };
                        let Some(unit) = read_stream(&mut stream, &accept).await else {
                            return;
                        };
                        if !on_unit(unit) {
                            return;
                        }
                    }
                    datagram = conn.read_datagram() => {
                        let Ok(datagram) = datagram else { return };
                        let unit = Unit::parse(datagram)
                            .ok()
                            .filter(|u| matches!(u.header.content, Content::Control(_)))
                            .filter(|u| accept(&u.header));
                        if !unit.is_some_and(&mut on_unit) {
                            return;
                        }
                    }
                }
            },
            Self::Tcp(rd, buf) => {
                let mut assembler = Assembler::new();
                loop {
                    let Ok((kind, len)) = tcp::read_frame(rd, buf).await else {
                        return;
                    };
                    match kind {
                        FRAME_PACKET => {
                            if assembler.feed(&buf[..len], &accept, &mut on_unit).is_err() {
                                return;
                            }
                        }
                        // `bind` is the only client control message; later ones are ignored.
                        FRAME_CONTROL => {}
                        _ => return,
                    }
                }
            }
        }
    }
}

/// One unit from its own stream: header, payload, then the end of the stream.
async fn read_stream(stream: &mut RecvStream, accept: impl Fn(&Header) -> bool) -> Option<Unit> {
    let mut head = [0u8; HEADER_LEN];
    stream.read_exact(&mut head).await.ok()?;
    let header = Header::decode(&head).ok().filter(|h| accept(h))?;
    let len = header.unit_len();
    let mut bytes = BytesMut::with_capacity(len);
    bytes.put_slice(&head);
    while bytes.len() < len {
        let chunk = stream.read_chunk(len - bytes.len(), true).await.ok()??;
        bytes.put_slice(&chunk.bytes);
    }
    // More than the header announced is a broken unit.
    match stream.read_chunk(1, true).await {
        Ok(None) => Some(Unit {
            header,
            bytes: bytes.freeze(),
        }),
        _ => None,
    }
}

/// The sending half of a broadcast link.
enum Tx {
    Quic { conn: Connection, ctl: SendStream },
    Tcp(WriteHalf<TlsStream>),
}

impl Tx {
    async fn control(&mut self, message: &str) -> bool {
        match self {
            Self::Quic { ctl, .. } => {
                let line = format!("{message}\n");
                ctl.write_all(line.as_bytes()).await.is_ok()
            }
            Self::Tcp(wr) => tcp::write_frame(wr, FRAME_CONTROL, message.as_bytes())
                .await
                .is_ok(),
        }
    }

    async fn unit(&mut self, bytes: Bytes) -> bool {
        match self {
            Self::Quic { conn, .. } => {
                let Ok(mut stream) = conn.open_uni().await else {
                    return false;
                };
                stream.write_chunk(bytes).await.is_ok() && stream.finish().is_ok()
            }
            Self::Tcp(wr) => {
                for chunk in bytes.chunks(MAX_FRAME) {
                    if tcp::write_frame(wr, FRAME_PACKET, chunk).await.is_err() {
                        return false;
                    }
                }
                true
            }
        }
    }

    /// Tells the client why the link ends, then closes it.
    async fn close(mut self, reason: &str) {
        let message = format!(r#"{{"v":1,"t":"end","reason":"{reason}"}}"#);
        let _ = timeout(GOODBYE_TIMEOUT, self.control(&message)).await;
        match self {
            Self::Quic { conn, mut ctl } => {
                // Closing the connection discards unsent stream data, so give the message a
                // moment to arrive; the close reason carries it as well.
                if ctl.finish().is_ok() {
                    let _ = timeout(GOODBYE_TIMEOUT, ctl.stopped()).await;
                }
                conn.close(0u32.into(), reason.as_bytes());
            }
            Self::Tcp(mut wr) => {
                let _ = timeout(GOODBYE_TIMEOUT, wr.shutdown()).await;
            }
        }
    }
}

fn split(link: Link) -> (Rx, Tx) {
    match link {
        Link::Quic { conn, ctl } => (Rx::Quic(conn.clone()), Tx::Quic { conn, ctl }),
        Link::Tcp { rd, wr } => (Rx::Tcp(rd, Box::new([0u8; MAX_FRAME])), Tx::Tcp(wr)),
    }
}

/// How a broadcast or one subscriber's link ended, as sent in `end` and logged.
fn reason(end: Option<BroadcastEnd>) -> &'static str {
    match end {
        None | Some(BroadcastEnd::Closed) => "closed",
        Some(BroadcastEnd::Deleted) => "deleted",
        Some(BroadcastEnd::Revoked) => "revoked",
        Some(BroadcastEnd::Expired) => "expired",
        Some(BroadcastEnd::Idle) => "idle",
    }
}

/// Waits for the registry to end this link; a dropped sender means the entry is gone.
async fn ended(end: &mut watch::Receiver<Option<BroadcastEnd>>) -> BroadcastEnd {
    loop {
        if let Some(reason) = *end.borrow_and_update() {
            return reason;
        }
        if end.changed().await.is_err() {
            return BroadcastEnd::Deleted;
        }
    }
}

/// Runs the publisher's link until the broadcast ends; then the whole broadcast ends with it.
pub async fn run_publisher(
    relay: &Relay,
    link: Link,
    id: BroadcastId,
    session: Arc<Session>,
    mut end: watch::Receiver<Option<BroadcastEnd>>,
) {
    let started = Instant::now();
    let (mut rx, mut tx) = split(link);
    let last_unit = AtomicU64::new(0);
    let since_start =
        |now: Instant| u64::try_from(now.duration_since(started).as_millis()).unwrap_or(u64::MAX);

    let outcome = if tx.control(READY).await {
        tokio::select! {
            () = rx.run(|_| true, |unit| {
                let now = Instant::now();
                last_unit.store(since_start(now), Ordering::Relaxed);
                session.publisher_unit(unit, &relay.total, now)
            }) => BroadcastEnd::Closed,
            () = async {
                loop {
                    let unit = session.to_publisher.next().await;
                    if !tx.unit(unit.bytes).await {
                        return;
                    }
                }
            } => BroadcastEnd::Closed,
            reason = ended(&mut end) => reason,
            () = async {
                let idle = relay.idle_timeout();
                let mut tick = tokio::time::interval(Duration::from_secs(1));
                loop {
                    tick.tick().await;
                    let quiet = since_start(Instant::now())
                        .saturating_sub(last_unit.load(Ordering::Relaxed));
                    if Duration::from_millis(quiet) >= idle {
                        return;
                    }
                }
            } => BroadcastEnd::Idle,
        }
    } else {
        BroadcastEnd::Closed
    };
    relay.broadcasts.end(id, outcome);
    tx.close(reason(Some(outcome))).await;
    tracing::info!(
        broadcast = %id,
        viewers = session.peak_viewers.load(Ordering::Relaxed),
        duration_s = started.elapsed().as_secs(),
        bytes_in = session.bytes_in.load(Ordering::Relaxed),
        bytes_out = session.bytes_out.load(Ordering::Relaxed),
        reason = reason(Some(outcome)),
        "broadcast ended"
    );
}

/// Runs one subscriber's link until it leaves, is removed or the broadcast ends.
pub async fn run_subscriber(
    relay: &Relay,
    link: Link,
    id: BroadcastId,
    session: Arc<Session>,
    subscriber: Arc<Subscriber>,
    mut end: watch::Receiver<Option<BroadcastEnd>>,
    leave: impl FnOnce(),
) {
    let (mut rx, mut tx) = split(link);
    let outcome = if tx.control(READY).await {
        tokio::select! {
            () = rx.run(
                |header| matches!(header.content, Content::Control(_)),
                |unit| session.subscriber_unit(&subscriber, unit, Instant::now()),
            ) => BroadcastEnd::Closed,
            () = send_to_subscriber(relay, &session, &subscriber, &mut tx) => BroadcastEnd::Closed,
            reason = ended(&mut end) => reason,
        }
    } else {
        BroadcastEnd::Closed
    };
    session.remove(subscriber.slot);
    leave();
    tx.close(reason(Some(outcome))).await;
    tracing::debug!(broadcast = %id, reason = reason(Some(outcome)), "subscriber left");
}

/// Sends queued units, paced by the broadcast's outbound limit and the relay's total: while a
/// subscriber waits for its share of the bandwidth its queue grows, which costs it temporal
/// layers first. Waiting instead of dropping keeps a keyframe burst to many subscribers whole.
async fn send_to_subscriber(
    relay: &Relay,
    session: &Session,
    subscriber: &Subscriber,
    tx: &mut Tx,
) {
    loop {
        let unit = subscriber.queue.next().await;
        let len = unit.bytes.len();
        let media = !matches!(unit.header.content, Content::Control(_));
        if media {
            for bucket in [&session.outbound, &relay.total] {
                loop {
                    let spent = lock(bucket).spend(len, Instant::now());
                    match spent {
                        Ok(()) => break,
                        Err(wait) => tokio::time::sleep(wait).await,
                    }
                }
            }
        }
        if !tx.unit(unit.bytes).await {
            return;
        }
        session.bytes_out.fetch_add(len as u64, Ordering::Relaxed);
        relay.count_bytes(len);
    }
}

#[cfg(test)]
mod tests {
    use super::super::control::{HEADER_LEN, VideoCodec};
    use super::*;

    fn video(keyframe: bool, layer: u8) -> Unit {
        let header = Header {
            content: Content::Video {
                codec: VideoCodec::Vp9,
                keyframe,
                temporal_layer: layer,
            },
            key_id: 1,
            sequence: 1,
            payload_len: 4,
        };
        let mut bytes = header.encode().to_vec();
        bytes.extend_from_slice(&[9; 4]);
        Unit::parse(Bytes::from(bytes)).unwrap()
    }

    fn control(kind: ControlKind, payload: &[u8]) -> Unit {
        let header = Header {
            content: Content::Control(kind),
            key_id: 0,
            sequence: 0,
            payload_len: u32::try_from(payload.len()).unwrap(),
        };
        let mut bytes = header.encode().to_vec();
        bytes.extend_from_slice(payload);
        Unit::parse(Bytes::from(bytes)).unwrap()
    }

    fn pop(queue: &Queue) -> Option<Unit> {
        queue.with(|policy| policy.pop())
    }

    fn pop_publisher(session: &Session) -> Option<Unit> {
        session.to_publisher.try_pop()
    }

    fn total() -> Mutex<TokenBucket> {
        Mutex::new(TokenBucket::new(mbit_to_bytes(200)))
    }

    #[test]
    fn media_reaches_every_subscriber_once() {
        let session = Session::new(15, 300);
        let a = session.add(0);
        let b = session.add(1);
        let now = Instant::now();
        assert!(session.publisher_unit(video(true, 0), &total(), now));
        let got_a = pop(&a.queue).unwrap();
        let got_b = pop(&b.queue).unwrap();
        // Both hold the same bytes, not copies.
        assert_eq!(got_a.bytes.as_ptr(), got_b.bytes.as_ptr());
        assert_eq!(
            session.bytes_in.load(Ordering::Relaxed),
            (HEADER_LEN + 4) as u64
        );
    }

    #[test]
    fn key_exchange_is_routed_by_slot() {
        let session = Session::new(15, 300);
        let a = session.add(3);
        let b = session.add(4);
        let now = Instant::now();
        assert!(session.subscriber_unit(&b, control(ControlKind::KeyRequest, b"hello"), now));
        let request = pop_publisher(&session).unwrap();
        assert_eq!(request.payload(), b"\x00\x04hello");

        assert!(session.publisher_unit(
            control(ControlKind::Key, b"\x00\x04sealed"),
            &total(),
            now
        ));
        assert!(pop(&a.queue).is_none());
        assert_eq!(pop(&b.queue).unwrap().payload(), b"sealed");
        // Unknown slots are ignored; a key without a slot breaks the protocol.
        assert!(session.publisher_unit(control(ControlKind::Key, b"\x00\x09x"), &total(), now));
        assert!(!session.publisher_unit(control(ControlKind::Key, b""), &total(), now));
        // Oversized key requests and media from a subscriber end its link.
        assert!(!session.subscriber_unit(&a, control(ControlKind::KeyRequest, &[0; 1025]), now));
        assert!(!session.subscriber_unit(&a, video(true, 0), now));
    }

    #[test]
    fn keyframe_requests_are_rate_limited() {
        let session = Session::new(15, 300);
        let a = session.add(0);
        let b = session.add(1);
        let start = Instant::now();
        let kf = || control(ControlKind::KeyframeRequest, &[]);
        let count = |session: &Session| {
            let mut n = 0;
            while pop_publisher(session).is_some() {
                n += 1;
            }
            n
        };
        session.subscriber_unit(&a, kf(), start);
        session.subscriber_unit(&b, kf(), start + Duration::from_millis(100));
        assert_eq!(count(&session), 1, "one per 500 ms per broadcast");
        session.subscriber_unit(&b, kf(), start + Duration::from_millis(600));
        session.subscriber_unit(&a, kf(), start + Duration::from_millis(1200));
        assert_eq!(count(&session), 1, "one per 2 s per subscriber");
        session.subscriber_unit(&a, kf(), start + Duration::from_millis(2100));
        assert_eq!(count(&session), 1);
    }

    #[test]
    fn layers_and_gone() {
        let session = Session::new(15, 300);
        let a = session.add(5);
        let now = Instant::now();
        session.subscriber_unit(&a, control(ControlKind::Layers, &[0]), now);
        session.publisher_unit(video(true, 0), &total(), now);
        session.publisher_unit(video(false, 1), &total(), now);
        assert!(pop(&a.queue).is_some());
        assert!(pop(&a.queue).is_none());

        session.remove(5);
        assert_eq!(pop_publisher(&session).unwrap().payload(), &[0, 5]);
        session.remove(5);
        assert!(pop_publisher(&session).is_none());
        assert_eq!(session.viewers(), 0);
    }

    #[test]
    fn inbound_over_the_limit_drops_and_resyncs() {
        let session = Session::new(1, 300);
        let a = session.add(0);
        let now = Instant::now();
        let big = |keyframe| {
            let header = Header {
                content: Content::Video {
                    codec: VideoCodec::Av1,
                    keyframe,
                    temporal_layer: 0,
                },
                key_id: 1,
                sequence: 1,
                payload_len: 200_000,
            };
            let mut bytes = header.encode().to_vec();
            bytes.resize(header.unit_len(), 0);
            Unit::parse(Bytes::from(bytes)).unwrap()
        };
        assert!(session.publisher_unit(big(true), &total(), now));
        assert!(session.publisher_unit(big(false), &total(), now));
        assert_eq!(pop(&a.queue).map(|u| u.bytes.len()), Some(200_014));
        assert!(pop(&a.queue).is_none());
        assert!(pop_publisher(&session).is_some(), "asks for a keyframe");
    }
}
