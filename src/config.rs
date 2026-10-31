//! The relay's TOML config (`mumpeer.toml`) and the API secret.
//!
//! The secret never lives in the config file: it comes from `MUMPEER_API_SECRET` or from
//! `api_secret_file`, so a config can be shared or committed without leaking it.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::sha256;

/// Environment variable that holds the API secret; wins over `api_secret_file`.
pub const SECRET_ENV: &str = "MUMPEER_API_SECRET";

/// Shorter secrets could be guessed; the bot side enforces the same minimum.
const MIN_SECRET_LEN: usize = 32;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Address announced to clients (DNS name or IP literal).
    pub public_host: String,
    #[serde(default = "default_relay_listen")]
    pub udp_listen: SocketAddr,
    #[serde(default = "default_relay_listen")]
    pub tcp_listen: SocketAddr,
    #[serde(default = "default_api_listen")]
    pub api_listen: SocketAddr,
    /// Holds the generated relay key and certificate.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    pub api_secret_file: Option<PathBuf>,
    /// The operator's own certificate chain and key (PEM) instead of the generated identity.
    pub cert_file: Option<PathBuf>,
    pub key_file: Option<PathBuf>,
    #[serde(default)]
    pub limits: Limits,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Per session and direction.
    pub session_mbit: u32,
    /// All sessions and directions together.
    pub total_mbit: u32,
    pub max_sessions: u32,
    pub max_sessions_per_peer: u32,
    pub token_ttl_s: u32,
    pub idle_timeout_s: u32,
    /// 0 = unlimited.
    pub max_session_s: u32,
    pub conn_per_ip_per_min: u32,
    pub failed_per_ip_per_min: u32,
    pub pairs_per_min: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            session_mbit: 10,
            total_mbit: 200,
            max_sessions: 50,
            max_sessions_per_peer: 4,
            token_ttl_s: 120,
            idle_timeout_s: 60,
            max_session_s: 0,
            conn_per_ip_per_min: 20,
            failed_per_ip_per_min: 10,
            pairs_per_min: 120,
        }
    }
}

fn default_relay_listen() -> SocketAddr {
    SocketAddr::from(([0u16; 8], 443))
}

fn default_api_listen() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 7480))
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/mumpeer")
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid config {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if !valid_public_host(&self.public_host) {
            bail!("public_host must be a DNS name or an IP address clients can reach");
        }
        if self.cert_file.is_some() != self.key_file.is_some() {
            bail!("cert_file and key_file must be set together");
        }
        let l = &self.limits;
        let must_be_positive = [
            ("session_mbit", l.session_mbit),
            ("total_mbit", l.total_mbit),
            ("max_sessions", l.max_sessions),
            ("max_sessions_per_peer", l.max_sessions_per_peer),
            ("token_ttl_s", l.token_ttl_s),
            ("idle_timeout_s", l.idle_timeout_s),
            ("conn_per_ip_per_min", l.conn_per_ip_per_min),
            ("failed_per_ip_per_min", l.failed_per_ip_per_min),
            ("pairs_per_min", l.pairs_per_min),
        ];
        for (name, value) in must_be_positive {
            if value == 0 {
                bail!("limits.{name} must be greater than 0");
            }
        }
        // Clients reject a `ttl` outside u16 (spec §6.4).
        if l.token_ttl_s > u32::from(u16::MAX) {
            bail!("limits.token_ttl_s must be at most {}", u16::MAX);
        }
        Ok(())
    }
}

/// The same rule clients apply to the announced host: a DNS name of LDH labels or an IP literal
/// that is not unspecified, multicast or broadcast.
fn valid_public_host(host: &str) -> bool {
    if let Ok(ip) = host.parse::<IpAddr>() {
        let broadcast = matches!(ip, IpAddr::V4(v4) if v4.is_broadcast());
        return !ip.is_unspecified() && !ip.is_multicast() && !broadcast;
    }
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// The bot's API secret. Only its SHA-256 is kept, so the secret itself is never in memory for
/// long and cannot end up in a log or a debug print.
pub struct ApiSecret([u8; 32]);

impl std::fmt::Debug for ApiSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiSecret(..)")
    }
}

impl ApiSecret {
    pub fn new(secret: &str) -> Result<Self> {
        // A trailing newline from `echo … > file` is not part of the secret.
        let secret = secret.trim_end();
        if secret.len() < MIN_SECRET_LEN {
            bail!("the API secret must be at least {MIN_SECRET_LEN} characters long");
        }
        Ok(Self(sha256(secret.as_bytes())))
    }

    /// From the environment value if given, else from `api_secret_file`.
    pub fn load(config: &Config, env: Option<String>) -> Result<Self> {
        if let Some(secret) = env {
            return Self::new(&secret).with_context(|| format!("invalid {SECRET_ENV}"));
        }
        let Some(path) = &config.api_secret_file else {
            bail!("no API secret: set {SECRET_ENV} or api_secret_file");
        };
        let secret = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read api_secret_file {}", path.display()))?;
        Self::new(&secret).with_context(|| format!("invalid secret in {}", path.display()))
    }

    /// Constant-time check: both sides are fixed-length digests, so the time does not depend on
    /// how much of the guess was right.
    pub fn matches(&self, candidate: &str) -> bool {
        let digest = sha256(candidate.as_bytes());
        self.0
            .iter()
            .zip(digest)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"public_host = "relay.example.org""#;

    #[test]
    fn defaults_match_the_spec() {
        let config = Config::parse(MINIMAL).unwrap();
        assert_eq!(config.udp_listen.port(), 443);
        assert_eq!(config.tcp_listen.port(), 443);
        assert_eq!(config.api_listen, "127.0.0.1:7480".parse().unwrap());
        let l = config.limits;
        assert_eq!(
            (l.session_mbit, l.total_mbit, l.max_sessions),
            (10, 200, 50)
        );
        assert_eq!(
            (l.max_sessions_per_peer, l.token_ttl_s, l.idle_timeout_s),
            (4, 120, 60)
        );
        assert_eq!(l.max_session_s, 0);
        assert_eq!(
            (
                l.conn_per_ip_per_min,
                l.failed_per_ip_per_min,
                l.pairs_per_min
            ),
            (20, 10, 120)
        );
    }

    #[test]
    fn the_example_config_is_valid() {
        let example = include_str!("../mumpeer.example.toml");
        let config = Config::parse(example).unwrap();
        assert_eq!(config.public_host, "relay.example.org");
    }

    #[test]
    fn rejects_bad_values() {
        assert!(Config::parse(r#"public_host = "0.0.0.0""#).is_err());
        assert!(Config::parse(r#"public_host = "bad_host.example""#).is_err());
        assert!(Config::parse(r#"public_host = """#).is_err());
        assert!(Config::parse("public_host = \"a.example\"\nsecret = \"x\"").is_err());
        assert!(Config::parse("public_host = \"a.example\"\n[limits]\nmax_sessions = 0").is_err());
        assert!(Config::parse("public_host = \"a.example\"\ncert_file = \"c.pem\"").is_err());
        assert!(Config::parse(r#"public_host = "127.0.0.1""#).is_ok());
    }

    #[test]
    fn secret_rules() {
        assert!(ApiSecret::new("short").is_err());
        let secret = ApiSecret::new(&format!("{}\n", "s".repeat(32))).unwrap();
        assert!(secret.matches(&"s".repeat(32)));
        assert!(!secret.matches(&"s".repeat(31)));
        assert_eq!(format!("{secret:?}"), "ApiSecret(..)");
    }
}
