//! Relay identity and TLS settings.
//!
//! The relay has no WebPKI certificate: clients pin the SHA-256 fingerprint of its certificate
//! from the bot's announcement. Clients authenticate with their Mumble client certificate, which
//! is self-signed too; the TLS handshake proves they own its key, and the relay identifies them
//! by the Mumble certificate hash (SHA-1 of the DER, lowercase hex).

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use quinn::{IdleTimeout, TransportConfig, VarInt};
use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

use crate::config::Config;
use crate::{hex, sha256};

/// ALPN of the relay protocol on both transports.
pub const ALPN: &[u8] = b"mumpeer/1";

/// Outer QUIC packets must carry one inner 1200-byte QUIC packet plus overhead (spec §6.5). A path
/// that cannot fails the UDP attempt, and the client falls back to TCP.
const OUTER_MTU: u16 = 1300;

const KEY_FILE: &str = "relay-key.pem";
const CERT_FILE: &str = "relay-cert.pem";

pub struct Identity {
    pub chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
    /// SHA-256 of the leaf certificate DER, lowercase hex; what clients pin.
    pub fingerprint: String,
}

impl Identity {
    /// The operator's own certificate if configured, else the one in `data_dir`, generated on
    /// first start.
    pub fn load_or_create(config: &Config) -> Result<Self> {
        let (cert_path, key_path) = match (&config.cert_file, &config.key_file) {
            (Some(cert), Some(key)) => (cert.clone(), key.clone()),
            _ => {
                let dir = &config.data_dir;
                let paths = (dir.join(CERT_FILE), dir.join(KEY_FILE));
                if !paths.0.exists() || !paths.1.exists() {
                    generate(dir, &config.public_host, &paths.0, &paths.1)?;
                }
                paths
            }
        };
        let chain = CertificateDer::pem_file_iter(&cert_path)
            .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
            .with_context(|| format!("cannot read certificate {}", cert_path.display()))?;
        let key = PrivateKeyDer::from_pem_file(&key_path)
            .with_context(|| format!("cannot read key {}", key_path.display()))?;
        let leaf = chain
            .first()
            .with_context(|| format!("no certificate in {}", cert_path.display()))?;
        let fingerprint = hex(&sha256(leaf.as_ref()));
        Ok(Self {
            chain,
            key,
            fingerprint,
        })
    }
}

/// A new ECDSA P-256 key and self-signed certificate. The key file is private (0600) because it
/// is the relay's identity that clients pin.
fn generate(dir: &Path, host: &str, cert_path: &Path, key_path: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot restrict {}", dir.display()))?;
    let certified = rcgen::generate_simple_self_signed(vec![host.to_owned()])
        .context("cannot generate the relay certificate")?;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(key_path)
        .and_then(|mut file| file.write_all(certified.signing_key.serialize_pem().as_bytes()))
        .with_context(|| format!("cannot write {}", key_path.display()))?;
    fs::write(cert_path, certified.cert.pem())
        .with_context(|| format!("cannot write {}", cert_path.display()))?;
    tracing::info!("generated a new relay identity in {}", dir.display());
    Ok(())
}

/// The Mumble certificate hash of a client certificate: SHA-1 of the DER, lowercase hex.
pub fn mumble_hash(cert: &CertificateDer<'_>) -> String {
    hex(ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, cert.as_ref()).as_ref())
}

/// Requires a client certificate and checks only that the client owns its key: Mumble client
/// certificates are self-signed, and access is decided afterwards by the certificate hash.
#[derive(Debug)]
struct AnyClientCert {
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for AnyClientCert {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// TLS 1.3 with a mandatory client certificate and ALPN `mumpeer/1`; used for TCP and QUIC.
pub fn relay_server_config(identity: &Identity) -> Result<rustls::ServerConfig> {
    let provider = provider();
    let mut config = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(AnyClientCert { provider }))
        .with_single_cert(identity.chain.clone(), identity.key.clone_key())?;
    config.alpn_protocols = vec![ALPN.to_vec()];
    Ok(config)
}

/// HTTPS for an API bound to a non-loopback address; the bot pins the same fingerprint.
pub fn api_server_config(identity: &Identity) -> Result<rustls::ServerConfig> {
    Ok(rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(identity.chain.clone(), identity.key.clone_key())?)
}

pub fn quic_server_config(identity: &Identity) -> Result<quinn::ServerConfig> {
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(relay_server_config(identity)?)
        .context("TLS config unusable for QUIC")?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    let mut transport = TransportConfig::default();
    transport
        // Keeps a waiting side alive until its partner binds (up to the token TTL).
        .keep_alive_interval(Some(Duration::from_secs(10)))
        .max_idle_timeout(Some(IdleTimeout::from(VarInt::from_u32(30_000))))
        .initial_mtu(OUTER_MTU)
        .min_mtu(OUTER_MTU)
        .mtu_discovery_config(None)
        // One control stream per connection, opened by the client.
        .max_concurrent_bidi_streams(VarInt::from_u32(1))
        .max_concurrent_uni_streams(VarInt::from_u32(0));
    config.transport_config(Arc::new(transport));
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_a_private_identity_once() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let config = Config::parse(&format!(
            "public_host = \"relay.test\"\ndata_dir = \"{}\"",
            data.display()
        ))
        .unwrap();
        let first = Identity::load_or_create(&config).unwrap();
        assert_eq!(first.fingerprint.len(), 64);
        let mode = fs::metadata(data.join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let second = Identity::load_or_create(&config).unwrap();
        assert_eq!(first.fingerprint, second.fingerprint);
    }

    #[test]
    fn mumble_hash_is_sha1_hex() {
        let cert = CertificateDer::from(b"abc".to_vec());
        assert_eq!(
            mumble_hash(&cert),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }
}
