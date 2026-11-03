#!/usr/bin/env bash
# Local mail servers for the integration tests (crates/mail-core/tests/server.rs):
#   Dovecot  IMAP, implicit TLS on localhost:31993 (any user name, password "secret")
#   Mailpit  SMTP with STARTTLS on localhost:31025, its web view and API on http://localhost:31080
# TLS uses a throwaway CA made here (target/test-server/); the tests trust it through
# SSL_CERT_FILE, so the app's normal TLS code is what's tested.
#
#   test-server/test-server.sh up      start both (Docker or Podman), print the test environment
#   test-server/test-server.sh env     print the environment again
#   test-server/test-server.sh down    stop and remove them
#
# Then:  eval "$(test-server/test-server.sh env)" && cargo test -p mail-core --test server
set -euo pipefail

DOVECOT=docker.io/dovecot/dovecot:2.4.5
MAILPIT=docker.io/axllent/mailpit:v1.31.6
ROOT=$(cd "$(dirname "$0")/.." && pwd)
DIR=$ROOT/target/test-server
ENGINE=${CONTAINER_ENGINE:-$(command -v docker || command -v podman || true)}
[ -n "$ENGINE" ] || { echo "needs docker or podman" >&2; exit 1; }

env_vars() {
  echo "export MAIL_TEST_IMAP=localhost:31993"
  echo "export MAIL_TEST_SMTP=localhost:31025"
  echo "export MAIL_TEST_MAILPIT=http://localhost:31080"
  echo "export SSL_CERT_FILE=$DIR/ca.crt"
}

certs() {
  [ -f "$DIR/tls.crt" ] && return
  mkdir -p "$DIR"
  openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=mail test CA" \
    -keyout "$DIR/ca.key" -out "$DIR/ca.crt" 2>/dev/null
  openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" -keyout "$DIR/tls.key" -out "$DIR/tls.csr" 2>/dev/null
  printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n' > "$DIR/ext.cnf"
  openssl x509 -req -in "$DIR/tls.csr" -CA "$DIR/ca.crt" -CAkey "$DIR/ca.key" -CAcreateserial -days 3650 \
    -extfile "$DIR/ext.cnf" -out "$DIR/tls.crt" 2>/dev/null
  chmod 644 "$DIR/tls.key"   # read by the containers' own users
}

wait_port() {
  for _ in $(seq 1 60); do
    (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null && return
    sleep 0.5
  done
  echo "nothing listening on port $1" >&2
  exit 1
}

case "${1:-}" in
  up)
    certs
    "$ENGINE" rm -f mail-test-dovecot mail-test-mailpit >/dev/null 2>&1 || true
    "$ENGINE" run -d --name mail-test-dovecot -p 127.0.0.1:31993:31993 -e USER_PASSWORD=secret \
      -v "$DIR/tls.crt:/etc/dovecot/ssl/tls.crt:ro,Z" -v "$DIR/tls.key:/etc/dovecot/ssl/tls.key:ro,Z" \
      "$DOVECOT" >/dev/null
    "$ENGINE" run -d --name mail-test-mailpit -p 127.0.0.1:31025:1025 -p 127.0.0.1:31080:8025 \
      -e MP_SMTP_TLS_CERT=/certs/tls.crt -e MP_SMTP_TLS_KEY=/certs/tls.key -e MP_SMTP_AUTH_ACCEPT_ANY=true \
      -v "$DIR:/certs:ro,Z" "$MAILPIT" >/dev/null
    wait_port 31993
    wait_port 31025
    wait_port 31080
    env_vars
    ;;
  env) env_vars ;;
  down) "$ENGINE" rm -f mail-test-dovecot mail-test-mailpit >/dev/null ;;
  *) sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 1 ;;
esac
