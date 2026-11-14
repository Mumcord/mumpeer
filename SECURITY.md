# Security policy

mumpeer is part of the Mumcord project. The [Mumcord security policy](https://github.com/Mumcord/mumcord/blob/main/SECURITY.md) applies to it in full: what to expect after a report, disclosure, and the rules for testing.

## Supported versions

Security fixes are made for the **latest release** and ship as a new patch release. Older releases do not get fixes; please update to the latest release.

## Reporting a vulnerability

**Do not report vulnerabilities in public issues, pull requests or discussions.**

Report privately through GitHub:

**<https://github.com/Mumcord/mumpeer/security/advisories/new>**

Only you and the maintainers can read the report. If you cannot use this form, open a normal issue that only says you would like to report a security problem and asks for a private channel. Do not include any details in it.

A good report contains the mumpeer version, how it is run (binary or Docker image), what an attacker can achieve and what they need for it (for example: a client on the network, a holder of a pair token, or a caller of the allowlist API), and steps to reproduce.

## What counts as a vulnerability

- A client, a network peer or an API caller that can crash the relay, make it hang or use unbounded memory or bandwidth through crafted packets, handshakes or requests, or that gets around the configured limits.
- Using the relay without a valid pair token and the matching client certificate, using a pair token for a pair it does not belong to, or reusing a token after it was used or expired.
- Any way to make the relay open connections to addresses of the attacker's choice (it must never become an open proxy).
- Access to the allowlist API without the API secret, or a way around its loopback-only default.
- A way to read or alter the relayed traffic, or to weaken the TLS 1.3 and client certificate checks; the relay key being exposed.
- The API secret, pair tokens, full IP addresses or full certificate hashes ending up in logs or error messages.

The end-to-end encryption between two Mumcord clients is part of the Mumcord client, so problems in it belong in the [Mumcord security policy](https://github.com/Mumcord/mumcord/blob/main/SECURITY.md).
