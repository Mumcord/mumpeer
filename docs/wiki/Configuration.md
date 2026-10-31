# Configuration

mumpeer reads one TOML file: `--config <path>`, else `$MUMPEER_CONFIG`, else
`/etc/mumpeer/mumpeer.toml` (the Docker image uses `/config/mumpeer.toml`). Unknown keys and
invalid values stop it at start with a clear error. Only `public_host` is required.

Every key with its default and all details:
[configuration reference](https://github.com/Mumcord/mumpeer/blob/main/docs/configuration.md).

## The essentials

```toml
public_host = "relay.example.org"   # what clients connect to: DNS name or IP address
udp_listen = "[::]:443"             # QUIC relay
tcp_listen = "[::]:443"             # TLS relay, for networks that block UDP
api_listen = "127.0.0.1:7480"       # bot API; keep it on loopback
data_dir = "/var/lib/mumpeer"       # relay key and certificate; keep it
api_secret_file = "/run/secrets/mumpeer_api_secret"
```

- **The secret** (at least 32 characters, `openssl rand -hex 32`) comes from
  `MUMPEER_API_SECRET` or the file in `api_secret_file`, never from the config itself. The
  bot needs the same value.
- **Ports** are announced exactly as mumpeer listens on them. With Docker, publish them under
  the same number (`443:443`).
- **Own certificate:** `cert_file` and `key_file` replace the generated identity. Every renewal
  changes the fingerprint users pinned, so prefer the generated one.

## Limits

```toml
[limits]
session_mbit = 10           # per session and direction; above it packets are dropped
total_mbit = 200            # all sessions together
max_sessions = 50           # then relay_full
max_sessions_per_peer = 4   # per client certificate; then peer_limit
token_ttl_s = 120           # both sides must connect within this time
idle_timeout_s = 60         # a session without packets ends
max_session_s = 0           # 0 = unlimited
conn_per_ip_per_min = 20    # new connections per source address
failed_per_ip_per_min = 10  # then the address is blocked for 10 minutes
pairs_per_min = 120         # pairs the bot may create
```

IPv6 sources are counted per /64. Set `total_mbit` below your uplink.

## Environment

| Variable | Meaning |
| --- | --- |
| `MUMPEER_CONFIG` | Config path |
| `MUMPEER_API_SECRET` | API secret, wins over `api_secret_file` |
| `RUST_LOG` | `info` (default), `debug`, `warn`, `error` |

Next: [[Operating]]
