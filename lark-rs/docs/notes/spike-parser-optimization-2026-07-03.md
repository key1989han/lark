# Spike: validating the parser-optimization hypotheses (2026-07-03)

*Throwaway measurement spike. Goal: turn the ranked levers in
[`parser-optimization-research-2026-07.md`](parser-optimization-research-2026-07.md)
into real before/after numbers. Optimize for learning, not mergeable code. The
throwaway harnesses are committed alongside so every number is re-runnable; nothing
here is productized.*

All wall-clock is a **recorded trend, not a gate** (ADR-0007) — only same-session
back-to-back ratios travel. Deterministic signals (allocation counts, token/span
counts) are the headline; wall-clock is context. Dev box: `Linux x86_64`, release +
LTO.

## TL;DR — three verdicts

| # | Hypothesis (research lever) | Deterministic result | Wall-clock trend | Verdict |
|---|---|---|---|---|
| 1 | **Baked/directly-executable DFA lexer** (B1, the top unproven lever) | baked scanner **~7× faster than the `regex-automata` DFA on pure scanning**; but only **~1.4×** end-to-end through `lex()` | scan-only 883 vs 128 MB/s; full-lex 58 vs 40 MB/s | **CONFIRMED at the scanner, but diluted** — token materialization, not scanning, dominates `lex()` |
| 2 | **Flat `kids[]` child arena for the DEFAULT owned tree** (M1/M5) | removes **exactly 0.999 alloc/node**; 0.942 → 0.778 allocs/byte | **1.66–1.69×** | **CONFIRMED** — flat on a 10× sweep; a `SmallVec`-inline variant lands within a whisker (0.979/node) |
| 3 | **Label interning to `u32`** (M2) | removes **exactly 1.000 alloc/node**; 0.942 → 0.778 allocs/byte | **1.36×** | **CONFIRMED** — and stacks linearly with #2 (0.942 → 0.613, 1.82×) |

The headline for the **owned default tree**: M2 + M5 together cut allocations **35%**
(0.942 → 0.613 allocs/byte) for **~1.82× wall-clock**, *without changing the owned-
`String` public contract* — distinct from the zero-copy span/tape backends, which
get further (0.007/byte) but change the output type. The remaining 0.613 allocs/byte
is owned **token value+type Strings** (upstream, lexer-side) — the fish the existing
span/tape work already targets.

Harnesses (committed): [`examples/baked_dfa_lex.rs`](../../examples/baked_dfa_lex.rs)
(exp 1), [`examples/tree_alloc_backends.rs`](../../examples/tree_alloc_backends.rs)
(exp 2 + 3).

---

## Method (per BENCH.md)

- **Demonstrate first.** Each experiment establishes the cost with a size-parametrized
  workload + a deterministic counter (a counting global allocator, or a token/span
  count) before any "fix" is written.
- **Two gotchas the research pre-flagged, both respected.**
  1. The measured path is the DEFAULT `parse()` path: `LalrParser::run → run_into →
     shape_reduction` (ADR-0029 fork 2). Exp 2/3 drive custom backends through the
     public `parse_into` seam, which *is* that same value-parametric `shape_reduction`
     loop — **not** the `ParserStack::reduce` recovery path.
  2. **Reused-parser** (built once, parsed/lexed many) and **one-shot** (build+parse)
     are reported as separate columns wherever a build-cost lever (baking,
     determinization) is in play.
- Workload is the JSON grammar over the `parse.rs`/`lex_backends.rs` `gen_json` shape
  (all-ASCII, so byte offset == character offset). JSON is a legitimate all-plain
  representative and the one grammar cheap to hand-bake; see the exp-1 caveat.

---

## Experiment 1 — Baked / directly-executable DFA vs the `regex-automata` DFA lexer

**Hypothesis (B1).** lark-rs's lexer is a *table-interpreted* DFA (`regex-automata`):
every byte is a `dfa.next_state(state, byte)` table lookup. RE/flex `--full`/`--fast`
show a *baked* scanner — the FSM compiled directly to branch/loop code (a `match` on
the lead byte + inline consumption loops, exactly flex `--fast`'s goto/switch) —
eliminates the table indirection. Confirm or kill.

**Workload / harness.** `examples/baked_dfa_lex.rs`. A hand-written baked JSON scanner
(one `match` on the lead byte, then an inline loop per token class — no transition
table, no `PatternID`). It reproduces the grammar's token boundaries **byte-for-byte**
(asserted equal to `BasicLexer::lex`'s spans on every size), so the comparison times
identical work. Two isolations plus a build-cost column:

| isolation | what it measures |
|---|---|
| **A. scan-only** | cursor advance token-by-token, `(kind, end)` only — no `Token`, no owned value, no line/col. The pure *scanner engine* (`DfaScanner::match_at`'s table walk vs the baked `match`) — where B1 lives. |
| **B. full `lex()`** | `BasicLexer::lex` (DFA) vs a baked full lexer materializing the **same** `Token`s (owned value + positions). Shows whether a faster scanner moves the end-to-end needle. |
| **C. build cost** | reused (lex only) vs one-shot (build+lex): the DFA pays determinization the baked scanner does not. |

**Result (dev box, `json_large` ≈ 92 KB; ratios travel).**

```
A. SCAN-ONLY   dfa 128 MB/s   baked 883 MB/s   →  baked ~6.9× faster (regex-crate: 15 MB/s)
B. FULL lex()  dfa  40 MB/s   baked  58 MB/s   →  baked ~1.4× faster
C. build       dfa reused 2.25 ms | one-shot 4.98 ms (build ≈ 2.73 ms)
               baked reused 1.63 ms | one-shot 1.63 ms (build ≈ 0 — compiled code)
```

The scan-only ~7× held flat across the 390 B → 92 KB sweep (0.12–0.15× time ratio),
and lands squarely on the Pfahler *~7×* figure the research surfaced for directly-
executable vs table-interpreted automata.

**Reading.**
- **The scanner hypothesis is CONFIRMED**: a baked/directly-executable scanner is
  ~7× the `regex-automata` DFA's raw byte-scanning throughput.
- **But `lex()` is not scanner-bound.** Scan-only DFA is 128 MB/s while full `lex()`
  is 40 MB/s — so **~70% of `lex()` is token materialization** (owned value `String` +
  positions: the `memcpy`+`malloc` the 2026-06-04 profile named), not scanning. A 7×
  scanner win therefore only compresses to **~1.4× end-to-end**. The larger lexer lever
  is eliminating per-token value allocation (the span/value-less-token backends), *not*
  a faster transition engine.
- **Baking is a one-shot lever.** It removes the DFA's ~2.7 ms determinization entirely
  — material for a build-once-parse-once (standalone / `include_lark!`) workload,
  irrelevant for a reused parser, exactly the split the research predicted.

**Verdict: CONFIRMED (scanner ~7×) but diluted (~1.4× through `lex()`).** A baked DFA
is a real scanner win and a real *one-shot* build win; it is a modest end-to-end lever
until per-token value materialization is addressed first.

**Caveats (honest scope).** (1) JSON is favorable to baking — few token classes, simple
structure; a lookaround-heavy grammar (python.lark's guarded terminals) would bake less
cleanly and is untested here. (2) This is a hand-baked scanner for one grammar, not a
general emitter, so it isolates the *ceiling* the technique offers, not what an
automatic baker would emit. (3) The wild-bank corpus generalization (baking a wild
grammar's scanner) was **not** run — the JSON head-to-head is the hypothesis test; a
wild baker is future work.

---

## Experiment 2 + 3 — Flat child arena (M5) and label interning (M2) on the owned tree

**Hypotheses.** M2: intern the fixed rule/terminal label set to a `Copy u32` — near-free,
shrinks every node label. M5: replace the per-node child `Vec` with a shared flat
`kids[]` arena — the `TapeTree` direction. `child_vec_alloc.rs` already isolated M5 on
the *zero-copy* path (1.000 alloc/node). The open question (research open-Q 2, "marginal
stacking"): on the **owned** default tree — owned `String` labels **and** owned token
values, the representation real `parse()` callers get — how much does each remove, and
do they stack?

**Workload / harness.** `examples/tree_alloc_backends.rs`. Five custom `OutputBuilder`s
driven through `parse_into` (the DEFAULT `shape_reduction` path), each measured with a
counting global allocator over one 594 KB / 98 001-node parse. Token values stay owned
`String` in **every** backend (lexer-allocated upstream, identical for all), so every
inter-backend delta is a clean isolation of the label / child-list change. Each backend
rebuilds a structurally-complete tree (node count asserted == `parse()`), so nothing is
measured by skipping work.

| backend | node label | child list | allocs/byte | isolates |
|---|---|---|---:|---|
| `parse()` [default] | `String` | `Vec`/node | **0.942** | the real default (reference) |
| reimpl-owned | `String` | `Vec`/node | 0.942 | harness sanity (matches `parse()` to 1 alloc) |
| **intern-label** | **`u32`** | `Vec`/node | **0.778** | M2 — the label `String` |
| **flat-kids** | `String` | **shared `kids[]`** | **0.778** | M5 — the per-node `Vec` |
| **flat-kids+intern** | **`u32`** | **shared `kids[]`** | **0.613** | both stacked |
| smallvec-kids | `String` | **inline `SmallVec<[Ref;4]>`** | 0.781 | M5 third point (inline child refs) |

**Deterministic result (per internal node, vs the owned reference).**

```
M2   label String   (owned − intern-label) :  1.000 alloc/node   (0.942 → 0.778 allocs/byte)
M5   child Vec       (owned − flat-kids)    :  0.999 alloc/node   (0.942 → 0.778 allocs/byte)
M2+M5 stacked        (owned − flat+intern)  :  1.999 alloc/node   (0.942 → 0.613 allocs/byte)
M5'  inline SmallVec (owned − smallvec)     :  0.979 alloc/node   (2001/98001 nodes spilled)
residual after M2+M5 = 0.613 allocs/byte    =  owned token value+type Strings (upstream)
```

Per-node savings are **flat across a 10× size sweep** (records 200 → 2000: M2
0.999 → 1.000, M5 0.992 → 0.999) — the "one alloc per node, independent of size"
the flattening literature (Sampson) predicts.

**Wall-clock trend (60 iters, 594 KB).**

```
parse() [default]  8.70 MB/s   1.00×
intern-label      11.86 MB/s   1.36×
flat-kids         14.69 MB/s   1.69×
flat-kids+intern  15.88 MB/s   1.82×
smallvec-kids     13.80 MB/s   1.59×
```

**Reading.**
- **M2 (label interning): CONFIRMED, exactly 1.000 alloc/node.** Near-free
  (the intern map allocates only once per *distinct* label — a tiny fixed set) and
  gives 1.36× on its own. This is the cheapest of the three levers.
- **M5 (flat child arena): CONFIRMED on the owned path, 0.999 alloc/node** — the
  owned-tree complement to `child_vec_alloc`'s zero-copy 1.000/node. 1.66–1.69× on its
  own. A `SmallVec`-inline child-ref variant (the research's missing third data point)
  captures **0.979/node** — nearly the full win without a shared buffer, spilling on
  only ~2% of nodes; a fine option when a single contiguous arena is undesirable.
- **They stack linearly.** M2 + M5 = 1.999 alloc/node, 0.942 → **0.613 allocs/byte**
  (−35%), **1.82×** — answering the research's open stacking question: the two are
  orthogonal, no plateau between them.
- **The residual is token strings.** After M2 + M5, 0.613 allocs/byte remains, all of
  it owned token value + type `String`s the lexer allocates upstream on the owned path.
  This is the *bigger* fish than label + child-`Vec` combined (0.329/byte), and it is
  exactly what the existing zero-copy span/tape backends remove (`parse_tape` reaches
  0.007/byte). So the sequencing implication for the owned tree: **M2 + M5 are the two
  levers that improve the default tree while keeping its owned-`String` semantics;
  token-value zero-copy is the separate, larger lever that changes the output type.**

**Verdict (both): CONFIRMED.** M2 and M5 each remove ~1 alloc/node exactly, stack to
−35% allocations / 1.82× on the owned default tree, and preserve the public owned-tree
contract. M2 is the cheapest; M5 has a viable inline-`SmallVec` variant.

---

## What this changes about the roadmap

- **Lexer.** The baked-DFA lever is real (~7× scanner) but the *first* lexer win is
  killing per-token value allocation, not the transition engine — ~70% of `lex()` is
  materialization. Baking is best cashed in on the **standalone / one-shot** path where
  it also erases determinization.
- **Owned tree.** M2 (label `u32`) + M5 (flat/`SmallVec` children) are a concrete,
  contract-preserving −35%/1.82× for the default `parse()` output, independent of the
  zero-copy span/tape track. M2 is the cheapest first step.
- **Nothing here is productized** — these are throwaway isolations. A real change would
  need the usual DoD (oracle-green, a deterministic gate, an ADR for any public-surface
  or default-representation move).
