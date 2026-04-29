#!/usr/bin/env bash
# Generate a self-signed TLS certificate for local server testing.
# The certificate covers localhost and 127.0.0.1.
#
# Requirements:
#   - ECDSA P-256 key (required by wtransport cert-hash pinning)
#   - Validity ≤ 14 days (required by wtransport cert-hash pinning)
#
# Usage:
#   bash deploy/gen-certs.sh
#
# Output files (created in deploy/certs/):
#   server.key  — private key
#   server.crt  — certificate (PEM)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CERTS_DIR="$SCRIPT_DIR/certs"
mkdir -p "$CERTS_DIR"

openssl ecparam -name prime256v1 -genkey -noout \
  -out "$CERTS_DIR/server.key"

openssl req -x509 -new \
  -key "$CERTS_DIR/server.key" \
  -out "$CERTS_DIR/server.crt" \
  -days   14 \
  -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1"

echo "Certificates written to $CERTS_DIR/"
echo "  server.key"
echo "  server.crt"
echo ""
echo "SHA-256 fingerprint (for client --cert-hash):"
openssl x509 -noout -fingerprint -sha256 -in "$CERTS_DIR/server.crt" \
  | sed 's/SHA256 Fingerprint=//' \
  | tr '[:upper:]' '[:lower:]' \
  | tr -d ':'
