# Spike: validating the 2026-07 parser-optimization hypotheses (measurements)

*2026-07-03. Throwaway measurement spike — turns the ranked recommendations in
[`parser-optimization-research-2026-07.md`](parser-optimization-research-2026-07.md)
("Suggested benchmark tasks") into real before/after numbers. No engine code
changed; no public API changed. Every number below is reproducible from the
committed examples. Per `BENCH.md`: deterministic counters (allocation counts)
are the headline signal, wall-clock is a same-session trend only.*

## TL;DR

| # | Hypothesis | Verdict |
|---|---|---|
| 1 | Baked/directly-executable lexer beats the interpreted `regex-automata` DFA (B1) | **Confirmed, strongly** — ~12x scanner-only throughput on a real (if simple) grammar |
| 2a | Label interning to `u32` on the default owned tree (M2) | **Confirmed, cleanly** — exactly 1 allocation removed per node (~17.5% of default's total allocs), ~1.3x wall-clock |
| 2b | Flat `kids[]` child arena generalizes from the zero-copy tree (child_vec_alloc) to the **owned** tree (M5) | **Confirmed** — a further ~21% allocation cut on top of interning (~35% total vs default), ~1.7x wall-clock |
| 2c | `SmallVec`-inline child list as a third data point (M5 alternative) | **Killed** — a plain recursive-by-value tree needs `Box` to break the size cycle, which cancels the inlining win; net allocs are *worse* than the baseline `Vec` |

All four experiments hold their per-node / per-byte ratios flat across a
1x/10x/40x input-size sweep (200/2000/8000 JSON records) — not an artifact of
one input size.

---

## Experiment 1 — Baked/directly-executable lexer vs `regex-automata` DFA (B1)

**File:** `examples/spike_baked_lexer.rs` — `cargo run --release --example spike_baked_lexer`

### Method

The research note's top-ranked, least-risky lever: RE/flex's `--fast`/`--full`
modes bake a DFA into generated goto/switch code instead of interpreting a
generic transition table at match time. lark-rs's default lexer
(`DfaScanner`/`dfa.rs`) drives a `regex-automata` `dense::DFA` through the
generic `Automaton` trait — every byte pays an equivalence-class lookup, a
transition-table index, and a trait call. A hand-written, grammar-specific
byte-at-a-time scanner for the JSON grammar (`benches/lex_backends.rs`'s
grammar, no lookaround — a "plain-DFA" workload) stands in for what a real
baked-DFA emitter would produce (a "quick-and-dirty ... table for ONE grammar",
per the research note — not a general emitter).

**A methodology correction worth recording.** The first draft of this spike
compared the baked scanner against `Lexer::lex()` (which also builds owned
`Token`s — two `String` allocations plus char-by-char position tracking) and
got a **~30-34x** ratio. That number is not "baked vs interpreted DFA" — it's
"a raw byte scan vs. a raw byte scan *plus* token construction," conflating two
different questions. The fixed comparison drives `BasicLexer::match_at` (the
raw per-position scanner seam, no `Token` built) on the DFA side, in the exact
same loop shape as the baked scanner — apples-to-apples, both sides doing
*only* scanning. Both scanners are also asserted to consume the whole input and
agree on token count, so the ratio isn't hiding a correctness gap.

### Result (scanner-only, reused lexer — build once, scan N times)

| workload | bytes | dfa MB/s | baked MB/s | baked/dfa |
|---|---:|---:|---:|---:|
| json_small  |   390 |  57.7 |  688.3 | **11.9x** |
| json_medium | 8,749 |  61.8 |  717.5 | **11.6x** |
| json_large  | 92,080 | 65.1 |  763.9 | **11.7x** |

One-shot build cost: baking is a compile-time artifact (a plain Rust function —
literally zero runtime construction cost), vs. `DfaScanner::build` at ~2.9-3.1
ms for this grammar. This side of the ledger is true by construction for a
hand-baked scanner and isn't the interesting number; the reused-throughput
column above is (lark-rs's headline use is build-once-parse-many).

For context (not compared to baked, since it does strictly more work): the
DFA's *full* `Lexer::lex()` — Token construction included — lands at 23.5-25.7
MB/s on the same inputs, ~2.7x slower than its own scanner-only number. Token
construction/allocation is real cost, but scanning still dominates the ratio to
baked.

### Verdict

**Confirmed, and the effect is bigger than the literature's own (unverified)
number** (Pfahler 1990's ~7x, never independently checked — see the research
note). A specialized byte-matcher beats the generic interpreted DFA by more
than an order of magnitude on a real (if simple) grammar's plain-terminal path
— exactly the ~55% of lark-rs's own profiled instruction budget the lexer
occupies (`BENCH.md`, 2026-06-04 profiling spike). This is the single most
promising lever the research note surfaced, now backed by a first real number
instead of literature-only evidence.

### Caveats / what this does NOT show

- **One grammar, hand-written, no general emitter.** JSON's terminal set is
  small and has no lookaround; a real baked-DFA backend would need to
  generalize to `lark-rs`'s full lowering machinery (guards, `unless`
  retyping, fences) — this spike deliberately didn't attempt that (the
  research note explicitly allows "a quick-and-dirty ... table for ONE
  grammar").
- **Wild-bank corpus not run.** Time-boxed out of this spike; the suggested
  follow-up experiment is the wild bank's more complex grammars (mixed
  plain+lookaround terminals), where the win may be smaller (the lookaround
  branches would still need *some* engine, baked or not).
- **Not a claim about SIMD (B2-B5).** A separate, larger engineering
  investment per the research note; untouched here.

### Recommendation

This is the strongest, cheapest-to-verify lever the research turned up, and
this spike's number makes the case stronger, not weaker. **Escalate for
scoping**: a real baked-DFA backend is a substantial feature (a new codegen
target hanging off the existing `regex-automata` DFA + standalone-codegen
architecture), not a bug-fix-tier change — proposing it belongs in `/roadmap`,
not this branch.

---

## Experiment 2+3 — Child-list representation (M5) and label interning (M2) on the OWNED tree

**File:** `examples/spike_owned_child_repr.rs` — `cargo run --release --example spike_owned_child_repr [records]`

### Why one file covers both

`examples/child_vec_alloc.rs` (already committed, pre-dates this spike)
isolated the per-node child-`Vec` cost between `SpanTree` and `TapeTree` — but
both of those are **zero-copy** backends (borrowed labels, borrowed token
values), so that result doesn't say anything about the **default, fully-owned**
tree `parse()` actually returns, where `Tree::data` is a fresh heap `String`
per node *in addition to* the child `Vec`. This spike closes that gap with a
five-point ladder, each point built as a plain [`OutputBuilder`] driven through
the **existing public** `Lark::parse_into` seam (#232/C7) — no src/ changes,
no new feature flag, nothing productionized:

- **A0** — `parser.parse()`, the real default (`String` label, `Vec<Child>`
  built by *stealing* the engine's pre-populated scratch buffer via
  `std::mem::take` — no copy).
- **A1** — a custom builder reproducing A0's representation exactly. Sanity
  check: A1's allocation count must match A0's, or the harness is suspect.
- **B** — swap the label for an interned `rule: u32`, keep the steal. Isolates
  **M2 alone**.
- **D** — B's `u32` label + a `SmallVec<[V; 4]>` child list built via
  `children.drain(..).collect()` (not `SmallVec::from_vec`, which would leave
  an existing heap `Vec` heap-allocated forever — only building fresh from an
  iterator gets small counts inline).
- **E** — B's `u32` label + a single shared flat `kids: Vec<u32>` arena
  (exactly `TapeTree`'s structure, but with **owned** `Token` leaves instead of
  borrowed spans) — full M5 on the owned tree.

D and E both deliberately **do not steal** the engine's scratch buffer — see
the "load-bearing detail" below.

### Load-bearing detail: stealing vs. not stealing

`shape_reduction`'s per-parse `ReduceScratch::values` buffer is recycled
*only* if the builder leaves it in place: the caller runs
`values.clear(); scratch.values = values;` unconditionally right after
`reduce()` returns. The **default builder steals it** (`std::mem::take`) as
the permanent `Tree.children` — free (no copy), but it means the *next*
reduction's scratch buffer starts from zero capacity, forcing a fresh
allocation. That nets out to exactly the ~1.0 alloc/node `child_vec_alloc.rs`
already measured on the zero-copy path, now confirmed to hold on the owned
path too (see A0/A1 below).

A `SmallVec`- or flat-arena-based builder **must not steal** (there's nothing
to steal into a `SmallVec`'s inline slots or a shared array) — it copies
elements out via `drain()`/`extend_from_slice` instead, which is one memcpy
but leaves the scratch buffer's capacity intact for the *next* reduction to
reuse. This is a real, if secondary, finding: **avoiding the steal is not
itself a win** (variant D pays a copy for it and shows no net improvement) —
the win only shows up when the destination *also* avoids allocating (E's
shared array, or D's inline slots when they don't spill).

### Result (2000-record JSON, 594 KB input, 98,001 internal nodes)

| variant | allocs | allocs/node | allocs/byte | wall-clock (median, trend) |
|---|---:|---:|---:|---:|
| A0 `parse()` (default) | 560,026 | 5.714 | 0.9424 | 84.96 ms (7.0 MB/s) |
| A1 String label, steal (sanity) | 560,027 | 5.715 | 0.9424 | — |
| B  `u32` label, steal | 462,026 | 4.715 | 0.7775 | 64.44 ms (9.2 MB/s) |
| D  `u32` label, `SmallVec<4>` | 464,030 | 4.735 | 0.7809 | 68.80 ms (8.6 MB/s) |
| E  `u32` label, flat `kids[]` | 364,062 | 3.715 | 0.6127 | 48.91 ms (12.2 MB/s) |

- **A1 vs A0**: off by exactly 1 allocation total (out of 560K) — the harness
  reproduces the real default's allocation profile to within noise; trust the
  deltas below.
- **B vs A0: -98,000 allocs, exactly 1.000 per node** (-17.5% of A0's total).
  Clean, isolated confirmation of M2: the label `String` is genuinely the only
  thing removed between A0 and B, and it costs exactly one allocation per node,
  every time — no surprises.
- **D vs B: +2,004 allocs (+0.4%), and wall-clock is *slower*, not faster.**
  Killed. A recursive owned-by-value tree (`enum Child { Tree(Node), ... }`)
  cannot have a variant hold an *inline* `SmallVec<[Self; N]>` — the compiler
  rejects it (`cycle detected when computing layout`) because inlining
  requires `Self`'s size to be finite, which is circular. Breaking the cycle
  needs `Box<Node>` in the `Tree` variant, which is itself one allocation per
  node — the exact cost `SmallVec` was supposed to remove. Net: a wash on
  small nodes (Box instead of Vec, same 1 alloc), and *worse* than the
  baseline on wide nodes (`array`/`object`, >4 children: Box **plus** a
  spilled `SmallVec` heap buffer = 2 allocs vs. `Vec`'s 1). **This is a real
  architectural fact, not a tuning miss**: `SmallVec`-inline children only pay
  off when the recursive occurrence is an index into an arena (`u32`, `Copy`,
  no cycle) rather than an owned value — which is just M5's flat-array idea
  again, not a genuinely separate lever.
- **E vs B: -391,960→-97,964 allocs (-21.2% of B, ~35% of A0 total)**, and
  wall-clock improves further (48.9 ms vs. B's 64.4 ms). Confirms M5
  generalizes from the zero-copy `SpanTree`/`TapeTree` comparison to the
  **owned** tree: even with real `Token` values still being copied, collapsing
  every node's child list into one shared growing array removes essentially
  all of the remaining child-container cost.
- **Scaling check**: re-run at 200 and 8,000 records (0.1x/4x the reference
  size) reproduces every ratio above to 3 significant figures — the per-node
  cost is flat, not an artifact of one input size.
- **Remaining floor after M2+M5 (E): 0.61 allocs/byte**, down from A0's 0.94
  (-35%). This residual is now dominated by the owned `Token.value`/`Token.type_`
  `String`s copied per leaf — exactly M6 (span-borrowing), which `SpanTree`/
  `TapeTree` already solve separately. Stacking M2+M5+M6 on one backend (an
  "owned-token TapeTree" is most of the way there; going fully zero-copy is
  `SpanTree`/`TapeTree` again) is the natural next data point, not attempted
  here.

### Verdicts

- **M2 (label interning): confirmed**, clean and isolated. The cheapest, most
  mechanical of the three levers this pass measured.
- **M5 (flat child arena) on the owned tree: confirmed**, generalizing the
  zero-copy result. The bigger of the two confirmed wins here.
- **M5 via naive `SmallVec`-inline: killed.** Not a viable independent lever
  for a recursive-by-value tree; folds back into "just do the flat arena."

### Recommendation

Both confirmed levers require changing the **public** `Tree::data: String` /
`Tree::children: Vec<Child>` representation (or shipping a new committed
output backend, à la `SpanTree`/`TapeTree`'s `ADR-0029 fork 3` experimental
pattern) — this is exactly the kind of "real trade-off, not falsifiable from a
counter alone" call `PRINCIPLES.md` §4 says to escalate, not decide
unilaterally on a spike branch. Two candidate shapes for a follow-up
`needs-decision`:

1. An "owned-tape" output backend (M2+M5, no M6) as a new experimental
   feature, exactly mirroring `TapeTree`'s existing pattern but with owned
   `Token`s — the natural next artifact if this direction is approved.
2. Fold M2 alone into the *existing* default `Tree` type as a breaking v-next
   change (`Tree::data` becomes a resolvable id, not a `String`) — bigger
   blast radius (public API), bigger one-line win (-17.5% allocs, no new
   backend to maintain), architect's call on which axis (compat vs. simplicity)
   wins.

---

## What this spike did not attempt

- SIMD/data-parallel lexing (B2-B5) — a hand-vectorized rewrite, out of scope
  for a "quick-and-dirty" spike.
- The wild-bank corpus for either experiment — both used the synthetic JSON
  workload `child_vec_alloc.rs`/`lex_backends.rs` already established, to keep
  the numbers comparable to prior spikes. A real grammar with lookaround
  (`python.lark`) or deep nesting would be the natural follow-up corpus.
- Token-value interning / stacking M2+M5+M6 on one backend (noted above as the
  natural next data point).
- Any change to `lark-rs/src/`, `Cargo.toml`, or public API — by design, this
  is a measurement-only branch.

## Reproducing

```bash
cd lark-rs
cargo run --release --example spike_baked_lexer
cargo run --release --example spike_owned_child_repr        # 2000 records (default)
cargo run --release --example spike_owned_child_repr 200    # smaller
cargo run --release --example spike_owned_child_repr 8000   # larger
```
