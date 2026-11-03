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

## Allowed uses

```toml
[uses]
images = true        # full-size image transfers
files = true         # file transfers
screen_share = true  # screen sharing
```

All three are optional and default to `true`, so a config without this section relays
everything. Set a switch to `false` to keep that kind of traffic off your relay.

What users see: the enabled uses are announced to Mumcord through the bot. For a disabled use,
Mumcord does not try the relay. The matching control is disabled with a reason shown, for example
the attach button when `files` is off. Direct connections between users are not affected.

Under the hood the bot API enforces the same switches. `GET /v1/status` lists the enabled uses,
and `POST /v1/pairs` refuses a disabled use with `403 use_not_allowed`. Details in the
[Bot API](https://github.com/Mumcord/mumpeer/blob/main/docs/api.md).

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
max_broadcasts = 4          # screen-share broadcasts at once; 0 = off
max_viewers_per_broadcast = 25  # viewers per broadcast
broadcast_in_mbit = 15      # sharer upload per broadcast
broadcast_out_mbit = 300    # all copies of one broadcast; counts against total_mbit
```

IPv6 sources are counted per /64. Set `total_mbit` below your uplink.

## Broadcasts

A **broadcast** is a screen share that one user shows to many people at once. Without it, the
sharer would have to upload the picture once per viewer, which quickly exceeds a home
connection. With a broadcast the sharer uploads **one** stream to your relay and the relay
copies it to each admitted viewer, up to 25 by default. The sharer's upload stays the same no
matter how many people watch; your relay's outgoing bandwidth carries the copies.

Like pairs, broadcasts are created only by your bot, for users it knows, and end-to-end
encryption still applies: the relay copies packets and cannot read them. Broadcasts live in
memory only.

| Key | Default | Meaning |
| --- | --- | --- |
| `max_broadcasts` | 4 | Broadcasts at the same time. Above it the API answers `503 relay_full`. `0` turns broadcasts off. |
| `max_viewers_per_broadcast` | 25 | Viewers per broadcast. The bot announces this number to clients; a broadcast never gets more, even if asked. |
| `broadcast_in_mbit` | 15 | Upload the sharer may send per broadcast. |
| `broadcast_out_mbit` | 300 | All copies of one broadcast together. Also counted against `total_mbit`. |

Things to know:

- Broadcasts do not count against `max_sessions` or `max_sessions_per_peer`.
- Broadcasts need `screen_share = true` in `[uses]`. With `screen_share = false` or
  `max_broadcasts = 0`, `GET /v1/status` has no `broadcast` field and Mumcord offers no
  broadcast through your relay.
- Size your uplink: one full broadcast at the defaults can send up to 300 Mbit/s out. Keep
  `total_mbit` below what your server can really send, and lower `broadcast_out_mbit` or
  `max_viewers_per_broadcast` if needed.
- **Forwarding the video itself comes in a later release.** This version only manages
  broadcasts and viewers (the bot API and the limits above); the relay does not copy video
  yet.

## Environment

| Variable | Meaning |
| --- | --- |
| `MUMPEER_CONFIG` | Config path |
| `MUMPEER_API_SECRET` | API secret, wins over `api_secret_file` |
| `RUST_LOG` | `info` (default), `debug`, `warn`, `error` |

Next: [[Operating]]
