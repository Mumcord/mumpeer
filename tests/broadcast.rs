//! End-to-end checks of the broadcast data plane on loopback (spec 07 FR-7.41, FR-7.45,
//! FR-7.50): one publisher, many subscribers over QUIC and TCP, the control flow between them and
//! how a broadcast ends.

// Test helpers panic on any unexpected error; that is the failure report.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mumpeer::Running;
use mumpeer::config::{ApiSecret, Config};
use quinn::{Connection, RecvStream, SendStream};
use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::time::timeout;

const SECRET: &str = "test-secret-test-secret-test-secret";
const WAIT: Duration = Duration::from_secs(10);
const HEADER_LEN: usize = 14;

type TlsClient = tokio_rustls::client::TlsStream<TcpStream>;

struct Relay {
    running: Running,
    _dir: tempfile::TempDir,
}

/// A relay on loopback; `extra` is appended to the config (limits first, then other tables).
async fn relay(limits: &str, extra: &str) -> Relay {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::parse(&format!(
        "public_host = \"127.0.0.1\"\nudp_listen = \"127.0.0.1:0\"\ntcp_listen = \"127.0.0.1:0\"\n\
         api_listen = \"127.0.0.1:0\"\ndata_dir = \"{}\"\n[limits]\n{limits}\n{extra}",
        dir.path().display()
    ))
    .unwrap();
    let running = mumpeer::start(config, ApiSecret::new(SECRET).unwrap())
        .await
        .unwrap();
    Relay { running, _dir: dir }
}

struct Identity {
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    hash: String,
}

fn identity() -> Identity {
    let certified = rcgen::generate_simple_self_signed(vec!["user".to_owned()]).unwrap();
    let der = certified.cert.der().clone();
    let digest = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, der.as_ref());
    let hash = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    Identity {
        chain: vec![der],
        key: PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into()),
        hash,
    }
}

/// Accepts exactly the relay certificate with the given SHA-256 fingerprint.
#[derive(Debug)]
struct Pinned(String);

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let digest = ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref());
        let fp: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        if fp == self.0 {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("fingerprint mismatch".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 not used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn client_tls(fp: &str, id: Option<&Identity>) -> rustls::ClientConfig {
    let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(Pinned(fp.to_owned())));
    let mut config = match id {
        Some(id) => builder
            .with_client_auth_cert(id.chain.clone(), id.key.clone_key())
            .unwrap(),
        None => builder.with_no_client_auth(),
    };
    config.alpn_protocols = vec![b"mumpeer/1".to_vec()];
    config
}

/// HTTP/1.1 request against the API; returns status and body.
async fn api(relay: &Relay, method: &str, path: &str, body: &str, secret: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(relay.running.api_addr).await.unwrap();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: relay\r\nAuthorization: Bearer {secret}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status = response[9..12].parse().unwrap();
    let body = response
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    (status, body)
}

async fn create(relay: &Relay, publisher: &Identity, max: u16) -> (String, String) {
    let body = format!(
        r#"{{"publisher":"{}","max_viewers":{max}}}"#,
        publisher.hash
    );
    let (status, body) = api(relay, "POST", "/v1/broadcasts", &body, SECRET).await;
    assert_eq!(status, 201, "{body}");
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    (
        body["id"].as_str().unwrap().to_owned(),
        body["tok"].as_str().unwrap().to_owned(),
    )
}

async fn admit(relay: &Relay, id: &str, viewer: &Identity) -> String {
    let body = format!(r#"{{"viewer":"{}"}}"#, viewer.hash);
    let path = format!("/v1/broadcasts/{id}/viewers");
    let (status, body) = api(relay, "POST", &path, &body, SECRET).await;
    assert_eq!(status, 201, "{body}");
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    body["tok"].as_str().unwrap().to_owned()
}

fn bind(token: &str, role: Option<&str>) -> String {
    match role {
        Some(role) => format!(r#"{{"v":1,"t":"bind","tok":"{token}","role":"{role}"}}"#),
        None => format!(r#"{{"v":1,"t":"bind","tok":"{token}"}}"#),
    }
}

fn header(kind: u8, flags: u8, layer: u8, codec: u8, key: u8, seq: u32, len: usize) -> Vec<u8> {
    let mut out = vec![1, kind, flags, layer, codec, key];
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&u32::try_from(len).unwrap().to_be_bytes());
    out
}

/// An AV1 frame whose payload repeats its sequence number, so copies can be compared.
fn video(keyframe: bool, layer: u8, seq: u32, len: usize) -> Vec<u8> {
    let mut unit = header(1, u8::from(keyframe), layer, 2, 1, seq, len);
    unit.extend(std::iter::repeat_n(seq as u8, len));
    unit
}

fn audio(seq: u32) -> Vec<u8> {
    let mut unit = header(2, 0, 0, 3, 1, seq, 80);
    unit.extend([7u8; 80]);
    unit
}

fn control(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut unit = header(3, 0, 0, kind, 0, 0, payload.len());
    unit.extend_from_slice(payload);
    unit
}

const KEYREQ: u8 = 1;
const KEY: u8 = 2;
const KF: u8 = 3;
const LAYERS: u8 = 4;
const GONE: u8 = 5;

struct Quic {
    conn: Connection,
    _send: SendStream,
    recv: RecvStream,
}

async fn quic_bind(relay: &Relay, id: &Identity, bind: &str) -> Quic {
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(client_tls(
        &relay.running.fingerprint,
        Some(id),
    ))
    .unwrap();
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    let mut transport = quinn::TransportConfig::default();
    transport.initial_mtu(1300);
    config.transport_config(Arc::new(transport));
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(config);
    let conn = endpoint
        .connect(relay.running.udp_addr, "relay")
        .unwrap()
        .await
        .unwrap();
    let (mut send, recv) = conn.open_bi().await.unwrap();
    send.write_all(format!("{bind}\n").as_bytes())
        .await
        .unwrap();
    Quic {
        conn,
        _send: send,
        recv,
    }
}

impl Quic {
    async fn control(&mut self) -> serde_json::Value {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while timeout(WAIT, self.recv.read_exact(&mut byte))
            .await
            .unwrap()
            .is_ok()
            && byte[0] != b'\n'
        {
            line.push(byte[0]);
        }
        serde_json::from_slice(&line).unwrap()
    }

    async fn send(&self, unit: &[u8]) {
        let mut stream = self.conn.open_uni().await.unwrap();
        stream.write_all(unit).await.unwrap();
        stream.finish().unwrap();
    }

    async fn recv(&self) -> Vec<u8> {
        let mut stream = timeout(WAIT, self.conn.accept_uni())
            .await
            .expect("a unit in time")
            .unwrap();
        stream.read_to_end(5 << 20).await.unwrap()
    }
}

struct Tcp {
    rd: ReadHalf<TlsClient>,
    wr: WriteHalf<TlsClient>,
    pending: Vec<u8>,
}

async fn tcp_bind(relay: &Relay, id: &Identity, bind: &str) -> Tcp {
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_tls(
        &relay.running.fingerprint,
        Some(id),
    )));
    let stream = TcpStream::connect(relay.running.tcp_addr).await.unwrap();
    let stream = connector
        .connect(ServerName::try_from("relay").unwrap(), stream)
        .await
        .unwrap();
    let (rd, wr) = tokio::io::split(stream);
    let mut tcp = Tcp {
        rd,
        wr,
        pending: Vec::new(),
    };
    tcp.frame(0, bind.as_bytes()).await;
    tcp
}

impl Tcp {
    async fn frame(&mut self, kind: u8, payload: &[u8]) {
        let mut frame = vec![kind];
        frame.extend_from_slice(&u16::try_from(payload.len()).unwrap().to_be_bytes());
        frame.extend_from_slice(payload);
        self.wr.write_all(&frame).await.unwrap();
        self.wr.flush().await.unwrap();
    }

    /// The next frame; `None` once the relay closed the link.
    async fn next_frame(&mut self) -> Option<(u8, Vec<u8>)> {
        let mut head = [0u8; 3];
        timeout(WAIT, self.rd.read_exact(&mut head))
            .await
            .expect("a frame in time")
            .ok()?;
        let mut payload = vec![0u8; usize::from(u16::from_be_bytes([head[1], head[2]]))];
        self.rd.read_exact(&mut payload).await.ok()?;
        Some((head[0], payload))
    }

    async fn control(&mut self) -> serde_json::Value {
        let (kind, payload) = self.next_frame().await.expect("control frame");
        assert_eq!(kind, 0);
        serde_json::from_slice(&payload).unwrap()
    }

    /// A unit cut into frames of at most 1500 bytes, as the protocol allows.
    async fn send(&mut self, unit: &[u8]) {
        for chunk in unit.chunks(1500) {
            self.frame(1, chunk).await;
        }
    }

    async fn recv(&mut self) -> Vec<u8> {
        loop {
            if self.pending.len() >= HEADER_LEN {
                let len = HEADER_LEN
                    + usize::try_from(u32::from_be_bytes(self.pending[10..14].try_into().unwrap()))
                        .unwrap();
                if self.pending.len() >= len {
                    return self.pending.drain(..len).collect();
                }
            }
            let (kind, payload) = self.next_frame().await.expect("unit frame");
            assert_eq!(kind, 1, "{}", String::from_utf8_lossy(&payload));
            self.pending.extend_from_slice(&payload);
        }
    }
}

fn kind_of(unit: &[u8]) -> (u8, u8) {
    (unit[1], unit[4])
}

fn payload(unit: &[u8]) -> &[u8] {
    &unit[HEADER_LEN..]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fans_out_to_25_subscribers_over_quic_and_tcp() {
    let relay = relay("conn_per_ip_per_min = 1000", "").await;
    let publisher = identity();
    let (id, tok) = create(&relay, &publisher, 25).await;
    let mut quic_subs = Vec::new();
    let mut tcp_subs = Vec::new();
    for n in 0..25 {
        let viewer = identity();
        let token = admit(&relay, &id, &viewer).await;
        if n % 5 == 0 {
            let mut sub = tcp_bind(&relay, &viewer, &bind(&token, Some("sub"))).await;
            assert_eq!(sub.control().await["t"], "ready");
            tcp_subs.push(sub);
        } else {
            let mut sub = quic_bind(&relay, &viewer, &bind(&token, Some("sub"))).await;
            assert_eq!(sub.control().await["t"], "ready");
            quic_subs.push(sub);
        }
    }
    let mut publisher = quic_bind(&relay, &publisher, &bind(&tok, Some("pub"))).await;
    assert_eq!(publisher.control().await["t"], "ready");

    let units = [
        video(true, 0, 1, 100_000),
        video(false, 1, 2, 3000),
        audio(3),
    ];
    // A delta frame before any keyframe reaches nobody.
    publisher.send(&video(false, 0, 0, 500)).await;
    for unit in &units {
        publisher.send(unit).await;
    }
    let expected: BTreeSet<Vec<u8>> = units.iter().cloned().collect();
    for sub in &quic_subs {
        let mut got = BTreeSet::new();
        for _ in 0..3 {
            got.insert(sub.recv().await);
        }
        assert_eq!(got, expected);
    }
    for sub in &mut tcp_subs {
        let mut got = BTreeSet::new();
        for _ in 0..3 {
            got.insert(sub.recv().await);
        }
        assert_eq!(got, expected);
    }
}

#[tokio::test]
async fn key_exchange_keyframe_requests_layers_and_gone() {
    let relay = relay("", "").await;
    let (publisher, a, b) = (identity(), identity(), identity());
    let (id, tok) = create(&relay, &publisher, 25).await;
    let a_tok = admit(&relay, &id, &a).await;
    let b_tok = admit(&relay, &id, &b).await;
    let mut publisher = quic_bind(&relay, &publisher, &bind(&tok, Some("pub"))).await;
    assert_eq!(publisher.control().await["t"], "ready");
    let mut a = quic_bind(&relay, &a, &bind(&a_tok, Some("sub"))).await;
    assert_eq!(a.control().await["t"], "ready");
    let mut b = tcp_bind(&relay, &b, &bind(&b_tok, Some("sub"))).await;
    assert_eq!(b.control().await["t"], "ready");

    // keyreq: the relay puts the subscriber's slot in front.
    b.send(&control(KEYREQ, b"b-public-key")).await;
    let request = publisher.recv().await;
    assert_eq!(kind_of(&request), (3, KEYREQ));
    let (b_slot, body) = payload(&request).split_at(2);
    assert_eq!(body, b"b-public-key");

    // key: routed to that slot only, slot stripped.
    let mut key = b_slot.to_vec();
    key.extend_from_slice(b"sealed-key");
    publisher.send(&control(KEY, &key)).await;
    let delivered = b.recv().await;
    assert_eq!(kind_of(&delivered), (3, KEY));
    assert_eq!(payload(&delivered), b"sealed-key");

    // kf as a datagram is forwarded; a second one right after is rate-limited.
    a.conn.send_datagram(control(KF, &[]).into()).unwrap();
    assert_eq!(kind_of(&publisher.recv().await), (3, KF));
    a.send(&control(KF, &[])).await;

    // layers 0: A gets only the base layer.
    a.send(&control(LAYERS, &[0])).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    publisher.send(&video(true, 0, 1, 2000)).await;
    publisher.send(&video(false, 2, 2, 500)).await;
    publisher.send(&video(false, 0, 3, 500)).await;
    assert_eq!(a.recv().await, video(true, 0, 1, 2000));
    assert_eq!(a.recv().await, video(false, 0, 3, 500));
    for seq in 1..=3u32 {
        assert_eq!(&b.recv().await[6..10], &seq.to_be_bytes());
    }

    // A leaves: the publisher learns its slot (and no second kf came before).
    a.conn.close(0u32.into(), b"bye");
    let gone = publisher.recv().await;
    assert_eq!(kind_of(&gone), (3, GONE));
    assert_ne!(payload(&gone), b_slot);
}

#[tokio::test]
async fn deleting_the_broadcast_ends_every_link() {
    let relay = relay("", "").await;
    let (publisher, viewer) = (identity(), identity());
    let (id, tok) = create(&relay, &publisher, 25).await;
    let v_tok = admit(&relay, &id, &viewer).await;
    let mut publisher = quic_bind(&relay, &publisher, &bind(&tok, Some("pub"))).await;
    assert_eq!(publisher.control().await["t"], "ready");
    let mut sub = tcp_bind(&relay, &viewer, &bind(&v_tok, Some("sub"))).await;
    assert_eq!(sub.control().await["t"], "ready");

    let (status, _) = api(
        &relay,
        "DELETE",
        &format!("/v1/broadcasts/{id}"),
        "",
        SECRET,
    )
    .await;
    assert_eq!(status, 204);
    let end = sub.control().await;
    assert_eq!(
        (end["t"].as_str(), end["reason"].as_str()),
        (Some("end"), Some("deleted"))
    );
    assert!(sub.next_frame().await.is_none());
    let end = publisher.control().await;
    assert_eq!(end["reason"], "deleted");
}

#[tokio::test]
async fn removing_a_viewer_closes_it_and_tells_the_publisher() {
    let relay = relay("", "").await;
    let (publisher, viewer) = (identity(), identity());
    let (id, tok) = create(&relay, &publisher, 25).await;
    let v_tok = admit(&relay, &id, &viewer).await;
    let publisher_link = quic_bind(&relay, &publisher, &bind(&tok, Some("pub"))).await;
    let mut sub = quic_bind(&relay, &viewer, &bind(&v_tok, Some("sub"))).await;
    assert_eq!(sub.control().await["t"], "ready");

    let path = format!("/v1/broadcasts/{id}/viewers/{}", viewer.hash);
    assert_eq!(api(&relay, "DELETE", &path, "", SECRET).await.0, 204);
    assert_eq!(sub.control().await["reason"], "deleted");
    assert_eq!(kind_of(&publisher_link.recv().await), (3, GONE));
}

#[tokio::test]
async fn a_silent_or_leaving_publisher_ends_the_broadcast() {
    let relay = relay("idle_timeout_s = 1", "").await;
    let (publisher, viewer) = (identity(), identity());

    let (id, tok) = create(&relay, &publisher, 25).await;
    let v_tok = admit(&relay, &id, &viewer).await;
    let mut publisher_link = quic_bind(&relay, &publisher, &bind(&tok, Some("pub"))).await;
    assert_eq!(publisher_link.control().await["t"], "ready");
    let mut sub = quic_bind(&relay, &viewer, &bind(&v_tok, Some("sub"))).await;
    assert_eq!(sub.control().await["t"], "ready");
    let started = Instant::now();
    assert_eq!(sub.control().await["reason"], "idle");
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(publisher_link.control().await["reason"], "idle");

    let (id, tok) = create(&relay, &publisher, 25).await;
    let v_tok = admit(&relay, &id, &viewer).await;
    let publisher_link = quic_bind(&relay, &publisher, &bind(&tok, Some("pub"))).await;
    let mut sub = tcp_bind(&relay, &viewer, &bind(&v_tok, Some("sub"))).await;
    assert_eq!(sub.control().await["t"], "ready");
    publisher_link.conn.close(0u32.into(), b"stop");
    assert_eq!(sub.control().await["reason"], "closed");
}

async fn refused_quic(relay: &Relay, id: &Identity, bind: &str) -> String {
    let mut link = quic_bind(relay, id, bind).await;
    let reply = link.control().await;
    assert_eq!(reply["t"], "error");
    reply["code"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn binds_are_checked() {
    let relay = relay("", "").await;
    let (publisher, viewer, other) = (identity(), identity(), identity());
    let (id, tok) = create(&relay, &publisher, 2).await;
    let v_tok = admit(&relay, &id, &viewer).await;
    admit(&relay, &id, &other).await;

    // A pair-less bind with a role but an unknown token, a wrong role, the wrong certificate.
    let unknown = "A".repeat(43);
    assert_eq!(
        refused_quic(&relay, &viewer, &bind(&unknown, Some("sub"))).await,
        "unknown_token"
    );
    assert_eq!(
        refused_quic(&relay, &viewer, &bind(&v_tok, Some("viewer"))).await,
        "bad_request"
    );
    assert_eq!(
        refused_quic(&relay, &viewer, &bind(&v_tok, Some("pub"))).await,
        "unknown_token"
    );
    assert_eq!(
        refused_quic(&relay, &publisher, &bind(&v_tok, Some("sub"))).await,
        "not_allowed"
    );
    assert_eq!(
        refused_quic(&relay, &viewer, &bind(&tok, Some("pub"))).await,
        "not_allowed"
    );

    // Each side binds once.
    let mut first = quic_bind(&relay, &publisher, &bind(&tok, Some("pub"))).await;
    assert_eq!(first.control().await["t"], "ready");
    assert_eq!(
        refused_quic(&relay, &publisher, &bind(&tok, Some("pub"))).await,
        "not_allowed"
    );

    // A subscriber sending media breaks the protocol and is closed.
    let mut sub = quic_bind(&relay, &viewer, &bind(&v_tok, Some("sub"))).await;
    assert_eq!(sub.control().await["t"], "ready");
    sub.send(&video(true, 0, 1, 10)).await;
    assert_eq!(sub.control().await["reason"], "closed");
}

#[tokio::test]
async fn broadcast_binds_need_screen_sharing_allowed() {
    let relay = relay("", "[uses]\nscreen_share = false").await;
    let (a, b) = (identity(), identity());
    let body = format!(r#"{{"a":"{}","b":"{}","use":"files"}}"#, a.hash, b.hash);
    let (status, body) = api(&relay, "POST", "/v1/pairs", &body, SECRET).await;
    assert_eq!(status, 201, "{body}");
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    let token = body["tok"].as_str().unwrap();
    assert_eq!(
        refused_quic(&relay, &a, &bind(token, Some("sub"))).await,
        "use_not_allowed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stuck_subscriber_does_not_hold_up_the_others() {
    let relay = relay(
        "broadcast_in_mbit = 10000\nbroadcast_out_mbit = 10000\ntotal_mbit = 100000",
        "",
    )
    .await;
    let (publisher, stuck, fast) = (identity(), identity(), identity());
    let (id, tok) = create(&relay, &publisher, 25).await;
    let stuck_tok = admit(&relay, &id, &stuck).await;
    let fast_tok = admit(&relay, &id, &fast).await;
    let mut publisher = quic_bind(&relay, &publisher, &bind(&tok, Some("pub"))).await;
    assert_eq!(publisher.control().await["t"], "ready");
    // Bound over TCP and then never read again: its socket fills up.
    let mut stuck = tcp_bind(&relay, &stuck, &bind(&stuck_tok, Some("sub"))).await;
    assert_eq!(stuck.control().await["t"], "ready");
    let fast = quic_bind(&relay, &fast, &bind(&fast_tok, Some("sub"))).await;

    let reader = tokio::spawn(async move {
        let mut fast = fast;
        assert_eq!(fast.control().await["t"], "ready");
        let mut seqs = Vec::new();
        loop {
            let unit = fast.recv().await;
            let seq = u32::from_be_bytes(unit[6..10].try_into().unwrap());
            seqs.push(seq);
            if seq == 1000 {
                return seqs;
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut keyreqs = 0;
    for seq in 1..=200u32 {
        publisher.send(&video(seq == 1, 0, seq, 100_000)).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        // Overflow of the stuck subscriber asks for keyframes now and then.
        while let Ok(Ok(stream)) =
            timeout(Duration::from_millis(0), publisher.conn.accept_uni()).await
        {
            let mut stream = stream;
            let unit = stream.read_to_end(4096).await.unwrap();
            keyreqs += usize::from(kind_of(&unit) == (3, KF));
        }
    }
    publisher.send(&video(true, 0, 1000, 1000)).await;
    let seqs = timeout(WAIT, reader).await.unwrap().unwrap();
    assert!(
        seqs.len() > 150,
        "the fast subscriber got {} of 201",
        seqs.len()
    );
    assert!(
        keyreqs >= 1,
        "the stuck subscriber's overflow asked for a keyframe"
    );
}

/// Perf smoke for the spec target (25 × 6 Mbit/s): prints the process CPU share, which includes
/// the 26 test clients. Run with `cargo test --release --test broadcast -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "perf measurement, run by hand"]
async fn perf_25_viewers_at_6_mbit() {
    let relay = relay("conn_per_ip_per_min = 1000", "").await;
    let publisher = identity();
    let (id, tok) = create(&relay, &publisher, 25).await;
    let mut subs = Vec::new();
    for _ in 0..25 {
        let viewer = identity();
        let token = admit(&relay, &id, &viewer).await;
        let mut sub = quic_bind(&relay, &viewer, &bind(&token, Some("sub"))).await;
        assert_eq!(sub.control().await["t"], "ready");
        subs.push(tokio::spawn(async move {
            let mut bytes = 0usize;
            while let Ok(Ok(mut stream)) =
                timeout(Duration::from_secs(2), sub.conn.accept_uni()).await
            {
                bytes += stream.read_to_end(5 << 20).await.unwrap().len();
            }
            bytes
        }));
    }
    let publisher = quic_bind(&relay, &publisher, &bind(&tok, Some("pub"))).await;
    // 6 Mbit/s at 30 fps: 25 000 bytes per frame, keyframe every 2 s, L1T3 layers.
    let cpu = || {
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        let fields: Vec<&str> = stat
            .rsplit(')')
            .next()
            .unwrap()
            .split_whitespace()
            .collect();
        fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
    };
    let (cpu_start, wall_start) = (cpu(), Instant::now());
    let mut tick = tokio::time::interval(Duration::from_millis(33));
    for n in 0..300u32 {
        tick.tick().await;
        let layer = [0, 2, 1, 2][n as usize % 4];
        publisher
            .send(&video(
                n % 60 == 0,
                if n % 60 == 0 { 0 } else { layer },
                n,
                25_000,
            ))
            .await;
    }
    let ticks = cpu() - cpu_start;
    let wall = wall_start.elapsed().as_secs_f64();
    let mut total = 0;
    for sub in subs {
        total += sub.await.unwrap();
    }
    println!(
        "CPU {:.0} % of one core over {wall:.1} s (relay and clients), {:.1} Mbit/s out in total",
        ticks as f64 / 100.0 / wall * 100.0,
        total as f64 * 8.0 / wall / 1e6
    );
}
