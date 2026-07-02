# ADR-0041: A committed seed range is the grammar fuzzer's regression tier

- **Status:** Proposed (pending architect ratification)
- **Date:** 2026-07-02

## Context

[ADR-0012](0012-differential-fuzzer-active-oracle.md) gave the differential
fuzzer two tiers: *discovery* (fresh entropy, nightly/manual, never gating) and
*regression* (the committed corpus `fuzz/inputs.json`, replayed per PR). That
regression tier works because input fuzzing runs against *trusted, committed
grammars*: a minimized find is one input string, frozen into the corpus like any
oracle case.

Grammar fuzzing (`--fuzz-grammars`, #38) has no such corpus to freeze into: each
case is a *random grammar* plus inputs, and committing random grammars wholesale
would be committing the haystack, exactly what ADR-0012 forbids. But the mode is
deterministic given a seed — a seed range **is** a corpus, stored in one line.
Epic #208's done-when required exactly this: the fuzzer running clean "over a
committed seed range", so that "is the LALR core still correct on grammars we
didn't think to write?" stays a falsifiable, regression-guarded question after
the epic closes.

## Decision

The grammar fuzzer gets a third tier: a **committed seed range**, pinned in
`scripts/fuzz-seed-range.sh` (initially seeds 1..300 × 40 grammars × 15 inputs,
verified 0 divergences on 2026-07-02), replayed by the gating
`seed-range-regression` job in the nightly fuzz workflow via `--gg-seed-range`.
A RED there is a **regression** (a once-clean seed diverged), not a discovery
find. The tier stays off the PR critical path (nightly, like discovery — a
multi-minute Python-Lark-in-the-loop sweep has no place in per-PR CI), but
unlike discovery it fails its job on any divergence.

The range is a ratchet with XFAIL-ledger semantics: **widening** it is routine
once the new seeds are verified clean; **narrowing** it needs the same scrutiny
as shrinking an XFAIL allow-list, because dropping a seed un-guards every
divergence class that seed covers.

## Consequences

- Epic-level fuzzer results stay guarded after the epic closes: the #176/#210
  divergence classes (and every other bug the 1..300 sweep would catch) can't
  silently regress. The committed corpus costs one shell script, not thousands
  of committed random grammars.
- A seed only replays under the same batch parameters, so find reports carry
  their full recipe (`seed` + `count`/`gg_rules`/`gg_inputs`), and
  `--gg-seed-range` without `--fuzz-grammars` is a loud error — a vacuous green
  on a gating sweep is the failure mode this tier exists to prevent. Pinned by
  `tools/tests/test_fuzz_differential.py::test_seed_range_find_path`.
- Per-seed batches are byte-identical between a range sweep and a standalone
  `--seed` run (fresh RNG per seed), so any find replays from its single seed.
- Tripwire: if the sweep's wall-clock outgrows the nightly budget (range
  widening, slower oracle), shard the range across matrix jobs or batch differ
  invocations per grammar rather than shrinking the range.
