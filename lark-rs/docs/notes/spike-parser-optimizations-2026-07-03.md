# Spike: parser-optimization hypotheses → measurements (2026-07-03)

**Consolidated record of three parallel spikes** (PRs #616, #617, #618 — all run
against the "Suggested benchmark tasks" of
[`parser-optimization-research-2026-07.md`](parser-optimization-research-2026-07.md)).
This branch (#617) is the canonical base; the one net-new result the siblings
contributed — #616's viable SmallVec-of-child-*refs* design — is folded into the
matrix below. Everything here is a *directional* experiment, not production code:
the harnesses are feature-gated examples, the library hooks are spike-only `cfg`
accessors, and nothing on the default build path changed.

**Method.** Deterministic signals are the headline per BENCH.md: experiment 1
gates on a token-stream differential (every variant must emit the byte-identical
`(terminal, end)` sequence before anything is timed — same automaton, same work,
so the delta is pure constant-factor interpretation cost); experiments 2–4 gate
on a counting global allocator plus a node-count equality assertion across
variants. Wall-clock is a trend; **all ratios below are same-session,
back-to-back, one shared Linux x86_64 box** (only ratios travel). Same-session
anchors used for the end-to-end math: `parse()` on the 594 KB JSON = **7.5 MB/s**,
seam scan = **95 MB/s**, full `BasicLexer::lex` = **35.5 MB/s**.

Re-run:

```bash
cargo run --release --features baked-dfa-spike --example bake_dfa_gen   # regen baked/json_dfa.rs
cargo run --release --features baked-dfa-spike --example baked_dfa_lex  # experiment 1
cargo run --release --example owned_tree_layout_alloc [records]         # experiments 2–4
```

---

## 1. Baked / directly-executable DFA vs the interpreted `regex-automata` DFA (research B1)

**Verdict: CONFIRMED at the scanner level — killed as an end-to-end lever at
today's bottleneck.**

All variants drive the *same* determinized automaton, extracted from the real
`DfaScanner` via the spike-only `baked-dfa-spike` accessors:

| variant | what it is |
|---|---|
| `seam` | `BasicLexer::match_at` — production path (`Input` per position + prefilter + `try_search_fwd`) |
| `raw` | hand-rolled `Automaton`-trait loop over the same dense DFA |
| `table` | flat baked `u32[state×256]` opcode table (RE/flex `--full`), safe indexing |
| `table-uc` | same table, unchecked indexing |
| `gen` | committed generated goto/switch Rust source (RE/flex `--fast`; JSON only) |

Scan throughput (whole-corpus tokenization loop, ratio vs `seam`; median):

| workload | seam MB/s | raw | table | table-uc | gen | scan share of full lex |
|---|---:|---:|---:|---:|---:|---:|
| json 56 KB  | 91  | 2.05× | 3.73× | 4.61× | 2.52× | 40% |
| json 594 KB | 95  | 2.01× | 3.88× | 4.61× | 2.50× | 37% |
| json 2.4 MB | 98  | 1.99× | 3.79× | 4.51× | 2.47× | 29% |
| matter_idl (444 KB, 67 patterns) | 37 | 1.25× | 1.39× | 1.39× | — | 65% |
| poetry_markers (639 B) | 107 | 2.16× | 2.88× | 3.16× | — | 50% |
| poetry_pep508 (593 B)  | 114 | 1.61× | 2.80× | 2.98× | — | 53% |
| tartiflette (32 KB GraphQL) | 85 | 1.69× | 2.63× | 2.78× | — | 46% |

The differential passed on every row (identical token streams incl. `unless`
retyping), so the ratios are clean interpretation-overhead isolations.

Sub-findings, in decreasing order of surprise:

1. **The flat opcode table beats real codegen.** The generated goto/switch
   scanner (`gen`, the RE/flex `--fast` analog) lands at ~2.5×, *well below* the
   flat table's ~3.8×. Rust has no computed goto; a two-level `match`
   (state dispatch, then byte dispatch) compiles to jump chains that lose to one
   indexed load from a 256-wide row. **For lark-rs the winning bake shape is the
   `--full` static table, not source generation** — which also removes the whole
   "emit and compile Rust per grammar" problem.
2. **Half the win needs no baking at all.** The `raw` variant — the same dense
   DFA, driven by a leaner loop (start state + no per-position `Input`
   construction, no prefilter consult, no enum dispatch) — is already ~2× the
   seam. That slice is reachable inside the existing engine without any new
   representation.
3. **The end-to-end ceiling is small.** Same-session, the scanner drive is
   ~8% of `parse()` (95 vs 7.5 MB/s), so even the 3.9× table saves only ~6%
   end-to-end. Scanning is also no longer the bulk of *lexing*: it is 29–65% of
   `BasicLexer::lex`, the rest being owned-`Token` materialization (two `String`s
   per token) — the same allocation story as experiments 2–4. The 2026-06-04
   "lexer ≈ 55% of instructions" profile predates the DFA-backend flip; the
   regex-engine share it indicted is largely gone.
4. **Bake cost is trivial; scope is not.** Flattening cost 0.1–0.3 ms against a
   1.8 ms scanner build (so a baked table is viable even one-shot), and tables
   ran 68–235 KiB per scanner. But the **contextual** lexer builds one scanner
   per distinct per-state terminal set (47–108 per wild grammar after dedup), so
   a production bake multiplies that footprint by ~50–100×; and several wild
   projects are out of naive-bake scope structurally: guarded/lookaround engines
   (cel, lark_lark, mappyfile, pylogics, vyper), context-sensitive start states
   (pyquil — look-behind-conditioned starts), hybrid overflow, `^`-anchored /
   multiline terminals (gersemi, hcl2, miniwdl fail even the basic-lexer build
   here).
5. `matter_idl`'s modest 1.39× shows the bound when per-token cost is dominated
   by something the bake doesn't touch (its case-insensitive keyword `unless`
   retype probes, shared by all variants).

**A measurement caution from the sibling spikes.** #616 and #618 benchmarked
*hand-written* JSON scanners against the DFA and reported ~7× and ~12×
scanner-only. Those numbers bundle grammar-specialization (a scanner hard-coded
to JSON's token shapes) with the bake itself, don't generalize to a mechanical
baker, and — quoted without the Amdahl dilution — overstate the lever. The
same-automaton isolation here (~4×, ~6% end-to-end) is the number to plan by.

**Takeaway.** The technique works exactly as the literature says, but at
lark-rs's current profile the cheap slice worth considering is the *leaner drive
loop* (finding 2) — and even that is bounded ~4% end-to-end. Re-rank the baked
DFA behind the memory-layout levers below; it remains a real **one-shot /
standalone** lever (baking also erases determinization from the build, and
`include_lark!` could bake the table at compile time). Revisit after the output
path stops being allocation-bound (a 4× scanner is worth 25%+ once lexing is
half the parse again).

---

## 2. Flat child arena (`kids[]`) for the DEFAULT owned tree (research M1/M5)

**Verdict: CONFIRMED.** `examples/owned_tree_layout_alloc.rs` re-runs the
child-list isolation on the *owned* output path (`child_vec_alloc` proved it for
the zero-copy span path): all variants keep fully-owned token values and `Meta`,
so deltas isolate one representation lever each. 594 KB JSON, 98 001 internal
nodes (flat across the 56 KB → 2.4 MB sweep):

| variant | allocs/node | Δ vs `parse()` | alloc bytes | wall-clock |
|---|---:|---:|---:|---:|
| `parse()` (label `String` + `Vec<Child>`/node) | 5.71 | — | 51.7 MB | 1.00× |
| `intern` (label → `u32`) | 4.71 | **−1.000/node** | 43.9 MB | 1.21× |
| `tape` (flat `nodes[]` + shared `kids[]`) | 4.72 | **−1.000/node** | 28.7 MB | 1.38× |
| `intern+tape` | 3.72 | **−2.000/node** | 28.2 MB | **1.57×** |
| `sv-nodes` (inline child *nodes*, ≤2) | 7.08 | **+1.37/node** | 28.7 MB | 1.10× |
| `sv-refs` (arena + inline child *refs*, ≤4; from #616) | 4.74 | **−0.979/node** | 42.8 MB | 1.28× |

(Wall-clock column re-measured in one back-to-back session; an earlier session
put the same ordering ~3 points higher — trend noise, the alloc counts are
identical.)

The owned path pays the same **exactly 1.000 child-`Vec` allocation per internal
node** the span path did, and the flat `kids[]` arena removes it for ~1.4× — and,
unlike on the span path, it also nearly **halves total allocated bytes** (the
per-node `Vec<Child>` rows are the byte bulk of an owned parse).

**SmallVec, two shapes with opposite verdicts (the M5 "third data point"):**

- **Inline child *nodes*: KILLED**, structurally: an inline child list cannot
  contain an unboxed recursive node (`SmallVec<[SChild; 2]>` inside the node is
  an infinite-size type — `Vec`'s heap pointer is what breaks the recursion in
  the default `Tree`), so the node (and the fat `Token`) must be boxed, trading
  the removed child-`Vec` for a per-node `Box`: net **+1.37 allocs/node vs the
  default**, dominated by `tape` on every axis.
- **Inline child *refs*: VIABLE** — the design from parallel spike **#616**
  (`tree_alloc_backends.rs`), reproduced here deterministically: node records in
  an arena, children as `Copy` `u32` handles inline in a `SmallVec<[Ref; 4]>`.
  The recursion goes through the handle, so nothing is boxed; only ~2% of nodes
  (2 001/98 001) spill past the inline capacity, capturing **0.979 of the 1.000
  child-`Vec` allocs/node**. In this session it trails the shared flat `kids[]`
  on wall-clock (1.28× vs 1.38×) and on alloc *bytes* (each record carries 4
  inline slots), so `tape` stays the recommendation — `sv-refs` is the fallback
  when a single shared child buffer is undesirable (e.g. subtree-local
  mutation).

---

## 3. Label interning to `u32` (research M2)

**Verdict: CONFIRMED, and it is near-free.** The `intern` row above: the label
`String` is exactly **1.000 allocation per internal node** (−7.8 MB of the
594 KB parse's churn), worth ~1.2–1.4× wall-clock on its own — and the interned
id is *already on the seam* (`OutputBuilder::reduce` receives the rule index;
`OutputContext` resolves names lazily), so an interned-label backend needs no
new interner, just a node type that stores the `u32`.

---

## 4. Marginal stacking (research Part 3, open question 2)

The two levers stack perfectly (−2.000 allocs/node, ~1.6×), and what remains is
now sharply identified: `intern+tape`'s residual **3.72 allocs/node ≈ 2 owned
`String`s per kept token** (`Token.type_` + `Token.value`, ~1.85 kept
tokens/node on this workload) plus O(log n) arena growth. So after M2+M5 the
owned path's allocation profile is *entirely* the lexer's owned token strings —
i.e. the next lever is M6 (span-borrowed values where possible, `Box<str>`/SSO
where ownership is forced), exactly as the research ranked. Note `Token.type_`
is pure redundancy on the hot path (the token already carries `type_id`); a
value-less/interned-type token would remove one of the two by itself.

**Triangulation.** M2 and M5 were independently reproduced by both parallel
spikes (#616, #618) with agreeing numbers — #616 measured 1.000 / 0.999
allocs/node and 1.36× / 1.69× / 1.82× (its session's ratios) for
intern / flat-kids / stacked, on an independently written harness. Three
harnesses, one answer: these two levers are real.

---

## Cross-cutting gotchas (read these before the next pass at any of this)

1. **Flat `--full` opcode table > `--fast` goto/switch codegen, in Rust.** No
   computed goto; a two-level `match` loses to one indexed load. Don't build a
   Rust source emitter for scanners — bake data, not code.
2. **SmallVec of child *nodes* is structurally impossible without boxing**
   (infinite-size recursion) **and loses; SmallVec of child *refs* into an arena
   is viable** (−0.979/node, ~2% spill) but still trails one shared flat
   `kids[]`.
3. **~Half the baked-scanner win is just a leaner drive loop** — no bake, no new
   representation: hoist the per-position `Input`/start-state/prefilter work out
   of `match_at`'s inner path (`raw` ≈ 2× the seam on the same automaton).
4. **After M2+M5 the residual is owned token strings** (→ M6 next), and
   `Token.type_` is redundant with `type_id` — one of the two per-token
   `String`s can go without any zero-copy machinery.

---

## Artifacts: the durable instrument vs the throwaway

**Durable instrument** — the parts that must survive for the baked-DFA lever to
be re-measurable when it is revisited (the numbers depend on these being
faithful, and the docs alone cannot reconstruct them):

| piece | why it's load-bearing |
|---|---|
| `bake()` (in `baked_dfa_lex.rs` / `bake_dfa_gen.rs`) | BFS flattening of the *real* dense DFA, incl. the **start-state context-sensitivity probe** (a look-behind-conditioned start state falsifies a single baked start — this is what disqualified pyquil) and the **delayed-by-one match / EOI-transition handling** that makes the baked semantics byte-identical |
| `spike_plain_dense` / `spike_retype` (`src/lexer/{dfa,mod}.rs`, behind `baked-dfa-spike`) | the only way to get the *production* automaton + the seam's `unless` retype out of the engine, so a variant measures the real thing and not a rebuilt approximation |
| `raw_match_at` (`baked_dfa_lex.rs`) | the no-bake leaner-drive-loop baseline (gotcha 3) — the first thing to prototype in-engine |
| `load_wild` + its SKIP taxonomy (`baked_dfa_lex.rs`) | maps exactly which real grammars a naive bake covers and *why* each of the others is out (guarded engine / fence / hybrid overflow / context-sensitive start / non-LALR) |
| `owned_tree_layout_alloc.rs` | the owned-path layout matrix: counting allocator + node-count equality, one builder per lever, incl. both SmallVec shapes |

**Throwaway** — `bake_dfa_gen.rs` + the generated `examples/baked/json_dfa.rs`
(the goto/switch `gen` variant) exist only to prove the *negative* in gotcha 1.
They are kept because keeping them green is free (behind the default-off
`baked-dfa-spike` feature, never built by CI or the fast gate) and deleting them
would make that negative unreproducible; if they ever cost maintenance, delete
them — the finding stands recorded here.

All library hooks are behind the default-off `baked-dfa-spike` feature; the
default build is unchanged.
