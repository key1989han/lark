# ADR-0042: intern `Tree.data` / `Token.type_` labels behind `&str` accessors (`Arc<str>` storage)

- **Status:** Accepted — ratified by the architect's merge of PR #611 (2026-07-03)
- **Date:** 2026-07-03

## Context

The 2026-07-02 perf spike (`docs/notes/perf-spike-2026-07-02.md`) profiled the
default `parse()` path after the auto-tier wins already in `master` (child-buffer
steal/scratch reuse, O(1) cursor advance). The remaining allocation count is
dominated by **name strings the engine already knows by id**:

| site | blocks per 146 KB JSON parse |
|---|---:|
| `build_token` — terminal-name `String` clone (`names[id].clone()`) | ~56 K (one per token, incl. ignored) |
| `TreeOutputBuilder::reduce` — label `to_string()` (`tree_name`) | ~28 K (one per node) |

A ceiling simulation (labels/type-names replaced with empty strings —
perf-equivalent to interning) measured **~1.20× further** on `parse()`
(14.7 → 12.2 ms on the 146 KB floor workload, same box, same session). The win
also lifts the `NullBuilder` floor, because the terminal-name clone happens in the
lexer, upstream of any output builder.

The tension: `Token.type_: String` and `Tree.data: String` are **public fields**,
re-exported from the crate root and surfaced through the PyO3/WASM/C bindings.
Removing the per-token / per-node allocation means changing their storage
representation — an escalate-tier public-API decision (PRINCIPLES.md §4/§6), which
is why it was raised as `needs-decision` #604 rather than taken autonomously.

Options considered (from #604):

1. **`Arc<str>` storage behind `&str` accessors** — clone becomes a refcount bump.
   Storage is private; the public surface is an accessor method returning `&str`.
2. **Fully interned** — store nothing; resolve `type_` / `data` lazily from
   `type_id` / rule id through the grammar tables. Fastest, but gives tokens/trees
   a lifetime tie to the grammar (or forces the accessor to take a context),
   infecting every public type that holds a `Token`/`Tree`.
3. **Leave the default path alone** — forgo the win, and point throughput-sensitive
   users at the span/tape backends (`SpanTree`), which already skip both
   allocations.

## Decision

Adopt **Option 1**: store the `Token` type label and `Tree` data label in private
`Arc<str>` fields and expose them through public `&str` accessor **methods**. A
clone of a `Token`/`Tree` becomes an atomic refcount bump rather than a heap
`String` allocation + copy; equal labels across tokens/nodes share one backing
allocation (the grammar's interned name), collapsing the ~56 K + ~28 K per-parse
allocations to O(distinct labels).

This follows the encapsulation precedent set by **ADR-0040** (source positions:
private `u32` storage behind `usize` accessor methods): the storage representation
is removed from the public API, so it can change again later without a second
breaking change, and no lifetime is imposed on the public `Token`/`Tree` types
(the reason Option 2 is rejected — a grammar-lifetime tie on `Token`/`Tree` is a
far larger blast radius than the ~1.20× justifies).

## Consequences

- **Buys:** eliminates the per-token terminal-name clone and the per-node label
  `to_string()`; measured ceiling ~1.20× on the 146 KB `parse()` floor, and a
  lower `NullBuilder` floor (the lexer-side clone is removed too).
- **Costs:** a breaking API change — field access (`tok.type_`, `tree.data`)
  becomes an accessor call (`tok.type_()`, `tree.data()`) for every external
  reader, mirrored in the PyO3/WASM/C bindings and the test suite. One-time churn,
  then stable regardless of any future storage change (the encapsulation payoff).
  Pre-users, breaking the public API is free (ADR-0025). Check ADR-0036 (PyO3
  `Token` IS-A `str`) for what the binding reads.
- **Grounding / gate:** behaviour-preserving by construction (same strings,
  different storage) → the full suite + all compliance banks are the regression
  net. A deterministic `lexer_name_alloc`-style counter (`perf-counters` feature,
  per ADR-0007) pins the zero-allocation claim without wall-clock prose, alongside
  the existing output-shape counters.
- **Scope:** this is the default-`parse()` allocation half of the ADR-0011
  "make the default tree cheaper" line; it composes with, and is independent of,
  the `SpanTree` zero-copy backend (which sidesteps these allocations by not
  materializing an owned tree at all).
- **Merge tier:** the implementation PR changes public `Token`/`Tree` accessors →
  **escalate-tier** (architect merges), per ADR-0016.

## Validation

- Library compiles; full test suite + LALR/Earley/dynamic/CYK/JSONTestSuite banks
  green (behaviour-identical).
- The new allocation counter reads zero per-token/per-node label allocations on the
  `parse()` path; positive control on a pre-change build.
- Wall-clock delta recorded in `docs/notes/perf-spike-2026-07-02.md` / `BENCH.md`
  as supporting evidence only (the counter is the gate).
