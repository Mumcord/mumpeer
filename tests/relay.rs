//! End-to-end checks of a running relay on loopback: the bot API, both transports, forwarding and
//! the refusals that keep it from being an open proxy (spec AC-4.15 … AC-4.17).

// Test helpers panic on any unexpected error; that is the failure report.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use mumpeer::Running;
use mumpeer::config::{ApiSecret, Config};
use quinn::{Connection, RecvStream, SendStream, TransportConfig};
use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::time::timeout;

const SECRET: &str = "test-secret-test-secret-test-secret";
const WAIT: Duration = Duration::from_secs(5);

type TlsClient = tokio_rustls::client::TlsStream<TcpStream>;

struct Relay {
    running: Running,
    _dir: tempfile::TempDir,
}

async fn relay(limits: &str) -> Relay {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::parse(&format!(
        "public_host = \"127.0.0.1\"\nudp_listen = \"127.0.0.1:0\"\ntcp_listen = \"127.0.0.1:0\"\n\
         api_listen = \"127.0.0.1:0\"\ndata_dir = \"{}\"\n[limits]\n{limits}",
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

async fn pair(relay: &Relay, a: &Identity, b: &Identity) -> String {
    let body = format!(r#"{{"a":"{}","b":"{}"}}"#, a.hash, b.hash);
    let (status, body) = api(relay, "POST", "/v1/pairs", &body, SECRET).await;
    assert_eq!(status, 201, "{body}");
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    body["tok"].as_str().unwrap().to_owned()
}

fn bind(token: &str) -> String {
    format!(r#"{{"v":1,"t":"bind","tok":"{token}"}}"#)
}

struct Quic {
    conn: Connection,
    _send: SendStream,
    recv: RecvStream,
}

async fn quic_connect(relay: &Relay, id: &Identity) -> Connection {
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(client_tls(
        &relay.running.fingerprint,
        Some(id),
    ))
    .unwrap();
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    let mut transport = TransportConfig::default();
    transport.initial_mtu(1300);
    config.transport_config(Arc::new(transport));
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(config);
    endpoint
        .connect(relay.running.udp_addr, "relay")
        .unwrap()
        .await
        .unwrap()
}

async fn quic_bind(relay: &Relay, id: &Identity, token: &str) -> Quic {
    let conn = quic_connect(relay, id).await;
    let (mut send, recv) = conn.open_bi().await.unwrap();
    send.write_all(format!("{}\n", bind(token)).as_bytes())
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
}

struct Tcp {
    rd: ReadHalf<TlsClient>,
    wr: WriteHalf<TlsClient>,
}

async fn tls_connect(relay: &Relay, id: Option<&Identity>) -> std::io::Result<TlsClient> {
    let connector =
        tokio_rustls::TlsConnector::from(Arc::new(client_tls(&relay.running.fingerprint, id)));
    let stream = TcpStream::connect(relay.running.tcp_addr).await?;
    connector
        .connect(ServerName::try_from("relay").unwrap(), stream)
        .await
}

async fn tcp_bind(relay: &Relay, id: &Identity, token: &str) -> Tcp {
    let stream = tls_connect(relay, Some(id)).await.unwrap();
    let (rd, wr) = tokio::io::split(stream);
    let mut tcp = Tcp { rd, wr };
    tcp.send(0, bind(token).as_bytes()).await;
    tcp
}

impl Tcp {
    async fn send(&mut self, kind: u8, payload: &[u8]) {
        let mut frame = vec![kind];
        frame.extend_from_slice(&u16::try_from(payload.len()).unwrap().to_be_bytes());
        frame.extend_from_slice(payload);
        self.wr.write_all(&frame).await.unwrap();
        self.wr.flush().await.unwrap();
    }

    async fn recv(&mut self) -> Option<(u8, Vec<u8>)> {
        let mut header = [0u8; 3];
        timeout(WAIT, self.rd.read_exact(&mut header))
            .await
            .unwrap()
            .ok()?;
        let mut payload = vec![0u8; usize::from(u16::from_be_bytes([header[1], header[2]]))];
        self.rd.read_exact(&mut payload).await.ok()?;
        Some((header[0], payload))
    }

    async fn control(&mut self) -> serde_json::Value {
        let (kind, payload) = self.recv().await.expect("control frame");
        assert_eq!(kind, 0);
        serde_json::from_slice(&payload).unwrap()
    }
}

#[tokio::test]
async fn forwards_between_two_quic_clients_unchanged() {
    let relay = relay("").await;
    let (a, b) = (identity(), identity());
    let token = pair(&relay, &a, &b).await;
    let mut qa = quic_bind(&relay, &a, &token).await;
    assert_eq!(qa.control().await["t"], "waiting");
    let mut qb = quic_bind(&relay, &b, &token).await;
    assert_eq!(qb.control().await["t"], "ready");
    assert_eq!(qa.control().await["t"], "ready");

    let packet: Vec<u8> = (0..1200).map(|i| (i % 251) as u8).collect();
    qa.conn.send_datagram(packet.clone().into()).unwrap();
    let got = timeout(WAIT, qb.conn.read_datagram())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.as_ref(), packet.as_slice());
    qb.conn.send_datagram(b"back".to_vec().into()).unwrap();
    let got = timeout(WAIT, qa.conn.read_datagram())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.as_ref(), b"back");

    let (_, status) = api(&relay, "GET", "/v1/status", "", SECRET).await;
    assert!(status.contains(r#""sessions":1"#), "{status}");
}

#[tokio::test]
async fn forwards_between_quic_and_tcp() {
    let relay = relay("").await;
    let (a, b) = (identity(), identity());
    let token = pair(&relay, &a, &b).await;
    let mut ta = tcp_bind(&relay, &a, &token).await;
    assert_eq!(ta.control().await["t"], "waiting");
    let mut qb = quic_bind(&relay, &b, &token).await;
    assert_eq!(qb.control().await["t"], "ready");
    assert_eq!(ta.control().await["t"], "ready");

    let packet = vec![7u8; 1200];
    ta.send(1, &packet).await;
    let got = timeout(WAIT, qb.conn.read_datagram())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.as_ref(), packet.as_slice());
    qb.conn.send_datagram(b"to tcp".to_vec().into()).unwrap();
    assert_eq!(ta.recv().await, Some((1, b"to tcp".to_vec())));
}

#[tokio::test]
async fn refuses_everyone_outside_the_pair() {
    let relay = relay("").await;
    let (a, b, c, d) = (identity(), identity(), identity(), identity());
    let token = pair(&relay, &a, &b).await;
    pair(&relay, &c, &d).await;

    // A certificate in no pair is closed right after the handshake.
    let stranger = identity();
    let conn = quic_connect(&relay, &stranger).await;
    let reason = timeout(WAIT, conn.closed()).await.unwrap();
    assert!(reason.to_string().contains("not_allowed"), "{reason}");
    let (rd, wr) = tokio::io::split(tls_connect(&relay, Some(&stranger)).await.unwrap());
    let mut tcp = Tcp { rd, wr };
    assert_eq!(tcp.control().await["code"], "not_allowed");

    // An allowlisted third certificate using A–B's token.
    let mut qc = quic_bind(&relay, &c, &token).await;
    assert_eq!(qc.control().await["code"], "not_allowed");
    let mut tc = tcp_bind(&relay, &d, &token).await;
    assert_eq!(tc.control().await["code"], "not_allowed");

    // The same side twice.
    let mut qa = quic_bind(&relay, &a, &token).await;
    assert_eq!(qa.control().await["t"], "waiting");
    let mut again = tcp_bind(&relay, &a, &token).await;
    assert_eq!(again.control().await["code"], "not_allowed");

    // Unknown token and wrong version.
    let mut unknown = quic_bind(&relay, &b, &"x".repeat(43)).await;
    assert_eq!(unknown.control().await["code"], "unknown_token");
    let mut tb = tcp_bind_raw(&relay, &b, r#"{"v":2,"t":"bind","tok":"x"}"#).await;
    assert_eq!(tb.control().await["code"], "version");

    // Without a client certificate the TLS handshake fails.
    let stream = tls_connect(&relay, None).await;
    let failed = match stream {
        Err(_) => true,
        Ok(stream) => {
            let (mut rd, _wr) = tokio::io::split(stream);
            let mut byte = [0u8; 1];
            timeout(WAIT, rd.read(&mut byte)).await.unwrap().is_err()
        }
    };
    assert!(
        failed,
        "a client without a certificate must not get through"
    );
}

async fn tcp_bind_raw(relay: &Relay, id: &Identity, message: &str) -> Tcp {
    let stream = tls_connect(relay, Some(id)).await.unwrap();
    let (rd, wr) = tokio::io::split(stream);
    let mut tcp = Tcp { rd, wr };
    tcp.send(0, message.as_bytes()).await;
    tcp
}

#[tokio::test]
async fn unbound_tokens_expire() {
    let relay = relay("token_ttl_s = 1").await;
    let (a, b) = (identity(), identity());
    let token = pair(&relay, &a, &b).await;
    let mut qa = quic_bind(&relay, &a, &token).await;
    assert_eq!(qa.control().await["t"], "waiting");
    assert_eq!(qa.control().await["code"], "expired");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut tb = tcp_bind(&relay, &b, &token).await;
    assert_eq!(tb.control().await["code"], "expired");
}

#[tokio::test]
async fn api_checks_secret_and_input() {
    let relay = relay("").await;
    let (status, _) = api(&relay, "GET", "/v1/status", "", "wrong-secret").await;
    assert_eq!(status, 401);
    let same = format!(r#"{{"a":"{0}","b":"{0}"}}"#, "a".repeat(40));
    assert_eq!(api(&relay, "POST", "/v1/pairs", &same, SECRET).await.0, 400);
    let bad = r#"{"a":"xyz","b":"abc"}"#;
    assert_eq!(api(&relay, "POST", "/v1/pairs", bad, SECRET).await.0, 400);
    let (status, body) = api(&relay, "GET", "/v1/status", "", SECRET).await;
    assert_eq!(status, 200);
    assert!(body.contains(&relay.running.fingerprint));
}

#[tokio::test]
async fn full_relay_refuses_new_sessions() {
    let relay = relay("max_sessions = 1").await;
    let (a, b, c, d) = (identity(), identity(), identity(), identity());
    let ab = pair(&relay, &a, &b).await;
    let cd = pair(&relay, &c, &d).await;
    let mut qa = quic_bind(&relay, &a, &ab).await;
    assert_eq!(qa.control().await["t"], "waiting");
    let mut qc = quic_bind(&relay, &c, &cd).await;
    assert_eq!(qc.control().await["code"], "relay_full");
    let body = format!(r#"{{"a":"{}","b":"{}"}}"#, c.hash, d.hash);
    let (status, body) = api(&relay, "POST", "/v1/pairs", &body, SECRET).await;
    assert_eq!((status, body.as_str()), (503, r#"{"code":"relay_full"}"#));
}

#[tokio::test]
async fn dropping_a_peer_ends_its_session() {
    let relay = relay("").await;
    let (a, b) = (identity(), identity());
    let token = pair(&relay, &a, &b).await;
    let mut qa = quic_bind(&relay, &a, &token).await;
    assert_eq!(qa.control().await["t"], "waiting");
    let mut tb = tcp_bind(&relay, &b, &token).await;
    assert_eq!(tb.control().await["t"], "ready");
    assert_eq!(qa.control().await["t"], "ready");

    let (status, _) = api(
        &relay,
        "DELETE",
        &format!("/v1/peers/{}", b.hash),
        "",
        SECRET,
    )
    .await;
    assert_eq!(status, 204);
    timeout(WAIT, qa.conn.closed()).await.unwrap();
    assert_eq!(tb.recv().await, None);
}
