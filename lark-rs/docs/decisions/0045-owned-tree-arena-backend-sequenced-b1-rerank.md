# ADR-0045: Owned-tree layout lands sequenced — opt-in arena backend before any default `Tree`/`Child` change; baked DFA re-ranked to a one-shot/standalone lever

- **Status:** Proposed (pending architect ratification)
- **Date:** 2026-07-03

## Context

The 2026-07-03 measurement spikes (consolidated in
`docs/notes/spike-parser-optimizations-2026-07-03.md`; triangulated by three
independent harnesses) quantified the owned default tree's layout levers with
deterministic signals (counting allocator + node-count equality; token-stream
differential for the lexer):

- **M2 (label interning):** exactly −1.000 alloc/internal-node, ~1.2–1.4×.
- **M5 (flat shared `kids[]` child arena):** exactly −1.000 alloc/node, ~1.4×,
  and it nearly halves total allocated bytes on the owned path.
- **M2+M5 stack linearly:** −2.000 allocs/node, ~1.6×; the residual is the owned
  token `String`s (M6 territory). SmallVec of child *nodes* is structurally
  impossible without boxing (net +1.37 allocs/node — killed); SmallVec of child
  *refs* into an arena is viable (−0.979/node) but trails the flat arena.
- **B1 (baked/directly-executable DFA):** ~4× at the scanner level on the same
  automaton, but the scanner drive is ~8% of `parse()` today → ~6% end-to-end.
  A leaner drive loop alone (no bake, no representation change) captures ~2× of
  the scan. The bake's real payoff surfaced on the standalone/one-shot path
  (see ADR-0046), where it also erases build cost and the `regex` dependency.

Adjacent decision already made: **ADR-0042** (Accepted) stores `Tree.data` /
`Token.type_` as `Arc<str>` behind `&str` accessors — that removes the per-node
label and per-token type-name allocations on the **default** path (the M2
allocation win for default callers) without a `u32` representation change, and
its accessor encapsulation means storage can evolve again without a second
public break. The remaining measured default-path lever is therefore **M5**
(the per-node child `Vec`).

The fork (#619): productize the full M2+M5 layout as (a) a new opt-in output
backend behind the `OutputBuilder` seam, (b) a breaking change to the default
`Tree`/`Child` representation (interned-id labels, arena-handle children), or
(c) sequenced — (a) first, then decide (b).

## Decision

1. **B1 is re-ranked**: the baked DFA is *not* an in-process reused-path lever
   at the current profile; it is a **one-shot / standalone lever**, executed on
   the generated-parser surface per ADR-0046. Revisit in-process only when
   lexing is again a dominant share of `parse()` (i.e. after the output side
   stops being allocation-bound). The **leaner drive loop** (~2× scan, no new
   representation, `raw_match_at` pattern) remains the one in-process scanner
   improvement worth taking when convenient.
2. **M2+M5 land sequenced (option c)**: ship an **opt-in owned arena/interned
   output backend** behind the existing `OutputBuilder` seam (the ADR-0027/0029
   line; the interned rule id is already on the seam). The default
   `Tree`/`Child` representation stays as ADR-0042 leaves it; a default-
   representation change is a separate future decision, informed by the
   backend's API feedback and the M6 (token-value) picture. The
   SmallVec-of-child-*refs* design is the recorded fallback if a single shared
   child buffer proves undesirable (e.g. subtree-local mutation).
3. **`Token.type_` field-drop: superseded by ADR-0042 for the allocation
   intent; the hot-path residual is deferred, not delivered.** ADR-0042
   eliminates the per-token label *allocation* via interned `Arc<str>` with no
   lifetime tie — a counting allocator shows an interned-`Arc<str>` token and
   an id-only token allocate identically. The field-drop itself is ADR-0042's
   **explicitly-rejected Option 2** (resolve `type_` from `type_id`; rejected
   on grammar-lifetime blast-radius grounds) — so it was priced and declined,
   not absorbed. A real residual remains on the floor: `Token` still carries
   `type_` (16 B + two atomic refcount RMWs per token) redundantly with the
   `Copy` `type_id` — measured ~+22% of token-handling today (owned-`String`
   values) and up to ~4.8× in a value-less (M6/`SpanTree`) token path
   (same-session microbench *isolation*, not end-to-end; the bench also used
   `&'static` names, hiding Option 2's lifetime cost). **Deferred to the
   M6/span `Token`-surface change**, where the residual is largest and an API
   break is already being paid; re-measure end-to-end there and weigh against
   Option 2's lifetime blast radius. This is *not* a reversal of ADR-0042:
   `Arc<str>` is a strict Pareto win over the current `String` and lands
   as-is.

## Consequences

- **Buys:** the full −2 allocs/node / ~1.6× is available to callers who opt in,
  with zero risk to default-path behaviour; no second breaking `Tree` change on
  top of ADR-0042's accessor churn; the standalone surface (ADR-0046's interned
  output) doubles as a proving ground for the interned representation.
- **Costs / rules out:** default `parse()` callers only get ADR-0042's share of
  the win until/unless a future ADR changes the default representation; two
  owned output shapes to keep green.
- **Tripwire to revisit (b):** real adoption of the opt-in backend plus a
  still-standing gap to the default path, or M6 work forcing a token/tree
  representation change anyway — then the (b) decision reopens with usage data.
- **Grounding / gates:** an implementation PR needs oracle-green across all
  banks and a deterministic allocs-per-node envelope (counting-allocator
  protocol or a `perf-counters` counter, per ADR-0007). Evidence is re-runnable
  via `examples/owned_tree_layout_alloc.rs` and the `baked-dfa-spike` examples.
- Resolves #619 (decision memo). Blast radius: additive backend →
  implementation PRs are reviewable under the normal DoD; any *default*-
  representation move stays escalate-tier.
