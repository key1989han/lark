#!/usr/bin/env bash
#
# FULL local CI gate — runs exactly what GitHub Actions' `fmt` + `test` jobs
# run. This is NOT the routine pre-push step: it duplicates the PR's CI run
# minute for minute. Use it to reproduce a red CI locally (e.g. debugging an
# oracle-freshness or scaling-gate failure without a push/CI round trip).
#
# The routine pre-push gate is lark-rs/scripts/check-fast.sh (fmt +
# `cargo test --all` — the Pareto cut), which the committed pre-push hook
# (.githooks/pre-push) runs; enable that once per clone with:
#
#   git config core.hooksPath .githooks
#
# Mirrors (.github/workflows/lark-rs.yml):
#
#   * Rust format  → cargo fmt --check --all
#   * lark-rs test → cargo test --all + fancy-oracle differential
#                    + perf-counters scaling gates + python.lark LALR gate
#                    + oracle-freshness gate
#
# Exits non-zero on the first failing gate.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LARK_RS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$LARK_RS_DIR/.." && pwd)"

note() { printf '\n\033[1;34m▶ %s\033[0m\n' "$1"; }
fail() { printf '\n\033[1;31m❌ %s\033[0m\n' "$1" >&2; exit 1; }

# 1+2. Format + test suite — delegated to the fast gate (the same script the
#      pre-push hook runs), so the routine and full gates cannot drift on these
#      two steps. Identical to the CI "fmt" job and "Cargo test" step. The test
#      run needs the JSONTestSuite submodule for full coverage (it skips that
#      corpus gracefully if absent).
"$SCRIPT_DIR/check-fast.sh" || fail "fast gate (fmt + cargo test --all) failed"

# 2a. Fancy-oracle differential (docs/LOOKAROUND_SCOPE.md): the default build has
#     zero fancy-regex code, so the L0 whole-lexer differential
#     (tests/test_scanner_differential.rs) only runs under the TEST-ONLY
#     fancy-oracle feature, which resurrects the Regex reference backend's fancy
#     side-probes as the independent oracle. It is the only test target gated on
#     the feature, so it is named explicitly — running every integration target
#     under the feature would just repeat step 2. `--lib` keeps the lib unit
#     tests (scanner.rs's cfg-gated probe code) in the feature build, the
#     "runs under both builds" contract in Cargo.toml. Matches the CI step.
note "Fancy-oracle differential: cargo test -p lark-rs --features fancy-oracle --lib --test test_scanner_differential"
( cd "$LARK_RS_DIR" && cargo test -p lark-rs --features fancy-oracle --lib --test test_scanner_differential ) \
  || fail "fancy-oracle differential failed — the lowered engine diverged from the fancy reference"

# 2b. Deterministic scaling gates — the regression nets keyed on the src/perf.rs
#     work counters only run with the perf-counters feature, so they are a no-op
#     in `cargo test --all`. One invocation, one build (matches the CI "Scaling
#     gates" step): Earley super-linearity (#56), CYK cubic envelope (#87), lexer
#     linear scan (#104), dense-DFA build cost (docs/LEXER_DFA_PLAN.md), LALR
#     parse-table density (#367).
note "Scaling gates: cargo test --features perf-counters --test test_earley_scaling --test test_cyk_scaling --test test_lexer_scaling --test test_lexer_dfa_build_scaling --test test_lalr_table_scaling"
( cd "$LARK_RS_DIR" && cargo test --features perf-counters \
    --test test_earley_scaling --test test_cyk_scaling \
    --test test_lexer_scaling --test test_lexer_dfa_build_scaling \
    --test test_lalr_table_scaling ) \
  || fail "a scaling gate failed — a complexity regression (see the failing test_*_scaling.rs)"

# 2b2. wide-positions gate (ADR-0040) — the u64 storage path (default build tests
#      u32). `cargo test --all` never sets it; mirror the CI steps, including the
#      span-tree combination where a missed position cast would surface.
note "wide-positions gate: cargo test --features wide-positions (+ span-tree/perf-counters test_span_tree)"
( cd "$LARK_RS_DIR" && cargo test --features wide-positions ) \
  || fail "wide-positions (u64) gate failed"
( cd "$LARK_RS_DIR" && cargo test --features "wide-positions span-tree perf-counters" --test test_span_tree ) \
  || fail "wide-positions + span-tree gate failed"

# 2c. python.lark LALR build gate (#79) — #[ignore]d because the build is slow
#     (~18s debug), so `cargo test --all` skips it. Matches the CI step.
note "python.lark LALR build gate: cargo test --lib tests::test_python_lark_builds_under_lalr -- --ignored --exact"
( cd "$LARK_RS_DIR" && cargo test --lib tests::test_python_lark_builds_under_lalr -- --ignored --exact ) \
  || fail "python.lark LALR build gate failed — the full python.lark table no longer builds"

# 3. Oracle-freshness gate — regenerate from Python Lark and require no diff.
#    (Needs 'pip install lark' and the JSONTestSuite submodule:
#     git submodule update --init lark-rs/tests/corpora/JSONTestSuite)
note "Oracle freshness: regenerate, expect no diff"
command -v python3 >/dev/null 2>&1 || fail "python3 not installed"
(
  cd "$LARK_RS_DIR"
  python3 tools/generate_oracles.py >/dev/null
  python3 tools/extract_lark_compliance.py >/dev/null
  python3 tools/generate_wild_oracles.py >/dev/null
) || fail "oracle generators failed (needs 'pip install lark regex' — regex backs the wild bank's synapse_storm grammar)"

if ! ( cd "$REPO_ROOT" && git diff --quiet -- lark-rs/tests/fixtures/oracles ); then
  printf '\nCommitted oracles are stale — regeneration changed:\n' >&2
  ( cd "$REPO_ROOT" && git --no-pager diff --stat -- lark-rs/tests/fixtures/oracles ) >&2
  fail "oracle-freshness gate failed — commit the regenerated fixtures"
fi

printf '\n\033[1;32m✅ All gates passed.\033[0m\n'
