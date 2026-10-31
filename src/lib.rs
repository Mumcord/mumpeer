//! mumpeer: a relay for Mumcord's peer-to-peer features. It forwards the end-to-end encrypted
//! inner QUIC packets between the two clients of a pair, like a TURN server, and never forwards
//! anywhere else.

pub mod api;
pub mod config;
pub mod limits;
pub mod relay;
pub mod tcp;
pub mod tls;
pub mod udp;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::task::JoinSet;

use crate::config::{ApiSecret, Config};
use crate::relay::Relay;
use crate::tls::Identity;

/// A running relay: its bound addresses and the tasks serving them.
pub struct Running {
    pub udp_addr: SocketAddr,
    pub tcp_addr: SocketAddr,
    pub api_addr: SocketAddr,
    pub fingerprint: String,
    pub relay: Arc<Relay>,
    tasks: JoinSet<Result<()>>,
}

impl Running {
    /// Resolves when a listener fails; the relay keeps running until then.
    pub async fn wait(mut self) -> Result<()> {
        match self.tasks.join_next().await {
            Some(result) => result.context("listener task panicked")?,
            None => Ok(()),
        }
    }
}

/// Binds all listeners and starts serving.
pub async fn start(config: Config, secret: ApiSecret) -> Result<Running> {
    let identity = Identity::load_or_create(&config)?;
    let endpoint = quinn::Endpoint::server(tls::quic_server_config(&identity)?, config.udp_listen)
        .with_context(|| format!("cannot listen on UDP {}", config.udp_listen))?;
    let tcp = tokio::net::TcpListener::bind(config.tcp_listen)
        .await
        .with_context(|| format!("cannot listen on TCP {}", config.tcp_listen))?;
    let api = tokio::net::TcpListener::bind(config.api_listen)
        .await
        .with_context(|| format!("cannot listen on API {}", config.api_listen))?;
    let udp_addr = endpoint.local_addr()?;
    let tcp_addr = tcp.local_addr()?;
    let api_addr = api.local_addr()?;

    let api_tls = if api_addr.ip().is_loopback() {
        None
    } else {
        tracing::warn!(
            "the API listens on non-loopback address {api_addr}; it uses HTTPS with the relay \
             certificate, so the bot must pin its fingerprint"
        );
        Some(tls::api_server_config(&identity)?)
    };
    let relay = Relay::new(
        &config,
        relay::Announced {
            host: config.public_host.clone(),
            udp: udp_addr.port(),
            tcp: tcp_addr.port(),
            fp: identity.fingerprint.clone(),
        },
    );
    let l = &config.limits;
    tracing::info!(
        "mumpeer {} relay for {}: UDP {udp_addr}, TCP {tcp_addr}, API {api_addr}; limits: \
         {} Mbit/s per session, {} Mbit/s total, {} sessions ({} per peer), token TTL {} s, \
         idle {} s",
        env!("CARGO_PKG_VERSION"),
        config.public_host,
        l.session_mbit,
        l.total_mbit,
        l.max_sessions,
        l.max_sessions_per_peer,
        l.token_ttl_s,
        l.idle_timeout_s,
    );
    tracing::info!("relay fingerprint (SHA-256): {}", identity.fingerprint);

    let mut tasks = JoinSet::new();
    tasks.spawn(udp::serve(endpoint, relay.clone()));
    tasks.spawn(tcp::serve(
        tcp,
        tls::relay_server_config(&identity)?,
        relay.clone(),
    ));
    tasks.spawn(api::serve(api, api_tls, Arc::new(secret), relay.clone()));
    tasks.spawn(relay::housekeeping(relay.clone()));
    Ok(Running {
        udp_addr,
        tcp_addr,
        api_addr,
        fingerprint: identity.fingerprint,
        relay,
        tasks,
    })
}

pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, data);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0xf)]));
    }
    out
}

/// Random bytes from the system generator. Failure means the OS has no entropy source, which a
/// relay cannot work without.
pub(crate) fn random<const N: usize>() -> [u8; N] {
    use ring::rand::SecureRandom;
    let mut out = [0u8; N];
    #[allow(clippy::expect_used)]
    ring::rand::SystemRandom::new()
        .fill(&mut out)
        .expect("the system random number generator failed");
    out
}

/// Unpadded base64url, the token format of spec §6.4 (32 bytes → 43 characters).
pub(crate) fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 0x3f) as usize]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings() {
        assert_eq!(hex(&[0x00, 0xab, 0xff]), "00abff");
        assert_eq!(base64url(b"\xfb\xff\xbf"), "-_-_");
        assert_eq!(base64url(b"ab"), "YWI");
        assert_eq!(base64url(&[0u8; 32]).len(), 43);
    }
}
