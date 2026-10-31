# Getting started

## Try it on one machine

mumbot ships a ready-made Docker Compose example with a standard Mumble server, mumpeer and the
relay bot:
[mumbot `examples/peer-with-relay`](https://github.com/Mumcord/mumbot/tree/main/examples/peer-with-relay).
Follow its README; it takes a few minutes and needs only Docker and `openssl`.

## Set it up for your community

You need a Linux host (x86_64 or aarch64) that the internet can reach on **UDP and TCP 443**,
ideally with a DNS name such as `relay.example.org`, and a Mumble server where the relay bot
can register.

### 1. Secret and config

```sh
mkdir mumpeer && cd mumpeer
echo "PEER_API_SECRET=$(openssl rand -hex 32)" > .env && chmod 600 .env
curl -fsSL -o mumpeer.toml https://raw.githubusercontent.com/Mumcord/mumpeer/main/mumpeer.example.toml
```

Edit `mumpeer.toml` and set `public_host = "relay.example.org"`. Everything else has a good
default. The secret never goes into the config.

### 2. Start mumpeer and the bot

`docker-compose.yml`:

```yaml
services:
  mumpeer:
    image: ghcr.io/mumcord/mumpeer:0.1.0
    restart: unless-stopped
    environment:
      MUMPEER_API_SECRET: ${PEER_API_SECRET:?set PEER_API_SECRET in .env}
    ports: ["443:443/udp", "443:443/tcp"]
    volumes:
      - ./mumpeer.toml:/config/mumpeer.toml:ro
      - mumpeer-data:/var/lib/mumpeer

  mumbot:
    image: ghcr.io/mumcord/mumbot:0.1.0
    command: ["peer"]
    restart: unless-stopped
    network_mode: "service:mumpeer"   # reaches the API on 127.0.0.1:7480
    environment:
      MUMBOT_CONFIG: /config/bot.toml
      MUMBOT_PEER_API_SECRET: ${PEER_API_SECRET:?set PEER_API_SECRET in .env}
    volumes:
      - ./bot.toml:/config/bot.toml:ro
      - mumbot-data:/data

volumes:
  mumpeer-data:
  mumbot-data:
```

Write `bot.toml` as described in
[mumbot's peer documentation](https://github.com/Mumcord/mumbot/blob/main/docs/peer.md)
(Mumble server address, server certificate pin, `[peer] api_url = "http://127.0.0.1:7480"`),
and register the bot on the Mumble server once. Then:

```sh
docker compose up -d
docker compose logs mumpeer | grep fingerprint
```

### 3. Check it

- The relay log shows `relay fingerprint (SHA-256): …`. You do not have to share it: the bot
  announces it.
- In Mumcord, the server info panel shows "Mumpeer relay: Yes" with the state "available".
- Two users share a screen while one of them hides their IP address (or a direct connection
  is impossible); the connection info shows "Via relay (UDP)".

**Keep the `mumpeer-data` volume.** It holds the relay key. A new key means a new fingerprint,
and every user would see "The relay of this server changed".

## Without Docker

Download the static binary from the [releases](https://github.com/Mumcord/mumpeer/releases)
and run it as a systemd service: see [[Operating]].

Next: [[Configuration]] · [[Operating]] · [[Security and privacy]]
