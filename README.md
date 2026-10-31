# mumpeer

mumpeer is a small relay server for [Mumcord](https://github.com/Mumcord/mumcord)'s peer-to-peer
features (screen sharing, full-size images). When two Mumcord users cannot connect directly
(carrier-grade NAT, strict firewalls, networks that block UDP) or a user wants to hide their IP
address from the other person, both connect to the relay instead, and it forwards their
end-to-end encrypted packets to each other, like a TURN server.

A server community runs mumpeer next to its Mumble server, together with the relay bot
`mumbot peer` ([Mumcord/mumbot](https://github.com/Mumcord/mumbot)). The bot tells Mumcord
clients that the relay exists and creates short-lived tokens for each pair of users through
mumpeer's API. The Mumble server itself stays unmodified, and Mumcord works fully without bot
and relay.

## How it works

- Clients connect over **QUIC on UDP** or, in networks that block UDP, **TLS over TCP**. Both
  use TLS 1.3 with the user's Mumble client certificate; clients pin the relay's certificate by
  its SHA-256 fingerprint, which the bot announces.
- The bot creates a **pair** through the API for two certificate hashes and gets a token. Only
  those two certificates can use that token, each side once, within the token lifetime (default
  120 s). Afterwards the relay forwards packets between exactly those two connections.
- The relay never forwards anywhere else and never opens connections itself, so it cannot be
  used as an open proxy.
- The forwarded packets are an inner QUIC connection between the two clients, encrypted end to
  end and pinned to their Mumble certificates. The relay cannot read or change them.

## Running with Docker

1. Create a secret for the API (at least 32 random characters) and a config:

   ```sh
   mkdir -p mumpeer && cd mumpeer
   openssl rand -hex 32 > api_secret && chmod 600 api_secret
   curl -fsSLO https://raw.githubusercontent.com/Mumcord/mumpeer/main/mumpeer.example.toml
   mv mumpeer.example.toml mumpeer.toml   # then set public_host
   ```

   `public_host` is the DNS name or IP address clients connect to. Keep `api_secret_file =
   "/run/secrets/mumpeer_api_secret"` (or pass the secret in `MUMPEER_API_SECRET`). The secret
   never goes into the config file.

2. Start the relay. The bot reaches the API on `127.0.0.1:7480` inside the relay's network, so
   run it in the same network namespace (`network_mode: "service:mumpeer"` in Docker Compose):

   ```yaml
   services:
     mumpeer:
       image: ghcr.io/mumcord/mumpeer:latest
       restart: unless-stopped
       ports: ["443:443/udp", "443:443/tcp"]
       volumes:
         - ./mumpeer.toml:/config/mumpeer.toml:ro
         - ./api_secret:/run/secrets/mumpeer_api_secret:ro
         - mumpeer-data:/var/lib/mumpeer
     mumbot:
       image: ghcr.io/mumcord/mumbot:latest
       network_mode: "service:mumpeer"
       command: ["peer", "--config", "/config/bot.toml"]
       # bot.toml: see the mumbot documentation of the peer module (api_url = "http://127.0.0.1:7480")
   volumes:
     mumpeer-data:
   ```

   The container runs as an unprivileged user. Keep the `mumpeer-data` volume: it holds the
   relay key, and a new key means a new fingerprint, which clients treat as "the relay changed"
   until each user trusts it again.

3. On first start the log shows `relay fingerprint (SHA-256): …`. The bot announces it; nothing
   else needs to be shared.

Without Docker, run the static binary from the releases as `mumpeer --config mumpeer.toml`
(or set `MUMPEER_CONFIG`).

## Firewall

Open UDP and TCP port 443 (or the ports in `udp_listen` and `tcp_listen`). Do not expose the
API port. If the API must listen on another address than loopback, mumpeer serves it over HTTPS
with the relay certificate and logs a warning; the bot then pins the fingerprint (`api_fp`).

## Configuration

See [`mumpeer.example.toml`](mumpeer.example.toml) for every key and its default. Limits:

| Key | Default | Meaning |
|---|---|---|
| `session_mbit` | 10 | Bandwidth per session and direction; above it packets are dropped |
| `total_mbit` | 200 | Bandwidth of all sessions together |
| `max_sessions` | 50 | Sessions at the same time (`relay_full` above) |
| `max_sessions_per_peer` | 4 | Sessions per client certificate (`peer_limit` above) |
| `token_ttl_s` | 120 | Time for both sides to connect with a new token |
| `idle_timeout_s` | 60 | A session without packets ends after this time |
| `max_session_s` | 0 | Maximum session length, 0 = unlimited |
| `conn_per_ip_per_min` | 20 | New connections per source address and minute |
| `failed_per_ip_per_min` | 10 | Failed handshakes or API logins per source address and minute; then blocked for 10 minutes |
| `pairs_per_min` | 120 | Pairs the bot may create per minute |

IPv6 sources are counted per /64.

## What the operator can see

The relay has to know where to send packets, so its operator can see:

- the IP addresses of both users of a session,
- which two certificate hashes talk to each other,
- how much data flows and when.

It cannot see the contents: they are encrypted end to end between the two users. Mumcord tells
users this before they use a relay.

Nothing is written to disk except the relay key and certificate. Pairs, sessions and counters
are in memory only.

## What is logged

At the default level (`RUST_LOG=info`): start, config summary, listener addresses, the relay
fingerprint, totals every five minutes, and one line per finished session (random session id,
transports, duration, bytes each way, dropped packets, end reason). Warnings for rate-limit
blocks and failed API logins show only the source network (/24 for IPv4, /48 for IPv6).
`RUST_LOG=debug` adds the first 8 characters of certificate hashes.

Never logged: full IP addresses, full certificate hashes, tokens, the API secret, packet
contents.

## API (for the bot)

HTTP/1.1 with JSON. Every request needs `Authorization: Bearer <secret>`; a wrong or missing
secret returns `401` and counts as a failed login of that address.

| Request | Response |
|---|---|
| `GET /v1/status` | `200 {"v":1,"boot_id","host","udp","tcp","fp","sessions","max_sessions"}` |
| `POST /v1/pairs` with `{"a":"<40 hex>","b":"<40 hex>"}` | `201 {"tok","expires_in"}`, `400`, `429 rate_limited`, `503 relay_full` |
| `DELETE /v1/peers/{hash}` | `204`; ends all pairs and sessions of that certificate |

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test                      # includes end-to-end relay tests on loopback
docker build -t mumpeer:dev .
```

`testenv/` holds a local test environment with a Mumble server, the relay bot and mumpeer, plus
network namespaces that block direct connections or UDP; see [`testenv/README.md`](testenv/README.md).

## Releasing

Maintainers only. A release is built by pushing a version tag; publishing stays a manual step.

1. Set the new version in `Cargo.toml`, run `cargo check` so `Cargo.lock` follows, and move the
   `[Unreleased]` entries in `CHANGELOG.md` under the new version. Merge this to `main`.
2. Tag the merge commit and push the tag: `git tag v0.2.0 && git push origin v0.2.0`.
3. The `Release` workflow checks that the tag matches the Cargo version and points to a commit
   on `main`, builds static Linux binaries (x86_64 and aarch64), and pushes the multi-arch image
   to `ghcr.io/mumcord/mumpeer` with the version tag. Plain versions also move `latest`;
   pre-release tags such as `v0.2.0-rc.1` do not.
4. The workflow creates a **draft** GitHub release with the binaries and `SHA256SUMS`. Review it
   and publish it by hand.

## License

GNU Affero General Public License v3.0, see [`LICENSE`](LICENSE).
