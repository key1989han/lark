# Perf spike 2026-07-01 — LALR hot-path allocation/dispatch wins

- **Status:** Findings note for a perf spike — **both PRs are now merged to
  `master`**: the auto-tier subset (E1/E2/E3/E5) via **PR #601**, and the E4
  public-API change (`u32` positions behind `usize` accessors, ADR-0040) via
  **PR #602** (architect-merged). Not policy. Wall-clock numbers are one shared box
  (4-core Xeon @ 2.80 GHz, rustc 1.94.1, release+LTO `bench` profile), measured
  **back-to-back against `master` in one session** — only same-session ratios travel
  (BENCH.md discipline).
- **Method:** profile first (callgrind + DHAT on `examples/profile_parse parse`,
  the 92 KB `json_large` workload), one experiment per suspected cost, each kept
  only with the full suite + scaling gates + scanner differential green.

## What landed in #601 (E1/E2/E3/E5) — 1.44–1.96×, no public API change

`cargo bench --bench parse`, `min_ns` (BENCH.md's least-noise estimator), this box:

| workload | master | E1/E2/E3/E5 | ratio |
|---|---:|---:|---:|
| parse json_small | 60.8 µs | 42.3 µs | **1.44×** |
| parse json_medium | 1.46 ms | 0.93 ms | **1.58×** |
| parse json_large | 20.24 ms | 11.73 ms | **1.73×** |
| parse arith_small | 12.7 µs | 7.2 µs | **1.76×** |
| parse arith_large | 791 µs | 404 µs | **1.96×** |

Profile deltas (json_large, callgrind): SipHash 5% → ~0%; instruction count and
heap churn both down ~a third. Earley is essentially unchanged (~1.06× in the
spike's fuller run) — its cost is the chart/forest, not token plumbing; that is a
separate spike.

### The experiments

| id | change | where |
|---|---|---|
| E1 | `TokenSource::peek_type`/`take_current` — LALR dispatch reads the cached token's id; SHIFT *moves* the token, never clones it. Each token was materialized 3× (built, cloned on `peek`, cloned into the builder) = 6 String allocs; now 1×. | `token_source.rs`, `lalr.rs` |
| E2 | dense per-token tables: terminal-name `Vec` (was `HashMap<SymbolId,String>`), contextual state→scanner `Vec` (was `HashMap<usize,usize>`), `%ignore` bitset (was `HashSet`). Removes three per-token SipHash probes. | `lexer/mod.rs`, `lexer/dynamic.rs` |
| E3 | `shape_reduction` drains the value-stack tail in place — no per-reduction `drain().collect()` intermediate `Vec`. | `tree_builder.rs`, `lalr.rs` |
| E5 | `shape_reduction`'s `kept` buffer pre-sized to the rule arity (kills the 4→8→16… growth reallocs — the dominant arith win); both scanner backends store the `unless` retype tables densely by `SymbolId::index()` (the last per-token SipHash probe). | `tree_builder.rs`, `dfa.rs`, `scanner.rs`, `plan.rs` |

All four are engine-internal; no public type changes. "Lean on counter gates"
(ADR-0007): E3/E5 preserve the `child_vec_allocs` (one per reduction) and output-
shape counters exactly, and `test_output_counters` + `test_child_vec_scaling`
stay green — the deterministic proof that the reduction structure is unchanged.

### What was actually costing (found ≠ guessed)

The profiler indicted four things, not the parse algorithm (single-digit percent):

1. **Every token was materialized three times.** `Contextual::lex_next` builds the
   `Token` (type_ clone + value alloc), `TokenSource::peek` **cloned** it out of the
   cache (the trait returned `Token` by value), and the SHIFT arm **cloned** it again
   into the builder — six String allocs per token where two suffice (E1).
2. **Four per-token SipHash probes** that should be array reads: `names[&id]`, the
   contextual `state_to_scanner.get(&state)`, `ignore.contains(&id)`, and
   `unless.get(&id)` in both scanner backends (E2 + E5's dense `unless`).
3. **Every reduction `drain().collect()`ed** the popped slots into a fresh `Vec` and
   grew its `kept` buffer from zero (4→8→16… reallocs) (E3 + E5).
4. **The value-stack element was 264 bytes** — `Child` 152 + `Meta` 104 (six
   `Option<usize>`); every push/pop/drain/splice memcpys the lot. This is the biggest
   memcpy source and the one E4 (below) targets.

Profile deltas after E1–E5 (json_large, callgrind, 5 parses): instructions
1103 M → **712 M** (−35%); SipHash 5% → ~0%; heap churn 183 MB → 123 MB per parse
(−33%); memcpy share 31.5% → 26.2% of the now-smaller pie. Allocation **block count
is unchanged** (~243 K/parse) — the count, not the bytes, is the next frontier (the
tape/arena work #242/#243 targets).

## E4 — `u32` positions behind `usize` accessors (landed, #602)

Storing `Token`/`Meta` positions in a private `u32` (behind `usize` accessor
methods) shrinks `Meta` 104 → 56 B and the value-stack element `GSlot<Child>`
264 → ~168 B — a pure-memcpy win every backend shares. Measured **in isolation** on
top of the merged E1–E5 (not the cumulative figure): **~1.16× json_large**, ~1.08×
json_medium, ~flat on flat expression grammars — the win concentrates in deep-tree
output where per-node `Meta` memcpy dominates. (`u64` would give *zero* win:
`Option<u64>` is 16 B like `usize`; the 32-bit width is the lever. An earlier draft
of this note over-credited E4 as "the largest contributor" — that conflated it with
E5's buffer pre-size, which drove the arith win and shipped in #601.) It changes the
**public API** (positions become accessor methods), so it took the escalate-tier
route and rides `performance-strategy.md` §5 ("should the default `Tree` get
cheaper?"). Landed as **ADR-0040** (PR #602, architect-merged): storage width is
private behind a `wide-positions` feature, so the `u32::MAX` ceiling (~4 GiB of
ASCII-range input) is a switchable mode, with a `checked_pos` debug tripwire instead
of silent truncation.

## Wild bank — the same wins on real grammars

Cross-check against `cargo bench --bench wild` (real-world grammars in `tests/wild/`),
same box, baseline worktree → spike, median:

| project | engine | baseline | spike | ratio |
|---|---|---:|---:|---:|
| cel | LALR | 3.02 ms | 1.87 ms | **1.62×** |
| lark_lark | LALR | 6.06 ms | 3.66 ms | **1.66×** |
| mappyfile | LALR | 20.19 ms | 14.01 ms | **1.44×** |
| matter_idl | LALR | 71.7 ms | 44.7 ms | **1.60×** |
| poetry_markers | LALR | 62.4 µs | 36.9 µs | **1.69×** |
| poetry_pep508 | LALR | 63.8 µs | 37.5 µs | **1.70×** |
| pylogics_ltl | LALR | 66.3 µs | 38.7 µs | **1.71×** |
| pyquil | LALR | 2.39 ms | 1.55 ms | **1.54×** |
| tartiflette | LALR | 5.09 ms | 3.35 ms | **1.52×** |
| vyper | LALR | 6.64 ms | 4.14 ms | **1.60×** |
| dotmotif / mistql | Earley | 6.21 / 22.4 ms | 6.16 / 22.2 ms | ~1.0× |

LALR real-world grammars land 1.44–1.71× (geomean ~1.6×), consistent with the
synthetic rows; Earley is flat, as expected (its cost is the chart/forest — a
separate spike). This measurement was taken on the full spike (E1–E5, i.e. including
E4); E1–E5-without-E4 lands slightly lower per the ~1.16× E4 marginal above.

## The ceiling: NullBuilder floor (instrument: `examples/parse_floor.rs`)

Parsing a 146 KB JSON through public `parse_into` with a do-nothing builder
(`Value = ()`) pays lexing + LALR dispatch + all shaping control flow but
materializes nothing. In the fuller spike run, **~47–60% of `parse()` time is
output materialization** even after these wins (ADR-0011 still holds), and the
opt-in `SpanTree` backend sits mid-gap, not at the floor. That is the honest
target for the tape/arena backends (#242/#243) and label interning, now with a
number attached.

## Identified, not landed (next spike's shopping list)

- **Double char-scan per token:** `build_token` walks the value to compute end
  line/col + char count; `advance_by_chars` re-walks the same text to move the
  cursor. Carry the post-token cursor with the cached token for O(1) advance.
- **Interned labels / `Arc<str>` token type names:** kills 2 allocs per
  node/token on the default path — public-surface (§5).
- **Earley:** untouched by these wins; needs its own profile-first pass (chart/
  forest representation, `SmallVec` item lists, the tracked explicit-walk fix).
- **Allocator:** mimalloc as the embedding binary's `#[global_allocator]` was worth
  ~1.15–1.25× on top of E1–E5 in the spike (E0 probe, `RUSTFLAGS="--cfg
  spike_mimalloc"`: ≈−20% vs the glibc baseline, ≈−12% once E1–E4 had removed the
  cheap allocations) — roughly **≈2× combined on JSON**. A `#[global_allocator]` is
  the embedding *binary's* choice, not a library dependency, so this stays a
  documented finding: worth calling out in BENCH.md/README and worth considering for
  the PyO3/WASM bindings (whose users can't pick the allocator themselves).

## Correctness

Full suite green (675/675) after every kept experiment, plus the four scaling
gates (`--features perf-counters`), the output/child-vec counter gates, and the
L0 scanner differential (`--features fancy-oracle`, 0 divergences). No oracle
regenerated, no XFAIL touched, no grammar changed.
