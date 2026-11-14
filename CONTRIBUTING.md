# Contributing to mumpeer

mumpeer is the relay for Mumcord's peer-to-peer features. Please read this before opening a pull request. Security problems go to the private channel in [SECURITY.md](SECURITY.md), never into a public issue. Open an issue before larger changes: the behaviour follows the relay specification in Mumcord (`specs/04-mumpeer-relay-specification.md`).

## Rules

- Write everything in English: code, comments, docs, commit messages and log and API text.
- Keep it simple. No speculative abstractions, no dead code, no commented-out blocks. Comments explain the why, not the what. Follow the style of the surrounding code.
- Never swallow errors silently: log them or report them cleanly to the caller. No `unwrap` outside tests, no unchecked casts and no `unsafe` without a reason.
- The relay only forwards. It forwards packets between the two bound sides of one pair, never dials out and never touches Mumble servers.
- A relay is long-running: never block the async runtime, bound every wait with a timeout, cap every buffer and count. No logging per packet.
- Commits are small and focused, one logical change each, in [Conventional Commits](https://www.conventionalcommits.org/) style (`feat:`, `fix:`, `docs:`, `chore:` ...).

## Security and privacy

- TLS 1.3 only, client certificates mandatory, the relay certificate pinned by fingerprint. Never log or commit the relay key.
- The API secret is never logged and compared in constant time. The API binds to loopback by default and uses TLS on any other address.
- Logs never contain full IP addresses, full certificate hashes, tokens, the secret or packet contents.
- Treat everything from clients and API callers as untrusted input: bound sizes before parsing, validate formats.
- Never commit secrets (API secret, relay key, certificates). Use git-ignored local files (`mumpeer.toml`, `testenv/.env`).

## Tests

- New logic gets tests. Relay flows get an end-to-end test on loopback (`tests/relay.rs`). A bug is first reproduced with a test, then fixed.
- Never test against public or third-party Mumble servers or relays. Use the local test environment (`testenv/up.sh`).
- Before opening a pull request, run these:
  - `cargo fmt --all --check`
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo test`
- Docker changes also need a `docker build` and a `/v1/status` check.

## Changelog and docs

User-visible changes get a line in `CHANGELOG.md` under the unreleased version, and the docs (README, `docs/`) are updated.
