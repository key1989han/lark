# Spike: L5 standalone-bake epic (#620) → measurements on the standalone surface (2026-07-03)

**Throwaway measurement spike.** Turns the phases of issue #620 (bake the DFA lexer
into `generate_standalone` / `include_lark!` parsers) into before/after numbers **on
the standalone surface**, before the epic is committed. The prior 2026-07-03 spike
(PR #617, `spike-parser-optimizations-2026-07-03.md`) proved the levers at the
*in-process DFA-scanner* level and **killed** the baked DFA as an in-process end-to-end
lever (~6%). #620's thesis is that this **inverts on the one-shot/standalone path**.
This spike tests that thesis where it lives: a *generated* parser's build+parse
economics and binary footprint.

Everything here is directional and feature-gated: the harnesses are throwaway examples
behind `baked-dfa-spike` plus one shell driver; nothing on the default build path
changed. **Only ratios travel** (shared Linux x86_64 box, release + LTO), per BENCH.md.

## The one fact that reprices the whole epic

**The standalone runtime does not use the DFA lexer, and does not use the contextual
lexer.** `standalone/runtime.rs`'s `Scanner` is a **`regex`-crate combined
alternation** compiled at load with `Regex::new(...)` + `captures_at` — the *basic*
lexer, on the pre-DFA-flip capture path (`src/standalone/mod.rs`: "Basic lexer only";
`runtime.rs::Scanner`). Two consequences dominate every number below:

1. **The bake multiplier is ×1, not ×47–108.** #620's "several MB/grammar worst case"
   is the *in-process contextual* lexer's deduped per-state scanner count. Standalone
   bakes **one** basic-lexer scanner, so per-grammar rodata is **14–235 KiB**, not
   megabytes. (`standalone_bake_probe`.)
2. **The "before" is the slow regex-crate capture scanner**, not the DFA seam. So the
   scanner is a *much* bigger share of a standalone parse than the in-process ~8%, and
   baking a table gives a far larger win than the in-process spike's 3.8× — because it
   also moves off the capture path the DFA flip already abandoned in-process.

## Reproduce

```bash
cd lark-rs
# Recon: bakeable set + per-scanner footprint (flat / byte-class / to_bytes).
cargo run --release --features baked-dfa-spike --example standalone_bake_probe
# Part A: scanner-representation throughput ladder, differential-gated.
cargo run --release --features baked-dfa-spike --example standalone_bake_lex
# Part B: emit + compile + measure three generated crates (stock / baked / classed).
examples/standalone_bake_partb.sh <grammar.lark> <start> <records|workload-file>
#   e.g.  examples/standalone_bake_partb.sh /path/json.lark start 2000
```

Deterministic signals: a token-stream differential (Part A: every rep emits the
byte-identical `(id, end)` stream before timing) and a structural tree digest (Part B:
FNV over the whole parse tree, asserted equal across stock/baked/classed **and** the
in-process basic-lexer oracle). Wall-clock is same-session back-to-back ratios only.

---

## Recon — the bakeable set and the ×1 footprint (L5c input)

`generate_standalone` over JSON + all 16 wild grammars, and of the accepted ones which
plain scanner **DFA-flattens** (single dense engine, context-insensitive start — the
`bake()` precondition). Footprint is per **one** scanner (the standalone multiplier):

| grammar | `generate_standalone` | DFA-flattens | states | classes | flat KiB | byte-class KiB | to_bytes KiB |
|---|---|---|---:|---:|---:|---:|---:|
| json           | bakes | **yes** | 68  | 55  | 68  | **14** | 18 |
| poetry_pep508  | bakes | **yes** | 131 | 89  | 131 | **46** | 67 |
| poetry_markers | bakes | **yes** | 235 | 63  | 235 | **58** | 60 |
| matter_idl     | bakes | **yes** | 158 | 170 | 158 | **105** | 160 |
| tartiflette    | **rejects** (can't-host terminal) | yes | 121 | 112 | 121 | 53 | 62 |
| cel, pyquil    | bakes | no (guarded / context-sensitive start) | — | — | — | — | — |
| gersemi_cmake, hcl2, miniwdl_wdl | rejects (lookaround) | no | — | — | — | — | — |
| lark_lark, mappyfile, pylogics_ltl, vyper, synapse_storm | rejects (can't-host / backref) | no | — | — | — | — | — |
| dotmotif, mistql | rejects (non-LALR) | — | — | — | — | — | — |

**Reading.**
- **Per-grammar rodata is 14–235 KiB** (byte-class: 14–105 KiB), ×1 — not the
  megabyte-scale #620 sized against the contextual multiplier. **The L5c "budget"
  question is much smaller than the epic assumes.**
- **Byte-class compression is the smallest encoding** in every row (2.9–4.9× under
  flat; ≤ `to_bytes` too) and, unlike `to_bytes`, needs no `regex-automata` at runtime
  (it's a plain `u32[state*classes]` + a 256-byte map), so it *preserves* the
  dependency-drop below. `to_bytes` keeps `regex-automata` linked and is therefore the
  wrong footprint variant for standalone.
- **Scope is narrow.** Only 4 wild grammars (+ JSON) both bake *and* flatten, and two of
  those (matter_idl, and pep508) turn out **not basic-lexer-parseable** (below), so the
  perf/footprint lever's real beneficiary population on today's wild bank is *tiny*.
  `tartiflette` flattens but the current standalone *rejects* it (a terminal the
  `regex` runtime can't host) — a **capability** L5 could unlock by baking the DFA,
  distinct from the perf lever.

---

## L5a — leaner drive loop — **verdict: premise does not hold on standalone**

#620's L5a is "hoist per-position work in the standalone runtime's DFA loop — ~2× scan,
*no representation change, no size cost*." But **the standalone runtime has no DFA loop
to lean out** — it runs the `regex` crate. Any DFA drive (leaner or not) is *already* a
representation change for standalone, and getting it at load means either shipping
`regex-automata` + paying determinization per parser build, or baking (L5b). So L5a as
specified is **N/A** here; its throughput is real but only reachable *through* L5b.

Evidence (Part A, `standalone_bake_lex`, 594 KB JSON, reused scan, ratio vs `rx` = the
stock standalone scanner; differential-gated identical streams):

| rep | what | MB/s | ×rx |
|---|---|---:|---:|
| **rx** | regex-crate combined alternation = **stock standalone** | 14.0 | 1.00× |
| seam | in-process `BasicLexer::match_at` (regex-automata DFA) | 117 | 8.4× |
| raw | hand `Automaton` loop over dense DFA (the "leaner loop") | 195 | **13.9×** |
| table | flat baked `u32[state*256]` | 312 | **22.2×** |
| table-uc | flat table, unchecked | 391 | 27.9× |
| classed | byte-class table (`u32[state*classes]`) | 295 | 21.0× |

The leaner loop (`raw`) is ~2× the DFA seam (13.9× / 8.4×) — reproducing the in-process
spike's finding 2 — but ~1.6× *below* the flat table, and you cannot ship it on
standalone without the determinization cost the bake erases. **The standalone win is
L5b, which subsumes the leaner loop.**

---

## L5b — flat-table bake into generated parsers — **verdict: CONFIRMED (and inverts the cost model)**

Part B compiles three real generated crates (stock / baked-flat / baked-byte-class),
splicing a static-table interpreter into the *same* generated runtime and **dropping the
`regex` dependency entirely** (JSON's `unless` is empty; case-insensitive keyword retype
is re-expressed with `eq_ignore_ascii_case`, no regex). 594 KB JSON, one shared box:

| variant | reused parse | one-shot | binary (raw / stripped) | `.text` | cold build | warm rebuild |
|---|---:|---:|---:|---:|---:|---:|
| **stock** (regex Scanner) | 5.3 MB/s (112 ms) | 177 ms | 2.04 / **1.81 MiB** | 1.75 MiB | 18.0 s | 13.3 s |
| **baked** (flat table) | 9.3 MB/s (64 ms) **1.76×** | 122 ms | 0.52 / **0.46 MiB** | 449 KiB | 4.4 s | 4.25 s |
| **classed** (byte-class) | 9.1 MB/s (65 ms) 1.71× | 121 ms | 0.47 / **0.40 MiB** | 394 KiB | 4.4 s | 4.2 s |

All three are **tree-identical to the in-process basic-lexer oracle** over the full
594 KB (FNV digest match).

**The three numbers #620 asked to size, on the standalone surface:**

- **Rodata budget: negative.** Baking the flat table *shrinks* the binary **4.1×**
  (1.81 → 0.46 MiB stripped; byte-class **4.5×** → 0.40 MiB). The 68 KiB table is a
  rounding error against the ~1.3 MiB `regex` engine it *replaces*. #620's "several MB
  of static tables per grammar" worry is **inverted**: on standalone, baking removes
  more than it adds.
- **Compile-time cost: negative.** Cold build **18.0 → 4.4 s (4.1×)**, warm rebuild
  **13.3 → 4.25 s (3.1×)** — again because the `regex` dependency (and its LTO IR) is
  dropped. The big `static` table does **not** slow rustc (baked warm 4.25 s).
- **One-shot break-even: input-size dependent.**
  - *Large input (server, one big document):* one-shot ≈ reused (parse-dominated);
    baking gives **~1.76×** end-to-end, and the erased `Regex::new` (0.26 ms) is noise.
  - *Small input (CLI/embedded, many short parses):* the per-invocation `Regex::new`
    dominates. poetry_markers (82 B): **one-shot 690 µs → 25 µs = 27×** (the regex build
    is erased), reused 8.2 → 4.7 µs (1.75×). **This is #620's "one-shot is where baking
    pays" thesis — confirmed** — but the erased cost is the **regex-crate combined
    build (0.26–0.66 ms, one scanner)**, not the ~1.8 ms determinization × many
    contextual scanners the epic cites (standalone never pays that).

**Second/third grammars confirm the size + compile inversion and the correctness of the
regex-free retype:**

| grammar (input) | stock stripped | baked / classed stripped | cold build stock→baked | correctness |
|---|---:|---:|---:|---:|
| json (594 KB)            | 1.81 MiB | 0.46 / 0.40 MiB | 18.0 → 4.4 s | ✅ = oracle |
| kw ci-keywords (95 KB)   | 1.81 MiB | 0.41 / 0.40 MiB | 18.2 → 4.24 s | ✅ = oracle (regex-free `unless`) |
| poetry_markers (82 B, **235-state** table) | 1.81 MiB | 0.64 / 0.45 MiB | 17.8 → 4.3 s | ✅ = oracle |

Even the **largest** table in the set (poetry_markers, 235 states / 235 KiB flat) yields
a binary **3× smaller** than stock; byte-class compression brings it to **4.2×** (and
saves 182 KiB of binary vs flat — byte-class matters most exactly where the table is
biggest).

---

## L5c — footprint variants — **verdict: byte-class table is the recommended default**

Throughput giveback of byte-class vs flat (Part A, reused scan): **~5% median (0–11%
range)** for a **1.5–4.9×** smaller table:

| grammar | flat ×rx | classed ×rx | giveback | table shrink |
|---|---:|---:|---:|---:|
| json          | 22.2× | 21.0× | 5.4% | 68 → 14 KiB (4.9×) |
| poetry_pep508 | 12.4× | 11.0× | 11%  | 131 → 46 KiB (2.9×) |
| matter_idl    | 6.63× | 6.48× | 2.3% | 158 → 105 KiB (1.5×) |
| poetry_markers| 12.2× | 13.1× | none (tiny-input noise) | 235 → 58 KiB (4.1×) |

Byte-class compression is nearly free on throughput, is the smallest encoding, and stays
**dependency-free** (unlike `regex-automata::to_bytes`, which keeps the crate linked and
forfeits the binary-size inversion). **Recommend byte-class as the L5b default encoding**,
with flat-256 as an opt-in for the last few % on hot grammars.

`matter_idl` is the ceiling case: 170 byte-classes (near 256, barely compressible) and a
per-token cost dominated by the `unless` case-insensitive keyword retype **shared by all
reps** — so every rep lands ~6× rx and the scanner engine matters least (reproducing the
in-process spike's finding 5). Table still beats rx 6.6×, and the binary/compile
inversion is unaffected (retype is regex-free in the bake).

---

## L5d — tape/interned output in the runtime copy — **verdict: deferred (not measured)**

Not attempted this spike. Rationale: (1) it is real surgery in the generated runtime's
`run`/`assemble`/`shape` path (a second copy of output shaping to keep honest under the
ADR-0008 mirror), (2) the in-process reference (−2 allocs/node, ~1.6×) already exists and
per the mirror must be *re-measured*, not assumed, and (3) a more urgent runtime issue
surfaced first (below) that any tape work must contend with. A single confirm/refute is
still worth a follow-up, but it is lower-value than L5b and independent of it.

### Incidental out-of-scope find (file separately)

**The standalone runtime has an O(n²) in flat-repetition (`tok+`) tree assembly.** A
95 KB input of `start: tok+` (20 000 repetitions) parsed in **~2.5 s** — scan-independent
(stock ≈ baked ≈ classed), so it is the `assemble`/transparent-recurse path rebuilding
the growing child vector per reduction (the standalone analog of the resolve-mode
quadratic #55 fixed in-process). Orthogonal to #620; should be its own issue. (It is why
the `kw` grammar is a size/compile/correctness data point only, not a throughput one.)

---

## Bottom line for #620

| phase | standalone verdict | headline number |
|---|---|---|
| **L5a** leaner loop | **N/A as specified** — stock is regex-crate, not a DFA; win is only reachable via L5b | raw = 13.9× rx (2× the seam), but not shippable without the bake |
| **L5b** flat-table bake | **CONFIRMED, cost model inverted** | 1.76× large-input parse; **27× one-shot** small-input; **binary 4.1× smaller**, **compile 4.1× faster** (regex dep dropped); tree-identical to oracle |
| **L5c** footprint | **byte-class is the default** | ~5% throughput giveback for 1.5–4.9× smaller table; **×1 multiplier, 14–105 KiB/grammar**, dependency-free |
| **L5d** tape output | **deferred** | not measured; in-process −2 allocs/node is the unverified reference |

**Net.** #620's two costed risks — rodata footprint and compile time — are **not costs on
the standalone surface; they are gains**, because baking a table lets the generated parser
drop the `regex` engine (the ×1 basic-lexer multiplier is what makes the table small
enough for this to hold). The value is real but **bimodal**: modest (1.76×) for one large
document, large (up to ~27×) for the many-small-inputs CLI/embedded pattern where the
per-invocation `Regex::new` is the cost. The **scope is the catch**: on today's wild
bank only JSON-shaped and tiny grammars both bake and basic-lex, so the perf lever's
audience is narrow unless L5 also pursues the **lookaround-capability** unlock (baking
the DFA to accept grammars the `regex` runtime currently rejects — e.g. `tartiflette`),
which is a *different* justification than the throughput/footprint one #620 leads with.

Suggested re-pricing: split L5b into **(a)** the flat/byte-class bake behind the existing
`generate_standalone` (small, differential + oracle-digest gated, net-negative
footprint/compile — cheap and safe) and **(b)** the lookaround-capability bake (the real
scope expansion, its own ADR). Drop the "several-MB rodata" and "compile-time cost" risks
from the standalone framing; they do not reproduce.

---

## Artifacts (throwaway, behind `baked-dfa-spike`; delete with the spike)

| file | role |
|---|---|
| `examples/standalone_bake_probe.rs` | bakeable set + per-scanner footprint (flat/byte-class/`to_bytes`), the ×1-multiplier table |
| `examples/standalone_bake_lex.rs` | scanner-rep throughput ladder incl. the **rx** (stock standalone) baseline + byte-class rep; token-stream differential gate; one-shot build costs |
| `examples/standalone_bake_emit.rs` | emits stock + baked-flat + baked-byte-class generated crates by splicing a static-table interpreter into the runtime and dropping `regex` |
| `examples/standalone_oracle_digest.rs` | in-process basic-lexer oracle digest (Part B correctness anchor) — *not* spike-gated |
| `examples/standalone_bake_partb.sh` | Part B driver: emit → compile 3 crates → binary size / compile time / one-shot / reused / digest gate |

Durable instrument reused verbatim from PR #617: `bake()` (BFS flatten + start-context
probe + delayed-match/EOI), the `spike_plain_dense` / `spike_retype` accessors
(`src/lexer/{dfa,mod}.rs`), and `raw_match_at`. The standalone additions are the `rx`
baseline, the byte-class rep, and the whole Part B generate→compile→measure loop.
