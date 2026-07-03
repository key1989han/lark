#!/usr/bin/env bash
#
# THROWAWAY SPIKE (standalone-bake spike 2026-07-03, #620) — Harness B driver.
# Emits a stock + baked JSON parser crate via the `gen_baked_parser` example, then
# compiles both and reports the one-shot / compile-time / binary-size deltas plus a
# per-parse tree digest (the correctness gate). See
# docs/notes/spike-standalone-bake-2026-07-03.md.
#
#   examples/standalone_bake_measure.sh <work_dir>
#
# <work_dir> is created/overwritten; needs `cargo`, `python3`, `bc`, `stat`.
set -euo pipefail

WORK="${1:?usage: standalone_bake_measure.sh <work_dir>}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"   # lark-rs/

rm -rf "$WORK"; mkdir -p "$WORK"

# 1. Emit the two crates.
( cd "$HERE" && cargo run --release --features baked-dfa-spike \
    --example gen_baked_parser -- "$WORK" >/dev/null )

# 2. A ~610 KB JSON workload matching the spike's json_594k shape.
python3 - "$WORK/input.json" <<'PY'
import json, sys
recs = [{f"key{f}": r * 10 + f for f in range(8)} | {f"name{f}": f"value{r}_{f}" for f in range(8)}
        for r in range(2000)]
open(sys.argv[1], "w").write(json.dumps(recs, separators=(", ", ": ")))
PY
printf '[1, 2, {"a": "b"}, true, null]' > "$WORK/tiny.json"

measure() {
  name="$1"; bin="$2"; dir="$WORK/$name"
  ( cd "$dir" && cargo build --release >/dev/null 2>&1 )          # warm deps (untimed)
  ( cd "$dir" && cargo clean >/dev/null 2>&1 )
  t0=$(date +%s.%N); ( cd "$dir" && cargo build --release >/dev/null 2>&1 ); t1=$(date +%s.%N)
  clean=$(echo "$t1 - $t0" | bc)
  touch "$dir/src/parser.rs"
  t0=$(date +%s.%N); ( cd "$dir" && cargo build --release >/dev/null 2>&1 ); t1=$(date +%s.%N)
  incr=$(echo "$t1 - $t0" | bc)
  size=$(stat -c %s "$dir/target/release/$bin")
  echo "=== $name ==="
  echo "clean_build_s $clean"
  echo "incr_build_s  $incr"
  echo "binary_bytes  $size"
  echo "-- 610 KB --"; "$dir/target/release/$bin" "$WORK/input.json"
  echo "-- 30 B  --"; "$dir/target/release/$bin" "$WORK/tiny.json"
}

measure stock stock_json
measure baked baked_json
echo "(gate: the two DIGEST_HASH lines per input must match across stock and baked)"
