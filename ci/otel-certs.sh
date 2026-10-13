#!/usr/bin/env bash
# Writes the certificates for OTLP over TLS (design §13) into DIR:
#
#   ca.pem, ca.key           a CA no system store trusts
#   server.pem, server.key   the collector, for localhost and 127.0.0.1
#   client.pem, client.key   a client certificate the collector's mTLS
#                            receivers accept
#   other-ca.pem             another CA, which trusts none of these
#
# Used by the TLS tests in crates/agentfs/tests/cli_telemetry.rs and by the
# collector in CI (ci/otelcol.yaml). Keys are left readable (0644): the
# collector container runs as another user. Test material only.
set -euo pipefail

dir=${1:?usage: otel-certs.sh DIR}
mkdir -p "$dir"
cd "$dir"

ca() { # name
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 3650 \
    -subj "/CN=agentfs test $1" -keyout "$1.key" -out "$1.pem" \
    -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign,cRLSign \
    2>/dev/null
}

leaf() { # name subject extensions
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
    -subj "/CN=$2" -keyout "$1.key" -out "$1.csr" 2>/dev/null
  printf '%s\n' "$3" >"$1.ext"
  openssl x509 -req -in "$1.csr" -CA ca.pem -CAkey ca.key -CAcreateserial -days 3650 \
    -extfile "$1.ext" -out "$1.pem" 2>/dev/null
  rm -f "$1.csr" "$1.ext"
}

ca ca
ca other-ca
leaf server localhost "basicConstraints=CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost,IP:127.0.0.1"
leaf client agentfs-client "basicConstraints=CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=clientAuth"
rm -f ca.srl other-ca.key
chmod 0644 ./*.key ./*.pem
