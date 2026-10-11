#!/bin/bash
# HTTP concurrency benchmark: N concurrent Http.Get requests against one
# fixed-latency local server, on 1.x and on 2.0.
#
# Per run it records: instance creation time and request execution time
# (separately), ticks during execution, the client's peak OS thread count
# (sampled from outside; Linux only), and the server's peak concurrent
# requests and completed count.
#
# Usage: bench/http-concurrency/run.sh path/to/1.x/shards [runs]
#   env COUNTS="1 32 100 1000" (instances), LATENCY_MS=50, PORT=18090
set -eu

SHARDS_1X="${1:?usage: run.sh path/to/1.x/shards [runs]}"
RUNS="${2:-3}"
COUNTS="${COUNTS:-1 32 100 1000}"
LATENCY_MS="${LATENCY_MS:-50}"
PORT="${PORT:-18090}"
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$DIR/../.." && pwd)"
URL="http://127.0.0.1:$PORT/work"
STATS="http://127.0.0.1:$PORT"

# 1000 concurrent connections need more than the common 1024 descriptor limit.
ulimit -n 16384 2> /dev/null || ulimit -n "$(ulimit -Hn)"

(cd "$ROOT" && cargo build --release -q -p otter-io --examples)
SERVER="$ROOT/target/release/examples/latency_server"
CLIENT_20="$ROOT/target/release/examples/bench_http"

"$SERVER" "$PORT" "$LATENCY_MS" 2> /dev/null &
SERVER_PID=$!
TMP="$(mktemp -d "${TMPDIR:-/tmp}/http-bench.XXXXXX")"
trap 'kill $SERVER_PID 2> /dev/null; rm -rf "$TMP"' EXIT
until curl -fs "$STATS/stats" > /dev/null; do sleep 0.05; done

# Runs a client, sampling its thread count until it exits. Prints
# "<BENCH line> threads=<peak>".
run_client() {
  local log="$TMP/client.log"
  "$@" > "$log" 2>&1 &
  local pid=$! peak=0 t
  while kill -0 "$pid" 2> /dev/null; do
    if [ -r "/proc/$pid/status" ]; then
      t=$(awk '/^Threads:/ { print $2 }' "/proc/$pid/status" 2> /dev/null || echo 0)
      [ "${t:-0}" -gt "$peak" ] && peak=$t
    fi
    sleep 0.002
  done
  wait "$pid" || { echo "client failed:" >&2; tail -20 "$log" >&2; return 1; }
  [ -r /proc/self/status ] || peak="n/a"
  echo "$(grep -o 'BENCH .*' "$log") threads=$peak"
}

echo "server latency ${LATENCY_MS} ms; runs per point: $RUNS"
for runtime in 1.x 2.0; do
  for n in $COUNTS; do
    for r in $(seq 1 "$RUNS"); do
      curl -fs "$STATS/reset" > /dev/null
      case $runtime in
        1.x) line=$(run_client "$SHARDS_1X" "$DIR/client-1x.shs" url:"$URL" instances:"$n") ;;
        2.0) line=$(run_client "$CLIENT_20" "$URL" "$n") ;;
      esac
      echo "${line#BENCH } server_$(curl -fs "$STATS/stats")" | sed 's/ completed=/ server_completed=/'
    done
  done
done
