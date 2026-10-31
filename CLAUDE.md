# mumpeer – relay for Mumcord peer-to-peer features

mumpeer is a small relay server that a server community runs next to its Mumble server. It forwards the already end-to-end encrypted QUIC packets of Mumcord's peer-to-peer features (screen sharing, full-size images) between two Mumcord clients, like a TURN server, when a direct connection fails or a user hides their IP address. The relay bot (`mumbot peer`, repo `Mumcord/mumbot`) announces the relay to Mumcord clients and creates pair tokens through mumpeer's allowlist API. Sister projects: the Mumcord client (`Mumcord/mumcord`) and Mumbot (`Mumcord/mumbot`).
Source of truth: `specs/04-mumpeer-relay-specification.md` in `Mumcord/mumcord` (relay handshake §6.5, allowlist API §6.6, limits FR-4.35, logging §9).
The user is a product manager and not a technical person → make technology decisions with a clear recommendation, explained simply.
Everything is in English: chat, specs, docs, code, comments, commit messages and log/API text.

## Answer style: caveman mode (always)

- Chat answers extremely short: no filler, no intro, no repetition. Sentence fragments are fine.
- Only the result and next steps. Bullet points instead of prose. For code changes, `file:line` is enough instead of code excerpts.
- Must still be understandable for a non-technical person.
- **Applies to chat only.** Specs, docs, commit messages and code comments are written fully and cleanly.

## Context budget: only 200k tokens → be frugal

- Search before reading (Grep/Glob). Read large files only in parts (`offset`/`limit`).
- Never read anything twice. Don't re-read your own changes to check them.
- Delegate broad searches across many files to a subagent (Explore) and take only its result.
- Keep agents short: concise brief, smallest suitable model, final report ≤ 10 lines. No long-lived agents, no status pings.
- Limit command output: `--quiet`, `| tail -n 50`, for tests print only the failures.
- Don't copy file contents, logs or diffs into the chat.
- Run independent tool calls in parallel. No unnecessary interim reports.

## Way of working

- Specs are the truth: only build what the specification says. Unclear or contradictory → ask instead of guessing. No scope creep.
- Delete your own build cache in ~/.cache/mumcord-build/<agent> when done (Rust targets are several GB each).
- One task = small, verifiable steps. Every change must be verifiable against the local test environment (`testenv/`, acceptance criteria of the spec).
- One fresh agent per subtask with a self-contained brief. Never bundle several tasks into one agent. The agent ends after its final report; follow-up work gets a new agent.
- Commit only on request. Never `git add -A`. Never push without being asked.
- Commits are small and focused: one logical change per commit, Conventional Commits style (`feat:`, `fix:`, `docs:`, `chore:` …), clear English message. Never add AI attribution: no `Co-Authored-By` trailers, no "Generated with" lines, no mention of Claude or AI.
- User-visible changes get a line in `CHANGELOG.md` under the unreleased version.
- Secrets never go into the repo (no API secrets, relay keys or certificates in files, docs or examples). Use only local git-ignored files (`mumpeer.toml`, `testenv/.env`) and CI variables.
- Never connect to public or third-party Mumble servers or relays for testing; use the local test environment (`testenv/up.sh`).
- **The relay only forwards.** It forwards packets between the two bound sides of one pair and never dials out, so it can never become an open proxy. It never touches Mumble servers: no server plugins, no server config changes, no connection to the Mumble server at all (that is the bot's job).
- Releases: a `v*` tag builds binaries and the Docker image; the user publishes by hand. Never create tags or releases without being asked.

## Stack

- **Language:** Rust (edition 2024, MSRV 1.90), one crate `mumpeer` (library + `mumpeer` binary). Linux only (x86_64 and aarch64, static musl).
- **Async runtime:** tokio. **QUIC:** quinn with datagrams. **TLS:** rustls with ring (TLS 1.3 only on the relay transports), tokio-rustls for TCP. **Certificates:** rcgen. **Hashes and random numbers:** ring.
- **HTTP:** the allowlist API is a small HTTP/1.1 handler on `tokio::net` (no web framework), JSON via serde/serde_json.
- **Config:** TOML (`toml` + serde). CLI: clap. Logs: tracing (`RUST_LOG`).
- **Packaging:** Docker image `ghcr.io/mumcord/mumpeer` (Alpine runtime, non-root), cross-built with cargo-zigbuild.
- **CI:** GitHub Actions (`ci.yml`: fmt, clippy, tests, Docker build with a `/v1/status` check).

Don't introduce anything without a reason.

## Coding rules

**General**
- Simple over clever. No speculative abstractions. No dead code, no commented-out blocks.
- Comments explain the *why*, not the *what*.
- Follow the style of the surrounding code.
- Never swallow errors silently: log them or report them cleanly to the caller.
- Strict typing; no escape hatches (`unwrap` outside tests, unchecked casts, `unsafe`) without a reason.

**Networking & runtime**
- Modules: `config`, `tls` (identity, client hash), `relay` (pairs, sessions, forwarding), `udp` (QUIC listener), `tcp` (TLS listener and framing), `api`, `limits`. Listeners hand authenticated links to `relay`; the API talks to `relay` only through its pair functions.
- The relay copies inner packets unchanged and never parses them. Keep the forwarding path free of per-packet allocation where possible, locks short, and no logging per packet.
- A relay is long-running: never block the async runtime, bound every wait with a timeout, cap every buffer and count.
- All state (pairs, sessions, counters) stays in memory; nothing about users is written to disk.

**Tests**
- New logic gets tests. Relay flows get an end-to-end test on loopback (`tests/relay.rs`) along the acceptance criteria.
- A bug is first reproduced with a test, then fixed.
- Before "done": `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` pass; Docker changes also `docker build` plus a `/v1/status` check. If not, say so honestly.

**Security & privacy**
- TLS 1.3 only, client certificates mandatory, the relay certificate pinned by fingerprint. Protect the relay key (private file permissions, never logged, never committed).
- The API secret is never logged (`Debug` hides it) and compared in constant time; the API binds to loopback by default and uses TLS on any other address.
- Logs: never full IP addresses (only /24 or /48 at `warn`), full certificate hashes (8 hex characters at `debug` only), tokens, the secret or packet contents.
- Treat everything from clients and API callers as untrusted input: bound sizes before parsing, validate formats.
