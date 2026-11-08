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
    #[serde(default)]
    pub uses: Uses,
    #[serde(default)]
    pub plugin_data: PluginData,
}

/// Which Mumcord features may use this relay. A missing key means allowed, so existing configs
/// keep relaying everything, including `plugin_data`; an operator turns it off with
/// `plugin_data = false`.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Uses {
    pub images: bool,
    pub files: bool,
    pub screen_share: bool,
    /// Camera broadcasts only; a pair never carries a camera.
    pub camera: bool,
    pub plugin_data: bool,
}

impl Default for Uses {
    fn default() -> Self {
        Self {
            images: true,
            files: true,
            screen_share: true,
            camera: true,
            plugin_data: true,
        }
    }
}

impl Uses {
    pub const NAMES: [&'static str; 5] =
        ["images", "files", "screen_share", "camera", "plugin_data"];

    /// The enabled uses by their API names, in a fixed order.
    pub fn enabled(&self) -> Vec<&'static str> {
        Self::NAMES
            .into_iter()
            .filter(|name| self.allows(name))
            .collect()
    }

    /// `false` for a disabled or unknown name.
    pub fn allows(&self, name: &str) -> bool {
        match name {
            "images" => self.images,
            "files" => self.files,
            "screen_share" => self.screen_share,
            "camera" => self.camera,
            "plugin_data" => self.plugin_data,
            _ => false,
        }
    }

    /// A pair without a `use` is only allowed when the three original uses are on. It never
    /// covers `plugin_data`, which must always be requested by name, nor `camera`, which is not a
    /// pair use at all.
    pub fn all(&self) -> bool {
        self.images && self.files && self.screen_share
    }
}

/// Limits for plugin data (`use = plugin_data`); only effective when `[uses] plugin_data` is on.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PluginData {
    /// Sustained upload of one sender; announced to clients.
    pub max_kbps_per_sender: u32,
    /// Plugin-data broadcasts at the same time.
    pub max_broadcasts: u32,
    /// Share of `limits.total_mbit` that plugin data may use, in percent.
    pub max_share_percent: u32,
    /// Largest single data unit in bytes.
    pub max_unit_bytes: u32,
}

impl Default for PluginData {
    fn default() -> Self {
        Self {
            max_kbps_per_sender: 512,
            max_broadcasts: 16,
            max_share_percent: 25,
            max_unit_bytes: 65536,
        }
    }
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
    /// 0 turns screen-share broadcasts off.
    pub max_broadcasts: u32,
    /// 0 turns camera broadcasts off; counted separately from `max_broadcasts`.
    pub max_camera_broadcasts: u32,
    /// Announced to clients as `bc`, a u16 on the wire; applies to cameras too.
    pub max_viewers_per_broadcast: u32,
    /// The publisher's upload of one broadcast.
    pub broadcast_in_mbit: u32,
    /// All copies of one broadcast together; also counted against `total_mbit`.
    pub broadcast_out_mbit: u32,
    /// The publisher's upload of one camera broadcast, instead of `broadcast_in_mbit`.
    pub camera_max_kbps: u32,
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
            max_broadcasts: 4,
            max_camera_broadcasts: 18,
            max_viewers_per_broadcast: 25,
            broadcast_in_mbit: 15,
            broadcast_out_mbit: 300,
            camera_max_kbps: 2500,
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
            ("max_viewers_per_broadcast", l.max_viewers_per_broadcast),
            ("broadcast_in_mbit", l.broadcast_in_mbit),
            ("broadcast_out_mbit", l.broadcast_out_mbit),
            ("camera_max_kbps", l.camera_max_kbps),
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
        // Announced to clients as the u16 `bc` (spec 07 §6.6).
        if l.max_viewers_per_broadcast > u32::from(u16::MAX) {
            bail!(
                "limits.max_viewers_per_broadcast must be at most {}",
                u16::MAX
            );
        }
        let p = &self.plugin_data;
        for (name, value) in [
            ("max_kbps_per_sender", p.max_kbps_per_sender),
            ("max_broadcasts", p.max_broadcasts),
            ("max_share_percent", p.max_share_percent),
            ("max_unit_bytes", p.max_unit_bytes),
        ] {
            if value == 0 {
                bail!("plugin_data.{name} must be greater than 0");
            }
        }
        if p.max_share_percent > 100 {
            bail!("plugin_data.max_share_percent must be at most 100");
        }
        // One unit is buffered whole by the data plane, so its size needs a sane upper bound.
        if p.max_unit_bytes > 1_048_576 {
            bail!("plugin_data.max_unit_bytes must be at most 1048576");
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
        assert_eq!(
            (
                l.max_broadcasts,
                l.max_viewers_per_broadcast,
                l.broadcast_in_mbit,
                l.broadcast_out_mbit
            ),
            (4, 25, 15, 300)
        );
        assert_eq!((l.max_camera_broadcasts, l.camera_max_kbps), (18, 2500));
        assert!(config.uses.camera);
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
        let limit =
            |line: &str| Config::parse(&format!("public_host = \"a.example\"\n[limits]\n{line}"));
        assert!(limit("max_broadcasts = 0").is_ok());
        assert!(limit("max_viewers_per_broadcast = 0").is_err());
        assert!(limit("max_viewers_per_broadcast = 65536").is_err());
        assert!(limit("max_viewers_per_broadcast = 65535").is_ok());
        assert!(limit("broadcast_in_mbit = 0").is_err());
        assert!(limit("broadcast_out_mbit = 0").is_err());
        assert!(limit("max_camera_broadcasts = 0").is_ok());
        assert!(limit("camera_max_kbps = 0").is_err());
    }

    #[test]
    fn uses_default_to_all_and_can_be_disabled() {
        let config = Config::parse(MINIMAL).unwrap();
        assert_eq!(
            config.uses.enabled(),
            ["images", "files", "screen_share", "camera", "plugin_data"]
        );
        let config = Config::parse("public_host = \"a.example\"\n[uses]\nfiles = false").unwrap();
        assert_eq!(
            config.uses.enabled(),
            ["images", "screen_share", "camera", "plugin_data"]
        );
        assert!(!config.uses.all());
        // Cameras are not part of what a pair without `use` needs.
        let config = Config::parse("public_host = \"a.example\"\n[uses]\ncamera = false").unwrap();
        assert_eq!(
            config.uses.enabled(),
            ["images", "files", "screen_share", "plugin_data"]
        );
        assert!(config.uses.all());
        assert!(Config::parse("public_host = \"a.example\"\n[uses]\nvideo = true").is_err());
        assert!(Config::parse("public_host = \"a.example\"\n[uses]\nfiles = 1").is_err());
    }

    #[test]
    fn plugin_data_is_on_by_default_and_can_be_disabled() {
        let config = Config::parse("public_host = \"a.example\"").unwrap();
        assert!(config.uses.allows("plugin_data"));
        assert!(config.uses.all());
        let off =
            Config::parse("public_host = \"a.example\"\n[uses]\nplugin_data = false").unwrap();
        assert!(!off.uses.allows("plugin_data"));
        assert!(off.uses.all());
        let on = Config::parse("public_host = \"a.example\"").unwrap();
        assert_eq!(
            on.uses.enabled(),
            ["images", "files", "screen_share", "camera", "plugin_data"]
        );
        assert_eq!(on.plugin_data, PluginData::default());
    }

    #[test]
    fn plugin_data_limits_are_validated() {
        let parse = |body: &str| {
            Config::parse(&format!(
                "public_host = \"a.example\"\n[plugin_data]\n{body}"
            ))
        };
        assert!(parse("max_kbps_per_sender = 128\nmax_share_percent = 100").is_ok());
        for bad in [
            "max_kbps_per_sender = 0",
            "max_broadcasts = 0",
            "max_share_percent = 0",
            "max_share_percent = 101",
            "max_unit_bytes = 0",
            "max_unit_bytes = 1048577",
            "speed = 1",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
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
