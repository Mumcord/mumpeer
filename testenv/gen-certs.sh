#!/bin/bash
# Generates the self-signed Mumble server certificate (once) into testenv/certs/.
set -euo pipefail
cd "$(dirname "$0")"
[ -f certs/cert.pem ] && exit 0
mkdir -p certs
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=Mumpeer Test Server" \
  -keyout certs/key.pem -out certs/cert.pem 2>/dev/null
echo "generated the test server certificate"
