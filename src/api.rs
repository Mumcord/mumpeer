//! Allowlist API for the relay bot (spec §6.6): a small HTTP/1.1 handler, one request per
//! connection, every request authenticated with the shared secret.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;

use crate::config::ApiSecret;
use crate::limits::log_prefix;
use crate::relay::{PairError, Relay};

const MAX_HEAD: usize = 8 * 1024;
const MAX_BODY: usize = 4 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The bot makes a few requests at a time; more parallel connections are not served.
const MAX_CONNECTIONS: usize = 32;

pub async fn serve(
    listener: TcpListener,
    tls: Option<rustls::ServerConfig>,
    secret: Arc<ApiSecret>,
    relay: Arc<Relay>,
) -> anyhow::Result<()> {
    let acceptor = tls.map(|config| TlsAcceptor::from(Arc::new(config)));
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                tracing::warn!("API accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let ip = peer.ip();
        if relay.sources.blocked(ip) {
            continue;
        }
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            tracing::warn!("too many parallel API connections; dropping one");
            continue;
        };
        let (acceptor, secret, relay) = (acceptor.clone(), secret.clone(), relay.clone());
        tokio::spawn(async move {
            let _permit = permit;
            let exchange = async {
                match acceptor {
                    Some(acceptor) => {
                        if let Ok(stream) = acceptor.accept(stream).await {
                            exchange(stream, ip, &secret, &relay).await;
                        }
                    }
                    None => exchange(stream, ip, &secret, &relay).await,
                }
            };
            let _ = timeout(REQUEST_TIMEOUT, exchange).await;
        });
    }
}

struct Response {
    status: u16,
    body: Option<serde_json::Value>,
}

impl Response {
    fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            body: Some(body),
        }
    }

    fn code(status: u16, code: &str) -> Self {
        Self::json(status, json!({ "code": code }))
    }

    fn to_bytes(&self) -> Vec<u8> {
        let reason = match self.status {
            200 => "OK",
            201 => "Created",
            204 => "No Content",
            400 => "Bad Request",
            401 => "Unauthorized",
            404 => "Not Found",
            405 => "Method Not Allowed",
            413 => "Content Too Large",
            429 => "Too Many Requests",
            431 => "Request Header Fields Too Large",
            _ => "Service Unavailable",
        };
        let mut out = format!("HTTP/1.1 {} {reason}\r\nConnection: close\r\n", self.status);
        match &self.body {
            Some(body) => {
                let body = body.to_string();
                out.push_str(&format!(
                    "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                ));
            }
            None => out.push_str("Content-Length: 0\r\n\r\n"),
        }
        out.into_bytes()
    }
}

struct Head {
    method: String,
    path: String,
    authorization: Option<String>,
    content_length: usize,
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    ip: IpAddr,
    secret: &ApiSecret,
    relay: &Relay,
) {
    let response = match handle(&mut stream, ip, secret, relay).await {
        Ok(response) | Err(response) => response,
    };
    if stream.write_all(&response.to_bytes()).await.is_ok() {
        let _ = stream.shutdown().await;
    }
}

async fn handle<S: AsyncRead + Unpin>(
    stream: &mut S,
    ip: IpAddr,
    secret: &ApiSecret,
    relay: &Relay,
) -> Result<Response, Response> {
    let mut buf = Vec::with_capacity(1024);
    let (head, body_start) = read_head(stream, &mut buf).await?;
    let authorized = head
        .authorization
        .as_deref()
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|candidate| secret.matches(candidate.trim()));
    if !authorized {
        tracing::warn!("API request without a valid secret from {}", log_prefix(ip));
        relay.sources.failed(ip);
        return Err(Response::code(401, "unauthorized"));
    }
    relay.bot_seen();
    if head.content_length > MAX_BODY {
        return Err(Response::code(413, "too_large"));
    }
    let mut body = buf.split_off(body_start.min(buf.len()));
    if body.len() < head.content_length {
        let have = body.len();
        body.resize(head.content_length, 0);
        stream
            .read_exact(&mut body[have..])
            .await
            .map_err(|_| Response::code(400, "bad_request"))?;
    }
    body.truncate(head.content_length);
    Ok(route(&head, &body, relay))
}

/// Reads up to the end of the header block; returns the parsed head and where the body starts.
async fn read_head<S: AsyncRead + Unpin>(
    stream: &mut S,
    buf: &mut Vec<u8>,
) -> Result<(Head, usize), Response> {
    let bad = || Response::code(400, "bad_request");
    let end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if buf.len() >= MAX_HEAD {
            return Err(Response::code(431, "too_large"));
        }
        let mut chunk = [0u8; 1024];
        let read = stream.read(&mut chunk).await.map_err(|_| bad())?;
        if read == 0 {
            return Err(bad());
        }
        buf.extend_from_slice(&chunk[..read]);
    };
    let text = std::str::from_utf8(&buf[..end]).map_err(|_| bad())?;
    let mut lines = text.split("\r\n");
    let mut request = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(path), Some(_version), None) = (
        request.next(),
        request.next(),
        request.next(),
        request.next(),
    ) else {
        return Err(bad());
    };
    let mut head = Head {
        method: method.to_owned(),
        path: path.to_owned(),
        authorization: None,
        content_length: 0,
    };
    for line in lines {
        let (name, value) = line.split_once(':').ok_or_else(bad)?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("authorization") {
            head.authorization = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("content-length") {
            head.content_length = value.parse().map_err(|_| bad())?;
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            // Bodies are tiny JSON; only Content-Length is supported.
            return Err(bad());
        }
    }
    Ok((head, end + 4))
}

#[derive(Deserialize)]
struct PairRequest {
    a: String,
    b: String,
}

fn route(head: &Head, body: &[u8], relay: &Relay) -> Response {
    let path = head.path.as_str();
    match (head.method.as_str(), path) {
        ("GET", "/v1/status") => Response::json(
            200,
            json!({
                "v": 1,
                "boot_id": relay.boot_id,
                "host": relay.announced.host,
                "udp": relay.announced.udp,
                "tcp": relay.announced.tcp,
                "fp": relay.announced.fp,
                "sessions": relay.sessions(),
                "max_sessions": relay.max_sessions(),
            }),
        ),
        ("POST", "/v1/pairs") => {
            let Ok(request) = serde_json::from_slice::<PairRequest>(body) else {
                return Response::code(400, "bad_request");
            };
            let (Some(a), Some(b)) = (cert_hash(&request.a), cert_hash(&request.b)) else {
                return Response::code(400, "bad_request");
            };
            if a == b {
                return Response::code(400, "bad_request");
            }
            match relay.create_pair(a, b) {
                Ok(token) => Response::json(
                    201,
                    json!({ "tok": token, "expires_in": relay.token_ttl_s() }),
                ),
                Err(PairError::RateLimited) => Response::code(429, "rate_limited"),
                Err(PairError::RelayFull) => Response::code(503, "relay_full"),
            }
        }
        ("DELETE", _) if path.starts_with("/v1/peers/") => {
            match cert_hash(&path["/v1/peers/".len()..]) {
                Some(hash) => {
                    relay.drop_peer(&hash);
                    Response {
                        status: 204,
                        body: None,
                    }
                }
                None => Response::code(400, "bad_request"),
            }
        }
        (_, "/v1/status" | "/v1/pairs") => Response::code(405, "method_not_allowed"),
        (_, _) if path.starts_with("/v1/peers/") => Response::code(405, "method_not_allowed"),
        _ => Response::code(404, "not_found"),
    }
}

/// A Mumble certificate hash: 40 hex digits, normalised to lowercase.
fn cert_hash(value: &str) -> Option<String> {
    (value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::relay::Announced;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    fn relay() -> Arc<Relay> {
        let config = Config::parse("public_host = \"relay.test\"").unwrap();
        let announced = Announced {
            host: "relay.test".into(),
            udp: 443,
            tcp: 443,
            fp: "ab".repeat(32),
        };
        Relay::new(&config, announced)
    }

    async fn request(relay: &Relay, raw: &str) -> String {
        let secret = ApiSecret::new(SECRET).unwrap();
        let (mut client, server) = tokio::io::duplex(16 * 1024);
        client.write_all(raw.as_bytes()).await.unwrap();
        exchange(server, "192.0.2.9".parse().unwrap(), &secret, relay).await;
        let mut out = String::new();
        client.read_to_string(&mut out).await.unwrap();
        out
    }

    fn authed(method: &str, path: &str, body: &str) -> String {
        format!(
            "{method} {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {SECRET}\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn status_needs_the_secret() {
        let relay = relay();
        let out = request(&relay, "GET /v1/status HTTP/1.1\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 401"), "{out}");
        let wrong = "GET /v1/status HTTP/1.1\r\nAuthorization: Bearer nope\r\n\r\n";
        assert!(request(&relay, wrong).await.starts_with("HTTP/1.1 401"));
        let out = request(&relay, &authed("GET", "/v1/status", "")).await;
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        let body: serde_json::Value =
            serde_json::from_str(out.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["v"], 1);
        assert_eq!(body["boot_id"].as_str().unwrap().len(), 16);
        assert_eq!(body["fp"], "ab".repeat(32));
        assert_eq!(body["max_sessions"], 50);
    }

    #[tokio::test]
    async fn pairs_validate_hashes() {
        let relay = relay();
        let a = "A".repeat(40);
        let b = "b".repeat(40);
        let ok = request(
            &relay,
            &authed("POST", "/v1/pairs", &format!(r#"{{"a":"{a}","b":"{b}"}}"#)),
        )
        .await;
        assert!(ok.starts_with("HTTP/1.1 201"), "{ok}");
        let body: serde_json::Value =
            serde_json::from_str(ok.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["tok"].as_str().unwrap().len(), 43);
        assert_eq!(body["expires_in"], 120);
        assert!(relay.allowed(&"a".repeat(40)));

        let same = format!(r#"{{"a":"{b}","b":"{b}"}}"#);
        assert!(
            request(&relay, &authed("POST", "/v1/pairs", &same))
                .await
                .starts_with("HTTP/1.1 400")
        );
        let short = r#"{"a":"abc","b":"def"}"#;
        assert!(
            request(&relay, &authed("POST", "/v1/pairs", short))
                .await
                .starts_with("HTTP/1.1 400")
        );
        let too_big = authed("POST", "/v1/pairs", &" ".repeat(MAX_BODY + 1));
        assert!(request(&relay, &too_big).await.starts_with("HTTP/1.1 413"));
    }

    #[tokio::test]
    async fn delete_peer_and_unknown_routes() {
        let relay = relay();
        relay
            .create_pair("a".repeat(40), "b".repeat(40))
            .ok()
            .unwrap();
        let out = request(
            &relay,
            &authed("DELETE", &format!("/v1/peers/{}", "a".repeat(40)), ""),
        )
        .await;
        assert!(out.starts_with("HTTP/1.1 204"), "{out}");
        assert!(!relay.allowed(&"b".repeat(40)));
        assert!(
            request(&relay, &authed("GET", "/v1/other", ""))
                .await
                .starts_with("HTTP/1.1 404")
        );
        assert!(
            request(&relay, &authed("PUT", "/v1/pairs", ""))
                .await
                .starts_with("HTTP/1.1 405")
        );
    }
}
