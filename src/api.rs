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

use crate::broadcast::{AdmitError, BroadcastId, BroadcastUse, CreateError};
use crate::config::ApiSecret;
use crate::config::Uses;
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
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            409 => "Conflict",
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
    /// What the pair is for: `images`, `files`, `screen_share` or `plugin_data` (`camera` is a
    /// broadcast use only).
    #[serde(default)]
    r#use: Option<String>,
}

#[derive(Deserialize)]
struct BroadcastRequest {
    publisher: String,
    max_viewers: u16,
    /// `screen_share` (default), `camera` or `plugin_data`.
    #[serde(default)]
    r#use: Option<String>,
    /// The publisher will use the Bovnar media profile (FR-7.96).
    #[serde(default)]
    media: bool,
}

#[derive(Deserialize)]
struct ViewerRequest {
    viewer: String,
}

const NO_CONTENT: Response = Response {
    status: 204,
    body: None,
};

fn route(head: &Head, body: &[u8], relay: &Relay) -> Response {
    let path = head.path.as_str();
    match (head.method.as_str(), path) {
        ("GET", "/v1/status") => {
            let mut status = json!({
                "v": 1,
                "boot_id": relay.boot_id,
                "host": relay.announced.host,
                "udp": relay.announced.udp,
                "tcp": relay.announced.tcp,
                "fp": relay.announced.fp,
                "sessions": relay.sessions(),
                "max_sessions": relay.max_sessions(),
                "uses": relay.uses.enabled(),
            });
            // Absent, not null, when broadcasts are off (spec 07 §6.6). Cameras alone keep it,
            // because clients need `bc` for camera sharing too (spec 08 FR-8.2).
            if let Some(max_viewers) = relay.broadcasts.max_viewers() {
                status["broadcast"] = json!({
                    "max_viewers": max_viewers,
                    "max_camera": relay.broadcasts.max_camera(),
                });
                // The relay speaks the Bovnar media profile (FR-7.96).
                status["media"] = json!(crate::broadcast::media::MEDIA_VERSION);
            }
            if relay.uses.plugin_data {
                status["plugin_data"] =
                    json!({ "max_kbps_per_sender": relay.plugin_data.max_kbps_per_sender });
            }
            Response::json(200, status)
        }
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
            // Without `use` the pair could serve any feature, so it is only allowed when all are.
            let allowed = match request.r#use.as_deref() {
                Some(name) if !Uses::NAMES.contains(&name) || name == "camera" => {
                    return Response::code(400, "bad_request");
                }
                Some(name) => relay.uses.allows(name),
                None => relay.uses.all(),
            };
            if !allowed {
                return Response::code(403, "use_not_allowed");
            }
            let plugin_data = request.r#use.as_deref() == Some("plugin_data");
            match relay.create_pair(a, b, plugin_data) {
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
                    NO_CONTENT
                }
                None => Response::code(400, "bad_request"),
            }
        }
        ("POST", "/v1/broadcasts") => create_broadcast(body, relay),
        (method, _) if path.starts_with("/v1/broadcasts/") => {
            broadcast_route(method, &path["/v1/broadcasts/".len()..], body, relay)
        }
        (_, "/v1/status" | "/v1/pairs" | "/v1/broadcasts") => {
            Response::code(405, "method_not_allowed")
        }
        (_, _) if path.starts_with("/v1/peers/") => Response::code(405, "method_not_allowed"),
        _ => Response::code(404, "not_found"),
    }
}

fn create_broadcast(body: &[u8], relay: &Relay) -> Response {
    let Ok(request) = serde_json::from_slice::<BroadcastRequest>(body) else {
        return Response::code(400, "bad_request");
    };
    let Some(publisher) = cert_hash(&request.publisher) else {
        return Response::code(400, "bad_request");
    };
    if request.max_viewers == 0 {
        return Response::code(400, "bad_request");
    }
    let use_ = match request.r#use.as_deref() {
        None | Some("screen_share") => BroadcastUse::ScreenShare,
        Some("camera") => BroadcastUse::Camera,
        Some("plugin_data") => BroadcastUse::PluginData,
        Some(_) => return Response::code(400, "bad_request"),
    };
    if !relay.uses.allows(use_.name()) {
        return Response::code(403, "use_not_allowed");
    }
    match relay
        .broadcasts
        .create(publisher, request.max_viewers, use_, request.media)
    {
        Ok((id, token)) => Response::json(
            201,
            json!({ "id": id.to_string(), "tok": token, "expires_in": relay.token_ttl_s() }),
        ),
        Err(CreateError::RelayFull) => Response::code(503, "relay_full"),
    }
}

/// `{id}`, `{id}/viewers` and `{id}/viewers/{hash}` below `/v1/broadcasts/`.
fn broadcast_route(method: &str, rest: &str, body: &[u8], relay: &Relay) -> Response {
    let mut parts = rest.split('/');
    let (id, viewers, hash, extra) = (parts.next(), parts.next(), parts.next(), parts.next());
    if extra.is_some() || viewers.is_some_and(|v| v != "viewers") {
        return Response::code(404, "not_found");
    }
    let Some(id) = id.and_then(BroadcastId::parse) else {
        return Response::code(400, "bad_request");
    };
    match (method, viewers.is_some(), hash) {
        ("DELETE", false, None) => {
            relay.broadcasts.delete(id);
            NO_CONTENT
        }
        ("POST", true, None) => {
            let Some(viewer) = serde_json::from_slice::<ViewerRequest>(body)
                .ok()
                .and_then(|request| cert_hash(&request.viewer))
            else {
                return Response::code(400, "bad_request");
            };
            match relay.broadcasts.admit(id, viewer) {
                Ok(token) => Response::json(
                    201,
                    json!({ "tok": token, "expires_in": relay.token_ttl_s() }),
                ),
                Err(AdmitError::UnknownBroadcast) => Response::code(404, "not_found"),
                Err(AdmitError::Publisher) => Response::code(400, "bad_request"),
                Err(AdmitError::BroadcastFull) => Response::code(409, "broadcast_full"),
            }
        }
        ("DELETE", true, Some(hash)) => match cert_hash(hash) {
            Some(hash) => {
                relay.broadcasts.remove_viewer(id, &hash);
                NO_CONTENT
            }
            None => Response::code(400, "bad_request"),
        },
        _ => Response::code(405, "method_not_allowed"),
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
        assert_eq!(
            body["uses"],
            json!(["images", "files", "screen_share", "camera", "plugin_data"])
        );
        assert_eq!(
            body["broadcast"],
            json!({ "max_viewers": 25, "max_camera": 18 })
        );
    }

    fn relay_with(config: &str) -> Arc<Relay> {
        let config = Config::parse(&format!("public_host = \"relay.test\"\n{config}")).unwrap();
        let announced = Announced {
            host: "relay.test".into(),
            udp: 443,
            tcp: 443,
            fp: "ab".repeat(32),
        };
        Relay::new(&config, announced)
    }

    fn json_body(out: &str) -> serde_json::Value {
        serde_json::from_str(out.split("\r\n\r\n").nth(1).unwrap()).unwrap()
    }

    fn create(publisher: &str, max_viewers: u32) -> String {
        authed(
            "POST",
            "/v1/broadcasts",
            &format!(r#"{{"publisher":"{publisher}","max_viewers":{max_viewers}}}"#),
        )
    }

    fn admit(id: &str, viewer: &str) -> String {
        authed(
            "POST",
            &format!("/v1/broadcasts/{id}/viewers"),
            &format!(r#"{{"viewer":"{viewer}"}}"#),
        )
    }

    #[tokio::test]
    async fn status_has_no_broadcast_when_they_are_off() {
        for config in [
            "[uses]\nscreen_share = false\ncamera = false",
            "[limits]\nmax_broadcasts = 0\nmax_camera_broadcasts = 0",
            "[limits]\nmax_broadcasts = 0\n[uses]\ncamera = false",
        ] {
            let relay = relay_with(config);
            let body = json_body(&request(&relay, &authed("GET", "/v1/status", "")).await);
            assert!(body.get("broadcast").is_none(), "{config}: {body}");
        }
        let relay = relay_with("[limits]\nmax_viewers_per_broadcast = 9");
        let body = json_body(&request(&relay, &authed("GET", "/v1/status", "")).await);
        assert_eq!(
            body["broadcast"],
            json!({ "max_viewers": 9, "max_camera": 18 })
        );
        // Either kind of broadcast keeps the object; the other one reads as off.
        let relay = relay_with("[uses]\nscreen_share = false");
        let body = json_body(&request(&relay, &authed("GET", "/v1/status", "")).await);
        assert_eq!(
            body["broadcast"],
            json!({ "max_viewers": 25, "max_camera": 18 })
        );
        let relay = relay_with("[uses]\ncamera = false");
        let body = json_body(&request(&relay, &authed("GET", "/v1/status", "")).await);
        assert_eq!(
            body["broadcast"],
            json!({ "max_viewers": 25, "max_camera": 0 })
        );
    }

    #[tokio::test]
    async fn camera_broadcasts_are_gated_and_capped() {
        let p = "a".repeat(40);
        let camera = |publisher: &str| {
            authed(
                "POST",
                "/v1/broadcasts",
                &format!(r#"{{"publisher":"{publisher}","max_viewers":25,"use":"camera"}}"#),
            )
        };

        let off = relay_with("[uses]\ncamera = false");
        let body = json_body(&request(&off, &authed("GET", "/v1/status", "")).await);
        assert_eq!(
            body["uses"],
            json!(["images", "files", "screen_share", "plugin_data"])
        );
        let out = request(&off, &camera(&p)).await;
        assert!(out.starts_with("HTTP/1.1 403 Forbidden"), "{out}");
        assert!(out.contains(r#""code":"use_not_allowed""#), "{out}");
        // Screen shares, with or without `use`, are not affected.
        assert!(
            request(&off, &create(&p, 25))
                .await
                .starts_with("HTTP/1.1 201")
        );

        let relay = relay_with("[limits]\nmax_broadcasts = 1\nmax_camera_broadcasts = 2");
        for _ in 0..2 {
            let out = request(&relay, &camera(&p)).await;
            assert!(out.starts_with("HTTP/1.1 201"), "{out}");
        }
        let out = request(&relay, &camera(&p)).await;
        assert!(out.starts_with("HTTP/1.1 503"), "{out}");
        assert!(out.contains(r#""code":"relay_full""#), "{out}");
        // The screen-share cap is counted separately.
        assert!(
            request(&relay, &create(&p, 25))
                .await
                .starts_with("HTTP/1.1 201")
        );

        // A camera is not a pair use.
        let b = "b".repeat(40);
        let out = request(
            &relay,
            &authed(
                "POST",
                "/v1/pairs",
                &format!(r#"{{"a":"{p}","b":"{b}","use":"camera"}}"#),
            ),
        )
        .await;
        assert!(out.starts_with("HTTP/1.1 400"), "{out}");
    }

    #[tokio::test]
    async fn broadcast_lifecycle() {
        let relay = relay_with("[limits]\nmax_viewers_per_broadcast = 1");
        let (p, v, w) = ("A".repeat(40), "b".repeat(40), "c".repeat(40));
        let out = request(&relay, &create(&p, 25)).await;
        assert!(out.starts_with("HTTP/1.1 201 Created"), "{out}");
        let body = json_body(&out);
        let id = body["id"].as_str().unwrap().to_owned();
        assert_eq!(id.len(), 16);
        assert_eq!(body["tok"].as_str().unwrap().len(), 43);
        assert_eq!(body["expires_in"], 120);
        assert!(relay.allowed(&"a".repeat(40)));

        let out = request(&relay, &admit(&id, &v)).await;
        assert!(out.starts_with("HTTP/1.1 201"), "{out}");
        let body = json_body(&out);
        assert_eq!(body["tok"].as_str().unwrap().len(), 43);
        assert_eq!(body["expires_in"], 120);
        assert!(relay.allowed(&v));

        let out = request(&relay, &admit(&id, &w)).await;
        assert!(out.starts_with("HTTP/1.1 409 Conflict"), "{out}");
        assert!(out.contains(r#""code":"broadcast_full""#), "{out}");
        assert!(
            request(&relay, &admit(&id, &p))
                .await
                .starts_with("HTTP/1.1 400")
        );
        assert!(
            request(&relay, &admit(&id, "xyz"))
                .await
                .starts_with("HTTP/1.1 400")
        );
        let unknown = request(&relay, &admit("0000000000000001", &w)).await;
        assert!(unknown.starts_with("HTTP/1.1 404"), "{unknown}");

        let kick = authed("DELETE", &format!("/v1/broadcasts/{id}/viewers/{v}"), "");
        assert!(request(&relay, &kick).await.starts_with("HTTP/1.1 204"));
        assert!(!relay.allowed(&v));
        assert!(
            request(&relay, &admit(&id, &w))
                .await
                .starts_with("HTTP/1.1 201")
        );

        let end = authed("DELETE", &format!("/v1/broadcasts/{id}"), "");
        assert!(request(&relay, &end).await.starts_with("HTTP/1.1 204"));
        assert!(!relay.allowed(&w));
        assert!(
            request(&relay, &admit(&id, &w))
                .await
                .starts_with("HTTP/1.1 404")
        );
        // Deleting what is already gone is fine: the bot may race the relay's own end.
        assert!(request(&relay, &end).await.starts_with("HTTP/1.1 204"));
    }

    #[tokio::test]
    async fn plugin_data_is_on_by_default_and_can_be_disabled() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let pair = authed(
            "POST",
            "/v1/pairs",
            &format!(r#"{{"a":"{a}","b":"{b}","use":"plugin_data"}}"#),
        );
        let create_pd = authed(
            "POST",
            "/v1/broadcasts",
            &format!(r#"{{"publisher":"{a}","max_viewers":5,"use":"plugin_data"}}"#),
        );

        let off = relay_with("[uses]\nplugin_data = false");
        let status = json_body(&request(&off, &authed("GET", "/v1/status", "")).await);
        assert_eq!(
            status["uses"],
            json!(["images", "files", "screen_share", "camera"])
        );
        assert!(status.get("plugin_data").is_none());
        for req in [&pair, &create_pd] {
            let out = request(&off, req).await;
            assert!(out.starts_with("HTTP/1.1 403"), "{out}");
            assert!(out.contains(r#""code":"use_not_allowed""#), "{out}");
        }

        let on = relay_with("[plugin_data]\nmax_kbps_per_sender = 256");
        let status = json_body(&request(&on, &authed("GET", "/v1/status", "")).await);
        assert_eq!(
            status["uses"],
            json!(["images", "files", "screen_share", "camera", "plugin_data"])
        );
        assert_eq!(status["plugin_data"], json!({ "max_kbps_per_sender": 256 }));
        for req in [&pair, &create_pd] {
            assert!(request(&on, req).await.starts_with("HTTP/1.1 201"));
        }
        // A pair without `use` still works and never serves plugin data.
        let plain = authed("POST", "/v1/pairs", &format!(r#"{{"a":"{a}","b":"{b}"}}"#));
        assert!(request(&on, &plain).await.starts_with("HTTP/1.1 201"));
        let bad = authed(
            "POST",
            "/v1/broadcasts",
            &format!(r#"{{"publisher":"{a}","max_viewers":5,"use":"video"}}"#),
        );
        assert!(request(&on, &bad).await.starts_with("HTTP/1.1 400"));
        // Plugin data off, screen share on: screen-share broadcasts are unaffected.
        assert!(
            request(&off, &create(&a, 5))
                .await
                .starts_with("HTTP/1.1 201")
        );
    }

    #[tokio::test]
    async fn broadcast_refusals() {
        let p = "a".repeat(40);
        let off = relay_with("[uses]\nscreen_share = false");
        let out = request(&off, &create(&p, 25)).await;
        assert!(out.starts_with("HTTP/1.1 403 Forbidden"), "{out}");
        assert!(out.contains(r#""code":"use_not_allowed""#), "{out}");

        let relay = relay_with("[limits]\nmax_broadcasts = 1");
        assert!(
            request(&relay, &create(&p, 25))
                .await
                .starts_with("HTTP/1.1 201")
        );
        let out = request(&relay, &create(&p, 25)).await;
        assert!(out.starts_with("HTTP/1.1 503"), "{out}");
        assert!(out.contains(r#""code":"relay_full""#), "{out}");
        let none = relay_with("[limits]\nmax_broadcasts = 0");
        assert!(
            request(&none, &create(&p, 25))
                .await
                .starts_with("HTTP/1.1 503")
        );

        for bad in [create(&p, 0), create(&p, 70000), create("abc", 25)] {
            assert!(
                request(&relay, &bad).await.starts_with("HTTP/1.1 400"),
                "{bad}"
            );
        }
        for (method, path, status) in [
            ("GET", "/v1/broadcasts", "405"),
            ("GET", "/v1/broadcasts/0000000000000001", "405"),
            ("POST", "/v1/broadcasts/0000000000000001", "405"),
            ("DELETE", "/v1/broadcasts/0000000000000001/viewers", "405"),
            ("DELETE", "/v1/broadcasts/xyz", "400"),
            (
                "DELETE",
                "/v1/broadcasts/0000000000000001/viewers/xyz",
                "400",
            ),
            ("DELETE", "/v1/broadcasts/0000000000000001/other", "404"),
            (
                "DELETE",
                "/v1/broadcasts/0000000000000001/viewers/a/b",
                "404",
            ),
        ] {
            let out = request(&relay, &authed(method, path, "")).await;
            assert!(
                out.starts_with(&format!("HTTP/1.1 {status}")),
                "{method} {path}: {out}"
            );
        }
    }

    #[tokio::test]
    async fn delete_peer_ends_its_broadcasts() {
        let relay = relay();
        let (p, v) = ("a".repeat(40), "b".repeat(40));
        let id = json_body(&request(&relay, &create(&p, 25)).await)["id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            request(&relay, &admit(&id, &v))
                .await
                .starts_with("HTTP/1.1 201")
        );
        let out = request(&relay, &authed("DELETE", &format!("/v1/peers/{p}"), "")).await;
        assert!(out.starts_with("HTTP/1.1 204"), "{out}");
        assert!(!relay.allowed(&p) && !relay.allowed(&v));
        assert!(
            request(&relay, &admit(&id, &v))
                .await
                .starts_with("HTTP/1.1 404")
        );
    }

    #[tokio::test]
    async fn disabled_uses_are_refused_for_pairs() {
        let config = Config::parse("public_host = \"relay.test\"\n[uses]\nfiles = false").unwrap();
        let announced = Announced {
            host: "relay.test".into(),
            udp: 443,
            tcp: 443,
            fp: "ab".repeat(32),
        };
        let relay = Relay::new(&config, announced);
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let pair = |extra: &str| {
            authed(
                "POST",
                "/v1/pairs",
                &format!(r#"{{"a":"{a}","b":"{b}"{extra}}}"#),
            )
        };

        let out = request(&relay, &authed("GET", "/v1/status", "")).await;
        let body: serde_json::Value =
            serde_json::from_str(out.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            body["uses"],
            json!(["images", "screen_share", "camera", "plugin_data"])
        );

        for (extra, status) in [
            (r#","use":"images""#, "201"),
            (r#","use":"screen_share""#, "201"),
            (r#","use":"files""#, "403"),
            ("", "403"),
            (r#","use":"video""#, "400"),
        ] {
            let out = request(&relay, &pair(extra)).await;
            assert!(
                out.starts_with(&format!("HTTP/1.1 {status}")),
                "{extra}: {out}"
            );
            if status == "403" {
                assert!(out.contains(r#""code":"use_not_allowed""#), "{out}");
            }
        }
    }

    #[tokio::test]
    async fn pairs_without_use_work_when_all_uses_are_enabled() {
        let relay = relay();
        let body = format!(r#"{{"a":"{}","b":"{}"}}"#, "a".repeat(40), "b".repeat(40));
        let out = request(&relay, &authed("POST", "/v1/pairs", &body)).await;
        assert!(out.starts_with("HTTP/1.1 201"), "{out}");
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
            .create_pair("a".repeat(40), "b".repeat(40), false)
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
