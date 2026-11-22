# Security policy

Pharos is pre-1.0 software that listens on the network, injects input, and can
bridge USB devices into the host. It has **not** had a security audit. Run it on a
trusted LAN and keep authentication enabled (the default).

## Reporting a vulnerability

Please report security issues **privately**, not in a public issue:

- Preferred: GitHub's **private vulnerability reporting** for this repository
  (repository **Security** tab → **Report a vulnerability**).
- If that is unavailable, open a minimal public issue asking for a private contact
  channel; do **not** include exploit details in it.

Please include what you did, what you expected, and what happened. We'll acknowledge
as soon as we can. There is no bug-bounty program.

## Scope notes

- `PHAROS_ALLOW_UNPAIRED=1` intentionally disables client authentication; issues that
  require it to be set are out of scope.
- The QUIC transport uses a throwaway/self-signed TLS certificate as an opaque tunnel;
  trust is established by the SPAKE2 pairing + Ed25519 identity pinning, not the cert.
  "The TLS cert isn't CA-signed" is by design, not a vulnerability.
