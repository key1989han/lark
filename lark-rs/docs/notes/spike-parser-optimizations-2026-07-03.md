# Spike: parser-optimization hypotheses → measurements (2026-07-03)

Throwaway measurement spike turning the recommendations of
[`parser-optimization-research-2026-07.md`](parser-optimization-research-2026-07.md)
into before/after numbers. Everything here is a *directional* experiment, not
production code: the harnesses are feature-gated examples, the library hooks are
spike-only `cfg` accessors, and nothing on the default build path changed.

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

**Takeaway.** The technique works exactly as the literature says, but at
lark-rs's current profile the cheap slice worth considering is the *leaner drive
loop* (finding 2) — and even that is bounded ~4% end-to-end. Re-rank the baked
DFA behind the memory-layout levers below; revisit after the output path stops
being allocation-bound (a 4× scanner is worth 25%+ once lexing is half the parse
again).

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
| `intern` (label → `u32`) | 4.71 | **−1.000/node** | 43.9 MB | 1.25× |
| `tape` (flat `nodes[]` + shared `kids[]`) | 4.72 | **−1.000/node** | 28.7 MB | 1.44× |
| `intern+tape` | 3.72 | **−2.000/node** | 28.2 MB | **1.61×** |
| `smallvec` (inline ≤2 children) | 7.08 | **+1.37/node** | 28.7 MB | 1.29× |

The owned path pays the same **exactly 1.000 child-`Vec` allocation per internal
node** the span path did, and the flat `kids[]` arena removes it for 1.44× — and,
unlike on the span path, it also nearly **halves total allocated bytes** (the
per-node `Vec<Child>` rows are the byte bulk of an owned parse).

**SmallVec-inline children: KILLED.** Structural, not just quantitative: an
inline child list cannot contain an unboxed recursive node (`SmallVec<[SChild; 2]>`
inside the node is an infinite-size type — `Vec`'s heap pointer is what breaks
the recursion in the default `Tree`), so the node (and the fat `Token`) must be
boxed, trading the removed child-`Vec` for a per-node `Box`: net **+1.37
allocs/node vs the default**, strictly dominated by `tape` on every axis.

---

## 3. Label interning to `u32` (research M2)

**Verdict: CONFIRMED, and it is near-free.** The `intern` row above: the label
`String` is exactly **1.000 allocation per internal node** (−7.8 MB of the
594 KB parse's churn), worth 1.25× wall-clock on its own — and the interned id
is *already on the seam* (`OutputBuilder::reduce` receives the rule index;
`OutputContext` resolves names lazily), so an interned-label backend needs no
new interner, just a node type that stores the `u32`.

---

## 4. Marginal stacking (research Part 3, open question 2)

The two levers stack perfectly (−2.000 allocs/node, 1.61×), and what remains is
now sharply identified: `intern+tape`'s residual **3.72 allocs/node ≈ 2 owned
`String`s per kept token** (`Token.type_` + `Token.value`, ~1.85 kept
tokens/node on this workload) plus O(log n) arena growth. So after M2+M5 the
owned path's allocation profile is *entirely* the lexer's owned token strings —
i.e. the next lever is M6 (span-borrowed values where possible, `Box<str>`/SSO
where ownership is forced), exactly as the research ranked. Note `Token.type_`
is pure redundancy on the hot path (the token already carries `type_id`); a
value-less/interned-type token would remove one of the two by itself.

---

## Artifacts

| file | role |
|---|---|
| `examples/baked_dfa_lex.rs` | experiment 1 harness (differential + timing), JSON sweep + wild bank |
| `examples/bake_dfa_gen.rs` → `examples/baked/json_dfa.rs` | goto/switch codegen (`gen` variant) |
| `src/lexer/{dfa,mod}.rs` `baked-dfa-spike` accessors | spike-only exposure of the plain dense DFA + retype seam |
| `examples/owned_tree_layout_alloc.rs` | experiments 2–4 (owned-output layout matrix) |

All library hooks are behind the default-off `baked-dfa-spike` feature; the
default build is unchanged. The examples are re-runnable as committed; delete
the feature + examples together when the spike's conclusions are absorbed.
