# Changelog

All notable changes to mumpeer are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Relay for Mumcord peer-to-peer features: QUIC over UDP and TLS over TCP (for networks that
  block UDP), mutual TLS with the Mumble client certificate, pair-bound tokens, and unchanged
  forwarding of the end-to-end encrypted inner packets between the two sides of a pair only.
- Allowlist API for the relay bot (`/v1/status`, `/v1/pairs`, `/v1/peers/{hash}`), authenticated
  with a secret from the environment or a file.
- Self-signed relay identity generated on first start; its SHA-256 fingerprint is printed and
  announced.
- Limits: bandwidth per session and in total, maximum sessions (overall and per certificate),
  new connections and failed handshakes per source address, pairs per minute.
- Release workflow: a `v*` tag builds static Linux binaries (x86_64, aarch64) and the multi-arch
  Docker image `ghcr.io/mumcord/mumpeer`, and creates a draft GitHub release with `SHA256SUMS`.
- `[uses]` config section to allow or disallow images, files and screen sharing on the relay.
  `GET /v1/status` lists the allowed `uses`, and `POST /v1/pairs` takes an optional `use` and
  answers `403 use_not_allowed` for a disabled one.
- Screen-share broadcast forwarding: the sharer uploads once and the relay copies the
  end-to-end encrypted video and audio to every admitted viewer, over QUIC or TLS/TCP. Viewers
  can ask the sharer for a keyframe, and a slow viewer is reduced to a lower quality layer
  instead of disturbing the others. The broadcast ends when the sharer's link ends.
- Screen-share broadcasts API for the relay bot: `POST /v1/broadcasts`, viewer admission and
  removal, and `DELETE /v1/broadcasts/{id}`. New limits `max_broadcasts`,
  `max_viewers_per_broadcast`, `broadcast_in_mbit` and `broadcast_out_mbit`; `GET /v1/status`
  announces `broadcast.max_viewers` when broadcasts are available.
- Test environment profiles (`MUMPEER_PROFILE`: `all`, `no-files`, `no-screen`, `no-images`,
  `no-broadcast`, `old`) and `MUMPEER_IMAGE` to run a previous mumpeer release.
- Minimal logging: no full IP addresses, certificate hashes, tokens or secrets.
- Docker image and a local test environment (`testenv/`) with network namespace profiles.
- Logo, README and documentation: operator guide, configuration reference, bot API reference,
  wire and handshake overview, troubleshooting, FAQ, and the pages of the GitHub wiki.
