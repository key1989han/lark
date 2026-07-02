#!/usr/bin/env bash
# Committed grammar-fuzz seed-range regression gate (epic #208).
#
# This pins THE committed seed range for `--fuzz-grammars`: the range below was
# swept clean (0 divergences) against the Python-Lark oracle on 2026-07-02
# (seeds 1..300 x 40 grammars x 15 inputs — ~12k generated grammars, ~4k built
# by the oracle, ~60k inputs diffed). Unlike the fresh-entropy nightly
# discovery tier, a divergence here is a REGRESSION (a once-clean seed went
# red) and must gate — per the "never regress a green corpus" invariant.
#
# Runs out-of-band (nightly workflow + explicit local runs), never on the PR
# critical path. Widening the range is fine once the new seeds are verified
# clean; narrowing it needs the same scrutiny as shrinking an XFAIL ledger.
#
# Usage: scripts/fuzz-seed-range.sh [extra fuzz_differential.py args...]
#   LARK_FUZZ_SEED_RANGE=1:500 scripts/fuzz-seed-range.sh   # override range
set -euo pipefail
cd "$(dirname "$0")/.."

RANGE="${LARK_FUZZ_SEED_RANGE:-1:300}"
COUNT="${LARK_FUZZ_GG_COUNT:-40}"
INPUTS="${LARK_FUZZ_GG_INPUTS:-15}"

echo "grammar-fuzz seed-range regression gate: seeds $RANGE, -n $COUNT, --gg-inputs $INPUTS"
exec python3 tools/fuzz_differential.py --fuzz-grammars \
  --gg-seed-range "$RANGE" -n "$COUNT" --gg-inputs "$INPUTS" "$@"
