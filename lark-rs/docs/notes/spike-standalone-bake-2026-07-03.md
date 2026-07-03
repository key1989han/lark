# Spike: standalone-bake (#620) — L5 phases → measurements on the standalone surface (2026-07-03)

**Measurement spike for epic #620** (bake the DFA lexer + optional tape output
into standalone / `include_lark!` parsers). The 2026-07-03 in-process spike
(PR #617, [`spike-parser-optimizations-2026-07-03.md`](spike-parser-optimizations-2026-07-03.md))
proved the levers at the *in-process scanner* level; this spike proves them where
#620 says they pay: a **generated parser's** one-shot build+parse economics and
binary footprint. Everything here is throwaway: spliced copies of real
`generate_standalone` output (never `tests/standalone/`), feature-gated examples,
no production or public-API change.

**Method.** Per BENCH.md, deterministic signals are the headline and gate before
any timing: every variant's **token stream** (`type_id`, value, line/column, char
spans) is asserted byte-identical to the stock generated parser's over the full
workload, and every variant's **parse trees** are asserted byte-identical
(rendered form) to the in-process basic-lexer LALR oracle's. Allocation counts
are from a counting global allocator; table/rodata sizes are byte counts;
compile/binary numbers are from real consumer crates. Wall-clock is a trend —
**all ratios are same-session, back-to-back, one Linux x86_64 box**. Same-session
anchors: in-process reused `parse()` = 8.5 MB/s (json 594 KB), 10.0 MB/s
(matter_idl), 17.8 MB/s (poetry_markers).

Re-run:

```bash
cargo run --release --features baked-dfa-spike --example standalone_bake_gen   # regen copies + L5c sweep
cargo run --release --features baked-dfa-spike --example standalone_bake_bench # gates + measurements
examples/standalone_spike/measure_size.sh                                      # compile time + binary/rodata
```

**Workloads.** json (the canonical bench grammar; synthetic 594 KB + a size
sweep) plus every wild-bank project that qualifies. Only **3 of 14** wild
projects survive the probe — see "scope findings" below; the qualifying set is
json + **matter_idl** (5/8 corpus inputs; the basic lexer cannot parse the other
3) + **poetry_markers** (12/12).

**Variants** (each a mechanical splice of the same `generate_standalone` output;
`examples/standalone_spike/gen/`):

| variant | scanner | what it isolates |
|---|---|---|
| `stock` | today's `regex`-crate combined alternation, compiled at `Parser::new` | the L5 value baseline |
| `lean` | the in-process dense DFA, **built at load time** + the spike's lean drive loop | "L5a without a bake" on this surface |
| `baked` | BFS-flattened `u32[state×256]` tables as `static` data + interpreter loop | L5b |
| `interned` | stock scanner; `Token::type_`/`Tree::data` as `&'static str` | L5d's M2 half (free here — names are already baked `&'static`) |

---

## 1. The L5 value baseline (what a stock generated parser pays today)

**Verdict: the epic's value case is CONFIRMED and larger than #620 states — the
standalone surface is scanner-bound in a way the in-process engine no longer is.**

* **Scan share of a stock generated parse is 52–72%** (json 52%, matter_idl 72%,
  poetry_markers 63%) — nothing like the in-process ~8% that killed B1 there.
  The stock runtime scans with `Regex::captures_at` per token (the slow
  capture-extraction engine), landing at 5.3–13.6 MB/s lex-only where the
  in-process DFA seam does ~95 MB/s. The generated parser is also ~2.2× slower
  than the in-process engine end-to-end (3.8 vs 8.5 MB/s on json).
* **Load cost is NOT the in-process ~1.8 ms/scanner figure** — it is the
  `regex` compile of one combined alternation: 0.25 ms (json), 0.31 ms
  (poetry_markers), **4.6 ms (matter_idl — of which ~3.8 ms is 48
  case-insensitive `unless` keywords, each compiled as its own `^(?i:kw)$`
  `Regex`, not the scanner itself)**.
* One-shot context (why standalone exists): on small inputs a stock generated
  parser already beats in-process `Lark::new`+parse by 12–30× (0.35 vs 5.2 ms on
  251 B json; 0.36 vs 10.0 ms on a 23 B marker; 4.7 vs 62 ms on 74 B matter_idl).
  In-process wins back above ~100 KB inputs on reused-throughput grounds.

## 2. L5b — flat-table bake spliced into the generated parser

**Verdict: CONFIRMED.** Same-session, gates green (streams + trees identical):

| grammar | variant | `Parser::new` | reused parse | lex-only | allocs/corpus-parse |
|---|---|---:|---:|---:|---:|
| json 594 KB | stock | 0.252 ms | 3.8 MB/s (1.00×) | 7.3 MB/s | 1 128 043 |
| | lean | 0.391 ms | 5.2 MB/s (1.39×) | 21.8 MB/s | 948 042 |
| | baked | **0.000 ms** | 4.8 MB/s (1.27×) | 16.9 MB/s | 948 042 |
| matter_idl 1.6 KB | stock | 4.57 ms | 3.8 MB/s (1.00×) | 5.3 MB/s | 2 473 |
| | lean | 6.23 ms | 8.1 MB/s (2.12×) | 19.2 MB/s | 1 993 |
| | baked | **3.83 ms** | 8.6 MB/s (2.25×) | 21.6 MB/s | 1 993 |
| poetry_markers 639 B | stock | 0.310 ms | 8.5 MB/s (1.00×) | 13.6 MB/s | 904 |
| | lean | 1.089 ms | 16.8 MB/s (1.96×) | 50.7 MB/s | 744 |
| | baked | **0.000 ms** | 17.5 MB/s (2.05×) | 58.5 MB/s | 744 |

One-shot (`Parser::new` + first parse), the #620 headline column:

| input | stock | baked | in-process |
|---|---:|---:|---:|
| json 251 B | 0.354 ms | **0.029 ms** | 5.2 ms |
| json 2.5 KB | 1.006 ms | 0.346 ms | 5.6 ms |
| json 27 KB | 7.6 ms | 6.4 ms | 7.8 ms |
| json 594 KB | 179 ms | 140 ms | 71 ms |
| marker 23 B | 0.359 ms | **0.001 ms** | 10.0 ms |
| matter_idl 74 B | 4.66 ms | 3.89 ms | 62 ms |

Deterministic deltas: `lean`/`baked` remove **exactly one allocation per scanner
invocation** (the `regex` `Captures` box: −180 001 on json = kept + ignored
matches; −480 matter_idl; −160 markers). Baked table rodata: 68 KiB (json),
158 KiB (matter_idl), 235 KiB (poetry_markers); flat-bake cost at generation
time 0.13–0.33 ms on top of generation's 5–34 ms.

Sub-findings:

1. **Baked always wins one-shot, at every size** — it never pays a scanner
   build *and* parses faster; on sub-KB inputs it is 12–360× the stock one-shot
   (0.001 vs 0.359 ms on a 23 B marker expression). There is no break-even to
   respect for L5b itself.
2. **"L5a without a bake" is a one-shot regression on this surface.** #620
   phases L5a (leaner drive loop, "no size cost") before L5b — but that phasing
   assumes the standalone runtime is already DFA-driven. It is not (it is a
   `regex` alternation), so the leanest no-rodata DFA variant must *determinize
   at load*, which costs **more** than the regex compile it replaces (lean
   `Parser::new`: 0.39 vs 0.25 ms json, 1.09 vs 0.31 ms markers, 6.2 vs 4.6 ms
   matter_idl). L5a as written is an **in-process** lever only; on the
   standalone surface the bake is what makes the DFA affordable at load, and
   L5a-alone should be dropped from the standalone plan.
3. **The interpreter-loop shape (flat table vs lean dense-DFA drive) is a wash
   on throughput here** — baked vs lean lex-only: 16.9 vs 21.8 (json), 21.6 vs
   19.2 (matter_idl), 58.5 vs 50.7 MB/s (markers). The prior spike's clean
   table>raw ordering does not reproduce at whole-lex level on this box; the
   bake's decisive edges are load time and footprint, not the loop.
4. **Case-insensitive `unless` keywords are the residual load cost and must be
   part of any real L5b.** matter_idl's baked `Parser::new` is still 3.83 ms —
   48 `^(?i:kw)$` `Regex` compiles the bake does not touch. A production bake
   that leaves `unless` on `Regex` forfeits most of the load win exactly on the
   keyword-heavy grammars that lex slowest.

## 3. L5b/L5c — compile time and binary footprint (real consumer crates)

**Verdict: the costs #620 asks to size are near-zero, and the bake is a large
footprint WIN, not a cost.** Minimal consumer crate (`regex` dep;
`regex-automata` added for lean), warm deps, leaf rebuild:

| grammar/variant | leaf compile | stripped binary | .text | .rodata | gen source |
|---|---:|---:|---:|---:|---:|
| json/stock | 0.90 s | 2 164 600 B | 1 138 KiB | 303 KiB | 55 KiB |
| json/baked | 0.82 s | **525 104 B** | 298 KiB | 95 KiB | 117 KiB |
| matter_idl/stock | 1.08 s | 2 267 168 B | 1 118 KiB | 344 KiB | 193 KiB |
| matter_idl/baked | 1.04 s | 2 416 904 B | 1 107 KiB | 503 KiB | 340 KiB |
| poetry_markers/stock | 0.89 s | 2 164 528 B | 1 114 KiB | 303 KiB | 52 KiB |
| poetry_markers/baked | 0.90 s | **694 552 B** | 298 KiB | 263 KiB | 250 KiB |

1. **Compile time does not move** (0.82–1.13 s leaf across all 12 variants;
   baked ≤ stock). Static tables are cheap for rustc at these sizes.
2. **When a grammar has no case-insensitive `unless` keywords, the baked parser
   never constructs a `Regex`, and the entire `regex` crate is dead-code
   eliminated: stripped binary −70–76%** (2.16 MB → 525/695 KB). A real L5b
   that also bakes the ci-`unless` check (a case-insensitive compare, no regex
   needed) could drop the `regex` dependency from generated parsers entirely —
   a packaging win #620 doesn't currently claim, and a step toward the no_std
   gesture in `standalone/mod.rs`'s docs. matter_idl (ci keywords keep `regex`
   linked) instead pays +150 KB for its 158 KiB table — the additive worst case.

## 4. L5c — footprint variants sweep (every qualifying scanner)

**Verdict: CONFIRMED-cheap; and the epic's multiplier fear is moot on this
surface.**

* **Which lexer does the standalone runtime use? BASIC, only.** One combined
  scanner per grammar — the ×47–108 deduped contextual-scanner multiplier (and
  the "several MB/grammar" worst case) in #620 does not exist on today's
  standalone surface. For reference, if a *contextual* standalone existed, the
  deduped per-state terminal-set counts here are json 10, matter_idl 60,
  poetry_markers 8, poetry_pep508 20.
* Table bytes per grammar (multiplier = 1):

| grammar | states | classes | 256×u32 | 256×u16 | cls×u32 | cls×u16 | `to_bytes` | class giveback |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| json | 68 | 35 | 68.0 KiB | 34.0 KiB | 9.5 KiB | 4.9 KiB | 18.2 KiB | 0.87× |
| matter_idl | 158 | 114 | 158.0 KiB | 79.0 KiB | 70.6 KiB | 35.4 KiB | 160.2 KiB | 0.82× |
| poetry_markers | 235 | 46 | 235.0 KiB | 117.5 KiB | 42.5 KiB | 21.4 KiB | 59.8 KiB | 0.99× |
| poetry_pep508 | 131 | 46 | 131.0 KiB | 65.5 KiB | 23.8 KiB | 12.0 KiB | 67.4 KiB | — |

  ("class giveback" = byte-class-compressed scan throughput as a fraction of the
  256-wide flat table, same-session, scan-only. u16 is size-only arithmetic —
  valid whenever states < 65 536, no throughput measurement taken.)
* **Rodata budget answer for #620:** worst observed grammar is ~235 KiB at the
  naive 256×u32 shape and 12–36 KiB byte-class-compressed u16, for ≤18% scan
  giveback (≤~7% end-to-end at these scan shares). Even the naive shape is a
  rounding error next to the 2.2 MB stripped baseline binary — and the baked
  variant *shrinks* the binary overall (see §3). `regex-automata`'s own
  `to_bytes` serialization is size-competitive with the flat u32 table but
  would keep the deserialization/validation code path; not needed.

## 5. L5d — interned output in the generated runtime

**Verdict: CONFIRMED (the M2/intern half; tape not attempted).** On the
standalone surface interning is *free*: symbol names and rule tree-names are
already `&'static str` in the baked `DATA`, so `Token::type_`/`Tree::data` can
simply stop copying them — no interner, unlike in-process M2. Deterministic
delta matches the closed form **exactly** on all three grammars:
`−(tokens_incl_EOI + shifted_tokens + tree_nodes)` allocations —
json −362 004 (= 132 002 + 132 001 + 98 001), matter_idl −790
(= 272 + 267 + 251), poetry_markers −238 (= 100 + 88 + 50). The extra
`shifted_tokens` term (vs in-process M2's −1/node) is standalone-specific: `run`
clones every shifted token onto the value stack, so the `type_` `String` was
being allocated **twice** per token. Wall-clock: 1.23× (json), 1.10×
(matter_idl), 1.16× (markers) — the in-process ~1.2–1.4× roughly transfers
across the ADR-0008 mirror. The remaining owned `String` per token is `value`
(M6 territory), exactly as in-process.

## Scope findings (feed these into #620's shaping)

1. **The standalone-able wild set is 3/14 projects.** Refusals: cel (guarded
   engine), gersemi/hcl2/miniwdl (lookaround out of DFA scope in-process too),
   lark_lark/mappyfile/pylogics/tartiflette/vyper (**lower in-process but the
   pure-`regex` standalone runtime cannot host them** — RC10), synapse_storm
   (Python-dialect rejection), pyquil (context-sensitive start states — the
   bake probe catches it, exactly as designed), dotmotif/mistql (non-LALR).
   A DFA-based standalone runtime is therefore also a **capability** lever: the
   five RC10 grammars become standalone-able in principle — but only with the
   guarded engine + guard tables baked too, which is beyond L5b's naive scope.
2. **A grammar can generate and still be useless: poetry_pep508 bakes fine but
   0/14 of its real inputs parse under the basic lexer** (contextual-dependent
   grammar; matter_idl loses 3/8 the same way). #620's "degrade gracefully per
   scanner" needs a sibling policy question: should `generate` warn/refuse when
   the grammar is contextual-load-bearing? Today it ships a parser that fails
   at runtime, per the documented (but easy to miss) basic-only limitation.
3. The flat-bake instrument transferred unchanged from the prior spike
   (`bake()` start-state probe + delayed-match/EOI handling); pyquil is again
   the only bake-probe rejection among generation survivors.

## What the architect needs to (re)price #620

* **L5b: confirmed, and cheaper than priced.** Rodata ≤235 KiB/grammar naive
  (≤36 KiB compressed), leaf compile time unchanged, binary usually *shrinks*
  (−70% when `unless`-ci is also baked away). Gains: one-shot 12–360× on small
  inputs, reused parse 1.3–2.3×, lex 2.3–4.3×. Scope additions: bake the
  ci-`unless` path (else keyword-heavy grammars keep a multi-ms load floor and
  the `regex` dep), and keep the per-scanner graceful degradation (pyquil).
* **L5a: drop as a standalone phase** — it only exists in-process; at the
  generated-parser surface a load-time DFA build is a one-shot regression.
* **L5c: default to 256-wide u32 (simplest, fastest) unless a size budget
  appears; byte-class×u16 is the 5–36 KiB fallback at ≤18% scan giveback.
  The contextual multiplier is not applicable (basic-only runtime).**
* **L5d: the intern half is free and confirmed (1.1–1.23×, exact alloc
  closed-form); decide it with #619's tape/M5 question — tape remains unmeasured
  here.**

## Artifacts

| piece | role |
|---|---|
| `examples/standalone_bake_gen.rs` | probe + splicer: regenerates the variant copies, prints the L5c sweep (SKIP taxonomy on the standalone surface) |
| `examples/standalone_spike/gen/*.rs` | the 12 committed variant copies (`@generated`, spike-only — regenerate, never hand-edit) |
| `examples/standalone_bake_bench.rs` | gates (stream + oracle-tree differentials) + all timing/alloc tables above |
| `examples/standalone_spike/measure_size.sh` | consumer-crate compile-time + binary/section sizes |

All behind the default-off `baked-dfa-spike` feature; the default build, the
emitter, and `tests/standalone/` fixtures are untouched. Delete the whole set
with the spike.
