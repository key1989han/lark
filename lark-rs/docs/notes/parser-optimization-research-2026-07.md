# Parser Optimization Research — Implementation Audit + Literature Study

*Compiled 2026-07-03. Part 1 is a code audit of lark-rs as it stands; Part 2 is a
web literature study (deep-research harness: 6 angles → 26 sources fetched → 117
claims extracted → 25 adversarially verified, 24 confirmed / 1 refuted). Every
Part-2 claim below carries its verification vote and primary source.*

---

## TL;DR

The literature offers **almost no asymptotic headroom** over what lark-rs already
ships. LALR, Joop-Leo Earley, and CYK are each at or near the relevant theoretical
bound, and no general-CFG algorithm (Marpa, GLL/FUN-GLL, parsing-with-derivatives)
beats the cubic worst case the binarized-SPPF Earley already achieves. That
*confirms the profiling verdict*: the remaining wins are **constant factors —
lexer throughput and memory/allocation layout — not algorithms.**

The concrete levers, ranked by fit to the measured bottleneck:

1. **Lexer: directly-executable / baked DFA** (RE/flex `--full`/`--fast` model) —
   the most on-target lever for the ~55% of instructions in the lexer, and it fits
   the existing table-interpreted `regex-automata` DFA + standalone-codegen story.
2. **Lexer: SIMD / data-parallel classification** (simdjson, SIMD sub-lexer,
   SIMD-DFA) — the largest *demonstrated* multipliers (3–12×), but a hand-vectorized
   rewrite, not a tune of the current DFA.
3. **Memory layout** (arena/flat trees, red-green, SoA) — the measured #1 bottleneck
   (~3 allocs/byte), and already lark-rs's own deferred roadmap (SpanTree/TapeTree/
   arena). Covered in depth by the **second pass, Part 3** below (label interning,
   flat `kids[]`, arena nodes, `u32` positions; red-green trees are a deliberate
   *non*-recommendation for a batch parser).
4. **LR table encoding** (comb-vector/row-displacement) — real but modest; table
   lookup is not the profiled bottleneck.
5. **Incremental & data-parallel parsing** — longer-horizon, workload-restricted
   (edit-driven / operator-precedence grammars only).

---

## Part 1 — What lark-rs already implements

### Three engines, one EBNF grammar

| Engine | Where | Core + optimizations already in place |
|---|---|---|
| **LALR(1)** | `parsers/lalr.rs` | *True* LALR(1) (spontaneous-generation + propagation lookaheads, not SLR FOLLOW). **Sparse** parse table: per-state `(terminal-id, action)` rows sorted ascending, linear-scanned (`action_at`/`goto_at`), O(filled) not O(states×terminals) — no hashing on the hot path (#367). |
| **Earley** | `parsers/earley/` | Binarized **SPPF** (Elizabeth Scott), arena-allocated by `NodeId`. **Joop-Leo** deterministic-reduction-path optimization with **lazy, reachability-bounded** spine reconstruction (`load_leo_paths`) → right recursion O(n²)→O(n). Per-column **`waiting` index** → completer O(matches) not O(column). Dynamic lexer for the general path. |
| **CYK** | `parsers/cyk.rs` | CNF conversion (TERM/BIN/UNIT + ε-removal) + O(n³·\|grammar\|) DP. Niche/last-resort backend. |

### Lexer

- `BasicLexer` + **`ContextualLexer`** (parser state narrows candidate terminals — Lark's primary USP).
- Two combined-scanner backends behind a `ScannerBackend` seam: the `regex`-crate alternation `Scanner`, and the default **`regex-automata` multi-pattern DFA** (`dfa.rs`) — returns a bare `PatternID` (no capture tracking), **anchored at `pos`** (never forward-scans), with a literal start-byte prefilter.
- **Bounded lookaround is *lowered* into the DFA** (`lookaround/`) — no runtime backtracking engine.
- Per-state scanners **deduped by terminal-set** and built **lazily** (`OnceCell`) — fixed a build cost up to 12× Python Lark's.

### Optimizations already landed

- **Interning** to `Copy` integer `SymbolId`; semantics are dense **flag arrays**, never name-prefix sniffing (engine never inspects a name).
- **Hot-path pass** (2026-07-01): removed per-token `String` clones (token was materialized 3×), four per-token **SipHash** probes → dense arrays, per-reduction `drain().collect()` and child-buffer reallocs. **~1.68× geomean** on synthetic LALR.
- **Lexer capture pass** (2026-06-04): capture-group resolved by **index at build** (was by name → ~2.5M `hash_one`/parse), reused `CaptureLocations` scratch. ~17–20%.
- **`\G` anchoring** fixed an O(n²) forward-scan pathology (124 KB Python parse 177 s → 0.24 s).
- **Experimental zero-copy output backends:** `SpanTree` (token values borrow input `&str`, labels borrow grammar, no `Tree` node → ~41% fewer allocs, ~1.3×); `TapeTree` (flat `nodes`/`kids` arrays → O(log n) allocator calls).

### The measured ceiling

- LALR ~**4–5×** over Python Lark (synthetic), **5–13×** wild bank; Earley ~**13–43×**; CYK ~**27×**. Project goal is **10–100×** — LALR has headroom.
- **Allocation-bound, not algorithm-bound** (callgrind + DHAT, 92 KB JSON): ~**301K allocations / 105 MB churn** per parse (~3 allocs/byte). ~**55%** of instructions in the **lexer** (the `regex` engine itself + memcpy/malloc), ~**32%** in **reduce/tree-building** (`String` labels, owned `Token` strings, per-node child `Vec`s), ~10% SipHash.
- Already-planned deferred work: arena/`Tape` output (`Box<str>`/interned labels, zero-copy spans), `u32` positions to shrink the value-stack element (ADR-0040), child-`Vec` pooling (#583), streaming fix for the still-O(n²) Earley `ambiguity='explicit'` walk.

---

## Part 2 — Literature study (verified findings)

### A. LR table representation (RQ1)

**A1 — Comb-vector / row-displacement compression → matrix-access-cost lookup.**
*(verified 3-0, high)* US Patent 5,105,353 (IBM, 1992) computes a terminal action in
"three indexing operations, one addition and one comparison … fairly close to the
cost of a matrix element access," via the classic three-technique scheme (merge
comparable states, overlay sparse rows into one vector, encode state numbers as
indices) — the Tarjan-Yao double-displacement / Dragon-book comb-vector method.
**Fit to lark-rs:** lark-rs's sparse per-state table trades size for a per-lookup
linear scan; comb-vector gives O(1)-small-constant lookup — but it's a
constant-factor table change, and table lookup is *not* the profiled bottleneck.
Modest gain.
Source: <https://patents.google.com/patent/US5105353A/en>

> **Surfaced but not adversarially verified (budget-dropped before the verify pass;
> treat as suggestive, not confirmed):** Pfahler, *Optimizing Directly Executable LR
> Parsers* (CC 1990, LNCS 477) reports directly-executable/hard-coded LR parsers up
> to **~7×** a table-interpreting parser; a recursive-ascent benchmark
> (modulovalue) measured **generated vs table-interpreted recursive-ascent at ~1.75×**
> (30 vs 17 MB/s). These point the same direction as A1/B1 — *compile the automaton,
> don't interpret a table* — and dovetail with lark-rs's existing standalone/
> `include_lark!` codegen, but the specific speedup numbers were not fact-checked in
> this run.

### B. Lexer throughput (RQ2) — the highest-leverage cluster

**B1 — Directly-executable / baked DFA lexers (RE/flex `--full` / `--fast`).**
*(verified 3-0, high)* RE/flex emits non-backtracking DFA matchers (same class as
lark-rs's `regex-automata` DFA) in two baked forms: `--full` bakes the FSM as a
static opcode table so "FSM construction overhead is eliminated … starts scanning
immediately," and `--fast` emits "optimized native C++ code" (goto/switch) trading
code size for speed. **Fit: the single most on-target lever** — lark-rs's DFA is
*table-interpreted*, both modes could be generated at grammar-compile time, and it
aligns with the existing standalone-codegen path.
Source: <https://www.genivia.com/doc/reflex/html/>

**B2 — SIMD structural classification (simdjson).** *(verified 3-0, high)* Langdale &
Lemire, *Parsing Gigabytes of JSON per Second* (arXiv:1902.08318, VLDB J. 2019):
2–3 GB/s single-core, "a quarter or fewer instructions than … RapidJSON," savings
originating in the SIMD stage-1 structural indexing — exactly the tokenization stage
that is ~55% of lark-rs's budget. **Fit: strong in principle, but requires
hand-written vectorized classification (not a table DFA), and simdjson is
JSON-specific.**
Source: <https://arxiv.org/abs/1902.08318>

**B3 — SIMD sub-lexer for a general language (C).** *(verified 3-0, high — but source
is a BSc thesis)* Bolfa (TU Delft, 2024) splits C17 into nine SIMD-friendly lexical
groups, each an independent sub-lexer over 32-byte AVX2 vectors merged by priority,
reporting **~12×** over GCC 13.1's lexer on Zen 3. **Caveats:** student research, one
machine, GCC's lexer also does preprocessing (apples-to-oranges); a comparable public
project (simd-lexing) reports only **3–5×** vs flex — treat 12× as an upper anecdote,
3–5× as the conservative expectation. Shows a *grammar-splitting SIMD* strategy that
generalizes beyond JSON.
Source: <https://repository.tudelft.nl/file/File_0dd21f86-ee80-49e2-b4d1-bdb56f84f265>

**B4 — Data-parallel DFA membership (speculative).** *(verified 3-0, high)* Ko, Jung,
Han & Burgstaller (arXiv:1210.5093, IJPP 2013): partition input → match chunks in
parallel by speculating the boundary start state → combine, provably failure-free
(sequential semantics preserved), and "fully vectorized" on SIMD-gather hardware.
**Caveats:** speculating over all start states costs up to |Q|× redundant work unless
the DFA is state-convergent; full-vectorization measured on an Intel SDE *simulator*.
An intra-core throughput lever *and* a path to multi-core lexing.
Source: <https://arxiv.org/abs/1210.5093>

**B5 — SIMD-DFA transition engines beat scalar tables (Hyperflex).** *(verified 3-0,
high)* Hyperflex (arXiv:2512.07123, IEEE 2025; authors incl. simdjson/Hyperscan's
Langdale): **8.89 Gbit/s, up to 2.27×** over Hyperscan's default (Mcclellan) DFA, now
deployed in Hyperscan. **Caveat:** naive SIMD-shuffle transitions cap at **<64 states**
(a 512-bit AVX-512 vector holds 64 8-bit entries), so a hybrid "hyper region" design
is needed — and multi-pattern *lexer* DFAs often exceed 64 states. Shows SIMD
transitions beat scalar tables, but the win needs careful engineering.
Source: <https://arxiv.org/pdf/2512.07123>

### C. Earley & general-CFG (RQ3, RQ4)

**C1 — Nothing beats binarized-SPPF Earley asymptotically; Marpa's linearity *is*
Joop-Leo.** *(C-claim 3-0; the "linear" sub-claim 2-1, high)* Kegler, *Marpa*
(arXiv:1910.08129): first to unite Leo 1991 (O(n) right recursion) with
Aycock-Horspool 2002 (nullable-rule fix); Theorem 12.10 gives O(n) time/space for
every **LR-regular** grammar. **Key caveat (why the linearity vote split 2-1):**
LR-regular linearity is *exactly* the Joop-Leo property lark-rs already ships, and on
the *ambiguous/nondeterministic* grammars that actually motivate reaching for Earley,
Marpa is O(n²)/O(n³), not linear. The linearity derives from Leo 1991 (peer-reviewed
TCS); the Marpa paper itself is self-published arXiv. **The one idea worth checking:**
whether lark-rs's Leo Earley already handles nullable/empty rules via the
Aycock-Horspool split-ε construction (see §Open Questions).
Source: <https://arxiv.org/pdf/1910.08129>

**C2 — Alternative families (GLL/FUN-GLL, parsing-with-derivatives) are the same
cubic class.** *(verified 3-0, high)* Parsing-with-derivatives is **O(n³), not
exponential** (Adams, Hollenbeck, Might, PLDI 2016). FUN-GLL is "worst-case O(n³) in
both space and runtime" (van Binsbergen/Scott/Johnstone, 2020) — identical to
binarized-SPPF Earley, which GLL also uses for its forest. GLL sequentialises
recursive descent to admit any CFG incl. left recursion (compiled, table-free
parsers); restructured variants FGLL/RGLL give "significant speedup" only *relative to
base GLL*, not to Earley/LALR. **Implication: switching the general path buys
constant-factor engineering (and a table-free compiled style), not a better bound.**
Sources: <https://dl.acm.org/doi/10.1145/2908080.2908128> ·
<https://www.sciencedirect.com/science/article/abs/pii/S2590118420300058> ·
<https://www.researchgate.net/publication/301759495_Structuring_the_GLL_parsing_algorithm_for_performance>

*Refuted (1-2):* the claim that FUN-GLL's BSR representation "maintains or constructs
**no** graph" did not survive verification — flagged as overstated.

### D. Incremental & parallel parsing (RQ6)

**D1 — Incremental LR (Wagner-Graham).** *(verified 3-0, high)* *Efficient and
Flexible Incremental Parsing* (ACM TOPLAS 1998): re-parse in **O(t + s·lg N)** (t new
terminals, s edit sites, N-node tree), independent of edit location, via balancing of
long sequences ("the central requirement … ignored in all previous approaches") —
the algorithm tree-sitter is built on. **Fit: orthogonal to raw throughput** —
relevant only for re-parse-on-edit (IDE/LSP), a substantial engine addition, not a
drop-in constant-factor win. The contextual-lexer LALR could in principle adopt it.
Source: <https://harmonia.cs.berkeley.edu/papers/twagner-parsing.pdf>

**D2 — Non-speculative data-parallel parsing (operator-precedence).** *(verified 3-0,
high)* Barenghi et al., *Parallel parsing made practical* (SCP 2015) + PAPAGENO: OPG
"local parsability" lets chunks be parsed independently and recombined with no
boundary guessing; on 16 cores, parallel lexing of 75 MiB JSON hit **7×** vs
sequential Flex and parsing up to **5.3×** vs sequential PAPAGENO. **Sharp caveat:**
operator-precedence languages are strictly weaker than LALR/general-CFG — applies only
to grammars expressible in OPG form, and the win is multi-core scaling, not
single-core efficiency.
Source: <https://www.sciencedirect.com/science/article/pii/S0167642315002610>

---

## Gaps in this research pass

- **RQ5 (memory layout) produced no *verified* claims** despite being THE measured
  bottleneck (~32% of instructions in tree-building, ~3 allocs/byte). The search
  surfaced strong on-topic sources — Adrian Sampson's *Flattening ASTs* (arena + u32
  handles = exactly the TapeTree direction), Lippert's red-green trees, and
  rust-analyzer's `cstree` (interned red-green nodes) — but none reached the verified
  top-set (the verify budget spent on the LR/lexer/Earley threads). **This is the
  most important next literature pass**, and it maps directly onto lark-rs's own
  deferred arena/Tape/`Box<str>` roadmap.
- No source directly compares **baked-DFA (B1)** vs **hand-vectorized SIMD (B2–B5)**
  throughput-per-engineering-effort on *multi-pattern lexer DFAs with >64 states* —
  the exact decision lark-rs faces.

## Open questions worth escalating

1. Does lark-rs's Joop-Leo Earley already handle nullable/empty rules via
   Aycock-Horspool 2002, or is that the one Marpa idea beyond Leo still worth
   adopting? *(This is answerable from `parsers/earley/` directly — see the nullable
   handling notes in `earley/recognizer.rs`.)*
2. For the lexer, which yields more throughput-per-effort on real Lark grammars: a
   directly-executable/baked DFA (RE/flex `--fast` style, generated from the existing
   `regex-automata` DFA) or a hand-vectorized SIMD classifier (simdjson/sub-lexer
   style)? No source settles it for >64-state multi-pattern DFAs.
3. Are any real Lark grammars (or the JSON corpus) expressible in operator-precedence
   form to unlock the PAPAGENO data-parallel path, or is OPG too restrictive for
   Lark's target space?

## How this maps to lark-rs's existing roadmap

| Lever | Verified evidence | Already tracked in lark-rs? |
|---|---|---|
| Arena / flat / interned-label trees | (RQ5 — no verified web claim, but strong sources) | **Yes** — SpanTree (C8), TapeTree (#243), arena (#244) |
| Baked / directly-executable DFA lexer | B1 (RE/flex) | Partially — standalone codegen exists (regex-based); a *baked DFA* is new |
| SIMD / data-parallel lexer | B2–B5 | No — new engineering model (hand-vectorized) |
| Comb-vector LR table | A1 | No — current table is sparse+linear-scan; not the bottleneck |
| `u32` positions / smaller stack element | (profiling) | **Yes** — ADR-0040 |
| Incremental parsing | D1 | No — orthogonal (IDE/LSP workload) |
| Operator-precedence data-parallel | D2 | No — workload-restricted |

**Bottom line for prioritization:** the evidence says spend effort where lark-rs's own
profiler already points — **memory/allocation layout (the arena/Tape work) and lexer
constant factors (a baked DFA first, SIMD later)** — because the algorithmic axis is
exhausted. The single highest-confidence, best-fit *new* idea from the literature is
**B1 (baked/directly-executable DFA lexer)**: it targets the largest instruction share,
matches the existing table-interpreted DFA + codegen architecture, and needs no
vectorization rewrite.

---

## Part 3 — Second pass: memory-layout / allocation techniques (RQ5, focused)

*A follow-up deep-research pass pinned entirely to the allocation axis the first pass
under-covered. 5 angles → 21 sources fetched → 95 claims → 25 verified, 24 confirmed /
1 refuted. This is the axis lark-rs's own profiler names as #1 (~3 allocs/byte, ~32% of
instructions in reduce/tree-building), so it directly informs the deferred arena/Tape
roadmap.*

### The three composable, orthogonal wins

The literature converges on three techniques that attack the three measured costs —
owned `String` labels, owned `Token` value `String`s, and the per-node child `Vec`.

**M1 — Flatten the tree into contiguous arrays + `u32` handles (the strongest
evidence).** *(verified 3-0, high)* Adrian Sampson (Cornell), *Flattening ASTs*: pack
nodes into one `Vec<Expr>`, reference children by `u32` index (`ExprRef`) not heap
pointer. Measured **2.4× speedup** (3.1 s → 1.3 s, 100M nodes); **~38% of the pointer
version's runtime was pure deallocation** eliminated by freeing the whole pool once;
**even excluding deallocation the flat version was 1.5× faster** (cache/locality).
**This is exactly lark-rs's `TapeTree` model** — the best-supported direction for a
*batch* parser. Caveat: the workload is a tree-walking interpreter over a synthetic
AST, not a parser building from real input, so it transfers by analogy (both are
allocation/locality-dominated), not like-for-like.
Source: <https://www.cs.cornell.edu/~asampson/blog/flattening.html>

**M2 — Intern the small fixed set of LABELS to `Copy` `u32`.** *(verified 3-0, high)*
rustc's `Symbol` and the `lasso` crate: a `DroplessArena` for the bytes + a content→index
hashtable + a `Vec` for reverse lookup; a novel string is copied once (Vacant), a repeat
returns the existing index (Occupied). All operations (equality, hashing, ordering) become
integer ops. **Fit: near-free for lark-rs** — the rule/terminal label set is tiny and
fixed (SpanTree already borrows them as `&str`); interning them to `u32` shrinks the node
label to 4 bytes and dedups by construction. Interning *token values* helps only when the
input repeats tokens heavily (JSON keys) and competes with span-borrowing (see M3).
Sources: <https://github.com/rust-lang/rust/blob/main/compiler/rustc_span/src/symbol.rs> ·
<https://github.com/Kixiron/lasso>

**M3 — Bump/arena-allocate node storage.** *(verified 3-0, high)* Oxc reported **~20%
overall** from moving its AST to a `bumpalo` arena (linear memory access + fast whole-arena
drop; profiling had shown sequential per-node `Box`/`Vec` drops). `bumpalo`: allocation is a
capacity check + pointer bump; **whole-arena free is O(1) but per-node `Drop` is not run**
(opt-in `bumpalo::boxed::Box` runs `Drop`). **Fit: the "arena-allocated nodes" deferred
item** — acceptable for lark-rs's non-`Drop`-heavy nodes, at a lifetime/borrow-plumbing
cost. Note: arena-of-nodes *still leaves the per-node child `Vec`* unless combined with a
flat `kids[]` arena (M5). Caveat: the 20% is a self-reported single-workload (JS compiler)
figure with no reproducible harness.
Sources: <https://oxc.rs/docs/learn/performance> · <https://github.com/fitzgen/bumpalo>

### Supporting findings

**M4 — `usize` → `u32` positions.** *(verified 3-0, high)* Oxc: **up to 5%** on large
files from shrinking the hot per-node `Span` 8→4 bytes ("larger than `u32` is a 4GB
file"). **Directly validates lark-rs's planned `u32` value-stack positions (ADR-0040).**
Source: <https://oxc.rs/docs/learn/performance>

**M5 — Child-list: a shared flat `kids[]` index arena beats a per-node `Vec`.**
*(verified, medium — directional, no isolated benchmark)* No surveyed source isolates the
per-node child-`Vec` cost, but the flattening (M1) and tape (M6) models all replace
per-node child pointers/`Vec`s with an **index range into one contiguous array** — exactly
`TapeTree`'s `kids[]`. `SmallVec`/inline-small-child and cross-node `Vec` pooling are named
in lark-rs's own deferred list but have no external tree-builder benchmark here. **This is
the biggest quantification gap → wants a lark-rs microbenchmark** (see Open Questions).
Sources: (derived from) <https://www.cs.cornell.edu/~asampson/blog/flattening.html> ·
<https://simdjson.org/api/0.4.0/md_doc_tape.html>

**M6 — Token VALUES: span-borrow (best) or SSO fallback.** *(verified 3-0, high)*
`servo/tendril` stores strings ≤8 bytes **inline, no heap allocation**, and is 16 bytes vs
`String`'s 24 on 64-bit. **Fit: SpanTree's span-borrowing already dominates** for a batch
parser that owns the input; SSO/`Box<str>` is the fallback for tokens that *must* be owned
(normalized/transformed values) and shrinks the `Token` element. (Single-source figures;
the general SSO principle is well established.)
Source: <https://github.com/servo/tendril>

**M7 — The extreme endpoint: simdjson tape + On-Demand.** *(verified 3-0, high)* The DOM
**tape** is one flat array of 64-bit values in document order (8-bit type + 56-bit payload);
arrays/objects store a **skip pointer** for O(1) subtree skipping. The **On-Demand** parser
(peer-reviewed, Keiser & Lemire, SPE 2024) **builds no tree at all** — materializes only
touched values. **Fit: validates the `TapeTree` direction**, and the 64-bit-packed
skip-pointer layout is a concrete *upgrade path* for `TapeTree`'s `nodes[]`/`kids[]`. On-Demand
itself is forward-only/single-pass and not transplantable to a general tree-producing parser
— the transferable idea is *eliding untouched-node materialization*, not the full model.
Sources: <https://simdjson.org/api/0.4.0/md_doc_tape.html> ·
<https://onlinelibrary.wiley.com/doi/10.1002/spe.3313>

### Red-green trees: a deliberate *non*-recommendation

**M8 — Roslyn/rowan red-green trees are for *incremental editing*, not batch parsing.**
*(verified 3-0, high; one sub-claim 2-1)* The green tree is immutable, parent-pointer-free,
width-not-position (enables structural sharing + **O(log n) incremental re-parse**); the red
tree is a lazy facade discarded on every edit. **But those are editing properties.** For a
batch parser, structural sharing is dead weight paid at construction time: rust-analyzer's
own maintainer measured a non-sharing contiguous representation (`syntree`) building trees
**~2× faster than rowan** and plans to move toward a more contiguous layout for faster
workspace load. **Conclusion: for lark-rs's batch use case, a flat tape (TapeTree) is
preferable to interned green nodes.** (The 2× is an informal, self-flagged-unfair maintainer
benchmark — directional.) Red-green becomes relevant only if lark-rs ever pursues the
incremental-parsing lever from Part 2 §D1.
Sources: <https://ericlippert.com/2012/06/08/red-green-trees/> ·
<https://github.com/rust-lang/rust-analyzer/issues/17491>

*Refuted (1-2):* the claim that simdjson DOM and On-Demand both first build a shared
32-bit-per-structural-char index usable alone for navigation — excluded as overstated.

### What this means for lark-rs (memory axis)

The three prototypes lark-rs already has are the *right* structures; the literature says
**push them further and combine them**, in this order of confidence/leverage:

1. **Label interning to `u32` (M2)** — near-free, low-risk, shrinks every node label,
   composes with everything. Do first.
2. **Flat `kids[]` child arena (M1+M5)** — kills the per-node child `Vec`, the one
   allocation SpanTree still pays. This is the `TapeTree` direction; the evidence for
   flattening is the strongest in the whole pass.
3. **Arena node storage + `u32` positions (M3+M4)** — the deferred arena work and ADR-0040,
   each with modest but real single-digit-to-20% external validation.
4. **SSO/`Box<str>` (M6)** only for tokens that must be owned; otherwise span-borrow.

**Sharpest remaining unknown (M5):** no external source quantifies the per-node child-`Vec`
cost in isolation. lark-rs is uniquely positioned to answer this itself — it already has
`SpanTree` (still one child `Vec`/node) and `TapeTree` (flat `kids[]`) as A/B prototypes, so
a `perf-counters` microbenchmark diffing the two *is* the missing measurement. That
experiment would ground the entire child-list decision better than any paper.

### Open questions (memory axis)

1. **Isolated child-`Vec` cost — ANSWERED (2026-07-03, `examples/child_vec_alloc.rs`).**
   Benchmarking SpanTree (per-node `Vec`) vs TapeTree (flat `kids[]`) with a counting
   allocator — the two paths differ *only* in child-list representation, both zero-copy
   (`tree_nodes_built == 0`) — isolates the cost to **exactly 1.000 allocation per
   internal node**, flat across a 43× size sweep (0.997→1.000→1.000). That single
   per-node `Vec` is **~96% of `parse_span()`'s remaining allocations** (0.165 of 0.172
   allocs/byte); the flat `kids[]` arena (`parse_tape`) removes it, landing at **0.007
   allocs/byte** for a ~1.76× wall-clock trend. So the per-node child `Vec` — not token
   strings or labels — is the dominant residual span-path allocation, and the flat
   child arena (M1/M5, the `Tape` direction #243) is the confirmed lever. A SmallVec-
   inline variant remains unmeasured. Full write-up: `BENCH.md` §"The per-node
   child-`Vec` cost, isolated".
2. **Marginal stacking:** does label-interning + span-borrowed values + flat `kids[]` get
   the default owned-`Tree` path closer to the ~3-allocs/byte target than TapeTree alone,
   and where does each technique's reduction plateau?
3. **Token-value interning vs span-borrow:** is there any real corpus where interning
   repeated token values beats zero-copy borrowing, for a parser that already owns the input?
4. **Batch analog of On-Demand:** can lark-rs elide materialization of subtrees the
   consumer's transformer never visits, or does the tree-producing contract force full
   materialization?
