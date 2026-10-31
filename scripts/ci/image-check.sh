#!/bin/bash
# Usage: scripts/ci/image-check.sh <image>
# Starts the image with a mounted config and secret file (host network, unprivileged ports), then
# checks that it runs as non-root, prints its fingerprint and answers /v1/status with the secret.
set -euo pipefail
image="$1"
work="$(mktemp -d)"
name="mumpeer-check-$$"
trap 'docker rm -f "$name" >/dev/null 2>&1 || true; rm -rf "$work"' EXIT

openssl rand -hex 32 > "$work/secret"
cat > "$work/mumpeer.toml" <<TOML
public_host = "127.0.0.1"
udp_listen = "127.0.0.1:44443"
tcp_listen = "127.0.0.1:44443"
api_listen = "127.0.0.1:47480"
api_secret_file = "/run/secrets/mumpeer_api_secret"
TOML
chmod 644 "$work/mumpeer.toml" "$work/secret"

docker run -d --name "$name" --network host \
  -v "$work/mumpeer.toml:/config/mumpeer.toml:ro" \
  -v "$work/secret:/run/secrets/mumpeer_api_secret:ro" \
  "$image" >/dev/null

for _ in $(seq 30); do
  if body=$(curl -fsS -H "Authorization: Bearer $(cat "$work/secret")" http://127.0.0.1:47480/v1/status); then
    break
  fi
  sleep 1
done
echo "status: ${body:-none}"
fp=$(printf '%s' "${body:-}" | sed -n 's/.*"fp":"\([0-9a-f]\{64\}\)".*/\1/p')
[ -n "$fp" ] || { docker logs "$name"; echo "no /v1/status answer" >&2; exit 1; }
docker logs "$name" 2>&1 | grep -q "relay fingerprint (SHA-256): $fp" \
  || { docker logs "$name"; echo "fingerprint not printed" >&2; exit 1; }
[ "$(docker exec "$name" id -u)" != 0 ] || { echo "runs as root" >&2; exit 1; }
code=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:47480/v1/status)
[ "$code" = 401 ] || { echo "status without secret answered $code" >&2; exit 1; }
echo "image ok: non-root, fingerprint printed, status answers only with the secret"
