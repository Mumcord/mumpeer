//! End-to-end checks of the plugin-data plane on loopback (spec 09 FR-9.84 … FR-9.86): `data`
//! units in plugin-data broadcasts over QUIC and TCP, the per-sender rate, unit size and broadcast
//! caps, the bind check per use, the per-sender cap of plugin-data pairs, and that plugin data
//! never takes the bandwidth screen sharing needs.

// Test helpers panic on any unexpected error; that is the failure report.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
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

/// Creates a broadcast for `use_`; returns the API status and, on success, id and token.
async fn try_create(relay: &Relay, publisher: &Identity, use_: &str) -> (u16, String) {
    let body = format!(
        r#"{{"publisher":"{}","max_viewers":8,"use":"{use_}"}}"#,
        publisher.hash
    );
    api(relay, "POST", "/v1/broadcasts", &body, SECRET).await
}

async fn create(relay: &Relay, publisher: &Identity, use_: &str) -> (String, String) {
    let (status, body) = try_create(relay, publisher, use_).await;
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

/// An Opus unit of `len` bytes, the largest sound unit being 4 KiB.
fn audio(seq: u32, len: usize) -> Vec<u8> {
    let mut unit = header(2, 0, 0, 3, 1, seq, len);
    unit.extend(std::iter::repeat_n(7u8, len));
    unit
}

/// A plugin-data unit whose payload repeats its sequence number, so copies can be compared.
fn data(seq: u32, len: usize) -> Vec<u8> {
    let mut unit = header(4, 0, 0, 0, 1, seq, len);
    unit.extend(std::iter::repeat_n(seq as u8, len));
    unit
}

fn sequence(unit: &[u8]) -> u32 {
    u32::from_be_bytes(unit[6..10].try_into().unwrap())
}

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

const PLUGIN_DATA_ON: &str = "[uses]\nplugin_data = true";

/// A bound publisher and one QUIC subscriber of a new broadcast for `use_`.
async fn bound(relay: &Relay, use_: &str) -> (Quic, Quic) {
    let (publisher, viewer) = (identity(), identity());
    let (id, tok) = create(relay, &publisher, use_).await;
    let v_tok = admit(relay, &id, &viewer).await;
    let mut publisher = quic_bind(relay, &publisher, &bind(&tok, Some("pub"))).await;
    assert_eq!(publisher.control().await["t"], "ready");
    let mut sub = quic_bind(relay, &viewer, &bind(&v_tok, Some("sub"))).await;
    assert_eq!(sub.control().await["t"], "ready");
    (publisher, sub)
}

/// Collects the sequence numbers of every unit the link receives until it closes.
fn collect(link: &Quic) -> Arc<Mutex<Vec<u32>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (conn, out) = (link.conn.clone(), seen.clone());
    tokio::spawn(async move {
        while let Ok(mut stream) = conn.accept_uni().await {
            let Ok(unit) = stream.read_to_end(2 << 20).await else {
                return;
            };
            out.lock().unwrap().push(sequence(&unit));
        }
    });
    seen
}

fn count(seen: &Mutex<Vec<u32>>, range: std::ops::Range<u32>) -> usize {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|s| range.contains(s))
        .count()
}

/// Waits until `done` holds, at most `WAIT`.
async fn eventually(done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < WAIT, "timed out");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn data_units_reach_viewers_over_quic_and_tcp_unchanged() {
    let relay = relay("", PLUGIN_DATA_ON).await;
    let (publisher, quic_viewer, tcp_viewer) = (identity(), identity(), identity());
    let (id, tok) = create(&relay, &publisher, "plugin_data").await;
    let q_tok = admit(&relay, &id, &quic_viewer).await;
    let t_tok = admit(&relay, &id, &tcp_viewer).await;
    let mut publisher = quic_bind(&relay, &publisher, &bind(&tok, Some("pub"))).await;
    assert_eq!(publisher.control().await["t"], "ready");
    let mut quic = quic_bind(&relay, &quic_viewer, &bind(&q_tok, Some("sub"))).await;
    assert_eq!(quic.control().await["t"], "ready");
    let mut tcp = tcp_bind(&relay, &tcp_viewer, &bind(&t_tok, Some("sub"))).await;
    assert_eq!(tcp.control().await["t"], "ready");

    // The largest unit the default `max_unit_bytes` allows, and small ones around it.
    let units = [data(1, 10), data(2, 65536 + 64), data(3, 1)];
    for unit in &units {
        publisher.send(unit).await;
        // The big unit uses up the sender's burst for a moment.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for unit in &units {
        assert_eq!(&quic.recv().await, unit);
        assert_eq!(&tcp.recv().await, unit);
    }
}

#[tokio::test]
async fn plugin_data_is_refused_when_switched_off() {
    let relay = relay("", "[uses]\nplugin_data = false").await;
    let (a, b) = (identity(), identity());
    assert_eq!(try_create(&relay, &a, "plugin_data").await.0, 403);
    let body = format!(
        r#"{{"a":"{}","b":"{}","use":"plugin_data"}}"#,
        a.hash, b.hash
    );
    assert_eq!(api(&relay, "POST", "/v1/pairs", &body, SECRET).await.0, 403);
}

#[tokio::test]
async fn plugin_data_binds_without_screen_sharing() {
    let relay = relay("", "[uses]\nscreen_share = false\nplugin_data = true").await;
    let viewer = identity();
    assert_eq!(try_create(&relay, &viewer, "screen_share").await.0, 403);
    let (publisher, sub) = bound(&relay, "plugin_data").await;
    publisher.send(&data(5, 100)).await;
    assert_eq!(sub.recv().await, data(5, 100));
}

#[tokio::test]
async fn plugin_data_broadcasts_have_their_own_cap() {
    let relay = relay(
        "max_broadcasts = 1",
        "[uses]\nplugin_data = true\n[plugin_data]\nmax_broadcasts = 2",
    )
    .await;
    let peer = identity();
    create(&relay, &peer, "plugin_data").await;
    create(&relay, &peer, "plugin_data").await;
    let (status, body) = try_create(&relay, &peer, "plugin_data").await;
    assert_eq!((status, body.as_str()), (503, r#"{"code":"relay_full"}"#));
    create(&relay, &peer, "screen_share").await;
    assert_eq!(try_create(&relay, &peer, "screen_share").await.0, 503);
}

#[tokio::test]
async fn oversized_or_foreign_units_end_the_publisher_link() {
    let relay = relay(
        "",
        "[uses]\nplugin_data = true\n[plugin_data]\nmax_unit_bytes = 1000",
    )
    .await;
    // 1000 bytes of plugin data plus the encryption overhead pass.
    let (mut publisher, mut sub) = bound(&relay, "plugin_data").await;
    publisher.send(&data(1, 1064)).await;
    assert_eq!(sub.recv().await, data(1, 1064));
    publisher.send(&data(2, 1065)).await;
    assert_eq!(publisher.control().await["reason"], "closed");
    assert_eq!(sub.control().await["reason"], "closed");

    // Media in a plugin-data broadcast, and plugin data in a screen share.
    let (mut publisher, _sub) = bound(&relay, "plugin_data").await;
    publisher.send(&audio(1, 80)).await;
    assert_eq!(publisher.control().await["reason"], "closed");
    let (mut publisher, _sub) = bound(&relay, "screen_share").await;
    publisher.send(&data(1, 80)).await;
    assert_eq!(publisher.control().await["reason"], "closed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_broadcast_sender_is_capped_at_its_rate() {
    // 64 kbit/s is 8 KB/s with the minimum burst of 64 KiB.
    let relay = relay(
        "",
        "[uses]\nplugin_data = true\n[plugin_data]\nmax_kbps_per_sender = 64",
    )
    .await;
    let (publisher, sub) = bound(&relay, "plugin_data").await;
    let seen = collect(&sub);
    for seq in 0..100 {
        publisher.send(&data(seq, 2000)).await;
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    // About 33 units fit the burst, one more per 250 ms after it.
    let passed = count(&seen, 0..100);
    assert!((25..=40).contains(&passed), "{passed} of 100 units passed");
    let got = seen.lock().unwrap().clone();
    assert!(got.is_sorted(), "units stay in order: {got:?}");
}

#[tokio::test]
async fn a_plugin_data_pair_side_is_capped_at_its_rate() {
    let relay = relay(
        "",
        "[uses]\nplugin_data = true\n[plugin_data]\nmax_kbps_per_sender = 64",
    )
    .await;
    let (a, b) = (identity(), identity());
    let body = format!(
        r#"{{"a":"{}","b":"{}","use":"plugin_data"}}"#,
        a.hash, b.hash
    );
    let (status, body) = api(&relay, "POST", "/v1/pairs", &body, SECRET).await;
    assert_eq!(status, 201, "{body}");
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    let token = body["tok"].as_str().unwrap();
    let mut ta = tcp_bind(&relay, &a, &bind(token, None)).await;
    assert_eq!(ta.control().await["t"], "waiting");
    let mut tb = tcp_bind(&relay, &b, &bind(token, None)).await;
    assert_eq!(tb.control().await["t"], "ready");
    assert_eq!(ta.control().await["t"], "ready");

    for _ in 0..150 {
        ta.frame(1, &[3; 1000]).await;
    }
    let mut received = 0;
    let mut head = [0u8; 3];
    while let Ok(Ok(_)) = timeout(Duration::from_millis(700), tb.rd.read_exact(&mut head)).await {
        let mut packet = vec![0u8; usize::from(u16::from_be_bytes([head[1], head[2]]))];
        tb.rd.read_exact(&mut packet).await.unwrap();
        received += packet.len();
    }
    // The 64 KiB burst and 8 KB/s after it, of the 150 KB sent.
    assert!(
        (60_000..=80_000).contains(&received),
        "{received} bytes passed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plugin_data_never_takes_the_bandwidth_screen_sharing_needs() {
    // 1 Mbit/s in total, all of it open to plugin data, so only the priority holds it back.
    let relay = relay(
        "total_mbit = 1",
        "[uses]\nplugin_data = true\n[plugin_data]\nmax_share_percent = 100",
    )
    .await;
    let (screen, screen_sub) = bound(&relay, "screen_share").await;
    let (plugin, plugin_sub) = bound(&relay, "plugin_data").await;
    let media_seen = collect(&screen_sub);
    let data_seen = collect(&plugin_sub);

    // Sound at 200 KB/s keeps the relay's 125 KB/s busy; plugin data adds 50 KB/s.
    let media = tokio::spawn(async move {
        for seq in 0..150 {
            screen.send(&audio(seq, 4000)).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        screen
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let busy = Instant::now();
    let mut seq = 0;
    while busy.elapsed() < Duration::from_millis(1500) {
        plugin.send(&data(seq, 1000)).await;
        seq += 1;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let sent_while_busy = seq;
    let _screen = media.await.unwrap();

    // Screen sharing got the bandwidth (about 15 units a second fit), plugin data none of it.
    let media = count(&media_seen, 0..150);
    assert!(media >= 20, "only {media} sound units passed");
    let passed = count(&data_seen, 0..sent_while_busy);
    assert!(
        passed <= 2,
        "{passed} of {sent_while_busy} plugin units passed"
    );

    // Once the relay is idle again, plugin data flows.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    for seq in 1000..1005 {
        plugin.send(&data(seq, 1000)).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    eventually(|| count(&data_seen, 1000..1005) == 5).await;
}
