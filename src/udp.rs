//! QUIC over UDP, the main relay transport (spec §6.5): the client opens one bidirectional stream
//! for newline-delimited control JSON; inner packets travel as QUIC datagrams.

use std::net::IpAddr;
use std::sync::Arc;

use quinn::{Connection, Endpoint, Incoming};
use rustls::pki_types::CertificateDer;
use tokio::time::timeout;

use crate::relay::{Link, Refusal, Relay};
use crate::tcp::{BIND_TIMEOUT, HANDSHAKE_TIMEOUT, MAX_CONTROL};
use crate::tls::{ALPN, mumble_hash};

pub async fn serve(endpoint: Endpoint, relay: Arc<Relay>) -> anyhow::Result<()> {
    while let Some(incoming) = endpoint.accept().await {
        // A stateless retry proves the source address first, so spoofed packets cannot use up
        // someone else's connection or failure budget.
        if !incoming.remote_address_validated() {
            if let Err(e) = incoming.retry() {
                e.into_incoming().ignore();
            }
            continue;
        }
        let ip = incoming.remote_address().ip();
        if !relay.sources.admit(ip) {
            incoming.refuse();
            continue;
        }
        tokio::spawn(handle(incoming, ip, relay.clone()));
    }
    anyhow::bail!("the QUIC endpoint closed")
}

async fn handle(incoming: Incoming, ip: IpAddr, relay: Arc<Relay>) {
    let handshake = async { incoming.accept()?.await };
    let conn = match timeout(HANDSHAKE_TIMEOUT, handshake).await {
        Ok(Ok(conn)) => conn,
        _ => {
            relay.sources.failed(ip);
            return;
        }
    };
    let Some(hash) = client_hash(&conn).filter(|_| alpn(&conn).as_deref() == Some(ALPN)) else {
        relay.sources.failed(ip);
        conn.close(1u32.into(), Refusal::BadRequest.code().as_bytes());
        return;
    };
    if !relay.allowed(&hash) {
        relay.sources.failed(ip);
        conn.close(1u32.into(), Refusal::NotAllowed.code().as_bytes());
        return;
    }
    let bind = async {
        let (ctl, mut recv) = conn.accept_bi().await.ok()?;
        let mut buf = [0u8; MAX_CONTROL + 1];
        let mut filled = 0;
        loop {
            let read = recv.read(&mut buf[filled..]).await.ok()??;
            let start = filled;
            filled += read;
            if let Some(end) = buf[start..filled].iter().position(|&b| b == b'\n') {
                return Some((ctl, buf, start + end));
            }
            if filled == buf.len() {
                return Some((ctl, buf, filled));
            }
        }
    };
    match timeout(BIND_TIMEOUT, bind).await {
        Ok(Some((ctl, buf, len))) if len <= MAX_CONTROL => {
            let link = Link::Quic { conn, ctl };
            relay.serve(link, &buf[..len], &hash, ip).await;
        }
        Ok(Some((ctl, _, _))) => {
            relay.sources.failed(ip);
            Link::Quic { conn, ctl }.fail(Refusal::BadRequest).await;
        }
        _ => {
            relay.sources.failed(ip);
            conn.close(1u32.into(), Refusal::BadRequest.code().as_bytes());
        }
    }
}

fn client_hash(conn: &Connection) -> Option<String> {
    let certs = conn
        .peer_identity()?
        .downcast::<Vec<CertificateDer<'static>>>()
        .ok()?;
    certs.first().map(mumble_hash)
}

fn alpn(conn: &Connection) -> Option<Vec<u8>> {
    conn.handshake_data()?
        .downcast::<quinn::crypto::rustls::HandshakeData>()
        .ok()?
        .protocol
}
