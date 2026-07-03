#!/usr/bin/env bash
# Committed grammar-fuzz seed-range regression gate (epic #208, ADR-0041).
#
# This pins THE committed seed range for `--fuzz-grammars`: the range below was
# swept clean (0 divergences) against the Python-Lark oracle on 2026-07-02
# (seeds 1..300 x 40 grammars x 15 inputs — 12000 generated grammars, 3832
# built by the oracle, 57480 inputs diffed). Unlike the fresh-entropy nightly
# discovery tier, a divergence here is a REGRESSION (a once-clean seed went
# red) and must gate — per the "never regress a green corpus" invariant.
#
# Runs out-of-band (nightly workflow + explicit local runs), never on the PR
# critical path. Widening the range is fine once the new seeds are verified
# clean; narrowing it needs the same scrutiny as shrinking an XFAIL ledger.
#
# Usage: scripts/fuzz-seed-range.sh [extra fuzz_differential.py args...]
#   LARK_FUZZ_SEED_RANGE=1:500 scripts/fuzz-seed-range.sh   # override range
#   LARK_FUZZ_SEED_RANGE=13:13 scripts/fuzz-seed-range.sh   # replay one seed
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/.."

RANGE="${LARK_FUZZ_SEED_RANGE:-1:300}"
COUNT="${LARK_FUZZ_GG_COUNT:-40}"
INPUTS="${LARK_FUZZ_GG_INPUTS:-15}"

# A dedicated scratch dir: the sweep runs for minutes and its grammar files are
# re-read during end-of-sweep minimization, so it must not share the default
# scratch dir with a concurrent ad-hoc --fuzz-grammars run (whose startup clear
# would delete this sweep's files mid-flight and fake a regression).
SCRATCH="${LARK_FUZZ_GG_SCRATCH:-target/fuzz_seed_range}"

echo "grammar-fuzz seed-range regression gate: seeds $RANGE, -n $COUNT, --gg-inputs $INPUTS"
exec python3 tools/fuzz_differential.py --fuzz-grammars \
  --gg-seed-range "$RANGE" -n "$COUNT" --gg-inputs "$INPUTS" \
  --gg-scratch-dir "$SCRATCH" "$@"
