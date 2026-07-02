# ADR-0040: source positions stored as `u32` behind `usize` accessors, `wide-positions` for >4 GiB

- **Status:** Proposed (pending architect ratification)
- **Date:** 2026-07-02

## Context

The perf spike (`docs/notes/perf-spike-2026-07-01.md`) profiled the LALR hot path
and found it dominated by `memcpy` of the parser's value-stack element. That
element is `GSlot<Child>`; its size is driven by `Meta` (six `Option<usize>` =
104 bytes) and `Token` (owned strings + six `usize` positions). Every
push/pop/drain/splice of the stack memcpys the whole element, so on deeply-nested
output the position payload is a first-order cost, not bookkeeping.

The lever: store positions in `u32` instead of `usize`. `Option<u32>` is 8 bytes
where `Option<usize>` is 16, so `Meta` shrinks 104 → 56 bytes and `GSlot<Child>`
264 → ~168 bytes — a pure-memcpy win shared by every engine (LALR/Earley/CYK) and
every `OutputBuilder` backend, including the `SpanTree` fast path and the
`NullBuilder` floor. (`u64` would *not* help — `Option<u64>` is also 16 bytes; the
win is specifically the 32-bit width.)

**Measured marginal, on top of the merged auto-tier pass (E1/E2/E3/E5, already in
`master`), `cargo bench --bench parse`, `min_ns`, one box:** json_large
11.73 → 10.15 ms (**~1.16×**), json_medium ~1.08×, arith_large/arith_small ~flat.
The win is concentrated in **deep-tree output** (JSON), where the per-node `Meta`
memcpy dominates; a flat expression grammar carries few positions per byte and
sees ~0%. (An earlier spike write-up called this "the largest contributor" — that
conflated it with E5's per-reduction buffer pre-size, which drove the arith win and
already shipped in the auto-tier PR; ~1.16× on JSON is the honest isolated figure.)

The tension is that `Token` and `Meta` are **public** types, re-exported from the
crate root and surfaced through the PyO3/WASM/C bindings. The naïve change — flip
`pub line: usize` to `pub line: u32` — makes the *storage width part of the public
API*, which (a) pins a 4 GiB (`u32::MAX`) input ceiling into the contract with no
escape hatch, and (b) is "shifty": a later `usize`/`u64` mode would be a second
breaking change, and a feature that flips a public field type breaks callers under
Cargo's additive feature unification.

Options considered:

1. **Public `u32` fields.** Smallest diff, but bakes the storage width — and the
   4 GiB ceiling — into the public API, with no clean path to a wide mode.
2. **Leave positions `usize`.** No churn; forgo the deep-tree memcpy win.
3. **Encapsulate: private `u32` storage behind `usize` accessors, width behind a
   feature** (this proposal). Storage width is no longer public, so a `wide-positions`
   mode is additive-safe and the ceiling is liftable.

Related fork: `performance-strategy.md` §5 ("should the default `Tree`/`Token`
itself get cheaper?"). This is the smallest, lowest-risk instance of "the default
gets cheaper," and the accessor shape is the precedent for doing it safely.

## Decision

*(Proposed — architect's call.)* Make the six position fields of `Token` and `Meta`
`pub(crate)`, stored in a private `PosInt` alias, and expose them through public
accessor **methods**:

- `Token::{line,column,end_line,end_column,start_pos,end_pos}() -> usize`
- `Meta::{line,column,end_line,end_column,start_pos,end_pos}() -> Option<usize>`

`PosInt` is `u32` by default; the **`wide-positions`** cargo feature widens it to
`u64` (restoring the pre-change layout and lifting the ceiling) for the rare
consumer parsing >4 GiB inputs. Because storage is private and the accessors always
return `usize`/`Option<usize>`, the feature only ever enlarges private storage — it
never changes a public type, so enabling it anywhere in a dependency graph is safe
under Cargo's additive feature unification.

## Consequences

- **Buys:** `Meta` 104 → 56 B, `GSlot<Child>` 264 → ~168 B; measured marginal
  ~1.16× json_large / ~1.08× json_medium / ~flat on flat grammars (see Context).
- **Costs:** a breaking API change — field access (`tok.line`) becomes a method
  call (`tok.line()`) for every external reader, mirrored in the PyO3/WASM/C
  bindings and the test suite. (`type_id`, `type_`, `value` stay public fields; only
  the six positions move to accessors.) One-time, then stable regardless of any
  future width change.
- **Ceiling, and its escape hatch:** default `u32` caps correct positions at 4 GiB
  (`u32::MAX`) of input; `--features wide-positions` (u64) removes the cap. In
  practice the default `parse()` builds an owned tree at ~3 allocations/byte, so a
  4 GiB input needs >12 GB of tree — no realistic full-tree parse approaches the
  cap; the feature exists mainly for the zero-copy span backend's future large-input
  use. **A `debug_assert!` at token construction should be added as the tripwire so
  the cap fails loudly rather than truncating silently** (follow-up within this PR).
- **Why not `u64` unconditionally:** `Option<u64>` is 16 B (same as today) — it
  would forgo the entire win. The 32-bit width *is* the lever.
- **Enforcement / evidence:** the library compiles under both the default and
  `--features wide-positions`; the full suite, the four scaling gates, and the L0
  scanner differential are green. Wall-clock deltas recorded in
  `docs/notes/perf-spike-2026-07-01.md` and `BENCH.md`.
