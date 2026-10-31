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
- Minimal logging: no full IP addresses, certificate hashes, tokens or secrets.
- Docker image and a local test environment (`testenv/`) with network namespace profiles.
