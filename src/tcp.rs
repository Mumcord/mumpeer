//! TLS over TCP for networks that block UDP (spec §6.5). Frames: 1 byte type, 2 bytes big-endian
//! length, payload; type 0 is a control JSON message, type 1 an inner packet.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;

use crate::relay::{Link, Refusal, Relay};
use crate::tls::{ALPN, mumble_hash};

pub const FRAME_CONTROL: u8 = 0;
pub const FRAME_PACKET: u8 = 1;
pub const MAX_FRAME: usize = 1500;
/// Control messages are small; a bigger first frame is not a `bind`.
pub const MAX_CONTROL: usize = 512;
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// The relay closes a connection that sends no valid `bind` within this time.
pub const BIND_TIMEOUT: Duration = Duration::from_secs(5);

/// Reads one frame into `buf`; returns its type and length. A length over [`MAX_FRAME`] is an
/// error, so a client cannot make the relay buffer more.
pub async fn read_frame<R: AsyncRead + Unpin>(
    rd: &mut R,
    buf: &mut [u8; MAX_FRAME],
) -> std::io::Result<(u8, usize)> {
    let mut header = [0u8; 3];
    rd.read_exact(&mut header).await?;
    let len = usize::from(u16::from_be_bytes([header[1], header[2]]));
    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    rd.read_exact(&mut buf[..len]).await?;
    Ok((header[0], len))
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    wr: &mut W,
    kind: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let len = u16::try_from(payload.len())
        .ok()
        .filter(|&len| usize::from(len) <= MAX_FRAME)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame too large"))?;
    // One write per frame, so header and payload end up in the same TLS record.
    let mut frame = [0u8; 3 + MAX_FRAME];
    frame[0] = kind;
    frame[1..3].copy_from_slice(&len.to_be_bytes());
    frame[3..3 + payload.len()].copy_from_slice(payload);
    wr.write_all(&frame[..3 + payload.len()]).await?;
    wr.flush().await
}

pub async fn serve(
    listener: TcpListener,
    config: rustls::ServerConfig,
    relay: Arc<Relay>,
) -> anyhow::Result<()> {
    let acceptor = TlsAcceptor::from(Arc::new(config));
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                // Usually out of file descriptors; give closing connections a moment.
                tracing::warn!("TCP accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let ip = peer.ip();
        if !relay.sources.admit(ip) {
            continue;
        }
        tokio::spawn(handle(acceptor.clone(), stream, ip, relay.clone()));
    }
}

async fn handle(acceptor: TlsAcceptor, stream: TcpStream, ip: IpAddr, relay: Arc<Relay>) {
    // Inner packets are latency sensitive and already sized; never wait to coalesce them.
    if let Err(e) = stream.set_nodelay(true) {
        tracing::debug!("cannot disable Nagle: {e}");
    }
    let stream = match timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
        Ok(Ok(stream)) => stream,
        _ => {
            relay.sources.failed(ip);
            return;
        }
    };
    let (_, conn) = stream.get_ref();
    let hash = conn
        .peer_certificates()
        .and_then(|certs| certs.first())
        .map(mumble_hash);
    let (Some(hash), true) = (hash, conn.alpn_protocol() == Some(ALPN)) else {
        relay.sources.failed(ip);
        return;
    };
    let (mut rd, wr) = tokio::io::split(stream);
    if !relay.allowed(&hash) {
        relay.sources.failed(ip);
        Link::Tcp { rd, wr }.fail(Refusal::NotAllowed).await;
        return;
    }
    let mut buf = [0u8; MAX_FRAME];
    match timeout(BIND_TIMEOUT, read_frame(&mut rd, &mut buf)).await {
        Ok(Ok((FRAME_CONTROL, len))) if len <= MAX_CONTROL => {
            relay
                .serve(Link::Tcp { rd, wr }, &buf[..len], &hash, ip)
                .await;
        }
        _ => {
            relay.sources.failed(ip);
            Link::Tcp { rd, wr }.fail(Refusal::BadRequest).await;
        }
    }
}
