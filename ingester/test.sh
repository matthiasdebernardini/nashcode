#!/usr/bin/env bash
#
# The ingester's test entry point. Real processes, no mocks: a MinIO container
# for the fleet bucket, a real celld node serving this Worker, and the
# assertions in test/ingester.test.mjs driving it over HTTP.
#
#   ingester/test.sh                 run everything
#   CELLD=/path/to/celld ingester/test.sh
#   KEEP=1 ingester/test.sh          leave the node and the bucket up afterwards
#
# Each run gets its own bucket, so cells never survive from one run to the next
# and a quota counter never leaks into the next test.

set -euo pipefail
cd "$(dirname "$0")"

# Progress goes to stderr so it appears as it happens and stays out of the TAP
# stream on stdout. Redirected to a file, stdout is block-buffered and a stalled
# run looks identical to one that never started.
say() { printf '\n\033[1m%s\033[0m\n' "$*" >&2; }
die() { printf '\n\033[31mingester/test.sh: %s\033[0m\n\n' "$*" >&2; exit 1; }

# ---- prerequisites -----------------------------------------------------------

CELLD=${CELLD:-$(command -v celld || true)}
[ -n "$CELLD" ] || die "no celld on PATH. Install it with 'curl -fsSL https://celld.dev/install.sh | sh',
or point CELLD at a binary. These tests were written against celld 0.2.1."

CELLD_VERSION=$("$CELLD" --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1 || true)
case "$CELLD_VERSION" in
  0.0.*|0.1.*) die "celld $CELLD_VERSION is too old: the ingester needs 0.2.0 or newer.
Set CELLD to a newer binary." ;;
  "") die "cannot read a version out of '$CELLD --version'" ;;
esac

command -v docker >/dev/null || die "no docker. The fleet bucket is a MinIO container;
celld has no filesystem or in-memory bucket mode, so there is nothing to fall back to."
docker info >/dev/null 2>&1 || die "docker is installed but not running. Start it and try again."
command -v node >/dev/null || die "no node. The assertions run under 'node --test'."
command -v esbuild >/dev/null || die "no esbuild on PATH. 'celld deploy' bundles the Worker with it:
npm i -g esbuild"

# ---- names and ports ---------------------------------------------------------

RUN_ID=$(node -e 'process.stdout.write(Date.now().toString(36) + Math.floor(Math.random() * 1e6).toString(36))')
MINIO_NAME="nashcode-ingester-test-$RUN_ID"
BUCKET="ingester-test-$RUN_ID"
WATCH=$(mktemp -d)
COLD_WATCH=$(mktemp -d)
LOG=$(mktemp -t ingester-celld)

read -r MINIO_PORT NODE_PORT COLD_PORT <<EOF
$(node -e '
const net = require("net");
const free = () => new Promise((ok) => { const s = net.createServer(); s.listen(0, "127.0.0.1", () => { const { port } = s.address(); s.close(() => ok(port)); }); });
Promise.all([free(), free(), free()]).then((ports) => console.log(ports.join(" ")));
')
EOF

# The drain token is minted here and never written down. It is the one secret in
# the system; the DSN keys the tests use are routing identifiers, not secrets.
DRAIN_TOKEN=$(node -e 'process.stdout.write(require("crypto").randomBytes(24).toString("hex"))')

export AWS_ACCESS_KEY_ID=ingestertest
export AWS_SECRET_ACCESS_KEY=ingestertest123
export AWS_REGION=us-east-1
export S3_ENDPOINT="http://127.0.0.1:$MINIO_PORT"

CELLD_PID=""
COLD_PID=""
cleanup() {
  local status=$?
  if [ "${KEEP:-}" = "1" ] && [ "$status" = "0" ]; then
    say "KEEP=1: node on http://127.0.0.1:$NODE_PORT, bucket s3://$BUCKET, log $LOG"
    say "drain token: $DRAIN_TOKEN"
    return
  fi
  [ -n "$CELLD_PID" ] && kill "$CELLD_PID" 2>/dev/null || true
  [ -n "$COLD_PID" ] && kill "$COLD_PID" 2>/dev/null || true
  docker rm -f "$MINIO_NAME" >/dev/null 2>&1 || true
  if [ "$status" != "0" ]; then
    say "celld log ($LOG), last 40 lines:"
    tail -40 "$LOG" 2>/dev/null || true
  fi
}
trap cleanup EXIT

# ---- the bucket --------------------------------------------------------------

say "[1/4] MinIO on 127.0.0.1:$MINIO_PORT, bucket $BUCKET"
docker run -d --name "$MINIO_NAME" -p "$MINIO_PORT:9000" \
  -e "MINIO_ROOT_USER=$AWS_ACCESS_KEY_ID" -e "MINIO_ROOT_PASSWORD=$AWS_SECRET_ACCESS_KEY" \
  minio/minio:latest server /data >/dev/null

for _ in $(seq 1 60); do
  curl -sf "http://127.0.0.1:$MINIO_PORT/minio/health/live" >/dev/null 2>&1 && break
  sleep 1
done
curl -sf "http://127.0.0.1:$MINIO_PORT/minio/health/live" >/dev/null 2>&1 || die "MinIO never came up"

docker exec "$MINIO_NAME" mc alias set local "http://127.0.0.1:9000" \
  "$AWS_ACCESS_KEY_ID" "$AWS_SECRET_ACCESS_KEY" >/dev/null
docker exec "$MINIO_NAME" mc mb "local/$BUCKET" >/dev/null

# ---- deploy ------------------------------------------------------------------

say "[2/4] celld deploy ($("$CELLD" --version 2>/dev/null | head -1))"
"$CELLD" deploy . --bucket "s3://$BUCKET" >>"$LOG" 2>&1 || die "celld deploy failed; see $LOG"

# ---- the node ----------------------------------------------------------------

say "[3/4] celld nodes on 127.0.0.1:$NODE_PORT and 127.0.0.1:$COLD_PORT"

# A minute of lease, and cells that evict after a second idle. Both matter only
# for the registry-outage test at the end: it stops MinIO for a few seconds, and
# with the stock ten-second lease a node self-fences and halts rather than
# staying up to be measured. Everything else is indifferent to these.
node_env() {
  CELLD_TTL_MS=60000
  CELLD_IDLE_EVICT_S=1
  CELLD_VAR_DRAIN_TOKEN="$DRAIN_TOKEN"
  CELLD_VAR_REGISTRY_TTL_MS=200
  CELLD_VAR_MAX_BUFFER_BYTES=8388608
  RUST_LOG=${RUST_LOG:-warn}
  export CELLD_TTL_MS CELLD_IDLE_EVICT_S CELLD_VAR_DRAIN_TOKEN CELLD_VAR_REGISTRY_TTL_MS \
    CELLD_VAR_MAX_BUFFER_BYTES RUST_LOG
}
node_env

CELLD_WATCH="$WATCH" "$CELLD" --bucket "s3://$BUCKET" --listen "127.0.0.1:$NODE_PORT" >>"$LOG" 2>&1 &
CELLD_PID=$!

# The second node exists to be cold. Nothing may touch it until the outage test,
# because the thing under test is a Worker isolate that has never managed to read
# the registry even once — the only state in which 503 is the right answer.
CELLD_WATCH="$COLD_WATCH" CELLD_NODE="cold-$RUN_ID" \
  "$CELLD" --bucket "s3://$BUCKET" --listen "127.0.0.1:$COLD_PORT" >>"$LOG" 2>&1 &
COLD_PID=$!

for _ in $(seq 1 60); do
  code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "http://127.0.0.1:$NODE_PORT/api/999/envelope/" -d '{}' || true)
  [ "$code" = "404" ] && break
  kill -0 "$CELLD_PID" 2>/dev/null || die "the celld node exited; see $LOG"
  sleep 1
done
[ "$code" = "404" ] || die "the celld node never answered; see $LOG"
kill -0 "$COLD_PID" 2>/dev/null || die "the cold celld node exited; see $LOG"

# ---- the assertions ----------------------------------------------------------

say "[4/4] node --test"
INGESTER_URL="http://127.0.0.1:$NODE_PORT" \
INGESTER_COLD_URL="http://127.0.0.1:$COLD_PORT" \
INGESTER_MINIO="$MINIO_NAME" \
INGESTER_MINIO_PORT="$MINIO_PORT" \
INGESTER_DRAIN_TOKEN="$DRAIN_TOKEN" \
INGESTER_FIXTURES="$(cd ../viewer/tests/fixtures/bugs && pwd)" \
  node --test test/*.test.mjs
