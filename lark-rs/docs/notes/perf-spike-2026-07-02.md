# Perf spike 2026-07-02 — attacking the output-materialization floor

- **Status:** findings note for the second LALR perf spike, picking up exactly
  where `perf-spike-2026-07-01.md` left off (its "identified, not landed"
  shopping list). Not policy. Wall-clock numbers are one shared box (4-core
  Xeon @ 2.80 GHz, rustc 1.94.1, release+LTO `bench` profile), measured
  **back-to-back against `master` (post-#601/#602) in one session** — only
  same-session ratios travel (BENCH.md discipline).
- **Method:** profile first (DHAT allocation-*count* breakdown on
  `examples/profile_parse parse`, `examples/parse_floor` as the ceiling
  instrument), one experiment per suspected cost, each kept only with the full
  suite (705/705) + the counter/scaling gates + the L0 scanner differential
  green.

## Where the first spike left the frontier

The 2026-07-01 spike ended with: ~47–60% of `parse()` is still **output
materialization** (the NullBuilder floor), and the allocation **block count**
is unchanged — the count, not the bytes, is the frontier. This box reproduced
that: 51.4% materialization share, and DHAT attributed the parse-phase blocks
of one 92 KB `json_large` parse as:

| site | blocks/parse | bytes |
|---|---:|---:|
| `shape_reduction`'s per-reduction `kept` child buffer | 46,073 | **81.4 MB** |
| `build_token` — terminal-name `String` clone | 29,185 | 213 KB |
| `build_token` — owned `Token.value` | 29,185 | 92 KB |
| `TreeOutputBuilder::reduce` — label `to_string()` | 15,873 | 85 KB |

Two structural findings fell out of the biggest line:

1. **The per-reduction buffer wasn't just a count problem — it was a hidden
   O(n²).** EBNF `+`/`*` lower to *left-recursive* transparent helpers
   (`_p: item | _p item`), so every step of a growing list **re-copied the
   whole accumulated prefix** into a fresh buffer: a length-n array cost
   1+2+…+n element moves at ~176 B each. That is what 81 MB of churn against
   85 KB of actual labels means.
2. **A dead-end worth recording:** the first fix attempt was a recycling
   *pool* of child buffers. It made things **worse** (bytes 110 → 150 MB,
   blocks barely moved): the pool filled with capacity-1 buffers from
   `expand1` collapses, so node reductions still realloc'd on every take, and
   `reserve` growth churned. The pool treated the symptom (allocation) while
   the disease was the *copy*.

## What landed in the working tree (auto-tier, engine-internal)

Three changes, none touching a public type:

| id | change | where |
|---|---|---|
| S1 | **Inline-buffer steal + `expand1` fast path.** A reduction whose first kept contribution is a transparent splice *steals* that buffer (the left-recursive helper case — the prefix is never copied again); an arity-1 `?rule` collapse over a plain value returns the stack slot untouched (no buffer exists at all). | `tree_builder.rs` |
| S2 | **O(1) cursor advance (the double char-scan fuse).** `build_token` already walks the matched text once for newline-aware end positions; `advance_by_chars` re-walked the same bytes to move the cursor. The scanner now reports the matched byte length, and `LexerState::advance_past` jumps straight to the token's own end position — one walk per token (including per ignored token) instead of two. | `lexer/mod.rs`, `token_source.rs` |
| S3 | **`ReduceScratch` recycling.** The node branch *drains* its child buffer instead of consuming it, so the emptied buffer (and, for reading builders, the values buffer) cycles through a per-parse scratch instead of round-tripping the allocator. Also: span-mode (value-less) tokens skip the terminal-*name* clone too — the span/tape builders resolve names lazily from `type_id`. | `tree_builder.rs`, `lalr.rs`, `lexer/mod.rs` |

Deterministic proof (the #583 gate, `--features perf-counters`):
`child_vec_allocs` now charges only *fresh* buffers, and on the list-grammar
gate it is **exactly 1 per parse, independent of n** (was `2n+1` — the gate
was updated to pin the new closed form; its own doc anticipated exactly this:
"the ratio drops below 1"). On `json_large`, fresh child buffers fell from one
per reduction (39.4 K) to below the node count (18.9 K with S1 alone), and
DHAT heap churn halved (110 → 52.6 MB per parse).

`cargo bench --bench parse`, `min_ns`, master → S1+S2+S3, this box:

| workload | master | spike | ratio |
|---|---:|---:|---:|
| parse json_small | 41.1 µs | 36.3 µs | **1.13×** |
| parse json_medium | 801 µs | 744 µs | **1.08×** |
| parse json_large | 9.96 ms | 8.02 ms | **1.24×** |
| parse arith_small | 6.64 µs | 5.54 µs | **1.20×** |
| parse arith_large | 396 µs | 345 µs | **1.15×** |

Earley rows are flat (1.00–1.04×) as expected — its forest walk shapes through
the *concrete* `assemble` path, untouched here (still a separate spike). Build
rows are flat. On the 146 KB `parse_floor` workload the cumulative owned-tree
win is larger (21.1 → 14.6 ms, **1.44×**) because that shape is list-heavy —
exactly where the O(n²) splice copying bit hardest.

## The tape backend (#243 C8c) — the floor is reachable

`examples/parse_floor.rs` (146 KB JSON), all four points, spike build:

| path | time | vs owned |
|---|---:|---:|
| `parse()` owned tree | 14.57 ms | 1.0× |
| `parse_span()` (`span-tree`) | 11.58 ms | 1.26× |
| **`parse_tape()` (`tape-tree`, new)** | **7.64 ms** | **1.91×** |
| `parse_into(NullBuilder)` — the floor | 7.36 ms | 1.98× |

The new **`TapeTree`** backend (`--features tape-tree`, internal-only,
`src/parsers/tape.rs`) appends the whole parse to two flat arrays (`nodes`,
`kids`): a token entry carries positions + the byte span of its text (sliced
zero-copy from the input at read time), a node entry carries its rule index +
a range into `kids`; labels stay interned. It lands **3.7% above the
NullBuilder floor** — materialization cost is essentially gone. Versus
`master`'s owned `parse()` at the start of this spike (21.1 ms), a tape
consumer gets **~2.8×**.

DHAT, 146 KB parse (grammar build excluded): owned path ≈ **168 K**
parse-phase blocks; tape path ≈ **2 K** — about **0.03 allocations per node**,
far below the one-per-node bound #243 names. The prize `parse_floor`
quantified is real and this backend collects nearly all of it.

Grounding (all in `tests/test_tape_tree.rs`, mirroring the C8 pattern):

- **Relative oracle (ADR-0026):** `parse_tape(input).materialize()` is
  byte-identical to `parse(input)` over curated shaping-heavy grammars *and
  the whole LALR compliance bank* (no XFAIL list — relative, zero divergences
  required), plus error/OK parity.
- **Counters (ADR-0007):** `tree_nodes_built == 0`,
  `token_value_string_bytes == 0`, `lexer_token_value_bytes == 0`, while
  `semantic_reduce_calls` keeps the exact closed form.
- **Known caveats, documented in the module:** filtered punctuation leaves
  orphaned (unreferenced) token entries on the tape; span/tape-mode tokens no
  longer carry an owned `type_` name (cold error paths resolve from
  `type_id`); `materialize` recurses to tree depth (projection utility, not an
  engine path — same caveat as `SpanNode::materialize`).

Per #243 this stays **internal / needs-decision**: promotion to a committed
`OutputMode` is an architect call gated on a named consumer. The natural
consumer story to evaluate: the PyO3/WASM bindings (visit a parse without
materializing Rust-side owned trees) and serde-style extraction. This spike
supplies the measured shape + gates for that decision, not the decision.

## Identified, not landed

- **Interned / `Arc<str>` labels + token type names (escalate, §5):** still
  the big *default-path* lever. After S1–S3 the owned path's remaining
  parse-phase blocks are ~56 K terminal-name clones + ~28 K label
  `to_string()`s + ~56 K owned values + ~26 K child lists (146 KB workload).
  A ceiling simulation (labels/type-names replaced with empty strings) on
  this box: **~1.20× further** on `parse()` (14.7 → 12.2 ms) — and it lifts
  the NullBuilder floor too (the name clone happens in the lexer). Requires
  changing the public `Token.type_`/`Tree.data` `String` fields —
  representation change behind accessors, the ADR-0040 route. Escalated (see
  the needs-decision issue this spike files).
- **`Child::Tree(Box<Tree>)` (shopping-list #4):** measured both directions
  in an isolated worktree — see the addendum at the end of this note.
- **Owned-path token value:** the remaining ~56 K `Token.value` allocations
  are semantic (the public `Token` owns its text). Only a representation
  change (SSO / `Arc<str>` / span) moves them — same escalation as labels.
- **Standalone runtime mirror:** `standalone/runtime.rs` keeps its own
  (deliberately separate, ADR-0008) copy of `shape_reduction`; S1/S3 were not
  mirrored there in this spike. Generated parsers still parse identically —
  they just don't get the speedup until a follow-up mirrors the change.
- **Earley:** untouched, by design — its cost is the chart/forest; needs its
  own profile-first spike arm (unchanged advice from 2026-07-01).
- **Tape byte volume:** the tape's array-doubling growth makes its total
  *bytes* slightly exceed the owned path's on big parses (blocks are ~100×
  fewer); a size-hint reservation from the input length would trim it if it
  ever matters.

## Correctness

Full suite green (705/705) after every kept experiment, plus the
counter/scaling gates (`perf-counters`), the span-tree and tape-tree
projection + counter gates, and the L0 scanner differential
(`fancy-oracle`, 0 divergences). No oracle regenerated, no XFAIL touched, no
grammar changed. The `test_child_vec_scaling` gate was updated in the same
change that made it obsolete — from pinning the ratio-1 state to pinning the
reuse closed form (fresh buffers == 1 on the list grammar), exactly the
transition its own documentation reserved for the #242/#243 work.

## Addendum: X3 (`Child::Tree(Box<Tree>)`) measurement

(filled in from the isolated-worktree experiment; see PR discussion)
