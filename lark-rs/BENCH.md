# lark-rs — Performance Baseline (recorded trend, not a gate)

This is the **performance analog of the correctness oracle**: a number for a
representation or algorithm change to move, established against the working LALR
engine *before* Earley (Phase 2) so the second engine has a baseline to be
measured against. It is deliberately **not a CI gate** — wall-clock on shared
runners is too noisy to enforce, and a flaky red perf gate gets muted. The
nightly `.github/workflows/lark-rs-bench.yml` records and uploads the numbers as a
trend; humans read regressions off the trend.

## Performance discipline (profile first — the #54/#55/#56 lesson)

Three rules, learned the hard way: #54 named a culprit (completer / Joop-Leo), #55's
profiler found a different one (the forest→tree walk), and #56 showed *both* halves
of a hypothesis can be wrong at once — its guessed explicit-walk culprit (a clone
loop) turned out linear, while a suspicion it had down-weighted (the completer
rescan) turned out real.

1. **Demonstrate before fixing.** A suspected super-linearity gets a committed,
   size-parametrized workload that *exhibits* it before any fix is written — the perf
   analog of "every bug reproducible as a test failure first" (`CLAUDE.md`).
   "Couldn't reproduce a pathology" is a valid, documented outcome (it closes the
   suspicion with evidence).
2. **Profile the root cause; don't guess it.** Fix the phase the profiler indicts,
   not the one a hypothesis names, and attach the profile to the change. #54
   attributed the growth to the completer; the cost was in the forest→tree walk.
3. **Regress on a deterministic signal, never wall-clock.** Gate on allocation-block
   counts (DHAT) or an instrumented copy/clone/rebuild counter, asserting *flat
   per-byte scaling* — not absolute time. Wall-clock on shared runners is too noisy
   to gate, and a flaky perf gate gets muted (the reason this whole bench is a
   recorded trend, not a gate).

## Deterministic scaling counters (the #56 gate)

Wall-clock is a recorded trend; the **gateable** signal is a set of deterministic
work counters in `lark_rs::perf`, compiled in only under the `perf-counters`
feature (zero overhead otherwise — the increments sit in the Earley hot path). They
make a suspected super-linearity reproducible as a *flat-per-byte* (or capped n²)
assertion that a shared runner can actually enforce:

```bash
# Demonstrate: print the counters across a size sweep for each #56 workload.
cargo run --release --features perf-counters --example profile_parse scaling
# Gate: the committed scaling regression net (CI runs this).
cargo test --features perf-counters --test test_earley_scaling
```

`completer_scan_steps` (Arm 1), `explicit_prefix_copies` (Arm 2, the *named* clone
loop — kept as a committed disproof that it is linear), and `explicit_node_children`
(Arm 2, the *real* O(n²) cost). Adding a new suspicion means adding a counter + a
sweep here, never a wall-clock threshold.

### Output-shape counters (semantic-output C5, #230)

The shared `TreeOutputBuilder` is the one place that materializes the output tree,
so three counters make the "the fast path builds the right *shape* of output" claim
falsifiable without wall-clock (ADR-0007):

* `tree_nodes_built` — every `Tree` node built (`build_node`).
* `token_value_string_bytes` — the owned token-value bytes copied into the output
  (`build_token`).
* `semantic_reduce_calls` — one per reduction shaped through `assemble`; for a known
  **LALR/CYK** input this equals the parser's user-rule reduction count (the augmented
  `$root` accept does not route through `assemble`). The Earley *resolve* walk shapes
  via `shape`/`shape_with_container` directly, so this counter does not track its
  reductions — unlike `tree_nodes_built`/`token_value_string_bytes`, which live in
  `build_node`/`build_token` and are engine-agnostic.

```bash
cargo test --features perf-counters --test test_output_counters
```

The gate pins the *default* `TreeOutputBuilder` backend: it **builds** trees
(`tree_nodes_built > 0`, `token_value_string_bytes > 0`), all three counters scale
with output shape over a size sweep, and `semantic_reduce_calls` equals the closed-
form reduction count for a known input.

**Eventual zero-tree gate (lands with C8, blocked on C7/#232).** A future tree-
bypassing span backend (`SpanTree`) must drive `tree_nodes_built == 0` and
`token_value_string_bytes == 0` for the same parse — the deterministic proof that
the fast path builds *no* tree and copies *no* token strings. Those two assertions
are NOT gated yet: the C3 fixture backend that landed (PR #261) walks an already-
built `ParseTree`, so it builds trees by construction. C5 lays the counter
infrastructure those C8 gates will key on (`token_value_string_bytes == 0` is the
same gate the #230 issue body already defers to C8).

## Running it

```bash
cd lark-rs
cargo bench --bench parse           # Rust LALR/Earley internal numbers + scaling
python3 tools/bench_compare.py      # Python Lark on parse.rs's JSON/arith grammars
cargo bench --bench vs_python_lark  # cross-engine JSON/Python/SQL/NL-CYK, prints the speedup
cargo bench --bench lex_backends    # lexer: regex Scanner vs regex-automata DfaScanner (L1)
cargo bench --bench wild            # wild-grammar bank: build + parse on real-world grammars
```

`wild` runs the real-world bank in `tests/wild/` (HCL2, mapfiles, GraphQL SDL,
PEP 508, MistQL, Storm, Vyper, Quil — see `tests/wild/README.md`) with each
project's own upstream Lark options: per project it reports single-shot build
cost plus corpus and largest-input parse throughput, and prints `SKIP` for
grammars lark-rs cannot build yet (the wild xfail set). First numbers worth
knowing (2026-06-10, dev box): wild LALR parse throughput lands at
6–14 MB/s on file-sized inputs; the mistql Earley + dynamic-lexer corpus is
~0.05 MB/s; and pyquil's corpus (tiny expression inputs, deep trees) shows the
small-input per-parse overhead that is part of the wild profile, unlike the
synthetic large-input workloads. The bench's input set self-updates as xfails
burn down: matter_idl's 100 KB/341 KB large-bucket files join its corpus row
the moment the `"optional"i` mis-lex is fixed. (Historical: pre-L4, mappyfile
built in ~1.5 s release vs Python's 0.13 s — a build-cost datapoint that
returns if its by-design refusal is ever revisited; cel benched ~0.3 MB/s on
tiny inputs before its upstream `{4-8}` typo became an L4 build refusal.)

`vs_python_lark` is the **cross-engine end-to-end comparison** (issue #50, the
"10–100×" headline) and is the single command that reports the speedup ratio: it
times lark-rs on four real workloads, then shells out to
`benches/vs_python_lark.py` to time the *byte-identical* inputs through the in-tree
Python Lark and prints `python_median / rust_median` per workload. See
"Cross-engine end-to-end" below. The `parse` bench and `tools/bench_compare.py`
remain the *internal* trend (LALR vs Earley, scaling shapes).

Both print the same columns; compare row-by-row:

```
BENCH<TAB>kind<TAB>name<TAB>bytes<TAB>median_ns<TAB>min_ns<TAB>mb_per_s
```

The `min_ns` column is the least-noise estimator (use it for run-to-run
comparison); `median_ns` drives the MB/s. Speedup on a row is
`python_median / rust_median`.

## The three comparisons (and what each means)

1. **Rust-LALR vs Python-Lark-LALR** — the defensible "faster than Python Lark"
   story. `tools/bench_compare.py` computes it on `parse.rs`'s grammars, and
   `cargo bench --bench vs_python_lark` reports it directly on the JSON/Python/SQL/NL
   workloads (see "Cross-engine end-to-end" below).
2. **Rust-Earley vs Python-Lark-Earley** — the same story for the second engine,
   now wired into `cargo bench --bench vs_python_lark` (JSON + SQL + python_sm;
   ~13× on JSON/SQL and ~43× on python_sm on the reference box, since Python
   Lark's Earley is much slower in absolute terms).
3. **Rust-Earley vs Rust-LALR** — the *cost of generality*, not "slowness."
   Earley is O(n³) worst case and solves a strictly harder problem; reading a
   cubic-Earley-on-pathological-input number as a regression against LALR is a
   category error. `cargo bench --bench parse` now wires this up: it re-runs the
   unambiguous workloads under `parser='earley'`, prints the per-row Earley/LALR
   ratio (`parse_earley` rows + a `ratio` line), and adds a reported-only
   pathological-ambiguous workload (`parse_earley_ambig`).

   **Reported, not gated — and the ratio was *not* a constant.** Sprint 2 originally
   meant to assert "...and within K× of LALR" here. Wiring the measurement up
   disproved that premise: the Earley/LALR ratio **grew with input size**
   (≈15×→32×→196× as JSON scaled 0.4K→8.7K→92K on the reference box). #55 fixed the
   **resolve-mode forest→tree walk** (two quadratics: copying the `Inline` child list
   of transparent left-recursive helpers, and deep-cloning each growing left subtree
   on memo), so the resolve-mode ratio on JSON/arith **stops growing with input
   size** — `earley_over_lalr_max` fell 311.8× → 17.9×. A single-K ceiling is still
   not asserted: wall-clock is too noisy to gate. The ratios are printed so the trend
   stays visible.

   **#56 — the residual suspicions, now resolved under the demonstrate-first
   discipline.** Each was taken through a committed, *deterministic* scaling artifact
   (`lark_rs::perf` work counters via `examples/profile_parse.rs scaling`, gated by
   `tests/test_earley_scaling.rs` — never wall-clock). The verdicts:

   - **Completer origin-column rescan — was real, now fixed.** The earlier "linear on
     JSON/arith" reading did *not* generalize: the completer rescanned the *whole*
     origin column with an O(column) `.filter` per completion, which is super-linear
     on a right-recursive grammar (`a: X a | X`) where later columns hold O(n)
     completed items. A per-column `waiting` index (expected-symbol → waiters) makes
     it O(matches); JSON/arith/nested/left-recursion now hold flat per-byte completer
     scan, gated. (So the old "the ratio grows *because* the completer rescans the
     origin column" claim was directionally right about the mechanism but was never
     verified — #56 verified and fixed it.)
   - **Right-recursion — linearized by Joop-Leo (#58).** Even with the index,
     `a: X a | X` stayed O(n²): non-Leo Earley builds O(n²) completed items
     regardless of the rescan (Python Lark still does — its Leo transitives are dead
     code, `create_leo_transitives` commented out; the upstream completer even
     references a nonexistent field, see lark-parser/lark#397). #58 implemented the
     Joop-Leo deterministic-reduction-path optimization with a lazy, reachability-
     bounded SPPF spine reconstruction over a forest-global `(key,start,end)` index.
     The forest drops from O(n²) to O(n) nodes. The gate now proves this **before
     vs after** on three grammars — the canonical `a: X a | X` plus two that people
     hand-write as right recursion and *cannot* express with `+` (a right-associative
     operator `?a: NAME "=" a | NAME` and a separated list `lst: ITEM "," lst | ITEM`,
     since `+` expands to flat *left* recursion): with the Leo toggle off the forest
     is super-linear (≥3× per doubling), with it on it is linear (≤2.3×). Wall-clock
     on the `=` chain (measured 2026-06-05, `--features perf-counters`): **17× @ n=256,
     38× @ n=512, 90× @ n=1024** (671 ms → 7.5 ms), the speedup growing ~linearly in
     n exactly as O(n²)→O(n) predicts. This is where lark-rs is now *faster than the
     Python oracle*. Restricted to strict right recursion (recognized symbol is the
     rule's last); nullable-tail recursion falls back to the regular completer.
   - **`ambiguity='explicit'` walk — guessed cause disproved.** The suspected culprit
     was `expand_packed`'s `l = list.clone(); l.push(rv)` loop. Measured, that loop is
     **linear** (its prefix is bounded by the rule arity). The genuine O(n²) is the
     per-node derivation-value rebuild in `symbol_derivations`: a transparent helper
     materializes Inlines of size 1,2,…,n — exactly the cost #55 streamed away for
     resolve, still present in explicit. Both are gated (loop stays linear; rebuild
     stays within its n² ceiling); the streaming fix is a **tracked follow-up**.

## Baseline snapshot

Machine-specific — capture fresh numbers on your own box; only **ratios and
trends** travel. Reference run:

- `Linux x86_64`, 4 cores, `rustc 1.94.1`, in-tree Python Lark, release + LTO.

| workload | bytes | Rust median | Python median | speedup |
|----------|------:|------------:|--------------:|--------:|
| build json        |   462 |  4.41 ms |  12.5 ms | ~2.8× |
| build arithmetic  |   462 |  6.12 ms |  11.9 ms | ~1.9× |
| parse json_small  |  ~390 |  0.11 ms |  0.54 ms | ~4.8× |
| parse json_medium | ~8.7K |  2.31 ms | 10.96 ms | ~4.7× |
| parse json_large  | ~92K  | 26.9 ms  | 118.8 ms | ~4.4× |
| parse arith_small |    33 |  0.02 ms |  0.11 ms | ~5.7× |
| parse arith_large | ~2.1K |  1.21 ms |  6.22 ms | ~5.1× |

**Reading of the baseline.** lark-rs LALR is currently ~4–5× faster than Python
Lark on parsing — real, but short of the project's "10–100×" headline. The gap is
expected headroom: the deliberately-deferred optimizations (see `CLAUDE.md` "defer
deliberately") have not been done, and parse throughput (~3.5 MB/s) is
allocation-bound, not algorithm-bound — now **measured**, not assumed (see below).
This harness is what makes that headroom measurable and turns each future
optimization into a tracked delta.

### Hot-path allocation/dispatch pass (2026-07-01, E1/E2/E3/E5)

A profile-first pass on the LALR hot path (`docs/notes/perf-spike-2026-07-01.md`)
removed the per-token clones (the token was materialized 3×), the four per-token
SipHash probes (terminal name, state→scanner, `%ignore`, `unless` — all now dense
arrays), the per-reduction `drain().collect()`, and the per-reduction child-buffer
growth reallocs. All engine-internal; no public type changed. Measured back-to-back
against `master` on one shared box (same caveat — **only ratios travel**), `min_ns`:

| workload | master | after | ratio |
|----------|-------:|------:|------:|
| parse json_small  | 60.8 µs | 42.3 µs | **1.44×** |
| parse json_medium | 1.46 ms | 0.93 ms | **1.58×** |
| parse json_large  | 20.24 ms | 11.73 ms | **1.73×** |
| parse arith_small | 12.7 µs | 7.2 µs | **1.76×** |
| parse arith_large | 791 µs | 404 µs | **1.96×** |

Geomean ~1.68× on the synthetic LALR workloads; the fuller spike also measured
1.44–1.71× on the wild-bank real-world LALR grammars. The one remaining public-
surface lever — `u32` positions shrinking the value-stack element 264 → ~168 B —
is escalated separately (ADR-0040). The `examples/parse_floor.rs` NullBuilder
instrument shows ~47–60% of `parse()` is still output materialization: the target
for the tape/arena backends (#242/#243).

## Cross-engine end-to-end: JSON / Python / SQL / NL-CYK (issues #50, #87)

`cargo bench --bench vs_python_lark` is the **cross-engine comparison** — the
number behind the project's "10–100×" goal — over four real workloads. It is the
throughput analog of the oracle: lark-rs and Python Lark parse the **same grammar**
over the **same bytes**, so the ratio is apples-to-apples. JSON / Python / SQL run
on **LALR + the contextual lexer** (Lark's primary USP); JSON, SQL, and a bounded
Python workload (`python_sm`, issue #78) *also* run on **Earley**, and the NL
workload runs on **CYK** — so all three engines have a cross-engine number (see
"Earley arm" and "CYK arm" below).

- **JSON** — the canonical JSON grammar over a ~92 KB array of records (the
  `json_large` shape from `parse.rs`).
- **Python** — the **real upstream `python.lark`** (issue #79), driven by the
  `Indenter`/`PythonIndenter` postlex hook over a generated source file that
  exercises the full language: classes + decorators, `async def`/`await`/`async
  with`/`async for`, list/dict/set comprehensions, the walrus `:=`, f-strings,
  `*args`/`**kwargs`, `lambda`, ternary, `try`/`except`/`finally`, `with`, slices,
  `del`/`assert`/`while`, augmented + annotated assignment, and `import`s. This is
  no longer a curated subset: lark-rs and Python Lark load the *same* in-tree
  `python.lark` (start `file_input`) and parse the byte-identical input. It became
  parseable end-to-end once #98 (EBNF-helper dedup → builds under LALR), #97/#100
  (leading-nullable distribution → parses), and the named-keyword-terminal
  `PatternStr` fix (async/await) landed; the lookaround terminals route to
  `fancy-regex` (#40). One construct stays off the generator — star-params *after*
  a positional in a `def` header (`def f(self, *a)`), which lark-rs's LALR table
  does not yet accept where Python Lark does — so def-site `*args`/`**kwargs` is
  exercised via a no-positional top-level function (call-site unpacking, which both
  accept, is used everywhere else). The bench both *builds* and *parse-checks* the
  workload on each engine before timing, so any divergence fails loudly.

  > **Perf note (2026-06-06): an O(n²) lexer pathology, found and fixed by this
  > swap.** Swapping in the real `python.lark` first exposed a quadratic: the
  > `fancy-regex`-routed lookaround terminals (`STRING`/`LONG_STRING`/`DEC_NUMBER`)
  > were matched per position with `find_from_pos`, an *unanchored forward search*,
  > so trying a sparse terminal like `STRING` at every offset scanned ahead to the
  > next quote — O(n²) over the file (a 124 KB parse took ~177 s; JSON/SQL, which
  > use no lookaround terminals, were unaffected, which is what localized it). The
  > fix anchors the per-position fancy match to the search start with `\G`
  > (`src/lexer/scanner.rs`, `Scanner::build`), so the search fails immediately when nothing
  > matches at `pos`. Behaviour-preserving by construction — `match_end_at` already
  > required `m.start() == pos`, so the match set is identical, only the forward
  > scan is gone — and verified green across the full compliance/oracle/stdlib
  > suite. Parsing dropped from ~177 s to ~0.24 s on the 124 KB workload and is now
  > linear per byte. (Follow-up: a committed deterministic lexer-scan-step gate, the
  > analog of the Earley/CYK scaling nets, would pin this so it can't silently
  > regress — the current net is only this wall-clock row.)
- **SQL** — a `SELECT`/`INSERT`/`UPDATE`/`DELETE` grammar (joins, `WHERE`,
  `GROUP BY`, `ORDER BY`, `BETWEEN`/`IN`) over a batch of statements.
- **NL** (CYK) — a small ambiguous natural-language grammar (PP-attachment +
  coordination) over one short sentence. This is the *realistic* CYK/CKY use case:
  unlike JSON/SQL/Python (all LALR-parseable), it genuinely needs a general-CFG
  engine, and parse count grows ~Catalan in the number of prepositional phrases.
  Bounded to ~40 tokens since CYK is O(n³·|grammar|) — the niche/last-resort
  backend, deliberately stressed but kept small.

The grammars live byte-identical in `benches/vs_python_lark.rs` and
`benches/vs_python_lark.py`. The Rust harness generates each input once, writes it
to a temp dir, and passes it to the Python script with `--inputs`, so both engines
time the exact same bytes (no generator-drift risk). Both sides assert the
workload parses before timing, so grammar drift fails loudly rather than silently
measuring an error path.

### Earley arm (JSON + SQL + python_sm)

JSON and SQL also run under `parser='earley'` — the second engine — so the
"Rust-Earley vs Python-Earley" comparison has a number, not just the
"Rust-Earley vs Rust-LALR" cost-of-generality one in `parse.rs`. The lexer is the
one each workload needs under Earley: **JSON → basic**, **SQL → dynamic** (the
basic lexer can't tell the assignment `=` from the comparison `=` in the SQL
grammar — true in *both* engines, so it's a fair constraint, not a lark-rs gap).

**`python_sm`** is the Earley row for the significant-whitespace workload
(issue #78): the real upstream `python.lark` + the `Indenter` postlex hook over
the **basic** lexer — the one Earley + postlex configuration both engines support
(both refuse postlex with the dynamic lexer; Python Lark's `lexer='auto'`
resolves to `'basic'` for Earley + postlex too). It is the same generator as the
LALR `python` workload **bounded to a few classes (~6 KB)**: Python Lark's Earley
measures ~0.001 MB/s on this grammar, so the full 125 KB input would take ~10
minutes *per iteration* on the Python side.

### CYK arm (NL) — issue #87

The **NL** workload runs under `parser='cyk'` on both engines, the like-for-like
"Rust-CYK vs Python-CYK" comparison. It is the one workload here whose grammar
genuinely *needs* a general-CFG parser: JSON/SQL/Python are all LALR-parseable, so
running them under CYK would be a contrived choice, whereas an ambiguous
phrase-structure grammar (PP-attachment) is the textbook realistic CYK/CKY use
case. Both engines run the **same** CNF conversion + O(n³) DP (lark-rs's `cyk.rs`
is a faithful port of Python Lark's `cyk.py`), so the ratio is a clean
implementation-vs-implementation number, not an algorithm difference.

The input is a single ambiguous sentence (~40 tokens), not a file — CYK is the
niche/last-resort backend and is `O(n³·|grammar|)`, so the arm is deliberately
small (the bound the issue prescribes). The deterministic *shape* of that cubic
cost is separately gated by `tests/test_cyk_scaling.rs` (#87); this arm is the
throughput trend.

### Reference run

Machine-specific — **only ratios travel**; capture fresh numbers on your own box.

- `Linux x86_64`, Intel Xeon @ 2.80 GHz, 4 cores, `rustc 1.94.1`, release + LTO.
- **Python Lark 1.3.1**, CPython 3.11.15 (the in-tree copy). LALR rows use
  `lexer='contextual'`; Earley rows use `lexer='basic'` (JSON, python_sm) /
  `'dynamic'` (SQL). Measured 2026-06-12 (full re-run — first measurement since
  the DFA lexer backend became the default and postlex landed on Earley, #78).

| engine | workload | bytes | Rust MB/s | Python MB/s | speedup |
|--------|----------|------:|----------:|------------:|--------:|
| LALR   | JSON      | ~92 KB  | ~5.4  | ~0.7   | **~7.2×** |
| LALR   | Python    | ~122 KB | ~1.5  | ~0.2   | **~6.7×** |
| LALR   | SQL       | ~57 KB  | ~3.6  | ~0.6   | **~5.6×** |
| Earley | JSON      | ~92 KB  | ~0.5  | ~0.04  | **~12.9×** |
| Earley | SQL       | ~57 KB  | ~0.1  | ~0.007 | **~13.3×** |
| Earley | python_sm | ~6 KB   | ~0.06 | ~0.001 | **~43.2×** |
| CYK    | NL        | ~186 B  | ~0.5  | ~0.02  | **~26.9×** |

**Reading.** Three separate stories:

- **LALR** — lark-rs is ~6–7× faster than Python Lark across all three workloads.
  The **Python** row used to be the honest outlier (~1.8× when its
  `STRING`/`LONG_STRING`/`DEC_NUMBER` terminals routed through `fancy-regex`);
  with the DFA backend lowering those lookarounds into the combined scanner
  (`docs/LEXER_DFA_PLAN.md`, L4) it now sits with the others (~6.7×, and ~3×
  faster in absolute MB/s than the old fancy-routed number). The general gap to
  the "10–100×" headline is the deliberately-deferred tree-representation work
  (`Box<str>`/arena labels, zero-copy spans — see the profiling findings below;
  parse throughput is allocation-bound, ~3 allocations per input byte, not
  algorithm-bound).
- **Earley** — the margin is *larger* (~13× on JSON/SQL, ~43× on python_sm),
  because Python Lark's Earley is dramatically slower in absolute terms (multiple
  seconds per parse here) while lark-rs's Earley stays in the tens-to-hundreds of
  ms. This is the second engine paying its cost-of-generality (Earley is much
  slower than LALR *within* lark-rs too — see `parse.rs`), but doing so far more
  cheaply than the reference implementation. The gap is widest on the two
  heaviest configurations: SQL's dynamic lexer and python_sm's big-grammar
  significant-whitespace parse (the row issue #78 made possible — neither engine
  could run Earley + postlex before).
- **CYK** — the widest margin (~29×). Same story as Earley but more pronounced:
  Python Lark's pure-Python CYK DP pays a large constant factor per table cell,
  while lark-rs's port keeps the same O(n³) shape in native code. The absolute
  numbers are tiny (a ~40-token sentence), so read this as "the general-CFG
  fallback is not a Python-speed cliff in lark-rs," not a throughput headline — CYK
  remains the last-resort backend.

This bench turns that remaining LALR headroom into a tracked delta: **re-run it
after each significant engine change and update the table.**

## Wild bank vs Python Lark (cross-engine, 2026-06-10)

`tools/bench_wild_python.py` is the Python-Lark side of `cargo bench --bench wild`:
it replays every wild project through the in-tree Python Lark with the exact
upstream options from each `meta.json`, timing build (single shot, like wild.rs)
and **each input individually** — so the analysis can aggregate exactly the
subset of inputs lark-rs parses (the Rust bench filters its corpus to inputs
that parse) and the two engines compare over byte-identical input sets.

```bash
cargo bench --bench wild > wild_rust.out
python3 tools/bench_wild_python.py lark_lark matter_idl mistql poetry_markers \
    poetry_pep508 pylogics_ltl pyquil tartiflette > wild_python.json
```

Reference run (shared runner, Linux x86_64, release+LTO; in-tree Python Lark,
CPython 3.11 — **only ratios travel**). Corpus rows are the same byte-identical
input sets on both engines (xfail inputs excluded from both):

| project | engine | corpus bytes | Rust ms | Python ms | parse speedup | Rust build | Py build | build ratio |
|---------|--------|---:|---:|---:|---:|---:|---:|---:|
| lark_lark      | LALR | 22.9K | 7.16 | 38.9 | **5.4×** | 241 ms | 55 ms | **0.2×** |
| matter_idl     | LALR | 2.4K | 0.36 | 3.9 | **10.8×** | 1380 ms | 118 ms | **0.1×** |
| mistql         | Earley/dyn | 1.4K | 36.9 | 1174 | **31.8×** | 2 ms | 40 ms | 18× |
| poetry_markers | LALR | 639 | 0.07 | 0.61 | **8.2×** | 23 ms | 14 ms | 0.6× |
| poetry_pep508  | LALR | 593 | 0.08 | 0.66 | **8.4×** | 35 ms | 29 ms | 0.8× |
| pylogics_ltl   | LALR | 63 | 0.09 | 0.64 | **7.1×** | 122 ms | 59 ms | 0.5× |
| pyquil         | LALR | 9.3K | 2.09 | 13.9 | **6.7×** | 962 ms | 219 ms | **0.2×** |
| tartiflette    | LALR | 32.5K | 4.92 | 59.6 | **12.1×** | 592 ms | 108 ms | **0.2×** |

**Reading.**

- **Parse: LALR geomean 8.1× (range 5.4–12.1×), Earley/dynamic ~32×.** The
  "10–100×" headline does **not** hold on real-world LALR grammars — wild
  parsing sits at 5–13×, consistent with the synthetic 4–5× plus the wild
  grammars' deeper trees. Earley reaches the headline's low end because Python
  Lark's Earley is so much slower in absolute terms.
- **Build: lark-rs was up to 12× *slower* than Python Lark** on every
  medium-or-larger LALR grammar — the wild bank's headline finding, **now
  fixed** (see below). Attribution
  (`cargo run --release --example wild_build_cost <project>`): the
  **contextual lexer's per-state scanner construction was ~95% of build
  time** (matter_idl: 1348 ms contextual vs 77 ms basic lexer; pyquil 963 vs
  49; tartiflette 575 vs 24 — both scanner backends pay it, the dense DFA
  slightly more than `regex`). Two gaps vs Python Lark's `ContextualLexer`:
  1. **No dedup** — `ContextualLexer::new` built one scanner per LALR state;
     Python keys lexers by `frozenset(accepts)` and shares them. Measured on
     the same grammars, that dedup is 4–5×: matter_idl 393 states → 86
     distinct terminal-sets, pyquil 540 → 108, tartiflette 315 → 81,
     lark_lark 118 → 47.
  2. **Eager construction** — Python's per-state `BasicLexer` compiles its
     scanner lazily on first use, so states never visited (most of them, for
     a typical input) cost nothing.

  **Fix landed (2026-06-10, this branch).** `ContextualLexer` now dedups
  states by sorted terminal-id key and builds each distinct scanner lazily on
  first use (a `OnceCell` per set). Terminal validation stays at construction
  time — `new` eagerly builds (and discards) the full-terminal scanner, the
  exact analog of Python's eager `root_lexer` init, so the categorized
  lookaround scope errors (`docs/LOOKAROUND_SCOPE.md`) still fail the build,
  not the parse. Full suite green (compliance banks, differential, wild).
  Wild builds re-measured (same shared runner): matter_idl 1380 → **101 ms**,
  pyquil 962 → **51 ms**, tartiflette 592 → **39 ms**, lark_lark 241 →
  **21 ms**, pylogics_ltl 122 → **22 ms** — i.e. contextual build cost is now
  ≈ basic-lexer build cost, and **every wild project now builds 1.2–4.3×
  faster than Python Lark** (parse throughput unchanged). The remaining build
  cost is the loader + LALR table, not the lexer.
- **Per-token allocations** remain the parse-side ceiling (the 2026-06-04
  profiling finding): `next_token` clones the terminal *name* `String` per
  token (`names[&id].clone()` — pure waste, the token already carries
  `type_id`; the map lookup is also SipHash where a dense `Vec` index would
  do) and allocates `value.to_string()` where a zero-copy span would do. The
  name-clone is removable without the deferred tree-representation rework.

## Lexer backends: regex Scanner vs regex-automata DfaScanner (L1)

`cargo bench --bench lex_backends` times the **lexer in isolation** (`BasicLexer::lex`)
under each of the two combined-scanner engines behind the `ScannerBackend` seam
(`src/lexer/mod.rs`): the original `regex`-crate `Scanner` (combined alternation +
capture groups) and the L1 `DfaScanner` (a `regex-automata` multi-pattern DFA over
the plain terminals, `docs/LEXER_DFA_PLAN.md`). The two are *correctness*-identical —
the L0 differential oracle (`tests/test_scanner_differential.rs`) is the gate — so
this is purely the throughput comparison the plan calls for, isolating the scanner
from parsing. It prints each backend's MB/s and the `dfa / regex` ratio (<1.0 = DFA
faster).

The DFA wins on the all-plain path for two structural reasons: it returns a bare
`PatternID` (no capture-group tracking — the per-token cost the 2026-06-04 profiling
spike below localized), and it searches **anchored at `pos`** (never forward-scans).
The plan's "re-add a literal prefilter so the common path doesn't regress" worry is
addressed by an explicit start-byte prefilter on `DfaScanner` (and the measured
common path *improves*, it does not regress). On a grammar dominated by the
`fancy-regex` lookaround side-probe (`python.lark`'s `STRING`/`LONG_STRING`), both
backends share that probe, so the ratio is ~1.0 — the swap neither helps nor hurts
the part it doesn't touch.

> **Staleness note (2026-06-10).** The "shared probe, ratio ~1.0" premise above (and
> the `python_8k` row's reading below) describes the recorded runs of 2026-06-08/09.
> Since the Stage-B idioms + the flag-wrapper strip landed, the **Dfa** backend lexes
> `python.lark` fully lowered (zero fancy side-probes) while the `Regex` reference
> still pays them — the workload now measures lowered-vs-fancy, and the python ratio
> is expected to move in the Dfa backend's favor on the next recorded run.

### Reference run

Machine-specific — **only ratios travel**; capture fresh numbers on your own box.

- `Linux x86_64`, release + LTO (the `bench` profile). Measured 2026-06-08.

| workload | bytes | regex MB/s | dfa MB/s | dfa/regex |
|----------|------:|-----------:|---------:|----------:|
| json_small  |   390 |  ~8.2 | ~22.5 | **~0.36×** |
| json_medium | ~8.7K |  ~8.7 | ~23.9 | **~0.36×** |
| json_large  |  ~92K |  ~9.7 | ~20.6 | **~0.47×** |
| expr_small  |   385 |  ~8.0 | ~12.1 | **~0.66×** |
| expr_large  |  ~28K |  ~9.6 | ~14.6 | **~0.66×** |
| python_8k   | ~8.0K |  ~0.5 |  ~0.5 | **~0.97×** |

### Default-flip re-run (DFA is now the default)

Re-run on the **flip** that makes `LexerBackend::Dfa` the default
(`LexerBackend::default()` / `LarkOptions.lexer_backend`). Same box family, the
`bench` profile (release + LTO). Measured 2026-06-09 — a shared runner, so the
absolute MB/s sit lower than the 2026-06-08 reference box; **only the ratios
travel**, and they reproduce the picture (DFA decisively faster on the all-plain
common path, a wash on the `fancy-regex`-dominated `python.lark`).

| workload | bytes | regex MB/s | dfa MB/s | dfa/regex |
|----------|------:|-----------:|---------:|----------:|
| json_small  |   390 |  ~6.2 | ~19.3 | **~0.32×** |
| json_medium | ~8.7K |  ~6.7 | ~21.6 | **~0.31×** |
| json_large  |  ~92K |  ~7.1 | ~22.9 | **~0.31×** |
| expr_small  |   385 |  ~6.1 | ~10.1 | **~0.60×** |
| expr_large  |  ~28K |  ~7.2 | ~12.7 | **~0.57×** |
| python_8k   | ~8.0K |  ~0.4 |  ~0.4 | **~0.96×** |

**Reading.** On all-plain grammars the DFA scanner is ~1.7–3.2× faster (JSON ~3.2×,
the identifier/number/operator stream ~1.7×) — the lexer is ~55% of LALR parse time
(profiling spike below), so this is a real end-to-end lever, not a micro-win. On
`python.lark` the shared `fancy-regex` `STRING` probe dominates both backends
(~0.4 MB/s either way), so the plain-engine swap is a wash there (ratio ~0.96×) —
exactly as expected, and the throughput-and-bakeability payoff for *those* terminals
is what later phases (L3 lowering, L5 baking) deliver. Because the swap is
correctness-identical (the L0 differential oracle is 0 divergences over the full
bank + JSON + python/lark corpora) and never slower, **`LexerBackend::Dfa` is now the
default**; `LexerBackend::Regex` remains selectable
(`LarkOptions.lexer_backend = LexerBackend::Regex` / `LexerConf::with_backend`) and
the differential keeps both engines gated against each other.

## Profiling findings (spike, 2026-06-04)

A one-off spike with `valgrind --tool=callgrind` (per-function instruction cost)
and `--tool=dhat` (allocations), on `json_large` (~92 KB), build with debug symbols
and LTO off. Reproduce with the committed `examples/profile_parse.rs`:

```bash
CARGO_PROFILE_RELEASE_DEBUG=true CARGO_PROFILE_RELEASE_LTO=false \
  cargo build --release --example profile_parse
valgrind --tool=callgrind ./target/release/examples/profile_parse parse 10
valgrind --tool=dhat      ./target/release/examples/profile_parse parse 1
```

**Headline: it is allocation-bound, decisively.** One parse of a 92 KB input does
**~301K allocations / 105 MB of churn** (≈3 allocations per input byte, >1000× the
input size). In the instruction profile, ~40% of all instructions are in
`memcpy` + `malloc`/`free`, and another ~10% in SipHash (`hash_one`).

**Where the time goes (inclusive, callgrind):**

| region | share | what |
|--------|------:|------|
| lexing (`Contextual::peek` → `next_token` → `Scanner::match_at`) | **~55%** | dominated by the `regex` engine + capture handling, *not* our logic |
| reduce / tree-building (`reduce` → `TreeBuilder::assemble` → `Tree::new`) | **~32%** | `String` clones, `Tree` label + children `Vec` allocation |

**Two concrete, localized root causes in the lexer** (`src/lexer/`, the backends' `match_at`),
both **shared by the future Earley engine** (it lexes through the same
`TokenSource`/`Scanner`) — **both now FIXED (perf sprint, 2026-06-04):**

1. ✅ **Capture group resolved by *name* per token.** `match_at` looped over groups
   calling `caps.name(group)` (string-keyed → SipHash) on every token — the ~2.5M
   `hash_one` calls. Fixed: each terminal's capture-group *index* is resolved once
   at `Scanner::build` (from `re.capture_names()`, robust to inner groups in a
   terminal's own pattern) and read by number in `match_at`.
2. ✅ **A fresh `Captures` allocated per match.** `captures_at` made the regex
   backtracker `malloc` per token. Fixed: a single `CaptureLocations` scratch
   buffer (held in the `Scanner` behind a `RefCell`, since the hot contextual path
   runs through `&self`) is reused across matches via `captures_read_at`.

**Measured result (same box, `examples/profile_parse`).** Allocations per
`json_large` parse fell **300,957 → 271,892 blocks** (DHAT), and the per-token
SipHash group-name lookups are gone entirely. End-to-end this is a **~17–20%
wall-clock speedup** on the contextual LALR path across every parse workload
(e.g. `json_large` 27.8 → 22.9 ms, ~3.3 → 4.0 MB/s; `arith_large` 1.21 → 0.97 ms),
lifting the speedup-vs-Python column accordingly. No public type changed; the full
oracle suite + compliance bank stayed green. The remaining lexer cost is now the
`regex` engine itself, not our capture handling.

**The other ~32% is the shared tree representation** — `Tree::data: String`,
`Token` owned strings, per-node child `Vec`s. This is the "load-bearing
abstraction" change (`Box<str>`/arena labels, zero-copy spans) that `CLAUDE.md`
defers behind the `TreeBuilder` chokepoint until a profiler justifies it. It now
does — but it is the change best made once Earley is a second consumer of that
representation. The opt-in `SpanTree` backend (C8/C8.1, #233/#582) is the first
slice of it, and `examples/span_alloc.rs` is the re-runnable before/after:

```bash
cargo run --release --features "span-tree,perf-counters" --example span_alloc
```

On a 594 KB JSON (dev box), `parse_span()` drives the lexer/output/tree-node
counters to `0` and removes **~41% of real heap allocations** (≈2.2 → 1.3
allocs/byte) for **≈1.3× wall-clock**. Alloc *bytes* barely move (~3%): the removed
allocations are many but tiny (token values, short labels); the byte *volume* is
dominated by working buffers both paths share (parser stacks, the token `Vec`, the
not-yet-reused per-node child `Vec` #583 tracks). That remaining volume is what the
arena/`Tape` follow-ups (#242/#243) target.

### The per-node child-`Vec` cost, isolated (`child_vec_alloc`)

The 2026-07 parser-optimization literature study
(`docs/notes/parser-optimization-research-2026-07.md`, Part 3 / finding M5) noted
that *no external source* isolates the per-node child-`Vec` cost — but lark-rs can,
because `SpanTree` and `TapeTree` are the same zero-copy parse differing **only** in
the child-list representation (`SpanBranch` owns one `children: Vec` per internal
node; `TapeTree` appends every node's children to one shared flat `kids[]`). With a
counting allocator, `allocs(parse_span) − allocs(parse_tape)` is therefore exactly
the child-`Vec` traffic. `examples/child_vec_alloc.rs` is the re-runnable measurement:

```bash
cargo run --release --features "span-tree,tape-tree,perf-counters" \
  --example child_vec_alloc [records]
```

Result (dev box, 594 KB JSON, 98 001 internal nodes): the child-`Vec` cost is
**exactly 1.000 allocations per internal node** and **flat** across a 43× size sweep
(0.997 @ 56 KB → 1.000 @ 594 KB → 1.000 @ 2.4 MB) — the "one `Vec` per node,
independent of size" the flattening literature (Sampson, *Flattening ASTs*)
predicts. That single per-node allocation is **~96% of `parse_span()`'s remaining
allocations** (0.165 of 0.172 allocs/byte), and the flat `kids[]` arena removes it:
`parse_tape()` lands at **0.007 allocs/byte** (O(log n) allocator calls) for a
**~1.76× wall-clock** trend over `parse_span()`. Both paths hold the zero-copy
invariant (`tree_nodes_built == 0`, `token_value_string_bytes == 0`), so the delta
is a clean isolation, not a confound. Conclusion: the per-node child `Vec` — not the
token strings or labels — is the dominant residual allocation on the span path, and a
shared flat child arena (the `Tape` direction, #243) is the lever that removes it.
This is a recorded measurement, not a gate (ADR-0007); the deterministic child-buffer
*gate* is `tests/test_child_vec_scaling.rs` on the `parse_into` path.

**Sequencing implication.** The single cheapest, highest-leverage, lowest-risk win
was the lexer pair (1)+(2): it attacks the larger (~55%) half, is purely local to
`Scanner`, touches no public type, and benefits both engines — so it was safe to do
*before* Earley. ✅ **Landed (perf sprint, 2026-06-04)** — see the measured result
above. The tree-representation half is still deferred until Earley exists to
co-design it (see `PHASE_2_PLAN.md` §10).

## Adding a workload

**Internal trend** (`parse`): edit `benches/parse.rs` (Rust) and mirror it in
`tools/bench_compare.py` (Python) so the rows line up. Keep generators
size-parameterized so a workload can scale to expose super-linear behavior. The
Earley workloads (the unambiguous grammars re-run under `parser='earley'`, plus a
pathological ambiguous grammar) light up when the engine lands.

**Cross-engine** (`vs_python_lark`): add the grammar + a size-parameterized
generator to **both** `benches/vs_python_lark.rs` and `benches/vs_python_lark.py`,
keeping the grammar strings byte-identical, then add the workload name to the three
arrays in each `main()`. The Rust harness writes the generated input to a temp file
and the Python script reads it via `--inputs`, so the two engines always time the
same bytes even if the generators ever drift.
