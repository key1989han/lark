# Spike: standalone-surface bake (L5 / #620) → measurements (2026-07-03)

**Throwaway measurement spike for issue #620** (execute the L5 DFA-lexer bake on the
standalone / `include_lark!` surface). The 2026-07-03 in-process spike (PR #617,
`spike-parser-optimizations-2026-07-03.md`) proved the baked-DFA lever at the
*in-process scanner seam* and there it was a small end-to-end lever (~6%, scanning
already fast on the `regex-automata` DFA). #620's thesis is that the same lever
**inverts on the one-shot / standalone path**. This spike takes #620's phases
(L5a/L5b/L5c/L5d) to real before/after numbers **on the generated-parser surface**,
so the architect can approve or reprice the epic.

**Headline: #620's value thesis is CONFIRMED and its *cost* concerns are INVERTED.**
Baking the DFA into a generated JSON parser makes it **1.78× faster to parse
(reused), 4.5× smaller as a binary, ~12× faster to compile, and ~25× faster
one-shot on small inputs** — because the standalone surface is **basic-lexer-only
(one scanner per grammar, not 47–108 contextual scanners)** and its scanner is the
**`regex` crate**, not `regex-automata`. Baking removes the `regex` dependency
entirely; the removed dep dwarfs the added table, so the footprint/compile-time
costs #620 flagged do not materialize — they reverse.

Re-run:

```bash
# Harness A — in-process scanner isolation (deterministic ratios + footprint):
cargo run --release --features baked-dfa-spike --example standalone_bake_spike

# Harness B — real generated crates (one-shot / compile-time / binary size):
cargo run --release --features baked-dfa-spike --example gen_baked_parser -- <out_dir>
#   then: bake-crates/measure.sh  (clean+incremental build time, stripped size,
#   one-shot + reused parse, per-parse tree digest as the correctness gate)
```

**Method.** Deterministic signal is the headline (BENCH.md). Correctness gate, per
variant: Harness A asserts every variant emits the **byte-identical token stream**
vs the live standalone scanner before timing (same automaton, same work ⇒ the delta
is pure interpretation cost); Harness B asserts the baked crate's **parse tree is
byte-identical** to the stock generated crate over the whole 610 KB workload (an FNV
digest of the tree's `Display`). Tree-identity chain to the oracle:
`live scanner ≡(A differential) baked tokens`; `stock generated ≡(test_standalone
round-trip) in-process oracle`; `baked ≡(B digest) stock` ⇒ **baked ≡ oracle**.
Wall-clock is a same-session, back-to-back trend on one shared Linux x86_64 box;
only ratios travel. Table/binary byte counts and build times are the size/compile
axis #620 asks to size.

**What the standalone runtime actually is** (the two facts that reprice #620):

1. **Basic lexer only → one scanner per grammar.** `src/standalone/runtime.rs` bakes
   a single combined-alternation scanner. #620's cost model multiplies table bytes
   by "47–108 deduped **contextual** scanners"; the standalone surface uses **none
   of them**. The multiplier is **1**.
2. **The scanner engine is the `regex` crate, not `regex-automata`.** The generated
   runtime does `Regex::new(combined_alternation)` at `Parser::new` and
   `captures_at` per position. This is the *slow* engine the in-process default left
   behind — so the standalone scanner is ~13–25 MB/s where the in-process DFA seam
   is ~90–110 MB/s. The bake's headroom on standalone is therefore **much larger**
   than the ~4× the in-process spike measured against the already-fast DFA seam.

---

## L5b — flat-table bake into a generated parser (the headline)

**Verdict: CONFIRMED, and stronger than #620 predicted — the cost concerns invert.**

Harness B generates two self-contained JSON parser crates from the *same*
`generate_standalone` output: **stock** (regex combined-alternation scanner, ships
today) and **baked** (the `Scanner` struct+impl surgically swapped for a flat
`u32[state×256]` opcode-table interpreter, and `use regex::Regex;` + the `regex`
dependency dropped). Every other line — `run`/`assemble`/`shape`/`Tree` — is
byte-identical, so a tree divergence is a bake bug. Workload: 610 KB JSON
(`input_594k.json`), tree digest identical across both crates (`39a3fee7f91749db`).

| metric | stock (regex) | baked (flat table) | delta |
|---|---:|---:|---:|
| **reused parse** (whole parse, min of 80) | 120.3 ms | 67.8 ms | **1.78× faster** |
| one-shot (Parser::new + first parse, 610 KB) | 173.4 ms | 125.4 ms | 1.38× faster |
| **stripped binary size** | 2,169,392 B (2.07 MiB) | 481,352 B (470 KiB) | **4.5× smaller** |
| **clean build** (generate→binary, deps included) | 8.90 s | 0.76 s | **11.7× faster** |
| incremental parser-only rebuild (deps cached) | 1.00 s | 0.72 s | 1.39× faster |
| tree digest (correctness gate) | `39a3fee7f91749db` | `39a3fee7f91749db` | **identical** |

**Reading.**

- **Reused parse is 1.78× faster end-to-end** — not scan-only, *whole parse*
  including LALR + owned-`Tree` build. On the standalone surface the `regex` scanner
  is ~half of the whole parse (Harness A: scan is 71–84% of *lexing*, and the real
  `runtime::Scanner` allocates a `Captures` per token, slower than Harness A's
  in-process `Regex` proxy — see the caveat below), so a ~20× scan more than halves
  the parse.
- **The binary shrinks 4.5×.** #620's stated risk is "several MB of static tables
  per grammar." The opposite happens: the 68 KiB table is *added*, but the ~1.6 MiB
  `regex` runtime is *removed*, netting a **4.5× smaller** binary. On the
  basic-lexer standalone surface, baking is a binary-size **win**, not a cost.
- **The crate compiles ~12× faster** (8.9 s → 0.76 s clean). The compile cost of the
  stock parser is the `regex` crate, not the parser; a 68 KiB `static [u32]` literal
  compiles in well under a second (incremental parser rebuild is actually *faster*
  baked, 1.00 → 0.72 s, because there is no `regex` to link).

---

## L5a — leaner drive loop (Harness A, in-process scanner isolation)

**Verdict: CONFIRMED, bounded — and the bigger free win on standalone is the engine
swap it presupposes.**

All variants drive the *same* determinized automaton (extracted from the real DFA
backend via the `baked-dfa-spike` accessors); the differential gates identical token
streams. Ratios vs **stock** (the `regex` combined-alternation scanner, the standalone
runtime's engine). Reused scan throughput, median:

| workload | stock MB/s | dfa (engine swap) | raw (L5a loop) | table (L5b) | table-cls (L5c) | scan % of lex |
|---|---:|---:|---:|---:|---:|---:|
| json 56 KB  | 14.2 | 6.5× | 10.9× | **21.1×** | 19.0× | 75% |
| json 594 KB | 14.6 | 6.6× | 11.2× | **22.1×** | 19.3× | 71% |
| poetry_markers (639 B, 235 states) | 23.1 | 4.8× | 8.7× | **12.2×** | 12.8× | 79% |
| poetry_pep508 (593 B, 131 states) | 25.3 | 4.4× | 6.5× | **11.9×** | 10.1× | 79% |

- **dfa** = swap the `regex` crate for the `regex-automata` DFA engine, *no baking*:
  already **4.4–6.6×** on the standalone surface (this is the engine the in-process
  default uses; standalone never adopted it). This is the free win L5a presupposes.
- **raw** = the leaner `Automaton`-trait drive loop (#620's L5a: hoist the
  per-position `Input`/prefilter) — a further ~1.7× over `dfa`, **6.5–11×** over
  stock, no representation change, no size cost.
- The whole ladder (stock → dfa → raw → table) is a clean decomposition: most of the
  standalone win is available *before* baking, just by using a better engine and a
  leaner loop; baking captures the last ~2× and, crucially, removes the runtime
  dependency (the L5b binary/compile wins above).

---

## L5c — footprint sweep

**Verdict: the multiplier is 1; #620's "several MB/grammar" fear does not apply to
the current standalone surface, and baking shrinks the binary regardless.**

The standalone runtime uses **one** basic-lexer scanner per grammar, so the
total-rodata-per-grammar #620 asks for is **one** table, not `deduped-scanner-count ×
table`. Per-scanner footprint (Harness A):

| grammar | baked states | flat 256-wide | byte-class (k-wide) | regex-automata `to_bytes` |
|---|---:|---:|---:|---:|
| json        | 68  | 68 KiB  | 14 KiB (55 classes) | 18 KiB |
| poetry_markers | 235 | 235 KiB | 57 KiB (62 classes) | 59 KiB |
| poetry_pep508  | 131 | 131 KiB | 45 KiB (88 classes) | 67 KiB |

- **Byte-class compression is cheap**: 4–5× smaller table (14–57 KiB) for a **5–13%
  throughput giveback** (json 22.1× → 19.3×; poetry_markers actually ties/edges
  ahead). One extra indirection through a 256-byte class map.
- **`regex-automata` `to_bytes`** lands between flat and byte-class (18–67 KiB) but
  is a serialized-automaton format needing `regex-automata` at runtime to deserialize
  — which reintroduces the dependency the flat/byte-class bake *removes* (the whole
  L5b binary win). So the flat or byte-class static table is the right shape;
  `to_bytes` is not worth the dep.
- **Net footprint verdict**: even the *largest* flat table here (235 KiB) is dwarfed
  by the ~1.6 MiB `regex` runtime the bake deletes, so **any** of these variants
  yields a net **smaller** binary. L5c (byte-class compression) is a nice-to-have for
  the table itself, not a gate on the epic.

---

## L5d — tape/interned output in the generated runtime (optional)

**Verdict: not measured this session; the data points to it as the clear next lever.**

The baked reused parse is now **67.8 ms**, of which scanning is ~2 ms (the baked table
at ~300 MB/s over 610 KB). The residual ~66 ms is LALR drive + **owned-`Tree`
materialization** (per-node `String` label + `Vec<Child>` + two owned `String`s per
token) — exactly the allocation profile the in-process spike's M2+M5 (−2 allocs/node,
~1.6×) targeted. So after L5b the generated parser is **output-bound**, mirroring the
in-process finding. L5d (tape + interned labels in the runtime copy) is the next ~1.6×
and is orthogonal to the scanner bake. A single confirm/refute across the ADR-0008
mirror is left as follow-up; the prediction is that it transfers (the runtime's `Tree`
is the same owned shape as the in-process default).

---

## The numbers the architect needs (to approve / reprice #620)

- **Rodata budget**: one table per grammar (basic lexer). 68–235 KiB flat, or
  14–57 KiB byte-class-compressed. **Not** the several-MB contextual-scanner product
  #620 sized — that surface doesn't exist in standalone today.
- **Binary size**: baking **reduces** the generated binary (JSON: 2.07 MiB → 470 KiB,
  4.5×) because it removes the `regex` dependency. Net-negative cost.
- **Compile-time cost**: baking **reduces** clean build ~12× (8.9 s → 0.76 s) and
  incremental parser rebuild 1.39× — the static table is cheap; the removed `regex`
  crate was the cost.
- **One-shot break-even**: the stock parser's fixed `Parser::new` cost (compiling the
  combined `regex`) is **~620 µs** (measured on a 30-byte input: one-shot 645 µs →
  25 µs baked, 25×). The baked parser pays ~0 fixed cost. **Baking wins one-shot at
  every input size**; the smaller the input, the larger the win (the fixed regex
  build dominates tiny parses).
- **Reused break-even**: baked is 1.78× on 610 KB whole-parse and ~2× on a 30-byte
  parse — a win at every size measured.
- **Scope (SKIP taxonomy holds)**: grammars whose basic lexer refuses to build
  (lookaround/scope: `gersemi_cmake`, `hcl2`) or that build a non-plain-dense scanner
  (`lark_lark`, `mappyfile`, `pylogics_ltl`, the `expr` synthetic) or don't build at
  all (wild xfails) are correctly skipped — the emitter must degrade per-scanner and
  keep the stock path for these, exactly as #620 says.

## Caveats

- **Harness A's `stock` is a lower bound on the real cost.** It measures the
  in-process `LexerBackend::Regex` seam, which reuses a `CaptureLocations` scratch
  (the 2026-06-04 fix). The shipped `runtime::Scanner::match_at` allocates a fresh
  `Captures` per token (`captures_at`), so the **real** standalone scanner is slower
  than the `stock` proxy — the true L5b lever is *at least* as large as reported
  (Harness B's whole-parse 1.78× already reflects the real scanner).
- **`unless` retype**: the baked splice asserts an empty `unless` map (JSON qualifies).
  A grammar with case-insensitive keyword retypes would need the retype reproduced in
  the baked `match_at` (the in-process `spike_retype` shows it's a hashmap + a few
  anchored regexes — cheap, but not wired in this throwaway). Not a blocker; a note for
  the productionization.
- **ADR-0008 mirror tax is real**: the baked drive loop is a second copy of scanner
  semantics. The Harness A differential + Harness B tree digest make drift loud, but
  it is recurring maintenance the epic must budget for (as #620 already flags).

## Artifacts (throwaway, `baked-dfa-spike` feature — delete with the spike)

- `examples/standalone_bake_spike.rs` — Harness A: in-process scanner isolation over
  the standalone surface (stock/dfa/raw/table/table-cls ladder + footprint + scan
  share), differential-gated. Reuses the prior spike's `bake()` + `spike_plain_dense`
  / `spike_retype` accessors.
- `examples/gen_baked_parser.rs` — Harness B generator: emits a stock + a baked JSON
  parser crate (the surgical `Scanner` splice + `regex`-dep removal) into an out dir.
- `bake-crates/measure.sh` (scratch) — compiles both crates and reports clean +
  incremental build time, stripped binary size, one-shot + reused parse, and the tree
  digest correctness gate. Regenerate the crates with `gen_baked_parser`.

All library hooks are the prior spike's default-off `baked-dfa-spike` accessors; the
default build is unchanged.
