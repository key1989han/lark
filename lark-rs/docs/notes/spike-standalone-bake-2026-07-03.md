# Spike: L5 standalone-bake epic (#620) → measurements on the standalone surface (2026-07-03)

**Consolidated record of three parallel spikes (PRs #621, #622, #623).** This branch
(#623) is the canonical base; the net-new results the siblings contributed are folded in
and attributed inline (§"Triangulation" lists what each added). Turns the phases of issue
#620 (bake the DFA lexer into `generate_standalone` / `include_lark!` parsers) into
before/after numbers **on the standalone surface**, before the epic is committed. The
prior 2026-07-03 spike (PR #617, `spike-parser-optimizations-2026-07-03.md`) proved the
levers at the *in-process DFA-scanner* level and **killed** the baked DFA as an in-process
end-to-end lever (~6%). #620's thesis is that this **inverts on the one-shot/standalone
path**. This spike tests that thesis where it lives: a *generated* parser's build+parse
economics and binary footprint.

Everything here is directional and feature-gated: the harnesses are throwaway examples
behind `baked-dfa-spike` plus one shell driver; nothing on the default build path
changed. **Only ratios travel** (shared Linux x86_64 box, release + LTO), per BENCH.md.

> **Triangulation (three independent harnesses, one answer).** #621, #622, and #623 each
> built a separate standalone-bake harness and independently confirmed the two
> load-bearing facts: the standalone runtime is a **`regex`-crate basic lexer** → the bake
> multiplier is **×1** (14–235 KiB/grammar, not the ×47–108 contextual worst case #620
> sized against), and baking **drops the `regex` dependency**, so binary size and compile
> time *invert* from costs into gains. Net-new to each: **#623** (this note) implemented
> the regex-free `unless` retype so the dep drops even for keyword grammars, measured the
> full generate→compile→size loop with an oracle-digest correctness gate, found the
> O(n²) `tok+` bug, and **ports + re-measures the L5d intern half** (the `baked_interned`
> variant); **#622** first measured the L5d intern half with an exact allocation
> closed-form and caught the matter_idl binary-*grows* exception (regex kept for ci
> keywords); **#621** framed the post-bake parser as **output-bound**. Where a figure is
> cited from a sibling rather than re-measured here, it is marked as such.

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
  those are **contextual-load-bearing**, so the basic-lexer standalone parses only part of
  their real corpus (matter_idl **5/8** inputs; **poetry_pep508 0/14**) — see the
  degradation-policy finding below. So the perf/footprint lever's real beneficiary
  population on today's wild bank is *tiny*. `tartiflette` flattens but the current
  standalone *rejects* it (a terminal the `regex` runtime can't host) — a **capability** L5
  could unlock by baking the DFA, distinct from the perf lever.

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

**Four grammars confirm the size + compile inversion and the correctness of the
regex-free retype:**

| grammar (input) | stock stripped | baked / classed stripped | cold build stock→baked | correctness |
|---|---:|---:|---:|---:|
| json (594 KB)            | 1.81 MiB | 0.46 / 0.40 MiB | 18.0 → 4.4 s | ✅ = oracle |
| kw ci-keywords (95 KB)   | 1.81 MiB | 0.41 / 0.40 MiB | 18.2 → 4.24 s | ✅ = oracle (regex-free `unless`) |
| poetry_markers (82 B, **235-state** table) | 1.81 MiB | 0.64 / 0.45 MiB | 17.8 → 4.3 s | ✅ = oracle |
| **matter_idl** (849 B, 57 `unless`, **47 ci**) | 1.91 MiB | **0.63 / 0.58 MiB** | — | ✅ = oracle |

Even the **largest** table in the set (poetry_markers, 235 states / 235 KiB flat) yields
a binary **3× smaller** than stock; byte-class compression brings it to **4.2×** (and
saves 182 KiB of binary vs flat — byte-class matters most exactly where the table is
biggest).

**matter_idl resolves the one exception the siblings could not — the regex-free retype is
this note's decisive advantage.** #622 measured matter_idl's baked binary *growing* to
2.42 MiB (+150 KB over its 2.27 MiB stock): its bake left the **47 case-insensitive
`unless` keywords on `Regex`** (`^(?i:kw)$` per keyword), so the whole `regex` crate stayed
linked and the 158 KiB table was pure addition. Because #623's `Scanner::new` re-expresses
the ci retype with **`eq_ignore_ascii_case`** (no regex), `regex` is fully dead-code
eliminated even for this keyword-heavy grammar: **baked 0.63 MiB / classed 0.58 MiB — a
3.0–3.3× *shrink*** (vs #622's baked 2.42 MiB, which *grew* +150 KB over its stock),
tree-identical to the oracle over the ci-keyword-heavy input (digest gate). So the "binary grows for ci-keyword grammars" caveat #622 flagged, and the "unless
not wired in this throwaway" caveat #621 flagged, are both **closed**: bake the ci-`unless`
as an ASCII compare and the dependency-drop holds for *every* qualifying grammar. (The
retype boundary: `eq_ignore_ascii_case` matches Python's `(?i:)` for ASCII keywords — all
bundled/wild ci keywords are ASCII; a non-ASCII ci keyword would need a Unicode-fold
compare, still regex-free. The digest gate would catch any divergence.)

**Compile-time decomposition — report the win as a range with its mechanism, not a point
estimate (folds in #622 §3 + #621).** The compile-time multiplier is entirely the removed
`regex` dependency, and its magnitude is **config-sensitive**:
- **Leaf rustc cost is flat.** #622 measured the parser crate alone (deps warm): all
  variants compile in **0.82–1.13 s**, baked ≤ stock. The 68–235 KiB `static` table is
  cheap for rustc; it does **not** add compile time.
- **The removed dependency is the whole win, and LTO scales it.** #623's numbers use
  `lto = true`: dropping `regex` erases its compile *and* its LTO-IR reprocessing at the
  final link, giving **cold 18.0 → 4.4 s (4.1×)** / warm-relink 13.3 → 4.25 s. #621's
  clean build (deps included) measured **8.9 → 0.76 s (11.7×)**. Both are real; the spread
  (4–12×) is LTO config + what else is in the crate. **Honest read: leaf/rustc cost is
  flat; the 4–12× is the removed `regex` crate (its build + LTO IR), not the table.**

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

## After L5b the generated parser is **output-bound** (bridge to L5d, from #621)

Once the scanner is baked, it stops being the bottleneck. #621's Harness B decomposes the
baked reused parse (610 KB JSON, 67.8 ms): **scanning is only ~2 ms** (the baked table at
~300 MB/s), so the residual **~66 ms is LALR table-drive + owned-`Tree` materialization**
(per-node `String` label + `Vec<Child>` + owned token `String`s). This mirrors the
in-process finding exactly: after the lexer is fixed, the generated parser is
**allocation/output-bound**, and the next lever is L5d (interned labels + tape output),
*not* more scanner work. It is also why the standalone reused throughput is ~in-process
`parse()` parity, not a new tier — see the scope correction below.

## L5d — interned / tape output — **verdict: intern half CONFIRMED (re-measured on this branch); tape unmeasured**

**Re-measured on this branch** (the `baked_interned` emit variant — first found by #622,
whose measurement this reproduces). `standalone_bake_emit.rs` now emits a fourth variant:
the flat-table baked scanner **plus** the intern change — `Token::type_` and `Tree::data`
retyped `String → &'static str`, threaded through the runtime's `Tree`/`Frame`/`Display`,
and the three `.to_string()` copies dropped (`internize()`, a 6-anchor splice on the
generated source). Measured by the counting allocator in the Part B harness, 594 KB JSON:

| variant | allocs/parse | Δ vs baked | reused | digest |
|---|---:|---:|---:|---|
| baked (flat) | 948 031 | — | 8.3 MB/s | `b596…d98b` |
| **baked_interned** | **586 027** | **−362 004** | **9.9 MB/s (1.19× over baked, 1.86× over stock)** | `b596…d98b` (identical) |

- **The closed form holds exactly: −362 004 = `tokens_incl_EOI + shifted_tokens +
  tree_nodes` = 132 002 + 132 001 + 98 001.** Same number #622 reported, now confirmed on
  this branch's own harness.
- **Interning is *free* on the standalone surface** — symbol names and rule tree-names are
  already `&'static str` in the baked `DATA`, so `Token::type_`/`Tree::data` just stop
  copying them (no interner, unlike in-process M2). The rendered form is byte-identical, so
  the oracle tree digest is **unchanged** (`b596…d98b` across stock/baked/interned/classed)
  — the correctness gate that the `&'static` threading did not break output.
- **The `shifted_tokens` term is standalone-specific** (vs in-process M2's −1/node), and
  verified here against `src/standalone/runtime.rs`: `run` does
  `value_stack.push(NodeValue::Token(token.clone()))`, so a shifted token's `type_` `String`
  is allocated in `lex` *and again* on the clone onto the value stack — **two allocations
  per shifted token**, both removed by interning. (Credit #622 for the original observation.)
- Wall-clock **1.19× over baked** here — squarely in #622's 1.10–1.23× band; the residual
  owned `String` per token is then `value` (M6 territory), same as in-process.

**Tape (M5) remains unmeasured by all three spikes.** #620 can decide L5d's intern half now
(free, confirmed, exact, oracle-identical); the tape half rides #619's default-`Tree`
decision.

### Incidental out-of-scope find — filed as its own issue

**The standalone runtime has an O(n²) in flat-repetition (`tok+`) tree assembly** (filed as
#624, `bug`; linked from #620). A 95 KB input of `start: tok+` (~20 000 repetitions) parsed in
**~2.5 s** — scan-independent (stock ≈ baked ≈ classed), so it is the
`assemble`/transparent-recurse path rebuilding the growing child vector per reduction (the
standalone analog of the resolve-mode quadratic #55 fixed in-process). Orthogonal to #620,
**not a blocker** for the epic; it is why the `kw` grammar is a size/compile/correctness
data point only, not a throughput one, and it is the more urgent runtime issue any L5d tape
work must contend with.

---

## Scope correction — the "lalrpop speed band" framing is *not* established

An informal aspiration attached to this epic in conversation was that standalone baking
"gets us into lalrpop's speed band." **The spikes did not establish that, and this note
should not be read as if they did.**

- **Baked standalone reused throughput is ~5–9 MB/s** (594 KB JSON) — roughly in-process
  `parse()` parity (~7.5–8.5 MB/s), **not a leap into a new tier.** The bake fixed the
  *lexer*; per the output-bound decomposition above, the parse is now LR-drive +
  owned-`Tree`-bound.
- **lalrpop's speed comes from two levers this spike did not touch:** (a) generated /
  specialized LR *code* instead of an interpreted runtime `ParseTable`, and (b)
  typed-AST / action-code output instead of the generic owned `Tree`. Reaching that tier
  needs L5d/tape (only the intern half is measured, ~1.10–1.23×) **plus** LR-drive
  specialization (not scoped anywhere) **plus** an actual head-to-head bench (never run).
- **Where the standalone bake genuinely stands out is one-shot / many-small-inputs**
  (25–27×, the erased per-invocation `Regex::new`) — a *different axis* from raw
  throughput, and not a lalrpop comparison at all.

**Honest framing:** the epic's *actual* claims — one-shot economics + the footprint/compile
inversion — are confirmed and better-than-priced; the lalrpop-parity framing is
aspirational, untested, and a substantially bigger program than #620.

## Soft spots (the numbers less certain than the headlines)

- **Large-input reused parse ratio is the least-certain figure: ~1.3–1.8×.** #622 got
  **1.27×** on JSON where #623 and #621 got **~1.76–1.78×** — same lever, different harness
  framing (whole-corpus vs single-doc, alloc profile). The bimodal read is the honest one:
  the big-document win is modest and spread; the *one-shot* small-input win (25–27×) is the
  robust one.
- **The compile-time multiplier is config-sensitive** (leaf ~flat; 4–12× is the removed
  `regex` dep under LTO — see the L5b decomposition). Report it as mechanism + range, never
  a point estimate.

## Scope / degradation-policy findings (feed #620's shaping)

1. **Narrow beneficiary set, and a degradation-policy question (sharpened by #622).** Only
   JSON-shaped and tiny grammars both bake *and* basic-lex on today's wild bank. Worse, a
   grammar can generate and still be useless: **`poetry_pep508` bakes but 0/14 of its real
   corpus parses under the basic lexer**; **matter_idl loses 3/8** the same way (both are
   contextual-load-bearing). #620's "degrade gracefully per scanner" therefore needs a
   *front-door* policy too: should `generate_standalone` **warn or refuse** when a grammar
   is contextual-load-bearing, instead of silently shipping a parser that fails at runtime?
   Today it ships the failing parser (the basic-only limitation is documented but easy to
   miss).
2. **Capability angle (distinct from perf).** `tartiflette` DFA-flattens but the current
   `regex`-runtime standalone *rejects* it (a can't-host terminal); #622 lists five such
   RC10 grammars (lark_lark, mappyfile, pylogics_ltl, vyper, tartiflette). Baking the DFA
   would make these standalone-able **in principle** — but only with the guarded engine +
   guard tables baked too, beyond L5b's naive scope. This is a *capability* justification,
   separate from the throughput/footprint one #620 leads with.

---

## Bottom line for #620

| phase | standalone verdict | headline number |
|---|---|---|
| **L5a** leaner loop | **N/A as specified** — stock is regex-crate, not a DFA; win is only reachable via L5b | raw = 13.9× rx (2× the seam), but not shippable without the bake |
| **L5b** flat-table bake | **CONFIRMED, cost model inverted** | 1.76× large-input parse; **27× one-shot** small-input; **binary 4.1× smaller**, **compile 4.1× faster** (regex dep dropped); tree-identical to oracle |
| **L5c** footprint | **byte-class is the default** | ~5% throughput giveback for 1.5–4.9× smaller table; **×1 multiplier, 14–105 KiB/grammar**, dependency-free |
| **L5d** intern / tape | **intern half CONFIRMED** (re-measured here via `baked_interned`, first found by #622); tape unmeasured | intern is free + exact: **−362 004** allocs/parse (= tokens+shifted+tree_nodes), 1.19× over baked, oracle-identical |

**Net.** #620's two costed risks — rodata footprint and compile time — are **not costs on
the standalone surface; they are gains**, because baking a table lets the generated parser
drop the `regex` engine (the ×1 basic-lexer multiplier is what makes the table small
enough for this to hold; #623's regex-free `unless` retype makes it hold even for
ci-keyword grammars, where #622's baked binary otherwise *grew*). The value is real but
**bimodal**: modest and spread (**~1.3–1.8×**) for one large document, large (up to ~27×)
for the many-small-inputs CLI/embedded pattern where the per-invocation `Regex::new` is the
cost. **Two catches for the decision:** (1) the "lalrpop speed band" framing is *not*
established — baked reused throughput is in-process parity, and reaching lalrpop's tier
needs LR-specialization + typed-AST output this spike did not scope (see the scope
correction); (2) the beneficiary set is narrow — only JSON-shaped and tiny grammars both
bake and basic-lex, and some grammars (poetry_pep508, matter_idl) generate but fail at
runtime under the basic lexer, so `generate_standalone` may need a warn/refuse for
contextual-load-bearing grammars. The **lookaround-capability** unlock (baking the DFA to
accept grammars the `regex` runtime rejects — e.g. `tartiflette`) is a *different*
justification than the throughput/footprint one #620 leads with.

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
| `examples/standalone_bake_emit.rs` | emits stock + baked-flat + **baked-interned (L5d)** + baked-byte-class generated crates by splicing a static-table interpreter into the runtime, dropping `regex`, and (interned) retyping `type_`/`data` to `&'static str` |
| `examples/standalone_oracle_digest.rs` | in-process basic-lexer oracle digest (Part B correctness anchor) — *not* spike-gated |
| `examples/standalone_bake_partb.sh` | Part B driver: emit → compile 4 crates → binary size / compile time / one-shot / reused / **allocs-per-parse (counting allocator)** / digest gate |

Durable instrument reused verbatim from PR #617: `bake()` (BFS flatten + start-context
probe + delayed-match/EOI), the `spike_plain_dense` / `spike_retype` accessors
(`src/lexer/{dfa,mod}.rs`), and `raw_match_at`. The standalone additions are the `rx`
baseline, the byte-class rep, and the whole Part B generate→compile→measure loop.
