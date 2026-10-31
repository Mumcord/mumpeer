# Security and privacy

## What protects the content

- **End-to-end encryption.** The relay forwards an inner QUIC connection between the two
  Mumcord clients. It is encrypted end to end and pinned to both users' Mumble certificates.
  The relay has no key for it. If the relay forwarded to anyone else, the inner handshake
  would fail.
- **The relay copies, it does not look.** Inner packets are passed on byte for byte and never
  parsed.

## Who may use the relay

- **Mutual TLS.** Clients connect with TLS 1.3 and must present their Mumble client
  certificate; the relay identifies them by its hash (the same hash the Mumble server uses).
- **Pinned relay.** Clients check the relay certificate against the SHA-256 fingerprint the
  bot announced. There is no certificate authority to trick. A changed fingerprint is not
  used until the user chooses "Trust new relay".
- **Only registered bots.** Mumcord accepts relay announcements only from registered users of
  the Mumble server, and pins the bot the first time.
- **Pair tokens.** The bot asks for a pair of two certificate hashes and gets a 256-bit random
  token. Only those two certificates can use it, each side once, within 120 seconds by default.
  The relay keeps only a SHA-256 hash of the token.
- **No open proxy.** The relay forwards only between the two bound sides of one pair and never
  opens a connection itself.

## The bot API

- Listens on loopback by default; any other address switches to HTTPS with the pinned relay
  certificate.
- Every request needs the shared secret (at least 32 characters), compared in constant time.
  The secret comes from the environment or a file, never from the config, and is never logged.
- Wrong secrets count as failed logins: ten in a minute block the address for 10 minutes.

## Abuse limits

Bandwidth per session and in total, sessions overall and per certificate, new connections and
failed handshakes per source address (IPv6 per /64), and pairs per minute. All configurable,
see [[Configuration]].

## What the operator can see

A relay has to know where to send packets, so whoever runs it can see:

- the **IP addresses** of both users of a session,
- **which two certificate hashes** talk to each other,
- **how much** data flows and **when**.

They **cannot** see screen shares, images or any other content. Mumcord tells users this
before they use a relay. Voice and text never go through the relay.

## What is stored and logged

- **Nothing about users is written to disk.** Pairs, sessions and counters are in memory only.
  The only files are the relay's own key (mode 0600) and certificate.
- **Logs** contain counts, durations, byte totals, end reasons and random session ids.
  Warnings show only the /24 (IPv4) or /48 (IPv6) network. `debug` adds the first 8 hex
  characters of certificate hashes.
- **Never logged:** full IP addresses, full certificate hashes, tokens, the API secret, packet
  contents.

## Operator checklist

- Keep the API on loopback; never publish port 7480.
- Keep the secret out of Git and config files (`.env` with mode 600, Docker secrets or systemd
  credentials).
- Protect and back up `relay-key.pem`; rotate it (delete both files, restart) if it leaks, and
  tell your users to expect "relay changed".
- Run the relay as a non-root user (the Docker image and the systemd unit do).
