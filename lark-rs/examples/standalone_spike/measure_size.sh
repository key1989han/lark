#!/usr/bin/env bash
#
# THROWAWAY SPIKE (standalone-bake spike 2026-07-03, issue #620): the L5c
# compile-time / footprint axis. For every generated-parser variant copy in
# gen/ (produced by `cargo run --release --features baked-dfa-spike --example
# standalone_bake_gen`), build a minimal consumer crate (deps: `regex`, plus
# `regex-automata` for the lean variant — exactly what each variant's generated
# file needs) and report:
#
#   * leaf-crate compile time — `touch src/main.rs` + rebuild with warm deps
#     (min of 3), i.e. the per-grammar rustc cost a consumer pays on every
#     rebuild of the file that includes the parser;
#   * stripped binary size, and the .text / .rodata / .data.rel.ro sections —
#     where the baked tables land.
#
# Usage: examples/standalone_spike/measure_size.sh [workdir]
# (workdir defaults to a mktemp dir; pass one to inspect the crates after.)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GEN_DIR="$SCRIPT_DIR/gen"
WORK="${1:-$(mktemp -d)}"
export CARGO_TARGET_DIR="$WORK/target"

echo "# standalone-bake spike: per-variant compile time + footprint (workdir: $WORK)"

for g in json matter_idl poetry_markers; do
  for v in stock lean baked interned; do
    src="$GEN_DIR/${g}_${v}.rs"
    [ -f "$src" ] || continue
    name="sa-${g//_/-}-${v}"
    crate="$WORK/${g}_${v}"
    mkdir -p "$crate/src"
    {
      echo '[package]'
      echo "name = \"$name\""
      echo 'version = "0.0.0"'
      echo 'edition = "2021"'
      echo
      echo '[dependencies]'
      echo 'regex = "1.10"'
      if [ "$v" = lean ]; then
        echo 'regex-automata = "0.4"'
      fi
      echo
      echo '[profile.release]'
      echo 'opt-level = 3'
    } > "$crate/Cargo.toml"
    cp "$src" "$crate/src/parser_gen.rs"
    cat > "$crate/src/main.rs" <<'EOF'
include!("parser_gen.rs");

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    let text = std::fs::read_to_string(&arg).unwrap_or(arg);
    match Parser::new().parse(&text) {
        Ok(t) => println!("{}", t.to_string().len()),
        Err(e) => println!("parse error: {e}"),
    }
}
EOF

    # Warm the dependency graph (not measured).
    (cd "$crate" && cargo build --release -q)

    # Leaf compile time: min of 3 touch+rebuild cycles (deps warm, so this is
    # rustc on the generated file + link).
    best=""
    for _ in 1 2 3; do
      touch "$crate/src/main.rs"
      t0=$(date +%s.%N)
      (cd "$crate" && cargo build --release -q)
      t1=$(date +%s.%N)
      dt=$(echo "$t1 $t0" | awk '{printf "%.2f", $1-$2}')
      if [ -z "$best" ] || awk "BEGIN{exit !($dt < $best)}"; then best="$dt"; fi
    done

    bin="$CARGO_TARGET_DIR/release/$name"
    cp "$bin" "$bin.stripped"
    strip "$bin.stripped"
    stripped=$(stat -c %s "$bin.stripped")
    text_sz=$(size -A "$bin.stripped" | awk '$1==".text"{print $2}')
    rodata_sz=$(size -A "$bin.stripped" | awk '$1==".rodata"{print $2}')
    relro_sz=$(size -A "$bin.stripped" | awk '$1==".data.rel.ro"{print $2}')
    src_sz=$(stat -c %s "$src")
    printf "%-24s leaf-compile %ss  stripped %8d B  .text %8d  .rodata %8d  .data.rel.ro %8d  (gen source %7d B)\n" \
      "$g/$v" "$best" "$stripped" "${text_sz:-0}" "${rodata_sz:-0}" "${relro_sz:-0}" "$src_sz"
  done
done
