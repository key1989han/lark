#!/usr/bin/env bash
#
# THROWAWAY SPIKE (L5 standalone-bake spike, 2026-07-03) — Part B driver for #620.
#
# Builds three self-contained generated-parser crates for one grammar and measures the
# numbers the epic asks for on real artifacts:
#   * stock          — the current generated parser (regex-crate Scanner)
#   * baked          — flat u32[state*256] static table, regex dependency DROPPED
#   * baked_classed  — byte-class-compressed static table, regex dependency DROPPED
#
# Per crate: cold build time (incl. deps), warm crate rebuild time, release binary size
# (raw + stripped, + .text/.rodata via `size`), one-shot parse (fresh Parser + first
# parse), reused parse throughput, and a structural tree digest. Asserts the three
# digests agree AND equal the in-process basic-lexer oracle digest.
#
# Usage: standalone_bake_partb.sh <grammar.lark> <start> <records|workload-file> [workdir]
set -euo pipefail

GRAMMAR="${1:?grammar path}"
START="${2:?start symbol}"
SPEC="${3:-2000}"
WORK="${4:-/tmp/claude-0/-home-user-lark/947350a7-3254-5eee-b229-e1a40f4d7bc9/scratchpad/partb}"
LARK_RS="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

rm -rf "$WORK"; mkdir -p "$WORK/gen"

# 1) workload: a numeric SPEC generates synthetic JSON; a path copies that file.
if [ -f "$SPEC" ]; then
    cp "$SPEC" "$WORK/workload.json"
else
    python3 - "$SPEC" > "$WORK/workload.json" <<'PY'
import sys
records=int(sys.argv[1]); fields=8
out=["["]
for r in range(records):
    if r: out.append(",")
    out.append("{")
    for f in range(fields):
        if f: out.append(",")
        out.append('"key%d": %d, "name%d": "value%d_%d"'%(f, r*10+f, f, r, f))
    out.append("}")
out.append("]")
sys.stdout.write("".join(out))
PY
fi
WBYTES=$(wc -c < "$WORK/workload.json")
echo "workload: $WBYTES bytes"

# 2) emit stock / baked / baked_classed generated sources.
( cd "$LARK_RS" && cargo run --release --quiet --features baked-dfa-spike \
    --example standalone_bake_emit -- "$GRAMMAR" "$START" "$WORK/gen" )

# 3) shared harness main.rs (identical for all three crates; only gen.rs differs).
cat > "$WORK/main.rs" <<'RS'
include!("gen.rs");
use std::time::{Duration, Instant};
fn push(h: &mut u64, b: &[u8]) { for &x in b { *h ^= x as u64; *h = h.wrapping_mul(0x100000001b3); } }
fn walk(t: &Tree, h: &mut u64, n: &mut u64) {
    *n += 1; push(h, b"T"); push(h, t.data.as_bytes()); push(h, b"(");
    for c in &t.children { match c {
        Child::Tree(st) => walk(st, h, n),
        Child::Token(tok) => { push(h, b"t"); push(h, tok.type_.as_bytes()); push(h, b"="); push(h, tok.value.as_bytes()); push(h, b";"); }
        Child::None => push(h, b"N"),
    } }
    push(h, b")");
}
fn canon(pt: &ParseTree) -> (u64, u64) {
    let mut h = 0xcbf29ce484222325u64; let mut n = 0u64;
    match pt {
        ParseTree::Tree(t) => walk(t, &mut h, &mut n),
        ParseTree::Token(tok) => { push(&mut h, b"t"); push(&mut h, tok.type_.as_bytes()); push(&mut h, b"="); push(&mut h, tok.value.as_bytes()); }
        ParseTree::None => push(&mut h, b"N"),
    }
    (h, n)
}
fn measure<F: FnMut()>(mut f: F) -> (f64, f64) {
    let mut iters = 1usize;
    loop {
        let t = Instant::now();
        for _ in 0..iters { f(); }
        if t.elapsed() >= Duration::from_millis(1) || iters >= 1 << 22 { break; }
        iters = (iters * 2).max(1);
    }
    let mut s: Vec<f64> = Vec::new();
    let o = Instant::now();
    while s.len() < 50 && o.elapsed() < Duration::from_millis(1500) {
        let t = Instant::now();
        for _ in 0..iters { f(); }
        s.push(t.elapsed().as_nanos() as f64 / iters as f64);
    }
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (s[s.len() / 2], s[0])
}
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let text = std::fs::read_to_string(&args[0]).expect("workload");
    let t0 = Instant::now();
    let p0 = Parser::new();
    let tree0 = p0.parse(&text).expect("parse");
    let oneshot_ns = t0.elapsed().as_nanos();
    let (digest, nodes) = canon(&tree0);
    let p = Parser::new();
    let (med, min) = measure(|| { let _ = p.parse(&text).expect("parse"); });
    let mbps = text.len() as f64 / med * 1e3;
    println!("RESULT one_shot_ns={} reused_med_ns={:.0} reused_min_ns={:.0} mb_s={:.1} nodes={} digest={:016x} bytes={}",
        oneshot_ns, med, min, mbps, nodes, digest, text.len());
}
RS

DIGESTS=()
build_and_run () {
    local name="$1" gen="$2" deps="$3"
    local crate="$WORK/$name"
    mkdir -p "$crate/src"
    cp "$WORK/gen/$gen" "$crate/src/gen.rs"
    cp "$WORK/main.rs" "$crate/src/main.rs"
    cat > "$crate/Cargo.toml" <<TOML
[package]
name = "$name"
version = "0.0.0"
edition = "2021"

[dependencies]
$deps

[profile.release]
opt-level = 3
lto = true
TOML
    # cold build (incl. deps)
    ( cd "$crate" && cargo build --release --quiet 2>/dev/null || true )  # warm the registry/index
    cargo clean --manifest-path "$crate/Cargo.toml" >/dev/null 2>&1 || true
    local t_cold_start t_cold_end
    t_cold_start=$(date +%s.%N)
    ( cd "$crate" && cargo build --release --quiet )
    t_cold_end=$(date +%s.%N)
    local cold; cold=$(echo "$t_cold_end - $t_cold_start" | bc)
    # warm crate-only rebuild
    touch "$crate/src/main.rs"
    local t_w_start t_w_end
    t_w_start=$(date +%s.%N)
    ( cd "$crate" && cargo build --release --quiet )
    t_w_end=$(date +%s.%N)
    local warm; warm=$(echo "$t_w_end - $t_w_start" | bc)
    # binary size
    local bin="$crate/target/release/$name"
    local raw stripped
    raw=$(stat -c%s "$bin")
    cp "$bin" "$bin.stripped"; strip "$bin.stripped"
    stripped=$(stat -c%s "$bin.stripped")
    local sizeline; sizeline=$(size "$bin" 2>/dev/null | tail -1 || echo "n/a")
    # run
    local out; out=$("$bin" "$WORK/workload.json")
    local dg; dg=$(echo "$out" | grep -o 'digest=[0-9a-f]*' | cut -d= -f2)
    DIGESTS+=("$name:$dg")
    printf '\n== %s ==\n' "$name"
    printf '  build:   cold %6.2fs   warm-rebuild %6.2fs\n' "$cold" "$warm"
    printf '  binary:  raw %8d B   stripped %8d B\n' "$raw" "$stripped"
    printf '  size(text/data/bss dec): %s\n' "$sizeline"
    printf '  %s\n' "$out"
}

build_and_run stock         stock.rs          'regex = "1.10"'
build_and_run baked         baked.rs          ''
build_and_run baked_classed baked_classed.rs  ''

# 4) in-process oracle digest.
echo
ORACLE=$( ( cd "$LARK_RS" && cargo run --release --quiet --example standalone_oracle_digest -- \
    "$GRAMMAR" "$START" "$WORK/workload.json" ) )
echo "$ORACLE"
ODG=$(echo "$ORACLE" | grep -o 'digest=[0-9a-f]*' | cut -d= -f2)

echo
echo "== correctness gate =="
ok=1
for entry in "${DIGESTS[@]}"; do
    name="${entry%%:*}"; dg="${entry#*:}"
    if [ "$dg" = "$ODG" ]; then echo "  OK   $name digest == oracle ($dg)"; else echo "  FAIL $name digest $dg != oracle $ODG"; ok=0; fi
done
[ "$ok" = 1 ] && echo "  ✅ all variants tree-identical to the in-process oracle" || { echo "  ❌ digest mismatch"; exit 1; }
