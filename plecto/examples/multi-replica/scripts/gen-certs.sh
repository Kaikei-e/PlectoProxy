#!/usr/bin/env bash
# Generates DEMO-ONLY credentials for the TLS scenarios: a self-signed server cert,
# a client CA + client cert (scenario B), and a 64-byte shared STEK (scenario A).
# Never use any of these in production.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p manifests/secrets
umask 077

# Server cert — SAN covers the LB entry (localhost) and direct replica access.
openssl req -x509 -newkey rsa:2048 -sha256 -nodes -days 30 \
  -keyout manifests/secrets/server.key -out manifests/secrets/server.crt \
  -subj "/CN=plecto-demo" \
  -addext "subjectAltName=DNS:localhost,DNS:plecto-1,DNS:plecto-2,IP:127.0.0.1"

# Client CA + client cert (scenario B).
openssl req -x509 -newkey rsa:2048 -sha256 -nodes -days 30 \
  -keyout manifests/secrets/client-ca.key -out manifests/secrets/client-ca.crt \
  -subj "/CN=plecto-demo-client-ca"
openssl req -newkey rsa:2048 -sha256 -nodes \
  -keyout manifests/secrets/client.key -out manifests/secrets/client.csr \
  -subj "/CN=plecto-demo-client"
openssl x509 -req -in manifests/secrets/client.csr -CA manifests/secrets/client-ca.crt \
  -CAkey manifests/secrets/client-ca.key -CAcreateserial -days 30 -out manifests/secrets/client.crt
rm -f manifests/secrets/client.csr manifests/secrets/client-ca.srl

# Shared STEK (scenario A): exactly 64 raw random bytes, owner-only — the permission
# check fails closed on anything group/other-readable (ADR 000062).
openssl rand 64 > manifests/secrets/stek.key

chmod 600 manifests/secrets/*
echo "demo-only credentials written to ./manifests/secrets"
