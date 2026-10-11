#!/bin/bash
# Runs the 1.x side of the matched scheduler comparison (otter's
# docs/stackless-experiment.md): steady state, and resume cost vs depth.
# Usage: bench/shards-1x/run.sh path/to/1.x/shards [runs]
set -eu
SHARDS="${1:?usage: run.sh path/to/1.x/shards [runs]}"
RUNS="${2:-3}"
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/shards-1x-bench.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

bench_line() { grep -o "BENCH .*" | head -1; }

echo "== steady state (instances, ticks=1000)"
for n in 100 1000 10000; do
  for r in $(seq 1 "$RUNS"); do
    line=$("$SHARDS" "$DIR/steady.shs" instances:$n ticks:1000 2>&1 | bench_line)
    ms=$(echo "$line" | sed -E 's/.*total_ms=([0-9.e+-]+).*/\1/')
    awk -v n="$n" -v ms="$ms" -v line="$line" 'BEGIN { printf "%s tick_us=%.2f ns_per_instance_tick=%.1f\n", line, ms * 1000 / 1000, ms * 1e6 / 1000 / n }'
  done
done

# Depth script: matched with otter's bench_depth. `depth` nested Do wires,
# then a bottom wire that increments a counter and pauses forever, so every
# measured tick is a pure resume (expected resumes: instances * ticks).
gen_depth() {
  local depth=$1 f="$TMP/depth-$1.shs"
  {
    echo '@define(instances "1000" IgnoreRedefined: true)'
    echo '@define(ticks "1000" IgnoreRedefined: true)'
    echo '@mesh(root)'
    echo "@wire(level-$depth { Repeat({Inc(resumes) Pause(0.0)} Forever: true) })"
    for ((i = depth - 1; i >= 0; i--)); do
      echo "@wire(level-$i { Do(level-$((i + 1))) })"
    done
    echo '@wire(root-entity { Do(level-0) } Looped: true)'
    cat <<'SHS'
@wire(spawner {
  @instances | ParseInt | Set(n)
  @ticks | ParseInt | Set(k)
  0 | Set(resumes Global: true)
  Repeat({Spawn(root-entity Joint: true)} Times: n)
  Repeat({Pause(0.0)} Times: 10)
  resumes | Set(r0)
  Time.NowMs | Set(t0)
  Repeat({Pause(0.0)} Times: k)
  Time.NowMs | Sub(t0) | Set(total-ms)
  resumes | Sub(r0) | Set(done)
  ["BENCH instances=" n " ticks=" k " total_ms=" total-ms " resumes=" done] | String.Format | Log
})
@schedule(root spawner)
@run(root) | Assert.Is(true)
SHS
  } > "$f"
  echo "$f"
}

echo "== resume cost vs depth (instances=1000, ticks=1000)"
# Match bench_depth's current depths (within 2.0's compose nesting limit).
for depth in 1 4 16 32; do
  f=$(gen_depth $depth)
  for r in $(seq 1 "$RUNS"); do
    line=$("$SHARDS" "$f" 2>&1 | bench_line)
    ms=$(echo "$line" | sed -E 's/.*total_ms=([0-9.e+-]+).*/\1/')
    awk -v d="$depth" -v ms="$ms" -v line="$line" 'BEGIN { printf "depth=%s %s tick_us=%.2f ns_per_instance_tick=%.1f\n", d, line, ms, ms * 1e6 / 1000 / 1000 }'
  done
done
